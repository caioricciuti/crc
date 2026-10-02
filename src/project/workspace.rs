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

    /// The folder Source Control falls back to when no project is open:
    /// not read for repositories, since it may be `/` or the home folder.
    pub fn unopened(dir: &Path) -> Workspace {
        Workspace {
            root: dir.to_path_buf(),
            ..Workspace::default()
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

/// The workspace file, under the open folder, when that folder is not
/// inside a repository: settings that belong to the workspace and to no
/// one repository.
pub const SETTINGS_FILE: &str = ".crc/workspace.toml";

/// Notes crc knows the role of, from `.crc/workspace.toml`, or found by
/// their usual names when the file says nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// The short doc saying where things stand, rewritten each session.
    pub state: Option<PathBuf>,
    /// The running log, added to each session.
    pub log: Option<PathBuf>,
    /// Words that mark the state doc's heading for what waits on you;
    /// matched without case. Empty: the usual ones.
    pub waiting_heading: Option<String>,
}

/// Where the state doc usually is, first match wins.
const STATE_NAMES: &[&str] = &["docs/state.md", "docs/STATE.md", "STATE.md", "state.md"];
/// Where the log usually is.
const LOG_NAMES: &[&str] = &["docs/log.md", "docs/LOG.md", "LOG.md", "log.md"];
/// Headings that usually hold what waits on the person.
const WAITING_WORDS: &[&str] = &[
    "waiting",
    "open for",
    "open, for",
    "blocked on",
    "needs you",
];

impl Settings {
    /// Reads the workspace file under `root`, then fills in what it left
    /// out from the usual names. Paths in the file are relative to `root`;
    /// one that leaves it is ignored.
    pub fn load(root: &Path) -> Settings {
        let text = std::fs::read_to_string(root.join(SETTINGS_FILE)).unwrap_or_default();
        let mut settings = Settings::parse(root, &text);
        let first = |names: &[&str]| names.iter().map(|n| root.join(n)).find(|p| p.is_file());
        if settings.state.is_none() {
            settings.state = first(STATE_NAMES);
        }
        if settings.log.is_none() {
            settings.log = first(LOG_NAMES);
        }
        settings
    }

    fn parse(root: &Path, text: &str) -> Settings {
        use crate::platform::settings::{split_line, unquote};
        let mut settings = Settings::default();
        let inside = |value: &str| {
            let rel = unquote(value)?;
            let rel = Path::new(rel.as_ref());
            let clean = rel
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)));
            (clean && !rel.as_os_str().is_empty()).then(|| root.join(rel))
        };
        for line in text.lines() {
            let Some((key, value)) = split_line(line) else {
                continue;
            };
            match key.trim() {
                "state" => settings.state = inside(value),
                "log" => settings.log = inside(value),
                "waiting_heading" => {
                    settings.waiting_heading = unquote(value)
                        .map(|v| v.into_owned())
                        .filter(|v| !v.is_empty());
                }
                _ => {}
            }
        }
        settings
    }
}

/// How many waiting items Home lists.
const WAITING_LIMIT: usize = 8;

/// The list items under the state doc's "waiting on you" heading: the
/// first heading whose text holds `heading` (or one of the usual words),
/// down to the next heading at its level or above. An item's wrapped
/// lines are joined; Markdown emphasis, code marks and link targets go.
pub fn waiting_items(markdown: &str, heading: Option<&str>) -> Vec<String> {
    let matches = |title: &str| {
        let title = title.to_lowercase();
        match heading {
            Some(words) => title.contains(&words.to_lowercase()),
            None => WAITING_WORDS.iter().any(|w| title.contains(w)),
        }
    };
    let mut items: Vec<String> = Vec::new();
    let mut section: Option<usize> = None;
    let mut fenced = false;
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let level = line.chars().take_while(|&c| c == '#').count();
        if level > 0 && line[level..].starts_with(' ') {
            match section {
                Some(at) if level <= at => break,
                Some(_) => continue,
                None if matches(line[level..].trim()) => section = Some(level),
                None => {}
            }
            continue;
        }
        if section.is_none() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let text = line.trim();
        let bullet = ["- ", "* ", "+ "]
            .iter()
            .find_map(|b| text.strip_prefix(b))
            .or_else(|| {
                let digits = text.chars().take_while(char::is_ascii_digit).count();
                (digits > 0)
                    .then(|| text[digits..].strip_prefix(". "))
                    .flatten()
            });
        match bullet {
            Some(rest) if indent < 2 => {
                if items.len() == WAITING_LIMIT {
                    break;
                }
                items.push(rest.trim().to_owned());
            }
            _ if !text.is_empty() && indent > 0 => {
                if let Some(last) = items.last_mut() {
                    last.push(' ');
                    last.push_str(text);
                }
            }
            _ => {}
        }
    }
    items.into_iter().map(|i| plain(&i)).collect()
}

/// Markdown inline marks taken out: `**`, `__`, backticks, and links
/// down to their text.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find("](").and_then(|close| {
            after[close + 2..]
                .find(')')
                .map(|end| (close, close + 2 + end + 1))
        }) {
            Some((close, end)) => {
                out.push_str(&after[..close]);
                rest = &after[end..];
            }
            None => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.replace("**", "").replace("__", "").replace('`', "")
}

/// How deep notes are looked at for "changed since you were last here".
const NOTES_DEPTH: usize = 4;
/// How many changed notes Home lists.
const NOTES_LIMIT: usize = 6;

impl Workspace {
    /// Notes (files under the open folder outside every member repository
    /// below it) modified after `since`, newest first. The open folder's
    /// own repository, when it is one, holds notes; repositories inside it
    /// do not.
    pub fn notes_changed_since(&self, since: u64) -> Vec<PathBuf> {
        let mut found: Vec<(u64, PathBuf)> = Vec::new();
        let skip: Vec<&PathBuf> = self.repos.iter().filter(|r| **r != self.root).collect();
        notes_walk(&self.root, 0, &skip, since, &mut found);
        found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        found
            .into_iter()
            .take(NOTES_LIMIT)
            .map(|(_, p)| p)
            .collect()
    }
}

fn notes_walk(
    dir: &Path,
    depth: usize,
    skip: &[&PathBuf],
    since: u64,
    found: &mut Vec<(u64, PathBuf)>,
) {
    if depth > NOTES_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if skipped(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if !skip.contains(&&path) {
                notes_walk(&path, depth + 1, skip, since, found);
            }
        } else if kind.is_file()
            && let Some(at) = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
            && at > since
        {
            found.push((at, path));
        }
    }
}

/// When the workspace at `root` was last opened, in Unix seconds, from
/// crc's own folder (never the workspace's).
pub fn last_seen(root: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(seen_file(root)?).ok()?;
    text.trim().parse().ok()
}

/// Records that the workspace at `root` was opened at `now`.
pub fn mark_seen(root: &Path, now: u64) {
    if let Some(file) = seen_file(root) {
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = crate::platform::write_atomically(&file, now.to_string().as_bytes());
    }
}

fn seen_file(root: &Path) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let key = crate::platform::fnv1a(root.as_os_str().as_bytes());
    Some(crate::platform::app_support()?.join(format!("workspaces/{key:016x}.seen")))
}

/// One repository as Home lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoSummary {
    pub path: PathBuf,
    pub label: String,
    /// `main ↑1`, or why it could not be read.
    pub status: String,
    pub changes: usize,
    /// Subjects of the commits since the last visit, newest first.
    pub commits: Vec<String>,
}

/// What Home shows for a workspace, read on a worker.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub settings: Settings,
    pub waiting: Vec<String>,
    pub repos: Vec<RepoSummary>,
    /// Notes changed since the last visit, newest first.
    pub notes: Vec<PathBuf>,
    /// The last visit, before this one, in Unix seconds.
    pub since: Option<u64>,
}

/// How many commits a repository lists under "since you were last here".
const COMMITS_LIMIT: usize = 20;

impl Workspace {
    /// Reads everything Home shows. Runs Git, so call it on a worker.
    pub fn summary(&self, since: Option<u64>) -> Summary {
        let settings = Settings::load(&self.root);
        let waiting = settings
            .state
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| waiting_items(&text, settings.waiting_heading.as_deref()))
            .unwrap_or_default();
        let repos = self
            .repos
            .iter()
            .map(|repo| {
                let (status, changes) = match super::git::snapshot(repo) {
                    Ok(s) => (
                        crate::platform::git_panel::snapshot_status(&s),
                        s.changes.len(),
                    ),
                    Err(e) => (e, 0),
                };
                let commits = since
                    .and_then(|t| super::git::commits_since(repo, t, COMMITS_LIMIT).ok())
                    .unwrap_or_default();
                RepoSummary {
                    path: repo.clone(),
                    label: self.label(repo),
                    status,
                    changes,
                    commits,
                }
            })
            .collect();
        let notes = since
            .map(|t| self.notes_changed_since(t))
            .unwrap_or_default();
        Summary {
            settings,
            waiting,
            repos,
            notes,
            since,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::TempTree;

    #[test]
    fn settings_come_from_the_file_then_the_usual_names() {
        let t = TempTree::new(
            "ws-settings",
            &[
                (
                    ".crc/workspace.toml",
                    "state = \"notes/now.md\"\nwaiting_heading = \"For me\"\nlog = \"../outside.md\"\n",
                ),
                ("notes/now.md", ""),
                ("docs/log.md", ""),
            ],
        );
        let s = Settings::load(&t.0);
        assert_eq!(s.state, Some(t.0.join("notes/now.md")));
        // A path leaving the workspace is ignored; the usual name is used.
        assert_eq!(s.log, Some(t.0.join("docs/log.md")));
        assert_eq!(s.waiting_heading.as_deref(), Some("For me"));

        let bare = TempTree::new("ws-settings-bare", &[("STATE.md", "")]);
        let s = Settings::load(&bare.0);
        assert_eq!(s.state, Some(bare.0.join("STATE.md")));
        assert_eq!(s.log, None);
    }

    #[test]
    fn waiting_items_are_the_list_under_the_heading() {
        let doc = "# State\n\n## Next\n\n- not this\n\n## Open, for me\n\n\
                   - Push the **release** branch\n  and tag it.\n\
                   - Read [the notes](https://example.com/n) with `care`\n\
                   \n  ```\n  - not an item\n  ```\n\
                   1. Numbered too\n### A sub-heading\n- still inside\n\
                   ## Standing rules\n- not this either\n";
        assert_eq!(
            waiting_items(doc, None),
            [
                "Push the release branch and tag it.",
                "Read the notes with care",
                "Numbered too",
                "still inside",
            ]
        );
        assert_eq!(waiting_items(doc, Some("NEXT")), ["not this"]);
        assert!(waiting_items(doc, Some("absent")).is_empty());
    }

    #[test]
    fn notes_changed_leave_out_repositories_inside() {
        let t = TempTree::new(
            "ws-changed",
            &[
                ("docs/a.md", ""),
                ("app/.git/", ""),
                ("app/code.rs", ""),
                (".crc/x", ""),
            ],
        );
        let ws = Workspace::open(&t.0);
        let changed = ws.notes_changed_since(0);
        assert_eq!(changed, [t.0.join("docs/a.md")]);
        assert!(ws.notes_changed_since(u64::MAX / 2).is_empty());
    }

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
