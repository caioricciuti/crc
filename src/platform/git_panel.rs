//! Native local source-control panel; Git processes run on workers.
use crate::project::git::{self, Change, Snapshot};
use crate::render::{
    font::Atlas,
    layout::{self, Theme, Viewport},
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
    StageHunk(Change, git::Hunk),
    Commit(String),
    Switch(String),
    CreateBranch(String),
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

pub struct Panel {
    directory: PathBuf,
    pub snapshot: Option<Snapshot>,
    pub selected: usize,
    pub list_scroll: usize,
    pub diff_scroll: usize,
    pub message: Buffer,
    pub note: String,
    /// Whether the editor column is showing the selected change instead of
    /// the active document.
    pub showing_diff: bool,
    diff: git::Diff,
    rx: Option<Receiver<Result<Reply, String>>>,
    submitted: Option<String>,
    /// When the worker last answered. The project watcher reports the
    /// repository changes the panel's own operations make; those are
    /// already reflected, so a refresh right after one is skipped.
    finished_at: Option<std::time::Instant>,
    /// A branch or remote command's outcome, success or failure, waiting
    /// for the window to show it in the status line.
    announcement: Option<String>,
    /// Whether the operation in flight is one that announces.
    announces: bool,
    /// A path to mark resolved once the worker is free: Mark Resolved saves
    /// first, and the save starts a refresh of its own.
    resolve_queued: Option<PathBuf>,
    /// Counts the worker's answers, so a view derived from the snapshot
    /// knows when to look again.
    generation: u64,
}

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
#[derive(Clone, Copy)]
pub struct Sidebar {
    pub branch: Viewport,
    pub refresh: Viewport,
    pub message: Viewport,
    pub commit: Viewport,
    pub list: Viewport,
    pub note: Viewport,
}

impl Sidebar {
    pub fn new(column: Viewport) -> Self {
        let x = column.x + 10.0;
        let width = (column.width - 20.0).max(0.0);
        let top = column.y + layout::SIDEBAR_HEADER_HEIGHT;
        let note_height = 20.0;
        let note = Viewport {
            x,
            y: column.y + column.height - note_height - 6.0,
            width,
            height: note_height,
        };
        let branch = Viewport {
            x,
            y: top,
            width: (width - 78.0).max(0.0),
            height: 22.0,
        };
        let message = Viewport {
            x,
            y: top + 28.0,
            width,
            height: 30.0,
        };
        let commit = Viewport {
            x,
            y: top + 64.0,
            width,
            height: 28.0,
        };
        Self {
            branch,
            refresh: Viewport {
                x: column.x + column.width - 78.0,
                y: top,
                width: 68.0,
                height: 22.0,
            },
            message,
            commit,
            list: Viewport {
                x: column.x,
                y: top + 102.0,
                width: column.width,
                height: (note.y - (top + 102.0) - 6.0).max(0.0),
            },
            note,
        }
    }

    /// The staging control at the trailing edge of a file row.
    pub fn toggle(&self, row: Viewport) -> Viewport {
        Viewport {
            x: row.x + row.width - 30.0,
            y: row.y + 3.0,
            width: 22.0,
            height: ROW - 6.0,
        }
    }
}

impl Panel {
    pub fn new(directory: PathBuf) -> Self {
        let mut panel = Self {
            directory,
            snapshot: None,
            selected: 0,
            list_scroll: 0,
            diff_scroll: 0,
            message: Buffer::new(),
            note: "Reading repository…".into(),
            showing_diff: false,
            diff: git::Diff::default(),
            rx: None,
            submitted: None,
            finished_at: None,
            announcement: None,
            announces: false,
            resolve_queued: None,
            generation: 0,
        };
        panel.refresh();
        panel
    }
    /// Whether the worker answered within the last moment, which is when
    /// a repository change on disk is the panel's own doing.
    pub fn settled_recently(&self) -> bool {
        self.finished_at
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(1500))
    }

    /// How many times the worker has answered.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn busy(&self) -> bool {
        self.rx.is_some()
    }
    pub fn branch(&self) -> String {
        self.snapshot
            .as_ref()
            .map(|s| s.branch.split("...").next().unwrap_or(&s.branch).to_owned())
            .unwrap_or_else(|| "Git".into())
    }
    /// The branch and how far it is from its upstream: `main ↑2 ↓1`.
    pub fn branch_status(&self) -> String {
        let Some(snapshot) = &self.snapshot else {
            return String::new();
        };
        let header = &snapshot.branch;
        let mut out = self.branch();
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
    /// Whether `path`, absolute, is unmerged as of the last status.
    pub fn is_conflicted(&self, path: &std::path::Path) -> bool {
        self.conflicted_change(path).is_some()
    }
    fn conflicted_change(&self, path: &std::path::Path) -> Option<&Change> {
        let snapshot = self.snapshot.as_ref()?;
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let root = std::fs::canonicalize(&snapshot.root).unwrap_or_else(|_| snapshot.root.clone());
        let relative = path.strip_prefix(&root).ok()?;
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
            self.resolve_queued = Some(path);
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
        !self.busy() && self.selected_change().is_some_and(Change::unstaged)
    }
    pub fn can_unstage(&self) -> bool {
        !self.busy() && self.selected_change().is_some_and(Change::staged)
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
    pub fn stage_index(&mut self, index: usize, staged: bool) {
        if self.busy() {
            return;
        }
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
        !self.busy()
            && self
                .snapshot
                .as_ref()
                .is_some_and(|s| s.changes.iter().any(Change::staged))
            && !self.message.rope.to_string().trim().is_empty()
    }
    pub fn refresh(&mut self) {
        self.start(Operation::Refresh);
    }
    pub fn select(&mut self, index: usize) {
        if let Some(change) = self.snapshot.as_ref().and_then(|s| s.changes.get(index)) {
            self.start(Operation::Select(change.path.clone()));
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
        if self.busy() {
            return;
        }
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
        if self.can_commit() {
            self.start(Operation::Commit(self.message.rope.to_string()));
        }
    }
    fn start(&mut self, operation: Operation) {
        if self.busy() {
            return;
        }
        self.submitted = if let Operation::Commit(message) = &operation {
            Some(message.clone())
        } else {
            None
        };
        self.announces = matches!(
            operation,
            Operation::Switch(_)
                | Operation::CreateBranch(_)
                | Operation::Remote(..)
                | Operation::Resolve(_)
        );
        let directory = self.directory.clone();
        let previous = self.selected_change().map(|c| c.path.clone());
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.note = "Working…".into();
        std::thread::spawn(move || {
            let result = (|| {
                let initial = git::snapshot(&directory)?;
                let mut selected_path = previous;
                let mut committed = false;
                let mut done = None;
                let error = match operation {
                    Operation::Switch(name) => match git::switch(&initial.root, &name) {
                        Ok(()) => {
                            done = Some(format!("switched to {name}"));
                            None
                        }
                        Err(error) => Some(error),
                    },
                    Operation::CreateBranch(name) => {
                        match git::create_branch(&initial.root, &name) {
                            Ok(()) => {
                                done = Some(format!("created and switched to {name}"));
                                None
                            }
                            Err(error) => Some(error),
                        }
                    }
                    Operation::Remote(what, sock) => {
                        match git::remote(&initial.root, what, sock.as_deref()) {
                            Ok(said) => {
                                let verb = match what {
                                    git::Remote::Fetch => "fetched",
                                    git::Remote::Pull => "pulled",
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
                    Operation::Stage(change, stage) => {
                        git::stage(&initial.root, &change, stage).err()
                    }
                    Operation::Resolve(change) => match git::stage(&initial.root, &change, true) {
                        Ok(()) => {
                            done = Some(format!("marked {} resolved", change.label()));
                            None
                        }
                        Err(error) => Some(error),
                    },
                    Operation::StageHunk(change, hunk) => {
                        git::stage_hunk(&initial.root, &change, &hunk).err()
                    }
                    Operation::Commit(message) => match git::commit(&initial.root, &message) {
                        Ok(()) => {
                            committed = true;
                            None
                        }
                        Err(error) => Some(error),
                    },
                };
                let snapshot = git::snapshot(&initial.root)?;
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
        let Some(rx) = &self.rx else {
            return false;
        };
        let reply = match rx.try_recv() {
            Ok(reply) => reply,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => Err("Git worker stopped".into()),
        };
        self.rx = None;
        self.generation += 1;
        self.finished_at = Some(std::time::Instant::now());
        match reply {
            Ok(reply) => {
                // Branch and remote commands announce, failures included:
                // they run from the menu, with the panel usually closed.
                if self.announces {
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
                if reply.committed
                    && self.submitted.as_deref() == Some(self.message.rope.to_string().as_str())
                {
                    self.message = Buffer::new();
                }
                // A failure is shown where the diff would be, as a note rather
                // than as diff lines, so git's stderr is never mistaken for
                // part of the change.
                if let Some(error) = &reply.error {
                    let mut failure = git::Diff::default();
                    for line in format!("Git command failed\n\n{error}").lines() {
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
                self.note = reply.error.unwrap_or_else(|| {
                    if reply.committed {
                        "Commit created".into()
                    } else {
                        String::new()
                    }
                });
            }
            Err(error) => {
                self.note = error;
            }
        }
        if let Some(path) = self.resolve_queued.take() {
            self.mark_resolved(path);
        }
        true
    }

    /// Wheel scrolling over the change list or over the diff.
    pub fn scroll(&mut self, delta: isize, over_list: bool, g: Sidebar, diff: Viewport) {
        if over_list {
            let visible = self.rows(g).len();
            self.list_scroll = self
                .list_scroll
                .saturating_add_signed(delta)
                .min(self.entries().len().saturating_sub(visible.max(1)));
        } else {
            let visible = (diff.height / DIFF_LINE).max(1.0) as usize;
            self.diff_scroll = self
                .diff_scroll
                .saturating_add_signed(delta)
                .min(self.diff.lines.len().saturating_sub(visible));
        }
    }

    /// Source control down the sidebar column: branch, commit box, then the
    /// changes grouped by whether they are staged, each row carrying the
    /// control that stages it.
    pub fn draw_sidebar(
        &self,
        atlas: &mut Atlas,
        column: Viewport,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) {
        let g = Sidebar::new(column);
        layout::push_ui_text(out, atlas, g.branch, &self.branch(), theme.text);
        layout::push_rounded_rect(out, g.refresh, 5.0, theme.tab_hover);
        layout::push_ui_text_centered(
            out,
            atlas,
            g.refresh,
            if self.busy() { "…" } else { "Refresh" },
            if self.busy() {
                theme.gutter_text
            } else {
                theme.status_text
            },
        );

        layout::push_rounded_rect(out, g.message, 5.0, theme.tab_active);
        let full = self.message.rope.to_string();
        let (shown, start) = layout::ui_input_window(&full, self.message.cursor());
        let text_rect = Viewport {
            x: g.message.x + 9.0,
            width: (g.message.width - 18.0).max(0.0),
            ..g.message
        };
        layout::push_ui_text(
            out,
            atlas,
            text_rect,
            if shown.is_empty() { "Message" } else { &shown },
            if shown.is_empty() {
                theme.status_text
            } else {
                theme.text
            },
        );
        let caret = layout::ui_caret_x(atlas, &shown, self.message.cursor().saturating_sub(start))
            .min(text_rect.width);
        layout::push_rect(
            out,
            atlas,
            [text_rect.x + caret, g.message.y + 6.0],
            [1.0, 18.0],
            theme.cursor,
        );

        let staged = self.staged_count();
        let can_commit = self.can_commit();
        layout::push_rounded_rect(
            out,
            g.commit,
            5.0,
            if can_commit {
                theme.palette_selected
            } else {
                theme.tab_hover
            },
        );
        // The button says what it will commit. "Commit" alone next to a list
        // that mixes staged and unstaged files does not.
        let commit_label = match staged {
            0 => "Commit".to_owned(),
            1 => "Commit 1 file".to_owned(),
            n => format!("Commit {n} files"),
        };
        layout::push_ui_text_centered(
            out,
            atlas,
            g.commit,
            &commit_label,
            if can_commit {
                theme.accent
            } else {
                theme.gutter_text
            },
        );

        let rows = self.rows(g);
        if rows.is_empty() && !self.busy() {
            layout::push_ui_text(
                out,
                atlas,
                Viewport {
                    x: g.list.x + 10.0,
                    y: g.list.y + 4.0,
                    width: (g.list.width - 20.0).max(0.0),
                    height: ROW,
                },
                "No changes",
                theme.status_text,
            );
        }
        for (entry, rect) in rows {
            match entry {
                Entry::Section { group, count } => {
                    layout::push_ui_text(
                        out,
                        atlas,
                        Viewport {
                            x: rect.x + 10.0,
                            y: rect.y + 4.0,
                            width: (rect.width - 20.0).max(0.0),
                            height: rect.height - 4.0,
                        },
                        &format!("{}  {count}", group.title()),
                        if group == Group::Conflicts {
                            theme.diff_removed
                        } else {
                            theme.status_text
                        },
                    );
                }
                Entry::File { change, group } => {
                    self.draw_file_row(atlas, g, rect, change, group, theme, out);
                }
            }
        }
        layout::push_ui_text(out, atlas, g.note, &self.note, theme.status_text);
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
        if change == self.selected && self.showing_diff {
            layout::push_rounded_rect(
                out,
                Viewport {
                    x: rect.x + 4.0,
                    y: rect.y + 1.0,
                    width: (rect.width - 8.0).max(0.0),
                    height: rect.height - 2.0,
                },
                4.0,
                theme.palette_selected,
            );
        }
        // One status letter, coloured, instead of a whole second line reading
        // "M Unstaged" under every file.
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
            _ => theme.syn_number,
        };
        layout::push_ui_text(
            out,
            atlas,
            Viewport {
                x: rect.x + 10.0,
                y: rect.y,
                width: 14.0,
                height: rect.height,
            },
            &String::from_utf8_lossy(&[code]),
            colour,
        );
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
        let toggle = g.toggle(rect);
        let name_rect = Viewport {
            x: rect.x + 28.0,
            y: rect.y,
            width: (if conflicted {
                rect.x + rect.width - 8.0
            } else {
                toggle.x
            } - rect.x
                - 34.0)
                .max(0.0),
            height: rect.height,
        };
        let name_width = layout::ui_text_width(atlas, &name);
        layout::push_ui_text(out, atlas, name_rect, &name, theme.text);
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
        // A conflicted file is staged by Mark Resolved above its text, once
        // no markers are left in it; a plus here would stage the markers.
        if conflicted {
            return;
        }
        // Stage/unstage sits on the row it acts on. The old pair lived at the
        // bottom of the panel, an empty column away from the list.
        layout::push_rounded_rect(out, toggle, 4.0, theme.tab_hover);
        layout::push_ui_text_centered(
            out,
            atlas,
            toggle,
            if staged { "−" } else { "+" },
            if self.busy() {
                theme.gutter_text
            } else {
                theme.status_text
            },
        );
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
        let mut width = 0;
        let shown: String = line
            .text
            .chars()
            .take_while(|ch| {
                width += crate::render::font::display_width(*ch);
                width <= columns
            })
            .collect();
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
        Panel {
            directory: PathBuf::from("/tmp"),
            snapshot: Some(Snapshot {
                root: PathBuf::from("/tmp"),
                branch: "main".into(),
                changes,
                in_progress: None,
            }),
            selected: 0,
            list_scroll: 0,
            diff_scroll: 0,
            message: Buffer::new(),
            note: String::new(),
            showing_diff: false,
            diff: git::Diff::default(),
            rx: None,
            submitted: None,
            finished_at: None,
            announcement: None,
            announces: false,
            resolve_queued: None,
            generation: 0,
        }
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
