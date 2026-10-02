//! A workspace: the folder that is open and the Git repositories in it.
//!
//! A plain repository is a workspace of one. A folder that holds several
//! repositories next to notes about them (a `docs/` folder, an
//! `AGENTS.md`) is a workspace of several, and Source Control shows one of
//! them at a time. Everything not inside a member repository is a note.
//!
//! Members are found by looking for `.git` (a folder, or the file a linked
//! worktree or submodule has) in the open folder and two levels below it.
//! Nothing is run: the walk only reads directories.

use std::path::{Path, PathBuf};

/// How deep below the open folder repositories are looked for.
const DEPTH: usize = 2;
/// A folder with more repositories than this is not a workspace someone
/// keeps by hand (a home folder, a checkout cache); the rest are ignored.
const LIMIT: usize = 64;

#[derive(Clone, Debug, Default)]
pub struct Workspace {
    root: PathBuf,
    /// Member repositories, the open folder first when it is one, then the
    /// rest by path.
    repos: Vec<PathBuf>,
    /// Which member Source Control shows.
    selected: usize,
}

impl Workspace {
    /// Reads `root` for repositories. Cheap: a few directory listings.
    pub fn open(root: &Path) -> Workspace {
        Workspace {
            root: root.to_path_buf(),
            repos: find_repos(root),
            selected: 0,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn repos(&self) -> &[PathBuf] {
        &self.repos
    }

    /// Whether Source Control needs a repository picker.
    pub fn has_several(&self) -> bool {
        self.repos.len() > 1
    }

    /// The folder Source Control reads: the selected member, or the open
    /// folder itself when it holds none (Git then finds the repository
    /// around it, as it always has).
    pub fn git_dir(&self) -> &Path {
        self.repos.get(self.selected).unwrap_or(&self.root)
    }

    /// Selects the member at `repo`. False when it is not one.
    pub fn select(&mut self, repo: &Path) -> bool {
        match self.repos.iter().position(|r| r == repo) {
            Some(index) => {
                self.selected = index;
                true
            }
            None => false,
        }
    }

    /// The member that holds `path`, the innermost when they nest.
    pub fn repo_of(&self, path: &Path) -> Option<&Path> {
        self.repos
            .iter()
            .filter(|repo| path.starts_with(repo))
            .max_by_key(|repo| repo.components().count())
            .map(PathBuf::as_path)
    }

    /// How a member is named in the picker: its path under the open
    /// folder, or the folder's own name for the open folder.
    pub fn label(&self, repo: &Path) -> String {
        match repo.strip_prefix(&self.root) {
            Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().into_owned(),
            _ => repo.file_name().map_or_else(
                || repo.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            ),
        }
    }

    /// The selected member's label, when there is a choice to show.
    pub fn selected_label(&self) -> Option<String> {
        self.has_several().then(|| self.label(self.git_dir()))
    }
}

fn is_repo(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// Folders a repository is never looked for in.
fn skipped(name: &str) -> bool {
    name.starts_with('.') || super::SKIP_DIRS.contains(&name)
}

fn find_repos(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(root, 1, &mut found);
    found.sort();
    found.truncate(LIMIT);
    if is_repo(root) {
        found.insert(0, root.to_path_buf());
    }
    found
}

fn walk(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if depth > DEPTH || found.len() >= LIMIT {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| !skipped(&e.file_name().to_string_lossy()))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    for child in dirs {
        if is_repo(&child) {
            found.push(child.clone());
        }
        // A repository can hold others (a notes repository with code
        // repositories ignored inside it), so the walk goes on below it.
        walk(&child, depth + 1, found);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::TempTree;

    #[test]
    fn finds_the_repositories_in_a_folder_of_notes() {
        let t = TempTree::new(
            "ws-notes",
            &[
                ("AGENTS.md", "rules"),
                ("docs/state.md", "now"),
                ("app/.git/", ""),
                ("app/src/main.rs", ""),
                ("site/.git", "gitdir: elsewhere"),
                ("libs/parser/.git/", ""),
                ("libs/deep/er/.git/", ""),
                (".hidden/.git/", ""),
                ("node_modules/x/.git/", ""),
            ],
        );
        let ws = Workspace::open(&t.0);
        let labels: Vec<String> = ws.repos().iter().map(|r| ws.label(r)).collect();
        assert_eq!(labels, ["app", "libs/parser", "site"]);
        assert!(ws.has_several());
        assert_eq!(ws.git_dir(), t.0.join("app"));
        assert_eq!(ws.selected_label().as_deref(), Some("app"));
    }

    #[test]
    fn a_plain_repository_is_a_workspace_of_one() {
        let t = TempTree::new("ws-plain", &[(".git/", ""), ("src/lib.rs", "")]);
        let ws = Workspace::open(&t.0);
        assert_eq!(ws.repos(), std::slice::from_ref(&t.0));
        assert!(!ws.has_several());
        assert_eq!(ws.selected_label(), None);
        assert_eq!(ws.git_dir(), t.0);
    }

    #[test]
    fn a_repository_holding_others_comes_first() {
        let t = TempTree::new("ws-nested", &[(".git/", ""), ("code/.git/", "")]);
        let ws = Workspace::open(&t.0);
        assert_eq!(ws.repos(), [t.0.clone(), t.0.join("code")]);
        let name = t.0.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(ws.label(&t.0), name);
        assert_eq!(
            ws.repo_of(&t.0.join("code/a.rs")),
            Some(t.0.join("code").as_path())
        );
        assert_eq!(ws.repo_of(&t.0.join("notes.md")), Some(t.0.as_path()));
    }

    #[test]
    fn a_folder_without_repositories_reads_git_from_itself() {
        let t = TempTree::new("ws-none", &[("notes.md", "")]);
        let ws = Workspace::open(&t.0);
        assert!(ws.repos().is_empty());
        assert_eq!(ws.git_dir(), t.0);
        assert_eq!(ws.repo_of(&t.0.join("notes.md")), None);
    }

    #[test]
    fn selecting_switches_only_to_a_member() {
        let t = TempTree::new("ws-select", &[("a/.git/", ""), ("b/.git/", "")]);
        let mut ws = Workspace::open(&t.0);
        assert!(ws.select(&t.0.join("b")));
        assert_eq!(ws.git_dir(), t.0.join("b"));
        assert!(!ws.select(&t.0.join("c")));
        assert_eq!(ws.git_dir(), t.0.join("b"));
    }
}
