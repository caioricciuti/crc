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
use crate::project::worktree::Worktree;
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
}

/// What a worker read of the folder workspaces: per folder workspace (by
/// id), the repositories in it; and the worktrees of those repositories
/// other than their main checkouts, every path resolved.
struct Listing {
    repos: Vec<(String, Vec<PathBuf>)>,
    worktrees: Vec<Workspace>,
}

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
    /// The main checkout's branch, which a worktree's is compared with.
    pub base: Option<String>,
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
        }
    }

    pub fn name(&self) -> String {
        self.name.clone()
    }

    /// What sessions and the selection hold on to: a folder workspace by
    /// its key (two can share a folder), a worktree by its path.
    pub fn id(&self) -> String {
        if self.folder {
            self.key.clone()
        } else {
            self.path.to_string_lossy().into_owned()
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
/// name. Hidden folders are skipped.
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
        .filter(|p| p.is_dir() && p.join(".git").exists())
        .collect();
    children.sort();
    found.extend(children);
    found
}

/// Reads what the folder workspaces hold, on the calling thread: their
/// repositories, and each repository's worktrees once.
fn scan(folders: &[(String, PathBuf)]) -> Listing {
    let mut repos = Vec::new();
    let mut worktrees: Vec<Workspace> = Vec::new();
    let mut mains: Vec<PathBuf> = Vec::new();
    for (id, dir) in folders {
        let found: Vec<PathBuf> = discover(dir)
            .iter()
            .map(|p| crate::platform::canonical(p))
            .collect();
        for repo in &found {
            let Ok(list) = crate::project::worktree::list(repo) else {
                continue;
            };
            let list: Vec<Worktree> = list
                .into_iter()
                .map(|mut w| {
                    w.path = crate::platform::canonical(&w.path);
                    w
                })
                .collect();
            let Some(main) = list.iter().find(|w| w.main).cloned() else {
                continue;
            };
            if mains.contains(&main.path) {
                continue;
            }
            mains.push(main.path.clone());
            for w in list.into_iter().filter(|w| !w.main) {
                if !worktrees.iter().any(|t| t.path == w.path) {
                    worktrees.push(Workspace::of(
                        w,
                        Some(main.path.clone()),
                        main.branch.clone(),
                    ));
                }
            }
        }
        repos.push((id.clone(), found));
    }
    Listing { repos, worktrees }
}

/// What the list keeps of your arrangement, per workspace id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Arrangement {
    /// Ids in the order you put them; ids not here follow, in Git's order.
    pub order: Vec<String>,
    pub pinned: std::collections::HashSet<String>,
    /// A name of your own for a workspace, shown instead of its branch.
    pub titles: std::collections::HashMap<String, String>,
}

impl Arrangement {
    /// One per line: `pin` or `-`, a tab, the id, a tab, the name (maybe
    /// empty).
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
            if flag == "pin" {
                out.pinned.insert(id.to_owned());
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
                format!(
                    "{}\t{id}\t{}\n",
                    if self.pinned.contains(id) { "pin" } else { "-" },
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

/// One per line: the name, a tab, the folder, a tab, the key. A line
/// written before keys existed keeps the id it had, `folder:` and its name,
/// so the sessions holding it stay in it.
fn parse_folders(text: &str) -> Vec<Workspace> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let (name, path) = (parts.next()?, parts.next()?);
            let key = parts
                .next()
                .filter(|k| !k.is_empty())
                .map_or_else(|| format!("folder:{name}"), str::to_owned);
            Some((name, path, key))
        })
        .filter(|(name, path, _)| !name.is_empty() && !path.is_empty())
        .map(|(name, path, key)| Workspace {
            name: name.to_owned(),
            path: PathBuf::from(path),
            branch: None,
            main: false,
            folder: true,
            key,
            repo: None,
            base: None,
        })
        .collect()
}

fn folders_text(folders: &[Workspace]) -> String {
    folders
        .iter()
        .map(|w| format!("{}\t{}\t{}\n", w.name, w.path.display(), w.key))
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
/// ahead of the main checkout, and its pull request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Info {
    pub ahead: usize,
    pub pull_request: Option<crate::project::worktree::PullRequest>,
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
    /// Per task path, once read.
    pub info: std::collections::HashMap<PathBuf, Info>,
    info_rx: Option<mpsc::Receiver<(PathBuf, Info)>>,
    /// Per session (by its review folder), what its team hooks said.
    pub teams: std::collections::HashMap<PathBuf, Team>,
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
    /// The workspace whose name is being edited in its row, by id, with
    /// the text so far and the caret's character index; set by the window
    /// from its field before each draw.
    pub inline: Option<(String, String, usize)>,
    /// Whether the terminal panel was open in the editor before Agents
    /// mode took the column, to put it back as it was.
    pub editor_terminal: bool,
    pub scroll: usize,
    pub hits: Vec<(Viewport, Action)>,
}

/// A task operation that worked: what to say, and a folder to start Claude
/// in (a new task's).
pub struct Done {
    pub said: String,
    pub start_in: Option<PathBuf>,
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
/// the others.
pub fn folder_task(tasks: &[Workspace], folder: &Path) -> Option<usize> {
    tasks
        .iter()
        .enumerate()
        .filter(|(_, t)| folder.starts_with(&t.path))
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
/// else a new one is made there.
pub fn follow(tasks: &[Workspace], owner: usize, cwd: &Path) -> Follow {
    let task = &tasks[owner];
    if task.path == cwd {
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
    /// in each folder workspace and their worktrees.
    pub fn refresh(&mut self) {
        self.refreshed_at = Some(std::time::Instant::now());
        self.inspector.invalidate();
        if self.github.is_none() {
            self.github = Some(crate::project::worktree::has_gh());
        }
        if !self.loaded {
            self.load();
        }
        let folders: Vec<(String, PathBuf)> = self
            .folders
            .iter()
            .map(|f| (f.id(), f.path.clone()))
            .collect();
        let (tx, rx) = mpsc::channel();
        let (info_tx, info_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let listing = scan(&folders);
            let tasks = listing.worktrees.clone();
            let _ = tx.send(listing);
            // Then what each row says, slower: `gh` asks GitHub. A test
            // instance does not.
            let ask_github = std::env::var_os("CRC_SELFTEST").is_none();
            for task in tasks {
                let (Some(branch), Some(main)) = (&task.branch, &task.repo) else {
                    continue;
                };
                let info = Info {
                    ahead: crate::project::worktree::ahead(main, branch).unwrap_or(0),
                    pull_request: ask_github
                        .then(|| crate::project::worktree::pull_request(&task.path, branch))
                        .flatten(),
                };
                if info_tx.send((task.path, info)).is_err() {
                    return;
                }
            }
        });
        self.rx = Some(rx);
        self.info_rx = Some(info_rx);
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

    /// Takes a finished read. Returns whether anything changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        if let Some(rx) = &self.info_rx {
            loop {
                match rx.try_recv() {
                    Ok((path, info)) => {
                        self.info.insert(path, info);
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
        changed | self.poll_list() | self.inspector.poll()
    }

    fn poll_list(&mut self) -> bool {
        let Some(rx) = &self.rx else {
            return false;
        };
        let listing = match rx.try_recv() {
            Ok(listing) => listing,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => Listing {
                repos: Vec::new(),
                worktrees: self.worktrees.clone(),
            },
        };
        self.rx = None;
        self.worktrees = listing.worktrees;
        self.repos = listing.repos.into_iter().collect();
        self.rebuild();
        true
    }

    /// The list from Git and the folder workspaces, in your order with the
    /// pinned on top.
    fn rebuild(&mut self) {
        let mut tasks: Vec<Workspace> = self
            .folders
            .iter()
            .chain(self.worktrees.iter())
            .cloned()
            .collect();
        tasks.sort_by_key(|t| {
            let id = t.id();
            (
                !t.main,
                !self.arrangement.pinned.contains(&id),
                self.arrangement.position(&id),
            )
        });
        self.tasks = tasks;
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

    /// Adds a folder workspace, its name made unique, and returns its id.
    pub fn add_folder(&mut self, name: &str, path: PathBuf) -> String {
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
            path: crate::platform::canonical(&path),
            branch: None,
            main: false,
            folder: true,
            key: new_key(&self.folders),
            repo: None,
            base: None,
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

    /// The workspace a dragged row lands before, by the cards last drawn:
    /// the first whose middle is under `y`; `None` is the end.
    pub fn drop_before(&self, y: f32) -> Option<String> {
        self.hits
            .iter()
            .filter_map(|(rect, action)| match action {
                Action::Select(i) => Some((*rect, *i)),
                _ => None,
            })
            .find(|(rect, _)| rect.y + rect.height * 0.5 > y)
            .and_then(|(_, i)| self.tasks.get(i).map(Workspace::id))
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

    /// Forgets a folder workspace, and its place, pin and name with it.
    pub fn remove_folder(&mut self, id: &str) {
        self.folders.retain(|t| t.id() != id);
        self.arrangement.order.retain(|o| o != id);
        self.arrangement.pinned.remove(id);
        self.arrangement.titles.remove(id);
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
        self.loaded && self.rx.is_none()
    }

    /// A read or an operation is in flight.
    pub fn busy(&self) -> bool {
        self.rx.is_some() || self.op.is_some() || self.info_rx.is_some() || self.inspector.busy()
    }

    /// The workspace by id.
    pub fn by_id(&self, id: &str) -> Option<usize> {
        self.tasks.iter().position(|t| t.id() == id)
    }

    /// The repositories a workspace's inspector shows: a worktree's own
    /// folder, or the ones found in a folder workspace (`None` until read).
    pub fn repos_of(&self, workspace: &Workspace) -> Option<Vec<PathBuf>> {
        if workspace.folder {
            self.repos.get(&workspace.id()).cloned()
        } else {
            Some(vec![workspace.path.clone()])
        }
    }

    /// The folder a new workspace takes unless you choose one: the selected
    /// workspace's (a worktree's repository), else `fallback`, the editor's.
    pub fn new_folder(&self, fallback: Option<&Path>) -> Option<PathBuf> {
        self.selected_index()
            .map(|i| &self.tasks[i])
            .map(|t| t.repo.clone().unwrap_or_else(|| t.path.clone()))
            .or_else(|| fallback.map(Path::to_path_buf))
    }

    /// The repository a new branch is made in: the selected worktree's, or
    /// the first one in the selected folder workspace.
    pub fn task_repo(&self) -> Option<PathBuf> {
        let task = &self.tasks[self.selected_index()?];
        match &task.repo {
            Some(repo) => Some(repo.clone()),
            None => self.repos_of(task)?.into_iter().next(),
        }
    }

    /// Puts the list in your order and nothing else: the pinned first,
    /// then the order you dragged them into, then the rest as they were
    /// made. Nothing that happens in a terminal moves a row. Returns
    /// whether the order changed.
    pub fn arrange(&mut self) -> bool {
        let canon: Vec<Workspace> = self
            .folders
            .iter()
            .chain(self.worktrees.iter())
            .cloned()
            .collect();
        let mut order: Vec<usize> = (0..canon.len()).collect();
        // The sort is stable: unplaced ones keep the order they were made.
        order.sort_by_key(|i| {
            let id = canon[*i].id();
            (
                !canon[*i].main,
                !self.arrangement.pinned.contains(&id),
                self.arrangement.position(&id),
            )
        });
        let arranged: Vec<Workspace> = order.into_iter().map(|i| canon[i].clone()).collect();
        if arranged == self.tasks {
            return false;
        }
        self.tasks = arranged;
        true
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

    /// One row per workspace, as cmux and supacode have it: its sessions
    /// are the terminal's tabs, not rows here. The selected workspace's
    /// team tasks follow it; a session no workspace holds gets a row.
    fn lines(&self, tabs: &[Tab]) -> Vec<Line> {
        let mut lines = Vec::new();
        let owner: Vec<Option<usize>> = tabs.iter().map(|t| task_of(&self.tasks, t)).collect();
        let selected = self.selected_index();
        for task in 0..self.tasks.len() {
            lines.push(Line::Task(task));
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

    fn height(line: &Line) -> f32 {
        match line {
            Line::Task(_) => CARD,
            _ => ROW,
        }
    }

    /// The first line that can be at the top: the list's end stays at the
    /// bottom of `room`, not above it.
    fn max_scroll(lines: &[Line], room: f32) -> usize {
        let mut used = 0.0;
        for (n, line) in lines.iter().enumerate().rev() {
            used += Panel::height(line);
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
        let max = Panel::max_scroll(&self.lines(tabs), Panel::room(column));
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
    /// `agents.add.0`, `agents.menu.0`, `agents.blank`. As last drawn.
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
        self.scroll = self.scroll.min(Panel::max_scroll(&lines, room));
        let selected = self.selected_index();
        let mut placed = Vec::new();
        let mut used = 0.0;
        for line in lines.iter().skip(self.scroll) {
            let h = Panel::height(line);
            if used + h > room {
                break;
            }
            placed.push((top + used, line));
            used += h;
        }
        let blank_top = top + used;
        if let Some(drag) = self.drag.as_ref().filter(|d| d.active) {
            let cards: Vec<f32> = placed
                .iter()
                .filter(|(_, line)| matches!(line, Line::Task(_)))
                .map(|(y, _)| *y)
                .collect();
            let at = cards
                .iter()
                .copied()
                .find(|y| y + CARD * 0.5 > drag.y)
                .or_else(|| cards.last().map(|y| y + CARD));
            if let Some(at) = at {
                layout::push_rect(
                    out,
                    atlas,
                    [column.x + 8.0, at - 1.0],
                    [column.width - 16.0, 2.0],
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
                    self.draw_card(*index, tabs, y, column, selected, atlas, theme, out);
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
    /// A workspace's row: its icon (a spinner while an agent in it says it
    /// is working), its name, and under it the most useful line there is:
    /// what a session waiting on you asks, what a working agent says, its
    /// team, else its terminals and where it is. At the right a steady
    /// strip: how many wait on you, and the pull request coloured by its
    /// checks; hover trades the strip for the menu button.
    #[allow(clippy::too_many_arguments)]
    fn draw_card(
        &mut self,
        index: usize,
        tabs: &[Tab],
        y: f32,
        column: Viewport,
        selected: Option<usize>,
        atlas: &mut Atlas,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) {
        use crate::term::status::State;
        let task = self.tasks[index].clone();
        let card = Viewport {
            x: column.x + 6.0,
            y: y + 1.0,
            width: (column.width - 12.0).max(0.0),
            height: CARD - 2.0,
        };
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
        let left = card.x + 10.0;
        let line1 = Viewport {
            x: left,
            y: card.y + 4.0,
            width: card.width - 20.0,
            height: 20.0,
        };
        let line2 = Viewport {
            y: card.y + 22.0,
            ..line1
        };
        // A point lower than the name's box: the glyph sits on its baseline.
        let icon_box = Viewport {
            x: left,
            y: line1.y + 1.0,
            width: 16.0,
            ..line1
        };
        if working.is_some() {
            layout::push_spinner(out, left, line1, theme.accent);
        } else {
            layout::push_icon_centered(
                out,
                atlas,
                icon_box,
                if remote.is_some() {
                    icons::SERVER
                } else if task.main {
                    icons::HOME
                } else if task.folder {
                    icons::FOLDER
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
                .get(&task.path)
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
            match mine.len() {
                0 => parts.push("no terminal".to_owned()),
                1 => parts.push("1 terminal".to_owned()),
                n => parts.push(format!("{n} terminals")),
            }
            if let Some(ahead) = self
                .info
                .get(&task.path)
                .map(|i| i.ahead)
                .filter(|a| *a > 0)
            {
                parts.push(format!("{ahead} ahead"));
            }
            // A folder workspace says which folder, unless its name does; a
            // workspace you named says its branch.
            let folder = task
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|n| task.folder && *n != name);
            if !task.folder && name != task.name() {
                parts.push(task.name());
            }
            if let Some(folder) = folder {
                parts.push(folder);
            }
            (parts.join(" \u{b7} "), theme.gutter_text)
        };
        layout::push_ui_text(
            out,
            atlas,
            Viewport {
                x: name_x,
                width: (line2.x + line2.width - name_x).max(0.0),
                ..line2
            },
            &said,
            colour,
        );
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
        }
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
        let dir = PathBuf::from("/tmp/crc-agents-test");
        let a = panel.add_folder("notes", dir.clone());
        let b = panel.add_folder("notes", dir);
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
        // A linked worktree's .git is a file.
        std::fs::create_dir_all(dir.join("w")).unwrap();
        std::fs::write(dir.join("w/.git"), "gitdir: x\n").unwrap();
        assert_eq!(
            discover(&dir),
            vec![dir.join("a"), dir.join("b"), dir.join("w")]
        );
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        assert_eq!(discover(&dir)[0], dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_workspace_follows_its_terminal_and_keeps_a_name_you_gave_it() {
        let mut panel = Panel::default();
        let auto = panel.add_folder("ch-ui", PathBuf::from("/tmp/crc-move/ch-ui"));
        let mine = panel.add_folder("CRC", PathBuf::from("/tmp/crc-move/ch-ui"));
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
    fn a_folder_inside_a_repository_finds_that_repository() {
        let dir = std::env::temp_dir().join(format!("crc-enclosing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        assert_eq!(discover(&dir.join("src/deep")), vec![dir.clone()]);
        let _ = std::fs::remove_dir_all(&dir);
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
        let old = panel.add_folder("crc", dir.clone());
        panel.set_title(&old, "Mine");
        panel.toggle_pin(&old);
        panel.remove_folder(&old);
        let new = panel.add_folder("crc", dir.clone());
        assert_ne!(old, new);
        let task = panel.tasks[panel.by_id(&new).unwrap()].clone();
        assert_eq!(panel.title(&task), "crc");
        assert!(!panel.is_pinned(&new));
        // A session whose workspace is gone stays with the first one on
        // its folder, not the newest.
        let first = panel.tasks[panel.by_id(&new).unwrap()].clone();
        panel.add_folder("crc", dir.clone());
        let at = folder_task(&panel.tasks, &first.path.join("src"));
        assert_eq!(at.map(|i| panel.tasks[i].id()), Some(new));
    }
}
