//! Agents mode: the terminal takes the editor column and the sidebar lists
//! workspaces, each with the sessions running in it: the ones made by hand
//! on a folder, and the worktrees of the Git repositories those folders
//! hold (the folder itself and its direct children), read from Git. The
//! list is one for the whole app, not the editor's open folder. The
//! toolbar's Editor / Agents switch (Cmd-Shift-A) flips between the two;
//! nothing stops when it does. The code calls a workspace a task where it
//! grew up as one.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use crate::platform::terminal::{Activity, Tab};
use crate::project::icons;
use crate::project::worktree::{IssueError, Runner, Worktree};
use crate::render::{
    font::Atlas,
    layout::{self, Theme, Viewport},
    metal::GlyphInstance,
};

pub const ROW: f32 = 26.0;
/// Under a task, its sessions start this far in.
const INDENT: f32 = 22.0;
/// A workspace's row: its name, and a line under it.
pub const CARD: f32 = 46.0;
/// A folder workspace's row: its name, the line under it, and its folder.
pub const FOLDER_CARD: f32 = 62.0;
/// A worktree's row under the folder workspace holding its repository:
/// the same two lines, closer together.
pub const CHILD: f32 = 40.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// The header's New Workspace.
    NewTask,
    Refresh,
    /// A task's row: shows its newest session, or starts a shell in it.
    Select(usize),
    /// A session's row, by terminal tab.
    Session(usize),
    /// A new terminal (the login shell) in a task.
    NewSession(usize),
    /// The empty space under the list: a new workspace with a terminal.
    Blank,
    /// The task's menu: open in the editor, merge, push, remove.
    Menu(usize),
    /// A folder workspace's chevron: shows or hides the worktrees under it.
    Fold(usize),
    /// The line over a folder workspace's merged worktrees: shows or hides
    /// them; its menu cleans them all up.
    Merged(usize),
}

/// What a worker read of the folder workspaces: per folder workspace (by
/// id), the repositories in it; and the worktrees of those repositories
/// other than their main checkouts, every path resolved. The local folder
/// workspaces are read apart from the remote ones, so a slow host never
/// holds this Mac's rows back.
struct Listing {
    /// The host whose folder workspaces were read; `None` for this Mac's.
    host: Option<String>,
    repos: Vec<(String, Vec<PathBuf>)>,
    worktrees: Vec<Workspace>,
    /// Per main checkout (on the host read), the worktrees Git records
    /// whose folder is gone.
    prunable: Prunable,
    /// Per main checkout on a host, its repository on GitHub (`owner/name`)
    /// from its origin, which `gh` here is told.
    github: std::collections::HashMap<RepoKey, String>,
    /// The remote folder workspaces, by id, whose host did not answer:
    /// what was last read of them stays.
    unreachable: Vec<String>,
}

/// Per main checkout, by host (`None` for this Mac) and path, how many
/// worktrees Git records whose folder is gone.
pub type Prunable = std::collections::HashMap<RepoKey, usize>;

/// A repository: its host (`None` for this Mac) and its main checkout.
pub type RepoKey = (Option<String>, PathBuf);

/// A place to work: a folder, and the sessions started in it. A worktree
/// of the open repository is one (named by its branch), and any folder
/// can be made one by hand (named by whoever made it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    pub name: String,
    pub path: PathBuf,
    /// The branch checked out, for a worktree; `None` for a folder
    /// workspace or a detached HEAD.
    pub branch: Option<String>,
    /// The repository's own checkout.
    pub main: bool,
    /// Made by hand on a folder: nothing in Git behind it.
    pub folder: bool,
    /// A folder workspace's own id, made once and kept through renames, so
    /// a new workspace never takes a removed one's name, pin or sessions.
    pub key: String,
    /// A worktree's repository: its main checkout, where branches are made,
    /// merged and removed.
    pub repo: Option<PathBuf>,
    /// The repository's default branch, which a worktree's is compared
    /// with (see [`crate::project::worktree::default_branch`]).
    pub base: Option<String>,
    /// A worktree branch's last commit: when, in Unix seconds, and its
    /// subject.
    pub commit: Option<(u64, String)>,
    /// A worktree branch that was committed to and is all on the default
    /// branch now: done, and safe to clean up. A squash merge Git cannot
    /// see is told by its pull request instead ([`Panel::is_merged`]).
    pub merged: bool,
    /// A remote folder workspace's ssh host (an alias from ~/.ssh/config
    /// or `user@host`): its path is on that host, its terminals open over
    /// ssh, and nothing of this Mac (Git, its folders) is read for it.
    pub host: Option<String>,
    /// A worktree on a host: its repository on GitHub (`owner/name`), from
    /// its `origin`, which `gh` here is told; `None` for one of this Mac's,
    /// which `gh` asks in its folder, or one not on GitHub.
    pub github: Option<String>,
}

impl Workspace {
    fn of(worktree: Worktree, repo: Option<PathBuf>, base: Option<String>) -> Workspace {
        Workspace {
            name: worktree.name(),
            path: worktree.path,
            branch: worktree.branch,
            main: worktree.main,
            folder: false,
            key: String::new(),
            repo,
            base,
            commit: None,
            merged: false,
            host: None,
            github: None,
        }
    }

    pub fn name(&self) -> String {
        self.name.clone()
    }

    /// What sessions and the selection hold on to: a folder workspace by
    /// its key (one kept from before may share its folder), a worktree by
    /// its path, and one on
    /// a host by `host:path`, never taken for a folder of this Mac.
    pub fn id(&self) -> String {
        if self.folder {
            self.key.clone()
        } else {
            row_id(self.host.as_deref(), &self.path)
        }
    }

    /// The worktree behind it, for the Git operations.
    pub fn worktree(&self) -> Option<Worktree> {
        (!self.folder).then(|| Worktree {
            path: self.path.clone(),
            branch: self.branch.clone(),
            main: self.main,
        })
    }
}

/// A worktree's id (see [`Workspace::id`]): its path, on `host` as
/// `host:path`.
pub fn row_id(host: Option<&str>, path: &Path) -> String {
    match host {
        Some(host) => format!("{host}:{}", path.display()),
        None => path.to_string_lossy().into_owned(),
    }
}

/// Where the folder workspaces are kept.
fn folders_file() -> Option<PathBuf> {
    Some(crate::platform::app_support()?.join("agents/workspaces"))
}

/// Where the arrangement is kept: the order you dragged the workspaces
/// into, which are pinned, and their own names.
fn order_file() -> Option<PathBuf> {
    Some(crate::platform::app_support()?.join("agents/order"))
}

/// The per-project files kept before the list was one for the app, with
/// `extension` (`workspaces` or `order`), by name. Read once, to carry
/// them over; left in place.
fn per_project_files(extension: &str) -> Vec<String> {
    let Some(dir) = crate::platform::app_support().map(|d| d.join("agents")) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == extension))
        .collect();
    files.sort();
    files
        .iter()
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .collect()
}

/// Every project's folder workspaces in one list. A key two projects both
/// had (`folder:crc` from before keys) is kept by the first; the other gets
/// its own, and its sessions find it by their folder.
fn merge_folders(texts: &[String]) -> Vec<Workspace> {
    let mut out: Vec<Workspace> = Vec::new();
    for text in texts {
        for mut w in parse_folders(text) {
            if out.iter().any(|o| o.key == w.key && o.path == w.path) {
                continue;
            }
            if out.iter().any(|o| o.key == w.key) {
                w.key = new_key(&out);
            }
            out.push(w);
        }
    }
    out
}

/// Every project's arrangement in one, the first place of an id kept.
fn merge_arrangements(texts: &[String]) -> Arrangement {
    let mut out = Arrangement::parse(&texts.concat());
    let mut seen = std::collections::HashSet::new();
    out.order.retain(|id| seen.insert(id.clone()));
    out
}

/// The Git repositories a folder holds: the one it is in (itself, or the
/// nearest folder above it with a `.git`), then its direct children by
/// name. Hidden folders are skipped, and so is a child whose `.git` is a
/// file: a linked worktree, which its repository's own list brings in.
pub fn discover(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Some(repo) = dir.ancestors().find(|d| d.join(".git").exists()) {
        found.push(repo.to_path_buf());
    }
    let mut children: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join(".git").exists() && !linked_worktree(p))
        .collect();
    children.sort();
    found.extend(children);
    found
}

/// What [`discover`] does, as one line for a host's shell: the repository
/// `dir` is in, then its direct children with a `.git`, by name, the hidden
/// ones and linked worktrees left out. Paths as the host has them, with
/// links resolved there.
pub fn discover_line(dir: &Path) -> String {
    format!(
        "cd -P {} || exit 1; export LC_ALL=C GIT_TERMINAL_PROMPT=0 GIT_OPTIONAL_LOCKS=0; \
         git rev-parse --show-toplevel 2>/dev/null; \
         for d in */; do d=${{d%/}}; [ -e \"$d/.git\" ] || continue; \
         [ -f \"$d/.git\" ] && grep -q '^gitdir:.*/worktrees/' \"$d/.git\" && continue; \
         printf '%s' \"$PWD/$d\"; echo; done",
        crate::platform::remote::shell_path(&dir.to_string_lossy())
    )
}

/// [`discover`], by `runner`. `Err` when the host did not answer, or the
/// folder is not there.
fn discover_on(runner: &Runner, dir: &Path) -> Result<Vec<PathBuf>, String> {
    let Runner::Ssh { host, control } = runner else {
        return Ok(discover(dir)
            .iter()
            .map(|p| crate::platform::canonical(p))
            .collect());
    };
    let text = crate::project::worktree::ssh_output(host, control.as_deref(), &discover_line(dir))?;
    let mut found: Vec<PathBuf> = Vec::new();
    for line in text.lines().filter(|l| l.starts_with('/')) {
        let path = PathBuf::from(line);
        if !found.contains(&path) {
            found.push(path);
        }
    }
    Ok(found)
}

/// How the Git of a workspace on `host` (this Mac's for `None`) is read
/// and run.
pub fn runner(host: Option<&str>) -> Runner {
    match host {
        None => Runner::Local,
        Some(host) => Runner::Ssh {
            host: host.to_owned(),
            control: crate::platform::remote::control_for(host),
        },
    }
}

/// Whether `dir` is a linked worktree: its `.git` a file pointing into a
/// repository's `worktrees` folder (a submodule's points elsewhere).
fn linked_worktree(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(".git")).is_ok_and(|text| {
        text.strip_prefix("gitdir:")
            .map(|to| to.trim().replace('\\', "/"))
            .is_some_and(|to| to.contains("/worktrees/"))
    })
}

/// Reads what the folder workspaces on `host` hold (this Mac's for
/// `None`), on the calling thread: their repositories, and each
/// repository's worktrees once. A remote one whose host does not answer is
/// left out, and named in [`Listing::unreachable`].
fn scan(folders: &[(String, PathBuf)], host: Option<&str>) -> Listing {
    let runner = runner(host);
    let mut repos = Vec::new();
    let mut worktrees: Vec<Workspace> = Vec::new();
    let mut mains: Vec<PathBuf> = Vec::new();
    let mut prunable = std::collections::HashMap::new();
    let mut github = std::collections::HashMap::new();
    let mut unreachable = Vec::new();
    for (id, dir) in folders {
        // A host that did not answer for one folder is not asked again,
        // and its timeout waited out again, for the next.
        if !unreachable.is_empty() {
            unreachable.push(id.clone());
            continue;
        }
        let found = match discover_on(&runner, dir) {
            Ok(found) => found,
            Err(e) if e == crate::project::worktree::UNREACHABLE => {
                unreachable.push(id.clone());
                continue;
            }
            Err(_) => Vec::new(),
        };
        // A host that stops answering partway: nothing of this folder is
        // taken, so its rows stay as they were.
        let mut read = Vec::new();
        let mut read_mains = Vec::new();
        let lost = found.iter().try_for_each(|repo| {
            if let Some((main, rows, missing, name)) =
                read_repo(&runner, repo, &mains, &read_mains, &worktrees, &read)?
            {
                if missing > 0 {
                    prunable.insert((host.map(str::to_owned), main.clone()), missing);
                }
                if let Some(name) = name {
                    github.insert((host.map(str::to_owned), main.clone()), name);
                }
                read_mains.push(main);
                read.extend(rows);
            }
            Ok::<(), String>(())
        });
        if lost.is_err() {
            unreachable.push(id.clone());
            continue;
        }
        mains.extend(read_mains);
        worktrees.extend(read);
        repos.push((id.clone(), found));
    }
    Listing {
        host: host.map(str::to_owned),
        repos,
        worktrees,
        prunable,
        github,
        unreachable,
    }
}

/// What [`read_repo`] read of a repository: its main checkout, its
/// worktrees' rows, how many worktrees Git records whose folder is gone,
/// and for one on a host its GitHub name.
type RepoRead = (PathBuf, Vec<Workspace>, usize, Option<String>);

/// One repository's worktrees other than its main checkout, as rows, with
/// the rest of a [`RepoRead`]; `None` when Git cannot list it or its main
/// checkout was read already (in `mains` or `more`). Rows already in
/// `rows` or `more_rows` are not made twice. `Err` when the host stopped
/// answering.
fn read_repo(
    runner: &Runner,
    repo: &Path,
    mains: &[PathBuf],
    more: &[PathBuf],
    rows: &[Workspace],
    more_rows: &[Workspace],
) -> Result<Option<RepoRead>, String> {
    use crate::project::worktree::{self as git, UNREACHABLE};
    let host = match runner {
        Runner::Local => None,
        Runner::Ssh { host, .. } => Some(host.clone()),
    };
    let (list, missing) = match git::list_all_on(runner, repo) {
        Ok(read) => read,
        Err(e) if e == UNREACHABLE => return Err(e),
        Err(_) => return Ok(None),
    };
    // A host's paths are its own: only this Mac's are resolved.
    let list: Vec<Worktree> = list
        .into_iter()
        .map(|mut w| {
            if host.is_none() {
                w.path = crate::platform::canonical(&w.path);
            }
            w
        })
        .collect();
    let Some(main) = list.iter().find(|w| w.main).cloned() else {
        return Ok(None);
    };
    if mains.contains(&main.path) || more.contains(&main.path) {
        return Ok(None);
    }
    // Once per repository: every branch's last commit, the default branch
    // the worktrees are compared with, and the branches all on it.
    let lost = |e: &String| e == UNREACHABLE;
    let tips = match git::tips_on(runner, &main.path) {
        Err(e) if lost(&e) => return Err(e),
        read => read.unwrap_or_default(),
    };
    // A host's repository's origin too: `gh` runs here, told its name.
    let (base, origin) = match git::default_and_origin_on(
        runner,
        &main.path,
        &tips,
        main.branch.as_deref(),
        host.is_some(),
    ) {
        Err(e) if lost(&e) => return Err(e),
        read => read.unwrap_or_default(),
    };
    let github = origin.as_deref().and_then(git::github_repo);
    let merged = match base
        .as_deref()
        .map(|b| git::merged_on(runner, &main.path, b))
    {
        Some(Err(e)) if lost(&e) => return Err(e),
        read => read.and_then(Result::ok).unwrap_or_default(),
    };
    let fresh: Vec<Worktree> = list
        .into_iter()
        .filter(|w| !w.main)
        .filter(|w| {
            !rows
                .iter()
                .chain(more_rows)
                .any(|t| t.path == w.path && t.host == host)
        })
        .collect();
    // On the default branch is not enough: a branch made and never
    // committed to is there from the start.
    let candidates: Vec<String> = fresh
        .iter()
        .filter_map(|w| w.branch.clone())
        .filter(|b| merged.contains(b) && base.as_deref() != Some(b.as_str()))
        .collect();
    let own = match git::own_commits_on(runner, &main.path, &candidates) {
        Err(e) if lost(&e) => return Err(e),
        read => read.unwrap_or_default(),
    };
    let done: Vec<&String> = candidates
        .iter()
        .zip(own)
        .filter_map(|(b, own)| own.then_some(b))
        .collect();
    let out = fresh
        .into_iter()
        .map(|w| {
            let commit = w.branch.as_ref().and_then(|b| {
                tips.iter()
                    .find(|t| t.branch == *b)
                    .map(|t| (t.time, t.subject.clone()))
            });
            let merged = w.branch.as_ref().is_some_and(|b| done.contains(&b));
            let mut workspace = Workspace::of(w, Some(main.path.clone()), base.clone());
            workspace.commit = commit;
            workspace.merged = merged;
            workspace.host = host.clone();
            workspace.github = github.clone();
            workspace
        })
        .collect();
    Ok(Some((main.path, out, missing, github)))
}

/// What the list keeps of your arrangement, per workspace id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Arrangement {
    /// Ids in the order you put them; ids not here follow, in Git's order.
    pub order: Vec<String>,
    pub pinned: std::collections::HashSet<String>,
    /// A name of your own for a workspace, shown instead of its branch.
    pub titles: std::collections::HashMap<String, String>,
    /// Folder workspaces whose worktrees are folded away under them.
    pub collapsed: std::collections::HashSet<String>,
    /// Folder workspaces whose merged worktrees are shown; folded is the
    /// default.
    pub merged_open: std::collections::HashSet<String>,
}

impl Arrangement {
    /// One per line: the flags (`pin`, `fold`, `merged`, comma-separated)
    /// or `-`, a tab, the id, a tab, the name (maybe empty). A file from
    /// before folding has `pin` or `-` alone.
    pub fn parse(text: &str) -> Arrangement {
        let mut out = Arrangement::default();
        for line in text.lines() {
            let mut parts = line.splitn(3, '\t');
            let (Some(flag), Some(id)) = (parts.next(), parts.next()) else {
                continue;
            };
            if id.is_empty() {
                continue;
            }
            out.order.push(id.to_owned());
            for flag in flag.split(',') {
                match flag {
                    "pin" => {
                        out.pinned.insert(id.to_owned());
                    }
                    "fold" => {
                        out.collapsed.insert(id.to_owned());
                    }
                    "merged" => {
                        out.merged_open.insert(id.to_owned());
                    }
                    _ => {}
                }
            }
            if let Some(title) = parts.next().filter(|t| !t.is_empty()) {
                out.titles.insert(id.to_owned(), title.to_owned());
            }
        }
        out
    }

    pub fn text(&self) -> String {
        self.order
            .iter()
            .map(|id| {
                let flags: Vec<&str> = [
                    (&self.pinned, "pin"),
                    (&self.collapsed, "fold"),
                    (&self.merged_open, "merged"),
                ]
                .into_iter()
                .filter(|(set, _)| set.contains(id))
                .map(|(_, flag)| flag)
                .collect();
                let flags = if flags.is_empty() {
                    "-".to_owned()
                } else {
                    flags.join(",")
                };
                format!(
                    "{flags}\t{id}\t{}\n",
                    self.titles.get(id).map_or("", String::as_str)
                )
            })
            .collect()
    }

    /// Where `id` stands in your order; unknown ids after every known one.
    fn position(&self, id: &str) -> usize {
        self.order
            .iter()
            .position(|o| o == id)
            .unwrap_or(usize::MAX)
    }

    /// Moves `id` to just before `before`, or to the end.
    pub fn move_before(&mut self, id: &str, before: Option<&str>, all: &[String]) {
        // Every workspace takes a place, so the move is exact.
        for known in all {
            if !self.order.contains(known) {
                self.order.push(known.clone());
            }
        }
        self.order.retain(|o| o != id);
        let at = before
            .and_then(|b| self.order.iter().position(|o| o == b))
            .unwrap_or(self.order.len());
        self.order.insert(at, id.to_owned());
    }

    /// Carries what was kept for `old` over to `new` (a renamed folder
    /// workspace's id changes).
    pub fn rename_id(&mut self, old: &str, new: &str) {
        for o in &mut self.order {
            if o == old {
                *o = new.to_owned();
            }
        }
        if self.pinned.remove(old) {
            self.pinned.insert(new.to_owned());
        }
        if let Some(title) = self.titles.remove(old) {
            self.titles.insert(new.to_owned(), title);
        }
        if self.collapsed.remove(old) {
            self.collapsed.insert(new.to_owned());
        }
        if self.merged_open.remove(old) {
            self.merged_open.insert(new.to_owned());
        }
    }
}

/// A workspace row being dragged to a new place in the list.
#[derive(Clone, Debug, PartialEq)]
pub struct RowDrag {
    pub id: String,
    pub start_y: f32,
    pub y: f32,
    /// Moved far enough to be a drag, not a click.
    pub active: bool,
}

/// One per line: the name, a tab, the folder, a tab, the key, and for a
/// remote workspace a tab and its host. A line written before keys existed
/// keeps the id it had, `folder:` and its name, so the sessions holding it
/// stay in it.
fn parse_folders(text: &str) -> Vec<Workspace> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, '\t');
            let (name, path) = (parts.next()?, parts.next()?);
            let key = parts
                .next()
                .filter(|k| !k.is_empty())
                .map_or_else(|| format!("folder:{name}"), str::to_owned);
            let host = parts.next().filter(|h| !h.is_empty()).map(str::to_owned);
            Some((name, path, key, host))
        })
        .filter(|(name, path, ..)| !name.is_empty() && !path.is_empty())
        .map(|(name, path, key, host)| Workspace {
            name: name.to_owned(),
            path: PathBuf::from(path),
            branch: None,
            main: false,
            folder: true,
            key,
            repo: None,
            base: None,
            commit: None,
            merged: false,
            host,
            github: None,
        })
        .collect()
}

fn folders_text(folders: &[Workspace]) -> String {
    folders
        .iter()
        .map(|w| match &w.host {
            Some(host) => format!("{}\t{}\t{}\t{host}\n", w.name, w.path.display(), w.key),
            None => format!("{}\t{}\t{}\n", w.name, w.path.display(), w.key),
        })
        .collect()
}

/// Whether a workspace's name is the one crc gave it: its folder's, or
/// that with a number after it to tell two apart.
fn named_after(name: &str, path: &Path) -> bool {
    let Some(base) = path.file_name().map(|n| n.to_string_lossy()) else {
        return false;
    };
    name == base
        || name
            .strip_prefix(&*base)
            .and_then(|rest| rest.strip_prefix(' '))
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// A key no workspace has had: the time and a count, so two made in the
/// same instant differ too.
fn new_key(taken: &[Workspace]) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    (0u32..)
        .map(|n| {
            format!(
                "ws:{now:x}{}",
                if n == 0 {
                    String::new()
                } else {
                    format!("-{n}")
                }
            )
        })
        .find(|key| !taken.iter().any(|w| w.key == *key))
        .expect("an unused key")
}

/// What a task's row says besides its name, read after the list: commits
/// ahead of the default branch, and its pull request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Info {
    pub ahead: usize,
    pub pull_request: Option<crate::project::worktree::PullRequest>,
}

/// What `gh` said about a row's pull request: it, none, or a failure.
type PullRequestAnswer = Result<Option<crate::project::worktree::PullRequest>, String>;

/// What `gh` said about a repository's open issues: them, and whether
/// the list stopped at [`crate::project::worktree::ISSUE_LIMIT`].
type IssuesAnswer = Result<(Vec<crate::project::worktree::Issue>, bool), IssueError>;

/// One issue read whole, body and comments, for its tab.
pub type IssueAnswer = Result<crate::project::worktree::Issue, IssueError>;

/// How long a repository's issue list waits between reads.
pub const ISSUES_EVERY: std::time::Duration = std::time::Duration::from_secs(300);

/// What was read of a repository's open issues on GitHub. Read only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueSet {
    /// The open issues, as `gh` lists them (no bodies), from the last read
    /// that worked.
    pub list: Vec<crate::project::worktree::Issue>,
    /// The list stopped at its limit: there may be more.
    pub capped: bool,
    /// Why the last read failed; the list is still the one before.
    pub error: Option<IssueError>,
    /// When `gh` last answered, either way.
    pub read_at: std::time::Instant,
}

impl IssueSet {
    /// How many there are: `12`, or `100+` when the list stopped.
    pub fn count(&self) -> String {
        format!("{}{}", self.list.len(), if self.capped { "+" } else { "" })
    }
}

/// An issue tab's title: `#42 Fix the login`, the title cut to fit a tab.
pub fn issue_tab_label(number: u64, title: &str) -> String {
    const COLUMNS: usize = 36;
    let title = title.trim();
    let shown = crate::text::columns::fit_columns(title.chars(), COLUMNS);
    if shown.len() < title.len() {
        format!("#{number} {}\u{2026}", shown.trim_end())
    } else {
        format!("#{number} {title}")
    }
}

/// Where Start Task leaves issue `number` of `repo` for Claude to read:
/// under `dir` (Application Support), out of every repository, named for
/// the repository by a hash so two of the same name never share one.
pub fn issue_file(dir: &Path, repo: &RepoKey, number: u64) -> PathBuf {
    let key = format!(
        "{}\n{}",
        repo.0.as_deref().unwrap_or(""),
        repo.1.to_string_lossy()
    );
    dir.join("issues").join(format!(
        "{:016x}-{number}.md",
        crate::platform::fnv1a(key.as_bytes())
    ))
}

/// Writes `text` to [`issue_file`], readable by you alone (0600), and
/// returns where.
pub fn write_issue_file(
    dir: &Path,
    repo: &RepoKey,
    number: u64,
    text: &str,
) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let path = issue_file(dir, repo, number);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    // One written before with a looser mode keeps it through `mode`.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(text.as_bytes())?;
    Ok(path)
}

/// What Claude is first told in an issue's task. Only the number and the
/// file's path: the issue's own words stay in the file, never on a
/// command line.
pub fn issue_prompt(file: &Path, number: u64) -> String {
    format!(
        "Read the GitHub issue in {} and work on issue #{number}.",
        file.display()
    )
}

/// `command` with `prompt`, when there is one, after it as one argument
/// quoted for `sh`.
pub fn with_prompt(command: &str, prompt: Option<&str>) -> String {
    match prompt {
        Some(prompt) => format!("{command} {}", crate::project::review::shell_quote(prompt)),
        None => command.to_owned(),
    }
}

/// The worktree in `list` that has `branch` checked out: an issue's task
/// started before, which Start Task goes back to rather than making again.
pub fn worktree_on_branch(list: &[Worktree], branch: &str) -> Option<PathBuf> {
    list.iter()
        .find(|w| !w.main && w.branch.as_deref() == Some(branch))
        .map(|w| w.path.clone())
}

/// How long the open rows wait between pull request reads: shorter while
/// an open pull request's checks run, longest when none is open.
pub fn pr_wait<'a>(
    prs: impl IntoIterator<Item = &'a crate::project::worktree::PullRequest>,
) -> std::time::Duration {
    use crate::project::worktree::Checks;
    let open: Vec<_> = prs.into_iter().filter(|pr| pr.state == "OPEN").collect();
    std::time::Duration::from_secs(if open.iter().any(|pr| pr.checks == Checks::Running) {
        20
    } else if !open.is_empty() {
        60
    } else {
        300
    })
}

/// How long a merged or closed pull request waits between reads: it
/// rarely changes again.
pub const PR_SETTLED: std::time::Duration = std::time::Duration::from_secs(600);

/// Whether a row's pull request is due a read: never read yet, settled and
/// [`PR_SETTLED`] old, or else `wait` old.
pub fn pr_due(
    pr: Option<&crate::project::worktree::PullRequest>,
    since: Option<std::time::Duration>,
    wait: std::time::Duration,
) -> bool {
    let Some(since) = since else {
        return true;
    };
    match pr {
        Some(pr) if pr.state == "MERGED" || pr.state == "CLOSED" => since >= PR_SETTLED,
        _ => since >= wait,
    }
}

/// The shortest wait, which the periodic read ticks at; a self-test may
/// make it shorter with `CRC_SELFTEST_PR_EVERY` (seconds).
pub fn pr_tick() -> std::time::Duration {
    pr_every_override().unwrap_or(std::time::Duration::from_secs(20))
}

fn pr_every_override() -> Option<std::time::Duration> {
    std::env::var_os("CRC_SELFTEST")?;
    let secs: f64 = std::env::var("CRC_SELFTEST_PR_EVERY").ok()?.parse().ok()?;
    (secs > 0.0).then(|| std::time::Duration::from_secs_f64(secs))
}

/// Whether pull requests are asked of `gh`: always in use; in a test
/// instance only when it says so with `CRC_SELFTEST_GH`, with a stand-in
/// `gh` on its PATH.
fn github_allowed() -> bool {
    std::env::var_os("CRC_SELFTEST").is_none() || std::env::var_os("CRC_SELFTEST_GH").is_some()
}

/// The branches a change under a repository's Git directory `git_dir` may
/// have pushed: a remote-tracking ref's (`refs/remotes/<remote>/<branch>`);
/// `None` when it cannot tell which (packed refs rewritten, or events
/// dropped), so every branch may have.
pub fn pushed_branches(git_dir: &Path, files: &[PathBuf]) -> Option<Vec<String>> {
    if files.is_empty() {
        return None;
    }
    let mut branches = Vec::new();
    for file in files {
        let Ok(inside) = file.strip_prefix(git_dir) else {
            continue;
        };
        if inside == Path::new("packed-refs") {
            return None;
        }
        let names: Vec<String> = inside
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        if names.len() < 4
            || names[0] != "refs"
            || names[1] != "remotes"
            || names.last().is_some_and(|n| n.ends_with(".lock"))
        {
            continue;
        }
        let branch = names[3..].join("/");
        if !branches.contains(&branch) {
            branches.push(branch);
        }
    }
    Some(branches)
}

/// A write this recent counts as writing now.
pub const WRITING: std::time::Duration = std::time::Duration::from_secs(8);

/// What a worktree has been doing, for its row: agents running from
/// another session (a background agent) leave no terminal here, only
/// their writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pace {
    /// Something wrote in it within [`WRITING`].
    Writing,
    /// The last write seen this run, this long ago.
    Quiet(std::time::Duration),
    /// No write seen this run: its branch's last commit, this long ago.
    Committed(std::time::Duration),
    Unknown,
}

impl Pace {
    /// From the time since the last write seen and since the branch's last
    /// commit.
    pub fn of(
        since_write: Option<std::time::Duration>,
        since_commit: Option<std::time::Duration>,
    ) -> Pace {
        match (since_write, since_commit) {
            (Some(w), _) if w < WRITING => Pace::Writing,
            (Some(w), _) => Pace::Quiet(w),
            (None, Some(c)) => Pace::Committed(c),
            (None, None) => Pace::Unknown,
        }
    }

    /// What the row says: `writing`, `quiet · 5 min`, `commit 2 h ago`.
    pub fn label(self) -> Option<String> {
        match self {
            Pace::Writing => Some("writing".into()),
            Pace::Quiet(d) => Some(format!("quiet \u{b7} {}", span(d))),
            Pace::Committed(d) => Some(format!("commit {} ago", span(d))),
            Pace::Unknown => None,
        }
    }

    /// For the self-test: `writing`, `quiet`, `commit` or `-`.
    pub fn word(self) -> &'static str {
        match self {
            Pace::Writing => "writing",
            Pace::Quiet(_) => "quiet",
            Pace::Committed(_) => "commit",
            Pace::Unknown => "-",
        }
    }
}

/// A time span the way a row says it: `<1 min`, `5 min`, `2 h`, `3 d`.
pub fn span(d: std::time::Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => "<1 min".into(),
        60..3600 => format!("{} min", s / 60),
        3600..86400 => format!("{} h", s / 3600),
        _ => format!("{} d", s / 86400),
    }
}

/// The line under a worktree's name when no session in it is waiting,
/// working or in ssh: whether it is merged, what it has been doing, its
/// terminals (none is not said: a background agent may be writing in
/// it), and how far ahead of the default branch it is.
pub fn worktree_line(pace: Pace, terminals: usize, ahead: usize, merged: bool) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    // Merged is what matters then; how long it has been quiet does not.
    if merged {
        parts.push("merged".into());
    }
    if !merged || matches!(pace, Pace::Writing) {
        parts.extend(pace.label());
    }
    match terminals {
        0 => {}
        1 => parts.push("1 terminal".into()),
        n => parts.push(format!("{n} terminals")),
    }
    if ahead > 0 && !merged {
        parts.push(format!("{ahead} ahead"));
    }
    parts
}

/// A task on a session's shared list, as Claude Code's agent teams keep
/// one: the lead and its teammates claim and finish them, and the hooks
/// TaskCreated and TaskCompleted tell the sidebar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamTask {
    pub id: String,
    pub subject: String,
    pub done: bool,
    /// Who made or finished it, when the hook named one.
    pub teammate: Option<String>,
}

/// What a session's hooks have said about its team: the tasks in the
/// order they were made, and the teammates with nothing left to do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Team {
    pub tasks: Vec<TeamTask>,
    pub idle: std::collections::BTreeSet<String>,
}

impl Team {
    /// Takes in what a hook wrote; `false` for an event that is not
    /// about the team.
    pub fn apply(&mut self, event: &crate::project::review::Event) -> bool {
        use crate::project::review::Event;
        match event {
            Event::Task {
                id,
                subject,
                done,
                teammate,
            } => {
                if let Some(who) = teammate {
                    self.idle.remove(who);
                }
                match self.tasks.iter_mut().find(|t| t.id == *id) {
                    Some(task) => {
                        task.done = *done;
                        if !subject.is_empty() {
                            task.subject = subject.clone();
                        }
                        if teammate.is_some() {
                            task.teammate = teammate.clone();
                        }
                    }
                    None => self.tasks.push(TeamTask {
                        id: id.clone(),
                        subject: subject.clone(),
                        done: *done,
                        teammate: teammate.clone(),
                    }),
                }
                true
            }
            Event::TeammateIdle(name) => {
                if !name.is_empty() {
                    self.idle.insert(name.clone());
                }
                true
            }
            _ => false,
        }
    }

    pub fn open(&self) -> usize {
        self.tasks.iter().filter(|t| !t.done).count()
    }
}

#[derive(Default)]
pub struct Panel {
    /// Agents mode is on: the terminal has the column, this the sidebar.
    pub on: bool,
    /// The kept workspaces were read; until then nothing is written over
    /// them.
    loaded: bool,
    /// Per folder workspace (by id), the repositories found in it, once
    /// read.
    pub repos: std::collections::HashMap<String, Vec<PathBuf>>,
    /// Every workspace, the folder ones first, then the worktrees in them.
    pub tasks: Vec<Workspace>,
    worktrees: Vec<Workspace>,
    pub folders: Vec<Workspace>,
    /// The workspace whose sessions the terminal shows, by
    /// [`Workspace::id`].
    pub selected: Option<String>,
    /// The session a context menu was opened on.
    pub menu_session: Option<usize>,
    /// The folder workspace the palette is renaming, by id.
    pub renaming: Option<String>,
    /// The repository an inspector button acts on, when the workspace has
    /// several; taken by the operation.
    pub target: Option<PathBuf>,
    rx: Option<mpsc::Receiver<Listing>>,
    /// This Mac's part of the read [`Panel::rx`] brings is still to come.
    reading_local: bool,
    /// The remote folder workspaces, by id, whose host did not answer the
    /// last read.
    pub unreachable: std::collections::HashSet<String>,
    /// Per worktree, by [`Workspace::id`] (a host's are `host:path`, never
    /// taken for one of this Mac's at the same path), once read.
    pub info: std::collections::HashMap<String, Info>,
    /// Per main checkout, the worktrees Git records whose folder is gone.
    pub prunable: Prunable,
    /// Per worktree id, its commits ahead, from the list's read.
    info_rx: Option<mpsc::Receiver<(String, usize)>>,
    /// The pull request reads, one `gh` after another on one worker.
    pr_rx: Option<mpsc::Receiver<(String, PullRequestAnswer)>>,
    /// Rows asked for while a read ran, read after it, by id.
    pr_wanted: Vec<String>,
    /// Per worktree id, when `gh` last answered about its pull request.
    pr_read: std::collections::HashMap<String, std::time::Instant>,
    /// Per repository, its open issues on GitHub, once read.
    pub issues: std::collections::HashMap<RepoKey, IssueSet>,
    /// The issue list reads, one repository after another on a worker of
    /// their own, so they never hold the pull requests back.
    issues_rx: Option<mpsc::Receiver<(RepoKey, IssuesAnswer)>>,
    /// Repositories asked for while a read ran, read after it.
    issues_wanted: Vec<RepoKey>,
    /// The one issue being read whole, for its tab: the latest asked.
    issue_rx: Option<mpsc::Receiver<(RepoKey, IssueAnswer)>>,
    /// Per main checkout on a host, its repository on GitHub, from its
    /// origin.
    pub github_repos: std::collections::HashMap<RepoKey, String>,
    /// A watcher per main checkout's Git directory, for pushes, and the
    /// main checkouts they are for.
    pub ref_watchers: Vec<crate::project::watch::Watcher>,
    pub ref_watched: Vec<PathBuf>,
    /// Worktrees (by id) whose branch was just pushed, by when: their pull
    /// request is read once the push has settled.
    pub pushed: std::collections::HashMap<String, std::time::Instant>,
    /// The worktree (by id) a pull request operation or a push runs on:
    /// its pull request is read again when the operation answers.
    pub pr_after: Option<String>,
    /// Per session (by its review folder), what its team hooks said.
    pub teams: std::collections::HashMap<PathBuf, Team>,
    /// Per worktree path, when a file in it was last written this run: by
    /// a crc-launched agent's hook, or seen by the worktree's watcher.
    pub wrote: std::collections::HashMap<PathBuf, std::time::Instant>,
    /// A watcher per worktree in the list, and the paths they watch.
    pub watchers: Vec<crate::project::watch::Watcher>,
    pub watched: Vec<PathBuf>,
    /// An operation on a task running on a worker, and what it says.
    pub op: Option<(String, mpsc::Receiver<Result<Done, String>>)>,
    /// The workspace a menu was opened on, by [`Workspace::id`]: the list
    /// can reorder while the menu is up.
    pub menu_task: Option<String>,
    /// The column at the right: the selected workspace's Git state.
    pub inspector: crate::platform::inspector::Inspector,
    /// When the worktrees and pull requests were last read.
    pub refreshed_at: Option<std::time::Instant>,
    /// Whether `gh` is installed, looked up once.
    pub github: Option<bool>,
    /// The periodic refresh is scheduled.
    pub ticking: bool,
    /// The two-second look at the terminals (ssh in or out) is scheduled.
    pub sessions_ticking: bool,
    /// Your order, pins and names, kept for the project.
    pub arrangement: Arrangement,
    /// A row being dragged.
    pub drag: Option<RowDrag>,
    /// The selected workspace its folded merged group was last opened for,
    /// by id, so folding it again by hand sticks.
    revealed: Option<String>,
    /// The workspace whose name is being edited in its row, by id, with
    /// the text so far and the caret's character index; set by the window
    /// from its field before each draw.
    pub inline: Option<(String, String, usize)>,
    pub scroll: usize,
    pub hits: Vec<(Viewport, Action)>,
}

/// A task operation that worked: what to say, and a folder to start a
/// terminal in (a new task's), with its host when it is on one, and what
/// to tell Claude when Claude starts there instead of a shell.
pub struct Done {
    pub said: String,
    pub start_in: Option<(Option<String>, PathBuf)>,
    pub claude: Option<String>,
}

impl Panel {
    /// Runs `work` on a worker, saying `doing` at the foot of the list
    /// until it answers.
    pub fn start(
        &mut self,
        doing: &str,
        work: impl FnOnce() -> Result<Done, String> + Send + 'static,
    ) {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(work());
        });
        self.op = Some((doing.to_owned(), rx));
    }

    /// The operation's answer, once it has one.
    pub fn take_done(&mut self) -> Option<Result<Done, String>> {
        let (_, rx) = self.op.as_ref()?;
        let answer = match rx.try_recv() {
            Ok(answer) => answer,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => Err("The task operation stopped".into()),
        };
        self.op = None;
        Some(answer)
    }
}

enum Line {
    Task(usize),
    /// A worktree under the folder workspace holding its repository.
    Child(usize),
    /// The fold over the merged worktrees under the folder workspace at
    /// this index.
    Merged(usize),
    Session(usize),
    /// A task on the team list of the session at the first index.
    TeamTask(usize, usize),
    Heading(&'static str),
    Note(String),
}

/// The workspace a session belongs to: the one it was started in, when
/// that still exists, else the one holding its folder.
pub fn task_of(tasks: &[Workspace], tab: &Tab) -> Option<usize> {
    if let Some(id) = &tab.workspace
        && let Some(i) = tasks.iter().position(|t| t.id() == *id)
    {
        return Some(i);
    }
    folder_task(tasks, &tab.folder)
}

/// The workspace holding a folder: the deepest one, so a worktree inside
/// the main checkout is not taken for the main one; a worktree before a
/// folder workspace on the same folder; of folder workspaces on one
/// folder, the first in the list, so a new one never takes the sessions of
/// the others. A remote workspace's folder is not on this Mac: it holds
/// only the sessions started in it.
pub fn folder_task(tasks: &[Workspace], folder: &Path) -> Option<usize> {
    tasks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.host.is_none() && folder.starts_with(&t.path))
        .max_by_key(|(i, t)| {
            (
                t.path.components().count(),
                !t.folder,
                std::cmp::Reverse(*i),
            )
        })
        .map(|(i, _)| i)
}

/// What a terminal's workspace does when its shell is now in `cwd`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Follow {
    /// It is still where the workspace is.
    Stay,
    /// The folder workspace moves there with it.
    Move,
    /// The terminal goes to the workspace at this index, which holds `cwd`.
    Join(usize),
    /// Nothing holds `cwd`: a new folder workspace is made on it.
    Make,
}

/// Where a terminal goes when it `cd`s, from the workspace `owner` that
/// has it. Into another worktree (one inside the main checkout too), the
/// terminal joins it; a worktree keeps the rest of its own folders; a
/// folder workspace moves with it; else it joins whatever holds the folder,
/// else a new one is made there. A remote workspace's terminal is ssh, in a
/// folder of this Mac that says nothing: it stays.
pub fn follow(tasks: &[Workspace], owner: usize, cwd: &Path) -> Follow {
    let task = &tasks[owner];
    if task.path == cwd || task.host.is_some() {
        return Follow::Stay;
    }
    match folder_task(tasks, cwd) {
        Some(h) if h != owner && !tasks[h].folder => Follow::Join(h),
        _ if !task.folder && cwd.starts_with(&task.path) => Follow::Stay,
        _ if task.folder => Follow::Move,
        Some(h) if h != owner => Follow::Join(h),
        _ => Follow::Make,
    }
}

impl Panel {
    /// Reads the kept workspaces once, then on a worker the repositories
    /// in each folder workspace, their worktrees and how far ahead each
    /// is. Pull requests are read on their own (see
    /// [`Panel::refresh_pull_requests`]): this is only Git.
    pub fn refresh(&mut self) {
        self.refreshed_at = Some(std::time::Instant::now());
        self.inspector.invalidate();
        if self.github.is_none() {
            self.github = Some(crate::project::worktree::has_gh());
        }
        if !self.loaded {
            self.load();
        }
        let (tx, rx) = mpsc::channel();
        let (info_tx, info_rx) = mpsc::channel();
        // This Mac's folders on one worker, each host's on its own: a host
        // that is slow or gone holds back only its own rows. A remote
        // folder's Git is read on its host, over ssh.
        let mut hosts: Vec<Option<String>> = vec![None];
        for host in self.folders.iter().filter_map(|f| f.host.clone()) {
            if !hosts.contains(&Some(host.clone())) {
                hosts.push(Some(host));
            }
        }
        for host in hosts {
            let folders: Vec<(String, PathBuf)> = self
                .folders
                .iter()
                .filter(|f| f.host == host)
                .map(|f| (f.id(), f.path.clone()))
                .collect();
            let (tx, info_tx) = (tx.clone(), info_tx.clone());
            std::thread::spawn(move || {
                let listing = scan(&folders, host.as_deref());
                let tasks = listing.worktrees.clone();
                if tx.send(listing).is_err() {
                    return;
                }
                drop(tx);
                // Then how far ahead each row is, a repository at a time.
                let runner = runner(host.as_deref());
                let mut repos: Vec<(PathBuf, Option<String>)> = Vec::new();
                for task in &tasks {
                    if let Some(main) = &task.repo
                        && !repos.iter().any(|(r, _)| r == main)
                    {
                        repos.push((main.clone(), task.base.clone()));
                    }
                }
                for (main, base) in repos {
                    let rows: Vec<(String, String)> = tasks
                        .iter()
                        .filter(|t| t.repo.as_ref() == Some(&main))
                        .filter_map(|t| Some((t.id(), t.branch.clone()?)))
                        .collect();
                    let branches: Vec<String> = rows.iter().map(|(_, b)| b.clone()).collect();
                    // A host that does not answer keeps the counts it had.
                    let Ok(aheads) = crate::project::worktree::aheads_on(
                        &runner,
                        &main,
                        base.as_deref(),
                        &branches,
                    ) else {
                        continue;
                    };
                    for ((id, _), ahead) in rows.into_iter().zip(aheads) {
                        if info_tx.send((id, ahead)).is_err() {
                            return;
                        }
                    }
                }
            });
        }
        self.rx = Some(rx);
        self.reading_local = true;
        self.info_rx = Some(info_rx);
    }

    /// Whether `gh` is asked about issues and pull requests, as far as is
    /// known: not before it was looked for.
    pub fn github_asked(&self) -> bool {
        self.github == Some(true) && github_allowed()
    }

    /// Whether pull requests can be asked about: `gh` is there, and this
    /// is not a test instance that keeps away from it.
    fn asks_github(&mut self) -> bool {
        if self.github.is_none() {
            self.github = Some(crate::project::worktree::has_gh());
        }
        self.github == Some(true) && github_allowed()
    }

    /// The worktrees with a branch, whose pull requests can be read: this
    /// Mac's, and a host's whose origin is on GitHub (`gh` runs here,
    /// told the repository).
    fn pr_rows(&self) -> impl Iterator<Item = &Workspace> {
        self.worktrees.iter().filter(|w| {
            w.branch.is_some() && w.repo.is_some() && (w.host.is_none() || w.github.is_some())
        })
    }

    /// Reads the pull requests that are due (see [`pr_due`]), when no read
    /// is running already.
    pub fn refresh_pull_requests(&mut self) {
        if self.pr_rx.is_some() {
            return;
        }
        let override_wait = pr_every_override();
        let wait = override_wait.unwrap_or_else(|| {
            pr_wait(
                self.pr_rows()
                    .filter_map(|w| self.info.get(&w.id())?.pull_request.as_ref()),
            )
        });
        let due: Vec<String> = self
            .pr_rows()
            .filter(|w| {
                let id = w.id();
                let pr = self.info.get(&id).and_then(|i| i.pull_request.as_ref());
                let since = self.pr_read.get(&id).map(|at| at.elapsed());
                pr_due(pr, since, wait)
            })
            .map(Workspace::id)
            .collect();
        self.read_pull_requests(due);
    }

    /// Reads every row's pull request again: Refresh, asked for.
    pub fn refresh_all_pull_requests(&mut self) {
        let all = self.pr_rows().map(Workspace::id).collect();
        self.read_pull_requests(all);
    }

    /// Reads the pull request of the worktree `id` again soon: after it
    /// was pushed, or a pull request operation on it answered.
    pub fn refresh_pull_request(&mut self, id: &str) {
        self.read_pull_requests(vec![id.to_owned()]);
    }

    /// A remote-tracking ref of the repository whose main checkout is
    /// `main` moved: the worktrees on `branches` (all of its worktrees
    /// for `None`) were pushed, and are read once it settles. Returns
    /// whether any was. Only this Mac's: a host's Git is not watched.
    pub fn note_push(&mut self, main: &Path, branches: Option<&[String]>) -> bool {
        let now = std::time::Instant::now();
        let rows: Vec<String> = self
            .pr_rows()
            .filter(|w| w.host.is_none() && w.repo.as_deref() == Some(main))
            .filter(|w| {
                branches.is_none_or(|b| w.branch.as_ref().is_some_and(|own| b.contains(own)))
            })
            .map(Workspace::id)
            .collect();
        for id in &rows {
            self.pushed.insert(id.clone(), now);
        }
        !rows.is_empty()
    }

    /// Reads the pull requests of the rows pushed at least `settle` ago.
    /// Returns whether any push is still settling.
    pub fn read_pushed(&mut self, settle: std::time::Duration) -> bool {
        let ready: Vec<String> = self
            .pushed
            .iter()
            .filter(|(_, at)| at.elapsed() >= settle)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &ready {
            self.pushed.remove(id);
        }
        self.read_pull_requests(ready);
        !self.pushed.is_empty()
    }

    /// The main checkouts of this Mac's worktrees in the list, for the
    /// watchers that notice a push.
    pub fn main_checkouts(&self) -> Vec<PathBuf> {
        let mut mains: Vec<PathBuf> = Vec::new();
        for main in self
            .pr_rows()
            .filter(|w| w.host.is_none())
            .filter_map(|w| w.repo.clone())
        {
            if !mains.contains(&main) {
                mains.push(main);
            }
        }
        mains
    }

    /// Asks `gh` about the rows `ids`, one after another on a worker; while
    /// a read runs they wait for it. A host's row is asked by its GitHub
    /// repository's name, `gh` running here.
    fn read_pull_requests(&mut self, ids: Vec<String>) {
        use crate::project::worktree::GitHub;
        if ids.is_empty() || !self.asks_github() {
            return;
        }
        if self.pr_rx.is_some() {
            for id in ids {
                if !self.pr_wanted.contains(&id) {
                    self.pr_wanted.push(id);
                }
            }
            return;
        }
        let rows: Vec<(String, GitHub, String)> = self
            .pr_rows()
            .filter(|w| ids.contains(&w.id()))
            .filter_map(|w| {
                let repo = match &w.host {
                    Some(_) => GitHub::Repo(w.github.clone()?),
                    None => GitHub::Dir(w.path.clone()),
                };
                Some((w.id(), repo, w.branch.clone()?))
            })
            .collect();
        if rows.is_empty() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for (id, repo, branch) in rows {
                let answer = crate::project::worktree::pull_request_in(&repo, &branch);
                if tx.send((id, answer)).is_err() {
                    return;
                }
            }
        });
        self.pr_rx = Some(rx);
    }

    /// Takes what `gh` said about the row `id`: a failure keeps the pull
    /// request it last had, so its badge does not come and go with the
    /// network. Returns whether the row changed.
    pub fn take_pull_request(&mut self, id: String, answer: PullRequestAnswer) -> bool {
        self.pr_read.insert(id.clone(), std::time::Instant::now());
        let Ok(pr) = answer else {
            return false;
        };
        let info = self.info.entry(id).or_default();
        let changed = info.pull_request != pr;
        info.pull_request = pr;
        changed
    }

    /// The repositories the selected workspace's inspector shows, by host
    /// and main checkout: a worktree's repository, or those found in a
    /// folder workspace (none until read).
    pub fn selected_repos(&self) -> Vec<RepoKey> {
        let Some(task) = self.selected_index().map(|i| &self.tasks[i]) else {
            return Vec::new();
        };
        let repos = match &task.repo {
            Some(repo) if !task.folder => vec![repo.clone()],
            _ => self.repos_of(task).unwrap_or_default(),
        };
        repos
            .into_iter()
            .map(|repo| (task.host.clone(), repo))
            .collect()
    }

    /// Every repository in the list, by host and main checkout.
    fn repo_keys(&self) -> std::collections::HashSet<RepoKey> {
        let found = self.folders.iter().flat_map(|f| {
            self.repos
                .get(&f.id())
                .into_iter()
                .flatten()
                .map(|r| (f.host.clone(), r.clone()))
        });
        let held = self
            .worktrees
            .iter()
            .filter_map(|w| Some((w.host.clone(), w.repo.clone()?)));
        found.chain(held).collect()
    }

    /// The issues read of the repository `repo`, once read.
    pub fn issues_of(&self, repo: &RepoKey) -> Option<&IssueSet> {
        self.issues.get(repo)
    }

    /// Where `gh` finds the repository `repo`: this Mac's in its checkout,
    /// a host's by its GitHub name; `None` for a host's not on GitHub.
    fn issue_target(&self, repo: &RepoKey) -> Option<crate::project::worktree::GitHub> {
        use crate::project::worktree::GitHub;
        match &repo.0 {
            None => Some(GitHub::Dir(repo.1.clone())),
            Some(_) => self.github_repos.get(repo).cloned().map(GitHub::Repo),
        }
    }

    /// The selected workspace's repositories whose issues are due at
    /// `now`: never read, or read [`ISSUES_EVERY`] ago.
    fn issues_due(&self, now: std::time::Instant) -> Vec<RepoKey> {
        self.selected_repos()
            .into_iter()
            .filter(|repo| {
                self.issues
                    .get(repo)
                    .is_none_or(|set| now.saturating_duration_since(set.read_at) >= ISSUES_EVERY)
            })
            .collect()
    }

    /// Reads the issues of the selected workspace's repositories that are
    /// due, when no read is running already. Only those: another
    /// workspace's are read when it is selected.
    pub fn refresh_issues(&mut self) {
        if self.issues_rx.is_some() {
            return;
        }
        let due = self.issues_due(std::time::Instant::now());
        self.read_issues(due);
    }

    /// Reads the issues of `repo` now, due or not: on selecting it, or
    /// Refresh.
    pub fn want_issues_now(&mut self, repo: RepoKey) {
        self.read_issues(vec![repo]);
    }

    /// Asks `gh` for the open issues of `repos`, one after another on a
    /// worker; while a read runs they wait for it. A host's repository
    /// not on GitHub is answered so without asking.
    fn read_issues(&mut self, repos: Vec<RepoKey>) {
        use crate::project::worktree::{ISSUE_LIMIT, issues_in};
        if repos.is_empty() || !self.asks_github() {
            return;
        }
        if self.issues_rx.is_some() {
            for repo in repos {
                if !self.issues_wanted.contains(&repo) {
                    self.issues_wanted.push(repo);
                }
            }
            return;
        }
        let jobs: Vec<_> = repos
            .into_iter()
            .map(|repo| {
                let target = self.issue_target(&repo);
                (repo, target)
            })
            .collect();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for (repo, target) in jobs {
                let answer = match target {
                    Some(target) => issues_in(&target, ISSUE_LIMIT).map(|list| {
                        let capped = list.len() >= ISSUE_LIMIT;
                        (list, capped)
                    }),
                    None => Err(IssueError::NotGitHub),
                };
                if tx.send((repo, answer)).is_err() {
                    return;
                }
            }
        });
        self.issues_rx = Some(rx);
    }

    /// Takes what `gh` said about `repo`'s issues: a failure keeps the
    /// list last read and says why. Returns whether anything shown
    /// changed.
    pub fn take_issues(&mut self, repo: RepoKey, answer: IssuesAnswer) -> bool {
        let read_at = std::time::Instant::now();
        let fresh = match (answer, self.issues.get(&repo)) {
            (Ok((list, capped)), _) => IssueSet {
                list,
                capped,
                error: None,
                read_at,
            },
            (Err(error), Some(old)) => IssueSet {
                error: Some(error),
                read_at,
                ..old.clone()
            },
            (Err(error), None) => IssueSet {
                list: Vec::new(),
                capped: false,
                error: Some(error),
                read_at,
            },
        };
        let changed = self.issues.get(&repo).is_none_or(|old| {
            (&old.list, old.capped, &old.error) != (&fresh.list, fresh.capped, &fresh.error)
        });
        self.issues.insert(repo, fresh);
        changed
    }

    fn poll_issues(&mut self) -> bool {
        let mut changed = false;
        while let Some(rx) = &self.issues_rx {
            match rx.try_recv() {
                Ok((repo, answer)) => changed |= self.take_issues(repo, answer),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.issues_rx = None;
                    let wanted = std::mem::take(&mut self.issues_wanted);
                    self.read_issues(wanted);
                }
            }
        }
        changed
    }

    /// Reads issue `number` of `repo` whole, body and comments, on a
    /// worker; [`Panel::take_issue_detail`] has the answer. Asking again
    /// drops the read before.
    pub fn request_issue(&mut self, repo: RepoKey, number: u64) {
        if !self.asks_github() {
            return;
        }
        let target = self.issue_target(&repo);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let answer = match target {
                Some(target) => crate::project::worktree::issue_in(&target, number),
                None => Err(IssueError::NotGitHub),
            };
            let _ = tx.send((repo, answer));
        });
        self.issue_rx = Some(rx);
    }

    /// The issue [`Panel::request_issue`] read, once it answers.
    pub fn take_issue_detail(&mut self) -> Option<(RepoKey, IssueAnswer)> {
        let rx = self.issue_rx.as_ref()?;
        let answer = match rx.try_recv() {
            Ok(answer) => answer,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.issue_rx = None;
                return None;
            }
        };
        self.issue_rx = None;
        Some(answer)
    }

    /// Whether an issue is being read whole.
    pub fn reading_issue(&self) -> bool {
        self.issue_rx.is_some()
    }

    /// Forgets what was read about workspaces no longer in the list, and
    /// the issues of repositories no longer in it.
    fn prune(&mut self) {
        let listed: std::collections::HashSet<String> =
            self.tasks.iter().map(Workspace::id).collect();
        self.info.retain(|id, _| listed.contains(id));
        self.pr_read.retain(|id, _| listed.contains(id));
        self.pushed.retain(|id, _| listed.contains(id));
        self.pr_wanted.retain(|id| listed.contains(id));
        let repos = self.repo_keys();
        self.issues.retain(|repo, _| repos.contains(repo));
        self.issues_wanted.retain(|repo| repos.contains(repo));
    }

    /// The kept workspaces and arrangement; the first time, every
    /// project's from before the list was one, carried over.
    fn load(&mut self) {
        self.loaded = true;
        match folders_file().and_then(|f| std::fs::read_to_string(f).ok()) {
            Some(text) => {
                self.folders = parse_folders(&text);
                self.arrangement = order_file()
                    .and_then(|f| std::fs::read_to_string(f).ok())
                    .map(|text| Arrangement::parse(&text))
                    .unwrap_or_default();
            }
            None => {
                self.folders = merge_folders(&per_project_files("workspaces"));
                self.arrangement = merge_arrangements(&per_project_files("order"));
                if !self.folders.is_empty() {
                    self.save_folders();
                    self.save_arrangement();
                }
            }
        }
        self.rebuild();
    }

    /// Whether a worktree's work is done: its branch is on the default
    /// branch, or its pull request was merged.
    pub fn is_merged(&self, task: &Workspace) -> bool {
        task.merged
            || self
                .info
                .get(&task.id())
                .and_then(|i| i.pull_request.as_ref())
                .is_some_and(|pr| pr.state == "MERGED")
    }

    /// A merged worktree, not a repository's own checkout.
    fn done(&self, task: &Workspace) -> bool {
        !task.folder && !task.main && self.is_merged(task)
    }

    /// The folder workspace whose merged group a worktree's row is in, by
    /// id: a merged one under a folder workspace.
    fn merged_group(&self, task: &Workspace) -> Option<String> {
        self.parent_of(task).filter(|_| self.done(task))
    }

    /// The merged worktrees under the folder workspace `id`, by index.
    pub fn merged_under(&self, id: &str) -> Vec<usize> {
        (0..self.tasks.len())
            .filter(|i| self.merged_group(&self.tasks[*i]).as_deref() == Some(id))
            .collect()
    }

    /// Whether the merged worktrees under the folder workspace `id` are
    /// shown.
    pub fn is_merged_open(&self, id: &str) -> bool {
        self.arrangement.merged_open.contains(id)
    }

    /// Shows the merged worktrees under a folder workspace, or folds them
    /// away.
    pub fn toggle_merged(&mut self, id: &str) {
        if !self.arrangement.merged_open.remove(id) {
            if !self.arrangement.order.iter().any(|o| o == id) {
                self.arrangement.order.push(id.to_owned());
            }
            self.arrangement.merged_open.insert(id.to_owned());
        }
        self.save_arrangement();
    }

    /// The selected workspace in a folded merged group (it merged, or a
    /// terminal of it came to the front): the group opens once for it, so
    /// the selection is in sight and not taken away.
    fn reveal_selected(&mut self) {
        let Some(i) = self.selected_index() else {
            return;
        };
        let id = self.tasks[i].id();
        if self.revealed.as_deref() == Some(id.as_str()) {
            return;
        }
        if let Some(group) = self.merged_group(&self.tasks[i]) {
            self.revealed = Some(id);
            if !self.is_merged_open(&group) && !self.is_collapsed(&group) {
                self.toggle_merged(&group);
            }
        }
    }

    /// Takes a finished read. Returns whether anything changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        if let Some(rx) = &self.info_rx {
            loop {
                match rx.try_recv() {
                    Ok((id, ahead)) => {
                        self.info.entry(id).or_default().ahead = ahead;
                        changed = true;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.info_rx = None;
                        break;
                    }
                }
            }
        }
        changed
            | self.poll_pull_requests()
            | self.poll_issues()
            | self.poll_list()
            | self.inspector.poll()
    }

    fn poll_pull_requests(&mut self) -> bool {
        let mut changed = false;
        while let Some(rx) = &self.pr_rx {
            match rx.try_recv() {
                Ok((id, answer)) => changed |= self.take_pull_request(id, answer),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pr_rx = None;
                    let wanted = std::mem::take(&mut self.pr_wanted);
                    self.read_pull_requests(wanted);
                }
            }
        }
        changed
    }

    fn poll_list(&mut self) -> bool {
        let mut changed = false;
        while let Some(rx) = &self.rx {
            match rx.try_recv() {
                Ok(listing) => {
                    self.take_listing(listing);
                    changed = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Every part has answered, or a worker stopped without:
                    // what was read before stays.
                    self.rx = None;
                    self.reading_local = false;
                }
            }
        }
        if changed {
            self.rebuild();
            self.prune();
            // Rows new to the list are read now; the others when due.
            self.refresh_pull_requests();
        }
        changed
    }

    /// Takes one part of a read: this Mac's folder workspaces, or one
    /// host's. The rows of the folders it did not read are kept, and so
    /// are those of a folder whose host did not answer.
    fn take_listing(&mut self, listing: Listing) {
        // The folders and rows this part read again: those on its host.
        let covers = |host: &Option<String>| *host == listing.host;
        let ids: Vec<String> = self
            .folders
            .iter()
            .filter(|f| covers(&f.host))
            .map(Workspace::id)
            .collect();
        let mut kept: Vec<Workspace> = Vec::new();
        let mut kept_mains: Vec<PathBuf> = Vec::new();
        for id in &listing.unreachable {
            let (Some(old), Some(folder)) = (
                self.repos.get(id),
                self.folders.iter().find(|f| f.id() == *id),
            ) else {
                continue;
            };
            kept_mains.extend(old.iter().cloned());
            kept.extend(
                self.worktrees
                    .iter()
                    .filter(|w| w.host == folder.host)
                    .filter(|w| w.repo.as_ref().is_some_and(|r| old.contains(r)))
                    .cloned(),
            );
        }
        for id in &ids {
            if listing.unreachable.contains(id) {
                self.unreachable.insert(id.clone());
            } else {
                self.unreachable.remove(id);
                self.repos.remove(id);
            }
        }
        self.repos.extend(listing.repos);
        let mut rows: Vec<Workspace> = std::mem::take(&mut self.worktrees)
            .into_iter()
            .filter(|w| !covers(&w.host))
            .collect();
        for row in listing.worktrees.into_iter().chain(kept) {
            if !rows.iter().any(|r| r.id() == row.id()) {
                rows.push(row);
            }
        }
        // This Mac's first, then each host's, whichever answered first.
        rows.sort_by_key(|w| w.host.is_some());
        self.worktrees = rows;
        // Its missing worktrees: those of a folder that did not answer stay.
        self.prunable
            .retain(|(host, main), _| *host != listing.host || kept_mains.contains(main));
        self.prunable.extend(listing.prunable);
        self.github_repos
            .retain(|(host, main), _| *host != listing.host || kept_mains.contains(main));
        self.github_repos.extend(listing.github);
        if listing.host.is_none() {
            self.reading_local = false;
        }
    }

    /// The list from Git and the folder workspaces, in your order with the
    /// pinned on top.
    fn rebuild(&mut self) {
        self.tasks = self.ordered();
    }

    /// Every workspace in your order: the pinned first, then the order you
    /// dragged them into, then the rest as they were made; each worktree
    /// whose repository a folder workspace holds right under that one, in
    /// the same order among its siblings, the merged ones last (their
    /// group).
    fn ordered(&self) -> Vec<Workspace> {
        let canon: Vec<&Workspace> = self.folders.iter().chain(self.worktrees.iter()).collect();
        let parents: Vec<Option<String>> = canon.iter().map(|w| self.parent_of(w)).collect();
        let key = |i: &usize| {
            let id = canon[*i].id();
            (
                parents[*i].is_some() && self.done(canon[*i]),
                !canon[*i].main,
                !self.arrangement.pinned.contains(&id),
                self.arrangement.position(&id),
            )
        };
        // The sorts are stable: unplaced ones keep the order they were made.
        let mut top: Vec<usize> = (0..canon.len()).filter(|i| parents[*i].is_none()).collect();
        top.sort_by_key(key);
        let mut children: Vec<usize> = (0..canon.len()).filter(|i| parents[*i].is_some()).collect();
        children.sort_by_key(key);
        let mut out = Vec::with_capacity(canon.len());
        for t in top {
            let id = canon[t].id();
            out.push(canon[t].clone());
            out.extend(
                children
                    .iter()
                    .filter(|c| parents[**c].as_deref() == Some(id.as_str()))
                    .map(|c| canon[*c].clone()),
            );
        }
        out
    }

    /// The folder workspace a worktree's row sits under, by id: of those
    /// whose repositories include its main checkout, the one deepest in
    /// the tree, then the first made. `None` for a folder workspace, or a
    /// worktree no folder workspace holds the repository of.
    pub fn parent_of(&self, workspace: &Workspace) -> Option<String> {
        let repo = workspace.repo.as_ref().filter(|_| !workspace.folder)?;
        self.folders
            .iter()
            .enumerate()
            .filter(|(_, f)| f.host == workspace.host)
            .filter(|(_, f)| self.repos.get(&f.id()).is_some_and(|r| r.contains(repo)))
            .max_by_key(|(i, f)| (f.path.components().count(), std::cmp::Reverse(*i)))
            .map(|(_, f)| f.id())
    }

    /// [`Panel::parent_of`] the workspace `id`.
    pub fn parent_of_id(&self, id: &str) -> Option<String> {
        self.parent_of(&self.tasks[self.by_id(id)?])
    }

    /// The worktrees in the list, for their watchers: this Mac's, as a
    /// host's folders cannot be watched from here.
    pub fn worktree_paths(&self) -> Vec<PathBuf> {
        self.worktrees
            .iter()
            .filter(|w| w.host.is_none())
            .map(|w| w.path.clone())
            .collect()
    }

    /// An agent's hook says it is writing `path`: now is the last write of
    /// the worktree holding it.
    pub fn note_write(&mut self, path: &Path) {
        if let Some(i) = folder_task(&self.tasks, path).filter(|i| !self.tasks[*i].folder) {
            let tree = self.tasks[i].path.clone();
            self.touched(&tree);
        }
    }

    /// Something was written in the worktree at `tree`, now.
    pub fn touched(&mut self, tree: &Path) {
        self.touched_at(tree, std::time::Instant::now());
    }

    /// Something was written in the worktree at `tree`, at `at`; an older
    /// time than the one kept changes nothing.
    pub fn touched_at(&mut self, tree: &Path, at: std::time::Instant) {
        let kept = self.wrote.entry(tree.to_path_buf()).or_insert(at);
        *kept = (*kept).max(at);
    }

    /// What a worktree has been doing: its last write this run, else its
    /// branch's last commit.
    pub fn pace(&self, workspace: &Workspace) -> Pace {
        let since_commit = workspace.commit.as_ref().map(|(time, _)| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH + std::time::Duration::from_secs(*time))
                .unwrap_or_default()
        });
        Pace::of(
            self.wrote.get(&workspace.path).map(|at| at.elapsed()),
            since_commit,
        )
    }

    /// How many worktrees sit under the workspace `id`.
    pub fn children(&self, id: &str) -> usize {
        self.tasks
            .iter()
            .filter(|t| self.parent_of(t).as_deref() == Some(id))
            .count()
    }

    pub fn is_collapsed(&self, id: &str) -> bool {
        self.arrangement.collapsed.contains(id)
    }

    /// Folds the worktrees under a folder workspace away, or shows them.
    pub fn toggle_collapsed(&mut self, id: &str) {
        if !self.arrangement.collapsed.remove(id) {
            if !self.arrangement.order.iter().any(|o| o == id) {
                self.arrangement.order.push(id.to_owned());
            }
            self.arrangement.collapsed.insert(id.to_owned());
        }
        self.save_arrangement();
    }

    /// The workspaces with a row, in the list's order: every one but the
    /// worktrees folded under their folder workspace, and the merged ones
    /// in a folded group (the selected one stays, so you see where you
    /// are). Control-1 to 9 count these.
    pub fn visible(&self) -> Vec<usize> {
        let selected = self.selected_index();
        (0..self.tasks.len())
            .filter(|i| {
                let task = &self.tasks[*i];
                selected == Some(*i)
                    || (self.parent_of(task).is_none_or(|p| !self.is_collapsed(&p))
                        && self
                            .merged_group(task)
                            .is_none_or(|g| self.is_merged_open(&g)))
            })
            .collect()
    }

    fn save_folders(&self) {
        let Some(file) = folders_file().filter(|_| self.loaded) else {
            return;
        };
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = crate::platform::write_atomically(&file, folders_text(&self.folders).as_bytes());
    }

    /// Adds a folder workspace, its name made unique, and returns its id;
    /// a folder has one workspace, so on one that has it already, nothing
    /// is added and the `Err` is that one's id.
    pub fn add_folder(&mut self, name: &str, path: PathBuf) -> Result<String, String> {
        let path = crate::platform::canonical(&path);
        if let Some(there) = self.folder_at(None, &path) {
            return Err(there.id());
        }
        Ok(self.add(name, path, None))
    }

    /// Adds a remote folder workspace: `path` on `host`, taken as typed
    /// (nothing on this Mac to resolve it against). Returns its id, or as
    /// [`Panel::add_folder`] the one already there.
    pub fn add_remote(&mut self, name: &str, host: &str, path: &str) -> Result<String, String> {
        if let Some(there) = self.folder_at(Some(host), Path::new(path)) {
            return Err(there.id());
        }
        Ok(self.add(name, PathBuf::from(path), Some(host.to_owned())))
    }

    /// The folder workspace on `path` on `host` (`None` for this Mac): the
    /// same folder however it is written, a slash at its end or (on this
    /// Mac) through a symlink. One on a host is never one of this Mac's.
    pub fn folder_at(&self, host: Option<&str>, path: &Path) -> Option<&Workspace> {
        let local = host.is_none().then(|| crate::platform::canonical(path));
        self.folders.iter().find(|f| {
            f.host.as_deref() == host
                && match &local {
                    Some(path) => crate::platform::canonical(&f.path) == *path,
                    None => f.path.as_path() == path,
                }
        })
    }

    /// Whether a workspace has the folder `path` of this Mac already: a
    /// folder workspace is on it, or it is a repository one holds, or a
    /// worktree's.
    pub fn has_workspace(&self, path: &Path) -> bool {
        let path = crate::platform::canonical(path);
        let local = |t: &&Workspace| t.host.is_none();
        self.folder_at(None, &path).is_some()
            || self
                .tasks
                .iter()
                .filter(local)
                .any(|t| t.path == path || t.repo.as_ref() == Some(&path))
            || self
                .folders
                .iter()
                .filter(local)
                .filter_map(|f| self.repos.get(&f.id()))
                .flatten()
                .any(|r| *r == path)
    }

    fn add(&mut self, name: &str, path: PathBuf, host: Option<String>) -> String {
        let base = name.trim();
        let base = if base.is_empty() { "workspace" } else { base };
        let mut name = base.to_owned();
        let mut n = 1;
        while self.tasks.iter().any(|t| t.folder && t.name == name) {
            n += 1;
            name = format!("{base} {n}");
        }
        let made = Workspace {
            name,
            path,
            branch: None,
            main: false,
            folder: true,
            key: new_key(&self.folders),
            repo: None,
            base: None,
            commit: None,
            merged: false,
            host,
            github: None,
        };
        let id = made.id();
        self.folders.push(made);
        self.rebuild();
        self.save_folders();
        id
    }

    /// Renames a folder workspace and returns its new id.
    pub fn rename_folder(&mut self, id: &str, name: &str) -> Option<String> {
        let name = name.trim();
        if name.is_empty() || self.folders.iter().any(|t| t.name == name && t.id() != id) {
            return None;
        }
        let space = self.folders.iter_mut().find(|t| t.id() == id)?;
        space.name = name.to_owned();
        let new_id = space.id();
        self.rebuild();
        self.save_folders();
        self.arrangement.rename_id(id, &new_id);
        self.save_arrangement();
        Some(new_id)
    }

    fn save_arrangement(&self) {
        let Some(file) = order_file().filter(|_| self.loaded) else {
            return;
        };
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = crate::platform::write_atomically(&file, self.arrangement.text().as_bytes());
    }

    /// The workspace the row `id`, dragged, lands before, by the cards
    /// last drawn: the first of its peers whose middle is under `y`;
    /// `None` is the end. A worktree under a folder workspace moves among
    /// its siblings only; any other row among the other top rows, its
    /// worktrees going with it.
    pub fn drop_before(&self, id: &str, y: f32) -> Option<String> {
        self.hits
            .iter()
            .filter_map(|(rect, action)| match action {
                Action::Select(i) if self.peers(id, *i) => Some((*rect, *i)),
                _ => None,
            })
            .find(|(rect, _)| rect.y + rect.height * 0.5 > y)
            .and_then(|(_, i)| self.tasks.get(i).map(Workspace::id))
    }

    /// Whether the workspace at `index` sits beside the one `id`: under
    /// the same folder workspace, or both at the top.
    fn peers(&self, id: &str, index: usize) -> bool {
        let parent = |w: &Workspace| self.parent_of(w);
        let Some(dragged) = self.by_id(id) else {
            return false;
        };
        self.tasks
            .get(index)
            .is_some_and(|t| parent(t) == parent(&self.tasks[dragged]))
    }

    /// What a workspace's row calls it: your name for it, else its own.
    pub fn title(&self, workspace: &Workspace) -> String {
        self.arrangement
            .titles
            .get(&workspace.id())
            .cloned()
            .unwrap_or_else(|| workspace.name())
    }

    /// Names a workspace yourself; an empty name gives it back its own.
    pub fn set_title(&mut self, id: &str, title: &str) {
        let title = title.trim();
        if title.is_empty() {
            self.arrangement.titles.remove(id);
        } else {
            if !self.arrangement.order.iter().any(|o| o == id) {
                self.arrangement.order.push(id.to_owned());
            }
            self.arrangement
                .titles
                .insert(id.to_owned(), title.to_owned());
        }
        self.save_arrangement();
    }

    pub fn is_pinned(&self, id: &str) -> bool {
        self.arrangement.pinned.contains(id)
    }

    /// Pins a workspace to the top of the list, or lets it go.
    pub fn toggle_pin(&mut self, id: &str) {
        if !self.arrangement.pinned.remove(id) {
            if !self.arrangement.order.iter().any(|o| o == id) {
                self.arrangement.order.push(id.to_owned());
            }
            self.arrangement.pinned.insert(id.to_owned());
        }
        self.save_arrangement();
    }

    /// Puts `id` before `before` (or last) in your order.
    pub fn move_before(&mut self, id: &str, before: Option<&str>) {
        let all: Vec<String> = self.tasks.iter().map(Workspace::id).collect();
        self.arrangement.move_before(id, before, &all);
        self.save_arrangement();
    }

    /// Moves a folder workspace to `path`, where its terminal went. One
    /// named after its folder takes the new folder's name; one you named
    /// keeps yours. Its id stays, and with it its sessions and pin.
    pub fn move_folder(&mut self, id: &str, path: &Path) {
        let path = crate::platform::canonical(path);
        let Some(at) = self.folders.iter().position(|t| t.id() == id) else {
            return;
        };
        let old = &self.folders[at];
        if old.path == path {
            return;
        }
        let rename = named_after(&old.name, &old.path)
            .then(|| path.file_name().map(|n| n.to_string_lossy().into_owned()))
            .flatten();
        if let Some(base) = rename {
            let mut name = base.clone();
            let mut n = 1;
            while self.folders.iter().any(|t| t.name == name && t.id() != id) {
                n += 1;
                name = format!("{base} {n}");
            }
            self.folders[at].name = name;
        }
        self.folders[at].path = path;
        self.rebuild();
        self.save_folders();
    }

    /// The folder workspace `id` and the worktrees under it, by index in
    /// the list: what goes when it does.
    pub fn family(&self, id: &str) -> Vec<usize> {
        (0..self.tasks.len())
            .filter(|i| {
                let t = &self.tasks[*i];
                (t.folder && t.id() == id) || self.parent_of(t).as_deref() == Some(id)
            })
            .collect()
    }

    /// Forgets a folder workspace, and its place, pin and name with it.
    /// The worktrees under it leave the list too, unless another folder
    /// workspace holds their repository; on disk nothing changes.
    pub fn remove_folder(&mut self, id: &str) {
        let under: Vec<String> = self
            .worktrees
            .iter()
            .filter(|w| self.parent_of(w).as_deref() == Some(id))
            .map(Workspace::id)
            .collect();
        self.folders.retain(|t| t.id() != id);
        self.repos.remove(id);
        self.unreachable.remove(id);
        let gone: Vec<String> = self
            .worktrees
            .iter()
            .filter(|w| under.contains(&w.id()) && self.parent_of(w).is_none())
            .map(Workspace::id)
            .collect();
        self.worktrees.retain(|w| !gone.contains(&w.id()));
        for id in gone.iter().map(String::as_str).chain([id]) {
            self.arrangement.order.retain(|o| o != id);
            self.arrangement.pinned.remove(id);
            self.arrangement.titles.remove(id);
            self.arrangement.collapsed.remove(id);
            self.arrangement.merged_open.remove(id);
        }
        self.rebuild();
        self.save_folders();
        self.save_arrangement();
    }

    /// The name of the folder workspace being renamed, for the palette.
    pub fn renaming_name(&self) -> Option<&str> {
        let id = self.renaming.as_deref()?;
        self.folders
            .iter()
            .find(|t| t.id() == id)
            .map(|t| t.name.as_str())
    }

    /// The workspaces are known: the kept ones are read and the read of
    /// their repositories has answered, so an empty list means there are
    /// none.
    pub fn listed(&self) -> bool {
        self.loaded && !self.reading_local
    }

    /// A read or an operation is in flight: an issue read whole for its
    /// tab too, which nothing else wakes the window for.
    pub fn busy(&self) -> bool {
        self.reading_list() || self.pr_rx.is_some() || self.reading_issue()
    }

    /// The list, an operation or the inspector is being read; a pull
    /// request read alone does not hold the list's.
    pub fn reading_list(&self) -> bool {
        self.rx.is_some() || self.op.is_some() || self.info_rx.is_some() || self.inspector.busy()
    }

    /// The workspace by id.
    pub fn by_id(&self, id: &str) -> Option<usize> {
        self.tasks.iter().position(|t| t.id() == id)
    }

    /// The repositories a workspace's inspector shows: a worktree's own
    /// folder, or the ones found in a folder workspace (`None` until read).
    /// A remote workspace's are paths on its host.
    pub fn repos_of(&self, workspace: &Workspace) -> Option<Vec<PathBuf>> {
        if workspace.folder {
            self.repos.get(&workspace.id()).cloned()
        } else {
            Some(vec![workspace.path.clone()])
        }
    }

    /// The worktree workspaces of `repos` on `host` (this Mac's for
    /// `None`), in the list's order.
    pub fn worktrees_of(&self, host: Option<&str>, repos: &[PathBuf]) -> Vec<Workspace> {
        self.tasks
            .iter()
            .filter(|t| !t.folder && t.host.as_deref() == host)
            .filter(|t| t.repo.as_ref().is_some_and(|r| repos.contains(r)))
            .cloned()
            .collect()
    }

    /// The folder a new workspace takes unless you choose one: `fallback`,
    /// the editor's, while no workspace has it. Never the selected one's
    /// folder or repository: that has its workspace, and a second one on
    /// it would take its worktrees.
    pub fn new_folder(&self, fallback: Option<&Path>) -> Option<PathBuf> {
        fallback
            .filter(|f| !self.has_workspace(f))
            .map(Path::to_path_buf)
    }

    /// Where the folder panel for a new workspace opens: in the folder
    /// holding the selected workspace's (a worktree's repository), so the
    /// new one is picked beside it; else `fallback`, the editor's. A remote
    /// workspace's folder is not one of this Mac's.
    pub fn pick_from(&self, fallback: Option<&Path>) -> Option<PathBuf> {
        self.selected_index()
            .map(|i| &self.tasks[i])
            .filter(|t| t.host.is_none())
            .and_then(|t| t.repo.as_ref().unwrap_or(&t.path).parent())
            .map(Path::to_path_buf)
            .or_else(|| fallback.map(Path::to_path_buf))
    }

    /// The repository a new branch is made in: the selected worktree's, or
    /// the first one in the selected folder workspace; with its host, when
    /// it is on one.
    pub fn task_repo(&self) -> Option<(Option<String>, PathBuf)> {
        let task = &self.tasks[self.selected_index()?];
        let repo = match &task.repo {
            Some(repo) => repo.clone(),
            None => self.repos_of(task)?.into_iter().next()?,
        };
        Some((task.host.clone(), repo))
    }

    /// Puts the list in your order and nothing else: the pinned first,
    /// then the order you dragged them into, then the rest as they were
    /// made. Nothing that happens in a terminal moves a row; a worktree
    /// that merged moves to its folder workspace's merged group. Returns
    /// whether the order changed.
    pub fn arrange(&mut self) -> bool {
        let arranged = self.ordered();
        let changed = arranged != self.tasks;
        self.tasks = arranged;
        self.reveal_selected();
        changed
    }

    pub fn selected_index(&self) -> Option<usize> {
        let selected = self.selected.as_deref()?;
        self.tasks.iter().position(|t| t.id() == selected)
    }

    /// Selects a workspace by index.
    pub fn select(&mut self, index: usize) {
        self.selected = self.tasks.get(index).map(Workspace::id);
    }

    /// A session's team tasks: the open ones, and the finished ones while
    /// anything is still open, so a list in progress reads whole and a
    /// finished one folds away.
    fn team_lines(&self, tabs: &[Tab], tab: usize, lines: &mut Vec<Line>) {
        let Some(team) = self.teams.get(&tabs[tab].review) else {
            return;
        };
        if team.open() == 0 {
            return;
        }
        lines.extend((0..team.tasks.len()).map(|i| Line::TeamTask(tab, i)));
    }

    /// The folder workspace whose merged group's line goes before the row
    /// at `index`, by index: its first merged worktree's, while the folder
    /// workspace is not folded itself.
    fn group_starts_at(&self, index: usize) -> Option<usize> {
        let group = self.merged_group(&self.tasks[index])?;
        if self.is_collapsed(&group)
            || index
                .checked_sub(1)
                .is_some_and(|before| self.merged_group(&self.tasks[before]) == Some(group.clone()))
        {
            return None;
        }
        self.by_id(&group)
    }

    /// One row per workspace, as cmux and supacode have it: its sessions
    /// are the terminal's tabs, not rows here. The selected workspace's
    /// team tasks follow it; a session no workspace holds gets a row. A
    /// folder workspace's merged worktrees follow its active ones under a
    /// line of their own, folded away until you open it.
    fn lines(&self, tabs: &[Tab]) -> Vec<Line> {
        let mut lines = Vec::new();
        let owner: Vec<Option<usize>> = tabs.iter().map(|t| task_of(&self.tasks, t)).collect();
        let selected = self.selected_index();
        let visible = self.visible();
        for task in 0..self.tasks.len() {
            if let Some(group) = self.group_starts_at(task) {
                lines.push(Line::Merged(group));
            }
            if !visible.contains(&task) {
                continue;
            }
            lines.push(if self.parent_of(&self.tasks[task]).is_some() {
                Line::Child(task)
            } else {
                Line::Task(task)
            });
            if selected == Some(task) {
                for (tab, _) in owner.iter().enumerate().filter(|(_, o)| **o == Some(task)) {
                    self.team_lines(tabs, tab, &mut lines);
                }
            }
        }
        let loose: Vec<usize> = (0..tabs.len()).filter(|i| owner[*i].is_none()).collect();
        if !loose.is_empty() {
            if !self.tasks.is_empty() {
                lines.push(Line::Heading("Other folders"));
            }
            for tab in loose {
                lines.push(Line::Session(tab));
                self.team_lines(tabs, tab, &mut lines);
            }
        }
        if self.tasks.is_empty() {
            lines.push(Line::Note(
                "Double-click below, or \u{21e7}\u{2318}N, for a workspace".into(),
            ));
        }
        lines
    }

    fn height(&self, line: &Line) -> f32 {
        match line {
            Line::Task(i) if self.tasks.get(*i).is_some_and(|t| t.folder) => FOLDER_CARD,
            Line::Task(_) => CARD,
            Line::Child(_) => CHILD,
            _ => ROW,
        }
    }

    /// The first line that can be at the top: the list's end stays at the
    /// bottom of `room`, not above it.
    fn max_scroll(&self, lines: &[Line], room: f32) -> usize {
        let mut used = 0.0;
        for (n, line) in lines.iter().enumerate().rev() {
            used += self.height(line);
            if used > room {
                return n + 1;
            }
        }
        0
    }

    fn room(column: Viewport) -> f32 {
        (column.height - layout::SIDEBAR_HEADER_HEIGHT).max(0.0)
    }

    pub fn scroll_by(&mut self, lines: isize, column: Viewport, tabs: &[Tab]) {
        let max = self.max_scroll(&self.lines(tabs), Panel::room(column));
        self.scroll = (self.scroll as isize + lines).clamp(0, max as isize) as usize;
    }

    /// For the self-test: the mode, the tasks by name, the selected one,
    /// and each session's task (`-` for none), the active one starred.
    pub fn report(&self, tabs: &[Tab], active: usize, pair: Option<(usize, usize)>) -> String {
        let name = |i: Option<usize>| i.map_or("-".to_owned(), |i| self.title(&self.tasks[i]));
        format!(
            "{} tasks={} selected={} sessions={} claude={} pair={} op={}",
            if self.on { "on" } else { "off" },
            self.tasks
                .iter()
                .map(|t| self.title(t))
                .collect::<Vec<_>>()
                .join(","),
            name(self.selected_index()),
            tabs.iter()
                .enumerate()
                .map(|(i, t)| {
                    let task = name(task_of(&self.tasks, t));
                    if i == active {
                        format!("*{task}")
                    } else {
                        task
                    }
                })
                .collect::<Vec<_>>()
                .join(","),
            tabs.iter().filter(|t| t.claude).count(),
            pair.map_or("-".to_owned(), |(l, r)| format!("{l}+{r}")),
            self.op.as_ref().map_or("-", |(said, _)| said.as_str()),
        )
    }

    /// The rows, for the self-test, in the order Control-1 to 9 count them:
    /// a worktree under a folder workspace as `>name`, a folder workspace
    /// with its worktrees folded away as `name(+2)`, one whose host did not
    /// answer as `name(unreachable)`, and the line over its merged
    /// worktrees as `~merged`, `~merged(+1)` while it hides one.
    pub fn report_tree(&self) -> String {
        let visible = self.visible();
        let mut out = Vec::new();
        for i in 0..self.tasks.len() {
            if let Some(group) = self.group_starts_at(i) {
                let id = self.tasks[group].id();
                let hidden = self
                    .merged_under(&id)
                    .iter()
                    .filter(|m| !visible.contains(m))
                    .count();
                out.push(if hidden > 0 {
                    format!("~merged(+{hidden})")
                } else {
                    "~merged".to_owned()
                });
            }
            if !visible.contains(&i) {
                continue;
            }
            let task = &self.tasks[i];
            let title = self.title(task);
            if self.parent_of(task).is_some() {
                out.push(format!(">{title}"));
                continue;
            }
            let id = task.id();
            let hidden = self.children(&id) - self.shown_children(&id);
            let title = if self.unreachable.contains(&id) {
                format!("{title}(unreachable)")
            } else {
                title
            };
            out.push(if self.is_collapsed(&id) && hidden > 0 {
                format!("{title}(+{hidden})")
            } else {
                title
            });
        }
        out.join(",")
    }

    /// The folder each folder workspace's row shows, for the self-test, as
    /// `name=folder` in the list's order.
    pub fn report_folders(&self) -> String {
        self.tasks
            .iter()
            .filter(|t| t.folder)
            .map(|t| format!("{}={}", self.title(t), shown_folder(t)))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// How many of the worktrees under `id` have a row.
    fn shown_children(&self, id: &str) -> usize {
        self.visible()
            .into_iter()
            .filter(|i| self.parent_of(&self.tasks[*i]).as_deref() == Some(id))
            .count()
    }

    /// The worktrees' activity, for the self-test: `title=writing`, `quiet`,
    /// `commit` or `-`, in the list's order.
    pub fn report_activity(&self) -> String {
        self.tasks
            .iter()
            .filter(|t| !t.folder)
            .map(|t| format!("{}={}", self.title(t), self.pace(t).word()))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// The pull requests read, for the self-test: per worktree with one,
    /// `name=#N:STATE:checks`.
    pub fn report_pull_requests(&self) -> String {
        self.tasks
            .iter()
            .filter(|t| !t.folder)
            .filter_map(|t| {
                let pr = self.info.get(&t.id())?.pull_request.as_ref()?;
                let checks = format!("{:?}", pr.checks).to_ascii_lowercase();
                Some(format!(
                    "{}=#{}:{}:{checks}",
                    self.title(t),
                    pr.number,
                    pr.state
                ))
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    /// The teams, for the self-test: per session with one, its tasks as
    /// `[x] subject (teammate)` and the idle teammates.
    pub fn report_teams(&self, tabs: &[Tab]) -> String {
        tabs.iter()
            .enumerate()
            .filter_map(|(i, tab)| {
                let team = self.teams.get(&tab.review)?;
                let tasks: Vec<String> = team
                    .tasks
                    .iter()
                    .map(|t| {
                        let mut s = format!("[{}] {}", if t.done { "x" } else { " " }, t.subject);
                        if let Some(who) = &t.teammate {
                            s.push_str(&format!(" ({who})"));
                        }
                        s
                    })
                    .collect();
                let idle: Vec<&str> = team.idle.iter().map(String::as_str).collect();
                Some(format!("{i}: {} idle={}", tasks.join("; "), idle.join(",")))
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }

    /// A region by the name a self-test script uses: `agents.new`,
    /// `agents.refresh`, `agents.task.0`, `agents.session.1`,
    /// `agents.add.0`, `agents.menu.0`, `agents.fold.0`, `agents.merged.0`
    /// (by the folder workspace's index), `agents.blank`. As last drawn.
    pub fn named(&self, name: &str) -> Option<Viewport> {
        let rest = name.strip_prefix("agents.")?;
        let (kind, n) = rest.split_once('.').unwrap_or((rest, ""));
        let n: usize = n.parse().unwrap_or(0);
        let want = match kind {
            "new" => Action::NewTask,
            "refresh" => Action::Refresh,
            "task" => Action::Select(n),
            "session" => Action::Session(n),
            "add" => Action::NewSession(n),
            "blank" => Action::Blank,
            "menu" => Action::Menu(n),
            "fold" => Action::Fold(n),
            "merged" => Action::Merged(n),
            _ => return None,
        };
        self.hits
            .iter()
            .find(|(_, a)| *a == want)
            .map(|(rect, _)| *rect)
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<Action> {
        self.hits
            .iter()
            .find(|(rect, _)| rect.contains(x, y))
            .map(|(_, action)| action.clone())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        tabs: &[Tab],
        active_tab: Option<usize>,
        atlas: &mut Atlas,
        column: Viewport,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) {
        self.hits.clear();
        // Ordered here, so the rows and their hits agree until the next
        // frame whatever changed in between; never under the pointer, so a
        // row does not move away from a click (cmux defers the same way).
        if !layout::hovered(column) && self.drag.is_none() {
            self.arrange();
        }
        layout::push_rect(
            out,
            atlas,
            [column.x, column.y],
            [column.width, column.height],
            theme.sidebar_background,
        );
        layout::push_sidebar_title(out, atlas, column, "AGENTS", 2, theme);
        let [new, refresh] = layout::sidebar_header_buttons::<2>(column);
        for (rect, icon, tip, action) in [
            (
                new,
                icons::ADD,
                "New Workspace  \u{21e7}\u{2318}N",
                Action::NewTask,
            ),
            (
                refresh,
                icons::REFRESH,
                "Refresh Workspaces",
                Action::Refresh,
            ),
        ] {
            layout::Button::new(rect)
                .icon(icon)
                .tone(layout::Tone::Ghost)
                .tip(tip)
                .draw(out, atlas, theme);
            self.hits.push((rect, action));
        }
        let top = column.y + layout::SIDEBAR_HEADER_HEIGHT;
        let x = column.x + 10.0;
        let width = (column.width - 20.0).max(0.0);
        let lines = self.lines(tabs);
        let room = Panel::room(column);
        self.scroll = self.scroll.min(self.max_scroll(&lines, room));
        let selected = self.selected_index();
        // The worktrees last under their folder workspace: the guide stops
        // at their middle.
        // The merged group's line is on the guide too.
        let parent_at = |line: &Line| match line {
            Line::Child(i) => self.parent_of(&self.tasks[*i]),
            Line::Merged(i) => Some(self.tasks[*i].id()),
            _ => None,
        };
        let mut last_child = std::collections::HashSet::new();
        let mut last_group = std::collections::HashSet::new();
        for (n, line) in lines.iter().enumerate() {
            if let Line::Child(i) | Line::Merged(i) = line {
                let parent = parent_at(line);
                let more = lines[n + 1..]
                    .iter()
                    .take_while(|l| !matches!(l, Line::Task(_)))
                    .any(|l| {
                        matches!(l, Line::Child(_) | Line::Merged(_)) && parent_at(l) == parent
                    });
                if !more {
                    if matches!(line, Line::Merged(_)) {
                        last_group.insert(*i);
                    } else {
                        last_child.insert(*i);
                    }
                }
            }
        }
        let mut placed = Vec::new();
        let mut used = 0.0;
        for line in lines.iter().skip(self.scroll) {
            let h = self.height(line);
            if used + h > room {
                break;
            }
            placed.push((top + used, line));
            used += h;
        }
        let blank_top = top + used;
        if let Some(drag) = self.drag.as_ref().filter(|d| d.active) {
            // Where it lands among its peers (see `drop_before`); after the
            // last, under the last one's worktrees too.
            let id = drag.id.clone();
            let child = self
                .by_id(&id)
                .is_some_and(|i| self.parent_of(&self.tasks[i]).is_some());
            let cards: Vec<(f32, f32, bool)> = placed
                .iter()
                .filter_map(|(y, line)| match line {
                    Line::Task(i) | Line::Child(i) => {
                        Some((*y, self.height(line), self.peers(&id, *i)))
                    }
                    _ => None,
                })
                .collect();
            let indent = if child { INDENT } else { 0.0 };
            let at = cards
                .iter()
                .filter(|c| c.2)
                .find(|(y, h, _)| y + h * 0.5 > drag.y)
                .map(|c| c.0)
                .or_else(|| {
                    let last = cards.iter().rposition(|c| c.2)?;
                    // A top row's worktrees follow it; a worktree's end is
                    // its last sibling's.
                    let end = if child { last } else { cards.len() - 1 };
                    Some(cards[end].0 + cards[end].1)
                });
            if let Some(at) = at {
                layout::push_rect(
                    out,
                    atlas,
                    [column.x + 8.0 + indent, at - 1.0],
                    [column.width - 16.0 - indent, 2.0],
                    theme.accent,
                );
            }
        }
        let blank_bottom = column.y + column.height - if self.op.is_some() { ROW } else { 0.0 };
        if blank_bottom - blank_top >= ROW {
            self.hits.push((
                Viewport {
                    x: column.x,
                    y: blank_top,
                    width: column.width,
                    height: blank_bottom - blank_top,
                },
                Action::Blank,
            ));
        }
        for (y, line) in placed {
            let row = Viewport {
                x: column.x + 4.0,
                y,
                width: (column.width - 8.0).max(0.0),
                height: ROW,
            };
            let text_rect = |from: f32, right: f32| Viewport {
                x: x + from,
                y: y + 4.0,
                width: (width - from - right).max(0.0),
                height: 20.0,
            };
            match line {
                Line::Note(text) => {
                    layout::push_ui_text(out, atlas, text_rect(2.0, 0.0), text, theme.gutter_text);
                }
                Line::Heading(text) => {
                    layout::push_ui_text(out, atlas, text_rect(2.0, 0.0), text, theme.status_text);
                }
                Line::Task(index) => {
                    self.draw_card(*index, tabs, y, column, selected, None, atlas, theme, out);
                }
                Line::Child(index) => {
                    let last = last_child.contains(index);
                    self.draw_card(
                        *index,
                        tabs,
                        y,
                        column,
                        selected,
                        Some(last),
                        atlas,
                        theme,
                        out,
                    );
                }
                Line::Merged(index) => {
                    let last = last_group.contains(index);
                    self.draw_merged_line(*index, y, column, last, atlas, theme, out);
                }
                Line::Session(index) => {
                    let tab = &tabs[*index];
                    if active_tab == Some(*index) {
                        layout::push_rounded_rect(out, row, layout::UI_RADIUS_SM, theme.row_hover);
                    } else if layout::hovered(row) {
                        layout::push_row_hover(out, row, theme);
                    }
                    let activity = tab.activity();
                    let dot_x = row.x + 8.0;
                    match activity {
                        Activity::Working => {
                            layout::push_spinner(out, dot_x - 2.0, row, theme.accent);
                        }
                        Activity::Waiting | Activity::Idle => {
                            layout::push_rounded_rect(
                                out,
                                Viewport {
                                    x: dot_x,
                                    y: y + 9.0,
                                    width: 8.0,
                                    height: 8.0,
                                },
                                4.0,
                                if activity == Activity::Waiting {
                                    theme.diff_modified
                                } else {
                                    theme.gutter_text
                                },
                            );
                        }
                    }
                    let state = match activity {
                        Activity::Waiting => "waiting",
                        Activity::Working => "working",
                        Activity::Idle => "idle",
                    };
                    let state_w = layout::ui_text_width(atlas, state) + 4.0;
                    layout::push_ui_text(
                        out,
                        atlas,
                        Viewport {
                            x: row.x + 26.0,
                            y: y + 4.0,
                            width: (row.width - 26.0 - state_w - 8.0).max(0.0),
                            height: 20.0,
                        },
                        &tab.name(),
                        theme.text,
                    );
                    layout::push_ui_text(
                        out,
                        atlas,
                        Viewport {
                            x: row.x + row.width - state_w - 4.0,
                            y: y + 4.0,
                            width: state_w,
                            height: 20.0,
                        },
                        state,
                        if activity == Activity::Waiting {
                            theme.diff_modified
                        } else {
                            theme.gutter_text
                        },
                    );
                    self.hits.push((row, Action::Session(*index)));
                }
                Line::TeamTask(tab, i) => {
                    let Some(task) = self
                        .teams
                        .get(&tabs[*tab].review)
                        .and_then(|t| t.tasks.get(*i))
                    else {
                        continue;
                    };
                    let indent = INDENT;
                    let row = Viewport {
                        x: row.x + indent,
                        width: (row.width - indent).max(0.0),
                        ..row
                    };
                    if layout::hovered(row) {
                        layout::push_row_hover(out, row, theme);
                    }
                    let tone = if task.done {
                        theme.gutter_text
                    } else {
                        theme.status_text
                    };
                    // Done: a check. Open: a ring, filled while a teammate is on it.
                    if task.done {
                        layout::push_icon_centered(
                            out,
                            atlas,
                            Viewport {
                                x: row.x + 4.0,
                                width: 16.0,
                                ..row
                            },
                            icons::CHECK,
                            theme.accent,
                        );
                    } else {
                        let busy = task
                            .teammate
                            .as_ref()
                            .is_some_and(|who| !self.teams[&tabs[*tab].review].idle.contains(who));
                        let dot = Viewport {
                            x: row.x + 8.0,
                            y: y + 9.0,
                            width: 8.0,
                            height: 8.0,
                        };
                        layout::push_rounded_rect(out, dot, 4.0, tone);
                        if !busy {
                            layout::push_rounded_rect(
                                out,
                                Viewport {
                                    x: dot.x + 2.0,
                                    y: dot.y + 2.0,
                                    width: 4.0,
                                    height: 4.0,
                                },
                                2.0,
                                theme.sidebar_background,
                            );
                        }
                    }
                    let who = match &task.teammate {
                        Some(who) if self.teams[&tabs[*tab].review].idle.contains(who) => {
                            format!("{who} idle")
                        }
                        Some(who) => who.clone(),
                        None => String::new(),
                    };
                    let who_w = if who.is_empty() {
                        0.0
                    } else {
                        layout::ui_text_width(atlas, &who) + 4.0
                    };
                    layout::push_ui_text(
                        out,
                        atlas,
                        Viewport {
                            x: row.x + 26.0,
                            y: y + 4.0,
                            width: (row.width - 26.0 - who_w - 8.0).max(0.0),
                            height: 20.0,
                        },
                        &task.subject,
                        tone,
                    );
                    if !who.is_empty() {
                        layout::push_ui_text(
                            out,
                            atlas,
                            Viewport {
                                x: row.x + row.width - who_w - 4.0,
                                y: y + 4.0,
                                width: who_w,
                                height: 20.0,
                            },
                            &who,
                            theme.gutter_text,
                        );
                    }
                    self.hits.push((row, Action::Session(*tab)));
                }
            }
        }
        if let Some((said, _)) = &self.op {
            let bottom = Viewport {
                x: column.x + 4.0,
                y: column.y + column.height - ROW,
                width: (column.width - 8.0).max(0.0),
                height: ROW,
            };
            layout::push_rect(
                out,
                atlas,
                [bottom.x, bottom.y],
                [bottom.width, bottom.height],
                theme.sidebar_background,
            );
            let w = layout::push_spinner(out, bottom.x + 8.0, bottom, theme.accent);
            layout::push_ui_text(
                out,
                atlas,
                Viewport {
                    x: bottom.x + 12.0 + w,
                    y: bottom.y + 4.0,
                    width: (bottom.width - 16.0 - w).max(0.0),
                    height: 20.0,
                },
                said,
                theme.status_text,
            );
        }
    }
}

/// A workspace's folder as its row and inspector show it: `host:path` for
/// one on a host, else as the palette shows a folder (`~/...`).
pub fn shown_folder(workspace: &Workspace) -> String {
    match &workspace.host {
        Some(host) => format!("{host}:{}", workspace.path.display()),
        None => crate::platform::window::shown_path(&workspace.path),
    }
}

/// A pull request's badge colour: its checks while open, else its state.
fn pr_colour(pr: &crate::project::worktree::PullRequest, theme: &Theme) -> [f32; 4] {
    use crate::project::worktree::Checks;
    match (pr.state.as_str(), pr.checks) {
        ("MERGED", _) => theme.accent,
        ("CLOSED", _) => theme.gutter_text,
        (_, Checks::Failing) => theme.diff_removed,
        (_, Checks::Running) => theme.diff_modified,
        (_, Checks::Passing) => theme.diff_added,
        (_, Checks::None) => theme.status_text,
    }
}

impl Panel {
    /// The line over the merged worktrees under the folder workspace at
    /// `index`, after its active ones: on their guide (`last` when nothing
    /// under it has a row), a chevron like the folder's and `Merged · N`.
    /// A click shows or hides them; its menu cleans them all up.
    #[allow(clippy::too_many_arguments)]
    fn draw_merged_line(
        &mut self,
        index: usize,
        y: f32,
        column: Viewport,
        last: bool,
        atlas: &mut Atlas,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) {
        let id = self.tasks[index].id();
        let count = self.merged_under(&id).len();
        let open = self.is_merged_open(&id);
        let row = Viewport {
            x: column.x + 6.0 + INDENT,
            y: y + 1.0,
            width: (column.width - 12.0 - INDENT).max(0.0),
            height: ROW - 2.0,
        };
        let guide_x = column.x + 6.0 + 10.0 + 8.0;
        let guide = layout::faded(theme.gutter_text, 0.45);
        let mid = (y + ROW * 0.5).round();
        layout::push_rect(
            out,
            atlas,
            [guide_x - 0.5, y],
            [1.0, if last { mid - y } else { ROW }],
            guide,
        );
        layout::push_rect(
            out,
            atlas,
            [guide_x - 0.5, mid],
            [row.x + 2.0 - guide_x, 1.0],
            guide,
        );
        let over = layout::hovered(row);
        if over {
            layout::push_rounded_rect(out, row, layout::UI_RADIUS, theme.row_hover);
        }
        let left = row.x + 10.0;
        let line = Viewport {
            x: left,
            y: row.y + 2.0,
            width: (row.width - 20.0).max(0.0),
            height: 20.0,
        };
        layout::push_icon_centered(
            out,
            atlas,
            Viewport {
                x: left,
                y: line.y + 1.0,
                width: 16.0,
                height: 20.0,
            },
            if open {
                icons::CHEVRON_DOWN
            } else {
                icons::CHEVRON_RIGHT
            },
            if over { theme.text } else { theme.gutter_text },
        );
        layout::push_ui_text(
            out,
            atlas,
            Viewport {
                x: left + 22.0,
                width: (line.width - 22.0).max(0.0),
                ..line
            },
            &format!("Merged \u{b7} {count}"),
            if over { theme.text } else { theme.status_text },
        );
        let tip = if open {
            "Hide the merged worktrees".to_owned()
        } else {
            format!(
                "Show {count} merged worktree{}",
                if count == 1 { "" } else { "s" }
            )
        };
        layout::hotspot(row, layout::Cursor::Pointing, Some(&tip));
        self.hits.push((row, Action::Merged(index)));
    }

    /// A workspace's row: its icon (a spinner while an agent in it says it
    /// is working), its name, and under it the most useful line there is:
    /// what a session waiting on you asks, what a working agent says, its
    /// team, else for a worktree what it has been doing (a write in it
    /// spins its icon too) and for a folder its terminals; a folder
    /// workspace's folder under that, so a wrong one shows. At the right a
    /// steady strip: how many wait on you, and the pull request coloured by
    /// its checks; hover trades the strip for the menu button.
    ///
    /// A worktree under its folder workspace (`child`, with whether it is
    /// the last there) is drawn in from it, smaller, on a guide line down
    /// from the folder's chevron; a folder workspace with worktrees under
    /// it has that chevron, and folded, their count.
    #[allow(clippy::too_many_arguments)]
    fn draw_card(
        &mut self,
        index: usize,
        tabs: &[Tab],
        y: f32,
        column: Viewport,
        selected: Option<usize>,
        child: Option<bool>,
        atlas: &mut Atlas,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) {
        use crate::term::status::State;
        let task = self.tasks[index].clone();
        let (indent, height, inset, gap) = match child {
            Some(_) => (INDENT, CHILD, 2.0, 17.0),
            None if task.folder => (0.0, FOLDER_CARD, 4.0, 18.0),
            None => (0.0, CARD, 4.0, 18.0),
        };
        let card = Viewport {
            x: column.x + 6.0 + indent,
            y: y + 1.0,
            width: (column.width - 12.0 - indent).max(0.0),
            height: height - 2.0,
        };
        // The guide: under the parent's icon, as far as this row's middle
        // when it is the last, with a tick across to it.
        let guide_x = column.x + 6.0 + 10.0 + 8.0;
        let guide = layout::faded(theme.gutter_text, 0.45);
        if let Some(last) = child {
            let mid = (y + height * 0.5).round();
            layout::push_rect(
                out,
                atlas,
                [guide_x - 0.5, y],
                [1.0, if last { mid - y } else { height }],
                guide,
            );
            layout::push_rect(
                out,
                atlas,
                [guide_x - 0.5, mid],
                [card.x + 2.0 - guide_x, 1.0],
                guide,
            );
        }
        let is_selected = selected == Some(index);
        let over = layout::hovered(card);
        if is_selected {
            layout::push_rounded_rect(out, card, layout::UI_RADIUS, theme.sidebar_selected);
        } else if over {
            layout::push_rounded_rect(out, card, layout::UI_RADIUS, theme.row_hover);
        }
        let mine: Vec<&Tab> = tabs
            .iter()
            .filter(|t| task_of(&self.tasks, t) == Some(index))
            .collect();
        let waiting: Vec<&&Tab> = mine
            .iter()
            .filter(|t| t.activity() == Activity::Waiting)
            .collect();
        // Working is what an agent says of itself, not output: a prompt or
        // an echo would make the row flicker while you type.
        let working = mine
            .iter()
            .find(|t| t.status.as_ref().is_some_and(|s| s.state == State::Working));
        let remote = mine.iter().find_map(|t| t.remote.as_ref());
        // A worktree being written in, by an agent here or one running
        // from elsewhere, spins like a working one.
        let pace = (!task.folder).then(|| self.pace(&task));
        let writing = pace == Some(Pace::Writing) && waiting.is_empty();
        let left = card.x + 10.0;
        let line1 = Viewport {
            x: left,
            y: card.y + inset,
            width: card.width - 20.0,
            height: 20.0,
        };
        let line2 = Viewport {
            y: line1.y + gap,
            ..line1
        };
        // A folder workspace with worktrees under it: the chevron under its
        // icon folds them; folded, their count ends the line under the name.
        let id = task.id();
        let children = if child.is_none() {
            self.children(&id)
        } else {
            0
        };
        let folded = children > 0 && self.is_collapsed(&id);
        let mut line2_end = line2.x + line2.width;
        if children > 0 {
            let chevron = Viewport {
                x: left - 4.0,
                y: line2.y,
                width: 24.0,
                height: 20.0,
            };
            let over = layout::hovered(chevron);
            layout::push_icon_centered(
                out,
                atlas,
                Viewport {
                    x: left,
                    y: line2.y + 1.0,
                    width: 16.0,
                    height: 20.0,
                },
                if folded {
                    icons::CHEVRON_RIGHT
                } else {
                    icons::CHEVRON_DOWN
                },
                if over { theme.text } else { theme.gutter_text },
            );
            if !folded {
                layout::push_rect(
                    out,
                    atlas,
                    [guide_x - 0.5, line2.y + 19.0],
                    [1.0, (y + height - line2.y - 19.0).max(0.0)],
                    guide,
                );
            }
            let tip = if folded {
                format!(
                    "Show {children} worktree{}",
                    if children == 1 { "" } else { "s" }
                )
            } else {
                "Hide the worktrees".to_owned()
            };
            layout::hotspot(chevron, layout::Cursor::Pointing, Some(&tip));
            self.hits.push((chevron, Action::Fold(index)));
            if folded {
                let label = children.to_string();
                let w = (layout::ui_text_width(atlas, &label) + 10.0).max(18.0);
                let x = line2.x + line2.width - w;
                layout::push_count(out, atlas, x, line2, children, theme);
                line2_end = x - 6.0;
            }
        }
        // A point lower than the name's box: the glyph sits on its baseline.
        let icon_box = Viewport {
            x: left,
            y: line1.y + 1.0,
            width: 16.0,
            ..line1
        };
        if working.is_some() || writing {
            layout::push_spinner(out, left, line1, theme.accent);
        } else {
            layout::push_icon_centered(
                out,
                atlas,
                icon_box,
                if remote.is_some() || (task.folder && task.host.is_some()) {
                    icons::SERVER
                } else if task.main {
                    icons::HOME
                } else if task.folder {
                    icons::FOLDER
                } else if self.is_merged(&task) {
                    icons::CHECK
                } else {
                    icons::GIT_BRANCH
                },
                if is_selected {
                    theme.text
                } else {
                    theme.status_text
                },
            );
        }
        // The strip, right to left: the pull request, then the waiting count.
        let right = card.x + card.width - 8.0;
        let mut strip_x = right;
        if over {
            let menu = Viewport {
                x: right - layout::UI_CONTROL_SM + 2.0,
                y: card.y + (card.height - layout::UI_CONTROL_SM) * 0.5,
                width: layout::UI_CONTROL_SM,
                height: layout::UI_CONTROL_SM,
            };
            layout::Button::new(menu)
                .icon(icons::ELLIPSIS)
                .tone(layout::Tone::Ghost)
                .tip("Workspace menu: Git, pull request, rename, remove")
                .draw(out, atlas, theme);
            self.hits.push((menu, Action::Menu(index)));
            strip_x = menu.x - 4.0;
        } else {
            let pr = self
                .info
                .get(&task.id())
                .and_then(|info| info.pull_request.as_ref());
            if let Some(pr) = pr {
                let label = format!("#{}", pr.number);
                let w = layout::ui_text_width(atlas, &label) + 2.0;
                strip_x -= w;
                layout::push_ui_text(
                    out,
                    atlas,
                    Viewport {
                        x: strip_x,
                        width: w,
                        ..line1
                    },
                    &label,
                    pr_colour(pr, theme),
                );
                strip_x -= 6.0;
            }
            if !waiting.is_empty() {
                let label = waiting.len().to_string();
                let w = (layout::ui_text_width(atlas, &label) + 10.0).max(18.0);
                let pill = Viewport {
                    x: strip_x - w,
                    y: line1.y + 2.0,
                    width: w,
                    height: 16.0,
                };
                layout::push_rounded_rect(out, pill, 8.0, theme.diff_modified);
                let ink = if theme.is_dark() {
                    [0.063, 0.086, 0.078, 1.0]
                } else {
                    [1.0, 1.0, 1.0, 1.0]
                };
                layout::push_ui_text_centered(out, atlas, pill, &label, ink);
                strip_x = pill.x - 6.0;
            }
        }
        let name_x = left + 22.0;
        let name = self.title(&task);
        let editing = self
            .inline
            .as_ref()
            .filter(|(id, _, _)| *id == task.id())
            .map(|(_, text, caret)| (text.clone(), *caret));
        if let Some((text, caret)) = editing {
            // The name, editable in place: a field over the name's line.
            let field = Viewport {
                x: name_x - 4.0,
                y: line1.y - 1.0,
                width: (card.x + card.width - 8.0 - name_x + 4.0).max(0.0),
                height: 22.0,
            };
            layout::push_rounded_rect(out, field, layout::UI_RADIUS_SM, theme.find_background);
            let prefix: String = text.chars().take(caret).collect();
            let caret_x = name_x + layout::ui_text_width(atlas, &prefix);
            layout::push_ui_text(
                out,
                atlas,
                Viewport {
                    x: name_x,
                    width: (field.x + field.width - name_x - 4.0).max(0.0),
                    ..line1
                },
                &text,
                theme.text,
            );
            layout::push_rect(
                out,
                atlas,
                [caret_x, field.y + 3.0],
                [1.5, field.height - 6.0],
                theme.cursor,
            );
        } else {
            layout::push_ui_text(
                out,
                atlas,
                Viewport {
                    x: name_x,
                    width: (strip_x - name_x).max(0.0),
                    ..line1
                },
                &name,
                theme.text,
            );
        }
        // The line under the name.
        let (said, colour) = if let Some(tab) = waiting.first() {
            (tab.state(), theme.diff_modified)
        } else if let Some(tab) = working {
            let text = tab.status.as_ref().map(|s| s.text()).unwrap_or_default();
            (
                if text.is_empty() {
                    "working".into()
                } else {
                    text
                },
                theme.status_text,
            )
        } else if let Some(host) = task
            .host
            .as_ref()
            .filter(|_| self.unreachable.contains(&task.id()))
        {
            // Its Git could not be read: the rows under it are the last read.
            let said = format!("host unreachable \u{b7} {host}:{}", task.path.display());
            (
                crate::platform::inspector::fit(atlas, &said, (line2_end - name_x).max(0.0)),
                theme.diff_removed,
            )
        } else if let Some(remote) = remote {
            (
                format!("ssh {} \u{b7} {}", remote.login(), remote.connected_for()),
                theme.status_text,
            )
        } else if let Some(open) = mine
            .iter()
            .filter_map(|t| self.teams.get(&t.review))
            .map(Team::open)
            .find(|n| *n > 0)
        {
            (
                format!("{open} team task{} open", if open == 1 { "" } else { "s" }),
                theme.status_text,
            )
        } else {
            let mut parts = Vec::new();
            if self.is_pinned(&task.id()) {
                parts.push("pinned".to_owned());
            }
            let ahead = self.info.get(&task.id()).map_or(0, |i| i.ahead);
            match pace {
                Some(pace) => parts.extend(worktree_line(
                    pace,
                    mine.len(),
                    ahead,
                    self.is_merged(&task),
                )),
                None => {
                    match mine.len() {
                        0 => parts.push("no terminal".to_owned()),
                        1 => parts.push("1 terminal".to_owned()),
                        n => parts.push(format!("{n} terminals")),
                    }
                    if ahead > 0 {
                        parts.push(format!("{ahead} ahead"));
                    }
                }
            }
            // A workspace you named says its branch; a folder workspace's
            // folder has a line of its own, under this one.
            if !task.folder && name != task.name() {
                parts.push(task.name());
            }
            let colour = if writing {
                theme.status_text
            } else {
                theme.gutter_text
            };
            // Too long for the row: the last facts go whole before
            // anything is cut, and a cut ends with an ellipsis.
            let room = (line2_end - name_x).max(0.0);
            while parts.len() > 1
                && layout::ui_text_width(atlas, &parts.join(" \u{b7} ")) > room + 0.5
            {
                parts.pop();
            }
            let said = crate::platform::inspector::fit(atlas, &parts.join(" \u{b7} "), room);
            (said, colour)
        };
        layout::push_ui_text(
            out,
            atlas,
            Viewport {
                x: name_x,
                width: (line2_end - name_x).max(0.0),
                ..line2
            },
            &said,
            colour,
        );
        // The folder, dimmer, cut from its start: its end is the folder
        // itself, the part that tells two apart.
        if task.folder && child.is_none() {
            let room = (right - name_x).max(0.0);
            let shown = crate::platform::inspector::fit_start(atlas, &shown_folder(&task), room);
            layout::push_ui_text(
                out,
                atlas,
                Viewport {
                    x: name_x,
                    y: line2.y + 16.0,
                    width: room,
                    height: 20.0,
                },
                &shown,
                layout::faded(theme.gutter_text, 0.8),
            );
        }
        self.hits.push((card, Action::Select(index)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(path: &str, main: bool) -> Workspace {
        Workspace::of(
            Worktree {
                path: PathBuf::from(path),
                branch: None,
                main,
            },
            None,
            None,
        )
    }

    fn folder(name: &str, path: &str) -> Workspace {
        Workspace {
            name: name.into(),
            path: PathBuf::from(path),
            branch: None,
            main: false,
            folder: true,
            key: format!("folder:{name}"),
            repo: None,
            base: None,
            commit: None,
            merged: false,
            host: None,
            github: None,
        }
    }

    #[test]
    fn a_remote_workspace_keeps_its_host_in_a_fourth_field() {
        let mut panel = Panel::default();
        let local = panel
            .add_folder("notes", PathBuf::from("/tmp/crc-agents-remote"))
            .unwrap();
        let remote = panel.add_remote("app", "myhost", "~/srv/app").unwrap();
        let text = folders_text(&panel.folders);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0].split('\t').count(), 3, "{text}");
        assert_eq!(lines[1], format!("app\t~/srv/app\t{remote}\tmyhost"));
        let back = parse_folders(&text);
        assert_eq!(back, panel.folders);
        assert_eq!(back[0].host, None);
        assert_eq!(back[1].host.as_deref(), Some("myhost"));
        assert_eq!(back[1].path, PathBuf::from("~/srv/app"));
        // Written without a host, a file reads and writes back the same.
        let old = format!("notes\t/tmp/crc-agents-remote\t{local}\nplain\t/p\n");
        let parsed = parse_folders(&old);
        assert!(parsed.iter().all(|w| w.host.is_none()));
        assert_eq!(
            folders_text(&parsed),
            format!("notes\t/tmp/crc-agents-remote\t{local}\nplain\t/p\tfolder:plain\n")
        );
        // An empty host field is none.
        assert_eq!(parse_folders("a\t/a\tws:1\t\n")[0].host, None);
    }

    #[test]
    fn a_remote_workspace_takes_no_local_terminal_and_never_moves() {
        let mut remote = folder("app", "/srv/app");
        remote.host = Some("myhost".into());
        let tasks = vec![remote, folder("srv", "/srv")];
        // A local terminal in a folder named as the remote one's is not its.
        assert_eq!(folder_task(&tasks, Path::new("/srv/app/x")), Some(1));
        assert_eq!(folder_task(&tasks[..1], Path::new("/srv/app")), None);
        // Its ssh runs in this Mac's home, which says nothing of where it is.
        assert_eq!(follow(&tasks, 0, Path::new("/home/me")), Follow::Stay);
        let panel = Panel {
            folders: tasks.clone(),
            tasks,
            selected: Some("folder:app".into()),
            ..Panel::default()
        };
        // Its repositories are its host's, unknown until they are read.
        assert_eq!(panel.repos_of(&panel.tasks[0]), None);
        assert_eq!(
            panel.new_folder(Some(Path::new("/e"))),
            Some(PathBuf::from("/e"))
        );
        assert_eq!(panel.task_repo(), None);
        // Once read, a new task's repository is its first one, on its host.
        let mut panel = panel;
        panel
            .repos
            .insert("folder:app".into(), vec![PathBuf::from("/srv/app/api")]);
        assert_eq!(
            panel.task_repo(),
            Some((Some("myhost".into()), PathBuf::from("/srv/app/api")))
        );
    }

    #[test]
    fn an_arrangement_keeps_order_pins_and_names_through_its_file() {
        let mut a = Arrangement::default();
        let all: Vec<String> = ["/r", "/w/a", "/w/b", "folder:notes"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        a.move_before("folder:notes", Some("/w/a"), &all);
        assert_eq!(a.order, ["/r", "folder:notes", "/w/a", "/w/b"]);
        a.move_before("/r", None, &all);
        assert_eq!(a.order, ["folder:notes", "/w/a", "/w/b", "/r"]);
        a.pinned.insert("/w/b".into());
        a.titles.insert("/w/a".into(), "login fix".into());
        let back = Arrangement::parse(&a.text());
        assert_eq!(back, a);
        a.rename_id("folder:notes", "folder:ideas");
        assert_eq!(a.order[0], "folder:ideas");
    }

    #[test]
    fn pinned_workspaces_follow_main_and_your_order_holds_within() {
        let mut panel = Panel {
            worktrees: vec![tree("/r", true), tree("/w/a", false), tree("/w/b", false)],
            folders: vec![folder("notes", "/r")],
            ..Panel::default()
        };
        panel.rebuild();
        panel.arrange();
        let ids = |p: &Panel| p.tasks.iter().map(Workspace::id).collect::<Vec<_>>();
        assert_eq!(ids(&panel), ["/r", "folder:notes", "/w/a", "/w/b"]);
        // Pinned goes up, under main; dragged order holds for the rest.
        panel.arrangement.pinned.insert("folder:notes".into());
        let all = ids(&panel);
        panel.arrangement.move_before("/w/b", Some("/w/a"), &all);
        panel.arrange();
        assert_eq!(ids(&panel), ["/r", "folder:notes", "/w/b", "/w/a"]);
        // Main stays first even dragged last.
        let all = ids(&panel);
        panel.arrangement.move_before("/r", None, &all);
        panel.arrange();
        assert_eq!(ids(&panel)[0], "/r");
    }

    #[test]
    fn a_folder_belongs_to_its_deepest_task() {
        let tasks = vec![
            tree("/r", true),
            tree("/r/.claude/worktrees/x", false),
            tree("/w/y", false),
        ];
        assert_eq!(folder_task(&tasks, Path::new("/r/src")), Some(0));
        assert_eq!(
            folder_task(&tasks, Path::new("/r/.claude/worktrees/x/src")),
            Some(1)
        );
        assert_eq!(folder_task(&tasks, Path::new("/w/y")), Some(2));
        assert_eq!(folder_task(&tasks, Path::new("/elsewhere")), None);
        // A sibling whose name starts the same is not inside.
        assert_eq!(folder_task(&tasks, Path::new("/rr")), None);
    }

    #[test]
    fn a_worktree_wins_a_folder_workspace_on_the_same_folder() {
        let tasks = vec![folder("notes", "/r"), tree("/r", true)];
        assert_eq!(folder_task(&tasks, Path::new("/r/src")), Some(1));
        let tasks = vec![folder("notes", "/r/docs"), tree("/r", true)];
        assert_eq!(folder_task(&tasks, Path::new("/r/docs/a")), Some(0));
    }

    #[test]
    fn a_terminal_leaving_its_worktree_joins_where_it_went() {
        let tasks = vec![
            tree("/r", true),
            tree("/r/.claude/worktrees/x", false),
            folder("notes", "/n"),
        ];
        let at = |owner, cwd: &str| follow(&tasks, owner, Path::new(cwd));
        // Inside its own worktree, a subfolder or a folder workspace under
        // it: it stays.
        assert_eq!(at(1, "/r/.claude/worktrees/x"), Follow::Stay);
        assert_eq!(at(1, "/r/.claude/worktrees/x/src"), Follow::Stay);
        // Out to the repository's checkout: it joins that.
        assert_eq!(at(1, "/r/src"), Follow::Join(0));
        // The main checkout into the worktree inside it.
        assert_eq!(at(0, "/r/.claude/worktrees/x/src"), Follow::Join(1));
        // Out to a folder workspace's folder, or somewhere nothing holds.
        assert_eq!(at(1, "/n/a"), Follow::Join(2));
        assert_eq!(at(1, "/elsewhere"), Follow::Make);
        // A folder workspace moves with it, until it reaches a worktree.
        assert_eq!(at(2, "/n"), Follow::Stay);
        assert_eq!(at(2, "/elsewhere"), Follow::Move);
        assert_eq!(at(2, "/n/a"), Follow::Move);
        assert_eq!(at(2, "/r/.claude/worktrees/x"), Follow::Join(1));
    }

    #[test]
    fn folder_workspaces_survive_their_file_and_keep_names_apart() {
        // No root: nothing is written; the text form is checked directly.
        let mut panel = Panel {
            worktrees: vec![tree("/r", true)],
            ..Panel::default()
        };
        let a = panel
            .add_folder("notes", PathBuf::from("/tmp/crc-agents-test"))
            .unwrap();
        let b = panel
            .add_folder("notes", PathBuf::from("/tmp/crc-agents-test-2"))
            .unwrap();
        assert_ne!(a, b);
        assert_eq!(panel.tasks.len(), 3);
        assert_eq!(panel.tasks[0].id(), "/r");
        assert_eq!(panel.tasks[2].name, "notes 2");
        let text = folders_text(&panel.folders);
        let back = parse_folders(&text);
        assert_eq!(back, panel.folders);
        assert_eq!(panel.rename_folder(&b, "notes"), None);
        // A rename keeps the id: the sessions and the pin stay with it.
        assert_eq!(panel.rename_folder(&b, "ideas"), Some(b.clone()));
        panel.renaming = Some(b.clone());
        assert_eq!(panel.renaming_name(), Some("ideas"));
        panel.remove_folder(&a);
        assert_eq!(
            panel.tasks.iter().map(Workspace::id).collect::<Vec<_>>(),
            vec!["/r".to_owned(), b]
        );
    }

    #[test]
    fn every_projects_workspaces_become_one_list() {
        let a = "crc\t/a\nnotes\t/a/notes\tws:1\n".to_owned();
        // `folder:crc` again on another folder, and ws:1 again on the same.
        let b = "crc\t/b\nnotes\t/a/notes\tws:1\n".to_owned();
        let all = merge_folders(&[a, b]);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].id(), "folder:crc");
        assert_eq!(all[1].id(), "ws:1");
        assert_eq!(all[2].path, PathBuf::from("/b"));
        assert!(all[2].id().starts_with("ws:"));
        let order = merge_arrangements(&["pin\tws:1\t\n".into(), "-\tws:1\t\n-\t/w\t\n".into()]);
        assert_eq!(order.order, ["ws:1", "/w"]);
        assert!(order.pinned.contains("ws:1"));
    }

    #[test]
    fn a_folder_holds_itself_and_its_children_as_repositories() {
        let dir = std::env::temp_dir().join(format!("crc-discover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["b/.git", "a/.git", ".hidden/.git", "plain", "c/d/.git"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        // A linked worktree's .git is a file into its repository's
        // worktrees: not a repository of the folder (its repository's list
        // has it). A submodule's .git is a file too, and it is one.
        std::fs::create_dir_all(dir.join("w")).unwrap();
        std::fs::write(dir.join("w/.git"), "gitdir: /r/.git/worktrees/w\n").unwrap();
        std::fs::create_dir_all(dir.join("s")).unwrap();
        std::fs::write(dir.join("s/.git"), "gitdir: ../.git/modules/s\n").unwrap();
        assert_eq!(
            discover(&dir),
            vec![dir.join("a"), dir.join("b"), dir.join("s")]
        );
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        assert_eq!(discover(&dir)[0], dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A folder workspace on `/r` (holding the repository `/r`) and one on
    /// `/n`, with `/r`'s worktrees `/w/a` and `/w/b` and a worktree of a
    /// repository no workspace holds.
    fn nested() -> Panel {
        let wt = |path: &str, repo: &str| {
            let mut w = tree(path, false);
            w.repo = Some(PathBuf::from(repo));
            w
        };
        let mut panel = Panel {
            folders: vec![folder("garden-log", "/r"), folder("notes", "/n")],
            worktrees: vec![wt("/w/a", "/r"), wt("/elsewhere/x", "/o"), wt("/w/b", "/r")],
            ..Panel::default()
        };
        panel
            .repos
            .insert("folder:garden-log".into(), vec![PathBuf::from("/r")]);
        panel.repos.insert("folder:notes".into(), Vec::new());
        panel.rebuild();
        panel
    }

    fn rows(panel: &Panel) -> Vec<String> {
        panel
            .lines(&[])
            .iter()
            .filter_map(|line| match line {
                Line::Task(i) => Some(panel.tasks[*i].id()),
                Line::Child(i) => Some(format!(">{}", panel.tasks[*i].id())),
                Line::Merged(i) => Some(format!("~{}", panel.tasks[*i].id())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn merged_worktrees_fold_into_a_group_after_the_active_ones() {
        let mut panel = nested();
        let ids = |p: &Panel| p.tasks.iter().map(Workspace::id).collect::<Vec<_>>();
        let merge = |p: &mut Panel, path: &str| {
            let w = p.worktrees.iter_mut().find(|w| w.path == Path::new(path));
            w.unwrap().merged = true;
        };
        // /w/a is merged: it moves after its active sibling, into the
        // group, folded; a merged worktree no folder holds stays a row.
        merge(&mut panel, "/w/a");
        merge(&mut panel, "/elsewhere/x");
        assert!(panel.arrange());
        assert_eq!(
            ids(&panel),
            [
                "folder:garden-log",
                "/w/b",
                "/w/a",
                "folder:notes",
                "/elsewhere/x"
            ]
        );
        assert_eq!(panel.merged_under("folder:garden-log"), [2]);
        assert_eq!(
            rows(&panel),
            [
                "folder:garden-log",
                ">/w/b",
                "~folder:garden-log",
                "folder:notes",
                "/elsewhere/x"
            ]
        );
        // Folded: no row and no number for it.
        assert_eq!(panel.visible(), [0, 1, 3, 4]);
        assert_eq!(panel.report_tree(), "garden-log,>b,~merged(+1),notes,x");
        panel.toggle_merged("folder:garden-log");
        assert_eq!(panel.visible(), [0, 1, 2, 3, 4]);
        assert_eq!(panel.report_tree(), "garden-log,>b,~merged,>a,notes,x");
        panel.toggle_merged("folder:garden-log");
        // The folder folded: no group line either.
        panel.toggle_collapsed("folder:garden-log");
        assert_eq!(
            rows(&panel),
            ["folder:garden-log", "folder:notes", "/elsewhere/x"]
        );
        panel.toggle_collapsed("folder:garden-log");

        // The selected worktree merges: it stays selected, and the group
        // opens so it is still in sight.
        panel.selected = Some("/w/b".into());
        panel.arrange();
        merge(&mut panel, "/w/b");
        panel.arrange();
        assert_eq!(panel.selected.as_deref(), Some("/w/b"));
        assert!(panel.is_merged_open("folder:garden-log"));
        assert_eq!(panel.report_tree(), "garden-log,~merged,>a,>b,notes,x");
        // Folded again by hand, it stays folded; the selected row keeps
        // its place under the line.
        panel.toggle_merged("folder:garden-log");
        panel.arrange();
        assert!(!panel.is_merged_open("folder:garden-log"));
        assert_eq!(panel.report_tree(), "garden-log,~merged(+1),>b,notes,x");
        // A pull request merged on GitHub counts too.
        let mut fresh = nested();
        fresh.info.insert(
            "/w/b".to_owned(),
            Info {
                ahead: 0,
                pull_request: Some(crate::project::worktree::PullRequest {
                    state: "MERGED".into(),
                    ..Default::default()
                }),
            },
        );
        fresh.arrange();
        assert_eq!(fresh.report_tree(), "garden-log,>a,~merged(+1),notes,x");
    }

    fn pr(state: &str, checks: crate::project::worktree::Checks) -> PullRequest {
        PullRequest {
            number: 7,
            state: state.into(),
            checks,
            ..Default::default()
        }
    }

    use crate::project::worktree::{Checks, PullRequest};

    #[test]
    fn pull_requests_are_read_more_often_while_checks_run() {
        let secs = |prs: &[PullRequest]| pr_wait(prs).as_secs();
        assert_eq!(secs(&[]), 300);
        assert_eq!(secs(&[pr("MERGED", Checks::Running)]), 300);
        assert_eq!(secs(&[pr("OPEN", Checks::Passing)]), 60);
        assert_eq!(
            secs(&[pr("OPEN", Checks::Passing), pr("OPEN", Checks::Running)]),
            20
        );
        let s = std::time::Duration::from_secs;
        let wait = s(60);
        // Never read: now.
        assert!(pr_due(None, None, wait));
        assert!(!pr_due(None, Some(s(30)), wait));
        assert!(pr_due(None, Some(s(60)), wait));
        let open = pr("OPEN", Checks::None);
        assert!(!pr_due(Some(&open), Some(s(59)), wait));
        assert!(pr_due(Some(&open), Some(s(61)), wait));
        // Merged and closed ones settle.
        for state in ["MERGED", "CLOSED"] {
            let done = pr(state, Checks::None);
            assert!(!pr_due(Some(&done), Some(s(300)), wait));
            assert!(pr_due(Some(&done), Some(PR_SETTLED), wait));
        }
    }

    #[test]
    fn a_failed_read_keeps_the_last_pull_request() {
        let mut panel = nested();
        let path = "/w/a".to_owned();
        assert!(panel.take_pull_request(path.clone(), Ok(Some(pr("OPEN", Checks::Running)))));
        assert!(!panel.take_pull_request(path.clone(), Err("offline".into())));
        assert_eq!(panel.report_pull_requests(), "a=#7:OPEN:running");
        assert!(panel.pr_read.contains_key(&path));
        assert!(panel.take_pull_request(path.clone(), Ok(Some(pr("MERGED", Checks::None)))));
        panel.arrange();
        assert_eq!(panel.report_tree(), "garden-log,>b,~merged(+1),notes,x");
        // gh says there is none: the badge goes.
        assert!(panel.take_pull_request(path, Ok(None)));
        assert_eq!(panel.report_pull_requests(), "");
    }

    fn issue(number: u64) -> crate::project::worktree::Issue {
        crate::project::worktree::Issue {
            number,
            title: format!("Issue {number}"),
            state: "OPEN".into(),
            ..Default::default()
        }
    }

    fn here(path: &str) -> RepoKey {
        (None, PathBuf::from(path))
    }

    #[test]
    fn issues_are_read_every_five_minutes_for_the_selected_workspace_only() {
        let mut panel = nested();
        assert!(panel.selected_repos().is_empty());
        assert!(panel.issues_due(std::time::Instant::now()).is_empty());
        // A folder workspace: the repositories found in it.
        panel.selected = Some("folder:garden-log".into());
        assert_eq!(panel.selected_repos(), vec![here("/r")]);
        assert_eq!(
            panel.issues_due(std::time::Instant::now()),
            vec![here("/r")]
        );
        assert!(panel.take_issues(here("/r"), Ok((vec![issue(1)], false))));
        let at = panel.issues_of(&here("/r")).unwrap().read_at;
        let s = std::time::Duration::from_secs;
        assert!(panel.issues_due(at + s(299)).is_empty());
        assert_eq!(panel.issues_due(at + ISSUES_EVERY), vec![here("/r")]);
        // A worktree: its repository's, read once for all its worktrees.
        panel.selected = Some("/w/a".into());
        assert_eq!(panel.selected_repos(), vec![here("/r")]);
        assert!(panel.issues_due(at + s(10)).is_empty());
        panel.selected = Some("/elsewhere/x".into());
        assert_eq!(panel.issues_due(at + s(10)), vec![here("/o")]);
        // A folder with no repository in it reads nothing.
        panel.selected = Some("folder:notes".into());
        assert!(panel.issues_due(at + ISSUES_EVERY).is_empty());
    }

    #[test]
    fn a_failed_issue_read_keeps_the_last_list() {
        let mut panel = nested();
        let r = here("/r");
        // A first read that fails: no list, and why.
        assert!(panel.take_issues(r.clone(), Err(IssueError::NeedsAuth)));
        let set = panel.issues_of(&r).unwrap();
        assert_eq!(
            (set.list.len(), &set.error),
            (0, &Some(IssueError::NeedsAuth))
        );
        assert!(panel.take_issues(r.clone(), Ok((vec![issue(1), issue(2)], false))));
        assert!(!panel.take_issues(r.clone(), Ok((vec![issue(1), issue(2)], false))));
        let offline = IssueError::Other("error connecting to api.github.com".into());
        assert!(panel.take_issues(r.clone(), Err(offline.clone())));
        let set = panel.issues_of(&r).unwrap();
        assert_eq!(
            (
                set.list.iter().map(|i| i.number).collect::<Vec<_>>(),
                &set.error
            ),
            (vec![1, 2], &Some(offline.clone()))
        );
        // The same failure again changes nothing shown, but counts as read.
        let before = set.read_at;
        assert!(!panel.take_issues(r.clone(), Err(offline)));
        assert!(panel.issues_of(&r).unwrap().read_at >= before);
        assert!(panel.take_issues(r.clone(), Ok((vec![issue(3)], true))));
        let set = panel.issues_of(&r).unwrap();
        assert_eq!((set.count(), &set.error), ("1+".to_owned(), &None));
    }

    #[test]
    fn the_issues_of_a_repository_no_longer_listed_are_forgotten() {
        let mut panel = nested();
        let host = (Some("box".to_owned()), PathBuf::from("/r"));
        for repo in [here("/r"), here("/o"), here("/gone"), host.clone()] {
            panel.take_issues(repo, Ok((vec![issue(1)], false)));
        }
        panel.issues_wanted = vec![here("/gone"), here("/o")];
        panel.prune();
        let mut kept: Vec<RepoKey> = panel.issues.keys().cloned().collect();
        kept.sort();
        assert_eq!(kept, vec![here("/o"), here("/r")]);
        assert_eq!(panel.issues_wanted, vec![here("/o")]);
        // Its last worktree gone, a repository no folder holds goes too.
        panel
            .worktrees
            .retain(|w| w.path != Path::new("/elsewhere/x"));
        panel.rebuild();
        panel.prune();
        assert_eq!(
            panel.issues.keys().cloned().collect::<Vec<_>>(),
            vec![here("/r")]
        );
    }

    #[test]
    fn a_hosts_repository_is_asked_about_by_its_github_name() {
        use crate::project::worktree::GitHub;
        let mut panel = Panel {
            folders: vec![{
                let mut f = folder("srv", "/srv");
                f.host = Some("box".into());
                f
            }],
            ..Panel::default()
        };
        let box_ = |path: &str| (Some("box".to_owned()), PathBuf::from(path));
        let listing = |names: &[(&str, &str)]| Listing {
            host: Some("box".into()),
            repos: vec![(
                "folder:srv".into(),
                vec![PathBuf::from("/srv/app"), PathBuf::from("/srv/tool")],
            )],
            worktrees: Vec::new(),
            prunable: std::collections::HashMap::new(),
            github: names
                .iter()
                .map(|(path, name)| (box_(path), (*name).to_owned()))
                .collect(),
            unreachable: Vec::new(),
        };
        // A repository with no worktree but its own checkout still has it.
        panel.take_listing(listing(&[("/srv/app", "me/app")]));
        panel.rebuild();
        panel.selected = Some("folder:srv".into());
        assert_eq!(
            panel.selected_repos(),
            vec![box_("/srv/app"), box_("/srv/tool")]
        );
        assert_eq!(
            panel.issue_target(&box_("/srv/app")),
            Some(GitHub::Repo("me/app".into()))
        );
        // Its origin not on GitHub: `gh` is not asked.
        assert_eq!(panel.issue_target(&box_("/srv/tool")), None);
        assert_eq!(
            panel.issue_target(&here("/srv/app")),
            Some(GitHub::Dir("/srv/app".into()))
        );
        // The next read of the host says what its origins are now.
        panel.take_listing(listing(&[("/srv/tool", "me/tool")]));
        assert_eq!(panel.issue_target(&box_("/srv/app")), None);
        assert_eq!(
            panel.issue_target(&box_("/srv/tool")),
            Some(GitHub::Repo("me/tool".into()))
        );
    }

    #[test]
    fn what_was_read_about_a_removed_worktree_is_forgotten() {
        let mut panel = nested();
        for path in ["/w/a", "/w/b"] {
            panel.take_pull_request(path.into(), Ok(Some(pr("OPEN", Checks::None))));
            panel.pushed.insert(path.into(), std::time::Instant::now());
        }
        panel.worktrees.retain(|w| w.path != Path::new("/w/a"));
        panel.rebuild();
        panel.prune();
        assert!(!panel.info.contains_key("/w/a"));
        assert!(!panel.pr_read.contains_key("/w/a"));
        assert!(!panel.pushed.contains_key("/w/a"));
        assert!(panel.info.contains_key("/w/b"));
    }

    #[test]
    fn a_hosts_worktree_never_shares_what_was_read_with_one_here() {
        // The same path on this Mac and on a host: two rows, two ids, each
        // its own ahead count and pull request.
        let mut panel = nested();
        let mut there = tree("/w/a", false);
        there.host = Some("box".into());
        there.repo = Some("/r".into());
        there.branch = Some("a".into());
        there.github = Some("me/garden".into());
        panel.worktrees.push(there);
        panel.rebuild();
        assert_eq!(
            panel
                .tasks
                .iter()
                .filter(|t| t.path == Path::new("/w/a"))
                .count(),
            2
        );
        panel.take_pull_request("box:/w/a".into(), Ok(Some(pr("OPEN", Checks::None))));
        let here = panel.tasks.iter().find(|t| t.id() == "/w/a").unwrap();
        let remote = panel.tasks.iter().find(|t| t.id() == "box:/w/a").unwrap();
        assert!(!panel.info.contains_key(&here.id()));
        assert_eq!(
            panel
                .info
                .get(&remote.id())
                .and_then(|i| i.pull_request.as_ref())
                .map(|p| p.number),
            Some(7)
        );
        // Its pull request is read (by its GitHub name), and a push heard
        // here never counts for it.
        assert!(panel.pr_rows().any(|w| w.id() == "box:/w/a"));
        for w in &mut panel.worktrees {
            if w.host.is_none() {
                w.branch = w.path.file_name().map(|n| n.to_string_lossy().into_owned());
            }
        }
        panel.note_push(Path::new("/r"), None);
        assert!(!panel.pushed.contains_key("box:/w/a"));
        assert!(!panel.main_checkouts().is_empty());
        // Without a GitHub origin it is not asked about.
        for w in &mut panel.worktrees {
            w.github = None;
        }
        assert!(!panel.pr_rows().any(|w| w.host.is_some()));
    }

    #[test]
    fn a_push_is_a_remote_tracking_ref_moving() {
        let git = Path::new("/r/.git");
        let files = |names: &[&str]| names.iter().map(|n| git.join(n)).collect::<Vec<_>>();
        assert_eq!(
            pushed_branches(
                git,
                &files(&[
                    "objects/ab/cdef",
                    "refs/remotes/origin/fix/seed-dates",
                    "refs/remotes/origin/fix/seed-dates.lock",
                    "logs/refs/remotes/origin/fix/seed-dates",
                    "refs/heads/main",
                ])
            ),
            Some(vec!["fix/seed-dates".to_owned()])
        );
        assert_eq!(
            pushed_branches(git, &files(&["objects/ab/cdef"])),
            Some(Vec::new())
        );
        assert_eq!(pushed_branches(git, &files(&["packed-refs"])), None);
        assert_eq!(pushed_branches(git, &[]), None);

        // Only that repository's worktree on that branch is read.
        let mut panel = nested();
        for w in &mut panel.worktrees {
            w.branch = w.path.file_name().map(|n| n.to_string_lossy().into_owned());
        }
        assert!(panel.note_push(Path::new("/r"), Some(&["b".to_owned()])));
        assert_eq!(panel.pushed.keys().collect::<Vec<_>>(), ["/w/b"]);
        assert!(!panel.note_push(Path::new("/r"), Some(&["x".to_owned()])));
        assert!(panel.note_push(Path::new("/r"), None));
        assert_eq!(panel.pushed.len(), 2);
        assert_eq!(
            panel.main_checkouts(),
            [PathBuf::from("/r"), PathBuf::from("/o")]
        );
    }

    #[test]
    fn the_merged_fold_survives_the_arrangement_file() {
        let mut a = Arrangement {
            order: vec!["ws:1".into()],
            ..Arrangement::default()
        };
        a.collapsed.insert("ws:1".into());
        a.merged_open.insert("ws:1".into());
        let text = a.text();
        assert_eq!(text, "fold,merged\tws:1\t\n");
        assert_eq!(Arrangement::parse(&text), a);
        a.rename_id("ws:1", "ws:2");
        assert!(a.merged_open.contains("ws:2"));
    }

    #[test]
    fn worktrees_sit_under_the_folder_holding_their_repository() {
        let mut panel = nested();
        assert_eq!(
            rows(&panel),
            [
                "folder:garden-log",
                ">/w/a",
                ">/w/b",
                "folder:notes",
                "/elsewhere/x"
            ]
        );
        assert_eq!(panel.children("folder:garden-log"), 2);
        assert_eq!(panel.visible(), [0, 1, 2, 3, 4]);
        // Folded: the worktrees have no row and no number.
        panel.toggle_collapsed("folder:garden-log");
        assert_eq!(
            rows(&panel),
            ["folder:garden-log", "folder:notes", "/elsewhere/x"]
        );
        assert_eq!(panel.visible(), [0, 3, 4]);
        assert_eq!(panel.report_tree(), "garden-log(+2),notes,x");
        // The selected one keeps its row, under its folder.
        panel.selected = Some("/w/b".into());
        assert_eq!(
            rows(&panel),
            ["folder:garden-log", ">/w/b", "folder:notes", "/elsewhere/x"]
        );
        assert_eq!(panel.report_tree(), "garden-log(+1),>b,notes,x");
        panel.toggle_collapsed("folder:garden-log");
        assert_eq!(panel.report_tree(), "garden-log,>a,>b,notes,x");
    }

    #[test]
    fn a_dragged_folder_takes_its_worktrees_and_a_worktree_stays_among_siblings() {
        let mut panel = nested();
        let ids = |p: &Panel| p.tasks.iter().map(Workspace::id).collect::<Vec<_>>();
        // The folder last: its worktrees go with it.
        panel.move_before("folder:garden-log", None);
        panel.arrange();
        assert_eq!(
            ids(&panel),
            [
                "folder:notes",
                "/elsewhere/x",
                "folder:garden-log",
                "/w/a",
                "/w/b"
            ]
        );
        // A worktree reorders among its siblings.
        panel.move_before("/w/b", Some("/w/a"));
        panel.arrange();
        assert_eq!(&ids(&panel)[2..], ["folder:garden-log", "/w/b", "/w/a"]);
        // Pinned, a worktree stays under its folder, first of its siblings.
        panel.move_before("/w/b", None);
        panel.toggle_pin("/w/b");
        panel.arrange();
        assert_eq!(&ids(&panel)[2..], ["folder:garden-log", "/w/b", "/w/a"]);
        // Peers: the folder's are the top rows, a worktree's its siblings.
        let at = |id: &str| panel.by_id(id).unwrap();
        assert!(panel.peers("folder:garden-log", at("folder:notes")));
        assert!(!panel.peers("folder:garden-log", at("/w/a")));
        assert!(panel.peers("/w/b", at("/w/a")));
        assert!(!panel.peers("/w/b", at("/elsewhere/x")));
    }

    #[test]
    fn a_worktree_sits_under_the_deepest_folder_holding_its_repository() {
        let mut panel = nested();
        panel.folders.push(folder("code", "/"));
        panel.folders.push(folder("garden-log 2", "/r"));
        for id in ["folder:code", "folder:garden-log 2"] {
            panel.repos.insert(id.into(), vec![PathBuf::from("/r")]);
        }
        panel.rebuild();
        let a = panel.tasks[panel.by_id("/w/a").unwrap()].clone();
        // Deeper than `/`; of the two on `/r`, the first made.
        assert_eq!(panel.parent_of(&a).as_deref(), Some("folder:garden-log"));
        assert_eq!(panel.parent_of(&panel.folders[0].clone()), None);
    }

    #[test]
    fn deepest_folder_holding_the_repo_owns_its_merged_group() {
        // A workspace on a repository (`seedbank` on garden's checkout) under
        // one on the folder holding it: the deeper one takes its worktrees,
        // merged ones too. By design; the fix is never making such a
        // second workspace by accident.
        let mut done = tree("/x/garden/garden/.claude/worktrees/old", false);
        done.repo = Some(PathBuf::from("/x/garden/garden"));
        done.merged = true;
        let mut panel = Panel {
            folders: vec![
                folder("GARDEN", "/x/garden"),
                folder("SEEDBANK", "/x/garden/garden"),
            ],
            worktrees: vec![done],
            ..Panel::default()
        };
        for id in ["folder:GARDEN", "folder:SEEDBANK"] {
            panel
                .repos
                .insert(id.into(), vec![PathBuf::from("/x/garden/garden")]);
        }
        panel.rebuild();
        let old = "/x/garden/garden/.claude/worktrees/old";
        assert_eq!(panel.parent_of_id(old).as_deref(), Some("folder:SEEDBANK"));
        let i = panel.by_id(old).unwrap();
        assert_eq!(panel.merged_under("folder:SEEDBANK"), [i]);
        assert!(panel.merged_under("folder:GARDEN").is_empty());
    }

    #[test]
    fn a_folder_has_one_workspace() {
        let dir = std::env::temp_dir().join(format!("crc-one-workspace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("garden")).unwrap();
        std::os::unix::fs::symlink(dir.join("garden"), dir.join("link")).unwrap();
        // Never written: the panel was not loaded from the file.
        let mut panel = Panel::default();
        let first = panel.add_folder("GARDEN", dir.join("garden")).unwrap();
        // The same folder, as typed, with a slash at its end, or through a
        // symlink: refused, the one there named.
        assert_eq!(
            panel.add_folder("seedbank", dir.join("garden")),
            Err(first.clone())
        );
        let slash = PathBuf::from(format!("{}/", dir.join("garden").display()));
        assert_eq!(panel.add_folder("seedbank", slash), Err(first.clone()));
        assert_eq!(
            panel.add_folder("seedbank", dir.join("link")),
            Err(first.clone())
        );
        assert_eq!(panel.folders.len(), 1);
        // A folder beside it is another.
        assert!(panel.add_folder("seedbank", dir.join("seedbank")).is_ok());
        // On a host: the same path there is refused, on another host or
        // this Mac it is a folder of its own.
        let app = panel.add_remote("app", "myhost", "/srv/app").unwrap();
        assert_eq!(
            panel.add_remote("again", "myhost", "/srv/app/"),
            Err(app.clone())
        );
        assert_eq!(panel.add_remote("again", "myhost", "/srv/app"), Err(app));
        assert!(panel.add_remote("app", "otherhost", "/srv/app").is_ok());
        assert!(panel.add_folder("app", PathBuf::from("/srv/app")).is_ok());
        assert_eq!(panel.folders.len(), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_workspace_never_defaults_to_a_folder_with_one() {
        let mut wt = tree("/x/garden/garden/.claude/worktrees/fix", false);
        wt.repo = Some(PathBuf::from("/x/garden/garden"));
        let mut panel = Panel {
            folders: vec![folder("GARDEN", "/x/garden"), folder("notes", "/x/notes")],
            worktrees: vec![wt],
            ..Panel::default()
        };
        panel.repos.insert(
            "folder:GARDEN".into(),
            vec![PathBuf::from("/x/garden/garden")],
        );
        panel.rebuild();
        let some = |p: &str| Some(PathBuf::from(p));
        // The worktree selected: neither its repository nor its folder.
        panel.selected = Some("/x/garden/garden/.claude/worktrees/fix".into());
        assert_eq!(panel.new_folder(None), None);
        assert_eq!(panel.new_folder(Some(Path::new("/x/garden/garden"))), None);
        assert_eq!(panel.new_folder(Some(Path::new("/x/garden"))), None);
        assert_eq!(panel.new_folder(Some(Path::new("/x/notes"))), None);
        // The open folder while nothing has it.
        assert_eq!(
            panel.new_folder(Some(Path::new("/x/free"))),
            some("/x/free")
        );
        // The folder panel opens beside the repository, so the new one is
        // picked next to it.
        assert_eq!(panel.pick_from(Some(Path::new("/e"))), some("/x/garden"));
        // A folder workspace selected: beside its folder.
        panel.selected = Some("folder:GARDEN".into());
        assert_eq!(panel.new_folder(None), None);
        assert_eq!(panel.pick_from(Some(Path::new("/e"))), some("/x"));
        // Nothing selected: the open folder.
        panel.selected = None;
        assert_eq!(panel.pick_from(Some(Path::new("/e"))), some("/e"));
        assert_eq!(panel.pick_from(None), None);
    }

    #[test]
    fn a_removed_workspace_takes_its_worktrees_with_it() {
        let mut panel = nested();
        panel.toggle_pin("/w/a");
        panel.set_title("/w/b", "login fix");
        let family: Vec<String> = panel
            .family("folder:garden-log")
            .into_iter()
            .map(|i| panel.tasks[i].id())
            .collect();
        assert_eq!(family, ["folder:garden-log", "/w/a", "/w/b"]);
        panel.remove_folder("folder:garden-log");
        // No orphans at the top: the worktrees went with it, and their pin
        // and name too. One no folder holds was never its to take.
        let ids: Vec<String> = panel.tasks.iter().map(Workspace::id).collect();
        assert_eq!(ids, ["folder:notes", "/elsewhere/x"]);
        assert!(!panel.is_pinned("/w/a"));
        assert!(!panel.arrangement.titles.contains_key("/w/b"));
        assert!(panel.family("folder:garden-log").is_empty());

        // Another folder workspace holding the repository keeps them.
        let mut panel = nested();
        panel.folders.push(folder("code", "/"));
        panel
            .repos
            .insert("folder:code".into(), vec![PathBuf::from("/r")]);
        panel.rebuild();
        panel.remove_folder("folder:garden-log");
        let a = panel.tasks[panel.by_id("/w/a").unwrap()].clone();
        assert_eq!(panel.parent_of(&a).as_deref(), Some("folder:code"));
    }

    #[test]
    fn folding_survives_the_arrangement_file_and_reads_old_ones() {
        let mut a = Arrangement {
            order: vec!["ws:1".into(), "ws:2".into(), "/w/a".into()],
            ..Arrangement::default()
        };
        a.pinned.insert("ws:1".into());
        a.collapsed.insert("ws:1".into());
        a.collapsed.insert("ws:2".into());
        a.titles.insert("/w/a".into(), "login fix".into());
        let text = a.text();
        assert!(text.starts_with("pin,fold\tws:1\t\nfold\tws:2\t\n-\t/w/a\tlogin fix\n"));
        assert_eq!(Arrangement::parse(&text), a);
        // A file from before folding.
        let old = Arrangement::parse("pin\tws:1\t\n-\t/w/a\tmine\n");
        assert!(old.pinned.contains("ws:1"));
        assert!(old.collapsed.is_empty());
        assert_eq!(old.titles["/w/a"], "mine");
        a.rename_id("ws:2", "ws:3");
        assert!(a.collapsed.contains("ws:3"));
    }

    #[test]
    fn a_workspace_follows_its_terminal_and_keeps_a_name_you_gave_it() {
        let mut panel = Panel::default();
        let auto = panel
            .add_folder("ch-ui", PathBuf::from("/tmp/crc-move/ch-ui"))
            .unwrap();
        let mine = panel
            .add_folder("CRC", PathBuf::from("/tmp/crc-move/crc"))
            .unwrap();
        panel.move_folder(&auto, Path::new("/tmp/crc-move/duck-ui"));
        panel.move_folder(&mine, Path::new("/tmp/crc-move/duck-ui"));
        let by = |id: &str| panel.tasks[panel.by_id(id).unwrap()].clone();
        assert_eq!(by(&auto).name, "duck-ui");
        assert_eq!(by(&auto).path, PathBuf::from("/tmp/crc-move/duck-ui"));
        assert_eq!(by(&mine).name, "CRC");
        assert_eq!(by(&mine).path, PathBuf::from("/tmp/crc-move/duck-ui"));
        assert!(named_after("crc 2", Path::new("/a/crc")));
        assert!(!named_after("crc two", Path::new("/a/crc")));
    }

    #[test]
    fn a_hosts_shell_finds_the_repositories_discover_does() {
        // The line a host runs, run by this Mac's shell, in a folder whose
        // name needs quoting.
        let dir =
            std::env::temp_dir().join(format!("crc-discover-line it's {}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["b/.git", "a/.git", ".hidden/.git", "plain", "c/d/.git"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        std::fs::create_dir_all(dir.join("w")).unwrap();
        std::fs::write(dir.join("w/.git"), "gitdir: /r/.git/worktrees/w\n").unwrap();
        std::fs::create_dir_all(dir.join("s")).unwrap();
        std::fs::write(dir.join("s/.git"), "gitdir: ../.git/modules/s\n").unwrap();
        let shell = |dir: &Path| {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(discover_line(dir))
                .output()
                .unwrap();
            (
                out.status.success(),
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(PathBuf::from)
                    .collect::<Vec<_>>(),
            )
        };
        let real = crate::platform::canonical(&dir);
        let local: Vec<PathBuf> = discover(&dir)
            .iter()
            .map(|p| crate::platform::canonical(p))
            .collect();
        assert_eq!(shell(&dir), (true, local));
        assert_eq!(
            shell(&dir).1,
            vec![real.join("a"), real.join("b"), real.join("s")]
        );
        // Not there: it fails, and finds nothing.
        assert_eq!(shell(&dir.join("gone")), (false, Vec::new()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hosts_rows_stay_while_it_does_not_answer() {
        let mut app = folder("app", "/srv");
        app.host = Some("box".into());
        // A folder of this Mac on a path like the host's.
        let mut panel = Panel {
            folders: vec![folder("srv", "/srv"), app],
            ..Panel::default()
        };
        let row = |path: &str, branch: &str| {
            let mut w = tree(path, false);
            w.branch = Some(branch.into());
            w.name = branch.into();
            w.repo = Some(PathBuf::from("/srv/app"));
            w.host = Some("box".into());
            w
        };
        let listing =
            |host: Option<&str>, repos: Vec<(&str, &[&str])>, rows, unreachable: &[&str]| Listing {
                host: host.map(str::to_owned),
                repos: repos
                    .into_iter()
                    .map(|(id, r)| (id.to_owned(), r.iter().map(PathBuf::from).collect()))
                    .collect(),
                worktrees: rows,
                prunable: std::collections::HashMap::new(),
                github: std::collections::HashMap::new(),
                unreachable: unreachable.iter().map(|s| (*s).to_owned()).collect(),
            };
        let take = |panel: &mut Panel, l: Listing| {
            panel.take_listing(l);
            panel.rebuild();
        };
        take(
            &mut panel,
            listing(
                Some("box"),
                vec![("folder:app", &["/srv/app"])],
                vec![row("/srv/wt/fix", "fix")],
                &[],
            ),
        );
        // This Mac's read, after: the host's rows stay, under their own
        // folder, not this Mac's folder with the same repository path.
        take(
            &mut panel,
            listing(None, vec![("folder:srv", &["/srv/app"])], Vec::new(), &[]),
        );
        assert_eq!(panel.report_tree(), "srv,app,>fix");
        assert_eq!(panel.tasks[2].id(), "box:/srv/wt/fix");
        // Nothing of the host is watched or asked of gh here.
        assert!(panel.worktree_paths().is_empty());
        assert!(panel.main_checkouts().is_empty());
        // The host does not answer: the rows it had stay, and its folder
        // says so.
        take(
            &mut panel,
            listing(Some("box"), Vec::new(), Vec::new(), &["folder:app"]),
        );
        assert_eq!(panel.report_tree(), "srv,app(unreachable),>fix");
        assert_eq!(
            panel.repos.get("folder:app"),
            Some(&vec![PathBuf::from("/srv/app")])
        );
        // It answers again: what it says now.
        take(
            &mut panel,
            listing(
                Some("box"),
                vec![("folder:app", &["/srv/app"])],
                vec![row("/srv/wt/next", "next")],
                &[],
            ),
        );
        assert_eq!(panel.report_tree(), "srv,app,>next");
        assert!(panel.unreachable.is_empty());
    }

    #[test]
    fn a_folder_inside_a_repository_finds_that_repository() {
        let dir = std::env::temp_dir().join(format!("crc-enclosing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        assert_eq!(discover(&dir.join("src/deep")), vec![dir.clone()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_worktree_row_says_what_it_has_been_doing() {
        let secs = std::time::Duration::from_secs;
        let line =
            |pace, terminals, ahead| worktree_line(pace, terminals, ahead, false).join(" \u{b7} ");
        // Written in just now: writing, whatever the last commit.
        let writing = Pace::of(Some(secs(3)), Some(secs(7200)));
        assert_eq!(writing, Pace::Writing);
        assert_eq!(line(writing, 0, 2), "writing \u{b7} 2 ahead");
        // Quiet since, in minutes, then hours.
        assert_eq!(
            line(Pace::of(Some(secs(300)), None), 0, 0),
            "quiet \u{b7} 5 min"
        );
        assert_eq!(
            line(Pace::of(Some(secs(9)), None), 1, 0),
            "quiet \u{b7} <1 min \u{b7} 1 terminal"
        );
        assert_eq!(
            line(Pace::of(Some(secs(3 * 3600 + 59)), None), 2, 1),
            "quiet \u{b7} 3 h \u{b7} 2 terminals \u{b7} 1 ahead"
        );
        // No write seen this run: the branch's last commit.
        assert_eq!(
            line(Pace::of(None, Some(secs(7200))), 0, 4),
            "commit 2 h ago \u{b7} 4 ahead"
        );
        assert_eq!(
            line(Pace::of(None, Some(secs(3 * 86400))), 0, 0),
            "commit 3 d ago"
        );
        // Nothing known, no terminal: never "no terminal".
        assert_eq!(line(Pace::of(None, None), 0, 1), "1 ahead");
        assert_eq!(line(Pace::Unknown, 0, 0), "");
        // Merged: said first, no longer "ahead" of anything, and quiet
        // time left out; still writing is still said.
        assert_eq!(
            worktree_line(Pace::of(None, Some(secs(7200))), 1, 1, true).join(" \u{b7} "),
            "merged \u{b7} 1 terminal"
        );
        assert_eq!(
            worktree_line(Pace::Writing, 0, 1, true).join(" \u{b7} "),
            "merged \u{b7} writing"
        );
    }

    #[test]
    fn a_hook_write_marks_the_worktree_holding_it() {
        let mut panel = nested();
        panel.note_write(Path::new("/w/a/src/beds.rs"));
        // A folder workspace's file is not a worktree's.
        panel.note_write(Path::new("/n/notes.md"));
        assert_eq!(panel.report_activity(), "a=writing,b=-,x=-");
        assert_eq!(panel.wrote.len(), 1);
    }

    #[test]
    fn a_line_from_before_keys_keeps_its_old_id() {
        let old = parse_folders("notes\t/r\n");
        assert_eq!(old[0].id(), "folder:notes");
        let new = parse_folders("notes\t/r\tws:1\n");
        assert_eq!(new[0].id(), "ws:1");
    }

    #[test]
    fn a_new_workspace_starts_empty() {
        // A removed workspace's name, pin and sessions do not come back
        // on a new one with the same name.
        let mut panel = Panel::default();
        let dir = PathBuf::from("/tmp/crc-agents-test");
        let old = panel.add_folder("crc", dir.clone()).unwrap();
        panel.set_title(&old, "Mine");
        panel.toggle_pin(&old);
        panel.remove_folder(&old);
        let new = panel.add_folder("crc", dir.clone()).unwrap();
        assert_ne!(old, new);
        let task = panel.tasks[panel.by_id(&new).unwrap()].clone();
        assert_eq!(panel.title(&task), "crc");
        assert!(!panel.is_pinned(&new));
        // A session whose workspace is gone stays with the first one on
        // its folder (two from before can share one), not the newest.
        let first = panel.tasks[panel.by_id(&new).unwrap()].clone();
        let mut second = first.clone();
        second.key = "ws:second".into();
        second.name = "crc 2".into();
        panel.folders.push(second);
        panel.rebuild();
        let at = folder_task(&panel.tasks, &first.path.join("src"));
        assert_eq!(at.map(|i| panel.tasks[i].id()), Some(new));
    }

    #[test]
    fn an_issue_reaches_claude_as_a_quoted_path_never_as_its_words() {
        use std::os::unix::fs::PermissionsExt;
        // A home with a quote and a command in it, and an issue that tries
        // to end the quoting with its title and body.
        let dir = std::env::temp_dir().join(format!(
            "crc-issue-'; touch pwned; '-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let hostile = "'; rm -rf ~; '";
        let issue = crate::project::worktree::Issue {
            number: 42,
            title: hostile.into(),
            body: hostile.into(),
            ..Default::default()
        };
        let repo = here("/r");
        let text = crate::project::worktree::issue_markdown(&issue);
        let file = write_issue_file(&dir, &repo, issue.number, &text).unwrap();
        assert_eq!(file, issue_file(&dir, &repo, 42));
        assert!(file.starts_with(dir.join("issues")));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let prompt = issue_prompt(&file, issue.number);
        let command = with_prompt("claude", Some(&prompt));
        assert!(!command.contains(hostile), "{command}");
        assert!(command.starts_with("claude '"), "{command}");
        // `sh` reads it back as one argument: the prompt, whole.
        let echo = with_prompt("printf '%s|%s' \"$#\"", Some(&prompt));
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "set -- {}; {echo}",
                crate::project::review::shell_quote(&prompt)
            ))
            .current_dir(&dir)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), format!("1|{prompt}"));
        assert!(!dir.join("pwned").exists());
        assert_eq!(with_prompt("claude", None), "claude");
        // Written again, a file left with a looser mode is made 0600.
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_issue_file(&dir, &repo, issue.number, "again").unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // Another repository's issue of the same number has its own file.
        assert_ne!(issue_file(&dir, &here("/other"), 42), file,);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn start_task_goes_back_to_an_issues_worktree() {
        let tree = |path: &str, branch: Option<&str>, main: bool| Worktree {
            path: PathBuf::from(path),
            branch: branch.map(str::to_owned),
            main,
        };
        let list = vec![
            tree("/r", Some("issue-7-fix"), true),
            tree("/w/a", Some("issue-7-fix-login"), false),
            tree("/w/b", None, false),
        ];
        assert_eq!(
            worktree_on_branch(&list, "issue-7-fix-login"),
            Some(PathBuf::from("/w/a"))
        );
        // The main checkout on that branch is not a task to go back to.
        assert_eq!(worktree_on_branch(&list, "issue-7-fix"), None);
        assert_eq!(worktree_on_branch(&list, "issue-8"), None);
    }

    #[test]
    fn an_issue_tab_is_named_for_its_number_and_title() {
        assert_eq!(issue_tab_label(42, " Fix login "), "#42 Fix login");
        let long = issue_tab_label(
            7,
            "Seed tray drains too slowly after watering in the morning",
        );
        assert_eq!(long, "#7 Seed tray drains too slowly after wa\u{2026}");
    }
}
