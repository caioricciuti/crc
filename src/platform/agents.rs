//! Agents mode: the terminal takes the editor column and the sidebar lists
//! workspaces, each with the sessions running in it: one per worktree of the
//! open repository, read from Git, and the ones added by hand on any folder.
//! The toolbar's Editor / Agents switch (Cmd-Shift-A) flips between the two;
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

/// Worktrees read on a worker: the main checkout they are for, and the list
/// with every path resolved.
type Listing = (PathBuf, Result<Vec<Worktree>, String>);

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
}

impl Workspace {
    fn of(worktree: Worktree) -> Workspace {
        Workspace {
            name: worktree.name(),
            path: worktree.path,
            branch: worktree.branch,
            main: worktree.main,
            folder: false,
        }
    }

    pub fn name(&self) -> String {
        self.name.clone()
    }

    /// What sessions and the selection hold on to: a folder workspace by
    /// its name (two can share a folder), a worktree by its path.
    pub fn id(&self) -> String {
        if self.folder {
            format!("folder:{}", self.name)
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

/// Where a project's folder workspaces are kept, by its root.
fn folders_file(root: &Path) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let key = crate::platform::fnv1a(root.as_os_str().as_bytes());
    Some(crate::platform::app_support()?.join(format!("agents/{key:016x}.workspaces")))
}

/// One per line: the name, a tab, the folder.
fn parse_folders(text: &str) -> Vec<Workspace> {
    text.lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(name, path)| !name.is_empty() && !path.is_empty())
        .map(|(name, path)| Workspace {
            name: name.to_owned(),
            path: PathBuf::from(path),
            branch: None,
            main: false,
            folder: true,
        })
        .collect()
}

fn folders_text(folders: &[Workspace]) -> String {
    folders
        .iter()
        .map(|w| format!("{}\t{}\n", w.name, w.path.display()))
        .collect()
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
    /// The main checkout the worktrees belong to.
    pub repo: Option<PathBuf>,
    /// The project root the folder workspaces are kept for.
    root: Option<PathBuf>,
    /// Every workspace, worktrees first, then the folder ones.
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
    /// A failed read or operation, said under the header.
    pub note: Option<String>,
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
/// folder workspace on the same folder.
pub fn folder_task(tasks: &[Workspace], folder: &Path) -> Option<usize> {
    tasks
        .iter()
        .enumerate()
        .filter(|(_, t)| folder.starts_with(&t.path))
        .max_by_key(|(_, t)| (t.path.components().count(), !t.folder))
        .map(|(i, _)| i)
}

impl Panel {
    /// Reads the worktrees of the repository `dir` is in, on a worker, and
    /// the folder workspaces kept for it.
    pub fn refresh(&mut self, dir: &Path) {
        self.refreshed_at = Some(std::time::Instant::now());
        self.inspector.invalidate();
        if self.github.is_none() {
            self.github = Some(crate::project::worktree::has_gh());
        }
        let root = crate::platform::canonical(dir);
        if self.root.as_deref() != Some(root.as_path()) {
            self.folders = folders_file(&root)
                .and_then(|file| std::fs::read_to_string(file).ok())
                .map(|text| parse_folders(&text))
                .unwrap_or_default();
            self.root = Some(root);
            self.rebuild();
        }
        let (tx, rx) = mpsc::channel();
        let (info_tx, info_rx) = mpsc::channel();
        let dir = dir.to_path_buf();
        std::thread::spawn(move || {
            let listing = crate::project::worktree::list(&dir).map(|list| {
                list.into_iter()
                    .map(|mut w| {
                        w.path = crate::platform::canonical(&w.path);
                        w
                    })
                    .collect::<Vec<_>>()
            });
            let main = match &listing {
                Ok(list) => list
                    .iter()
                    .find(|w| w.main)
                    .map_or_else(|| dir.clone(), |w| w.path.clone()),
                Err(_) => dir,
            };
            let tasks = listing.as_ref().map_or_else(|_| Vec::new(), Clone::clone);
            let _ = tx.send((main.clone(), listing));
            // Then what each row says, slower: `gh` asks GitHub. A test
            // instance does not.
            let ask_github = std::env::var_os("CRC_SELFTEST").is_none();
            for task in tasks.into_iter().filter(|t| !t.main) {
                let Some(branch) = &task.branch else {
                    continue;
                };
                let info = Info {
                    ahead: crate::project::worktree::ahead(&main, branch).unwrap_or(0),
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
        let Ok((main, listing)) = rx.try_recv() else {
            return false;
        };
        self.rx = None;
        match listing {
            Ok(tasks) => {
                self.repo = Some(main);
                self.worktrees = tasks.into_iter().map(Workspace::of).collect();
                if self
                    .note
                    .as_deref()
                    .is_some_and(|n| n.starts_with("No Git"))
                {
                    self.note = None;
                }
            }
            Err(_) => {
                self.repo = None;
                self.worktrees.clear();
                self.note = Some("No Git repository here: workspaces are folders".into());
            }
        }
        self.rebuild();
        true
    }

    fn rebuild(&mut self) {
        self.tasks = self
            .worktrees
            .iter()
            .chain(self.folders.iter())
            .cloned()
            .collect();
    }

    fn save_folders(&self) {
        let Some(file) = self.root.as_deref().and_then(folders_file) else {
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
        Some(new_id)
    }

    pub fn remove_folder(&mut self, id: &str) {
        self.folders.retain(|t| t.id() != id);
        self.rebuild();
        self.save_folders();
    }

    /// The workspaces are known: the worktree read for the open folder
    /// has answered, so an empty list means there are none.
    pub fn listed(&self) -> bool {
        self.root.is_some() && self.rx.is_none()
    }

    /// A read or an operation is in flight.
    pub fn busy(&self) -> bool {
        self.rx.is_some() || self.op.is_some() || self.info_rx.is_some() || self.inspector.busy()
    }

    /// The workspace by id.
    pub fn by_id(&self, id: &str) -> Option<usize> {
        self.tasks.iter().position(|t| t.id() == id)
    }

    /// The main checkout's branch: what other branches are compared with.
    pub fn base_branch(&self) -> Option<String> {
        self.tasks
            .iter()
            .find(|t| t.main)
            .and_then(|t| t.branch.clone())
    }

    /// Orders the list by what needs you, the main checkout first: a
    /// session waiting on you, then failing checks, then a pull request
    /// ready to merge, then the rest, each group in Git's order. Output
    /// alone does not move a row: a prompt or an echo would make the list
    /// jump while you type. Returns whether the order changed.
    pub fn arrange(&mut self, tabs: &[Tab]) -> bool {
        use crate::project::worktree::Checks;
        let canon: Vec<Workspace> = self
            .worktrees
            .iter()
            .chain(self.folders.iter())
            .cloned()
            .collect();
        let owner: Vec<Option<usize>> = tabs.iter().map(|t| task_of(&canon, t)).collect();
        let has = |i: usize, activity: Activity| {
            tabs.iter()
                .zip(&owner)
                .any(|(t, o)| *o == Some(i) && t.activity() == activity)
        };
        let rank = |i: usize| -> u8 {
            let pr = self
                .info
                .get(&canon[i].path)
                .and_then(|info| info.pull_request.as_ref())
                .filter(|pr| pr.state == "OPEN");
            if canon[i].main {
                0
            } else if has(i, Activity::Waiting) {
                1
            } else if pr.is_some_and(|pr| pr.checks == Checks::Failing) {
                2
            } else if pr.is_some_and(|pr| pr.checks == Checks::Passing && !pr.draft) {
                3
            } else {
                4
            }
        };
        let mut order: Vec<usize> = (0..canon.len()).collect();
        order.sort_by_key(|i| rank(*i));
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

    /// A session's row, then its team's tasks: the open ones, and the
    /// finished ones while anything is still open, so a list in progress
    /// reads whole and a finished one folds away.
    fn session_lines(&self, tabs: &[Tab], tab: usize, lines: &mut Vec<Line>) {
        lines.push(Line::Session(tab));
        let Some(team) = self.teams.get(&tabs[tab].review) else {
            return;
        };
        if team.open() == 0 {
            return;
        }
        lines.extend((0..team.tasks.len()).map(|i| Line::TeamTask(tab, i)));
    }

    fn lines(&self, tabs: &[Tab]) -> Vec<Line> {
        let mut lines = Vec::new();
        if let Some(note) = &self.note {
            lines.push(Line::Note(note.clone()));
        }
        let owner: Vec<Option<usize>> = tabs.iter().map(|t| task_of(&self.tasks, t)).collect();
        for task in 0..self.tasks.len() {
            lines.push(Line::Task(task));
            for (tab, _) in owner.iter().enumerate().filter(|(_, o)| **o == Some(task)) {
                self.session_lines(tabs, tab, &mut lines);
            }
        }
        let loose: Vec<usize> = (0..tabs.len()).filter(|i| owner[*i].is_none()).collect();
        if !loose.is_empty() {
            if !self.tasks.is_empty() {
                lines.push(Line::Heading("Other folders"));
            }
            for tab in loose {
                self.session_lines(tabs, tab, &mut lines);
            }
        }
        if tabs.is_empty() && self.tasks.len() <= 1 {
            lines.push(Line::Note(
                "Click below or \u{21e7}\u{2318}N for a new workspace".into(),
            ));
        }
        lines
    }

    fn visible(column: Viewport) -> usize {
        ((column.height - layout::SIDEBAR_HEADER_HEIGHT).max(0.0) / ROW).floor() as usize
    }

    pub fn scroll_by(&mut self, lines: isize, column: Viewport, tabs: &[Tab]) {
        let max = self
            .lines(tabs)
            .len()
            .saturating_sub(Panel::visible(column));
        self.scroll = (self.scroll as isize + lines).clamp(0, max as isize) as usize;
    }

    /// For the self-test: the mode, the tasks by name, the selected one,
    /// and each session's task (`-` for none), the active one starred.
    pub fn report(&self, tabs: &[Tab], active: usize, pair: Option<(usize, usize)>) -> String {
        let name = |i: Option<usize>| i.map_or("-".to_owned(), |i| self.tasks[i].name());
        format!(
            "{} tasks={} selected={} sessions={} claude={} pair={} op={}",
            if self.on { "on" } else { "off" },
            self.tasks
                .iter()
                .map(Workspace::name)
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
        // frame whatever changed in between.
        self.arrange(tabs);
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
        let rows = Panel::visible(column);
        self.scroll = self.scroll.min(lines.len().saturating_sub(rows));
        let selected = self.selected_index();
        let shown = lines.len().saturating_sub(self.scroll).min(rows);
        let blank_top = top + shown as f32 * ROW;
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
        for (n, line) in lines.iter().skip(self.scroll).take(rows).enumerate() {
            let y = top + n as f32 * ROW;
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
                    let task = &self.tasks[*index];
                    let over = layout::hovered(row);
                    if selected == Some(*index) {
                        layout::push_rounded_rect(
                            out,
                            row,
                            layout::UI_RADIUS_SM,
                            theme.sidebar_selected,
                        );
                    } else if over {
                        layout::push_row_hover(out, row, theme);
                    }
                    layout::push_icon_centered(
                        out,
                        atlas,
                        Viewport {
                            x,
                            width: 18.0,
                            ..row
                        },
                        if task.main {
                            icons::HOME
                        } else if task.folder {
                            icons::FOLDER
                        } else {
                            icons::GIT_BRANCH
                        },
                        if selected == Some(*index) {
                            theme.accent
                        } else {
                            theme.status_text
                        },
                    );
                    let mine: Vec<&Tab> = tabs
                        .iter()
                        .filter(|t| task_of(&self.tasks, t) == Some(*index))
                        .collect();
                    let waiting = mine
                        .iter()
                        .filter(|t| t.activity() == Activity::Waiting)
                        .count();
                    let working = mine.iter().any(|t| t.activity() == Activity::Working);
                    // Hover trades the summary for the row's two buttons.
                    let buttons = 2.0 * (layout::UI_CONTROL_SM + 2.0);
                    let name = task.name();
                    layout::push_ui_text(
                        out,
                        atlas,
                        text_rect(24.0, buttons + 4.0),
                        &name,
                        theme.text,
                    );
                    // A folder workspace says which folder, after its name.
                    let folder_name = task
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .filter(|_| task.folder)
                        .filter(|n| *n != name);
                    if let Some(folder_name) = folder_name {
                        let from = 24.0 + layout::ui_text_width(atlas, &name) + 8.0;
                        let rect = text_rect(from, buttons + 4.0);
                        if rect.width > 30.0 {
                            layout::push_ui_text(out, atlas, rect, &folder_name, theme.gutter_text);
                        }
                    }
                    let right = row.x + row.width - 4.0;
                    if over {
                        let menu = Viewport {
                            x: right - layout::UI_CONTROL_SM,
                            y: y + (ROW - layout::UI_CONTROL_SM) * 0.5,
                            width: layout::UI_CONTROL_SM,
                            height: layout::UI_CONTROL_SM,
                        };
                        let add = Viewport {
                            x: menu.x - layout::UI_CONTROL_SM - 2.0,
                            ..menu
                        };
                        layout::Button::new(add)
                            .icon(icons::ADD)
                            .tone(layout::Tone::Ghost)
                            .tip("New Terminal Here")
                            .draw(out, atlas, theme);
                        layout::Button::new(menu)
                            .icon(icons::ELLIPSIS)
                            .tone(layout::Tone::Ghost)
                            .tip("Open, Merge, Push or Remove")
                            .draw(out, atlas, theme);
                        self.hits.push((add, Action::NewSession(*index)));
                        self.hits.push((menu, Action::Menu(*index)));
                    } else if waiting > 0 {
                        let label = if waiting == 1 {
                            "waiting".to_owned()
                        } else {
                            format!("{waiting} waiting")
                        };
                        let w = layout::ui_text_width(atlas, &label) + 4.0;
                        layout::push_ui_text(
                            out,
                            atlas,
                            Viewport {
                                x: right - w,
                                y: y + 4.0,
                                width: w,
                                height: 20.0,
                            },
                            &label,
                            theme.diff_modified,
                        );
                    } else if working {
                        layout::push_spinner(out, right - 22.0, row, theme.accent);
                    } else if let Some((said, colour)) = self
                        .info
                        .get(&task.path)
                        .and_then(|info| summary(info, theme))
                    {
                        let w = layout::ui_text_width(atlas, &said) + 4.0;
                        layout::push_ui_text(
                            out,
                            atlas,
                            Viewport {
                                x: right - w,
                                y: y + 4.0,
                                width: w,
                                height: 20.0,
                            },
                            &said,
                            colour,
                        );
                    } else if *index < 9 {
                        let hint = format!("\u{2303}{}", index + 1);
                        let w = layout::ui_text_width(atlas, &hint) + 4.0;
                        layout::push_ui_text(
                            out,
                            atlas,
                            Viewport {
                                x: right - w,
                                y: y + 4.0,
                                width: w,
                                height: 20.0,
                            },
                            &hint,
                            theme.gutter_text,
                        );
                    }
                    self.hits.push((row, Action::Select(*index)));
                }
                Line::Session(index) => {
                    let tab = &tabs[*index];
                    let indent = if self.tasks.is_empty() { 0.0 } else { INDENT };
                    let row = Viewport {
                        x: row.x + indent,
                        width: (row.width - indent).max(0.0),
                        ..row
                    };
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
                    let indent = if self.tasks.is_empty() {
                        INDENT
                    } else {
                        2.0 * INDENT
                    };
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

/// A task row's note: its pull request and checks, or how many commits it
/// has that the main checkout does not.
fn summary(info: &Info, theme: &Theme) -> Option<(String, [f32; 4])> {
    use crate::project::worktree::Checks;
    if let Some(pr) = &info.pull_request {
        let (word, colour) = match (pr.state.as_str(), pr.checks) {
            ("MERGED", _) => ("merged", theme.accent),
            ("CLOSED", _) => ("closed", theme.gutter_text),
            (_, Checks::Failing) => ("checks failing", theme.diff_removed),
            (_, Checks::Running) => ("checks running", theme.status_text),
            (_, Checks::Passing) => ("checks pass", theme.diff_added),
            (_, Checks::None) => ("open", theme.status_text),
        };
        return Some((format!("#{} {word}", pr.number), colour));
    }
    (info.ahead > 0).then(|| (format!("{} ahead", info.ahead), theme.gutter_text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(path: &str, main: bool) -> Workspace {
        Workspace::of(Worktree {
            path: PathBuf::from(path),
            branch: None,
            main,
        })
    }

    fn folder(name: &str, path: &str) -> Workspace {
        Workspace {
            name: name.into(),
            path: PathBuf::from(path),
            branch: None,
            main: false,
            folder: true,
        }
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
    fn folder_workspaces_survive_their_file_and_keep_names_apart() {
        // No root: nothing is written; the text form is checked directly.
        let mut panel = Panel {
            worktrees: vec![tree("/r", true)],
            ..Panel::default()
        };
        let dir = PathBuf::from("/tmp/crc-agents-test");
        let a = panel.add_folder("notes", dir.clone());
        let b = panel.add_folder("notes", dir);
        assert_eq!(a, "folder:notes");
        assert_eq!(b, "folder:notes 2");
        assert_eq!(panel.tasks.len(), 3);
        assert_eq!(panel.tasks[0].id(), "/r");
        let text = folders_text(&panel.folders);
        let back = parse_folders(&text);
        assert_eq!(back, panel.folders);
        assert_eq!(panel.rename_folder(&b, "notes"), None);
        assert_eq!(
            panel.rename_folder(&b, "ideas"),
            Some("folder:ideas".into())
        );
        panel.remove_folder(&a);
        assert_eq!(
            panel.tasks.iter().map(Workspace::id).collect::<Vec<_>>(),
            vec!["/r".to_owned(), "folder:ideas".to_owned()]
        );
    }
}
