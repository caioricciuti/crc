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
    /// Chosen lines of hunks, each with the indexes of its lines.
    StageLines(Change, Vec<(git::Hunk, Vec<usize>)>),
    /// One change the editor's gutter marks, staged from the editor's text.
    StageText(StageText),
    /// The message, and whether it replaces HEAD.
    Commit(String, bool),
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

impl Operation {
    /// What the panel's note says while it runs, named: "Committing",
    /// not "Working". `first` when the repository has not been read yet.
    fn verb(&self, first: bool) -> &'static str {
        match self {
            Operation::Refresh if first => "Reading repository",
            Operation::Refresh => "Refreshing",
            Operation::Select(_) => "Reading diff",
            Operation::Stage(_, stage) | Operation::StageAll(_, stage) => {
                if *stage {
                    "Staging"
                } else {
                    "Unstaging"
                }
            }
            Operation::StageHunk(_, hunk) => {
                if hunk.staged {
                    "Unstaging"
                } else {
                    "Staging"
                }
            }
            Operation::StageLines(_, lines) => {
                if lines.first().is_some_and(|(hunk, _)| hunk.staged) {
                    "Unstaging"
                } else {
                    "Staging"
                }
            }
            Operation::StageText(_) => "Staging",
            Operation::Commit(_, false) => "Committing",
            Operation::Commit(_, true) => "Amending",
            Operation::Switch(_) => "Switching branch",
            Operation::CreateBranch(_) => "Creating branch",
            Operation::DeleteBranch(..) => "Deleting branch",
            Operation::RenameBranch(..) => "Renaming branch",
            Operation::Abort(_) => "Aborting",
            Operation::ContinueRebase => "Continuing rebase",
            Operation::Remote(what, _) => what.verb(),
            Operation::Resolve(_) => "Marking resolved",
        }
    }

    /// Whether the status line says it too: work you asked for that can
    /// take a while. Reading the repository or a diff is the panel's own,
    /// and happens on every save.
    fn told(&self) -> bool {
        !matches!(self, Operation::Refresh | Operation::Select(_))
    }
}

/// A change from the editor to stage: the file, its text as of HEAD and in
/// the editor, the change between them, and what the status line says
/// once it is staged.
pub struct StageText {
    pub path: PathBuf,
    pub head: std::sync::Arc<String>,
    pub text: String,
    pub change: crate::ide::diff::Change,
    pub done: String,
}

struct Reply {
    snapshot: Snapshot,
    selected: usize,
    diff: String,
    committed: bool,
    /// The commit replaced HEAD.
    amended: bool,
    error: Option<String>,
    /// What a branch or remote command did, for the status line.
    done: Option<String>,
}

/// The commit message's text inset inside its box, for drawing and clicks.
pub const MESSAGE_PAD: f32 = 9.0;

/// Conventional-commit types the message field completes, most used first:
/// a prefix several of them share completes to the earliest.
pub const COMMIT_TYPES: [&str; 11] = [
    "feat", "fix", "chore", "docs", "refactor", "perf", "test", "build", "ci", "style", "revert",
];

/// The rest of the commit type `message` starts to spell, offered while the
/// caret sits at the end of a lowercase word on the first line. Nothing once
/// a scope, a colon, a `!` or a space follows, or the word is a whole type.
pub fn commit_type_suggestion(message: &str, cursor: usize) -> Option<&'static str> {
    if cursor != message.len()
        || message.is_empty()
        || !message.bytes().all(|b| b.is_ascii_lowercase())
    {
        return None;
    }
    COMMIT_TYPES
        .iter()
        .find(|kind| kind.len() > message.len() && kind.starts_with(message))
        .map(|kind| &kind[message.len()..])
}

/// The subject line length the message field counts against.
pub const SUBJECT_LIMIT: usize = 50;

/// The length of the message's subject, its first line, in characters.
pub fn subject_len(message: &str) -> usize {
    message
        .lines()
        .next()
        .map_or(0, |line| line.chars().count())
}

/// The column body lines are wrapped at by convention; the field marks it
/// and tints what runs past it.
pub const BODY_LIMIT: usize = 72;
/// Height of one line of the message field.
pub const MESSAGE_LINE: f32 = 18.0;
/// Lines the message field grows to before it scrolls.
pub const MESSAGE_MAX_LINES: usize = 8;

/// The byte ranges of the message's lines, line breaks left out. A
/// trailing break opens an empty last line, where the caret then sits.
pub fn message_lines(message: &str) -> Vec<std::ops::Range<usize>> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (at, _) in message.match_indices('\n') {
        lines.push(start..at);
        start = at + 1;
    }
    lines.push(start..message.len());
    lines
}

/// The line of `lines` that holds byte `at`.
pub fn message_line_of(lines: &[std::ops::Range<usize>], at: usize) -> usize {
    lines
        .iter()
        .position(|line| at <= line.end)
        .unwrap_or(lines.len().saturating_sub(1))
}

/// How many lines the field shows for `message`: one at least, then one
/// per line up to [`MESSAGE_MAX_LINES`].
pub fn message_rows(message: &str) -> usize {
    (message.matches('\n').count() + 1).min(MESSAGE_MAX_LINES)
}

/// The first line shown, moved as little as it takes to keep the caret's
/// line among the `rows` shown and no blank rows below the last line.
pub fn message_top(top: usize, caret_line: usize, rows: usize, lines: usize) -> usize {
    let rows = rows.max(1);
    let top = if caret_line < top {
        caret_line
    } else if caret_line >= top + rows {
        caret_line + 1 - rows
    } else {
        top
    };
    top.min(lines.saturating_sub(rows))
}

/// The caret line's sideways scroll, moved to keep the caret at `caret` in
/// a `width` wide view, and no further right than a line `widest` wide
/// needs. Past
/// the right edge it moves as little as it takes; past the left, back to
/// the lines' starts when the caret fits there, else a quarter in.
pub fn message_scroll_x(scroll: f32, caret: f32, width: f32, widest: f32) -> f32 {
    let scroll = if caret < scroll {
        if caret <= width - 1.0 {
            0.0
        } else {
            caret - width * 0.25
        }
    } else if caret > scroll + width - 1.0 {
        caret - width + 1.0
    } else {
        scroll
    };
    scroll.min((widest - width + 1.0).max(0.0)).max(0.0)
}

/// The bytes of body line `line` past [`BODY_LIMIT`] characters, which the
/// field tints. The subject has its own limit and counter.
pub fn past_body_limit(message: &str, line: usize) -> Option<std::ops::Range<usize>> {
    if line == 0 {
        return None;
    }
    let range = message_lines(message).get(line)?.clone();
    let (at, _) = message[range.clone()].char_indices().nth(BODY_LIMIT)?;
    Some(range.start + at..range.end)
}

/// Whether a body follows the subject without the blank line Git and
/// most tools expect between them.
pub fn body_needs_blank_line(message: &str) -> bool {
    message_lines(message)
        .get(1)
        .is_some_and(|second| !message[second.clone()].trim().is_empty())
}

/// The top of the message's first line inside its box, centred in a
/// one-line field.
const MESSAGE_BAND: f32 = (layout::UI_CONTROL - MESSAGE_LINE) * 0.5;

/// A message line shaped in the UI font, which shapes 512 bytes at most:
/// a longer line is shaped up to there and the rest is not drawn.
fn shape_message_line(
    atlas: &mut Atlas,
    line: &str,
) -> Option<std::rc::Rc<crate::render::font::ShapedLine>> {
    let mut end = line.len().min(500);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    atlas.shape_ui(&line[..end])
}

/// An operation on the worker.
struct InFlight {
    rx: Receiver<Result<Reply, String>>,
    /// The message, when it is a commit.
    submitted: Option<String>,
    /// Whether its outcome is said in the status line.
    announces: bool,
    /// Whether the diff keeps its scroll and cursor once it is read again,
    /// when it is still the same file's: a refresh, or a hunk or lines
    /// staged from it, where the reader carries on.
    keeps_place: bool,
    /// A hunk or lines were staged: the selection is gone from where it was.
    stages_from_diff: bool,
    /// Whether the status line says what runs, beside the panel's note.
    told: bool,
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
    /// The diff line the keyboard is on, and where a selection of lines
    /// started: Space stages the hunk or the lines between them.
    pub diff_cursor: usize,
    pub diff_anchor: Option<usize>,
    pub message: Buffer,
    /// Commit replaces HEAD instead of adding a commit.
    pub amend: bool,
    /// HEAD's message as Amend put it in the field, so turning Amend off
    /// takes it back out unless it was edited.
    amend_prefill: Option<String>,
    /// The message field's first line shown, and the line the caret was
    /// on with its sideways scroll in points, as last drawn: a click lands
    /// on what is on screen. Only the caret's line scrolls sideways.
    message_top: std::cell::Cell<usize>,
    message_scroll: std::cell::Cell<(usize, f32)>,
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
/// it, the message, Commit. The message grows a line at a time with what
/// is written in it, up to [`MESSAGE_MAX_LINES`], and pushes the rest
/// down. Then the list, and a status line at the bottom.
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
    /// The Amend toggle, at the trailing end of Commit's row.
    pub amend: Viewport,
    pub list: Viewport,
    pub note: Viewport,
}

/// Width of the Pull and Push buttons: an arrow and a count.
const SYNC_WIDTH: f32 = 44.0;
/// Width of the Amend toggle: a pencil and its name.
const AMEND_WIDTH: f32 = 80.0;

impl Sidebar {
    /// The layout for a message field showing `message_rows` lines (see
    /// [`message_rows`]); [`Panel::sidebar`] passes the panel's own.
    pub fn new(column: Viewport, message_rows: usize) -> Self {
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
            height: control + MESSAGE_LINE * message_rows.saturating_sub(1) as f32,
        };
        // A narrow column keeps the pencil alone, so Commit keeps its room.
        let amend_width = if width >= AMEND_WIDTH * 2.5 {
            AMEND_WIDTH
        } else {
            control.min(width)
        };
        let amend = Viewport {
            x: x + width - amend_width,
            y: message.y + message.height + layout::UI_GAP,
            width: amend_width,
            height: control,
        };
        let commit = Viewport {
            x,
            y: amend.y,
            width: (amend.x - layout::UI_GAP - x).max(0.0),
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
            amend,
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
    /// Where the panel draws in `column`, with the message field as tall
    /// as what is written in it.
    pub fn sidebar(&self, column: Viewport) -> Sidebar {
        Sidebar::new(column, message_rows(&self.message.rope.to_string()))
    }

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
            diff_cursor: 0,
            diff_anchor: None,
            message: Buffer::new(),
            amend: false,
            amend_prefill: None,
            message_top: Default::default(),
            message_scroll: Default::default(),
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
    /// The operation under way that the status line says too, as the
    /// note has it ("Pushing…"): one you asked for, not a refresh.
    pub fn working(&self) -> Option<&str> {
        self.in_flight
            .as_ref()
            .filter(|f| f.told)
            .map(|_| self.note.as_str())
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
        } else if self.amend {
            // Amending with nothing staged rewords HEAD.
            match self.amend_blocker() {
                Some(why) => Some(why),
                None if self.message.rope.to_string().trim().is_empty() => {
                    Some("Write a commit message first")
                }
                None => None,
            }
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
        let ready = if self.amend {
            self.snapshot.is_some() && self.amend_blocker().is_none()
        } else {
            self.snapshot
                .as_ref()
                .is_some_and(|s| s.changes.iter().any(Change::staged))
        };
        ready && !self.message.rope.to_string().trim().is_empty()
    }

    /// Why Amend cannot be turned on, for its tooltip; `None` when it can.
    pub fn amend_blocker(&self) -> Option<&'static str> {
        let snapshot = self.snapshot.as_ref()?;
        if snapshot.head_message.is_none() {
            Some("Nothing to amend: no commits yet")
        } else if let Some(what) = snapshot.in_progress {
            Some(match what {
                git::InProgress::Merge => "A merge is in progress: finish or abort it first",
                git::InProgress::Rebase => "A rebase is in progress: finish or abort it first",
                git::InProgress::CherryPick => {
                    "A cherry-pick is in progress: finish or abort it first"
                }
                git::InProgress::Revert => "A revert is in progress: finish or abort it first",
            })
        } else if snapshot.head_pushed {
            Some("Already pushed: amending would rewrite a published commit")
        } else {
            None
        }
    }

    /// Turns Amend on or off. On, an empty message field starts from
    /// HEAD's message; off, that message goes again unless it was edited.
    pub fn set_amend(&mut self, on: bool) {
        if on == self.amend || (on && self.amend_state() == "blocked") {
            return;
        }
        self.amend = on;
        if on {
            let head = self.snapshot.as_ref().and_then(|s| s.head_message.clone());
            if let Some(head) = head
                && self.message.rope.len_bytes() == 0
            {
                self.message = Buffer::new();
                self.message.insert(&head);
                self.amend_prefill = Some(head);
            }
        } else {
            if self
                .amend_prefill
                .as_deref()
                .is_some_and(|prefill| self.message.rope.to_string() == prefill)
            {
                self.message = Buffer::new();
            }
            self.amend_prefill = None;
        }
    }

    /// Whether the dump and the toggle read Amend as off, on or blocked.
    pub fn amend_state(&self) -> &'static str {
        if self.amend {
            "on"
        } else if self.snapshot.is_none() || self.amend_blocker().is_some() {
            "blocked"
        } else {
            "off"
        }
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
    /// Stages one change from the editor, in the repository its file is
    /// in, and says how it went in the status line.
    pub fn stage_text(&mut self, job: StageText) {
        self.start(Operation::StageText(job));
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
            x: row.x + row.width - 96.0,
            y: row.y + 1.0,
            width: 88.0,
            height: row.height - 2.0,
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

    /// The diff line drawn at `y`, if there is one.
    pub fn diff_line_at(&self, diff: Viewport, y: f32) -> Option<usize> {
        if y < diff.y || y >= diff.y + diff.height {
            return None;
        }
        let line = self.diff_scroll + ((y - diff.y) / DIFF_LINE) as usize;
        (line < self.diff.lines.len()).then_some(line)
    }

    /// Where diff line `line` is drawn, while it is in view.
    pub fn diff_line_rect(&self, diff: Viewport, line: usize) -> Option<Viewport> {
        let row = line.checked_sub(self.diff_scroll)?;
        let visible = (diff.height / DIFF_LINE).max(0.0) as usize;
        (line < self.diff.lines.len() && row < visible).then_some(Viewport {
            x: diff.x,
            y: diff.y + row as f32 * DIFF_LINE,
            width: diff.width,
            height: DIFF_LINE,
        })
    }

    /// Puts the diff's cursor on a line; `extend` keeps where the
    /// selection started (a shift-click, a drag), otherwise it starts over.
    pub fn set_diff_cursor(&mut self, line: usize, extend: bool) {
        let last = self.diff.lines.len().saturating_sub(1);
        if extend {
            self.diff_anchor.get_or_insert(self.diff_cursor);
        } else {
            self.diff_anchor = None;
        }
        self.diff_cursor = line.min(last);
    }

    /// Up or Down in the diff, by `delta` lines, keeping the cursor among
    /// the `visible` rows drawn.
    pub fn move_diff_cursor(&mut self, delta: isize, extend: bool, visible: usize) {
        let line = self.diff_cursor.saturating_add_signed(delta);
        self.set_diff_cursor(line, extend);
        let visible = visible.max(1);
        if self.diff_cursor < self.diff_scroll {
            self.diff_scroll = self.diff_cursor;
        } else if self.diff_cursor >= self.diff_scroll + visible {
            self.diff_scroll = self.diff_cursor + 1 - visible;
        }
    }

    /// The lines a selection covers, first to last: the cursor's line alone
    /// without one.
    pub fn diff_selection(&self) -> std::ops::RangeInclusive<usize> {
        let anchor = self.diff_anchor.unwrap_or(self.diff_cursor);
        anchor.min(self.diff_cursor)..=anchor.max(self.diff_cursor)
    }

    /// For every diff line under a hunk's header, the hunk and the line's
    /// index among the lines under it, as [`git::stage_hunk_lines`] takes it.
    fn line_owners(&self) -> Vec<Option<(usize, usize)>> {
        let mut owners = Vec::with_capacity(self.diff.lines.len());
        // The hunk being read: its index, header line and how many lines
        // its text has under the header.
        let mut current: Option<(usize, usize, usize)> = None;
        for (index, line) in self.diff.lines.iter().enumerate() {
            if let Some(hunk) = line.hunk {
                let lines = self.diff.hunks[hunk].raw.split_terminator('\n').count();
                current = Some((hunk, index, lines.saturating_sub(1)));
                owners.push(None);
                continue;
            }
            if line.kind == git::DiffKind::Section {
                current = None;
            }
            owners.push(current.and_then(|(hunk, header, lines)| {
                let offset = index - header - 1;
                (offset < lines).then_some((hunk, offset))
            }));
        }
        owners
    }

    /// Space in the diff: the selected added and removed lines, or the
    /// whole hunk the cursor is in (or on the header of) when none are
    /// selected. Staged lines are unstaged. A selection running into the
    /// other section keeps to the side its first changed line is on.
    pub fn stage_selection(&mut self) {
        let owners = self.line_owners();
        let range = self.diff_selection();
        let changed: Vec<(usize, usize)> = range
            .clone()
            .filter(|&i| {
                matches!(
                    self.diff.lines.get(i).map(|l| l.kind),
                    Some(git::DiffKind::Added | git::DiffKind::Removed)
                )
            })
            .filter_map(|i| owners.get(i).copied().flatten())
            .collect();
        let Some(&(first, _)) = changed.first() else {
            // On a header, or a selection of unchanged lines: the hunk.
            let cursor = self.diff_cursor;
            let hunk = self
                .diff
                .lines
                .get(cursor)
                .and_then(|l| l.hunk)
                .or_else(|| owners.get(cursor).copied().flatten().map(|(h, _)| h));
            if let Some(hunk) = hunk
                && self.can_act_on_hunk(hunk)
            {
                self.stage_hunk(hunk);
            }
            return;
        };
        let staged = self.diff.hunks[first].staged;
        let mut picks: Vec<(git::Hunk, Vec<usize>)> = Vec::new();
        for (hunk, offset) in changed {
            if self.diff.hunks[hunk].staged != staged || !self.can_act_on_hunk(hunk) {
                continue;
            }
            match picks.last_mut() {
                Some((last, lines)) if last.ordinal == self.diff.hunks[hunk].ordinal => {
                    lines.push(offset)
                }
                _ => picks.push((self.diff.hunks[hunk].clone(), vec![offset])),
            }
        }
        if picks.is_empty() {
            return;
        }
        if let Some(change) = self.selected_change().cloned() {
            self.start(Operation::StageLines(change, picks));
        }
    }
    /// The commit type completion for the message as it stands, if any; a
    /// selection would be replaced by typing, so it offers none.
    pub fn commit_type_hint(&self) -> Option<&'static str> {
        if self.message.selection().is_some() {
            return None;
        }
        commit_type_suggestion(&self.message.rope.to_string(), self.message.cursor())
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
            self.start(Operation::Commit(message, self.amend));
        }
    }
    fn start(&mut self, operation: Operation) {
        if self.busy() {
            if !matches!(operation, Operation::Refresh) || self.queued.is_none() {
                self.queued = Some(operation);
            }
            return;
        }
        let submitted = if let Operation::Commit(message, _) = &operation {
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
                | Operation::StageText(_)
                | Operation::Commit(..)
        );
        let stages_from_diff = matches!(
            operation,
            Operation::StageHunk(..) | Operation::StageLines(..)
        );
        // The refresh the watcher starts after a stage included.
        let keeps_place = stages_from_diff || matches!(operation, Operation::Refresh);
        let directory = self.directory.clone();
        let guard = self.guard.clone();
        let previous = self.selected_change().map(|c| c.path.clone());
        let (tx, rx) = mpsc::channel();
        self.note = format!("{}…", operation.verb(self.snapshot.is_none()));
        self.in_flight = Some(InFlight {
            rx,
            submitted,
            announces,
            keeps_place,
            stages_from_diff,
            told: operation.told(),
        });
        self.note_success = false;
        std::thread::spawn(move || {
            let result = (|| {
                // The root alone: a whole status here was thrown away.
                let root = git::toplevel(&directory)?;
                let mut selected_path = previous;
                let mut committed = false;
                let mut amended = false;
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
                    Operation::StageLines(change, picks) => {
                        git::stage_hunk_lines(&root, &change, &picks).err()
                    }
                    Operation::StageText(job) => {
                        // The file's own repository, which in a workspace
                        // of several need not be the panel's.
                        let staged = job
                            .path
                            .parent()
                            .ok_or_else(|| "no parent directory".to_string())
                            .and_then(git::toplevel)
                            .and_then(|top| {
                                let relative = job
                                    .path
                                    .strip_prefix(&top)
                                    .map_err(|_| "outside the repository".to_string())?;
                                git::stage_text_hunk(
                                    &top,
                                    relative,
                                    &job.head,
                                    &job.text,
                                    &job.change,
                                )
                            });
                        match staged {
                            Ok(()) => {
                                done = Some(job.done);
                                None
                            }
                            Err(error) => Some(error),
                        }
                    }
                    Operation::Commit(message, amend) => {
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
                            None => match git::commit(&root, &message, amend) {
                                Ok(()) => {
                                    committed = true;
                                    amended = amend;
                                    let subject = message.lines().next().unwrap_or("").trim();
                                    let verb = if amend { "amended" } else { "committed" };
                                    done = Some(format!("{verb} \u{201c}{subject}\u{201d}"));
                                    None
                                }
                                Err(error) => Some(error),
                            },
                        }
                    }
                };
                let mut snapshot = git::snapshot(&root)?;
                git::read_head(&mut snapshot);
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
                    amended,
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
                let before = self.selected_change().map(|c| c.path.clone());
                self.snapshot = Some(reply.snapshot);
                self.selected = reply.selected;
                self.diff = git::Diff::parse(&reply.diff);
                // The same file read again keeps the reader's place.
                // Staged from the diff, the cursor goes to where the
                // selection started, on what moved up into its place.
                let last = self.diff.lines.len().saturating_sub(1);
                if flight.keeps_place
                    && reply.error.is_none()
                    && before.is_some()
                    && before.as_ref() == self.selected_change().map(|c| &c.path)
                {
                    if flight.stages_from_diff {
                        self.diff_cursor = *self.diff_selection().start();
                        self.diff_anchor = None;
                    }
                    self.diff_cursor = self.diff_cursor.min(last);
                    self.diff_anchor = self.diff_anchor.map(|a| a.min(last));
                    self.diff_scroll = self.diff_scroll.min(last);
                } else {
                    self.diff_cursor = 0;
                    self.diff_anchor = None;
                    self.diff_scroll = 0;
                }
                // The list may be shorter now (a commit took most of it):
                // scrolled past its end, it drew as "No changes".
                self.list_scroll = self.list_scroll.min(self.entries().len().saturating_sub(1));
                if reply.committed
                    && flight.submitted.as_deref() == Some(self.message.rope.to_string().as_str())
                {
                    self.message = Buffer::new();
                }
                // Amend is for the one commit it was turned on for, and
                // goes off once HEAD can no longer be amended: pushed, say.
                if reply.amended || (self.amend && self.amend_blocker().is_some()) {
                    self.set_amend(false);
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
                    if reply.amended {
                        "Commit amended".into()
                    } else if reply.committed {
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

    /// The commit message inside its `field`: each line on its row, the
    /// subject's length at the right of the first, a hairline at the body's
    /// 72nd column with what runs past it tinted, and the caret kept in
    /// view both ways.
    fn draw_message(
        &self,
        atlas: &mut Atlas,
        field: Viewport,
        focused: bool,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) {
        let full = self.message.rope.to_string();
        let lines = message_lines(&full);
        let rows = message_rows(&full);
        let cursor = self.message.cursor();
        let caret_line = message_line_of(&lines, cursor);
        let top = message_top(self.message_top.get(), caret_line, rows, lines.len());
        self.message_top.set(top);
        let shown = top..(top + rows).min(lines.len());

        // The subject's length, right-aligned on the first line once there
        // is a message; the subject clips short of it.
        let counter = (!full.is_empty()).then(|| {
            let length = subject_len(&full);
            let label = format!("{length}/{SUBJECT_LIMIT}");
            let width = layout::ui_text_width(atlas, &label).ceil();
            (length, label, width)
        });
        let counter_room = counter.as_ref().map_or(0.0, |(_, _, width)| width + 6.0);
        let body_width = (field.width - MESSAGE_PAD * 2.0).max(0.0);
        let cut = |line: usize| if line == 0 { counter_room } else { 0.0 };
        let row = |line: usize| Viewport {
            x: field.x + MESSAGE_PAD,
            y: field.y + MESSAGE_BAND + (line - top) as f32 * MESSAGE_LINE,
            width: (body_width - cut(line)).max(0.0),
            height: MESSAGE_LINE,
        };
        let shaped: Vec<_> = shown
            .clone()
            .map(|line| shape_message_line(atlas, &full[lines[line].clone()]))
            .collect();
        let x_of = |line: usize, byte: usize| {
            shaped[line - top]
                .as_ref()
                .map_or(0.0, |shaped| shaped.x_of_byte(byte - lines[line].start))
        };
        // Sideways by measured width, the caret's line alone, so a long
        // line shows its end with the caret there and every other line
        // still reads from its start.
        let caret_x = x_of(caret_line, cursor);
        let (scrolled, previous) = self.message_scroll.get();
        let scroll = message_scroll_x(
            if scrolled == caret_line {
                previous
            } else {
                0.0
            },
            caret_x,
            row(caret_line).width,
            x_of(caret_line, lines[caret_line].end),
        );
        self.message_scroll.set((caret_line, scroll));
        let scroll_of = |line: usize| if line == caret_line { scroll } else { 0.0 };

        // A body straight under the subject: Git and most tools read the
        // first paragraph as the subject. Said, not fixed.
        if body_needs_blank_line(&full) && shown.contains(&1) {
            let band = Viewport {
                x: field.x + 3.0,
                width: (field.width - 6.0).max(0.0),
                ..row(1)
            };
            let amber = theme.diff_modified;
            layout::push_rounded_rect(out, band, 3.0, [amber[0], amber[1], amber[2], 0.12]);
            layout::hotspot(
                band,
                layout::Cursor::Text,
                Some("Leave a blank line between the subject and the body"),
            );
        }
        // The 72nd column, once there is a body to measure against: a
        // hairline on each body row, where that row's scroll puts it. The
        // font is proportional, so the column is 72 average advances of
        // ordinary prose: near the 72nd character of a sentence, while the
        // tint counts characters exactly.
        if lines[1..].iter().any(|line| !line.is_empty()) {
            const PROSE: &str = "the quick brown fox jumps over the lazy dog";
            let at = layout::ui_text_width(atlas, PROSE) / PROSE.len() as f32 * BODY_LIMIT as f32;
            for line in shown.start.max(1)..shown.end {
                let rect = row(line);
                let x = at - scroll_of(line);
                if (0.0..rect.width).contains(&x) {
                    layout::push_rect(
                        out,
                        atlas,
                        [(rect.x + x).round(), rect.y],
                        [1.0, rect.height],
                        theme.hairline,
                    );
                }
            }
        }

        // Behind the glyphs, so the text stays readable on top of the band.
        // A selection that runs on past a line's end takes its break too.
        if let Some(selection) = self.message.selection() {
            for line in shown.clone() {
                let range = lines[line].clone();
                if selection.end <= range.start || selection.start > range.end {
                    continue;
                }
                let rect = row(line);
                let x0 = x_of(line, selection.start.max(range.start)) - scroll_of(line);
                let mut x1 = x_of(line, selection.end.min(range.end)) - scroll_of(line);
                if selection.end > range.end {
                    x1 += 4.0;
                }
                let (x0, x1) = (x0.clamp(0.0, rect.width), x1.clamp(0.0, rect.width));
                if x1 > x0 {
                    layout::push_rect(
                        out,
                        atlas,
                        [rect.x + x0, rect.y],
                        [x1 - x0, rect.height],
                        theme.selection,
                    );
                }
            }
        }
        if full.is_empty() {
            let placeholder = if focused {
                "Message"
            } else if self.amend {
                "Message (\u{2318}\u{21a9} to amend)"
            } else {
                "Message (\u{2318}\u{21a9} to commit)"
            };
            layout::push_ui_text(out, atlas, row(0), placeholder, theme.status_text);
        }
        for line in shown.clone() {
            let Some(shaped) = &shaped[line - top] else {
                continue;
            };
            let past = past_body_limit(&full, line);
            let start = lines[line].start;
            layout::push_ui_line(out, atlas, row(line), shaped, scroll_of(line), |byte| {
                if past
                    .as_ref()
                    .is_some_and(|past| past.contains(&(start + byte)))
                {
                    theme.diff_modified
                } else {
                    theme.text
                }
            });
        }
        if focused {
            let rect = row(caret_line);
            let x = (caret_x - scroll).clamp(0.0, rect.width);
            layout::push_rect(
                out,
                atlas,
                [rect.x + x, rect.y],
                [1.0, rect.height],
                theme.cursor,
            );
            // The rest of a commit type being typed, dimmed after the
            // caret; Tab takes it.
            if let Some(rest) = self.commit_type_hint() {
                layout::push_ui_text(
                    out,
                    atlas,
                    Viewport {
                        x: rect.x + x,
                        width: (rect.width - x).max(0.0),
                        ..rect
                    },
                    rest,
                    theme.status_text,
                );
            }
        }
        if let Some((length, label, width)) = &counter
            && top == 0
        {
            let rect = Viewport {
                x: field.x + field.width - MESSAGE_PAD - width,
                width: *width,
                height: layout::UI_CONTROL,
                ..field
            };
            let over = *length > SUBJECT_LIMIT;
            layout::push_ui_text(
                out,
                atlas,
                rect,
                label,
                if over {
                    theme.diff_modified
                } else {
                    theme.status_text
                },
            );
            layout::hotspot(
                rect,
                layout::Cursor::Arrow,
                Some("Subject lines read best at 50 characters or fewer"),
            );
        }
    }

    /// The byte of the message a click at (`x`, `y`) in the field lands
    /// on, the field scrolled as it was last drawn.
    pub fn message_byte_at(&self, atlas: &mut Atlas, g: Sidebar, x: f32, y: f32) -> usize {
        let full = self.message.rope.to_string();
        let lines = message_lines(&full);
        let rows = message_rows(&full);
        let top = self.message_top.get().min(lines.len().saturating_sub(rows));
        let row = ((y - g.message.y - MESSAGE_BAND) / MESSAGE_LINE).max(0.0) as usize;
        let line = (top + row.min(rows - 1)).min(lines.len() - 1);
        let range = lines[line].clone();
        let (scrolled, scroll) = self.message_scroll.get();
        let at = x - g.message.x - MESSAGE_PAD + if scrolled == line { scroll } else { 0.0 };
        shape_message_line(atlas, &full[range.clone()])
            .map_or(range.start, |shaped| range.start + shaped.byte_at_x(at))
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
        let g = self.sidebar(column);
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

        // The message: a field, ringed while it has the keyboard, a line
        // tall per line written until it scrolls.
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
        self.draw_message(atlas, g.message, focused, theme, out);

        // The button says what it will commit. "Commit" alone next to a list
        // that mixes staged and unstaged files does not.
        let staged = self.staged_count();
        let commit_label = match staged {
            _ if self.amend => "Amend".to_owned(),
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
            // The shortcut, where the button has room for it.
            None if commit.hint("\u{2318}\u{21a9}").content_width(atlas, 8.0) <= g.commit.width => {
                commit.hint("\u{2318}\u{21a9}")
            }
            None => commit.tip("\u{2318}\u{21a9}"),
        };
        commit.draw(out, atlas, theme);
        // Amend: a toggle that turns Commit into a rewrite of HEAD, off
        // with its reason once HEAD is not this branch's own to rewrite.
        let amend_blocker = if self.amend {
            None
        } else {
            self.amend_blocker()
        };
        // While amending, the primary button already reads Amend, so this
        // one is the way back out.
        let amend = Button::new(g.amend)
            .label(match (g.amend.width >= AMEND_WIDTH, self.amend) {
                (false, _) => "",
                (true, true) => "Cancel",
                (true, false) => "Amend",
            })
            .icon(if self.amend {
                icons::CLOSE
            } else {
                icons::EDIT
            })
            .enabled(amend_blocker.is_none());
        match amend_blocker {
            Some(why) => amend.tip(why),
            None if self.amend => amend.tip("Stop amending: commit as a new commit"),
            None => amend.tip("Amend the last commit instead of adding one"),
        }
        .draw(out, atlas, theme);

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
        // Git's stderr can run to many lines, and the first is often ssh
        // talking about a key file; the line Git itself marks as the error
        // names it. The whole of it is in the tooltip.
        let first = headline(&self.note);
        layout::push_ui_text(
            out,
            atlas,
            Viewport {
                x: rect.x + lead,
                width: (rect.width - lead).max(0.0),
                ..rect
            },
            &first,
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
            // The panel's own shape, sketched, while Git is first asked:
            // the branch, the message and Commit, then a few rows.
            for rect in [g.branch, g.message, g.commit, g.amend] {
                layout::push_skeleton_row(out, rect, 0.0, (1.0, 0.0), false, theme);
            }
            let mut row_y = g.list.y + 4.0;
            for widths in [(0.35, 0.0), (0.6, 0.0), (0.5, 0.0), (0.7, 0.0)] {
                layout::push_skeleton_row(
                    out,
                    Viewport {
                        x: g.list.x + 24.0,
                        y: row_y,
                        width: (g.list.width - 48.0).max(0.0),
                        height: ROW,
                    },
                    16.0,
                    widths,
                    false,
                    theme,
                );
                row_y += ROW;
            }
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

/// The diff view's cursor line and the lines selected with it.
pub struct DiffMarks {
    pub cursor: usize,
    pub selection: std::ops::RangeInclusive<usize>,
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
        let marks = DiffMarks {
            cursor: self.diff_cursor,
            selection: self.diff_selection(),
        };
        draw_diff_lines(
            &self.diff.lines,
            self.diff_scroll,
            empty,
            &action,
            Some(&marks),
            atlas,
            rect,
            theme,
            out,
        );
    }
}

/// Diff lines in a column: old and new line numbers in a gutter, additions
/// and removals on their own bands, and hunk headers as separators. `action`
/// names the button a hunk header carries, if any, and `marks` the cursor
/// and selected lines where the diff has them. Shared by Source Control and
/// the review of a change proposed by Claude.
#[allow(clippy::too_many_arguments)]
pub fn draw_diff_lines(
    lines: &[git::DiffLine],
    scroll: usize,
    empty: &str,
    action: &dyn Fn(usize) -> Option<&'static str>,
    marks: Option<&DiffMarks>,
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
        if let Some(marks) = marks {
            let index = scroll + row;
            // The selection over the change's own band, as the editor
            // draws one; a lone cursor is the current line.
            if marks.selection.start() != marks.selection.end() && marks.selection.contains(&index)
            {
                layout::push_rect(
                    out,
                    atlas,
                    [rect.x, y],
                    [rect.width, DIFF_LINE],
                    theme.selection,
                );
            } else if index == marks.cursor {
                layout::push_rect(
                    out,
                    atlas,
                    [rect.x, y],
                    [rect.width, DIFF_LINE],
                    theme.current_line,
                );
            }
            if index == marks.cursor {
                layout::push_rect(out, atlas, [rect.x, y], [2.0, DIFF_LINE], theme.cursor);
            }
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
                Button::new(button)
                    .label(label)
                    .icon(if label == "Stage" {
                        icons::ADD
                    } else {
                        icons::REMOVE
                    })
                    .radius(layout::UI_RADIUS_SM)
                    .tip(if label == "Stage" {
                        "Stage this hunk (Space stages the hunk or the selected lines)"
                    } else {
                        "Unstage this hunk (Space unstages the hunk or the selected lines)"
                    })
                    .draw(out, atlas, theme);
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

/// The line of Git's output that says what went wrong, without its
/// `fatal:` or `error:`, and capitalised: what a one-line note shows.
pub fn headline(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("fatal:") || l.starts_with("error:"))
        .or_else(|| text.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("");
    let bare = line
        .strip_prefix("fatal:")
        .or_else(|| line.strip_prefix("error:"))
        .unwrap_or(line)
        .trim();
    let mut chars = bare.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
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
            head_message: Some("plant the beans".into()),
            head_pushed: false,
        };
        let mut panel = Panel::idle(PathBuf::from("/tmp"), Some(snapshot));
        panel.note = String::new();
        panel
    }

    #[test]
    fn the_note_names_the_operation_and_the_status_line_says_the_slow_ones() {
        // Each start runs Git on /tmp on a worker, which fails there; the
        // note is set before it is asked.
        let mut committing = panel(vec![]);
        committing.start(Operation::Commit("plant".into(), false));
        assert_eq!(committing.note, "Committing…");
        assert_eq!(committing.working(), Some("Committing…"));
        let mut amending = panel(vec![]);
        amending.start(Operation::Commit("plant".into(), true));
        assert_eq!(amending.note, "Amending…");
        let mut pushing = panel(vec![]);
        pushing.start(Operation::Remote(git::Remote::Push, None));
        assert_eq!(pushing.working(), Some("Pushing…"));
        // A refresh is the panel's own: named on the panel, quiet in the
        // status line, and "Reading repository" the first time.
        let mut refreshing = panel(vec![]);
        refreshing.start(Operation::Refresh);
        assert_eq!(refreshing.note, "Refreshing…");
        assert_eq!(refreshing.working(), None);
        let mut first = Panel::idle(PathBuf::from("/tmp"), None);
        first.start(Operation::Refresh);
        assert_eq!(first.note, "Reading repository…");
        assert_eq!(first.working(), None);
        assert!(first.busy());
    }

    #[test]
    fn diff_lines_know_their_hunk_and_place_under_it() {
        let mut panel = panel(vec![change("garden.txt", b'M', b'M')]);
        panel.diff = git::Diff::parse(
            "STAGED\n\ndiff --git a/garden.txt b/garden.txt\n--- a/garden.txt\n+++ b/garden.txt\n\
             @@ -1,2 +1,3 @@\n beans\n+carrots\n peas\n\
             WORKING TREE\n\ndiff --git a/garden.txt b/garden.txt\n--- a/garden.txt\n+++ b/garden.txt\n\
             @@ -2,2 +2,2 @@ fn plant\n-peas\n+leeks\n\\ No newline at end of file\n",
        );
        let owners = panel.line_owners();
        let kinds: Vec<_> = panel.diff.lines.iter().map(|l| l.kind).collect();
        use git::DiffKind::*;
        assert_eq!(
            kinds,
            [
                Section, Hunk, Context, Added, Context, Section, Hunk, Removed, Added, Note
            ]
        );
        assert_eq!(
            owners,
            [
                None,
                None,
                Some((0, 0)),
                Some((0, 1)),
                Some((0, 2)),
                None,
                None,
                Some((1, 0)),
                Some((1, 1)),
                Some((1, 2)),
            ]
        );

        // Shift-Down from the addition selects into the next section; the
        // view scrolls to keep the cursor in it.
        panel.set_diff_cursor(3, false);
        panel.move_diff_cursor(1, true, 3);
        panel.move_diff_cursor(1, true, 3);
        assert_eq!(panel.diff_selection(), 3..=5);
        assert_eq!(panel.diff_scroll, 3);
        panel.move_diff_cursor(-9, false, 3);
        assert_eq!(panel.diff_selection(), 0..=0);
        assert_eq!(panel.diff_scroll, 0);
        panel.move_diff_cursor(99, false, 3);
        assert_eq!(panel.diff_cursor, 9);
    }

    #[test]
    fn a_note_leads_with_the_line_git_marks_as_the_error() {
        let said = "Load key \"/tmp/key.pub\": invalid format\ngit@github.com: Permission denied (publickey).\nfatal: Could not read from remote repository.\n\nPlease make sure you have the correct access rights";
        assert_eq!(headline(said), "Could not read from remote repository.");
        assert_eq!(headline("nothing to commit"), "Nothing to commit");
        assert_eq!(headline(""), "");
    }

    #[test]
    fn the_message_completes_a_commit_type() {
        let at_end = |text: &str| commit_type_suggestion(text, text.len());
        assert_eq!(at_end("fe"), Some("at"));
        // Several types match: the first in the list wins.
        assert_eq!(at_end("re"), Some("factor"));
        assert_eq!(at_end("c"), Some("hore"));
        assert_eq!(at_end("feat"), None);
        assert_eq!(at_end("ci"), None);
        assert_eq!(at_end("feat:"), None);
        assert_eq!(at_end("fix("), None);
        assert_eq!(at_end("fix!"), None);
        assert_eq!(at_end("fe "), None);
        assert_eq!(at_end("Fe"), None);
        assert_eq!(at_end("FE"), None);
        assert_eq!(at_end("xyz"), None);
        assert_eq!(at_end(""), None);
        assert_eq!(commit_type_suggestion("fe", 1), None);
        assert_eq!(commit_type_suggestion("fe", 99), None);
        assert_eq!(at_end("feat: x\nfe"), None);
        assert_eq!(at_end("\nfe"), None);
    }

    #[test]
    fn the_subject_length_counts_characters_of_the_first_line() {
        assert_eq!(subject_len(""), 0);
        assert_eq!(subject_len("fix: typo"), 9);
        // One each, not their UTF-8 bytes.
        assert_eq!(subject_len("caf\u{e9}"), 4);
        assert_eq!(subject_len("ship it \u{1f680}"), 9);
        assert_eq!(
            subject_len("feat: x\n\nA body line that is much longer."),
            7
        );
        assert_eq!(subject_len("\nbody"), 0);
        assert_eq!(subject_len("a\r\nb"), 1);
    }

    #[test]
    fn the_message_splits_into_lines_and_bytes_map_to_them() {
        assert_eq!(message_lines(""), vec![0..0]);
        assert_eq!(message_lines("fix: x"), vec![0..6]);
        // Return at the end opens an empty line for the caret.
        assert_eq!(message_lines("fix: x\n"), vec![0..6, 7..7]);
        assert_eq!(message_lines("a\n\nbody"), vec![0..1, 2..2, 3..7]);
        let lines = message_lines("a\n\nbody");
        assert_eq!(message_line_of(&lines, 0), 0);
        // The end of a line, before its break, is still that line.
        assert_eq!(message_line_of(&lines, 1), 0);
        assert_eq!(message_line_of(&lines, 2), 1);
        assert_eq!(message_line_of(&lines, 3), 2);
        assert_eq!(message_line_of(&lines, 7), 2);
        assert_eq!(message_line_of(&lines, 99), 2);
        assert_eq!(message_rows(""), 1);
        assert_eq!(message_rows("fix: x"), 1);
        assert_eq!(message_rows("fix: x\n"), 2);
        assert_eq!(message_rows(&"line\n".repeat(20)), MESSAGE_MAX_LINES);
    }

    /// Past eight lines the field scrolls, as little as keeps the caret's
    /// line shown, and never leaves blank rows under the last line.
    #[test]
    fn the_message_scrolls_to_keep_the_caret_in_view() {
        assert_eq!(message_top(0, 0, 1, 1), 0);
        assert_eq!(message_top(0, 7, 8, 12), 0);
        assert_eq!(message_top(0, 8, 8, 12), 1);
        assert_eq!(message_top(0, 11, 8, 12), 4);
        assert_eq!(message_top(4, 6, 8, 12), 4);
        assert_eq!(message_top(4, 2, 8, 12), 2);
        // Lines deleted under a scrolled field.
        assert_eq!(message_top(4, 3, 8, 9), 1);
        assert_eq!(message_top(4, 0, 1, 1), 0);
        // Sideways, by measured width rather than a count of characters:
        // a caret at the end of a long line shows the end.
        assert_eq!(message_scroll_x(0.0, 50.0, 100.0, 50.0), 0.0);
        assert_eq!(message_scroll_x(0.0, 300.0, 100.0, 300.0), 201.0);
        assert_eq!(message_scroll_x(201.0, 250.0, 100.0, 300.0), 201.0);
        assert_eq!(message_scroll_x(201.0, 120.0, 100.0, 300.0), 95.0);
        // Back to a line that fits: its start shows too.
        assert_eq!(message_scroll_x(201.0, 60.0, 100.0, 300.0), 0.0);
        // The text got shorter: no blank run at the right.
        assert_eq!(message_scroll_x(201.0, 40.0, 100.0, 40.0), 0.0);
    }

    #[test]
    fn body_lines_past_seventy_two_characters_are_tinted_from_there() {
        let body = "b".repeat(80);
        let message = format!("feat: x\n\n{body}\nshort");
        let start = "feat: x\n\n".len();
        assert_eq!(
            past_body_limit(&message, 2),
            Some(start + BODY_LIMIT..start + 80)
        );
        assert_eq!(past_body_limit(&message, 1), None);
        assert_eq!(past_body_limit(&message, 3), None);
        assert_eq!(past_body_limit(&message, 9), None);
        // The subject has its own limit.
        let long_subject = "s".repeat(90);
        assert_eq!(past_body_limit(&long_subject, 0), None);
        // Exactly at the limit is fine; counted in characters, cut on one.
        assert_eq!(past_body_limit(&format!("s\n{}", "b".repeat(72)), 1), None);
        let wide = format!("s\n{}\u{e9}\u{e9}", "b".repeat(71));
        assert_eq!(past_body_limit(&wide, 1), Some(2 + 71 + 2..2 + 71 + 4));
    }

    /// A taller message moves everything under it by the same amount, and
    /// the panel sizes the field from what is written in it.
    #[test]
    fn the_message_field_grows_with_its_lines_and_pushes_the_list_down() {
        let one = Sidebar::new(column(), 1);
        let three = Sidebar::new(column(), 3);
        let grown = MESSAGE_LINE * 2.0;
        assert_eq!(three.message.y, one.message.y);
        assert_eq!(three.message.height, one.message.height + grown);
        assert_eq!(three.commit.y, one.commit.y + grown);
        assert_eq!(three.amend.y, one.amend.y + grown);
        assert_eq!(three.list.y, one.list.y + grown);
        assert_eq!(three.list.height, one.list.height - grown);
        assert_eq!(three.note.y, one.note.y);
        let mut p = panel(Vec::new());
        assert_eq!(p.sidebar(column()).message.height, one.message.height);
        p.message.insert("feat: x\n\nbody");
        assert_eq!(p.sidebar(column()).message.height, three.message.height);
        p.message.insert(&"\nmore".repeat(20));
        assert_eq!(
            p.sidebar(column()).message.height,
            Sidebar::new(column(), MESSAGE_MAX_LINES).message.height
        );
    }

    #[test]
    fn a_body_straight_under_the_subject_is_flagged() {
        assert!(!body_needs_blank_line(""));
        assert!(!body_needs_blank_line("feat: x"));
        assert!(!body_needs_blank_line("feat: x\n"));
        assert!(!body_needs_blank_line("feat: x\n\nbody"));
        assert!(!body_needs_blank_line("feat: x\n  \nbody"));
        assert!(body_needs_blank_line("feat: x\nbody"));
    }

    /// Amend is offered only for a HEAD that is still this branch's own,
    /// rewords with nothing staged, and takes back the message it put in.
    #[test]
    fn amend_is_blocked_with_a_reason_and_rewords_with_nothing_staged() {
        let mut p = panel(Vec::new());
        assert_eq!(p.amend_state(), "off");
        assert!(!p.can_commit());
        assert_eq!(p.commit_blocker(), Some("Stage a change to commit it"));

        p.set_amend(true);
        assert_eq!(p.amend_state(), "on");
        assert_eq!(p.message.rope.to_string(), "plant the beans");
        assert!(p.can_commit(), "a reword needs nothing staged");
        assert_eq!(p.commit_blocker(), None);
        // Unedited, it goes again with Amend.
        p.set_amend(false);
        assert_eq!(p.message.rope.to_string(), "");
        // Edited, it stays.
        p.set_amend(true);
        p.message.insert(" and water them");
        p.set_amend(false);
        assert_eq!(p.message.rope.to_string(), "plant the beans and water them");
        // A message already there is not replaced.
        p.set_amend(true);
        assert_eq!(p.message.rope.to_string(), "plant the beans and water them");
        p.message = Buffer::new();
        assert_eq!(p.commit_blocker(), Some("Write a commit message first"));
        assert!(!p.can_commit());
        p.set_amend(false);

        let snapshot = p.snapshot.as_mut().unwrap();
        snapshot.head_pushed = true;
        assert_eq!(
            p.amend_blocker(),
            Some("Already pushed: amending would rewrite a published commit")
        );
        assert_eq!(p.amend_state(), "blocked");
        p.set_amend(true);
        assert!(!p.amend, "a blocked toggle stays off");

        let snapshot = p.snapshot.as_mut().unwrap();
        snapshot.head_pushed = false;
        snapshot.in_progress = Some(git::InProgress::Merge);
        assert_eq!(
            p.amend_blocker(),
            Some("A merge is in progress: finish or abort it first")
        );

        let snapshot = p.snapshot.as_mut().unwrap();
        snapshot.in_progress = None;
        snapshot.head_message = None;
        assert_eq!(p.amend_blocker(), Some("Nothing to amend: no commits yet"));
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
        let g = Sidebar::new(column(), 1);
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
        for (height, lines) in [180.0, 320.0, 700.0, 1400.0]
            .into_iter()
            .flat_map(|h| [1, 3, MESSAGE_MAX_LINES].map(|n| (h, n)))
        {
            let column = Viewport { height, ..column() };
            let g = Sidebar::new(column, lines);
            for (name, rect) in [
                ("repo", g.repo),
                ("branch", g.branch),
                ("refresh", g.refresh),
                ("message", g.message),
                ("commit", g.commit),
                ("amend", g.amend),
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
            assert!(g.commit.x + g.commit.width <= g.amend.x);
            assert_eq!(g.commit.y, g.amend.y);
            // The message grows down and pushes Commit and the list with it.
            assert_eq!(
                g.message.height,
                layout::UI_CONTROL + MESSAGE_LINE * (lines - 1) as f32
            );
            assert!(g.message.y + g.message.height < g.commit.y);
            assert!(g.commit.y + g.commit.height < g.list.y);
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
        let g = Sidebar::new(column(), 1);
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
        let g = Sidebar::new(
            Viewport {
                height: 300.0,
                ..column()
            },
            1,
        );
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
