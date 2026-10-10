//! Tasks: a repository's Git worktrees, each a branch checked out in a
//! folder of its own, so several agents can work at once without editing
//! the same files.
//!
//! The list is Git's own (`git worktree list`), so worktrees made outside
//! crc show too and nothing is kept on the side. Worktrees crc makes go
//! under Application Support, out of the project's tree, its index and its
//! file watcher.

use std::path::{Path, PathBuf};
use std::process::Command;

/// One checkout of the repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// The branch checked out, without `refs/heads/`; `None` when HEAD is
    /// detached.
    pub branch: Option<String>,
    /// The repository's own checkout, which cannot be removed.
    pub main: bool,
}

impl Worktree {
    /// What the sidebar calls it: the branch, or the folder's name.
    pub fn name(&self) -> String {
        match &self.branch {
            Some(branch) => branch.clone(),
            None => self.path.file_name().map_or_else(
                || self.path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            ),
        }
    }
}

fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    cmd
}

fn run(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = git(dir).args(args).output().map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let said = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(said
            .strip_prefix("fatal: ")
            .or_else(|| said.strip_prefix("error: "))
            .unwrap_or(&said)
            .to_owned())
    }
}

/// Every worktree of the repository `dir` is in, its own checkout first.
/// Prunable ones (their folder is gone) are left out.
pub fn list(dir: &Path) -> Result<Vec<Worktree>, String> {
    Ok(parse_list(&run(dir, &["worktree", "list", "--porcelain"])?))
}

/// `git worktree list --porcelain`: blocks separated by blank lines, the
/// first the main worktree.
pub fn parse_list(text: &str) -> Vec<Worktree> {
    let mut out = Vec::new();
    for (index, block) in text.split("\n\n").enumerate() {
        let mut path = None;
        let mut branch = None;
        let mut skip = false;
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch ") {
                branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_owned());
            } else if line == "bare" || line.starts_with("prunable") {
                // A bare repository has no checkout to work in; a prunable
                // one's folder is gone.
                skip = true;
            }
        }
        if let Some(path) = path.filter(|_| !skip) {
            out.push(Worktree {
                path,
                branch,
                main: index == 0,
            });
        }
    }
    out
}

/// A branch name from what someone typed: lower case, runs of anything
/// but letters, digits, `/`, `.` and `_` turned into one `-`.
pub fn branch_name(typed: &str) -> String {
    let mut out = String::new();
    for c in typed.trim().chars() {
        if c.is_alphanumeric() || matches!(c, '/' | '.' | '_') {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out
        .trim_matches(|c| matches!(c, '-' | '/' | '.'))
        .to_owned();
    // Git refuses `..`, `@{` and a trailing `.lock`; keep it simple.
    out.replace("..", ".")
}

/// Where crc puts the worktrees of the repository at `main`: one folder
/// per repository, named for it, with a hash so two repositories of the
/// same name do not share one.
pub fn home(main: &Path) -> Option<PathBuf> {
    let base = match std::env::var_os("CRC_WORKTREES") {
        Some(dir) => PathBuf::from(dir),
        None => crate::platform::app_support()?.join("worktrees"),
    };
    let canonical = crate::platform::canonical(main);
    let name = canonical
        .file_name()
        .map_or_else(|| "repo".into(), |n| n.to_string_lossy().into_owned());
    let hash = crate::platform::fnv1a(canonical.as_os_str().as_encoded_bytes());
    Some(base.join(format!("{name}-{:08x}", hash as u32)))
}

/// Makes a worktree for a new task: a new branch `name` from the main
/// checkout's HEAD, in its own folder. Returns the folder.
pub fn add(main: &Path, name: &str) -> Result<PathBuf, String> {
    let branch = branch_name(name);
    if branch.is_empty() {
        return Err("Type a name with a letter or digit in it".into());
    }
    let dir = home(main)
        .ok_or("No home folder to put the worktree in")?
        .join(branch.replace('/', "-"));
    if dir.exists() {
        return Err(format!("{} already exists", dir.display()));
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let target = dir.to_string_lossy().into_owned();
    run(main, &["worktree", "add", "-b", &branch, &target, "HEAD"])?;
    Ok(dir)
}

/// Whether the checkout at `dir` has changes Git would lose on removal:
/// anything modified, staged or untracked.
pub fn is_dirty(dir: &Path) -> Result<bool, String> {
    Ok(
        !run(dir, &["status", "--porcelain", "--untracked-files=normal"])?
            .trim()
            .is_empty(),
    )
}

/// Removes a task's worktree, refused while it has changes. The branch is
/// deleted too when it is merged into the main checkout's HEAD; an
/// unmerged one is kept, and the answer says so.
pub fn remove(main: &Path, tree: &Worktree) -> Result<String, String> {
    if tree.main {
        return Err("The repository's own checkout is not a task".into());
    }
    if is_dirty(&tree.path)? {
        return Err(format!(
            "{} has uncommitted changes; commit or discard them first",
            tree.name()
        ));
    }
    let target = tree.path.to_string_lossy().into_owned();
    run(main, &["worktree", "remove", &target])?;
    let Some(branch) = &tree.branch else {
        return Ok(format!("Removed {}", tree.name()));
    };
    Ok(match run(main, &["branch", "-d", branch]) {
        Ok(_) => format!("Removed {branch} and its branch"),
        Err(_) => format!("Removed {branch}; the branch has unmerged commits and was kept"),
    })
}

/// Merges a task's branch into whatever the main checkout has out.
/// Refused while the main checkout has changes of its own; a conflict
/// leaves the merge in progress for Source Control to finish or abort.
pub fn merge(main: &Path, branch: &str) -> Result<String, String> {
    if is_dirty(main)? {
        return Err("The main checkout has uncommitted changes; commit or stash them first".into());
    }
    let into = run(main, &["branch", "--show-current"])?.trim().to_owned();
    match run(main, &["merge", "--no-edit", branch]) {
        Ok(_) => Ok(format!("Merged {branch} into {into}")),
        Err(e) if e.contains("CONFLICT") || e.contains("Automatic merge failed") => Err(format!(
            "Merging {branch} into {into} stopped on conflicts; resolve them in Source Control"
        )),
        Err(e) => Err(e),
    }
}

/// How far the task's branch is from where it started: commits on it the
/// main checkout's HEAD does not have.
pub fn ahead(main: &Path, branch: &str) -> Result<usize, String> {
    let range = format!("HEAD..{branch}");
    Ok(run(main, &["rev-list", "--count", &range])?
        .trim()
        .parse()
        .unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_porcelain_list() {
        let text = "worktree /r/crc\nHEAD 1111\nbranch refs/heads/main\n\n\
                    worktree /w/fix-login\nHEAD 2222\nbranch refs/heads/fix/login\n\n\
                    worktree /w/detached\nHEAD 3333\ndetached\n\n\
                    worktree /w/gone\nHEAD 4444\nbranch refs/heads/gone\nprunable gitdir file points to non-existent location\n";
        let list = parse_list(text);
        assert_eq!(list.len(), 3);
        assert!(list[0].main);
        assert_eq!(list[0].branch.as_deref(), Some("main"));
        assert_eq!(list[1].name(), "fix/login");
        assert!(!list[1].main);
        assert_eq!(list[2].branch, None);
        assert_eq!(list[2].name(), "detached");
    }

    #[test]
    fn branch_names_from_typing() {
        assert_eq!(branch_name("Fix the login bug!"), "fix-the-login-bug");
        assert_eq!(branch_name("  feat/Agents  mode "), "feat/agents-mode");
        assert_eq!(branch_name("a..b"), "a.b");
        assert_eq!(branch_name("!!!"), "");
    }

    #[test]
    fn makes_merges_and_removes_a_task() {
        let tree = crate::project::TempTree::new("worktree", &[("a.txt", "one\n")]);
        let repo = &tree.0;
        let ok = |args: &[&str]| assert!(git(repo).args(args).output().unwrap().status.success());
        ok(&["init", "-q", "-b", "main"]);
        ok(&["-c", "user.name=t", "-c", "user.email=t@t", "add", "a.txt"]);
        ok(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "one",
        ]);
        let home = repo.with_extension("trees");
        // SAFETY: tests in this module are the only readers of the variable.
        unsafe { std::env::set_var("CRC_WORKTREES", &home) };

        let dir = add(repo, "Try It").unwrap();
        assert!(dir.join("a.txt").is_file());
        let list = list(repo).unwrap();
        assert_eq!(list.len(), 2);
        let task = list[1].clone();
        assert_eq!(task.branch.as_deref(), Some("try-it"));

        std::fs::write(dir.join("b.txt"), "two\n").unwrap();
        assert!(is_dirty(&dir).unwrap());
        assert!(remove(repo, &task).is_err());
        let commit = |args: &[&str]| {
            assert!(git(&dir).args(args).output().unwrap().status.success());
        };
        commit(&["add", "b.txt"]);
        commit(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "two",
        ]);
        assert_eq!(ahead(repo, "try-it").unwrap(), 1);
        assert_eq!(merge(repo, "try-it").unwrap(), "Merged try-it into main");
        assert!(repo.join("b.txt").is_file());
        assert_eq!(
            remove(repo, &task).unwrap(),
            "Removed try-it and its branch"
        );
        assert!(!dir.exists());
        let _ = std::fs::remove_dir_all(&home);
    }
}
