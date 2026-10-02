//! Native local source-control panel; Git processes run on workers.
use crate::project::git::{self, Change, Snapshot};
use crate::project::icons;
use crate::render::{
    font::Atlas,
    layout::{self, Button, Theme, Tone, Viewport},
    metal::GlyphInstance,
};
use crate::text::buffer::Buffer;
use std::{
    path::PathBuf,
    sync::mpsc::{self, Receiver},
};

enum Operation {
    Refresh,
    Select(PathBuf),
    Stage(Change, bool),
    /// Every change of a section, staged or unstaged together.
    StageAll(Vec<Change>, bool),
    StageHunk(Change, git::Hunk),
    Commit(String),
    Switch(String),
    CreateBranch(String),
    /// A branch, and whether to delete it unmerged.
    DeleteBranch(String, bool),
    RenameBranch(String, String),
    /// Give up what Git left half done.
    Abort(git::InProgress),
    ContinueRebase,
    Remote(git::Remote, Option<PathBuf>),
    /// `git add` on a path whose conflict the user has resolved.
    Resolve(Change),
}
struct Reply {
    snapshot: Snapshot,
    selected: usize,
    diff: String,
    committed: bool,
    error: Option<String>,
    /// What a branch or remote command did, for the status line.
    done: Option<String>,
}

/// The commit message's text inset inside its box, for drawing and clicks.
pub const MESSAGE_PAD: f32 = 9.0;

/// An operation on the worker.
struct InFlight {
    rx: Receiver<Result<Reply, String>>,
    /// The message, when it is a commit.
    submitted: Option<String>,
    /// Whether its outcome is said in the status line.
    announces: bool,
}

pub struct Panel {
    directory: PathBuf,
    /// The repository's name in a workspace of several, drawn as a menu
    /// in the header row; `None` when there is nothing to choose.
    pub repo: Option<String>,
    /// Text a commit must not add (see `Workspace::guard`); empty for
    /// no check.
    pub guard: Vec<String>,
    pub snapshot: Option<Snapshot>,
    pub selected: usize,
    pub list_scroll: usize,
    pub diff_scroll: usize,
    pub message: Buffer,
    pub note: String,
    /// The note says something went right (a commit), not wrong.
    note_success: bool,
    /// Sections folded shut, by [`Group::index`].
    pub collapsed: [bool; 3],
    /// Whether the editor column is showing the selected change instead of
    /// the active document.
    pub showing_diff: bool,
    diff: git::Diff,
    /// The operation the worker is running, if any.
    in_flight: Option<InFlight>,
    /// When the worker last answered. The project watcher reports the
    /// repository changes the panel's own operations make; those are
    /// already reflected, so a refresh right after one is skipped.
    finished_at: Option<std::time::Instant>,
    /// A branch or remote command's outcome, success or failure, waiting
    /// for the window to show it in the status line.
    announcement: Option<String>,
    /// Files marked resolved while Git was busy, added in order once it is
    /// free: Mark Resolved saves first, and the save starts a refresh of
    /// its own.
    resolve_queued: std::collections::VecDeque<PathBuf>,
    /// A change clicked while Git was busy, shown once it is done.
    select_queued: Option<PathBuf>,
    /// What was asked for while Git was busy (a stage, a commit, a
    /// refresh the watcher wanted), run once it is done. The latest wins;
    /// a refresh never replaces something the person asked for.
    queued: Option<Operation>,
    /// Counts the worker's answers, so a view derived from the snapshot
    /// knows when to look again.
    generation: u64,
}

/// How a commit the workspace's leak guard refused is said.
const REFUSED: &str = "Not committed: ";

/// Height of one file row in the change list.
pub const ROW: f32 = 26.0;
/// Height of a `Staged Changes` / `Changes` section heading.
pub const SECTION: f32 = 26.0;

/// The headings of the change list, in order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Group {
    /// Unmerged paths, which are neither staged nor unstaged until the
    /// conflict in them is resolved and added.
    Conflicts,
    Staged,
    Changes,
}

impl Group {
    pub fn title(self) -> &'static str {
        match self {
            Group::Conflicts => "Conflicts",
            Group::Staged => "Staged",
            Group::Changes => "Changes",
        }
    }
    pub fn index(self) -> usize {
        match self {
            Group::Conflicts => 0,
            Group::Staged => 1,
            Group::Changes => 2,
        }
    }
    fn holds(self, change: &Change) -> bool {
        match self {
            Group::Conflicts => change.conflicted(),
            Group::Staged => change.staged(),
            Group::Changes => change.unstaged(),
        }
    }
}

/// One line of the change list. A file modified in the index *and* in the
/// working tree is a real state, and it appears under both headings, so a row
/// is a change together with the group it is listed under.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Entry {
    Section { group: Group, count: usize },
    File { change: usize, group: Group },
}

impl Entry {
    pub fn height(self) -> f32 {
        match self {
            Entry::Section { .. } => SECTION,
            Entry::File { .. } => ROW,
        }
    }
}

/// Where source control draws inside the sidebar column.
///
/// This used to be a centred modal clamped to 1180x760 whatever it contained,
/// with the staging buttons pinned to the panel's bottom edge: three changed
/// files left roughly 700pt of empty column between the list and the buttons
/// that acted on it, and the editor was unusable while it was open. The
/// sidebar is as wide as it is and the content simply flows down it.
///
/// The header row is the Explorer's: the panel's name (or, in a workspace,
/// the repository menu) with Refresh and the Git menu at its right. Below
/// it, one control-height row each: the branch with Pull and Push beside
/// it, the message, Commit. Then the list, and a status line at the bottom.
#[derive(Clone, Copy)]
pub struct Sidebar {
    /// The repository menu, in the title row, in a workspace of several.
    pub repo: Viewport,
    pub refresh: Viewport,
    /// Opens the Git menu: fetch, pull, push and the branch commands.
    pub more: Viewport,
    pub branch: Viewport,
    pub pull: Viewport,
    pub push: Viewport,
    pub message: Viewport,
    pub commit: Viewport,
    pub list: Viewport,
    pub note: Viewport,
}

/// Width of the Pull and Push buttons: an arrow and a count.
const SYNC_WIDTH: f32 = 44.0;

impl Sidebar {
    pub fn new(column: Viewport) -> Self {
        let x = column.x + layout::UI_INSET;
        let width = (column.width - layout::UI_INSET * 2.0).max(0.0);
        let [refresh, more] = layout::sidebar_header_buttons::<2>(column);
        let title = layout::sidebar_title_row(column);
        let repo = Viewport {
            x: title.x - 6.0,
            width: (refresh.x - title.x + 6.0 - layout::UI_GAP).max(0.0),
            ..title
        };
        let top = column.y + layout::SIDEBAR_HEADER_HEIGHT + 2.0;
        let control = layout::UI_CONTROL;
        let push = Viewport {
            x: x + width - SYNC_WIDTH,
            y: top,
            width: SYNC_WIDTH.min(width),
            height: control,
        };
        let pull = Viewport {
            x: push.x - 4.0 - SYNC_WIDTH,
            ..push
        };
        let branch = Viewport {
            x,
            y: top,
            width: (pull.x - layout::UI_GAP - x).max(0.0),
            height: control,
        };
        let message = Viewport {
            x,
            y: branch.y + control + 8.0,
            width,
            height: control,
        };
        let commit = Viewport {
            x,
            y: message.y + control + layout::UI_GAP,
            width,
            height: control,
        };
        let note_height = 24.0;
        let note = Viewport {
            x,
            y: column.y + column.height - note_height - 6.0,
            width,
            height: note_height,
        };
        let list_top = commit.y + control + 12.0;
        Self {
            repo,
            refresh,
            more,
            branch,
            pull,
            push,
            message,
            commit,
            list: Viewport {
                x: column.x,
                y: list_top,
                width: column.width,
                height: (note.y - list_top - 4.0).max(0.0),
            },
            note,
        }
    }

    /// The staging control at the trailing edge of a file row, shown while
    /// the pointer is on the row.
    pub fn toggle(&self, row: Viewport) -> Viewport {
        let size = layout::UI_CONTROL_SM;
        Viewport {
            x: row.x + row.width - layout::UI_INSET - 16.0 - 4.0 - size,
            y: row.y + ((row.height - size) * 0.5).floor(),
            width: size,
            height: size,
        }
    }

    /// Open File, left of the staging control.
    pub fn open(&self, row: Viewport) -> Viewport {
        let toggle = self.toggle(row);
        Viewport {
            x: toggle.x - 2.0 - toggle.width,
            ..toggle
        }
    }

    /// The section heading's Stage All or Unstage All.
    pub fn section_action(&self, row: Viewport) -> Viewport {
        let size = layout::UI_CONTROL_SM;
        Viewport {
            x: row.x + row.width - layout::UI_INSET + 4.0 - size,
            y: row.y + ((row.height - size) * 0.5).floor(),
            width: size,
            height: size,
        }
    }

    /// The status letter at a row's trailing edge.
    fn letter(&self, row: Viewport) -> Viewport {
        Viewport {
            x: row.x + row.width - layout::UI_INSET - 16.0,
            width: 16.0,
            ..row
        }
    }
}

impl Panel {
    pub fn new(directory: PathBuf) -> Self {
        let mut panel = Self::idle(directory, None);
        panel.refresh();
        panel
    }

    /// A panel that has asked the repository nothing yet: no worker runs.
    fn idle(directory: PathBuf, snapshot: Option<Snapshot>) -> Self {
        Self {
            directory,
            repo: None,
            guard: Vec::new(),
            snapshot,
            selected: 0,
            list_scroll: 0,
            diff_scroll: 0,
            message: Buffer::new(),
            note: "Reading repository…".into(),
            note_success: false,
            collapsed: [false; 3],
            showing_diff: false,
            diff: git::Diff::default(),
            in_flight: None,
            finished_at: None,
            announcement: None,
            resolve_queued: Default::default(),
            select_queued: None,
            queued: None,
            generation: 0,
        }
    }
    /// How many times the worker has answered.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn busy(&self) -> bool {
        self.in_flight.is_some()
    }
    pub fn branch(&self) -> String {
        self.snapshot
            .as_ref()
            .map(|s| branch_name(&s.branch))
            .unwrap_or_else(|| "Git".into())
    }
    /// The branch and how far it is from its upstream: `main ↑2 ↓1`.
    pub fn branch_status(&self) -> String {
        self.snapshot
            .as_ref()
            .map_or_else(String::new, snapshot_status)
    }
    /// Whether `path`, absolute, is unmerged as of the last status.
    pub fn is_conflicted(&self, path: &std::path::Path) -> bool {
        self.conflicted_change(path).is_some()
    }
    fn conflicted_change(&self, path: &std::path::Path) -> Option<&Change> {
        let snapshot = self.snapshot.as_ref()?;
        // Asked for every open document when Git changes: nothing to find,
        // and no path to resolve, in the usual case of no conflicts.
        if !snapshot.changes.iter().any(Change::conflicted) {
            return None;
        }
        // Open documents' paths and Git's top level are already resolved;
        // the disk is asked only when they do not line up.
        let relative = match path.strip_prefix(&snapshot.root) {
            Ok(relative) => relative.to_path_buf(),
            Err(_) => {
                let path = crate::platform::canonical(path);
                let root = crate::platform::canonical(&snapshot.root);
                path.strip_prefix(&root).ok()?.to_path_buf()
            }
        };
        snapshot
            .changes
            .iter()
            .find(|c| c.conflicted() && c.path == relative)
    }
    /// How many paths are unmerged.
    pub fn conflict_count(&self) -> usize {
        self.snapshot
            .as_ref()
            .map_or(0, |s| s.changes.iter().filter(|c| c.conflicted()).count())
    }
    /// `git add` on `path`, absolute, once its conflict is resolved: now,
    /// or as soon as the operation in flight finishes.
    pub fn mark_resolved(&mut self, path: PathBuf) {
        if self.busy() {
            if !self.resolve_queued.contains(&path) {
                self.resolve_queued.push_back(path);
            }
            return;
        }
        match self.conflicted_change(&path).cloned() {
            Some(change) => self.start(Operation::Resolve(change)),
            None => {
                self.announcement = Some(format!(
                    "{} is not in conflict",
                    path.file_name().map_or_else(
                        || path.display().to_string(),
                        |n| n.to_string_lossy().into_owned()
                    )
                ))
            }
        }
    }
    /// The repository's top level, once read.
    pub fn root(&self) -> Option<&std::path::Path> {
        self.snapshot.as_ref().map(|s| s.root.as_path())
    }
    pub fn switch_branch(&mut self, name: String) {
        self.start(Operation::Switch(name));
    }
    pub fn create_branch(&mut self, name: String) {
        self.start(Operation::CreateBranch(name));
    }
    pub fn delete_branch(&mut self, name: String, force: bool) {
        self.start(Operation::DeleteBranch(name, force));
    }
    pub fn rename_branch(&mut self, old: String, new: String) {
        self.start(Operation::RenameBranch(old, new));
    }
    pub fn abort(&mut self, what: git::InProgress) {
        self.start(Operation::Abort(what));
    }
    pub fn continue_rebase(&mut self) {
        self.start(Operation::ContinueRebase);
    }
    /// The merge, rebase, cherry-pick or revert Git has under way.
    pub fn in_progress(&self) -> Option<git::InProgress> {
        self.snapshot.as_ref().and_then(|s| s.in_progress)
    }
    pub fn remote(&mut self, what: git::Remote, ssh_auth_sock: Option<PathBuf>) {
        if self.busy() {
            self.announcement = Some("Git is busy; try again in a moment".into());
            return;
        }
        self.start(Operation::Remote(what, ssh_auth_sock));
        self.note = format!("{}…", what.verb());
    }
    /// The last branch or remote outcome, once.
    pub fn take_announcement(&mut self) -> Option<String> {
        self.announcement.take()
    }
    pub fn selected_change(&self) -> Option<&Change> {
        self.snapshot.as_ref()?.changes.get(self.selected)
    }
    pub fn can_stage(&self) -> bool {
        self.selected_change().is_some_and(Change::unstaged)
    }
    pub fn can_unstage(&self) -> bool {
        self.selected_change().is_some_and(Change::staged)
    }
    /// The change list as it is drawn: a heading per non-empty section, then
    /// its files. Built once per frame and reused for hit testing, so what is
    /// clicked is always what was drawn.
    pub fn entries(&self) -> Vec<Entry> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        let mut entries = Vec::new();
        for group in [Group::Conflicts, Group::Staged, Group::Changes] {
            let files: Vec<usize> = snapshot
                .changes
                .iter()
                .enumerate()
                .filter(|(_, c)| group.holds(c))
                .map(|(i, _)| i)
                .collect();
            if files.is_empty() {
                continue;
            }
            entries.push(Entry::Section {
                group,
                count: files.len(),
            });
            if self.collapsed[group.index()] {
                continue;
            }
            entries.extend(
                files
                    .into_iter()
                    .map(|change| Entry::File { change, group }),
            );
        }
        entries
    }

    /// The rectangle each visible entry occupies, from the top of the list.
    pub fn rows(&self, g: Sidebar) -> Vec<(Entry, Viewport)> {
        let mut y = g.list.y;
        let mut rows = Vec::new();
        for entry in self.entries().into_iter().skip(self.list_scroll) {
            let height = entry.height();
            if y + height > g.list.y + g.list.height {
                break;
            }
            rows.push((
                entry,
                Viewport {
                    x: g.list.x,
                    y,
                    width: g.list.width,
                    height,
                },
            ));
            y += height;
        }
        rows
    }

    pub fn entry_at(&self, g: Sidebar, x: f32, y: f32) -> Option<(Entry, Viewport)> {
        self.rows(g)
            .into_iter()
            .find(|(_, rect)| rect.contains(x, y))
    }

    /// Stage or unstage one change by index, rather than whatever happens to
    /// be selected. The buttons live on the row they act on now.
    /// Asked for while Git is busy, these wait their turn (see `queued`).
    pub fn stage_index(&mut self, index: usize, staged: bool) {
        let Some(change) = self
            .snapshot
            .as_ref()
            .and_then(|s| s.changes.get(index))
            .cloned()
        else {
            return;
        };
        if (staged && !change.unstaged()) || (!staged && !change.staged()) {
            return;
        }
        self.start(Operation::Stage(change, staged));
    }

    /// Folds a section shut, or opens it.
    pub fn toggle_group(&mut self, group: Group) {
        let i = group.index();
        self.collapsed[i] = !self.collapsed[i];
        self.list_scroll = self.list_scroll.min(self.entries().len().saturating_sub(1));
    }

    /// Stages every change under Changes, or unstages every one under
    /// Staged.
    pub fn stage_group(&mut self, group: Group) {
        let stage = match group {
            Group::Changes => true,
            Group::Staged => false,
            Group::Conflicts => return,
        };
        let changes: Vec<Change> = self
            .snapshot
            .as_ref()
            .map(|s| {
                s.changes
                    .iter()
                    .filter(|c| group.holds(c))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if !changes.is_empty() {
            self.start(Operation::StageAll(changes, stage));
        }
    }

    /// How far the branch is ahead of and behind its upstream, if it has
    /// one.
    pub fn ahead_behind(&self) -> Option<(usize, usize)> {
        let header = &self.snapshot.as_ref()?.branch;
        if !header.contains("...") {
            return None;
        }
        let mut counts = (0, 0);
        if let Some(open) = header.find('[') {
            let inside = &header[open + 1..header.rfind(']').unwrap_or(header.len())];
            for part in inside.split(", ") {
                if let Some(n) = part.strip_prefix("ahead ") {
                    counts.0 = n.trim().parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix("behind ") {
                    counts.1 = n.trim().parse().unwrap_or(0);
                }
            }
        }
        Some(counts)
    }

    /// Why Commit cannot run, for its tooltip; `None` when it can.
    pub fn commit_blocker(&self) -> Option<&'static str> {
        if self.busy() {
            Some("Git is busy")
        } else if self.staged_count() == 0 {
            Some("Stage a change to commit it")
        } else if self.message.rope.to_string().trim().is_empty() {
            Some("Write a commit message first")
        } else {
            None
        }
    }

    pub fn staged_count(&self) -> usize {
        self.snapshot
            .as_ref()
            .map_or(0, |s| s.changes.iter().filter(|c| c.staged()).count())
    }

    pub fn hunk_counts(&self) -> (usize, usize) {
        let staged = self.diff.hunks.iter().filter(|h| h.staged).count();
        (staged, self.diff.hunks.len() - staged)
    }

    pub fn can_commit(&self) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|s| s.changes.iter().any(Change::staged))
            && !self.message.rope.to_string().trim().is_empty()
    }
    pub fn refresh(&mut self) {
        self.start(Operation::Refresh);
    }
    pub fn select(&mut self, index: usize) {
        if let Some(change) = self.snapshot.as_ref().and_then(|s| s.changes.get(index)) {
            let path = change.path.clone();
            if self.busy() {
                self.select_queued = Some(path);
            } else {
                self.start(Operation::Select(path));
            }
        }
    }
    pub fn stage(&mut self, staged: bool) {
        if (staged && !self.can_stage()) || (!staged && !self.can_unstage()) {
            return;
        }
        if let Some(change) = self.selected_change().cloned() {
            self.start(Operation::Stage(change, staged));
        }
    }
    pub fn stage_hunk(&mut self, index: usize) {
        let Some(change) = self.selected_change().cloned() else {
            return;
        };
        let Some(hunk) = self.diff.hunks.get(index).cloned() else {
            return;
        };
        if !self.diff.truncated && git::can_stage_hunk(&change, hunk.staged) {
            self.start(Operation::StageHunk(change, hunk));
        }
    }

    fn can_act_on_hunk(&self, index: usize) -> bool {
        !self.busy()
            && !self.diff.truncated
            && self.selected_change().is_some_and(|change| {
                self.diff
                    .hunks
                    .get(index)
                    .is_some_and(|hunk| git::can_stage_hunk(change, hunk.staged))
            })
    }

    pub fn hunk_action_rect(row: Viewport) -> Viewport {
        Viewport {
            x: row.x + row.width - 80.0,
            y: row.y + 2.0,
            width: 72.0,
            height: row.height - 4.0,
        }
    }

    pub fn hunk_action_rects(&self, diff: Viewport) -> Vec<Viewport> {
        let visible = (diff.height / DIFF_LINE).max(0.0) as usize;
        self.diff
            .lines
            .iter()
            .skip(self.diff_scroll)
            .take(visible)
            .enumerate()
            .filter_map(|(row, line)| {
                let index = line.hunk?;
                self.can_act_on_hunk(index).then(|| {
                    Self::hunk_action_rect(Viewport {
                        x: diff.x,
                        y: diff.y + row as f32 * DIFF_LINE,
                        width: diff.width,
                        height: DIFF_LINE,
                    })
                })
            })
            .collect()
    }

    pub fn hunk_action_at(&self, diff: Viewport, x: f32, y: f32) -> Option<usize> {
        if !diff.contains(x, y) {
            return None;
        }
        let row = ((y - diff.y) / DIFF_LINE) as usize;
        let line = self.diff.lines.get(self.diff_scroll + row)?;
        let index = line.hunk?;
        let row_rect = Viewport {
            x: diff.x,
            y: diff.y + row as f32 * DIFF_LINE,
            width: diff.width,
            height: DIFF_LINE,
        };
        (self.can_act_on_hunk(index) && Self::hunk_action_rect(row_rect).contains(x, y))
            .then_some(index)
    }
    pub fn commit(&mut self) {
        let message = self.message.rope.to_string();
        // A second click while the first commit runs is the same commit.
        if self
            .in_flight
            .as_ref()
            .is_some_and(|f| f.submitted.as_deref() == Some(message.as_str()))
        {
            return;
        }
        if self.can_commit() {
            self.start(Operation::Commit(message));
        }
    }
    fn start(&mut self, operation: Operation) {
        if self.busy() {
            if !matches!(operation, Operation::Refresh) || self.queued.is_none() {
                self.queued = Some(operation);
            }
            return;
        }
        let submitted = if let Operation::Commit(message) = &operation {
            Some(message.clone())
        } else {
            None
        };
        let announces = matches!(
            operation,
            Operation::Switch(_)
                | Operation::CreateBranch(_)
                | Operation::DeleteBranch(..)
                | Operation::RenameBranch(..)
                | Operation::Abort(_)
                | Operation::ContinueRebase
                | Operation::Remote(..)
                | Operation::Resolve(_)
                | Operation::StageAll(..)
                | Operation::Commit(_)
        );
        let directory = self.directory.clone();
        let guard = self.guard.clone();
        let previous = self.selected_change().map(|c| c.path.clone());
        let (tx, rx) = mpsc::channel();
        self.in_flight = Some(InFlight {
            rx,
            submitted,
            announces,
        });
        self.note = "Working…".into();
        self.note_success = false;
        std::thread::spawn(move || {
            let result = (|| {
                // The root alone: a whole status here was thrown away.
                let root = git::toplevel(&directory)?;
                let mut selected_path = previous;
                let mut committed = false;
                let mut done = None;
                let error = match operation {
                    Operation::Switch(name) => match git::switch(&root, &name) {
                        Ok(()) => {
                            done = Some(format!("switched to {name}"));
                            None
                        }
                        Err(error) => Some(error),
                    },
                    Operation::CreateBranch(name) => match git::create_branch(&root, &name) {
                        Ok(()) => {
                            done = Some(format!("created and switched to {name}"));
                            None
                        }
                        Err(error) => Some(error),
                    },
                    Operation::DeleteBranch(name, force) => {
                        match git::delete_branch(&root, &name, force) {
                            Ok(()) => {
                                done = Some(format!("deleted {name}"));
                                None
                            }
                            Err(error) => Some(error),
                        }
                    }
                    Operation::Abort(what) => match git::abort(&root, what) {
                        Ok(()) => {
                            done = Some(format!("{} aborted", what.command()));
                            None
                        }
                        Err(error) => Some(error),
                    },
                    Operation::ContinueRebase => match git::continue_rebase(&root) {
                        Ok(()) => {
                            done = Some("rebase continued".into());
                            None
                        }
                        Err(error) => Some(error),
                    },
                    Operation::RenameBranch(old, new) => {
                        match git::rename_branch(&root, &old, &new) {
                            Ok(()) => {
                                done = Some(format!("renamed {old} to {new}"));
                                None
                            }
                            Err(error) => Some(error),
                        }
                    }
                    Operation::Remote(what, sock) => {
                        match git::remote(&root, what, sock.as_deref()) {
                            Ok(said) => {
                                let verb = match what {
                                    git::Remote::Fetch => "fetched",
                                    git::Remote::Pull
                                    | git::Remote::PullRebase
                                    | git::Remote::PullMerge => "pulled",
                                    git::Remote::Push => "pushed",
                                };
                                let last = said.lines().last().unwrap_or("").trim().to_owned();
                                done = Some(if last.is_empty() {
                                    verb.to_string()
                                } else {
                                    format!("{verb}: {last}")
                                });
                                None
                            }
                            Err(error) => Some(error),
                        }
                    }
                    Operation::Refresh => None,
                    Operation::Select(path) => {
                        selected_path = Some(path);
                        None
                    }
                    Operation::Stage(change, stage) => git::stage(&root, &change, stage).err(),
                    Operation::StageAll(changes, stage) => {
                        let mut error = None;
                        for change in &changes {
                            if let Err(e) = git::stage(&root, change, stage) {
                                error = Some(e);
                                break;
                            }
                        }
                        if error.is_none() {
                            let n = changes.len();
                            let files = if n == 1 { "file" } else { "files" };
                            let verb = if stage { "staged" } else { "unstaged" };
                            done = Some(format!("{verb} {n} {files}"));
                        }
                        error
                    }
                    Operation::Resolve(change) => match git::stage(&root, &change, true) {
                        Ok(()) => {
                            done = Some(format!("marked {} resolved", change.label()));
                            None
                        }
                        Err(error) => Some(error),
                    },
                    Operation::StageHunk(change, hunk) => {
                        git::stage_hunk(&root, &change, &hunk).err()
                    }
                    Operation::Commit(message) => {
                        // The workspace's private markers, checked
                        // against exactly what would be committed.
                        let leaked = if guard.is_empty() {
                            None
                        } else {
                            crate::project::workspace::leak(&git::staged_additions(&root)?, &guard)
                        };
                        match leaked {
                            Some(hit) => Some(format!(
                                "{REFUSED}{hit}, which this workspace keeps out of its repositories. \
                                 Remove it from the staged change, or change private_markers in .crc/workspace.toml."
                            )),
                            None => match git::commit(&root, &message) {
                                Ok(()) => {
                                    committed = true;
                                    let subject = message.lines().next().unwrap_or("").trim();
                                    done = Some(format!("committed \u{201c}{subject}\u{201d}"));
                                    None
                                }
                                Err(error) => Some(error),
                            },
                        }
                    }
                };
                let snapshot = git::snapshot(&root)?;
                let selected = selected_path
                    .and_then(|p| snapshot.changes.iter().position(|c| c.path == p))
                    .unwrap_or(0);
                let diff = snapshot
                    .changes
                    .get(selected)
                    .map(|c| git::diff(&snapshot.root, c))
                    .transpose()?
                    .unwrap_or_else(|| "Working tree clean".into());
                Ok(Reply {
                    snapshot,
                    selected,
                    diff,
                    committed,
                    error,
                    done,
                })
            })();
            let _ = tx.send(result);
        });
    }
    pub fn poll(&mut self) -> bool {
        let Some(flight) = &self.in_flight else {
            return false;
        };
        let reply = match flight.rx.try_recv() {
            Ok(reply) => reply,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => Err("Git worker stopped".into()),
        };
        let Some(flight) = self.in_flight.take() else {
            return false;
        };
        self.generation += 1;
        self.finished_at = Some(std::time::Instant::now());
        match reply {
            Ok(reply) => {
                // Branch and remote commands announce, failures included:
                // they run from the menu, with the panel usually closed.
                if flight.announces {
                    self.announcement = reply.done.clone().or_else(|| {
                        reply.error.as_ref().map(|e| {
                            // Git's own reason, not its closing "Aborting".
                            e.lines()
                                .find(|l| l.starts_with("error:") || l.starts_with("fatal:"))
                                .or_else(|| e.lines().last())
                                .unwrap_or(e)
                                .to_owned()
                        })
                    });
                }
                self.snapshot = Some(reply.snapshot);
                self.selected = reply.selected;
                self.diff = git::Diff::parse(&reply.diff);
                self.diff_scroll = 0;
                // The list may be shorter now (a commit took most of it):
                // scrolled past its end, it drew as "No changes".
                self.list_scroll = self.list_scroll.min(self.entries().len().saturating_sub(1));
                if reply.committed
                    && flight.submitted.as_deref() == Some(self.message.rope.to_string().as_str())
                {
                    self.message = Buffer::new();
                }
                // A failure is shown where the diff would be, as a note rather
                // than as diff lines, so git's stderr is never mistaken for
                // part of the change.
                if let Some(error) = &reply.error {
                    // The workspace's own refusal is not a Git failure, and
                    // the sidebar's note is too narrow to read it whole: the
                    // status line says which line and marker.
                    let heading = match error.strip_prefix(REFUSED) {
                        Some(rest) => {
                            let hit = rest.split(", which").next().unwrap_or(rest);
                            self.announcement = Some(format!("{REFUSED}{hit}"));
                            "Commit refused"
                        }
                        None => "Git command failed",
                    };
                    let mut failure = git::Diff::default();
                    for line in format!("{heading}\n\n{error}").lines() {
                        failure.lines.push(git::DiffLine {
                            kind: git::DiffKind::Note,
                            old: None,
                            new: None,
                            text: line.to_owned(),
                            hunk: None,
                        });
                    }
                    self.diff = failure;
                }
                self.note_success = reply.error.is_none() && reply.committed;
                self.note = reply.error.unwrap_or_else(|| {
                    if reply.committed {
                        "Commit created".into()
                    } else {
                        String::new()
                    }
                });
            }
            Err(error) => {
                // A menu-driven Fetch or Switch that failed before its
                // command ran is said in the status line too, where it can
                // be seen with the panel closed.
                if flight.announces {
                    self.announcement = Some(error.clone());
                }
                self.note = error;
            }
        }
        if let Some(path) = self.resolve_queued.pop_front() {
            self.mark_resolved(path);
        } else if let Some(operation) = self.queued.take() {
            self.start(operation);
        } else if let Some(path) = self.select_queued.take() {
            self.start(Operation::Select(path));
        }
        true
    }

    /// Wheel scrolling over the change list or over the diff.
    pub fn scroll(&mut self, delta: isize, over_list: bool, g: Sidebar, diff: Viewport) {
        if over_list {
            let visible = self.rows(g).len();
            self.list_scroll = layout::scroll_clamped(
                self.list_scroll,
                delta,
                self.entries().len(),
                visible.max(1),
            );
        } else {
            let visible = (diff.height / DIFF_LINE).max(1.0) as usize;
            self.diff_scroll =
                layout::scroll_clamped(self.diff_scroll, delta, self.diff.lines.len(), visible);
        }
    }

    /// Source control down the sidebar column: the header's actions, the
    /// branch with Pull and Push, the commit box, then the changes grouped
    /// by whether they are staged, each row carrying the controls that act
    /// on it. `focused` is whether the message field has the keyboard.
    pub fn draw_sidebar(
        &self,
        atlas: &mut Atlas,
        column: Viewport,
        focused: bool,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) {
        let g = Sidebar::new(column);
        let busy = self.busy();

        // The title row: the panel's name, or in a workspace of several
        // the repository menu, then Refresh and the Git menu.
        match &self.repo {
            Some(repo) => Button::new(g.repo)
                .label(repo)
                .icon(icons::FOLDER_OUTLINE)
                .tone(Tone::Ghost)
                .menu()
                .tip("Switch Repository")
                .draw(out, atlas, theme),
            None => {
                layout::push_sidebar_title(out, atlas, column, "SOURCE CONTROL", 2, theme);
            }
        }
        Button::new(g.refresh)
            .icon(icons::REFRESH)
            .tone(Tone::Ghost)
            .enabled(!busy)
            .tip("Refresh")
            .draw(out, atlas, theme);
        Button::new(g.more)
            .icon(icons::ELLIPSIS)
            .tone(Tone::Ghost)
            .tip("More Git Actions")
            .draw(out, atlas, theme);
        if busy {
            // Work under way, where the eye already is.
            layout::push_progress(
                out,
                atlas,
                Viewport {
                    x: column.x,
                    y: column.y + layout::SIDEBAR_HEADER_HEIGHT - 2.0,
                    width: column.width - 1.0,
                    height: 2.0,
                },
                theme,
            );
        }

        let Some(snapshot) = &self.snapshot else {
            self.draw_no_repository(atlas, g, theme, out);
            return;
        };
        let _ = snapshot;

        // The branch reads as a menu: a box with a chevron, the branch list
        // behind it. Pull and Push beside it say how far apart the branch
        // and its upstream are.
        let branch = self.branch();
        let detached = branch == "detached HEAD";
        let tip_branch = if detached {
            "Switch Branch"
        } else {
            "Switch, Create or Delete Branches"
        };
        let branch_label = match self.in_progress() {
            Some(what) => format!("{branch} \u{b7} {}", what.label()),
            None => branch,
        };
        Button::new(g.branch)
            .label(&branch_label)
            .icon(crate::render::layout::ACTIVITY_ICONS[1].0)
            .menu()
            .tip(tip_branch)
            .draw(out, atlas, theme);
        let sync = self.ahead_behind();
        let (ahead, behind) = sync.unwrap_or((0, 0));
        let count = |n: usize| if n > 0 { n.to_string() } else { String::new() };
        let behind_label = count(behind);
        let ahead_label = count(ahead);
        let pull_tip = match sync {
            None => "Pull (no upstream)".to_owned(),
            Some((_, 0)) => "Pull: up to date with upstream".to_owned(),
            Some((_, n)) => format!(
                "Pull {n} commit{} from upstream",
                if n == 1 { "" } else { "s" }
            ),
        };
        let push_tip = match sync {
            None => "Push and set upstream".to_owned(),
            Some((0, _)) => "Push: nothing to push".to_owned(),
            Some((n, _)) => format!(
                "Push {n} commit{} to upstream",
                if n == 1 { "" } else { "s" }
            ),
        };
        Button::new(g.pull)
            .icon(icons::ARROW_DOWN)
            .label(&behind_label)
            .on(behind > 0)
            .enabled(!busy)
            .tip(&pull_tip)
            .draw(out, atlas, theme);
        Button::new(g.push)
            .icon(icons::ARROW_UP)
            .label(&ahead_label)
            .on(ahead > 0)
            .enabled(!busy)
            .tip(&push_tip)
            .draw(out, atlas, theme);

        // The message: a field, ringed while it has the keyboard.
        if focused {
            layout::push_focus_ring(out, g.message, layout::UI_RADIUS, 1.5, theme);
        }
        layout::push_rounded_rect(
            out,
            g.message,
            layout::UI_RADIUS,
            if focused || layout::hovered(g.message) {
                theme.background_f32()
            } else {
                theme.tab_active
            },
        );
        layout::hotspot(g.message, layout::Cursor::Text, None);
        let full = self.message.rope.to_string();
        let text_rect = Viewport {
            x: g.message.x + MESSAGE_PAD,
            width: (g.message.width - MESSAGE_PAD * 2.0).max(0.0),
            ..g.message
        };
        let band = ((g.message.height - 18.0) * 0.5).floor();
        layout::push_ui_field(
            out,
            atlas,
            text_rect,
            (band, 18.0),
            &layout::UiField {
                text: &full,
                cursor: self.message.cursor(),
                selection: self.message.selection(),
                placeholder: if focused {
                    "Message"
                } else {
                    "Message (\u{2318}\u{21a9} to commit)"
                },
                focused,
            },
            theme,
        );

        // The button says what it will commit. "Commit" alone next to a list
        // that mixes staged and unstaged files does not.
        let staged = self.staged_count();
        let commit_label = match staged {
            0 => "Commit".to_owned(),
            1 => "Commit 1 File".to_owned(),
            n => format!("Commit {n} Files"),
        };
        let blocker = self.commit_blocker();
        let mut commit = Button::new(g.commit)
            .label(&commit_label)
            .icon(icons::CHECK)
            .tone(Tone::Primary)
            .enabled(blocker.is_none());
        commit = match blocker {
            Some(why) => commit.tip(why),
            None => commit.hint("\u{2318}\u{21a9}"),
        };
        commit.draw(out, atlas, theme);

        let rows = self.rows(g);
        if rows.is_empty() && !busy {
            self.draw_clean(atlas, g, theme, out);
        }
        for (entry, rect) in rows {
            match entry {
                Entry::Section { group, count } => {
                    let expanded = !self.collapsed[group.index()];
                    layout::push_row_hover(out, rect, theme);
                    layout::push_section_heading(
                        out,
                        atlas,
                        rect,
                        group.title(),
                        Some(count),
                        expanded,
                        if group == Group::Conflicts {
                            theme.diff_removed
                        } else {
                            theme.status_text
                        },
                        theme,
                    );
                    if layout::hovered(rect) && group != Group::Conflicts {
                        let (icon, tip) = match group {
                            Group::Staged => (icons::REMOVE, "Unstage All"),
                            _ => (icons::ADD, "Stage All"),
                        };
                        Button::new(g.section_action(rect))
                            .icon(icon)
                            .tone(Tone::Ghost)
                            .enabled(!busy)
                            .tip(tip)
                            .draw(out, atlas, theme);
                    }
                }
                Entry::File { change, group } => {
                    self.draw_file_row(atlas, g, rect, change, group, theme, out);
                }
            }
        }
        self.draw_note(atlas, g, theme, out);
    }

    /// Below the list: what Git is doing, or what went wrong or right.
    fn draw_note(
        &self,
        atlas: &mut Atlas,
        g: Sidebar,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) {
        if self.note.is_empty() {
            return;
        }
        let rect = g.note;
        let (lead, colour) = if self.busy() {
            let w = layout::push_spinner(out, rect.x + 2.0, rect, theme.accent);
            (w + 8.0, theme.status_text)
        } else {
            let (icon, colour) = if self.note_success {
                (icons::PASS, theme.diff_added)
            } else {
                (icons::ERROR, theme.diff_removed)
            };
            let w = layout::icon_width(atlas, icon);
            layout::push_icon_centered(out, atlas, Viewport { width: w, ..rect }, icon, colour);
            (
                w + 6.0,
                if self.note_success {
                    theme.status_text
                } else {
                    colour
                },
            )
        };
        // Git's stderr can run to many lines; the first one names it, and
        // the whole of it is in the editor column.
        let first = self
            .note
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        layout::push_ui_text(
            out,
            atlas,
            Viewport {
                x: rect.x + lead,
                width: (rect.width - lead).max(0.0),
                ..rect
            },
            first,
            colour,
        );
        layout::hotspot(rect, layout::Cursor::Arrow, Some(&self.note));
    }

    /// The list when there is nothing in it: the working tree is clean.
    fn draw_clean(
        &self,
        atlas: &mut Atlas,
        g: Sidebar,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) {
        let x = g.list.x + layout::UI_INSET;
        let width = (g.list.width - layout::UI_INSET * 2.0).max(0.0);
        let line = |y: f32| Viewport {
            x,
            y,
            width,
            height: 22.0,
        };
        let icon = layout::icon_width(atlas, icons::PASS);
        layout::push_icon_centered(
            out,
            atlas,
            Viewport {
                width: icon,
                ..line(g.list.y + 4.0)
            },
            icons::PASS,
            theme.diff_added,
        );
        layout::push_ui_text(
            out,
            atlas,
            Viewport {
                x: x + icon + 6.0,
                width: (width - icon - 6.0).max(0.0),
                ..line(g.list.y + 4.0)
            },
            "No changes",
            theme.text,
        );
        layout::push_ui_text(
            out,
            atlas,
            line(g.list.y + 28.0),
            "The working tree matches the last commit.",
            theme.gutter_text,
        );
    }

    /// The column when there is no repository to show: reading one, or
    /// none here, said in words rather than as Git's error.
    fn draw_no_repository(
        &self,
        atlas: &mut Atlas,
        g: Sidebar,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) {
        let x = g.branch.x;
        let width = g.message.width;
        let mut y = g.branch.y + 4.0;
        if self.busy() {
            let row = Viewport {
                x,
                y,
                width,
                height: 22.0,
            };
            let w = layout::push_spinner(out, x + 2.0, row, theme.accent);
            layout::push_ui_text(
                out,
                atlas,
                Viewport {
                    x: x + w + 10.0,
                    width: (width - w - 10.0).max(0.0),
                    ..row
                },
                "Reading repository\u{2026}",
                theme.status_text,
            );
            return;
        }
        let not_a_repo = self
            .note
            .to_ascii_lowercase()
            .contains("not a git repository");
        let (title, body) = if not_a_repo {
            (
                "No Git repository",
                "This folder is not under version control. Open a folder that is, or run git init in the terminal.",
            )
        } else {
            ("Git could not read this folder", self.note.as_str())
        };
        layout::push_ui_text(
            out,
            atlas,
            Viewport {
                x,
                y,
                width,
                height: 22.0,
            },
            title,
            theme.text,
        );
        y += 26.0;
        for line in layout::wrap_words(atlas, body, width).into_iter().take(8) {
            layout::push_ui_text(
                out,
                atlas,
                Viewport {
                    x,
                    y,
                    width,
                    height: 20.0,
                },
                &line,
                theme.gutter_text,
            );
            y += 20.0;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_file_row(
        &self,
        atlas: &mut Atlas,
        g: Sidebar,
        rect: Viewport,
        change: usize,
        group: Group,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) {
        let Some(item) = self.snapshot.as_ref().and_then(|s| s.changes.get(change)) else {
            return;
        };
        let staged = group == Group::Staged;
        let conflicted = group == Group::Conflicts;
        let selected = change == self.selected && self.showing_diff;
        if selected {
            layout::push_rounded_rect(
                out,
                Viewport {
                    x: rect.x + 6.0,
                    y: rect.y + 1.0,
                    width: (rect.width - 12.0).max(0.0),
                    height: rect.height - 2.0,
                },
                layout::UI_RADIUS_SM + 1.0,
                theme.palette_selected,
            );
            layout::hotspot(rect, layout::Cursor::Pointing, None);
        } else {
            layout::push_row_hover(out, rect, theme);
        }
        let over = layout::hovered(rect);
        // One status letter, coloured, at the trailing edge, the way every
        // Git client lines them up, instead of a second line per file.
        let code = if staged { item.index } else { item.worktree };
        let code = if conflicted {
            b'!'
        } else if code == b' ' {
            b'M'
        } else {
            code
        };
        let colour = match code {
            b'!' => theme.diff_removed,
            b'A' | b'?' => theme.diff_added,
            b'D' => theme.diff_removed,
            b'R' => theme.accent,
            _ => theme.diff_modified,
        };
        let path = item.path.to_string_lossy();
        let name = item
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
        let parent = item
            .path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Indented under the section's title, file icon first, like the tree.
        let icon = icons::for_file(&item.path).glyph;
        let icon_x = rect.x + 24.0;
        let icon_w = layout::icon_width(atlas, icon);
        layout::push_icon_centered(
            out,
            atlas,
            Viewport {
                x: icon_x,
                width: icon_w,
                ..rect
            },
            icon,
            if selected {
                theme.accent
            } else {
                theme.status_text
            },
        );
        let name_x = icon_x + icon_w.max(18.0) + 6.0;
        let trailing = if over && !conflicted {
            g.open(rect).x - 4.0
        } else {
            g.letter(rect).x - 4.0
        };
        let name_rect = Viewport {
            x: name_x,
            y: rect.y,
            width: (trailing - name_x).max(0.0),
            height: rect.height,
        };
        let deleted = code == b'D';
        let name_colour = if deleted {
            layout::faded(theme.text, 0.6)
        } else {
            theme.text
        };
        let name_width = layout::ui_text_width(atlas, &name);
        layout::push_ui_text(out, atlas, name_rect, &name, name_colour);
        if deleted {
            // Struck through, so a removal reads as one before the letter.
            layout::push_rect(
                out,
                atlas,
                [name_x, rect.y + (rect.height * 0.5).round()],
                [name_width.min(name_rect.width), 1.0],
                layout::faded(theme.text, 0.6),
            );
        }
        if !parent.is_empty() && name_width + 8.0 < name_rect.width {
            layout::push_ui_text(
                out,
                atlas,
                Viewport {
                    x: name_rect.x + name_width + 8.0,
                    width: (name_rect.width - name_width - 8.0).max(0.0),
                    ..name_rect
                },
                &parent,
                theme.gutter_text,
            );
        }
        layout::push_ui_text_centered(
            out,
            atlas,
            g.letter(rect),
            &String::from_utf8_lossy(&[code]),
            colour,
        );
        // A conflicted file is staged by Mark Resolved above its text, once
        // no markers are left in it; a plus here would stage the markers.
        if conflicted || !over {
            return;
        }
        // Stage/unstage sits on the row it acts on, with Open File beside
        // it, both shown while the pointer is on the row.
        Button::new(g.open(rect))
            .icon(icons::GO_TO_FILE)
            .tone(Tone::Ghost)
            .tip("Open File")
            .draw(out, atlas, theme);
        Button::new(g.toggle(rect))
            .icon(if staged { icons::REMOVE } else { icons::ADD })
            .tone(Tone::Ghost)
            .enabled(!self.busy())
            .tip(if staged {
                "Unstage Changes"
            } else {
                "Stage Changes"
            })
            .draw(out, atlas, theme);
    }
}

/// Row height in the diff view, matching the editor's own line height closely
/// enough that the two columns do not look like different applications.
pub const DIFF_LINE: f32 = 20.0;
/// Width of the two line-number columns plus the change marker.
const DIFF_GUTTER: f32 = 92.0;

impl Panel {
    pub fn diff_title(&self) -> String {
        match self.selected_change() {
            Some(change) => change.label(),
            None => "No change selected".into(),
        }
    }

    pub fn diff_summary(&self) -> String {
        format!("+{} −{}", self.diff.added, self.diff.removed)
    }

    /// The selected change in the editor column: old and new line numbers in
    /// a gutter, additions and removals on their own bands, and hunk headers
    /// as separators carrying the enclosing context git reported.
    ///
    /// The old panel printed `git diff` verbatim into a text box, headers and
    /// all, so the reader had to skip four lines of plumbing per file and had
    /// no line numbers at all.
    pub fn draw_diff(
        &self,
        atlas: &mut Atlas,
        rect: Viewport,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) {
        let empty = if self.busy() {
            "Reading…"
        } else {
            "No textual change to show"
        };
        let action = |index: usize| {
            self.can_act_on_hunk(index).then(|| {
                if self.diff.hunks[index].staged {
                    "Unstage"
                } else {
                    "Stage"
                }
            })
        };
        draw_diff_lines(
            &self.diff.lines,
            self.diff_scroll,
            empty,
            &action,
            atlas,
            rect,
            theme,
            out,
        );
    }
}

/// Diff lines in a column: old and new line numbers in a gutter, additions
/// and removals on their own bands, and hunk headers as separators. `action`
/// names the button a hunk header carries, if any. Shared by Source Control
/// and the review of a change proposed by Claude.
#[allow(clippy::too_many_arguments)]
pub fn draw_diff_lines(
    lines: &[git::DiffLine],
    scroll: usize,
    empty: &str,
    action: &dyn Fn(usize) -> Option<&'static str>,
    atlas: &mut Atlas,
    rect: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    let (cell_w, _) = atlas.cell_size();
    let columns = ((rect.width - DIFF_GUTTER - 12.0).max(0.0) / cell_w) as usize;
    let visible = (rect.height / DIFF_LINE).max(0.0) as usize;
    if lines.is_empty() {
        layout::push_ui_text(
            out,
            atlas,
            Viewport {
                x: rect.x + 16.0,
                y: rect.y + 8.0,
                width: (rect.width - 32.0).max(0.0),
                height: DIFF_LINE,
            },
            empty,
            theme.status_text,
        );
        return;
    }
    for (row, line) in lines.iter().skip(scroll).take(visible).enumerate() {
        let y = rect.y + row as f32 * DIFF_LINE;
        let band = match line.kind {
            git::DiffKind::Added => Some(theme.diff_added_band),
            git::DiffKind::Removed => Some(theme.diff_removed_band),
            _ => None,
        };
        if let Some(colour) = band {
            layout::push_rect(out, atlas, [rect.x, y], [rect.width, DIFF_LINE], colour);
        }
        if line.kind == git::DiffKind::Hunk {
            // A rule across the column, with the context git named sitting
            // on it. This is a boundary, not a line of the file.
            layout::push_rect(
                out,
                atlas,
                [rect.x, y + DIFF_LINE * 0.5],
                [rect.width, 1.0 / atlas.metrics.scale],
                theme.divider,
            );
            let action = line.hunk.and_then(action).map(|label| {
                (
                    label,
                    Panel::hunk_action_rect(Viewport {
                        x: rect.x,
                        y,
                        width: rect.width,
                        height: DIFF_LINE,
                    }),
                )
            });
            if !line.text.is_empty() {
                let caption = format!("  {}  ", line.text);
                let available = action.as_ref().map_or(rect.width, |(_, button)| {
                    button.x - rect.x - DIFF_GUTTER - 4.0
                });
                let width = layout::ui_text_width(atlas, &caption).min(available.max(0.0));
                if width > 0.0 {
                    layout::push_rect(
                        out,
                        atlas,
                        [rect.x + DIFF_GUTTER, y],
                        [width, DIFF_LINE],
                        theme.sidebar_background,
                    );
                    layout::push_ui_text(
                        out,
                        atlas,
                        Viewport {
                            x: rect.x + DIFF_GUTTER,
                            y,
                            width,
                            height: DIFF_LINE,
                        },
                        &caption,
                        theme.gutter_text,
                    );
                }
            }
            if let Some((label, button)) = action {
                layout::push_rounded_rect(out, button, 4.0, theme.tab_hover);
                layout::push_ui_text_centered(out, atlas, button, label, theme.accent);
            }
            continue;
        }
        if line.kind == git::DiffKind::Section {
            layout::push_ui_text(
                out,
                atlas,
                Viewport {
                    x: rect.x + 12.0,
                    y,
                    width: (rect.width - 24.0).max(0.0),
                    height: DIFF_LINE,
                },
                if line.text == "STAGED" {
                    "Staged"
                } else {
                    "Working tree"
                },
                theme.accent,
            );
            continue;
        }
        let number =
            |out: &mut Vec<GlyphInstance>, atlas: &mut Atlas, value: Option<usize>, x: f32| {
                if let Some(value) = value {
                    layout::push_ui_text_right(
                        out,
                        atlas,
                        Viewport {
                            x,
                            y,
                            width: 34.0,
                            height: DIFF_LINE,
                        },
                        &value.to_string(),
                        theme.gutter_text,
                    );
                }
            };
        // Two number columns and the marker, none of them overlapping:
        // 4..38, 42..76, marker 80..90, text from DIFF_GUTTER.
        number(out, atlas, line.old, rect.x + 4.0);
        number(out, atlas, line.new, rect.x + 42.0);
        let (marker, colour) = match line.kind {
            git::DiffKind::Added => ("+", theme.diff_added),
            git::DiffKind::Removed => ("−", theme.diff_removed),
            git::DiffKind::Note => (" ", theme.status_text),
            _ => (" ", theme.sidebar_text),
        };
        layout::push_ui_text(
            out,
            atlas,
            Viewport {
                x: rect.x + 80.0,
                y,
                width: 10.0,
                height: DIFF_LINE,
            },
            marker,
            colour,
        );
        // Code stays monospace, so a diff lines up the way the file does.
        let shown = crate::text::columns::fit_columns(line.text.chars(), columns);
        layout::push_text(
            out,
            atlas,
            rect.x + DIFF_GUTTER,
            y,
            &shown,
            if line.kind == git::DiffKind::Context {
                theme.sidebar_text
            } else {
                colour
            },
        );
    }
}

/// The branch from the status header: `main` from `main...origin/main
/// [ahead 1]`, `No commits yet on main` or `Initial commit on main`, and
/// `detached HEAD` for `HEAD (no branch)`.
/// A repository's branch, how far it is from its upstream, and any
/// operation in progress: `main ↑2 ↓1 · merging`.
pub fn snapshot_status(snapshot: &Snapshot) -> String {
    let header = &snapshot.branch;
    let mut out = branch_name(header);
    if let Some(open) = header.find('[') {
        let counts = &header[open + 1..header.rfind(']').unwrap_or(header.len())];
        for part in counts.split(", ") {
            if let Some(n) = part.strip_prefix("ahead ") {
                out.push_str(&format!(" ↑{n}"));
            } else if let Some(n) = part.strip_prefix("behind ") {
                out.push_str(&format!(" ↓{n}"));
            }
        }
    }
    if let Some(what) = snapshot.in_progress {
        out.push_str(" · ");
        out.push_str(what.label());
    }
    out
}

fn branch_name(header: &str) -> String {
    let header = header
        .strip_prefix("No commits yet on ")
        .or_else(|| header.strip_prefix("Initial commit on "))
        .unwrap_or(header);
    if header.starts_with("HEAD (no branch)") {
        return "detached HEAD".into();
    }
    let name = header.split("...").next().unwrap_or(header);
    name.split(" [").next().unwrap_or(name).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::git::Change;

    fn change(path: &str, index: u8, worktree: u8) -> Change {
        Change {
            path: PathBuf::from(path),
            original: None,
            index,
            worktree,
        }
    }

    /// Built without touching a repository: `Panel::new` starts a worker.
    fn panel(changes: Vec<Change>) -> Panel {
        let snapshot = Snapshot {
            root: PathBuf::from("/tmp"),
            branch: "main".into(),
            changes,
            in_progress: None,
        };
        let mut panel = Panel::idle(PathBuf::from("/tmp"), Some(snapshot));
        panel.note = String::new();
        panel
    }

    #[test]
    fn branch_names_read_plainly() {
        assert_eq!(branch_name("main...origin/main [ahead 2]"), "main");
        assert_eq!(branch_name("topic"), "topic");
        assert_eq!(branch_name("No commits yet on main"), "main");
        assert_eq!(branch_name("HEAD (no branch)"), "detached HEAD");
    }

    #[test]
    fn branch_status_shows_ahead_and_behind() {
        let mut p = panel(Vec::new());
        assert_eq!(p.branch_status(), "main");
        p.snapshot.as_mut().unwrap().branch = "main...origin/main [ahead 5, behind 2]".into();
        assert_eq!(p.branch_status(), "main ↑5 ↓2");
        p.snapshot.as_mut().unwrap().branch = "dev...origin/dev [behind 1]".into();
        assert_eq!(p.branch_status(), "dev ↓1");
    }

    fn column() -> Viewport {
        Viewport {
            x: 0.0,
            y: 0.0,
            width: 240.0,
            height: 700.0,
        }
    }

    /// The flat list with "M Unstaged" under every name could not show that a
    /// file is staged *and* modified again since. Grouping can, and such a
    /// file has to appear under both headings.
    #[test]
    fn a_file_staged_and_modified_again_is_listed_under_both_headings() {
        let panel = panel(vec![
            change("staged.rs", b'M', b' '),
            change("both.rs", b'M', b'M'),
            change("dirty.rs", b' ', b'M'),
            change("new.rs", b'?', b'?'),
        ]);
        let entries = panel.entries();
        assert_eq!(
            entries[0],
            Entry::Section {
                group: Group::Staged,
                count: 2
            }
        );
        assert_eq!(
            entries[1],
            Entry::File {
                change: 0,
                group: Group::Staged
            }
        );
        assert_eq!(
            entries[2],
            Entry::File {
                change: 1,
                group: Group::Staged
            }
        );
        assert_eq!(
            entries[3],
            Entry::Section {
                group: Group::Changes,
                count: 3
            }
        );
        let unstaged: Vec<_> = entries[4..]
            .iter()
            .map(|e| match e {
                Entry::File { change, .. } => *change,
                Entry::Section { .. } => unreachable!("one section per side"),
            })
            .collect();
        assert_eq!(unstaged, vec![1, 2, 3], "both.rs is listed on both sides");
    }

    /// An unmerged file is neither staged nor unstaged: it has a heading of
    /// its own, first, and no staging control.
    #[test]
    fn conflicted_files_are_listed_first_and_only_once() {
        let panel = panel(vec![
            change("dirty.rs", b' ', b'M'),
            change("both.rs", b'U', b'U'),
            change("added.rs", b'A', b'A'),
        ]);
        let entries = panel.entries();
        assert_eq!(
            entries[..3],
            [
                Entry::Section {
                    group: Group::Conflicts,
                    count: 2
                },
                Entry::File {
                    change: 1,
                    group: Group::Conflicts
                },
                Entry::File {
                    change: 2,
                    group: Group::Conflicts
                },
            ]
        );
        assert_eq!(entries.len(), 5, "then Changes with dirty.rs alone");
        assert_eq!(panel.conflict_count(), 2);
        assert_eq!(panel.staged_count(), 0);
    }

    #[test]
    fn branch_status_names_a_merge_in_progress() {
        let mut p = panel(Vec::new());
        p.snapshot.as_mut().unwrap().in_progress = Some(git::InProgress::Merge);
        assert_eq!(p.branch_status(), "main · MERGING");
    }

    #[test]
    fn an_empty_repository_lists_nothing_rather_than_an_empty_heading() {
        assert!(panel(Vec::new()).entries().is_empty());
    }

    /// Staging acts on the row that was clicked. The old pair of buttons acted
    /// on whatever happened to be selected, from the bottom of the panel.
    #[test]
    fn the_row_toggle_and_the_row_body_are_different_targets() {
        let panel = panel(vec![change("dirty.rs", b' ', b'M')]);
        let g = Sidebar::new(column());
        let rows = panel.rows(g);
        let (entry, rect) = rows
            .iter()
            .find(|(e, _)| matches!(e, Entry::File { .. }))
            .expect("one file row");
        assert_eq!(
            *entry,
            Entry::File {
                change: 0,
                group: Group::Changes
            }
        );
        let toggle = g.toggle(*rect);
        assert!(rect.contains(rect.x + 40.0, rect.y + 4.0), "the name area");
        assert!(!toggle.contains(rect.x + 40.0, rect.y + 4.0));
        assert!(toggle.contains(toggle.x + 4.0, toggle.y + 4.0));
        // The toggle stays inside its row, so it can never stage a neighbour.
        assert!(toggle.x >= rect.x && toggle.x + toggle.width <= rect.x + rect.width);
        assert!(toggle.y >= rect.y && toggle.y + toggle.height <= rect.y + rect.height);
    }

    /// The modal clamped itself to a fixed size whatever it held. The docked
    /// column has to stay inside the sidebar it is given, at any height.
    #[test]
    fn the_column_contents_stay_inside_the_sidebar_at_any_height() {
        for height in [180.0, 320.0, 700.0, 1400.0] {
            let column = Viewport { height, ..column() };
            let g = Sidebar::new(column);
            for (name, rect) in [
                ("repo", g.repo),
                ("branch", g.branch),
                ("refresh", g.refresh),
                ("message", g.message),
                ("commit", g.commit),
                ("list", g.list),
                ("note", g.note),
            ] {
                assert!(
                    rect.x >= column.x && rect.x + rect.width <= column.x + column.width + 0.01,
                    "{name} escapes the column at height {height}"
                );
                assert!(rect.height >= 0.0, "{name} has negative height");
            }
            assert!(
                g.list.y >= column.y + layout::SIDEBAR_HEADER_HEIGHT,
                "the list overlaps the sidebar switcher"
            );
            // The repository menu is the title, in the header row, left of
            // the header's buttons and above the branch.
            let (switcher, _) = layout::sidebar_switcher(column);
            assert_eq!(g.repo.y, switcher.y);
            assert!(g.repo.x + g.repo.width <= g.refresh.x);
            assert!(g.refresh.x + g.refresh.width <= g.more.x);
            assert!(g.repo.y + g.repo.height <= g.branch.y);
            assert!(g.branch.x + g.branch.width <= g.pull.x);
            assert!(g.pull.x + g.pull.width <= g.push.x);
            assert!(
                g.note.y + g.note.height <= column.y + column.height,
                "the note falls out of the bottom at height {height}"
            );
        }
    }

    /// Rows are only reported where they were drawn, so a click below the last
    /// change does nothing rather than staging the file nearest to it.
    #[test]
    fn the_empty_space_below_the_last_change_is_not_a_row() {
        let panel = panel(vec![change("dirty.rs", b' ', b'M')]);
        let g = Sidebar::new(column());
        let rows = panel.rows(g);
        let last = rows.last().expect("a row").1;
        let below = last.y + last.height + 40.0;
        assert!(below < g.list.y + g.list.height, "fixture has room below");
        assert!(panel.entry_at(g, last.x + 20.0, below).is_none());
    }

    /// A short column cannot draw every row; it must not report ones it
    /// clipped, or a click would land on a file that is not on screen.
    #[test]
    fn a_short_column_reports_only_the_rows_it_drew() {
        let changes: Vec<_> = (0..40)
            .map(|i| change(&format!("file{i}.rs"), b' ', b'M'))
            .collect();
        let panel = panel(changes);
        let g = Sidebar::new(Viewport {
            height: 300.0,
            ..column()
        });
        let rows = panel.rows(g);
        assert!(rows.len() < panel.entries().len(), "the fixture overflows");
        for (_, rect) in &rows {
            assert!(
                rect.y + rect.height <= g.list.y + g.list.height + 0.01,
                "a row was reported past the bottom of the list"
            );
        }
    }
}
