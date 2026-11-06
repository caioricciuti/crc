//! The terminal panel: sessions under the editor, Claude Code among them.
//!
//! The emulator and the pseudo-terminal are in [`crate::term`]; this is the
//! panel the window shows. A header of session tabs, then the screen of the
//! active one, drawn cell by cell through the same glyph pipeline as the
//! editor. Rows are as tall as a glyph cell rather than the editor's looser
//! lines, so box drawing joins up: `claude` draws its input in a box.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::project::icons;
use crate::render::font::Atlas;
use crate::render::layout::split::{Dir, Divider, Edge, Node, Zone, zone_at, zone_rect};
use crate::render::layout::{self, Button, Theme, Tone, Viewport};
use crate::render::metal::GlyphInstance;
use crate::term::pty::Session;
use crate::term::{self, BOLD, Cell, Color, DIM, HIDDEN, INVERSE, STRIKE, UNDERLINE, WIDE_TAIL};

/// The session tabs across the top of the panel.
pub const HEADER: f32 = 30.0;
/// Space between the panel's edges and the screen.
const PAD: f32 = 8.0;
/// Height when first opened.
pub const DEFAULT_HEIGHT: f32 = 320.0;
/// The shortest the panel may be dragged: the header and a few rows.
pub const MIN_HEIGHT: f32 = HEADER + 80.0;

/// What a tab runs, kept so an exited one can be started again.
#[derive(Clone, Debug)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    /// Inherited variables the program must not see.
    pub unset: Vec<String>,
}

/// Which of the editor's inherited variable `names` a new session must not
/// see. Opened from an agent's shell (and kept by Restart to Update, which
/// `open`s it with the same environment), the editor carries that shell's
/// settings for a pipe: `NO_COLOR` turns colour off in `claude`, `ls` and
/// the rest, `PAGER=cat` stops `git log` paging, `SHLVL` counts the agent's
/// shells. Claude Code's and Codex's own markers make a `claude` here think
/// it runs under them. A login shell sets back whatever its profile sets.
pub fn inherited_unset(names: impl IntoIterator<Item = String>) -> Vec<String> {
    const PLAIN: [&str; 7] = [
        "NO_COLOR",
        "FORCE_COLOR",
        "PAGER",
        "GIT_PAGER",
        "GH_PAGER",
        "SHLVL",
        "AI_AGENT",
    ];
    names
        .into_iter()
        .filter(|name| {
            PLAIN.contains(&name.as_str())
                || name == "CLAUDECODE"
                || name.starts_with("CLAUDE_CODE_")
                || name.starts_with("CODEX_")
        })
        .collect()
}

pub struct Tab {
    pub session: Session,
    pub title: String,
    /// Runs Claude Code, connected to this window.
    pub claude: bool,
    pub launch: Launch,
    /// What the program last asked to be told, until someone types into
    /// the tab: the notification's text, empty for the bell.
    pub attention: Option<String>,
    /// When it last printed something.
    pub last_output: Option<std::time::Instant>,
    /// Its agent review session's folder, in its `CRC_SESSION_DIR`.
    pub review: PathBuf,
    /// How far into the session's events the window has read.
    pub events_read: u64,
    /// Every shell command its agent ran, and how each ended.
    pub receipt: crate::project::receipt::Receipt,
    /// Files the session has checkpoints for.
    pub review_files: usize,
    /// Its folder, resolved, for the workspace it belongs to when it was
    /// not started in one.
    pub folder: PathBuf,
    /// The workspace it was started in, by [`crate::platform::agents::Workspace::id`].
    pub workspace: Option<String>,
    /// Held while the tab lives: the thread that watches its agent's hook
    /// events stops when this goes.
    pub alive: std::sync::Arc<()>,
    /// The `crc --hold` helper running it, when one does (see
    /// [`crate::term::hold`]): the program outlives the window.
    pub held: Option<String>,
    /// What its program says it is doing (OSC 7501, which Claude Code
    /// speaks), copied from the terminal as output arrives. Outranks
    /// the guesses from output and hooks while it is there.
    pub status: Option<crate::term::status::Status>,
    /// Where its program is now, resolved: the shell's working directory
    /// after its last `cd`, read by [`Tab::read_cwd`]; where it started
    /// until then.
    pub cwd: PathBuf,
    /// Its program's pid, once found.
    pub program_pid: Option<i32>,
    /// The ssh session its shell is running, while it runs one.
    pub remote: Option<crate::platform::remote::Remote>,
    /// Whether its program was last told it has the keyboard, for focus
    /// reports (mode 1004) on a change only.
    pub told_focus: bool,
    /// When something was last typed into it; see [`Tab::heeds_bell`].
    pub last_typed: Option<Instant>,
    /// Words were typed into its prompt and not sent: auto-continue
    /// leaves it alone, since its word would go after them. See
    /// [`draft_after`].
    pub draft_pending: bool,
    /// When its process was last read for where it is and whether it runs
    /// ssh; see [`Tab::probe`].
    pub last_probe: Option<Instant>,
    /// It printed while a read was held back: one is owed.
    pub probe_owed: bool,
    /// What was shown of it when its output was last looked at; see
    /// [`Tab::face_changed`].
    pub seen: Option<Face>,
    /// Its agent's streak of temporary API errors, while auto-continue
    /// tries it again; see [`crate::platform::retry`].
    pub retry: Option<crate::platform::retry::Retry>,
    /// The cursor's line when Return was last typed into its agent: where
    /// the prompt it sent is, until its hook says so.
    pub last_return: Option<u64>,
    /// Where the prompts sent to its agent are, for the marks down the
    /// screen's right edge.
    pub marks: crate::term::marks::PromptMarks,
    /// Its wait has been told, by a notification or by showing: set when
    /// it starts waiting, cleared when it stops (see `notify::edge` in the
    /// window).
    pub notified: bool,
    /// What is shown of it where it is on screen, and what names it in a
    /// layout of panes.
    pub view: View,
}

/// What names a tab in a layout of panes, for as long as it lives and
/// wherever it goes among the tabs: a number no other tab ever has, so a
/// pane can never come to mean another session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PaneId(u64);

impl PaneId {
    /// One no tab has had.
    fn next() -> PaneId {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        PaneId(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// Selected text of a session, from the press to the pointer: line
/// numbers (see [`crate::term::Term::view_line`]) and column boundaries.
pub type Selection = ((u64, usize), (u64, usize));

/// What is shown of a session where it is on screen. Each tab has its
/// own, so scrolling or selecting in one pane never moves or clears
/// another.
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    /// What names its tab in a layout of panes.
    pub id: PaneId,
    /// The workspace its tab is in, by the window's name for it, as the
    /// window last said ([`Panel::regroup`]): a layout holds the tabs of
    /// one workspace.
    pub group: Option<String>,
    /// Lines scrolled back into history; 0 follows the output.
    pub back: usize,
    pub selection: Option<Selection>,
    /// A press in its screen is being dragged.
    pub selecting: bool,
    /// Where the drag last was, for scrolling the history under a pointer
    /// held past the screen's edge.
    pub drag_point: Option<(f32, f32)>,
}

impl View {
    /// A new tab's: following the output, nothing selected.
    pub fn new() -> View {
        View {
            id: PaneId::next(),
            group: None,
            back: 0,
            selection: None,
            selecting: false,
            drag_point: None,
        }
    }

    /// As it is when its tab comes on screen from the strip: at the
    /// output, nothing selected.
    pub fn reset(&mut self) {
        self.back = 0;
        self.selection = None;
        self.selecting = false;
        self.drag_point = None;
    }
}

impl Default for View {
    fn default() -> Self {
        View::new()
    }
}

/// A workspace's panes: its sessions on screen together, side by side or
/// stacked, and the one with the keyboard. Kept only while there are two
/// or more: a workspace with one session showing has none.
#[derive(Clone, Debug, PartialEq)]
pub struct Panes {
    /// The workspace whose tabs these are, as [`View::group`] names it.
    pub group: Option<String>,
    pub tree: Node<PaneId>,
    /// The pane that has the keyboard while these show, and gets it back
    /// when they show again.
    pub focus: PaneId,
}

/// A divider between two panes, held and dragged.
#[derive(Clone, Debug, PartialEq)]
pub struct DividerDrag {
    /// Whose panes it is among.
    pub group: Option<String>,
    /// Which divider, by its place in the tree (see [`Divider::id`]): good
    /// while the tree keeps its shape, and the drag ends when it does not.
    pub id: usize,
    /// How far from the middle of its gap it was pressed.
    pub grab: (f32, f32),
}

/// What a session is doing, as the Agents sidebar shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    /// It asked for something (a notification, the bell, an agent that
    /// finished its turn) and nobody has typed into it since.
    Waiting,
    /// It printed in the last few seconds.
    Working,
    Idle,
}

impl Activity {
    /// The word the sidebar and the switcher say it with.
    pub fn label(self) -> &'static str {
        match self {
            Activity::Waiting => "waiting",
            Activity::Working => "working",
            Activity::Idle => "idle",
        }
    }
}

impl Tab {
    /// What picks it again while it lives, its index moving as tabs open
    /// and close: its review folder, which is its own, or for one without
    /// (on a host) where its lifetime token lives.
    pub fn id(&self) -> PathBuf {
        if self.review.as_os_str().is_empty() {
            PathBuf::from(format!("tab:{:p}", std::sync::Arc::as_ptr(&self.alive)))
        } else {
            self.review.clone()
        }
    }

    /// Whether what changes on this Mac can be its agent's, for review:
    /// it has a review folder, and is not ssh to a host (a remote
    /// workspace's terminal, or a shell in ssh now), whose writes are
    /// there.
    pub fn tracks_review(&self) -> bool {
        !self.review.as_os_str().is_empty()
            && self.remote.is_none()
            && !crate::platform::remote::is_ssh_launch(&self.launch.program, &self.launch.args)
    }

    /// Reads whether its shell is running ssh now, and where to. Returns
    /// whether that changed.
    pub fn read_remote(&mut self) -> bool {
        let Some(pid) = self.find_program_pid() else {
            return false;
        };
        let target = crate::platform::remote::of_shell(pid);
        let now = match (target, &self.remote) {
            (Some(t), Some(r)) if t.destination == r.destination => return false,
            (Some(t), _) => Some(crate::platform::remote::Remote {
                destination: t.destination,
                user: t.user,
                host: t.host,
                port: t.port,
                since: std::time::Instant::now(),
            }),
            (None, None) => return false,
            (None, Some(_)) => None,
        };
        self.remote = now;
        true
    }

    fn find_program_pid(&mut self) -> Option<i32> {
        if self.program_pid.is_none() {
            self.program_pid = self.session.program_pid().or_else(|| {
                let dir = crate::term::hold::dir()?;
                crate::term::hold::program_pid(&dir, self.held.as_deref()?)
            });
        }
        self.program_pid
    }

    /// Reads where its program is now: one call into the kernel. Returns
    /// whether it moved.
    pub fn read_cwd(&mut self) -> bool {
        let Some(now) = self
            .find_program_pid()
            .and_then(crate::platform::process_cwd)
            .map(|p| crate::platform::canonical(&p))
        else {
            return false;
        };
        if now == self.cwd {
            return false;
        }
        self.cwd = now;
        true
    }

    pub fn activity(&self) -> Activity {
        use crate::term::status::State;
        match self.status.as_ref().map(|s| s.state) {
            Some(State::Working) => return Activity::Working,
            Some(State::Blocked | State::Done | State::Error) => return Activity::Waiting,
            Some(State::Idle) | None => {}
        }
        if self.attention.is_some() {
            Activity::Waiting
        } else if self
            .last_output
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(3))
        {
            Activity::Working
        } else {
            Activity::Idle
        }
    }

    /// Whether auto-continue has a try scheduled for it.
    pub fn retrying(&self) -> bool {
        self.retry.is_some_and(|r| r.pending())
    }

    /// How Home and the status line say what the tab is doing.
    pub fn state(&self) -> String {
        use crate::term::status::State;
        if self.retrying() {
            return match self.attention.as_deref() {
                Some(text) if !text.is_empty() => format!("retrying: {text}"),
                _ => "retrying".into(),
            };
        }
        if let Some(status) = &self.status {
            match status.state {
                State::Working => return "working".into(),
                State::Blocked | State::Done => {
                    return format!("waiting: {}", status.text());
                }
                State::Error => return format!("waiting: failed: {}", status.text()),
                State::Idle => {}
            }
        }
        match (&self.attention, self.activity()) {
            (Some(text), _) if text.is_empty() => "waiting: rang the bell".into(),
            (Some(text), _) => format!("waiting: {text}"),
            (None, Activity::Working) => "working".into(),
            (None, _) => "idle".into(),
        }
    }

    /// The title shown for it.
    pub fn name(&self) -> String {
        label(self)
    }

    /// What the sidebar, the inspector, Home and the tab strip show of it.
    pub fn face(&self) -> Face {
        Face {
            activity: self.activity(),
            attention: self.attention.is_some(),
            status: self.status.as_ref().map(|s| (s.state, s.text())),
            state: self.state(),
            name: self.name(),
        }
    }

    /// Whether what is shown of it changed since its output was last
    /// looked at, `was` its activity before its newest output: a title,
    /// a status or a ring arrives with the output, so what came before is
    /// what was remembered. Remembered again for the next look.
    pub fn face_changed(&mut self, was: Activity) -> bool {
        let face = self.face();
        let changed = was != face.activity || self.seen.as_ref() != Some(&face);
        self.seen = Some(face);
        changed
    }

    /// Whether its process may be read now (its cwd, its ssh): at most
    /// every [`PROBE_EVERY`], as output can wake the window hundreds of
    /// times a second. One held back is owed, at the time returned.
    pub fn probe(&mut self, now: Instant) -> When {
        match self.last_probe {
            Some(at) if now.saturating_duration_since(at) < PROBE_EVERY => {
                self.probe_owed = true;
                When::At(at + PROBE_EVERY)
            }
            _ => {
                self.last_probe = Some(now);
                self.probe_owed = false;
                When::Now
            }
        }
    }

    /// The title without the review count, for naming its session.
    pub fn base_name(&self) -> String {
        title_of(self)
    }

    /// [`Tab::base_name`] for a caller that holds its terminal locked.
    fn base_name_of(&self, term: &term::Term) -> String {
        titled(self, term)
    }

    /// Whether a bell rung at `now` asks for attention. One right after a
    /// keystroke is the program answering the key (a shell given a key it
    /// has nothing bound to, a completion with nothing to offer, a
    /// backspace at an empty line), not a call for you: it marks nothing.
    pub fn heeds_bell(&self, now: Instant) -> bool {
        !self
            .last_typed
            .is_some_and(|at| now.saturating_duration_since(at) < BELL_ECHO)
    }

    /// `bytes` typed into it by the person, a keystroke or a reply from a
    /// notification. Returns whether that answered something it asked.
    pub fn type_in(&mut self, bytes: &[u8]) -> bool {
        // Typing into it is the answer to what it asked. A result it
        // reported is seen now; a block stays until it says so.
        let mut answered = self.attention.take().is_some();
        let mut term = self.session.term.lock().unwrap_or_else(|e| e.into_inner());
        if term.status.acknowledge() {
            self.status = None;
            answered = true;
        }
        // Return sends an agent its prompt: the line it was sent from,
        // for the mark its hook asks for.
        if self.claude && bytes == b"\r" {
            self.last_return = Some(term.view_line(term.cursor().0, 0));
        }
        drop(term);
        self.session.write(bytes);
        self.last_typed = Some(Instant::now());
        self.draft_pending = draft_after(bytes, self.draft_pending);
        // The person answered: auto-continue leaves it to them.
        self.retry = None;
        answered
    }

    /// Whether the agent its hooks speak for is still what the keyboard
    /// reaches. The shell crc started is still its program (the kernel
    /// has the arguments it was started with) and runs nothing: the agent
    /// typed into it has left, and what is typed now goes to the shell.
    /// A program that took the shell's place (`-c claude` is exec'd) or a
    /// shell with something in front counts as the agent; one that cannot
    /// be read is not guessed at.
    pub fn agent_in_front(&mut self) -> bool {
        let Some(pid) = self.find_program_pid() else {
            return true;
        };
        agent_in_front_of(
            &self.launch.args,
            crate::platform::process_args(pid).as_deref(),
            &crate::platform::child_pids(pid),
        )
    }
}

/// [`Tab::agent_in_front`] from what the kernel says of the tab's program:
/// its arguments (`None` when it cannot be read) and its children.
pub fn agent_in_front_of(
    launch_args: &[String],
    args: Option<&[String]>,
    children: &[i32],
) -> bool {
    match args.and_then(|args| args.get(1..)) {
        Some(rest) if rest == launch_args => !children.is_empty(),
        _ => true,
    }
}

/// Whether the prompt holds words not sent after `bytes` were typed into
/// it, `pending` whether it did before. A Return or line feed sends them;
/// Esc, Ctrl-C and Ctrl-U clear the line. A paste is words; an escape
/// sequence (an arrow, a function key) moves without adding any, and a
/// control byte (a backspace, a tab) changes the line without ending it,
/// so both leave the answer as it was.
pub fn draft_after(bytes: &[u8], pending: bool) -> bool {
    match bytes {
        [] => pending,
        [0x1b] | [0x03] | [0x15] => false,
        _ if bytes.starts_with(b"\x1b[200~") => true,
        [0x1b, ..] => pending,
        [.., b'\r' | b'\n'] => false,
        _ if bytes.iter().all(|b| *b < 0x20 || *b == 0x7f) => pending,
        _ => true,
    }
}

/// How long after a keystroke a bell is taken as the program's answer to
/// it rather than a call for attention.
pub const BELL_ECHO: Duration = Duration::from_millis(300);

pub struct Panel {
    /// The folder workspaces a terminal left by `cd` since the window last
    /// looked: one left with no terminal goes.
    pub closed_in: Vec<String>,
    /// The tab showing was closed: the workspace it was started in, its
    /// folder (which name the workspace it was in) and its place, so the
    /// next tab shown is that workspace's, not whichever slid into place.
    pub closed_active: Option<(Option<String>, PathBuf, usize)>,
    pub open: bool,
    /// Whether typing goes to the terminal rather than the editor.
    pub focus: bool,
    pub height: f32,
    pub tabs: Vec<Tab>,
    /// The session with the keyboard: the one showing, or of several
    /// panes the focused one.
    pub active: usize,
    /// The tabs the header lists, when not all: in Agents mode, the
    /// selected task's.
    pub shown: Option<Vec<usize>>,
    /// The panes of each workspace that shows more than one session, one
    /// layout a workspace at most and no tab in two. The one showing is
    /// the one the active tab is in.
    pub layouts: Vec<Panes>,
    /// A divider between two panes, while it is dragged: the sessions
    /// keep their sizes until it is let go.
    pub divider_drag: Option<DividerDrag>,
    /// Redraws asked for by tabs not on screen, paced.
    pub pace: Pace,
    /// A press reported to a program that asked for the mouse: its tab,
    /// the button, and the cell (row, column) last reported, which motion
    /// and the release follow wherever the pointer goes.
    pub mouse_press: Option<(usize, term::mouse::Button, (usize, usize))>,
    /// The tab and cell the pointer was last reported over, so motion is
    /// reported only when it reaches another cell.
    pub mouse_cell: Option<(usize, (usize, usize))>,
    /// The link under the pointer while Command is held, and its tab:
    /// underlined, as what a Command-click there opens.
    pub link: Option<(usize, term::Link)>,
    /// What Command-clicks on links opened, under a self-test, which
    /// opens nothing.
    pub opened: Vec<String>,
    /// The ids of tabs closed while their wait was told, whose
    /// notifications are still to be taken back.
    pub withdrawn: Vec<String>,
    /// A tab pressed in the header, dragged along the strip once the
    /// pointer has moved far enough, or off it onto a pane.
    pub tab_drag: Option<TabDrag>,
    /// The tab a right-click's menu is about, by [`Tab::id`]: tabs can
    /// close and move while the menu is open, and its items act on that
    /// session or on none.
    pub menu_tab: Option<String>,
    /// How far the tab strip is scrolled, in points, while its tabs do
    /// not fit; see [`strip`].
    pub strip_scroll: f32,
    /// The wheel or a drag moved the strip since the active tab changed:
    /// it stays where it was put. Otherwise it keeps that tab in view.
    pub strip_free: bool,
    /// The active tab when the strip was last laid out for a frame, by
    /// the address of its lifetime token: another one is brought into view.
    pub strip_seen: usize,
}

/// A tab pressed in the header: a drag to another place among the tabs
/// listed once the pointer has left where it was pressed, and to a pane
/// once it has been taken under the strip.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TabDrag {
    pub tab: usize,
    pub start_x: f32,
    /// Where the pointer is now.
    pub x: f32,
    pub y: f32,
    /// Moved far enough to be a drag, not a click.
    pub active: bool,
    /// The session the press took the screen or a pane from, showing the
    /// tab in its place as a click does. A drop beside a pane gives it
    /// that place back first: the tab is added to what was showing.
    pub before: Option<PaneId>,
    /// Taken under the strip, on its way to a pane; level with the strip
    /// again it is on its way along it.
    pub pane: bool,
    /// Where it lands if let go where the pointer is, as the last move
    /// worked it out: the frame only draws it.
    pub drop: Option<PaneDrop>,
}

/// Where a tab dragged over the panes lands.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaneDrop {
    /// The session whose pane the pointer is over.
    pub pane: PaneId,
    /// Beside it, cutting its pane in two at that edge, or in its place.
    pub zone: Zone,
    /// What lights up: the room the tab gets of that pane.
    pub rect: Viewport,
}

/// How far the pointer moves from a press on a tab before it is a drag.
pub const TAB_DRAG_AFTER: f32 = 4.0;
/// How far under the strip a held tab is taken before it is on its way to
/// a pane.
pub const PANE_DROP_AFTER: f32 = 8.0;

/// Where the tab at `index` is once the one at `from` has moved to `to`,
/// the tabs between them each a place along. Every index kept across a
/// move goes through this, so each still names the session it named.
pub fn moved(index: usize, from: usize, to: usize) -> usize {
    if index == from {
        to
    } else if from < index && index <= to {
        index - 1
    } else if to <= index && index < from {
        index + 1
    } else {
        index
    }
}

impl Default for Panel {
    fn default() -> Self {
        Panel {
            closed_in: Vec::new(),
            closed_active: None,
            open: false,
            focus: false,
            height: DEFAULT_HEIGHT,
            tabs: Vec::new(),
            active: 0,
            shown: None,
            layouts: Vec::new(),
            divider_drag: None,
            pace: Pace::default(),
            mouse_press: None,
            mouse_cell: None,
            link: None,
            opened: Vec::new(),
            withdrawn: Vec::new(),
            tab_drag: None,
            menu_tab: None,
            strip_scroll: 0.0,
            strip_free: false,
            strip_seen: 0,
        }
    }
}

impl Panel {
    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    /// The tab whose [`Tab::id`] is `id`, while it lives.
    pub fn tab_by_id(&self, id: &str) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.id().as_os_str() == std::ffi::OsStr::new(id))
    }

    /// Whether keys should go to the terminal.
    pub fn has_keys(&self) -> bool {
        self.open && self.focus && !self.tabs.is_empty()
    }

    /// Whether the active session is one the header lists: not, in Agents
    /// mode, when the selected workspace has none left.
    pub fn showing(&self) -> bool {
        self.shown
            .as_ref()
            .is_none_or(|shown| shown.contains(&self.active))
    }

    /// Where the tab a layout calls `id` is among the tabs now.
    pub fn index_of(&self, id: PaneId) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.view.id == id)
    }

    /// The layout the tab `id` is a pane of, by its place among them.
    fn layout_of(&self, id: PaneId) -> Option<usize> {
        self.layouts
            .iter()
            .position(|panes| panes.tree.contains(&id))
    }

    /// The panes showing: the layout the active tab is in. None while one
    /// session shows alone.
    pub fn panes(&self) -> Option<&Panes> {
        let id = self.active_tab()?.view.id;
        self.layouts.iter().find(|panes| panes.tree.contains(&id))
    }

    /// Each session on screen and where its screen is, left before right
    /// and top before bottom: the active one, or every pane of its layout.
    pub fn screens(&self, screen: Viewport) -> Vec<(usize, Viewport)> {
        match self.panes() {
            Some(panes) => panes
                .tree
                .rects(screen, PANE_GAP)
                .into_iter()
                .filter_map(|(id, rect)| Some((self.index_of(*id)?, rect)))
                .collect(),
            None => vec![(self.active, screen)],
        }
    }

    /// Gives `tab` the keyboard where its workspace's panes have it: the
    /// pane it is in, or in place of the session in the pane that had the
    /// keyboard, which goes back to being a tab of the strip alone.
    fn place(&mut self, tab: usize) {
        let Some(shown) = self.tabs.get(tab) else {
            return;
        };
        let id = shown.view.id;
        let at = self.layout_of(id).or_else(|| {
            let at = self
                .layouts
                .iter()
                .position(|panes| panes.group == shown.view.group)?;
            let focus = self.layouts[at].focus;
            self.layouts[at].tree.replace(&focus, id).then_some(at)
        });
        if let Some(at) = at {
            self.layouts[at].focus = id;
        }
        self.active = tab;
    }

    /// Shows `tab` with the keyboard. Among its workspace's panes it is
    /// the pane it is in, as it was left, or it takes the place of the
    /// session in the pane that had the keyboard. A tab that was in no
    /// pane starts at its output with nothing selected.
    pub fn show(&mut self, tab: usize) {
        let Some(shown) = self.tabs.get(tab) else {
            return;
        };
        let kept = self.layout_of(shown.view.id).is_some();
        self.place(tab);
        if !kept {
            self.tabs[tab].view.reset();
        }
    }

    /// Cuts the pane with the keyboard in two, with `new` at its `edge`
    /// and the keyboard in it. Returns false, changing nothing, when
    /// `new` is the session showing, is another workspace's, is in a pane
    /// already, or the layout takes no more panes.
    pub fn split(&mut self, edge: Edge, new: usize) -> bool {
        let (Some(from), Some(to)) = (self.tabs.get(self.active), self.tabs.get(new)) else {
            return false;
        };
        let (from_id, to_id) = (from.view.id, to.view.id);
        let group = from.view.group.clone();
        if new == self.active || to.view.group != group || self.layout_of(to_id).is_some() {
            return false;
        }
        // A session showing alone while its workspace has panes takes its
        // place among them first.
        self.place(self.active);
        let (at, made) = match self.layout_of(from_id) {
            Some(at) => (at, false),
            None if self.layouts.iter().any(|panes| panes.group == group) => return false,
            None => {
                self.layouts.push(Panes {
                    group,
                    tree: Node::Leaf(from_id),
                    focus: from_id,
                });
                (self.layouts.len() - 1, true)
            }
        };
        if !self.layouts[at].tree.split(&from_id, edge, to_id) {
            if made {
                self.layouts.remove(at);
            }
            return false;
        }
        self.layouts[at].focus = to_id;
        // Another shape: a divider held is not the one it was.
        self.divider_drag = None;
        self.active = new;
        self.tabs[new].view.reset();
        true
    }

    /// Takes the tab `id` out of the layout it is a pane of: what was
    /// beside it gets its room, and the keyboard when it had it there. A
    /// layout left with one pane is no layout. Returns the pane that got
    /// the room.
    fn take_leaf(&mut self, id: PaneId) -> Option<PaneId> {
        let at = self.layout_of(id)?;
        let Panes { group, tree, focus } = self.layouts.remove(at);
        let heir = tree.heir(&id).copied();
        let mut rest = Some(tree);
        Node::remove(&mut rest, &id);
        self.divider_drag = None;
        if let (Some(tree), Some(heir)) = (rest, heir)
            && tree.leaf_count() > 1
        {
            self.layouts.insert(
                at,
                Panes {
                    group,
                    focus: if focus == id { heir } else { focus },
                    tree,
                },
            );
        }
        heir
    }

    /// Close Pane: the pane with the keyboard goes and its session stays
    /// a tab of the strip; what was beside it gets its room and the
    /// keyboard. Returns whether there was a pane to close.
    pub fn close_pane(&mut self) -> bool {
        let Some(id) = self.active_tab().map(|tab| tab.view.id) else {
            return false;
        };
        match self.take_leaf(id).and_then(|heir| self.index_of(heir)) {
            Some(next) => {
                self.active = next;
                true
            }
            None => false,
        }
    }

    /// The keyboard to the pane `step` along from the one that has it, in
    /// the order they read, round from the last to the first. Returns
    /// whether it moved.
    pub fn focus_step(&mut self, step: isize) -> bool {
        let Some(panes) = self.panes() else {
            return false;
        };
        let leaves: Vec<PaneId> = panes.tree.leaves().into_iter().copied().collect();
        let Some(place) = leaves
            .iter()
            .position(|id| self.index_of(*id) == Some(self.active))
        else {
            return false;
        };
        let to = (place as isize + step).rem_euclid(leaves.len() as isize) as usize;
        match self
            .index_of(leaves[to])
            .filter(|next| *next != self.active)
        {
            Some(next) => {
                self.place(next);
                true
            }
            None => false,
        }
    }

    /// The keyboard to the pane beside the one that has it, toward an
    /// edge of the panel's `screen`. Returns whether there is one.
    pub fn focus_toward(&mut self, screen: Viewport, toward: Edge) -> bool {
        let next = self.panes().and_then(|panes| {
            let id = self.active_tab()?.view.id;
            let next = panes.tree.neighbour(screen, PANE_GAP, &id, toward)?;
            self.index_of(*next)
        });
        match next {
            Some(next) => {
                self.place(next);
                true
            }
            None => false,
        }
    }

    /// The session whose pane a point of the panel's `screen` is in, or
    /// nearest to: the gaps and the padding belong to the pane beside
    /// them.
    pub fn pane_at(&self, screen: Viewport, x: f32, y: f32) -> usize {
        let away = |rect: &Viewport| {
            let dx = (rect.x - x).max(x - (rect.x + rect.width)).max(0.0);
            let dy = (rect.y - y).max(y - (rect.y + rect.height)).max(0.0);
            dx.max(dy)
        };
        self.screens(screen)
            .into_iter()
            .min_by(|(_, p), (_, q)| away(p).total_cmp(&away(q)))
            .map_or(self.active, |(tab, _)| tab)
    }

    /// The dividers between the panes showing, in the panel's `screen`.
    pub fn dividers(&self, screen: Viewport) -> Vec<Divider> {
        self.panes().map_or(Vec::new(), |panes| {
            panes.tree.dividers(screen, PANE_GAP, DIVIDER_SLOP)
        })
    }

    /// The divider a press at a point grabs.
    pub fn divider_at(&self, screen: Viewport, x: f32, y: f32) -> Option<Divider> {
        self.panes()?
            .tree
            .divider_at(screen, PANE_GAP, DIVIDER_SLOP, x, y)
    }

    /// A press on `divider` at a point: it is held, and follows the
    /// pointer from where it was taken.
    pub fn grab_divider(&mut self, divider: &Divider, x: f32, y: f32) {
        let Some(panes) = self.panes() else {
            return;
        };
        let gap = divider.rect;
        self.divider_drag = Some(DividerDrag {
            group: panes.group.clone(),
            id: divider.id,
            grab: (
                x - (gap.x + gap.width * 0.5),
                y - (gap.y + gap.height * 0.5),
            ),
        });
    }

    /// The layout a held divider is in and which way it splits, while it
    /// is still the one showing; otherwise the drag is over.
    fn held_divider(&mut self, screen: Viewport) -> Option<(usize, usize, Dir)> {
        let drag = self.divider_drag.as_ref()?;
        let id = self.active_tab().map(|tab| tab.view.id);
        let found = id
            .and_then(|id| self.layout_of(id))
            .filter(|at| self.layouts[*at].group == drag.group)
            .and_then(|at| {
                let bars = self.layouts[at].tree.dividers(screen, PANE_GAP, 0.0);
                Some((at, drag.id, bars.get(drag.id)?.dir))
            });
        if found.is_none() {
            self.divider_drag = None;
        }
        found
    }

    /// The pointer moved with a divider held: the divider goes with it,
    /// as far as leaving every pane [`PANE_MIN_COLS`] by
    /// [`PANE_MIN_ROWS`] allows. The sessions are not told: see
    /// [`Panel::fit`]. Returns whether the panes changed.
    pub fn drag_divider(&mut self, atlas: &Atlas, screen: Viewport, x: f32, y: f32) -> bool {
        let Some((at, id, dir)) = self.held_divider(screen) else {
            return false;
        };
        let (dx, dy) = self.divider_drag.as_ref().map_or((0.0, 0.0), |d| d.grab);
        let min = pane_min(atlas, dir);
        self.layouts[at]
            .tree
            .drag_divider(id, screen, PANE_GAP, x - dx, y - dy, min)
    }

    /// A double click on divider `id`: its two sides get half each, as
    /// far as the smallest pane allows. Returns whether the panes changed.
    pub fn center_divider(&mut self, atlas: &Atlas, screen: Viewport, id: usize) -> bool {
        self.divider_drag = None;
        let Some(at) = self
            .active_tab()
            .and_then(|tab| self.layout_of(tab.view.id))
        else {
            return false;
        };
        let bars = self.layouts[at].tree.dividers(screen, PANE_GAP, 0.0);
        let Some(dir) = bars.get(id).map(|bar| bar.dir) else {
            return false;
        };
        let min = pane_min(atlas, dir);
        self.layouts[at]
            .tree
            .center_divider(id, screen, PANE_GAP, min)
    }

    /// Whether the pane with the keyboard has room to be cut in two at
    /// `edge` with each half still the smallest a pane is dragged to.
    pub fn can_split(&self, atlas: &Atlas, screen: Viewport, edge: Edge) -> bool {
        let pane = self
            .screens(screen)
            .into_iter()
            .find(|(tab, _)| *tab == self.active)
            .map_or(screen, |(_, rect)| rect);
        can_cut(atlas, pane, edge)
    }

    /// A press on `tab` in the strip at a point: it shows, as a click
    /// does, and is held, to be dragged along the strip or onto a pane.
    /// The session it took the screen or a pane from is kept, for a drop
    /// that adds the tab to what was showing.
    pub fn press_tab(&mut self, tab: usize, x: f32, y: f32) {
        let Some(pressed) = self.tabs.get(tab) else {
            return;
        };
        let id = pressed.view.id;
        let before = self
            .active_tab()
            .filter(|was| {
                self.showing()
                    && was.view.id != id
                    && was.view.group == pressed.view.group
                    && self.layout_of(id).is_none()
            })
            .map(|was| was.view.id);
        self.show(tab);
        self.tab_drag = Some(TabDrag {
            tab,
            start_x: x,
            x,
            y,
            active: false,
            before,
            pane: false,
            drop: None,
        });
    }

    /// The pointer moved with a tab held, the strip in `header` and the
    /// panes in `screen`. Along the strip the tab is on its way to another
    /// place among the tabs once it has left where it was pressed. Taken
    /// [`PANE_DROP_AFTER`] under the strip, where `panes` are to be had,
    /// it is on its way to a pane, until it is level with the strip again.
    /// Where it would land is worked out here, once a move, for the frame
    /// to draw. Returns whether it is being dragged.
    pub fn drag_tab(
        &mut self,
        atlas: &Atlas,
        header: Viewport,
        screen: Viewport,
        panes: bool,
        x: f32,
        y: f32,
    ) -> bool {
        let Some(drag) = &mut self.tab_drag else {
            return false;
        };
        drag.x = x;
        drag.y = y;
        let under = y - (header.y + header.height);
        if !panes || under < 0.0 {
            drag.pane = false;
        } else if under > PANE_DROP_AFTER {
            drag.pane = true;
            drag.active = true;
        }
        drag.active |= (x - drag.start_x).abs() > TAB_DRAG_AFTER;
        let active = drag.active;
        let drop = self.drop_at(atlas, screen, x, y);
        if let Some(drag) = &mut self.tab_drag {
            drag.drop = drop;
        }
        active
    }

    /// Where the tab held over the panes in `screen` lands if let go at a
    /// point: the pane under it, the gaps and the padding belonging to
    /// the pane beside them, and the zone of that pane. None off the
    /// panel, where the drop would change nothing (a tab on its own pane,
    /// or put where it already is), and where a pane cut in two there
    /// would leave a half smaller than a pane gets.
    pub fn drop_at(&self, atlas: &Atlas, screen: Viewport, x: f32, y: f32) -> Option<PaneDrop> {
        let drag = self.tab_drag.filter(|drag| drag.active && drag.pane)?;
        let body = Viewport {
            x: screen.x - PAD,
            y: screen.y - PAD * 0.5,
            width: screen.width + PAD * 2.0,
            height: screen.height + PAD,
        };
        if !self.showing() || !body.contains(x, y) {
            return None;
        }
        let over = self.pane_at(screen, x, y);
        let (_, rect) = self
            .screens(screen)
            .into_iter()
            .find(|(tab, _)| *tab == over)?;
        // Just inside it, from a gap or the padding beside it.
        let inside =
            |at: f32, from: f32, extent: f32| at.clamp(from, (from + extent - 0.01).max(from));
        let zone = zone_at(
            rect,
            inside(x, rect.x, rect.width),
            inside(y, rect.y, rect.height),
        )?;
        let pane = self.tabs.get(over)?.view.id;
        self.dropped(&drag, pane, zone)?;
        if let Zone::Edge(edge) = zone
            && !can_cut(atlas, rect, edge)
        {
            return None;
        }
        Some(PaneDrop {
            pane,
            zone,
            rect: zone_rect(rect, PANE_GAP, zone),
        })
    }

    /// The panes as they are with the tab of `drag` let go in `zone` of
    /// the pane showing `over`, one pane alone being no layout. A tab the
    /// press showed in place of another comes from the strip: the other
    /// has its place back, and the tab is beside the pane it was let go
    /// on, or in its place. A tab that was a pane already moves there,
    /// what was beside it getting its room. None where nothing changes or
    /// the layout takes no more.
    fn dropped(&self, drag: &TabDrag, over: PaneId, zone: Zone) -> Option<Node<PaneId>> {
        let tab = self.tabs.get(drag.tab)?;
        let id = tab.view.id;
        let now = match self.panes() {
            Some(panes) => panes.tree.clone(),
            None => Node::Leaf(self.active_tab()?.view.id),
        };
        if !now.contains(&id) || !now.contains(&over) {
            return None;
        }
        let before = drag.before.filter(|before| {
            self.layout_of(*before).is_none()
                && self
                    .tabs
                    .iter()
                    .any(|was| was.view.id == *before && was.view.group == tab.view.group)
        });
        let mut tree = now.clone();
        let done = match (before, zone) {
            (Some(before), Zone::Edge(edge)) => {
                let target = if over == id { before } else { over };
                tree.replace(&id, before) && tree.split(&target, edge, id)
            }
            // In its own place it is there already.
            (Some(before), Zone::Center) => {
                over != id && tree.replace(&id, before) && tree.replace(&over, id)
            }
            (None, Zone::Edge(edge)) => tree.move_leaf(&id, &over, edge),
            (None, Zone::Center) => {
                let mut rest = Some(tree);
                let taken = over != id && Node::remove(&mut rest, &id);
                tree = rest?;
                taken && tree.replace(&over, id)
            }
        };
        (done && !tree.alike(&now)).then_some(tree)
    }

    /// The button went up at a point with a tab held over the panes in
    /// `screen`: the tab lands where [`Panel::drop_at`] has it, with the
    /// keyboard, and nowhere when that is nowhere. The drag is over
    /// either way. Returns whether the panes changed.
    pub fn drop_pane(&mut self, atlas: &Atlas, screen: Viewport, x: f32, y: f32) -> bool {
        let landed = self.drop_at(atlas, screen, x, y).and_then(|drop| {
            let drag = self.tab_drag?;
            Some((drag.tab, self.dropped(&drag, drop.pane, drop.zone)?))
        });
        self.tab_drag = None;
        let Some((tab, tree)) = landed else {
            return false;
        };
        let (id, group) = (self.tabs[tab].view.id, self.tabs[tab].view.group.clone());
        let at = self.layouts.iter().position(|panes| panes.group == group);
        if let Some(at) = at {
            self.layouts.remove(at);
        }
        if tree.leaf_count() > 1 {
            self.layouts.insert(
                at.unwrap_or(self.layouts.len()),
                Panes {
                    group,
                    tree,
                    focus: id,
                },
            );
        }
        // Another shape: a divider held is not the one it was.
        self.divider_drag = None;
        self.active = tab;
        true
    }

    /// For the self-test: the pane a held tab would land on, by its
    /// session's title, and the zone of it, as `title:zone`; `-` while
    /// there is none.
    pub fn drop_report(&self) -> String {
        self.tab_drag
            .filter(|drag| drag.active && drag.pane)
            .and_then(|drag| drag.drop)
            .map_or("-".to_owned(), |drop| {
                let pane = self
                    .index_of(drop.pane)
                    .and_then(|index| self.tabs.get(index))
                    .map_or("?".to_owned(), Tab::base_name);
                let zone = match drop.zone {
                    Zone::Center => "center",
                    Zone::Edge(Edge::Left) => "left",
                    Zone::Edge(Edge::Right) => "right",
                    Zone::Edge(Edge::Top) => "top",
                    Zone::Edge(Edge::Bottom) => "bottom",
                };
                format!("{pane}:{zone}")
            })
    }

    /// Keeps every session's grid the size of where it is drawn in the
    /// panel's `screen`: a pane's the size of its pane, whether its
    /// workspace's panes show now or not, so coming back to them tells no
    /// program anything; every other tab's the size of the whole screen.
    /// A size a session already has is not set again (see
    /// [`Session::resize`]), so a change of layout tells each program it
    /// touches once. Nothing while a divider is dragged: a program that
    /// draws its whole screen on every change would draw it for each
    /// step, so the panes are cut or padded until the divider is let go.
    /// Nothing while a tab is held either: the press shows it where the
    /// drop may not leave it, so each session hears where it ended up,
    /// once, when the button goes up.
    pub fn fit(&self, atlas: &Atlas, screen: Viewport) {
        if self.divider_drag.is_some() || self.tab_drag.is_some() {
            return;
        }
        let whole = grid_size(atlas, screen);
        let mut sizes = vec![whole; self.tabs.len()];
        for panes in &self.layouts {
            for (id, rect) in panes.tree.rects(screen, PANE_GAP) {
                if let Some(index) = self.index_of(*id) {
                    sizes[index] = grid_size(atlas, rect);
                }
            }
        }
        for (tab, (cols, rows)) in self.tabs.iter().zip(sizes) {
            tab.session.resize(cols, rows);
        }
    }

    /// The window says which workspace each tab is in, and the layouts
    /// are put right with it.
    pub fn regroup(&mut self, group: impl Fn(&Tab) -> Option<String>) {
        for tab in &mut self.tabs {
            let now = group(tab);
            if tab.view.group != now {
                tab.view.group = now;
            }
        }
        self.settle();
    }

    /// Puts the layouts right with the tabs as they are: a pane whose tab
    /// has gone, or has left for another workspace, gives its room to the
    /// one beside it; no workspace has two layouts; each layout's
    /// keyboard is in one of its panes; and the tab showing, when its
    /// workspace has panes, is one of them.
    pub fn settle(&mut self) {
        let stray: Vec<PaneId> = self
            .layouts
            .iter()
            .flat_map(|panes| {
                panes.tree.leaves().into_iter().copied().filter(|id| {
                    !self
                        .tabs
                        .iter()
                        .any(|tab| tab.view.id == *id && tab.view.group == panes.group)
                })
            })
            .collect();
        for id in stray {
            self.take_leaf(id);
        }
        let mut at = 0;
        while at < self.layouts.len() {
            let group = &self.layouts[at].group;
            if self.layouts[..at].iter().any(|panes| panes.group == *group) {
                self.layouts.remove(at);
                continue;
            }
            let panes = &mut self.layouts[at];
            if !panes.tree.contains(&panes.focus)
                && let Some(first) = panes.tree.leaves().first()
            {
                panes.focus = **first;
            }
            at += 1;
        }
        if self.showing() && self.active < self.tabs.len() {
            self.place(self.active);
        }
    }

    /// The session that has the keyboard among the panes of workspace
    /// `group`, for showing them again as they were left.
    pub fn focused_in(&self, group: Option<&str>) -> Option<usize> {
        let panes = self
            .layouts
            .iter()
            .find(|panes| panes.group.as_deref() == group)?;
        self.index_of(panes.focus)
    }

    /// The tabs whose screens are drawn: none while the panel is not
    /// `on_screen`, else the active one, or every pane of its layout.
    pub fn visible(&self, on_screen: bool) -> Vec<usize> {
        if !on_screen || self.tabs.is_empty() {
            return Vec::new();
        }
        match self.panes() {
            Some(panes) => panes
                .tree
                .leaves()
                .into_iter()
                .filter_map(|id| self.index_of(*id))
                .collect(),
            None => vec![self.active],
        }
    }

    /// How far back the session with the keyboard is scrolled.
    pub fn back(&self) -> usize {
        self.active_tab().map_or(0, |tab| tab.view.back)
    }

    /// What is selected in the session with the keyboard.
    pub fn selection(&self) -> Option<Selection> {
        self.active_tab()?.view.selection
    }

    /// The tab whose selection a held press is dragging.
    pub fn selecting(&self) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.view.selecting)
    }

    /// Whether a selection is being dragged, the pointer having moved.
    pub fn selecting_dragged(&self) -> bool {
        self.tabs
            .iter()
            .any(|tab| tab.view.selecting && tab.view.drag_point.is_some())
    }

    /// The button went up: a drag selects no further, and a press that
    /// never moved selected nothing.
    pub fn end_selecting(&mut self) {
        for tab in &mut self.tabs {
            let view = &mut tab.view;
            if std::mem::take(&mut view.selecting) && view.selection.is_some_and(|(a, b)| a == b) {
                view.selection = None;
            }
            view.drag_point = None;
        }
    }

    /// The panes showing as text (see [`Node::encode`]), each pane what
    /// `name` gives for its tab's place, the one with the keyboard
    /// starred; `-` while one session shows alone.
    pub fn panes_text(&self, name: impl Fn(usize) -> String) -> String {
        self.panes().map_or("-".to_owned(), |panes| {
            panes.tree.encode(|id| {
                let pane = self.index_of(*id).map_or("?".to_owned(), &name);
                if *id == panes.focus {
                    format!("*{pane}")
                } else {
                    pane
                }
            })
        })
    }

    /// For the self-test: each pane showing in the order they read, as
    /// `title:columnsxrows:rN:bN:sN@x,y,width,height`: its session's
    /// grid, how many times its program was told another size, how far
    /// back it is scrolled, whether anything is selected in it (1 or 0),
    /// and where it is.
    pub fn panes_report(&self, screen: Viewport) -> String {
        let panes: Vec<String> = self
            .screens(screen)
            .into_iter()
            .filter_map(|(index, rect)| {
                let tab = self.tabs.get(index)?;
                let term = tab.session.term.lock().unwrap_or_else(|e| e.into_inner());
                Some(format!(
                    "{}:{}x{}:r{}:b{}:s{}@{},{},{},{}",
                    tab.base_name_of(&term),
                    term.cols(),
                    term.rows(),
                    tab.session.resizes(),
                    tab.view.back,
                    u8::from(tab.view.selection.is_some()),
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height
                ))
            })
            .collect();
        panes.join(" ")
    }

    /// What names the tab at `index` wherever it goes and for as long as
    /// it lives: its [`Tab::id`], as [`Panel::tab_by_id`] takes it. For
    /// what outlasts a frame, such as the tab an open menu is about.
    pub fn id_of(&self, index: usize) -> Option<String> {
        self.tabs
            .get(index)
            .map(|tab| tab.id().to_string_lossy().into_owned())
    }

    /// The tab a right-click's menu is about, where it is now; none once
    /// it has closed.
    pub fn menu_tab_index(&self) -> Option<usize> {
        self.tab_by_id(self.menu_tab.as_deref()?)
    }

    /// Closes the tab whose [`Tab::id`] is `id`, a menu's Close: that
    /// session wherever it is by now, and nothing when it has gone.
    /// Returns whether there was one.
    pub fn close_by_id(&mut self, id: &str) -> bool {
        match self.tab_by_id(id) {
            Some(index) => {
                self.close_tab(index);
                true
            }
            None => false,
        }
    }

    /// The tabs the header lists, in the order it lists them: all of
    /// them, or in Agents mode the selected workspace's.
    pub fn listed(&self) -> Vec<usize> {
        (0..self.tabs.len())
            .filter(|i| self.shown.as_ref().is_none_or(|shown| shown.contains(i)))
            .collect()
    }

    /// Moves the tab at `from` to `to`, the tabs between them each a place
    /// along, and everything that names a tab by its place with them: the
    /// session with the keyboard, the tabs listed, a press a program is
    /// being told of, the cell and the link under the pointer, a drag.
    /// The panes name their tabs by [`PaneId`], and each tab's view goes
    /// with it. Returns whether it moved.
    pub fn move_tab(&mut self, from: usize, to: usize) -> bool {
        let len = self.tabs.len();
        if from == to || from >= len || to >= len {
            return false;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        let at = |index: &mut usize| *index = moved(*index, from, to);
        at(&mut self.active);
        if let Some(shown) = &mut self.shown {
            shown.iter_mut().for_each(at);
            shown.sort_unstable();
        }
        if let Some((tab, ..)) = &mut self.mouse_press {
            at(tab);
        }
        if let Some((tab, _)) = &mut self.mouse_cell {
            at(tab);
        }
        if let Some((tab, _)) = &mut self.link {
            at(tab);
        }
        if let Some(drag) = &mut self.tab_drag {
            at(&mut drag.tab);
        }
        // The place a closed tab left is the place of the one that slid
        // into it; past the last there is none to follow.
        if let Some((.., place)) = &mut self.closed_active
            && *place < len
        {
            at(place);
        }
        true
    }

    /// Moves `tab` `step` places along the tabs in `listed`, the header's
    /// order: past the tabs of other workspaces between them, which keep
    /// their places among themselves. Not past either end.
    pub fn step_tab(&mut self, listed: &[usize], tab: usize, step: isize) -> bool {
        let Some(place) = listed.iter().position(|i| *i == tab) else {
            return false;
        };
        match place.checked_add_signed(step).and_then(|to| listed.get(to)) {
            Some(to) => self.move_tab(tab, *to),
            None => false,
        }
    }

    /// Moves `tab` into the gap before the `slot`th of the tabs in
    /// `listed`, after the last when `slot` is how many there are: where
    /// a drag along the strip lets it go.
    pub fn drop_tab(&mut self, listed: &[usize], tab: usize, slot: usize) -> bool {
        let Some(place) = listed.iter().position(|i| *i == tab) else {
            return false;
        };
        // Its own place is not counted once it has left it.
        let to = if slot > place { slot - 1 } else { slot };
        match listed.get(to) {
            Some(to) => self.move_tab(tab, *to),
            None => false,
        }
    }

    /// The tab `step` along `listed` from the one showing, round from the
    /// last to the first; none when no other is listed.
    pub fn cycled(&self, listed: &[usize], step: isize) -> Option<usize> {
        let place = listed.iter().position(|i| *i == self.active)?;
        let to = (place as isize + step).rem_euclid(listed.len() as isize) as usize;
        (listed[to] != self.active).then_some(listed[to])
    }

    /// The wheel over the strip, or a drag held at one of its ends: `by`
    /// points along, held to what there is by the next frame.
    pub fn scroll_strip(&mut self, by: f32) {
        self.strip_scroll = (self.strip_scroll + by).max(0.0);
        self.strip_free = true;
    }

    /// Lays the strip out for a frame and keeps where it is scrolled to:
    /// a tab that has just become the active one is brought into view.
    pub fn settle_strip(&mut self, atlas: &mut Atlas, header: Viewport) {
        let seen = self
            .active_tab()
            .filter(|_| self.showing())
            .map_or(0, |tab| std::sync::Arc::as_ptr(&tab.alive) as usize);
        if seen != self.strip_seen {
            self.strip_seen = seen;
            self.strip_free = false;
        }
        self.strip_scroll = strip(self, atlas, header).scroll;
    }

    /// For the self-test: the tabs by title in the strip's order, the one
    /// showing starred and those the header does not list in brackets,
    /// the panes by title (see [`Panel::panes_text`]), whether the strip
    /// is scrolled, and the tab being dragged along it.
    pub fn report(&self) -> String {
        let listed = self.listed();
        let title = |index: usize| self.tabs.get(index).map_or("?".to_owned(), Tab::base_name);
        format!(
            "{} panes={} scrolled={} drag={}",
            (0..self.tabs.len())
                .map(|i| match (i == self.active, listed.contains(&i)) {
                    (true, true) => format!("*{}", title(i)),
                    (false, true) => title(i),
                    (true, false) => format!("(*{})", title(i)),
                    (false, false) => format!("({})", title(i)),
                })
                .collect::<Vec<_>>()
                .join(","),
            self.panes_text(title),
            if self.strip_scroll > 0.5 { "yes" } else { "no" },
            self.tab_drag
                .filter(|drag| drag.active)
                .map_or("-".to_owned(), |drag| title(drag.tab)),
        )
    }

    /// Closes the tab at `index`. A pane it was in gives its room to the
    /// one beside it, which has the keyboard next when the closed one had
    /// it; closed from no pane, the tab that slid into its place is at
    /// the front, for the window to choose its workspace's next.
    pub fn close_tab(&mut self, index: usize) {
        self.mouse_press = None;
        self.mouse_cell = None;
        self.tab_drag = None;
        // What is on screen now: a tab that comes to the front from the
        // strip starts at its output, a pane that stays is left alone.
        let seen: Vec<PaneId> = self
            .visible(true)
            .into_iter()
            .filter_map(|tab| Some(self.tabs.get(tab)?.view.id))
            .collect();
        if index < self.tabs.len() {
            // Its pane goes first, while its tab still says which it is.
            let heir = self.take_leaf(self.tabs[index].view.id);
            // Dropping the session hangs up on its program.
            let tab = self.tabs.remove(index);
            if tab.notified {
                self.withdrawn.push(tab.id().to_string_lossy().into_owned());
            }
            // Its agent review session goes with it: what its agent changed
            // is for the Git panel now. A tab left to a helper at quit is
            // detached, not closed, and keeps its session.
            if !tab.review.as_os_str().is_empty() {
                crate::project::review::remove(&tab.review);
            }
            let beside = heir.and_then(|heir| self.index_of(heir));
            if index != self.active {
                // The one showing stays the one, a place down when the
                // closed one was before it.
                if index < self.active {
                    self.active -= 1;
                }
            } else if let Some(beside) = beside {
                self.active = beside;
            } else {
                self.closed_active = Some((tab.workspace.clone(), tab.folder.clone(), index));
            }
            // The tabs listed, renumbered as they are.
            if let Some(shown) = &mut self.shown {
                shown.retain(|i| *i != index);
                for i in shown.iter_mut().filter(|i| **i > index) {
                    *i -= 1;
                }
            }
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
        if self.tabs.is_empty() {
            self.open = false;
            self.focus = false;
        }
        if let Some(tab) = self.tabs.get_mut(self.active)
            && !seen.contains(&tab.view.id)
        {
            tab.view.reset();
        }
    }
}

/// The least time between redraws that only tabs off screen ask for.
pub const BACKGROUND_EVERY: Duration = Duration::from_millis(100);
/// The least time between two reads of a tab's process.
pub const PROBE_EVERY: Duration = Duration::from_millis(500);

/// What is shown of a tab when its screen is not: a change in it is what
/// output off screen redraws for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Face {
    pub activity: Activity,
    pub attention: bool,
    pub status: Option<(term::status::State, String)>,
    pub state: String,
    pub name: String,
}

/// When something is done: now, at a time, or not at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum When {
    Now,
    At(Instant),
    Never,
}

/// The pace of redraws for output in tabs off screen: one at most every
/// [`BACKGROUND_EVERY`], and one held back drawn when its time comes.
#[derive(Default, Debug)]
pub struct Pace {
    /// When a change off screen last drew.
    drew: Option<Instant>,
    /// A change held back, to draw then.
    pub due: Option<Instant>,
    /// When the settle scheduled runs, while one is.
    pub armed: Option<Instant>,
}

impl Pace {
    /// Whether output draws now, later or not at all: `visible` when a tab
    /// on screen printed, `changed` when one off screen changed its
    /// [`Face`] or what the window says of it. Output off screen that
    /// changed nothing shown draws nothing.
    pub fn decide(&mut self, visible: bool, changed: bool, now: Instant) -> When {
        if visible {
            // The frame shows every change held back too.
            self.due = None;
            return When::Now;
        }
        if !changed {
            return When::Never;
        }
        match self.drew {
            Some(at) if now.saturating_duration_since(at) < BACKGROUND_EVERY => {
                let due = at + BACKGROUND_EVERY;
                self.due = Some(self.due.map_or(due, |d| d.min(due)));
                When::At(due)
            }
            _ => {
                self.drew = Some(now);
                self.due = None;
                When::Now
            }
        }
    }

    /// Whether a held-back change is due by `now`, once.
    pub fn take_due(&mut self, now: Instant) -> bool {
        match self.due {
            Some(due) if due <= now => {
                self.due = None;
                self.drew = Some(now);
                true
            }
            _ => false,
        }
    }

    /// Whether a settle at `at` has to be scheduled: none is, or the one
    /// that is runs later.
    pub fn arm(&mut self, at: Instant) -> bool {
        if self.armed.is_some_and(|armed| armed <= at) {
            return false;
        }
        self.armed = Some(at);
        true
    }
}

/// Matches Find keeps in a session; past this the count says so.
pub const FIND_LIMIT: usize = 10_000;

/// A match in a session: where it starts, and the line and columns
/// `[from, to)` of each row it covers, several across a wrap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub start: (u64, usize),
    pub spans: Vec<(u64, usize, usize)>,
}

/// What a search of a session is for: the session (its terminal's
/// address), its generation, the query and the options.
pub type FindKey = (usize, u64, String, crate::platform::search::Options);

/// Output alone is searched again at most this often, so a busy program
/// is not searched on every frame.
pub const FIND_AGAIN_AFTER: Duration = Duration::from_millis(100);
/// A search for a new query running this long has the bar say so.
const SEARCHING_SHOWN_AFTER: Duration = Duration::from_millis(150);

/// A line of filter mode's list: the first match on it, the line it
/// starts on, the line's text around it, and the characters `[from, to)`
/// of that text each match on the line covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilterRow {
    /// Its first match, by its place in [`Find::matches`].
    pub hit: usize,
    pub line: u64,
    pub text: String,
    pub marks: Vec<(usize, usize)>,
}

/// Filter mode's list while it shows: the row chosen, and the first row
/// showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    pub selected: usize,
    pub scroll: usize,
}

/// What a search of a session found: the matches, the list of the lines
/// they are on, and the number of the first line the session kept.
#[derive(Default)]
pub struct Found {
    pub matches: Vec<Hit>,
    pub rows: Vec<FilterRow>,
    pub first: u64,
}

/// A terminal search's number and what it found.
type FindAnswer = (u64, Found);

/// What [`Find::sync`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Synced {
    /// The matches are for what was asked, or on their way.
    Fresh,
    /// Only the output changed, and it was searched a moment ago: a frame
    /// has to look again.
    Later,
    /// A search started on a worker.
    Started,
    /// Answered at once: an empty query, or a pattern that is not valid,
    /// in the search's words.
    Done(Option<String>),
}

/// Find in the terminal: the active session's matches and the one the bar
/// is on. Searched on a worker when the output, the query, the options or
/// the session change (see [`Find::sync`]); the matches it has stay drawn
/// until the answer comes.
#[derive(Default)]
pub struct Find {
    /// What the matches are for.
    pub made_for: Option<FindKey>,
    pub matches: Vec<Hit>,
    pub current: Option<usize>,
    /// Where the current match starts, so it stays current while output
    /// arrives and the list is made again.
    pub anchor: Option<(u64, usize)>,
    /// An invalid pattern, in the search's words.
    pub error: Option<String>,
    /// When the last search started.
    pub searched: Option<Instant>,
    /// One row per line with a match, for filter mode, and the number of
    /// the first line the session kept when they were found.
    pub rows: Vec<FilterRow>,
    pub first_line: u64,
    /// Filter mode's list, while it shows over the screen.
    pub filter: Option<Filter>,
    /// A step asked for before the matches of a new query came, forward
    /// or back: taken once they do.
    pub pending_step: Option<bool>,
    /// The current match is to be scrolled into view once the answer
    /// comes.
    pub reveal: bool,
    /// A frame was asked for to say the search is taking a while.
    pub said_searching: bool,
    /// The search running: its number, what it is for, and where its
    /// answer arrives.
    running: Option<(u64, FindKey, std::sync::mpsc::Receiver<FindAnswer>)>,
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    generation: u64,
}

impl Find {
    /// Takes the matches of a new search. The current one stays on its
    /// anchor, or moves to the next after it, or the last before it;
    /// with no anchor it is the last, the newest output.
    pub fn take(&mut self, matches: Vec<Hit>) {
        self.matches = matches;
        self.error = None;
        self.place();
    }

    /// The current match from the anchor, as [`Find::take`] says.
    fn place(&mut self) {
        self.current = match self.anchor {
            _ if self.matches.is_empty() => None,
            None => Some(self.matches.len() - 1),
            Some(at) => Some(
                self.matches
                    .iter()
                    .position(|m| m.start >= at)
                    .unwrap_or(self.matches.len() - 1),
            ),
        };
        self.anchor = self.current.map(|i| self.matches[i].start);
    }

    /// The next or previous match, around the end to the other end.
    pub fn step(&mut self, forward: bool) {
        let len = self.matches.len();
        if len == 0 {
            return;
        }
        let next = match self.current {
            None if forward => 0,
            None => len - 1,
            Some(i) if forward => (i + 1) % len,
            Some(i) => (i + len - 1) % len,
        };
        self.current = Some(next);
        self.anchor = Some(self.matches[next].start);
    }

    pub fn current_hit(&self) -> Option<&Hit> {
        self.matches.get(self.current?)
    }

    /// "3 of 17" as numbers: the current match's place from 0, and how
    /// many there are.
    pub fn count(&self) -> (usize, usize) {
        (self.current.unwrap_or(0), self.matches.len())
    }

    /// Whether a search runs on a worker.
    pub fn busy(&self) -> bool {
        self.running.is_some()
    }

    /// Whether the matches shown are for another query, other options or
    /// another session than the search on its way: a step waits for the
    /// answer.
    pub fn searching(&self) -> bool {
        let Some((_, asked, _)) = &self.running else {
            return false;
        };
        self.made_for
            .as_ref()
            .is_none_or(|made| made.0 != asked.0 || made.2 != asked.2 || made.3 != asked.3)
    }

    /// Whether the bar says it is searching: [`Find::searching`] for a
    /// moment already. Most searches answer before a frame would show it,
    /// and saying so for one frame on every key typed would only flicker.
    pub fn searching_shown(&self) -> bool {
        self.searching()
            && self
                .searched
                .is_some_and(|at| at.elapsed() >= SEARCHING_SHOWN_AFTER)
    }

    /// The main thread's part of keeping the matches up to date with `key`
    /// in `term`, which the caller holds locked: the matches of lines the
    /// session no longer keeps go, and a search starts when the session,
    /// the query or the options changed, or the output did and it was not
    /// searched in the last [`FIND_AGAIN_AFTER`] (`forced` or not). One
    /// for the output alone waits for the one running; one for anything
    /// else stops it. Only the cells are copied here; the text is made
    /// and searched on the worker.
    pub fn sync(&mut self, key: FindKey, term: &term::Term, forced: bool) -> Synced {
        self.prune(term.first_line());
        let asked = self
            .running
            .as_ref()
            .map(|(_, asked, _)| asked)
            .or(self.made_for.as_ref());
        if let Some(asked) = asked {
            if *asked == key {
                return Synced::Fresh;
            }
            let output_only = asked.0 == key.0 && asked.2 == key.2 && asked.3 == key.3;
            if output_only && self.running.is_some() {
                return Synced::Fresh;
            }
            if output_only
                && !forced
                && self
                    .searched
                    .is_some_and(|at| at.elapsed() < FIND_AGAIN_AFTER)
            {
                return Synced::Later;
            }
        }
        // Another session: its matches, and where the last one's match
        // was, mean nothing here.
        if self.made_for.as_ref().is_none_or(|m| m.0 != key.0) {
            self.stop();
            self.made_for = None;
            self.matches.clear();
            self.rows.clear();
            self.current = None;
            self.anchor = None;
        }
        // An empty query and an invalid pattern are answered at once,
        // with no worker: a pattern is checked against no text.
        let checked = match key.2.as_str() {
            "" => Ok(()),
            query => find_in(&term::Haystack::default(), query, key.3).map(|_| ()),
        };
        if key.2.is_empty() || checked.is_err() {
            self.stop();
            self.made_for = Some(key);
            self.searched = Some(Instant::now());
            self.matches.clear();
            self.rows.clear();
            self.current = None;
            self.pending_step = None;
            self.error = checked.err();
            return Synced::Done(self.error.clone());
        }
        self.start(key, term.find_lines());
        Synced::Started
    }

    /// Searches `lines` for `key`'s query on a worker. A search still
    /// running is stopped, and its answer dropped if it comes anyway.
    fn start(&mut self, key: FindKey, lines: term::Lines) {
        self.stop();
        self.generation += 1;
        let generation = self.generation;
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_cancel = std::sync::Arc::clone(&cancel);
        let (query, options) = (key.2.clone(), key.3);
        std::thread::spawn(move || {
            if let Some(found) = search_lines(&lines, &query, options, &worker_cancel) {
                let _ = tx.send((generation, found));
            }
        });
        self.running = Some((generation, key, rx));
        self.cancel = Some(cancel);
        self.searched = Some(Instant::now());
        self.said_searching = false;
    }

    /// Takes the worker's answer once it is there; `true` when it came.
    pub fn poll(&mut self) -> bool {
        let Some((_, _, rx)) = &self.running else {
            return false;
        };
        let answer = match rx.try_recv() {
            Ok(answer) => Some(answer),
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => None,
        };
        let Some((_, key, _)) = self.running.take() else {
            return false;
        };
        self.cancel = None;
        answer.is_some_and(|(made, found)| self.land(made, key, found))
    }

    /// The answer of search number `made`, for `key`: dropped unless it is
    /// the last search started. The current match keeps its anchor, and a
    /// step asked for while it was on its way is taken. `true` when it
    /// was taken.
    fn land(&mut self, made: u64, key: FindKey, found: Found) -> bool {
        if made != self.generation {
            return false;
        }
        self.made_for = Some(key);
        self.rows = found.rows;
        self.first_line = found.first;
        self.take(found.matches);
        if let Some(forward) = self.pending_step.take() {
            self.step(forward);
            self.reveal = true;
        }
        if let Some(filter) = self.filter.as_mut() {
            filter.selected = filter.selected.min(self.rows.len().saturating_sub(1));
        }
        true
    }

    /// Lets go of the matches on lines before `first`, which the session
    /// no longer keeps. A current match among them falls to the next one.
    pub fn prune(&mut self, first: u64) {
        let gone = self.matches.partition_point(|h| h.start.0 < first);
        if gone == 0 {
            return;
        }
        self.matches.drain(..gone);
        let rows = self.rows.len();
        self.rows.retain_mut(|row| {
            row.hit = match row.hit.checked_sub(gone) {
                Some(hit) => hit,
                None => return false,
            };
            true
        });
        // The rows gone were the list's first: the rest move up.
        let dropped = rows - self.rows.len();
        if let Some(filter) = self.filter.as_mut() {
            filter.selected = filter
                .selected
                .saturating_sub(dropped)
                .min(self.rows.len().saturating_sub(1));
            filter.scroll = filter.scroll.saturating_sub(dropped);
        }
        if self.current.is_some() {
            self.place();
        }
    }

    /// The row of filter mode's list the current match is on, or the
    /// nearest before it.
    pub fn current_row(&self) -> usize {
        let at = self.current.unwrap_or(0);
        self.rows
            .partition_point(|row| row.hit <= at)
            .saturating_sub(1)
    }

    fn stop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.running = None;
    }
}

impl Drop for Find {
    /// Closing the bar stops the worker.
    fn drop(&mut self) {
        self.stop();
    }
}

/// The matches of `query` in a session's text, at most [`FIND_LIMIT`].
pub fn find_in(
    hay: &term::Haystack,
    query: &str,
    options: crate::platform::search::Options,
) -> Result<Vec<Hit>, String> {
    Ok(found_in(hay, query, options)?
        .into_iter()
        .map(|(hit, _)| hit)
        .collect())
}

/// [`find_in`], with the bytes of the text each match covers.
fn found_in(
    hay: &term::Haystack,
    query: &str,
    options: crate::platform::search::Options,
) -> Result<Vec<(Hit, std::ops::Range<usize>)>, String> {
    let found = crate::platform::search::find_first(&hay.text, query, "", options, FIND_LIMIT)?;
    Ok(found
        .into_iter()
        .filter_map(|m| {
            let spans = hay.spans(m.range.clone());
            let (line, from, _) = *spans.first()?;
            Some((
                Hit {
                    start: (line, from),
                    spans,
                },
                m.range,
            ))
        })
        .collect())
}

/// The worker's part of a search: the text of `lines` made, searched for
/// `query`, and the list of the lines with a match made. Nothing once
/// `cancel` is set.
pub fn search_lines(
    lines: &term::Lines,
    query: &str,
    options: crate::platform::search::Options,
    cancel: &std::sync::atomic::AtomicBool,
) -> Option<Found> {
    let cancelled = || cancel.load(std::sync::atomic::Ordering::Relaxed);
    let hay = lines.haystack();
    if cancelled() {
        return None;
    }
    let found = found_in(&hay, query, options).unwrap_or_default();
    if cancelled() {
        return None;
    }
    let rows = filter_rows(&hay.text, &found);
    Some(Found {
        matches: found.into_iter().map(|(hit, _)| hit).collect(),
        rows,
        first: lines.first_line(),
    })
}

/// One row per line of `text` with a match in `found`, in order: the
/// line's text around its first match, with every match on it marked.
/// A line the terminal wrapped is one line.
pub fn filter_rows(text: &str, found: &[(Hit, std::ops::Range<usize>)]) -> Vec<FilterRow> {
    let mut rows = Vec::new();
    let mut index = 0;
    while index < found.len() {
        let (hit, range) = &found[index];
        let start = text[..range.start].rfind('\n').map_or(0, |i| i + 1);
        let end = text[range.start..]
            .find('\n')
            .map_or(text.len(), |i| range.start + i);
        let next = index
            + 1
            + found[index + 1..]
                .iter()
                .take_while(|(_, r)| r.start < end)
                .count();
        // Each match's bytes in the line, as far as the line goes.
        let on_line: Vec<_> = found[index..next]
            .iter()
            .map(|(_, r)| r.start - start..r.end.min(end) - start)
            .collect();
        let (snippet, marks) = marked_snippet(&text[start..end], range.start - start, &on_line);
        rows.push(FilterRow {
            hit: index,
            line: hit.start.0,
            text: snippet,
            marks,
        });
        index = next;
    }
    rows
}

/// Find in Terminals keeps this many matches of one session, the newest.
pub const SEARCH_ALL_PER_TERMINAL: usize = 50;
/// And this many in all.
pub const SEARCH_ALL_LIMIT: usize = 500;
/// A pause in typing this long starts the search.
const SEARCH_ALL_DEBOUNCE: Duration = Duration::from_millis(150);
/// A match's row shows this many characters of its line, this many of
/// them before it.
const SNIPPET: usize = 120;
const SNIPPET_BEFORE: usize = 40;

/// A session for Find in Terminals: its terminal's address, which picks it
/// again, where it is ("workspace / title"), and its text, copied out.
pub struct Searched {
    pub term: usize,
    pub place: String,
    pub hay: term::Haystack,
}

/// A match of Find in Terminals: the session's terminal's address, where
/// the match starts, and the palette row's words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermHit {
    pub term: usize,
    pub start: (u64, usize),
    pub snippet: String,
    pub detail: String,
}

/// The matches of `query` in every session, in their order, each one's
/// newest first: at most [`SEARCH_ALL_PER_TERMINAL`] of a session and
/// [`SEARCH_ALL_LIMIT`] in all. Stops early once `cancel` is set.
pub fn search_all(
    sessions: &[Searched],
    query: &str,
    options: crate::platform::search::Options,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Vec<TermHit>, String> {
    let mut hits = Vec::new();
    if query.is_empty() {
        return Ok(hits);
    }
    // An invalid pattern says so with no session open too.
    find_in(&term::Haystack::default(), query, options)?;
    for session in sessions {
        let room = SEARCH_ALL_LIMIT - hits.len();
        if room == 0 || cancel.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        let found = find_in(&session.hay, query, options)?;
        let first = session.hay.first_line().unwrap_or(0);
        for hit in found
            .into_iter()
            .rev()
            .take(SEARCH_ALL_PER_TERMINAL.min(room))
        {
            let (line, col) = hit.start;
            hits.push(TermHit {
                term: session.term,
                start: hit.start,
                snippet: session
                    .hay
                    .row_text(line, col)
                    .map(|(text, at)| snippet(text, at))
                    .unwrap_or_default(),
                detail: format!(
                    "{}  \u{b7}  line {}",
                    session.place,
                    line.saturating_sub(first) + 1
                ),
            });
        }
    }
    Ok(hits)
}

/// Up to [`SNIPPET`] characters of `text` around byte `at`, blanks
/// trimmed, with an ellipsis where the line goes on.
pub fn snippet(text: &str, at: usize) -> String {
    marked_snippet(text, at, &[]).0
}

/// [`snippet`], and the characters `[from, to)` of it that the bytes
/// `marks` of `text` cover, as much of each as it shows.
pub fn marked_snippet(
    text: &str,
    at: usize,
    marks: &[std::ops::Range<usize>],
) -> (String, Vec<(usize, usize)>) {
    let starts: Vec<usize> = text.char_indices().map(|(at, _)| at).collect();
    let index = |byte: usize| starts.partition_point(|&b| b < byte);
    let chars: Vec<char> = text.chars().collect();
    let from = index(at)
        .saturating_sub(SNIPPET_BEFORE)
        .min(chars.len().saturating_sub(SNIPPET));
    let to = (from + SNIPPET).min(chars.len());
    let shown: String = chars[from..to].iter().collect();
    let lead = shown.chars().take_while(|c| c.is_whitespace()).count();
    let trimmed = shown.trim();
    let mut out = String::new();
    if from > 0 {
        out.push('\u{2026}');
    }
    // Where the shown characters start in `out`, and how many there are.
    let (base, kept) = (from + lead, trimmed.chars().count());
    let shift = usize::from(from > 0);
    out.push_str(trimmed);
    if to < chars.len() {
        out.push('\u{2026}');
    }
    let marked = marks
        .iter()
        .filter_map(|range| {
            let a = index(range.start).max(base);
            let b = index(range.end).min(base + kept);
            (b > a).then(|| (a - base + shift, b - base + shift))
        })
        .collect();
    (out, marked)
}

/// Find in Terminals while the palette searches every session: what was
/// typed, the search for it on a worker, and the last search's answer.
#[derive(Default)]
pub struct SearchAll {
    /// The palette's query as last seen, and when its search is due.
    typed: String,
    due: Option<Instant>,
    /// The search running: its number, and where its answer arrives.
    running: Option<(u64, std::sync::mpsc::Receiver<SearchAnswer>)>,
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    generation: u64,
    /// The query the matches are for, the matches, and an invalid
    /// pattern in the search's words.
    pub query: String,
    pub hits: Vec<TermHit>,
    pub error: Option<String>,
}

/// A search's number, its query and what it found.
type SearchAnswer = (u64, String, Result<Vec<TermHit>, String>);

impl SearchAll {
    /// Notes the palette's query; `true` when it changed, so a search is
    /// due once typing pauses.
    pub fn note(&mut self, query: &str) -> bool {
        if query == self.typed {
            return false;
        }
        self.typed = query.to_owned();
        self.due = Some(Instant::now() + SEARCH_ALL_DEBOUNCE);
        true
    }

    /// What the last search was asked, or the next will be.
    pub fn typed(&self) -> &str {
        &self.typed
    }

    /// Whether typing has paused long enough for the search to start.
    pub fn due(&self, now: Instant) -> bool {
        self.due.is_some_and(|at| now >= at)
    }

    /// Searches `sessions` for what was typed on a worker. A search still
    /// running is stopped, and its answer dropped if it comes anyway.
    pub fn start(&mut self, sessions: Vec<Searched>, options: crate::platform::search::Options) {
        self.due = None;
        self.stop();
        self.generation += 1;
        let query = self.typed.clone();
        if query.is_empty() {
            self.query = query;
            self.hits.clear();
            self.error = None;
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_cancel = std::sync::Arc::clone(&cancel);
        let generation = self.generation;
        std::thread::spawn(move || {
            let found = search_all(&sessions, &query, options, &worker_cancel);
            let _ = tx.send((generation, query, found));
        });
        self.running = Some((generation, rx));
        self.cancel = Some(cancel);
    }

    /// Takes the worker's answer once it is there; `true` when it came.
    pub fn poll(&mut self) -> bool {
        let Some((generation, rx)) = &self.running else {
            return false;
        };
        let answer = match rx.try_recv() {
            Ok(answer) => Some(answer),
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => None,
        };
        let generation = *generation;
        self.running = None;
        self.cancel = None;
        let Some((_, query, found)) = answer.filter(|(made, ..)| *made == generation) else {
            return true;
        };
        self.query = query;
        match found {
            Ok(hits) => {
                self.hits = hits;
                self.error = None;
            }
            Err(error) => {
                self.hits.clear();
                self.error = Some(error);
            }
        }
        true
    }

    /// Whether a search waits for typing to pause or for its worker.
    pub fn busy(&self) -> bool {
        self.due.is_some() || self.running.is_some()
    }

    /// Whether the list is empty only because its answer is on its way.
    pub fn loading(&self) -> bool {
        self.busy() && self.hits.is_empty() && self.error.is_none() && !self.typed.is_empty()
    }

    fn stop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.running = None;
    }
}

impl Drop for SearchAll {
    /// Closing the palette stops the worker.
    fn drop(&mut self) {
        self.stop();
    }
}

/// The find bar is at most this wide: a panel at the top right of the
/// pane, over the grid rather than taking room from it, which would
/// resize the program's terminal. It used to span the pane, and hid a
/// whole row of what the program printed for the sake of one field.
const FIND_BAR_WIDTH: f32 = 560.0;
/// Air between the bar and the pane's top and right edges.
pub const FIND_BAR_MARGIN: f32 = 6.0;

/// The find bar over the session with the keyboard, in the panel `rect`:
/// one row at the top right of its pane, as wide as [`FIND_BAR_WIDTH`]
/// or the pane.
pub fn find_bar_rect(panel: &Panel, rect: Viewport) -> Viewport {
    let (header, screen) = split(rect);
    let pane = active_pane(panel, screen);
    let room = pane.width + PAD * 2.0 - FIND_BAR_MARGIN;
    let width = FIND_BAR_WIDTH.min(room).max(0.0);
    Viewport {
        x: pane.x + pane.width + PAD - FIND_BAR_MARGIN - width,
        y: header.y + header.height + FIND_BAR_MARGIN,
        width,
        height: layout::FIND_ROW_HEIGHT,
    }
}

/// The screen rows the find bar hides, at the top.
pub fn find_bar_rows(atlas: &Atlas) -> usize {
    ((FIND_BAR_MARGIN + layout::FIND_ROW_HEIGHT - PAD * 0.5) / row_height(atlas)).ceil() as usize
}

/// The screen of the session with the keyboard, in the panel's `screen`.
fn active_pane(panel: &Panel, screen: Viewport) -> Viewport {
    panel
        .screens(screen)
        .into_iter()
        .find(|(tab, _)| *tab == panel.active)
        .map_or(screen, |(_, s)| s)
}

/// Air inside filter mode's list, above its first row and below its last.
const FILTER_INSET: f32 = 4.0;

/// The height of a row of filter mode's list: a line of the terminal's
/// font with room around it.
pub fn filter_row_height(atlas: &Atlas) -> f32 {
    (atlas.metrics.line_height + 8.0).round().max(22.0)
}

/// Filter mode's list for `rows` rows, in the panel `rect`: under the
/// find bar `bar`, across the pane of the session with the keyboard to
/// the bar's right edge, as tall as its rows or down to the pane's foot.
pub fn filter_rect(
    panel: &Panel,
    rect: Viewport,
    bar: Viewport,
    rows: usize,
    atlas: &Atlas,
) -> Viewport {
    let pane = active_pane(panel, split(rect).1);
    let x = pane.x - PAD + FIND_BAR_MARGIN;
    let y = bar.y + bar.height + FIND_BAR_MARGIN;
    // Whole rows only, so the list ends where its last row does.
    let row_h = filter_row_height(atlas);
    let fit = ((pane.y + pane.height - y - FILTER_INSET * 2.0) / row_h)
        .floor()
        .max(0.0) as usize;
    let room = fit as f32 * row_h + FILTER_INSET * 2.0;
    let wanted = rows.clamp(1, 10_000) as f32 * row_h + FILTER_INSET * 2.0;
    Viewport {
        x,
        y,
        width: (bar.x + bar.width - x).max(0.0),
        height: wanted.min(room),
    }
}

/// How many rows filter mode's `list` shows at once.
pub fn filter_visible(list: Viewport, atlas: &Atlas) -> usize {
    ((list.height - FILTER_INSET * 2.0) / filter_row_height(atlas))
        .floor()
        .max(0.0) as usize
}

/// The `n`th row showing in filter mode's `list`.
pub fn filter_row_rect(list: Viewport, n: usize, atlas: &Atlas) -> Viewport {
    let height = filter_row_height(atlas);
    Viewport {
        x: list.x + FILTER_INSET,
        y: list.y + FILTER_INSET + n as f32 * height,
        width: (list.width - FILTER_INSET * 2.0).max(0.0),
        height,
    }
}

/// The first of `rows` rows filter mode's list shows, `visible` at a
/// time, scrolled to `scroll`.
pub fn filter_first(scroll: usize, rows: usize, visible: usize) -> usize {
    scroll.min(rows.saturating_sub(visible))
}

/// Filter mode's list in `list`: one row per line with a match, its
/// number in the session and its text in the terminal's font, each match
/// on it marked, those of the current match's line in the stronger
/// colour, and the row chosen lit. Only the rows showing are laid out.
pub fn draw_filter(
    find: &Find,
    atlas: &mut Atlas,
    list: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    layout::push_panel(out, list, layout::UI_RADIUS_LG, theme);
    let row_h = filter_row_height(atlas);
    if find.rows.is_empty() {
        let says = if find.error.is_some() {
            "Not a valid regular expression"
        } else if find.searching() {
            "Searching\u{2026}"
        } else {
            "No matching lines"
        };
        let row = filter_row_rect(list, 0, atlas);
        layout::push_ui_text_elided(
            out,
            atlas,
            Viewport {
                x: row.x + 8.0,
                width: (row.width - 16.0).max(0.0),
                ..row
            },
            says,
            theme.status_text,
        );
        return;
    }
    let filter = find.filter.unwrap_or_default();
    let visible = filter_visible(list, atlas);
    let first = filter_first(filter.scroll, find.rows.len(), visible);
    let shown = &find.rows[first..(first + visible).min(find.rows.len())];
    let m = atlas.metrics;
    let advance = m.advance;
    let number = |row: &FilterRow| (row.line.saturating_sub(find.first_line) + 1).to_string();
    // The numbers right-aligned in a gutter as wide as the widest showing.
    let digits = shown.iter().map(|r| number(r).len()).max().unwrap_or(1);
    let gutter = digits as f32 * advance + 14.0;
    for (n, row) in shown.iter().enumerate() {
        let index = first + n;
        let rect = filter_row_rect(list, n, atlas);
        if index == filter.selected {
            layout::push_rounded_rect(out, rect, layout::UI_RADIUS, theme.palette_selected);
        } else if layout::hovered(rect) {
            layout::push_rounded_rect(out, rect, layout::UI_RADIUS, theme.row_hover);
        }
        layout::hotspot(rect, layout::Cursor::Pointing, None);
        let y = rect.y + ((row_h - m.line_height) * 0.5).round();
        let label = number(row);
        let label_x = rect.x + 6.0 + (digits - label.len()) as f32 * advance;
        layout::push_text(out, atlas, label_x, y, &label, theme.gutter_text);
        // As much of the text as the row has room for, from a little
        // before its first match when the whole would not fit.
        let x = rect.x + 6.0 + gutter;
        let room = ((rect.x + rect.width - 8.0 - x) / advance).floor().max(0.0) as usize;
        let chars: Vec<char> = row.text.chars().collect();
        let mut columns = Vec::with_capacity(chars.len() + 1);
        let mut column = 0;
        for ch in &chars {
            columns.push(column);
            column += crate::render::font::display_width(*ch);
        }
        columns.push(column);
        let skip = match row.marks.first() {
            Some(&(_, to)) if columns[to] > room => {
                let want = columns[to].saturating_sub(room * 2 / 3);
                columns.partition_point(|&c| c < want).min(chars.len())
            }
            _ => 0,
        };
        let fits = columns.partition_point(|&c| c <= columns[skip] + room) - 1;
        let end = fits.max(skip);
        let at = |i: usize| x + (columns[i] - columns[skip]) as f32 * advance;
        // The row the current match is on has its matches in the
        // stronger colour: it lies between the row's first match and the
        // next row's.
        let next = find.rows.get(index + 1).map_or(usize::MAX, |r| r.hit);
        let current = find.current.is_some_and(|c| c >= row.hit && c < next);
        let colour = if current {
            theme.find_current
        } else {
            theme.find_match
        };
        for &(from, to) in &row.marks {
            let (from, to) = (from.max(skip), to.min(end));
            if to <= from {
                continue;
            }
            layout::push_rounded_rect(
                out,
                Viewport {
                    x: at(from),
                    y: rect.y + 3.0,
                    width: at(to) - at(from),
                    height: row_h - 6.0,
                },
                3.0,
                colour,
            );
        }
        let text: String = chars[skip..end].iter().collect();
        layout::push_text(out, atlas, x, y, &text, theme.text);
    }
    if find.rows.len() > visible && visible > 0 {
        // An overlay scrollbar, as the palette's: a thumb in the trailing
        // gutter, sized by the share of rows in view.
        let track = visible as f32 * row_h - 8.0;
        let thumb = (track * visible as f32 / find.rows.len() as f32).max(24.0);
        let at = first as f32 / (find.rows.len() - visible) as f32;
        let [r, g, b, _] = theme.status_text;
        layout::push_rounded_rect(
            out,
            Viewport {
                x: list.x + list.width - 7.0,
                y: list.y + FILTER_INSET + 4.0 + (track - thumb) * at,
                width: 4.0,
                height: thumb,
            },
            2.0,
            [r, g, b, 0.45],
        );
    }
}

/// How far back to scroll so line `line` shows below the top `covered`
/// rows (under the find bar): as it is when it already does, otherwise
/// with the line in the middle of what is left, as far as the history
/// goes. `base` is the number of the screen's first line.
pub fn back_showing(
    line: u64,
    base: u64,
    back: usize,
    rows: usize,
    covered: usize,
    history: usize,
) -> usize {
    let shows = |back: usize| {
        let top = base.saturating_sub(back as u64);
        line >= top + covered as u64 && line < top + rows as u64
    };
    if shows(back) {
        return back;
    }
    // On the screen itself: back to following the output.
    if shows(0) {
        return 0;
    }
    let middle = (covered + rows.saturating_sub(covered) / 2) as u64;
    (base + middle).saturating_sub(line).min(history as u64) as usize
}

/// The lines of what came before shown above a prompt jumped to.
pub const PROMPT_CONTEXT: usize = 2;

/// How far back to scroll so line `line` shows `covered` +
/// [`PROMPT_CONTEXT`] rows down from the top, as far as the history goes:
/// a prompt jumped to, under what led to it. `base` is the number of the
/// screen's first line.
pub fn prompt_back(line: u64, base: u64, history: usize, covered: usize) -> usize {
    (base + (PROMPT_CONTEXT + covered) as u64)
        .saturating_sub(line)
        .min(history as u64) as usize
}

/// The width of the strip down a screen's right edge where its prompt
/// marks are, in the padding beside the grid.
const MARK_TRACK: f32 = 10.0;
/// A mark's height.
pub const TICK_HEIGHT: f32 = 2.0;
/// How near a mark a press has to be to land on it.
const TICK_REACH: f32 = 4.0;

/// The strip of prompt marks beside `screen`.
pub fn mark_track(screen: Viewport) -> Viewport {
    Viewport {
        x: screen.x + screen.width + PAD - MARK_TRACK,
        y: screen.y,
        width: MARK_TRACK,
        height: screen.height,
    }
}

/// The top of the mark for line `line`, out of `total` lines from line
/// `first`: its share of the history is its share of the track.
pub fn tick_y(track: Viewport, line: u64, first: u64, total: usize) -> f32 {
    let run = (track.height - TICK_HEIGHT).max(0.0);
    let at = line.saturating_sub(first) as f32 / total.max(1) as f32;
    track.y + run * at.clamp(0.0, 1.0)
}

/// The line of the mark nearest `y`, when a press there lands on one:
/// the hit test for [`tick_y`].
pub fn mark_at(
    track: Viewport,
    y: f32,
    lines: impl Iterator<Item = u64>,
    first: u64,
    total: usize,
) -> Option<u64> {
    lines
        .filter(|&line| line >= first)
        .map(|line| {
            let centre = tick_y(track, line, first, total) + TICK_HEIGHT / 2.0;
            (line, (centre - y).abs())
        })
        .filter(|&(_, off)| off <= TICK_REACH)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(line, _)| line)
}

/// The row, the column boundary nearest the point and the column of the
/// cell under it, for a point in `screen`, clamped into the grid.
pub fn point(
    atlas: &Atlas,
    screen: Viewport,
    x: f32,
    y: f32,
    cols: usize,
    rows: usize,
) -> (usize, usize, usize) {
    let advance = atlas.metrics.advance;
    let row = ((y - screen.y) / row_height(atlas)).floor().max(0.0) as usize;
    let at = ((x - screen.x) / advance).max(0.0);
    (
        row.min(rows.saturating_sub(1)),
        (at.round() as usize).min(cols),
        (at.floor() as usize).min(cols.saturating_sub(1)),
    )
}

/// The rows of the panel: the tab header and the screen.
pub fn split(rect: Viewport) -> (Viewport, Viewport) {
    let (header, rest) = rect.split_top(HEADER);
    let screen = Viewport {
        x: rest.x + PAD,
        y: rest.y + PAD * 0.5,
        width: (rest.width - PAD * 2.0).max(0.0),
        height: (rest.height - PAD).max(0.0),
    };
    (header, screen)
}

/// Between two panes: the padding of each and the line between them.
pub const PANE_GAP: f32 = PAD * 2.0 + 1.0;
/// How far into the padding round a pane what lights up for a drop
/// reaches: short of the strip above it and of the line beside it.
const DROP_GROW: f32 = PAD * 0.5 - 1.0;

/// What lights up for a drop on the screen `rect` of a pane, or on the
/// part of one a tab would get: that, and a little of the padding round
/// it.
pub fn lit_pane(rect: Viewport) -> Viewport {
    Viewport {
        x: rect.x - DROP_GROW,
        y: rect.y - DROP_GROW,
        width: rect.width + DROP_GROW * 2.0,
        height: rect.height + DROP_GROW * 2.0,
    }
}
/// How thick the focus line is, the rule over the pane with the keyboard.
/// It lies against the pane's first row, clear of the strip, and is half
/// the weight of the active tab's rule: under that tab the two were one
/// long line, and this one is the pane's.
const FOCUS_LINE: f32 = 1.0;
/// How far past the gap, into the pane on each side, a press still grabs
/// the divider.
const DIVIDER_SLOP: f32 = 2.0;
/// The smallest a pane is dragged to, in columns and rows.
pub const PANE_MIN_COLS: usize = 20;
pub const PANE_MIN_ROWS: usize = 5;

/// The least room a pane keeps along `dir` when a divider moves: the
/// columns of a split side by side, the rows of a stacked one.
pub fn pane_min(atlas: &Atlas, dir: Dir) -> f32 {
    match dir {
        Dir::Horizontal => PANE_MIN_COLS as f32 * atlas.metrics.advance,
        Dir::Vertical => PANE_MIN_ROWS as f32 * row_height(atlas),
    }
}

/// Whether `pane` has room to be cut in two at `edge` with each half still
/// the smallest a pane is dragged to.
fn can_cut(atlas: &Atlas, pane: Viewport, edge: Edge) -> bool {
    let (dir, extent) = match edge {
        Edge::Left | Edge::Right => (Dir::Horizontal, pane.width),
        Edge::Top | Edge::Bottom => (Dir::Vertical, pane.height),
    };
    extent >= pane_min(atlas, dir) * 2.0 + PANE_GAP
}

/// The height of one terminal row.
pub fn row_height(atlas: &Atlas) -> f32 {
    let m = atlas.metrics;
    m.snap(m.cell_height).max(1.0)
}

/// How many columns and rows fit in `screen`.
pub fn grid_size(atlas: &Atlas, screen: Viewport) -> (usize, usize) {
    let cols = (screen.width / atlas.metrics.advance).floor().max(2.0) as usize;
    let rows = (screen.height / row_height(atlas)).floor().max(1.0) as usize;
    (cols, rows)
}

/// The widest a tab is: a longer title is cut in its middle.
pub const TAB_MAX: f32 = 240.0;
/// The narrowest the tabs shrink to when they do not all fit, before the
/// strip scrolls instead: a short name such as "zsh" whole beside the
/// close button, and enough of both ends of a long title to tell it from
/// its neighbours.
pub const TAB_MIN: f32 = 120.0;
/// The least room a title cut short keeps beside its review count: the
/// ellipsis and a few letters.
const TITLE_MIN: f32 = 28.0;
/// How near an end of the strip a dragged tab scrolls it.
pub const STRIP_EDGE: f32 = 28.0;
/// The room a tab takes beside its title: the editor tab's padding and
/// close slot, so the two strips are one.
const TAB_CHROME: f32 = layout::TAB_PADDING_X * 2.0 + layout::TAB_CLOSE_SLOT;

/// The room a tab's dot takes before its title while it waits on someone.
fn dot_width(tab: &Tab) -> f32 {
    if tab.attention.is_some() { 5.0 } else { 0.0 }
}

/// One tab of the header, as [`strip`] places it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StripTab {
    /// What of it shows: nothing (no width) for a tab the header does not
    /// list or one scrolled out of sight.
    pub rect: Viewport,
    /// All of it, where it would be were none of it cut off.
    pub whole: Viewport,
    /// Its close button; no width unless the whole button shows.
    pub close: Viewport,
}

/// The header's tabs and its new-shell button for a frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Strip {
    /// One for each of the panel's tabs, listed or not.
    pub tabs: Vec<StripTab>,
    pub new: Viewport,
    /// Where the tabs go: the header up to the buttons at its end.
    pub area: Viewport,
    /// How far the tabs are scrolled, and the furthest they can be.
    pub scroll: f32,
    pub max_scroll: f32,
}

/// The widths of tabs `natural` wide in `room`. All as they are when they
/// fit. Otherwise the widest give way first, each tab over a cap cut to
/// it, the cap such that they fill the room and no lower than
/// [`TAB_MIN`]: past that they keep their widths and the strip scrolls.
fn fit_widths(natural: &[f32], room: f32) -> Vec<f32> {
    if natural.iter().sum::<f32>() <= room {
        return natural.to_vec();
    }
    let mut sorted = natural.to_vec();
    sorted.sort_by(f32::total_cmp);
    let mut left = room;
    let mut cap = TAB_MIN;
    for (n, width) in sorted.iter().enumerate() {
        let share = left / (sorted.len() - n) as f32;
        if *width > share {
            cap = share.floor().max(TAB_MIN);
            break;
        }
        left -= width;
    }
    natural.iter().map(|width| width.min(cap)).collect()
}

/// The header's tabs, their close buttons and the new-shell button.
///
/// Each tab is as wide as its label, [`TAB_MAX`] at most. When they do
/// not fit before the buttons they shrink (see [`fit_widths`]), and when
/// that is not enough the strip scrolls: by the wheel, by a drag held at
/// an end, and so that the active tab is in view unless the strip was
/// moved since it became the active one. The new-shell button follows
/// the last tab and stops before Hide Panel, so both always show and no
/// tab is under either.
pub fn strip(panel: &Panel, atlas: &mut Atlas, header: Viewport) -> Strip {
    let hide = header_hide(header);
    let new_at_most = (hide.x - layout::UI_GAP - layout::UI_CONTROL_SM).max(header.x);
    let area = Viewport {
        width: (new_at_most - layout::UI_GAP - header.x).max(0.0),
        ..header
    };
    let listed = panel.listed();
    let natural: Vec<f32> = listed
        .iter()
        .map(|i| {
            let tab = &panel.tabs[*i];
            (layout::ui_text_width(atlas, &label(tab)) + TAB_CHROME + dot_width(tab)).min(TAB_MAX)
        })
        .collect();
    let widths = fit_widths(&natural, area.width);
    let total: f32 = widths.iter().sum();
    let max_scroll = (total - area.width).max(0.0);
    let mut scroll = panel.strip_scroll.clamp(0.0, max_scroll);
    if !panel.strip_free
        && let Some(place) = listed.iter().position(|i| *i == panel.active)
    {
        let start: f32 = widths[..place].iter().sum();
        let end = start + widths[place];
        if start < scroll {
            scroll = start;
        } else if end > scroll + area.width {
            // Its start, where the strip is narrower than the tab.
            scroll = (end - area.width).min(start);
        }
        scroll = scroll.clamp(0.0, max_scroll);
    }
    let none = Viewport {
        width: 0.0,
        ..header
    };
    let mut tabs = vec![
        StripTab {
            rect: none,
            whole: none,
            close: none,
        };
        panel.tabs.len()
    ];
    let (left, right) = (area.x, area.x + area.width);
    let mut x = area.x - scroll;
    for (index, width) in listed.iter().zip(&widths) {
        let whole = Viewport {
            x,
            width: *width,
            ..header
        };
        x += width;
        let from = whole.x.max(left);
        let to = (whole.x + whole.width).min(right);
        if to <= from {
            tabs[*index].whole = whole;
            continue;
        }
        let centre = whole.x + whole.width - layout::TAB_PADDING_X - atlas.metrics.advance * 0.5;
        let close = Viewport {
            x: centre - layout::TAB_CLOSE_HIT * 0.5,
            y: header.y + ((header.height - layout::TAB_CLOSE_HIT) * 0.5).floor(),
            width: layout::TAB_CLOSE_HIT,
            height: layout::TAB_CLOSE_HIT,
        };
        let close_shows = close.x >= from && close.x + close.width <= to;
        // All of it, as it is, where none is cut off.
        let cut = from > whole.x || to < whole.x + whole.width;
        tabs[*index] = StripTab {
            rect: Viewport {
                x: from,
                width: if cut { to - from } else { whole.width },
                ..header
            },
            whole,
            close: if close_shows { close } else { none },
        };
    }
    let new = Viewport {
        x: (area.x + total).min(right) + layout::UI_GAP,
        y: header.y + ((header.height - layout::UI_CONTROL_SM) * 0.5).floor(),
        width: layout::UI_CONTROL_SM,
        height: layout::UI_CONTROL_SM,
    };
    Strip {
        tabs,
        new,
        area,
        scroll,
        max_scroll,
    }
}

/// The gap a tab dragged to `x` would drop into, counted along the tabs
/// in `listed`: before the first whose middle is past the pointer, else
/// after the last. From the frame's own rectangles, as the labels under
/// the pointer change width while it is held.
pub fn drop_slot(strip: &Strip, listed: &[usize], x: f32) -> usize {
    listed
        .iter()
        .position(|i| {
            let tab = strip.tabs[*i].whole;
            x < tab.x + tab.width * 0.5
        })
        .unwrap_or(listed.len())
}

/// Where the line for a drop into `slot` goes: the edge between the two
/// tabs it would land between, kept inside the strip.
fn drop_line(strip: &Strip, listed: &[usize], slot: usize) -> Option<f32> {
    let x = match listed.get(slot) {
        Some(next) => strip.tabs[*next].whole.x,
        None => {
            let last = strip.tabs[*listed.last()?].whole;
            last.x + last.width
        }
    };
    let area = strip.area;
    Some(x.clamp(area.x + 1.0, (area.x + area.width - 1.0).max(area.x + 1.0)))
}

/// Which way a tab dragged to `x` scrolls a strip that does not fit:
/// back within [`STRIP_EDGE`] of its start or before it, on within as
/// much of its end or past it, and not at all between.
pub fn drag_scroll(strip: &Strip, x: f32) -> f32 {
    let area = strip.area;
    if strip.max_scroll <= 0.0 {
        0.0
    } else if x < area.x + STRIP_EDGE {
        -1.0
    } else if x > area.x + area.width - STRIP_EDGE {
        1.0
    } else {
        0.0
    }
}

/// Each tab and its close button, then the new-shell button: what of the
/// strip takes a press. See [`strip`].
pub fn header_hits(
    panel: &Panel,
    atlas: &mut Atlas,
    header: Viewport,
) -> (Vec<(Viewport, Viewport)>, Viewport) {
    let strip = strip(panel, atlas, header);
    let tabs = strip.tabs.iter().map(|tab| (tab.rect, tab.close)).collect();
    (tabs, strip.new)
}

/// What the screen says when the workspace listed has no terminal left.
fn empty_state() -> layout::EmptyState<'static> {
    layout::EmptyState::new(
        "No terminal in this workspace",
        "\u{2318}T starts one here.",
    )
    .icon(icons::TERMINAL)
    .button("New Terminal", Some(icons::ADD))
}

/// The New Terminal button of the empty screen, while the panel shows no
/// session: where [`draw`] puts it, for the window to act on.
pub fn empty_hit(panel: &Panel, atlas: &mut Atlas, screen: Viewport) -> Option<Viewport> {
    if panel.showing() {
        return None;
    }
    empty_state().layout(atlas, screen).button
}

/// Hide Panel, at the header's trailing edge.
pub fn header_hide(header: Viewport) -> Viewport {
    let size = layout::UI_CONTROL_SM;
    Viewport {
        x: header.x + header.width - layout::UI_INSET + 4.0 - size,
        y: header.y + ((header.height - size) * 0.5).floor(),
        width: size,
        height: size,
    }
}

/// The title the program set, when it set one (`claude` does, and so do
/// most shell prompts), otherwise the name the tab started with.
fn label(tab: &Tab) -> String {
    let title = shown_title(tab);
    match tab.review_files {
        0 => title,
        n => format!("{title} \u{b7} {n} to review"),
    }
}

/// A tab's title without its review count: inside ssh the tab says where.
fn shown_title(tab: &Tab) -> String {
    match &tab.remote {
        Some(remote) => format!("ssh {}", remote.destination),
        None => title_of(tab),
    }
}

/// A tab's label as `room` has it: whole when it fits, else its title cut
/// in the middle. The count of files to review stays whole after a title
/// cut short while that leaves the title [`TITLE_MIN`], then as the number
/// alone after whatever of the title there is room for, and is cut with
/// the rest only where not even the number fits.
fn fitted_label(atlas: &mut Atlas, tab: &Tab, room: f32) -> String {
    let title = shown_title(tab);
    let count = tab.review_files;
    let whole = match count {
        0 => title.clone(),
        n => format!("{title} \u{b7} {n} to review"),
    };
    // Half a point of slack: the room is the label's own width taken back
    // out of its tab's, a rounding error short of it.
    if layout::ui_text_width(atlas, &whole) <= room + 0.5 {
        return whole;
    }
    if count > 0 {
        let words = format!(" \u{b7} {count} to review");
        let left = room - layout::ui_text_width(atlas, &words);
        if left >= TITLE_MIN {
            return layout::elide_middle(atlas, &title, left) + &words;
        }
        let number = format!(" \u{b7} {count}");
        let left = room - layout::ui_text_width(atlas, &number);
        let cut = layout::elide_middle(atlas, &title, left);
        if !cut.is_empty() {
            return cut + &number;
        }
    }
    layout::elide_middle(atlas, &whole, room)
}

fn title_of(tab: &Tab) -> String {
    let term = tab.session.term.lock().unwrap_or_else(|e| e.into_inner());
    titled(tab, &term)
}

fn titled(tab: &Tab, term: &term::Term) -> String {
    let title = term.title.trim();
    if title.is_empty() {
        tab.title.clone()
    } else {
        title.chars().take(40).collect()
    }
}

/// The 16 named colours on the Graphite background.
const NAMED: [[f32; 4]; 16] = [
    [0.231, 0.243, 0.255, 1.0],
    [0.886, 0.431, 0.408, 1.0],
    [0.596, 0.812, 0.624, 1.0],
    [0.902, 0.784, 0.471, 1.0],
    [0.482, 0.643, 0.902, 1.0],
    [0.788, 0.561, 0.871, 1.0],
    [0.467, 0.804, 0.808, 1.0],
    [0.800, 0.816, 0.820, 1.0],
    [0.455, 0.475, 0.494, 1.0],
    [1.000, 0.541, 0.510, 1.0],
    [0.706, 0.902, 0.710, 1.0],
    [1.000, 0.871, 0.561, 1.0],
    [0.612, 0.757, 1.000, 1.0],
    [0.878, 0.667, 0.957, 1.0],
    [0.569, 0.906, 0.906, 1.0],
    [0.965, 0.969, 0.973, 1.0],
];

fn rgb(color: Color, default: [f32; 4]) -> [f32; 4] {
    match color {
        Color::Default => default,
        Color::Indexed(i) => term::palette(i, &NAMED),
        Color::Rgb(r, g, b) => [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0],
    }
}

/// A cell's text and background colours, attributes applied.
fn colours(cell: &Cell, theme: &Theme, background: [f32; 4]) -> ([f32; 4], [f32; 4]) {
    let fg = match cell.fg {
        // Bold brightens the eight base colours, as terminals have always done.
        Color::Indexed(i) if i < 8 && cell.flags & BOLD != 0 => NAMED[i as usize + 8],
        other => rgb(other, theme.text),
    };
    let bg = rgb(cell.bg, background);
    let (mut fg, bg) = if cell.flags & INVERSE != 0 {
        (bg, fg)
    } else {
        (fg, bg)
    };
    if cell.flags & DIM != 0 {
        fg[3] *= 0.6;
    }
    if cell.flags & HIDDEN != 0 {
        fg = bg;
    }
    (fg, bg)
}

/// The panel: header, then the active session's screen, with `find`'s
/// matches in it while the find bar searches it.
pub fn draw(
    panel: &Panel,
    find: Option<&Find>,
    atlas: &mut Atlas,
    rect: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    let background = [
        theme.background[0] as f32,
        theme.background[1] as f32,
        theme.background[2] as f32,
        1.0,
    ];
    layout::push_rect(
        out,
        atlas,
        [rect.x, rect.y],
        [rect.width, rect.height],
        background,
    );
    let (header, screen) = split(rect);
    layout::push_rect(
        out,
        atlas,
        [header.x, header.y],
        [header.width, header.height],
        theme.sidebar_background,
    );
    layout::push_rect(
        out,
        atlas,
        [rect.x, rect.y],
        [rect.width, 1.0],
        theme.hairline,
    );

    let strip = strip(panel, atlas, header);
    let listed = panel.listed();
    // A tab on its way to another place along the strip, or off it onto
    // a pane: its name then goes with the pointer, as the strip has it.
    let drag = panel.tab_drag.filter(|drag| drag.active);
    let lifted = drag.filter(|drag| drag.pane);
    let mut lifted_name = None;
    let first = out.len();
    for (place, index) in listed.iter().enumerate() {
        let (index, tab) = (*index, &panel.tabs[*index]);
        let StripTab { rect, whole, close } = strip.tabs[index];
        if rect.width <= 0.0 {
            continue;
        }
        // The same tab as the editor's: the strip's height, the page's
        // fill under the active one with the accent rule where it meets
        // the screen, a lift under the pointer, a hairline between the rest.
        let active = index == panel.active;
        // The one being dragged is faint where it still is: the line
        // says where it goes.
        let faint = |colour: [f32; 4]| {
            if drag.is_some_and(|drag| drag.tab == index) {
                [colour[0], colour[1], colour[2], colour[3] * 0.4]
            } else {
                colour
            }
        };
        let hovered = drag.is_none() && layout::hovered(rect);
        if active {
            layout::push_rect(
                out,
                atlas,
                [rect.x, rect.y],
                [rect.width, rect.height],
                theme.tab_active,
            );
            layout::push_rect(
                out,
                atlas,
                [
                    whole.x + layout::TAB_PADDING_X,
                    whole.y + whole.height - 2.0,
                ],
                [whole.width - layout::TAB_PADDING_X * 2.0, 2.0],
                faint(theme.accent),
            );
        } else if hovered {
            layout::push_rect(
                out,
                atlas,
                [rect.x, rect.y],
                [rect.width, rect.height],
                theme.row_hover,
            );
        } else if place > 0 && listed[place - 1] != panel.active {
            layout::push_rect(
                out,
                atlas,
                [whole.x, whole.y + 6.0],
                [1.0, whole.height - 12.0],
                theme.hairline,
            );
        }
        layout::hotspot(rect, layout::Cursor::Pointing, None);
        let mut label_x = whole.x + layout::TAB_PADDING_X;
        // A tab waiting on the person: a dot before its title.
        if tab.attention.is_some() {
            let dot = Viewport {
                x: label_x - 5.0,
                y: whole.y + whole.height / 2.0 - 3.0,
                width: 6.0,
                height: 6.0,
            };
            // Whole or not at all: a round thing cut at the strip's end
            // is no longer round.
            if dot.x >= rect.x && dot.x + dot.width <= rect.x + rect.width {
                layout::push_rounded_rect(out, dot, 3.0, faint(theme.accent));
            }
            label_x += 5.0;
        }
        let colour = if tab.claude {
            theme.accent
        } else if active {
            theme.tab_text
        } else {
            theme.tab_text_inactive
        };
        let room =
            (whole.x + whole.width - layout::TAB_PADDING_X - layout::TAB_CLOSE_SLOT - label_x)
                .max(0.0);
        let text = fitted_label(atlas, tab, room);
        layout::push_ui_text_elided(
            out,
            atlas,
            Viewport {
                x: label_x,
                width: room,
                ..whole
            },
            &text,
            faint(colour),
        );
        if lifted.is_some_and(|drag| drag.tab == index) {
            lifted_name = Some(text);
        }
        // The close cross on the tab that is showing and the one under the
        // pointer, the way editor tabs do it; not on one half out of sight.
        if (active || hovered) && drag.is_none() && close.width > 0.0 {
            Button::new(close)
                .icon(icons::CLOSE)
                .tone(Tone::Ghost)
                .radius(layout::UI_RADIUS_SM)
                .tip("Close Terminal")
                .draw(out, atlas, theme);
        }
    }
    // A tab scrolled half out of sight ends where the strip does.
    let (left, right) = (strip.area.x, strip.area.x + strip.area.width);
    for quad in &mut out[first..] {
        layout::clip_horizontal(quad, left, right);
    }
    // Where the dragged tab lands if let go here: a line in the accent
    // between the two it would go between. None with the pointer off the
    // strip, where letting go leaves it where it was.
    if let Some(drag) = drag
        && header.contains(drag.x, drag.y)
        && let Some(x) = drop_line(&strip, &listed, drop_slot(&strip, &listed, drag.x))
    {
        layout::push_rect(
            out,
            atlas,
            [x - 1.0, header.y + 4.0],
            [2.0, header.height - 8.0],
            theme.accent,
        );
    }
    Button::new(strip.new)
        .icon(icons::ADD)
        .tone(Tone::Ghost)
        .tip("New Terminal")
        .draw(out, atlas, theme);
    Button::new(header_hide(header))
        .icon(icons::CHEVRON_DOWN)
        .tone(Tone::Ghost)
        .tip("Hide Panel  \u{2303}`")
        .draw(out, atlas, theme);

    let screens = panel.screens(screen);
    if screens.len() > 1 {
        // A line down the middle of each gap, run on into the padding or
        // the gap at its ends so that it meets the line it stops at. The
        // one held is in the accent.
        let (left, right) = (screen.x - PAD, screen.x + screen.width + PAD);
        let (top, bottom) = (screen.y - PAD * 0.5, screen.y + screen.height + PAD * 0.5);
        for divider in panel.dividers(screen) {
            let held = panel
                .divider_drag
                .as_ref()
                .is_some_and(|drag| drag.id == divider.id);
            let colour = if held { theme.accent } else { theme.hairline };
            let gap = divider.rect;
            let (pos, size) = match divider.dir {
                Dir::Horizontal => {
                    let from = (gap.y - PAD).max(top);
                    let to = (gap.y + gap.height + PAD).min(bottom);
                    ([(gap.x + gap.width * 0.5).floor(), from], [1.0, to - from])
                }
                Dir::Vertical => {
                    let from = (gap.x - PAD).max(left);
                    let to = (gap.x + gap.width + PAD).min(right);
                    ([from, (gap.y + gap.height * 0.5).floor()], [to - from, 1.0])
                }
            };
            layout::push_rect(out, atlas, pos, size, colour);
        }
        // The pane with the keyboard: a thin line over its screen,
        // against its first row.
        if let Some((_, focused)) = screens.iter().find(|(tab, _)| *tab == panel.active) {
            layout::push_rect(
                out,
                atlas,
                [focused.x, focused.y - FOCUS_LINE],
                [focused.width, FOCUS_LINE],
                if panel.focus {
                    theme.accent
                } else {
                    theme.hairline
                },
            );
        }
    }
    // The workspace listed has no terminal left: the one at the front is
    // another's, and is not shown in its place.
    if !panel.showing() {
        layout::push_empty_state(out, atlas, screen, empty_state(), theme);
        return;
    }
    for (index, screen) in screens {
        let find = find.filter(|_| index == panel.active);
        draw_screen(panel, find, index, screen, atlas, theme, background, out);
    }
    // A tab taken off the strip: the room it would get lit up over the
    // screens, a little into the padding round it, and its name by the
    // pointer over that.
    if let Some(drag) = lifted {
        if let Some(drop) = drag.drop {
            layout::push_drop_zone(out, atlas, lit_pane(drop.rect), theme);
        }
        let name = lifted_name.or_else(|| {
            let tab = panel.tabs.get(drag.tab)?;
            Some(fitted_label(atlas, tab, TAB_MAX - TAB_CHROME))
        });
        if let Some(name) = name {
            let body = Viewport {
                y: header.y + header.height,
                height: (rect.height - header.height).max(0.0),
                ..rect
            };
            layout::push_drag_chip(out, atlas, (drag.x, drag.y), body, &name, theme);
        }
    }
}

/// One session's screen: its cells, its selection, its cursor, and how
/// far back it is scrolled. Only what fits in `screen` is drawn: a grid
/// larger than its pane (a divider is being dragged, or the session has
/// not been told its new size yet) is cut to whole cells, the rows kept
/// being those down to the cursor's, and a smaller one leaves the rest of
/// the pane empty.
#[allow(clippy::too_many_arguments)]
fn draw_screen(
    panel: &Panel,
    find: Option<&Find>,
    index: usize,
    screen: Viewport,
    atlas: &mut Atlas,
    theme: &Theme,
    background: [f32; 4],
    out: &mut Vec<GlyphInstance>,
) {
    let Some(tab) = panel.tabs.get(index) else {
        return;
    };
    let active = index == panel.active;
    let m = atlas.metrics;
    let advance = m.advance;
    let row_h = row_height(atlas);
    // push_text centres a glyph in an editor line; move it up to sit in a
    // terminal row instead.
    let text_dy = -m.glyph_dy(m.line_height) + m.glyph_dy(row_h);
    // What is on screen, copied out under the lock and drawn after it is
    // let go: the reader thread takes the same lock for every burst of
    // output, and a frame's glyph work would hold it up.
    let fit_cols = (screen.width / advance).floor().max(0.0) as usize;
    let (rows, cols, back, visible, cursor, cursor_visible, kept, top) = {
        let mut term = tab.session.term.lock().unwrap_or_else(|e| e.into_inner());
        let byte = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        let rgb = |c: [f32; 4]| [byte(c[0] as f64), byte(c[1] as f64), byte(c[2] as f64)];
        term.colors = (
            rgb(theme.text),
            [
                byte(theme.background[0]),
                byte(theme.background[1]),
                byte(theme.background[2]),
            ],
        );
        let rows = term.rows().min((screen.height / row_h).floor() as usize);
        let back = tab.view.back.min(term.scrollback_len());
        // The first row drawn: past the top when the grid is taller than
        // the pane, far enough that the cursor's row is the last.
        let top = if back == 0 {
            (term.cursor().0 + 1).min(term.rows()).saturating_sub(rows)
        } else {
            0
        };
        let visible: Vec<(Vec<crate::term::Cell>, u64)> = (top..top + rows)
            .map(|row| {
                (
                    term.visible_row(row, back).to_vec(),
                    term.view_line(row, back),
                )
            })
            .collect();
        (
            rows,
            term.cols().min(fit_cols),
            back,
            visible,
            term.cursor(),
            term.modes.cursor_visible,
            // What the prompt marks are placed in: the first line kept and
            // how many there are. None on a full-screen program's screen.
            (!term.is_alternate()).then(|| {
                (
                    term.first_line(),
                    term.scrollback_len() + term.rows(),
                    term.view_line(top, back),
                )
            }),
            top,
        )
    };
    // Find's matches on the rows showing, the current one marked.
    let mut found: Vec<(u64, usize, usize, bool)> = Vec::new();
    if let (Some(find), Some((_, first)), Some((_, last))) = (find, visible.first(), visible.last())
    {
        let from = find
            .matches
            .partition_point(|h| h.spans.last().is_some_and(|s| s.0 < *first));
        for (i, hit) in find.matches.iter().enumerate().skip(from) {
            if hit.start.0 > *last {
                break;
            }
            let current = find.current == Some(i);
            found.extend(hit.spans.iter().map(|&(l, a, b)| (l, a, b, current)));
        }
    }
    let mut run = String::new();
    let cells_from = out.len();
    for (row, (cells, line)) in visible.iter().enumerate() {
        let line = *line;
        let y = screen.y + row as f32 * row_h;
        // What of the row the screen has room for.
        let room = cols.min(cells.len());
        // Backgrounds, merged into runs of one colour.
        let mut col = 0;
        while col < room {
            let (_, bg) = colours(&cells[col], theme, background);
            let start = col;
            while col < room && colours(&cells[col], theme, background).1 == bg {
                col += 1;
            }
            if bg != background {
                layout::push_rect(
                    out,
                    atlas,
                    [screen.x + start as f32 * advance, y],
                    [(col - start) as f32 * advance, row_h],
                    bg,
                );
            }
        }
        // Find's matches, as the editor draws them: the current one in
        // the stronger colour, so "2 of 7" can be seen as well as read.
        for &(_, from, to, current) in found.iter().filter(|f| f.0 == line) {
            let to = to.min(cols);
            if to <= from {
                continue;
            }
            let x = screen.x + from as f32 * advance;
            let width = (to - from) as f32 * advance;
            let colour = if current {
                theme.find_current
            } else {
                theme.find_match
            };
            layout::push_rect(out, atlas, [x, y], [width, row_h], colour);
        }
        // The selection, over the backgrounds and under the text.
        if let Some((a, b)) = tab.view.selection {
            let (start, end) = if a <= b { (a, b) } else { (b, a) };
            if (start.0..=end.0).contains(&line) {
                let from = if line == start.0 { start.1 } else { 0 };
                let to = if line == end.0 { end.1.min(cols) } else { cols };
                if to > from {
                    layout::push_rect(
                        out,
                        atlas,
                        [screen.x + from as f32 * advance, y],
                        [(to - from) as f32 * advance, row_h],
                        theme.selection,
                    );
                }
            }
        }
        // Text in runs of one colour and decoration.
        let mut col = 0;
        while col < room {
            let first = cells[col];
            let (fg, _) = colours(&first, theme, background);
            let decoration = first.flags & (UNDERLINE | STRIKE);
            let start = col;
            run.clear();
            while col < room {
                let cell = cells[col];
                if cell.flags & WIDE_TAIL == 0 {
                    if colours(&cell, theme, background).0 != fg
                        || cell.flags & (UNDERLINE | STRIKE) != decoration
                    {
                        break;
                    }
                    run.push(cell.ch);
                }
                col += 1;
            }
            if col == start {
                col += 1;
                continue;
            }
            let x = screen.x + start as f32 * advance;
            if run.chars().any(|c| c != ' ') {
                layout::push_text(out, atlas, x, y + text_dy, &run, fg);
            }
            let width = (col - start) as f32 * advance;
            // The font's own underline, on whole device pixels, inside the
            // row; a strike through the middle of the lower-case letters.
            let line = m.underline_thickness;
            let baseline = y + text_dy + m.glyph_dy(m.line_height) + m.ascent;
            if decoration & UNDERLINE != 0 {
                let under = m
                    .snap(baseline + m.underline_position - line * 0.5)
                    .min(y + row_h - line);
                layout::push_rect(out, atlas, [x, under], [width, line], fg);
            }
            if decoration & STRIKE != 0 {
                let through = m.snap(baseline - m.ascent * 0.3 - line * 0.5);
                layout::push_rect(out, atlas, [x, through], [width, line], fg);
            }
        }
        // The link a Command-click would open, underlined in the accent,
        // with the pointing hand over it.
        let link = panel.link.as_ref().filter(|(tab, _)| *tab == index);
        for &(_, from, to) in link
            .iter()
            .flat_map(|(_, l)| &l.spans)
            .filter(|s| s.0 == line)
        {
            let to = to.min(cols);
            if to <= from {
                continue;
            }
            let x = screen.x + from as f32 * advance;
            let width = (to - from) as f32 * advance;
            let thickness = m.underline_thickness;
            let baseline = y + text_dy + m.glyph_dy(m.line_height) + m.ascent;
            let under = m
                .snap(baseline + m.underline_position - thickness * 0.5)
                .min(y + row_h - thickness);
            layout::push_rect(out, atlas, [x, under], [width, thickness], theme.accent);
            layout::hotspot(
                Viewport {
                    x,
                    y,
                    width,
                    height: row_h,
                },
                layout::Cursor::Pointing,
                None,
            );
        }
    }
    // A glyph wider than its cell, in the last column a narrow pane has
    // room for, stops short of the pane beside it.
    let edge = PAD * 0.5;
    for quad in &mut out[cells_from..] {
        layout::clip_horizontal(quad, screen.x - edge, screen.x + screen.width + edge);
    }
    // The cursor, where the program left it, when it is showing.
    let (row, col) = cursor;
    if back == 0
        && cursor_visible
        && (top..top + rows).contains(&row)
        && col <= cols
        && !tab.session.has_exited()
    {
        let x = screen.x + col as f32 * advance;
        let y = screen.y + (row - top) as f32 * row_h;
        if panel.focus && active {
            layout::push_rect(out, atlas, [x, y], [2.0, row_h], theme.cursor);
        } else {
            let c = theme.cursor;
            layout::push_rect(out, atlas, [x, y], [1.0, row_h], [c[0], c[1], c[2], 0.5]);
        }
    }
    if back > 0 {
        let note = format!("↑ {back} lines");
        // Under the find bar while it is open, which sits where the
        // pill does.
        let below_bar = if find.is_some() && active {
            FIND_BAR_MARGIN * 2.0 + layout::FIND_ROW_HEIGHT - PAD * 0.5
        } else {
            0.0
        };
        // The shared pill, at the row's end and never wider than it: a
        // narrow pane cuts the words, not the pill at its edge.
        let row = Viewport {
            y: screen.y + below_bar,
            height: 22.0,
            ..screen
        };
        let (_, width) = layout::tag_fit(atlas, &note, row.width);
        if width > 0.0 {
            let x = row.x + row.width - width;
            // Over the session's own text, which a tint alone lets through.
            let under = Viewport {
                x,
                y: row.y + 3.0,
                width,
                height: 16.0,
            };
            layout::push_rounded_rect(out, under, layout::UI_RADIUS_SM, background);
            layout::push_tag(out, atlas, x, row, &note, theme.status_text);
        }
    }
    if let Some((first, total, top)) = kept {
        let place = (first, total, top, rows);
        let has_marks = tab.marks.iter().any(|m| m.line >= first);
        let has_finds = find.is_some_and(|f| f.matches.iter().any(|h| h.start.0 >= first));
        if has_marks || has_finds {
            draw_track_band(screen, place, theme, out);
        }
        if has_marks {
            draw_marks(tab, screen, (first, total), atlas, theme, out);
        }
        if let Some(find) = find.filter(|_| has_finds) {
            draw_find_ticks(find, screen, place, atlas, theme, out);
        }
    }
}

/// The faint band down the track for what the view shows, as the
/// editor's scrollbar draws its thumb: under the prompt marks and the
/// find ticks, so each is read against where the view is.
fn draw_track_band(
    screen: Viewport,
    (first, total, top, rows): (u64, usize, u64, usize),
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    let track = mark_track(screen);
    let [r, g, b, _] = theme.status_text;
    let from = tick_y(track, top, first, total);
    let to = tick_y(track, top + rows as u64, first, total) + TICK_HEIGHT;
    let band = Viewport {
        x: track.x + 3.0,
        y: from,
        width: track.width - 4.0,
        height: (to - from).max(TICK_HEIGHT * 2.0),
    };
    layout::push_rounded_rect(out, band, 2.0, [r, g, b, 0.12]);
}

/// Find's matches down the track: a tick in the match colour where each
/// is in the history, the current one wider and in the stronger colour,
/// so a search over a long session shows where it hit before the bar is
/// stepped through. A press on one goes to it ([`find_tick_at`]).
fn draw_find_ticks(
    find: &Find,
    screen: Viewport,
    (first, total, _, _): (u64, usize, u64, usize),
    atlas: &Atlas,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    let track = mark_track(screen);
    let mut last = f32::NEG_INFINITY;
    let [r, g, b, _] = theme.find_match;
    for (i, hit) in find.matches.iter().enumerate() {
        if hit.start.0 < first {
            continue;
        }
        let y = tick_y(track, hit.start.0, first, total);
        let current = find.current == Some(i);
        // Matches closer than a point draw as one; the current one always.
        if y - last < 1.0 && !current {
            continue;
        }
        last = y;
        if current {
            layout::push_rect(
                out,
                atlas,
                [track.x, y - 1.0],
                [track.width, TICK_HEIGHT + 2.0],
                theme.find_current,
            );
        } else {
            layout::push_rect(
                out,
                atlas,
                [track.x + 2.0, y],
                [track.width - 2.0, TICK_HEIGHT],
                [r, g, b, 0.9],
            );
        }
        layout::hotspot(
            Viewport {
                x: track.x,
                y: y + TICK_HEIGHT / 2.0 - TICK_REACH,
                width: track.width,
                height: TICK_REACH * 2.0,
            },
            layout::Cursor::Pointing,
            None,
        );
    }
}

/// The match whose tick is nearest `y` on the track, when a press there
/// lands on one: the first of the matches on that line.
pub fn find_tick_at(
    track: Viewport,
    y: f32,
    matches: &[Hit],
    first: u64,
    total: usize,
) -> Option<usize> {
    let line = mark_at(track, y, matches.iter().map(|h| h.start.0), first, total)?;
    matches.iter().position(|h| h.start.0 == line)
}

/// The prompts sent to a tab's agent, down its screen's right edge: a
/// tick in the accent for each, where it is in the history, over a faint
/// band for what the view shows, as the editor's scrollbar draws its
/// thumb. Each tick names its prompt under the pointer. Nothing when the
/// tab has no prompt still in its history.
fn draw_marks(
    tab: &Tab,
    screen: Viewport,
    (first, total): (u64, usize),
    atlas: &Atlas,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    if !tab.marks.iter().any(|m| m.line >= first) {
        return;
    }
    let track = mark_track(screen);
    let mut last = f32::NEG_INFINITY;
    for mark in tab.marks.iter().filter(|m| m.line >= first) {
        let y = tick_y(track, mark.line, first, total);
        // Prompts closer than a point draw, and answer, as one.
        if y - last < 1.0 {
            continue;
        }
        last = y;
        layout::push_rect(
            out,
            atlas,
            [track.x + 2.0, y],
            [track.width - 2.0, TICK_HEIGHT],
            theme.accent,
        );
        layout::hotspot(
            Viewport {
                x: track.x,
                y: y + TICK_HEIGHT / 2.0 - TICK_REACH,
                width: track.width,
                height: TICK_REACH * 2.0,
            },
            layout::Cursor::Pointing,
            (!mark.text.is_empty()).then_some(mark.text.as_str()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::search::Options;

    fn find(t: &term::Term, query: &str, options: Options) -> Vec<Hit> {
        find_in(&t.haystack(), query, options).unwrap()
    }

    #[test]
    fn an_agent_shells_environment_stays_out_of_new_sessions() {
        // What a Codex shell had set in the crc that lost its colours.
        let names = [
            "NO_COLOR",
            "PAGER",
            "GIT_PAGER",
            "GH_PAGER",
            "SHLVL",
            "CODEX_CI",
            "CLAUDECODE",
            "CLAUDE_CODE_ENTRYPOINT",
            "HOME",
            "PATH",
            "SSH_AUTH_SOCK",
            "CLAUDE_EFFORT",
            "EDITOR",
        ];
        let unset = inherited_unset(names.map(String::from));
        assert_eq!(
            unset,
            [
                "NO_COLOR",
                "PAGER",
                "GIT_PAGER",
                "GH_PAGER",
                "SHLVL",
                "CODEX_CI",
                "CLAUDECODE",
                "CLAUDE_CODE_ENTRYPOINT",
            ]
        );
    }

    #[test]
    fn a_match_across_a_wrap_covers_both_rows() {
        let mut t = term::Term::new(10, 4);
        t.advance(b"abcdefghijklm\r\nshort hijk");
        let first = t.view_line(0, 0);
        let found = find(&t, "hijk", Options::default());
        assert_eq!(
            found,
            [
                Hit {
                    start: (first, 7),
                    spans: vec![(first, 7, 10), (first + 1, 0, 1)],
                },
                Hit {
                    start: (first + 2, 6),
                    spans: vec![(first + 2, 6, 10)],
                },
            ]
        );
        // A line that ended is not joined to the next.
        assert!(find(&t, "mshort", Options::default()).is_empty());
        assert_eq!(find(&t, "m\nshort", Options::default()).len(), 1);
    }

    #[test]
    fn wide_characters_cover_two_columns() {
        let mut t = term::Term::new(8, 3);
        t.advance("漢字 x漢".as_bytes());
        let line = t.view_line(0, 0);
        let found = find(&t, "字 x", Options::default());
        assert_eq!(found[0].spans, vec![(line, 2, 6)]);
        let found = find(&t, "漢", Options::default());
        assert_eq!(
            found.iter().map(|h| h.spans[0]).collect::<Vec<_>>(),
            [(line, 0, 2), (line, 6, 8)]
        );
    }

    #[test]
    fn case_follows_the_option() {
        let mut t = term::Term::new(20, 2);
        t.advance(b"Error error ERROR");
        assert_eq!(find(&t, "error", Options::default()).len(), 3);
        let case = Options {
            case_sensitive: true,
            ..Options::default()
        };
        assert_eq!(find(&t, "error", case).len(), 1);
        let regex = Options {
            regex: true,
            ..Options::default()
        };
        assert!(find_in(&t.haystack(), "(", regex).is_err());
    }

    #[test]
    fn history_and_screen_are_searched_up_to_the_limit() {
        let mut t = term::Term::new(100, 2);
        for _ in 0..200 {
            t.advance(&[b'a'; 60]);
            t.advance(b"\r\n");
        }
        assert_eq!(find(&t, "a", Options::default()).len(), FIND_LIMIT);
        // A full-screen program: only its screen.
        t.advance(b"\x1b[?1049h\x1b[Hfull a");
        assert_eq!(find(&t, "a", Options::default()).len(), 1);
    }

    #[test]
    fn matches_keep_their_lines_while_history_drops_off() {
        let mut t = term::Term::new(8, 2);
        t.advance(b"needle\r\n");
        let before = find(&t, "needle", Options::default());
        for n in 0..50 {
            t.advance(format!("{n}\r\n").as_bytes());
        }
        assert_eq!(find(&t, "needle", Options::default()), before);
        let line = before[0].start.0;
        t.clear_all();
        assert!(find(&t, "needle", Options::default()).is_empty());
        t.advance(b"needle");
        let after = find(&t, "needle", Options::default());
        assert!(after[0].start.0 > line);
        assert_eq!(after[0].start.0, t.view_line(0, 0));
    }

    fn searched(term: usize, place: &str, output: &str) -> Searched {
        let mut t = term::Term::new(40, 4);
        t.advance(output.as_bytes());
        Searched {
            term,
            place: place.into(),
            hay: t.haystack(),
        }
    }

    fn search(
        sessions: &[Searched],
        query: &str,
        options: Options,
    ) -> Result<Vec<TermHit>, String> {
        search_all(
            sessions,
            query,
            options,
            &std::sync::atomic::AtomicBool::new(false),
        )
    }

    #[test]
    fn a_snippet_is_the_matched_line_from_the_history() {
        let mut t = term::Term::new(20, 3);
        t.advance(b"  first needle here\r\n");
        for n in 0..10 {
            t.advance(format!("{n}\r\n").as_bytes());
        }
        let hay = t.haystack();
        let hit = &find_in(&hay, "needle", Options::default()).unwrap()[0];
        assert!(hit.start.0 < t.view_line(0, 0), "in the history");
        let (text, at) = hay.row_text(hit.start.0, hit.start.1).unwrap();
        assert_eq!((text, &text[at..]), ("  first needle here", "needle here"));
        assert_eq!(snippet(text, at), "first needle here");
    }

    #[test]
    fn a_wrapped_lines_snippet_joins_its_rows() {
        let mut t = term::Term::new(10, 4);
        t.advance(b"abcdefghijklmnop needle\r\nnext");
        let hay = t.haystack();
        let hit = &find_in(&hay, "needle", Options::default()).unwrap()[0];
        assert_eq!(hit.start.0, t.view_line(0, 0) + 1);
        let (text, at) = hay.row_text(hit.start.0, hit.start.1).unwrap();
        assert_eq!(text, "abcdefghijklmnop needle");
        assert_eq!(&text[at..], "needle");
        // A long line keeps what is around the match, and says it goes on.
        let long = format!("{}needle{}", "a".repeat(100), "b".repeat(100));
        let shown = snippet(&long, 100);
        assert_eq!(shown.chars().count(), SNIPPET + 2);
        assert!(shown.starts_with('\u{2026}') && shown.ends_with('\u{2026}'));
        assert!(shown.contains(&format!("{}needle", "a".repeat(SNIPPET_BEFORE))));
    }

    #[test]
    fn searching_every_terminal_goes_in_tab_order_newest_first() {
        let sessions = [
            searched(1, "work / sh", "kiwi one\r\nplum\r\nkiwi two\r\n"),
            searched(2, "zsh", "nothing\r\n"),
            searched(3, "other / claude", "a kiwi\r\n"),
        ];
        let hits = search(&sessions, "kiwi", Options::default()).unwrap();
        let shown: Vec<(usize, &str, &str)> = hits
            .iter()
            .map(|h| (h.term, h.snippet.as_str(), h.detail.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                (1, "kiwi two", "work / sh  \u{b7}  line 3"),
                (1, "kiwi one", "work / sh  \u{b7}  line 1"),
                (3, "a kiwi", "other / claude  \u{b7}  line 1"),
            ]
        );
        assert_eq!(hits[2].start.1, 2);
        assert!(
            search(&sessions, "", Options::default())
                .unwrap()
                .is_empty()
        );
        let regex = Options {
            regex: true,
            ..Options::default()
        };
        assert!(search(&sessions, "(", regex).is_err());
        assert!(search(&[], "(", regex).is_err());
    }

    #[test]
    fn searching_every_terminal_keeps_to_its_limits() {
        let many: String = (0..SEARCH_ALL_PER_TERMINAL + 20)
            .map(|n| format!("x{n}\r\n"))
            .collect();
        let sessions: Vec<Searched> = (0..12).map(|n| searched(n, "sh", &many)).collect();
        let hits = search(&sessions, "x", Options::default()).unwrap();
        assert_eq!(hits.len(), SEARCH_ALL_LIMIT);
        assert!(hits[..SEARCH_ALL_PER_TERMINAL].iter().all(|h| h.term == 0));
        assert_eq!(hits[SEARCH_ALL_PER_TERMINAL].term, 1);
        // The newest of a session's matches, not its oldest.
        let last = format!("x{}", SEARCH_ALL_PER_TERMINAL + 19);
        assert_eq!(hits[0].snippet, last);
        assert_eq!(hits.last().unwrap().term, 9);
    }

    #[test]
    fn a_search_of_every_terminal_waits_for_typing_and_answers_once() {
        let mut search = SearchAll::default();
        assert!(!search.note(""), "nothing typed yet");
        assert!(search.note("kiwi"));
        assert!(search.busy() && search.loading());
        assert!(!search.due(Instant::now()));
        assert!(search.due(Instant::now() + SEARCH_ALL_DEBOUNCE));
        search.start(vec![searched(7, "sh", "kiwi\r\n")], Options::default());
        let started = Instant::now();
        while !search.poll() && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!search.busy());
        assert_eq!(search.query, "kiwi");
        assert_eq!(search.hits.len(), 1);
        // Cleared: nothing, at once.
        search.note("");
        search.start(Vec::new(), Options::default());
        assert!(!search.busy() && search.hits.is_empty());
    }

    fn tab() -> Tab {
        Tab {
            session: Session::offline(20, 4).expect("a socket pair"),
            title: "plot".into(),
            claude: false,
            launch: Launch {
                program: "/bin/sh".into(),
                args: Vec::new(),
                cwd: PathBuf::from("/srv/plot"),
                env: Vec::new(),
                unset: Vec::new(),
            },
            attention: None,
            last_output: None,
            review: PathBuf::new(),
            events_read: 0,
            receipt: Default::default(),
            review_files: 0,
            folder: PathBuf::from("/srv/plot"),
            workspace: None,
            alive: std::sync::Arc::new(()),
            held: None,
            status: None,
            cwd: PathBuf::from("/srv/plot"),
            program_pid: None,
            remote: None,
            told_focus: false,
            last_typed: None,
            draft_pending: false,
            last_probe: None,
            probe_owed: false,
            retry: None,
            last_return: None,
            marks: Default::default(),
            notified: false,
            seen: None,
            view: View::new(),
        }
    }

    #[test]
    fn a_draft_is_words_typed_and_not_sent() {
        // Typing leaves a draft; Return sends it, Esc or Ctrl-C clears it.
        assert!(draft_after(b"c", false));
        assert!(draft_after(b"ontinue", true));
        assert!(!draft_after(b"\r", true));
        assert!(!draft_after(b"yes\n", true));
        assert!(!draft_after(b"\x1b", true));
        assert!(!draft_after(b"\x03", true));
        assert!(!draft_after(b"\x15", true));
        // Keys that move or edit do not say whether anything is left.
        for bytes in [&b"\x1b[A"[..], b"\x1bOB", b"\x7f", b"\t", b"\x1b[I", b""] {
            assert!(draft_after(bytes, true), "{bytes:?}");
            assert!(!draft_after(bytes, false), "{bytes:?}");
        }
        // A paste is words, even one with a line break in it.
        assert!(draft_after(b"\x1b[200~fix it\r\x1b[201~", false));
        assert!(draft_after(b"\x1b[200~fix it\x1b[201~", false));
    }

    #[test]
    fn the_agent_has_left_when_the_shell_runs_nothing() {
        let shell: Vec<String> = vec!["-l".into()];
        let still_shell: Vec<String> = vec!["/bin/zsh".into(), "-l".into()];
        // The shell crc started, with claude in front of it, then alone.
        assert!(agent_in_front_of(&shell, Some(&still_shell), &[412]));
        assert!(!agent_in_front_of(&shell, Some(&still_shell), &[]));
        // A `-c claude` tab: the shell exec'd into the agent, which has
        // no children while it waits.
        let claude_tab: Vec<String> = ["-l", "-i", "-c", "claude"].map(String::from).to_vec();
        let claude: Vec<String> = vec!["claude".into()];
        assert!(agent_in_front_of(&claude_tab, Some(&claude), &[]));
        // Nothing read of the program: not guessed at.
        assert!(agent_in_front_of(&shell, None, &[]));
    }

    #[test]
    fn a_tab_is_found_by_id_until_it_closes_and_a_told_one_is_withdrawn() {
        let mut panel = Panel::default();
        for name in ["one", "two", "three"] {
            let mut tab = tab();
            tab.review = PathBuf::from(format!("/nonexistent/sessions/{name}"));
            panel.tabs.push(tab);
        }
        panel.tabs[1].notified = true;
        assert_eq!(panel.tab_by_id("/nonexistent/sessions/two"), Some(1));
        panel.close_tab(0);
        assert_eq!(panel.tab_by_id("/nonexistent/sessions/two"), Some(0));
        assert!(
            panel.withdrawn.is_empty(),
            "never told: nothing to take back"
        );
        panel.close_tab(0);
        assert_eq!(panel.tab_by_id("/nonexistent/sessions/two"), None);
        assert_eq!(panel.withdrawn, ["/nonexistent/sessions/two"]);
    }

    #[test]
    fn closing_a_terminal_removes_its_session() {
        let tree = crate::project::TempTree::new(
            "terminal-close-session",
            &[("sessions/one/files/", ""), ("sessions/two/events", "")],
        );
        let root = tree.0.join("sessions");
        let mut panel = Panel::default();
        for name in ["one", "two"] {
            let mut tab = tab();
            tab.review = root.join(name);
            panel.tabs.push(tab);
        }
        panel.close_tab(0);
        assert!(!root.join("one").exists(), "closed: its session goes");
        assert!(root.join("two").is_dir(), "still open: its session stays");
        assert_eq!(
            crate::project::review::sessions(&root),
            vec![root.join("two")]
        );
        // Removed on a thread, once moved aside.
        let until = Instant::now() + Duration::from_secs(5);
        while root.join("one.gone").exists() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!root.join("one.gone").exists());
    }

    #[test]
    fn a_bell_right_after_a_keystroke_is_the_programs_answer() {
        let now = Instant::now();
        let mut tab = tab();
        // Nothing typed: a ring is a call, as a script's at its start.
        assert!(tab.heeds_bell(now));
        // Typed into just now: the ring answers the key.
        tab.last_typed = Some(now);
        assert!(!tab.heeds_bell(now));
        assert!(!tab.heeds_bell(now + BELL_ECHO / 2));
        // Long enough after, it is the program's own.
        assert!(tab.heeds_bell(now + BELL_ECHO));
        assert!(tab.heeds_bell(now + Duration::from_secs(30)));
    }

    /// Output in `tab` as the window takes it: the screen has it, then
    /// the window looks.
    fn print(tab: &mut Tab, bytes: &[u8], now: Instant) -> bool {
        tab.session.feed(bytes);
        let was = tab.activity();
        tab.last_output = Some(now);
        tab.face_changed(was)
    }

    #[test]
    fn output_off_screen_draws_only_what_it_changed() {
        let now = Instant::now();
        let mut tab = tab();
        let mut pace = Pace::default();
        // Idle to working is shown in its row.
        let changed = print(&mut tab, b"   Compiling turnip v0.1.0\r\n", now);
        assert!(changed);
        assert_eq!(pace.decide(false, changed, now), When::Now);
        // More output while it works: nothing shown changed.
        let later = now + BACKGROUND_EVERY * 2;
        let changed = print(&mut tab, b"   Compiling turnip v0.2.0\r\n", later);
        assert!(!changed);
        assert_eq!(pace.decide(false, changed, later), When::Never);
        // A title that came with the output is a change, and so is a ring.
        assert!(print(&mut tab, b"\x1b]0;radish\x07", later));
        assert!(!print(&mut tab, b"radish\r\n", later));
        tab.attention = Some(String::new());
        assert!(print(&mut tab, b"\x07", later));
        // Working again after it went quiet, with nothing else new.
        tab.attention = None;
        tab.face_changed(tab.activity());
        tab.last_output = Some(now - Duration::from_secs(10));
        assert!(print(&mut tab, b"x", later));
    }

    #[test]
    fn output_on_screen_always_draws() {
        let now = Instant::now();
        let mut pace = Pace::default();
        assert_eq!(pace.decide(false, true, now), When::Now);
        assert_eq!(
            pace.decide(true, false, now + Duration::from_millis(1)),
            When::Now
        );
        let mut panel = Panel {
            tabs: vec![tab(), tab(), tab()],
            active: 1,
            ..Panel::default()
        };
        assert_eq!(panel.visible(false), Vec::<usize>::new());
        assert_eq!(panel.visible(true), vec![1]);
        // Every pane is on screen, whichever has the keyboard.
        assert!(panel.split(Edge::Left, 0));
        assert_eq!(panel.visible(true), vec![0, 1]);
        assert!(panel.split(Edge::Bottom, 2));
        assert_eq!(panel.visible(true), vec![0, 2, 1]);
        panel.show(1);
        assert_eq!(panel.visible(true), vec![0, 2, 1]);
        assert_eq!(panel.visible(false), Vec::<usize>::new());
    }

    #[test]
    fn changes_off_screen_draw_ten_times_a_second_and_the_last_is_kept() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut pace = Pace::default();
        assert_eq!(pace.decide(false, true, t0), When::Now);
        // Within the interval: held back, to the end of it.
        assert_eq!(pace.decide(false, true, at(30)), When::At(at(100)));
        assert_eq!(pace.decide(false, true, at(60)), When::At(at(100)));
        assert_eq!(pace.due, Some(at(100)));
        assert!(!pace.take_due(at(99)));
        assert!(pace.take_due(at(100)), "the change held back draws");
        assert!(!pace.take_due(at(101)), "once");
        assert_eq!(pace.decide(false, true, at(150)), When::At(at(200)));
        // A frame for a tab on screen draws it too.
        assert_eq!(pace.decide(true, false, at(160)), When::Now);
        assert_eq!(pace.due, None);
        assert_eq!(pace.decide(false, true, at(210)), When::Now);
        // A settle already scheduled earlier covers a later one.
        assert!(pace.arm(at(300)));
        assert!(!pace.arm(at(400)));
        assert!(pace.arm(at(250)));
    }

    #[test]
    fn a_process_is_read_twice_a_second_at_most() {
        let t0 = Instant::now();
        let mut tab = tab();
        assert_eq!(tab.probe(t0), When::Now);
        let later = tab.probe(t0 + Duration::from_millis(100));
        assert_eq!(later, When::At(t0 + PROBE_EVERY));
        assert!(tab.probe_owed);
        assert_eq!(tab.probe(t0 + PROBE_EVERY), When::Now);
        assert!(!tab.probe_owed);
    }

    fn hit(line: u64, col: usize) -> Hit {
        Hit {
            start: (line, col),
            spans: vec![(line, col, col + 1)],
        }
    }

    #[test]
    fn a_press_on_a_find_tick_picks_the_first_match_of_its_line() {
        let track = mark_track(Viewport {
            x: 0.0,
            y: 30.0,
            width: 400.0,
            height: 202.0,
        });
        let matches = vec![hit(1000, 0), hit(1050, 2), hit(1050, 9), hit(1099, 0)];
        let at = |line: u64| tick_y(track, line, 1000, 100) + TICK_HEIGHT / 2.0;
        assert_eq!(find_tick_at(track, at(1050), &matches, 1000, 100), Some(1));
        assert_eq!(find_tick_at(track, at(1099), &matches, 1000, 100), Some(3));
        assert_eq!(
            find_tick_at(track, at(1000) + 3.5, &matches, 1000, 100),
            Some(0)
        );
        assert_eq!(find_tick_at(track, at(1025), &matches, 1000, 100), None);
        assert_eq!(find_tick_at(track, at(1050), &[], 1000, 100), None);
    }

    /// Waits for `find`'s worker to answer; `false` if it never did.
    fn answered(find: &mut Find) -> bool {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(20) {
            if find.poll() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    }

    fn key(t: &term::Term, query: &str) -> FindKey {
        (1, t.generation, query.into(), Options::default())
    }

    #[test]
    fn an_answer_for_a_search_since_replaced_is_dropped() {
        let mut t = term::Term::new(20, 4);
        t.advance(b"kiwi\r\nlime\r\n");
        let mut find = Find::default();
        assert_eq!(find.sync(key(&t, "kiwi"), &t, true), Synced::Started);
        assert!(answered(&mut find));
        assert_eq!(find.count(), (0, 1));
        // A new query: the old matches stay until its answer, a step
        // waits for it, and the first search's answer, should it come
        // after all, is dropped.
        assert_eq!(find.sync(key(&t, "lime"), &t, true), Synced::Started);
        assert!(find.searching() && find.busy());
        assert_eq!(find.sync(key(&t, "lime"), &t, false), Synced::Fresh);
        assert_eq!(find.current_hit().map(|h| h.start), Some((0, 0)));
        find.pending_step = Some(false);
        let stale = Found {
            matches: vec![hit(0, 0), hit(0, 2)],
            ..Found::default()
        };
        assert!(!find.land(1, key(&t, "kiwi"), stale));
        assert_eq!(find.made_for.as_ref().map(|m| m.2.as_str()), Some("kiwi"));
        assert!(answered(&mut find));
        assert!(!find.busy() && !find.searching());
        assert_eq!(find.made_for.as_ref().map(|m| m.2.as_str()), Some("lime"));
        assert_eq!(find.current_hit().map(|h| h.start), Some((1, 0)));
        assert!(
            find.reveal && find.pending_step.is_none(),
            "the step was taken"
        );
        // Output alone: searched again, but not more than so often.
        t.advance(b"lime\r\n");
        assert_eq!(find.sync(key(&t, "lime"), &t, false), Synced::Later);
        assert_eq!(find.sync(key(&t, "lime"), &t, true), Synced::Started);
        assert!(!find.searching(), "the matches are this query's");
        assert!(answered(&mut find));
        assert_eq!(find.count(), (0, 2));
        // An empty query and an invalid pattern need no worker.
        assert_eq!(find.sync(key(&t, ""), &t, true), Synced::Done(None));
        assert!(find.matches.is_empty() && !find.busy());
        let regex = Options {
            regex: true,
            ..Options::default()
        };
        let invalid = (1, t.generation, "(".to_string(), regex);
        assert!(matches!(
            find.sync(invalid, &t, true),
            Synced::Done(Some(_))
        ));
        assert!(find.error.is_some() && !find.busy());
    }

    #[test]
    fn matches_on_lines_let_go_of_go_and_the_current_falls_to_the_next() {
        let row = |hit: usize, line: u64| FilterRow {
            hit,
            line,
            text: String::new(),
            marks: Vec::new(),
        };
        let mut find = Find::default();
        find.take(vec![hit(1, 0), hit(5, 2), hit(5, 6), hit(9, 0)]);
        find.rows = vec![row(0, 1), row(1, 5), row(3, 9)];
        find.filter = Some(Filter {
            selected: 1,
            scroll: 1,
        });
        find.step(true);
        assert_eq!(find.anchor, Some((1, 0)));
        find.prune(1);
        assert_eq!(find.matches.len(), 4, "line 1 is still kept");
        find.prune(4);
        assert_eq!(find.matches, [hit(5, 2), hit(5, 6), hit(9, 0)]);
        assert_eq!((find.current, find.anchor), (Some(0), Some((5, 2))));
        assert_eq!(find.rows, [row(0, 5), row(2, 9)]);
        assert_eq!(
            find.filter,
            Some(Filter {
                selected: 0,
                scroll: 0
            })
        );
        assert_eq!(find.current_row(), 0);
        find.step(false);
        assert_eq!(find.current_row(), 1);
        find.prune(100);
        assert!(find.matches.is_empty() && find.rows.is_empty());
        assert_eq!(find.current, None);
    }

    #[test]
    fn a_search_of_a_full_history_leaves_the_main_thread_at_once() {
        let mut t = term::Term::new(120, 40);
        for n in 0..12_000 {
            let line = format!("{n:06} the quick brown fox jumps over the lazy dog, {n} times\r\n");
            t.advance(line.as_bytes());
        }
        assert!(t.scrollback_len() >= 9_000);
        // Slow on purpose: from every place on a line, to its end and back.
        let query = r".*cat";
        let options = Options {
            regex: true,
            ..Options::default()
        };
        // What searching on this thread costs, for comparison.
        let started = Instant::now();
        assert!(find_in(&t.haystack(), query, options).unwrap().is_empty());
        let inline = started.elapsed();
        let mut find = Find::default();
        let started = Instant::now();
        let synced = find.sync((1, t.generation, query.into(), options), &t, true);
        let main = started.elapsed();
        assert_eq!(synced, Synced::Started);
        assert!(answered(&mut find));
        assert!(find.matches.is_empty() && find.error.is_none());
        // A few milliseconds of copying, against the whole search: well
        // under a tenth of it, whatever the machine's speed.
        assert!(
            main < Duration::from_millis(25) && main * 10 < inline,
            "the main thread's part took {main:?}, searching took {inline:?}"
        );
    }

    #[test]
    fn filter_mode_lists_each_line_once_with_its_matches_marked() {
        let mut t = term::Term::new(10, 6);
        t.advance(b"ab ab x\r\nnone\r\nxxxxxxxxabab\r\nab");
        let first = t.view_line(0, 0);
        let found = search_lines(
            &t.find_lines(),
            "ab",
            Options::default(),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(found.matches.len(), 5);
        let rows: Vec<_> = found
            .rows
            .iter()
            .map(|r| (r.hit, r.line - first, r.text.as_str(), r.marks.clone()))
            .collect();
        assert_eq!(
            rows,
            [
                (0, 0, "ab ab x", vec![(0, 2), (3, 5)]),
                // Wrapped over two rows: one line, both of its matches.
                (2, 2, "xxxxxxxxabab", vec![(8, 10), (10, 12)]),
                (4, 4, "ab", vec![(0, 2)]),
            ]
        );
        // Cancelled: no answer.
        let cancel = std::sync::atomic::AtomicBool::new(true);
        assert!(search_lines(&t.find_lines(), "ab", Options::default(), &cancel).is_none());
    }

    #[test]
    fn a_long_line_is_cut_around_its_match_with_the_marks_moved() {
        let text = format!("{}needle{}", "x".repeat(200), " y".repeat(100));
        let (shown, marks) = marked_snippet(&text, 200, &[200..206, 0..4, 400..402]);
        assert!(shown.starts_with('\u{2026}') && shown.ends_with('\u{2026}'));
        let chars: Vec<char> = shown.chars().collect();
        let (from, to) = marks[0];
        assert_eq!(chars[from..to].iter().collect::<String>(), "needle");
        assert_eq!(marks.len(), 1, "the others are not in what shows");
        assert_eq!(shown, snippet(&text, 200));
        // Blanks trimmed off the front move the marks too.
        let (shown, marks) = marked_snippet("   ab", 3, std::slice::from_ref(&(3..5)));
        assert_eq!((shown.as_str(), marks), ("ab", vec![(0, 2)]));
    }

    #[test]
    fn the_current_match_stays_on_its_anchor_and_steps_around() {
        let mut find = Find::default();
        find.take(vec![hit(1, 0), hit(5, 2), hit(9, 0)]);
        assert_eq!(find.current, Some(2), "the newest first");
        find.step(true);
        assert_eq!((find.current, find.anchor), (Some(0), Some((1, 0))));
        find.step(false);
        assert_eq!(find.current, Some(2));
        find.step(false);
        assert_eq!(find.anchor, Some((5, 2)));
        // Output arrives: more matches before and after, the same one stays.
        find.take(vec![hit(0, 0), hit(1, 0), hit(5, 2), hit(9, 0), hit(12, 0)]);
        assert_eq!(find.current, Some(2));
        assert_eq!(find.count(), (2, 5));
        // Its text went: the next after where it was.
        find.take(vec![hit(1, 0), hit(9, 0)]);
        assert_eq!(find.current_hit().map(|h| h.start), Some((9, 0)));
        find.take(vec![hit(1, 0)]);
        assert_eq!(find.current, Some(0), "or the last before it");
        find.take(Vec::new());
        assert_eq!(find.current, None);
        find.step(true);
        assert_eq!(find.current, None);
    }

    #[test]
    fn scrolling_brings_a_match_into_view_below_the_bar() {
        // 10 rows, the screen's first line 100, 100 lines of history.
        assert_eq!(back_showing(105, 100, 0, 10, 2, 100), 0, "in view");
        assert_eq!(
            back_showing(100, 100, 0, 10, 2, 100),
            6,
            "under the bar: to the middle of the rest"
        );
        assert_eq!(back_showing(40, 100, 0, 10, 2, 100), 66);
        assert_eq!(
            back_showing(45, 100, 60, 10, 2, 100),
            60,
            "in view back there"
        );
        assert_eq!(
            back_showing(105, 100, 60, 10, 2, 100),
            0,
            "back to the output"
        );
        assert_eq!(
            back_showing(1, 100, 0, 10, 2, 50),
            50,
            "as far as history goes"
        );
    }

    #[test]
    fn a_prompt_jumped_to_shows_under_what_led_to_it() {
        // The screen's first line 100, 100 lines of history.
        assert_eq!(prompt_back(60, 100, 100, 0), 42);
        assert_eq!(prompt_back(60, 100, 100, 2), 44, "below the find bar");
        assert_eq!(prompt_back(1, 100, 50, 0), 50, "as far as history goes");
        assert_eq!(prompt_back(108, 100, 100, 0), 0, "on the screen itself");
    }

    #[test]
    fn a_mark_is_pressed_where_it_is_drawn() {
        let track = mark_track(Viewport {
            x: 0.0,
            y: 30.0,
            width: 400.0,
            height: 202.0,
        });
        assert_eq!(track.x + track.width, 400.0 + PAD, "in the padding");
        // 100 lines from line 1000: two points a line.
        assert_eq!(tick_y(track, 1000, 1000, 100), 30.0);
        assert_eq!(tick_y(track, 1050, 1000, 100), 130.0);
        // Clamped into the track: a line dropped, one past the end.
        assert_eq!(tick_y(track, 10, 1000, 100), 30.0);
        assert_eq!(tick_y(track, 5000, 1000, 100), 230.0);
        for line in [1000, 1003, 1050, 1099] {
            let y = tick_y(track, line, 1000, 100) + TICK_HEIGHT / 2.0;
            assert_eq!(
                mark_at(track, y, [1000, 1003, 1050, 1099].into_iter(), 1000, 100),
                Some(line)
            );
        }
        let lines = || [1010, 1060, 900].into_iter();
        assert_eq!(mark_at(track, 51.0 + 3.5, lines(), 1000, 100), Some(1010));
        assert_eq!(
            mark_at(track, 51.0 + 4.5, lines(), 1000, 100),
            None,
            "too far"
        );
        assert_eq!(mark_at(track, 31.0, lines(), 1000, 100), None, "dropped");
    }

    /// A panel of tabs titled as given, all listed.
    fn panel_of(titles: &[&str]) -> Panel {
        Panel {
            open: true,
            tabs: titles
                .iter()
                .map(|title| Tab {
                    title: (*title).to_owned(),
                    ..tab()
                })
                .collect(),
            ..Panel::default()
        }
    }

    fn titles(panel: &Panel) -> Vec<String> {
        panel.tabs.iter().map(|tab| tab.title.clone()).collect()
    }

    /// Every place a tab is named by its index names the same session
    /// after a move as before it: for every size up to seven, every move,
    /// and every tab each holder could be naming.
    #[test]
    fn every_index_names_the_same_session_after_a_move() {
        let link = || term::Link {
            url: "https://example.org/plot".into(),
            spans: vec![(3, 0, 4)],
        };
        for len in 1..=7usize {
            let names: Vec<String> = (0..len).map(|i| format!("t{i}")).collect();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            for from in 0..len {
                for to in 0..len {
                    for at in 0..len {
                        let other = (at + 1) % len;
                        let mut panel = panel_of(&names);
                        // Two panes where there are two tabs, the keyboard
                        // in `at`, and every tab scrolled its own way.
                        panel.active = other;
                        assert_eq!(panel.split(Edge::Right, at), len > 1);
                        panel.active = at;
                        for (i, tab) in panel.tabs.iter_mut().enumerate() {
                            tab.view.back = i + 1;
                        }
                        // Every other tab from `at` on, as a workspace's
                        // tabs lie among the others'.
                        panel.shown = Some((0..len).filter(|i| i % 2 == at % 2).collect());
                        panel.mouse_press = Some((at, term::mouse::Button::Left, (1, 2)));
                        panel.mouse_cell = Some((other, (3, 4)));
                        panel.link = Some((at, link()));
                        panel.tab_drag = Some(TabDrag {
                            tab: at,
                            start_x: 0.0,
                            x: 9.0,
                            y: 0.0,
                            active: true,
                            before: None,
                            pane: false,
                            drop: None,
                        });
                        panel.closed_active = Some((None, PathBuf::new(), other));
                        let name = |panel: &Panel, i: usize| panel.tabs[i].title.clone();
                        let panes = |panel: &Panel| {
                            let mut scrolled: Vec<(String, usize)> = panel
                                .tabs
                                .iter()
                                .map(|tab| (tab.title.clone(), tab.view.back))
                                .collect();
                            scrolled.sort();
                            (panel.panes_text(|i| name(panel, i)), scrolled)
                        };
                        let shown = |panel: &Panel| -> Vec<String> {
                            let mut shown: Vec<String> = panel
                                .shown
                                .iter()
                                .flatten()
                                .map(|i| name(panel, *i))
                                .collect();
                            shown.sort();
                            shown
                        };
                        let before = (
                            name(&panel, panel.active),
                            panes(&panel),
                            shown(&panel),
                            name(&panel, panel.mouse_press.unwrap().0),
                            name(&panel, panel.mouse_cell.unwrap().0),
                            name(&panel, panel.link.as_ref().unwrap().0),
                            name(&panel, panel.tab_drag.unwrap().tab),
                            name(&panel, panel.closed_active.as_ref().unwrap().2),
                        );
                        let mut order: Vec<String> = titles(&panel);
                        let moving = order.remove(from);
                        order.insert(to, moving);
                        assert_eq!(panel.move_tab(from, to), from != to);
                        let after = (
                            name(&panel, panel.active),
                            panes(&panel),
                            shown(&panel),
                            name(&panel, panel.mouse_press.unwrap().0),
                            name(&panel, panel.mouse_cell.unwrap().0),
                            name(&panel, panel.link.as_ref().unwrap().0),
                            name(&panel, panel.tab_drag.unwrap().tab),
                            name(&panel, panel.closed_active.as_ref().unwrap().2),
                        );
                        let what = format!("{len} tabs, {from} to {to}, holders on {at}");
                        assert_eq!(titles(&panel), order, "{what}");
                        assert_eq!(before, after, "{what}");
                        if len > 1 {
                            let (text, _) = panes(&panel);
                            let wanted = format!("h(0.5,t{other},*t{at})");
                            assert_eq!(text, wanted, "{what}");
                        }
                        check_panes(&panel, &what);
                        // What else a press carries is untouched.
                        assert_eq!(panel.mouse_press.unwrap().2, (1, 2), "{what}");
                        assert_eq!(panel.mouse_cell.unwrap().1, (3, 4), "{what}");
                        assert_eq!(panel.link.as_ref().unwrap().1, link(), "{what}");
                        // The tabs listed stay in the strip's order.
                        let listed = panel.shown.clone().unwrap();
                        assert!(listed.is_sorted(), "{what}: {listed:?}");
                    }
                }
            }
        }
        // Nothing to move: nothing changes, and it says so.
        let mut panel = panel_of(&["a", "b"]);
        panel.active = 1;
        assert!(!panel.move_tab(0, 2) && !panel.move_tab(2, 0) && !panel.move_tab(1, 1));
        assert_eq!(
            (titles(&panel), panel.active),
            (vec!["a".into(), "b".into()], 1)
        );
    }

    /// A workspace's tabs move among themselves wherever the other
    /// workspaces' tabs lie between them, and those keep their order.
    #[test]
    fn a_tab_moves_among_the_tabs_listed_only() {
        let all = ["o0", "a", "o1", "b", "c", "o2", "d"];
        let listed = [1usize, 3, 4, 6];
        let others = |panel: &Panel| -> Vec<String> {
            let mut names = titles(panel);
            names.retain(|n| n.starts_with('o'));
            names
        };
        let mine = |panel: &Panel| -> Vec<String> {
            let mut names = titles(panel);
            names.retain(|n| !n.starts_with('o'));
            names
        };
        // A drop into every gap, from every place.
        for place in 0..listed.len() {
            for slot in 0..=listed.len() {
                let mut panel = panel_of(&all);
                panel.shown = Some(listed.to_vec());
                panel.active = listed[place];
                let mut want = mine(&panel);
                let moving = want.remove(place);
                want.insert(if slot > place { slot - 1 } else { slot }, moving.clone());
                let stays = slot == place || slot == place + 1;
                let what = format!("{moving} into gap {slot}");
                assert_eq!(
                    panel.drop_tab(&listed, listed[place], slot),
                    !stays,
                    "{what}"
                );
                assert_eq!(mine(&panel), want, "{what}");
                assert_eq!(others(&panel), ["o0", "o1", "o2"], "{what}");
                assert_eq!(panel.tabs[panel.active].title, moving, "{what}");
                // The same tabs are listed, now in the order asked for.
                let shown: Vec<String> = panel
                    .listed()
                    .iter()
                    .map(|i| panel.tabs[*i].title.clone())
                    .collect();
                assert_eq!(shown, want, "{what}");
            }
        }
        // A step either way, and none past an end.
        let mut panel = panel_of(&all);
        panel.shown = Some(listed.to_vec());
        assert!(panel.step_tab(&listed, 3, 1));
        assert_eq!(mine(&panel), ["a", "c", "b", "d"]);
        let listed = panel.listed();
        assert!(panel.step_tab(&listed, listed[1], -1));
        assert_eq!(mine(&panel), ["c", "a", "b", "d"]);
        let listed = panel.listed();
        assert!(!panel.step_tab(&listed, listed[0], -1), "the first stays");
        assert!(!panel.step_tab(&listed, listed[3], 1), "the last stays");
        assert!(!panel.step_tab(&listed, 0, 1), "another workspace's");
        assert_eq!(mine(&panel), ["c", "a", "b", "d"]);
        assert_eq!(others(&panel), ["o0", "o1", "o2"]);
        // Round the tabs listed, from the one showing.
        panel.active = listed[3];
        assert_eq!(panel.cycled(&listed, 1), Some(listed[0]));
        assert_eq!(panel.cycled(&listed, -1), Some(listed[2]));
        panel.active = 0;
        assert_eq!(
            panel.cycled(&listed, 1),
            None,
            "the one showing is not listed"
        );
        assert_eq!(panel_of(&["only"]).cycled(&[0], 1), None);
    }

    /// A drag ends with any tab that goes.
    #[test]
    fn a_closed_tab_ends_a_drag() {
        let mut panel = panel_of(&["a", "b", "c"]);
        panel.tab_drag = Some(TabDrag {
            tab: 2,
            start_x: 0.0,
            x: 0.0,
            y: 0.0,
            active: true,
            before: None,
            pane: false,
            drop: None,
        });
        panel.close_tab(0);
        assert_eq!(panel.tab_drag, None);
    }

    /// A menu opened on b of a, b, c, as the sidebar's session menu and
    /// the strip's tab menu keep it: by its id. Another tab closes while
    /// the menu is open (its program ended), and the menu's Close still
    /// closes b, not the tab that slid into b's place.
    #[test]
    fn a_menus_close_is_its_own_tabs_after_another_closed() {
        let mut panel = panel_of(&["a", "b", "c"]);
        let id = panel.id_of(1).expect("b");
        panel.menu_tab = Some(id.clone());
        panel.close_tab(0);
        assert_eq!(titles(&panel), ["b", "c"]);
        assert_eq!(panel.tab_by_id(&id), Some(0));
        assert_eq!(panel.menu_tab_index(), Some(0));
        assert!(panel.close_by_id(&id));
        assert_eq!(titles(&panel), ["c"], "b went, c stayed");
        // And after a move: the same session at its new place.
        let mut panel = panel_of(&["a", "b", "c"]);
        let id = panel.id_of(1).expect("b");
        panel.menu_tab = Some(id.clone());
        assert!(panel.move_tab(1, 2));
        assert_eq!(panel.menu_tab_index(), Some(2));
        assert!(panel.close_by_id(&id));
        assert_eq!(titles(&panel), ["a", "c"]);
    }

    /// The menu's own tab closed while it was open: its items find no
    /// tab, and Close closes nothing.
    #[test]
    fn a_menus_close_does_nothing_once_its_tab_has_gone() {
        let mut panel = panel_of(&["a", "b", "c"]);
        let id = panel.id_of(1).expect("b");
        panel.menu_tab = Some(id.clone());
        panel.close_tab(1);
        assert_eq!(panel.tab_by_id(&id), None);
        assert_eq!(panel.menu_tab_index(), None);
        assert!(!panel.close_by_id(&id));
        assert_eq!(titles(&panel), ["a", "c"], "neither neighbour went");
        assert_eq!(panel.id_of(7), None);
    }

    fn header(width: f32) -> Viewport {
        Viewport {
            x: 260.0,
            y: 400.0,
            width,
            height: HEADER,
        }
    }

    /// What the strip must hold to at any width: no tab past its ends,
    /// under a button or over another, the new-shell and hide buttons
    /// inside the header and apart, and the active tab in view.
    fn check_strip(panel: &Panel, atlas: &mut Atlas, header: Viewport, what: &str) -> Strip {
        let strip = strip(panel, atlas, header);
        let hide = header_hide(header);
        let (hits, new) = header_hits(panel, atlas, header);
        assert_eq!(new, strip.new, "{what}");
        assert_eq!(hits.len(), panel.tabs.len(), "{what}");
        let end = |r: Viewport| r.x + r.width;
        assert!(
            new.x >= header.x && end(new) <= hide.x,
            "{what}: {new:?} {hide:?}"
        );
        assert!(end(hide) <= end(header), "{what}: {hide:?}");
        let mut last = header.x;
        for (index, (rect, close)) in hits.iter().enumerate() {
            let listed = panel.listed().contains(&index);
            if !listed {
                assert_eq!((rect.width, close.width), (0.0, 0.0), "{what}: tab {index}");
                continue;
            }
            if rect.width == 0.0 {
                assert_eq!(close.width, 0.0, "{what}: tab {index}");
                continue;
            }
            assert!(
                rect.x >= last - 0.01,
                "{what}: tab {index} over the one before"
            );
            assert!(
                end(*rect) <= new.x - layout::UI_GAP + 0.01,
                "{what}: tab {index} {rect:?} under the button at {new:?}"
            );
            assert!(
                strip.tabs[index].whole.width <= TAB_MAX,
                "{what}: tab {index}"
            );
            if close.width > 0.0 {
                assert!(
                    close.x >= rect.x && end(*close) <= end(*rect),
                    "{what}: tab {index}"
                );
            }
            last = end(*rect);
        }
        if panel.showing() && !panel.tabs.is_empty() && strip.area.width >= TAB_MAX {
            let active = strip.tabs[panel.active];
            assert_eq!(active.rect, active.whole, "{what}: the active tab is cut");
            assert!(active.close.width > 0.0, "{what}: its close button is gone");
        }
        strip
    }

    #[test]
    fn a_few_tabs_are_as_wide_as_their_labels() {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let panel = panel_of(&["zsh", "vim notes.md", "top"]);
        let strip = check_strip(&panel, &mut atlas, header(900.0), "three");
        assert_eq!((strip.scroll, strip.max_scroll), (0.0, 0.0));
        let mut x = 260.0;
        for (tab, title) in strip.tabs.iter().zip(["zsh", "vim notes.md", "top"]) {
            let width = layout::ui_text_width(&mut atlas, title) + TAB_CHROME;
            assert_eq!(tab.rect, tab.whole);
            assert!((tab.rect.x - x).abs() < 0.01 && (tab.rect.width - width).abs() < 0.01);
            x += width;
        }
        // The new-shell button right after the last, as it has been.
        assert!((strip.new.x - (x + layout::UI_GAP)).abs() < 0.01);
        // Another workspace's tab takes no room.
        let mut panel = panel;
        panel.shown = Some(vec![0, 2]);
        panel.active = 2;
        let strip = check_strip(&panel, &mut atlas, header(900.0), "two of three");
        assert_eq!(strip.tabs[1].rect.width, 0.0);
        assert!(
            (strip.tabs[2].rect.x - strip.tabs[0].rect.x - strip.tabs[0].rect.width).abs() < 0.01
        );
        // None listed: the button alone, at the start.
        panel.shown = Some(Vec::new());
        let strip = check_strip(&panel, &mut atlas, header(900.0), "none listed");
        assert_eq!(strip.new.x, 260.0 + layout::UI_GAP);
    }

    #[test]
    fn a_long_title_is_cut_in_the_middle_at_the_widest_a_tab_gets() {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let long = "seedlings: watering schedule for the north bed and the cold frame";
        let mut panel = panel_of(&["zsh", long]);
        panel.active = 1;
        let strip = check_strip(&panel, &mut atlas, header(900.0), "long");
        assert_eq!(strip.tabs[1].whole.width, TAB_MAX);
        let room = TAB_MAX - TAB_CHROME;
        let cut = fitted_label(&mut atlas, &panel.tabs[1], room);
        assert!(
            cut.starts_with("seedlings") && cut.ends_with("frame"),
            "{cut}"
        );
        assert!(cut.contains('\u{2026}') && layout::ui_text_width(&mut atlas, &cut) <= room);
        // The count of files to review stays whole after the cut title,
        // then as the number, and goes only where there is no room.
        panel.tabs[1].review_files = 12;
        let cut = fitted_label(&mut atlas, &panel.tabs[1], room);
        assert!(
            cut.starts_with("seed") && cut.ends_with(" \u{b7} 12 to review"),
            "{cut}"
        );
        assert!(
            layout::ui_text_width(&mut atlas, &cut) <= room + 0.5,
            "{cut}"
        );
        let narrow = TAB_MIN - TAB_CHROME;
        let cut = fitted_label(&mut atlas, &panel.tabs[1], narrow);
        assert!(
            cut.ends_with(" \u{b7} 12") && cut.contains('\u{2026}'),
            "{cut}"
        );
        assert!(
            layout::ui_text_width(&mut atlas, &cut) <= narrow + 0.5,
            "{cut}"
        );
        // A short name is whole at the least a tab shrinks to.
        assert_eq!(fitted_label(&mut atlas, &panel.tabs[0], narrow), "zsh");
        let mut waiting = tab();
        waiting.title = "zsh".into();
        waiting.attention = Some(String::new());
        assert_eq!(
            fitted_label(&mut atlas, &waiting, narrow - dot_width(&waiting)),
            "zsh"
        );
    }

    #[test]
    fn many_tabs_shrink_and_then_scroll_with_the_buttons_in_sight() {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let names: Vec<String> = (0..12)
            .map(|i| format!("garden notes and seed orders, bed {i}"))
            .collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut panel = panel_of(&names);
        // Wide enough for all at less than their own width: they shrink
        // to the same width and fill the strip, and nothing scrolls.
        let wide = header(12.0 * 150.0 + 80.0);
        let strip_wide = check_strip(&panel, &mut atlas, wide, "shrunk");
        assert_eq!(strip_wide.max_scroll, 0.0);
        let width = strip_wide.tabs[0].whole.width;
        assert!((TAB_MIN..TAB_MAX).contains(&width), "{width}");
        assert!(
            strip_wide
                .tabs
                .iter()
                .all(|t| t.whole.width == width && t.rect == t.whole)
        );
        assert!(
            width * 12.0 > strip_wide.area.width - 12.0,
            "they fill the strip"
        );
        // A short one among them keeps its width: the long ones give way.
        panel.tabs[3].title = "zsh".into();
        let mixed = check_strip(&panel, &mut atlas, wide, "mixed");
        let short = layout::ui_text_width(&mut atlas, "zsh") + TAB_CHROME;
        assert!((mixed.tabs[3].whole.width - short).abs() < 0.01);
        assert!(mixed.tabs[0].whole.width > width);
        // Too narrow even at the least: the strip scrolls, the buttons
        // stay where they are, and whichever tab is active is in view.
        let narrow = header(700.0);
        for active in 0..12 {
            panel.active = active;
            panel.strip_scroll = 0.0;
            panel.strip_free = false;
            let strip = check_strip(&panel, &mut atlas, narrow, &format!("active {active}"));
            assert!(strip.max_scroll > 0.0);
            assert!(strip.tabs.iter().all(|t| t.whole.width <= TAB_MIN));
            let button_end = strip.new.x + strip.new.width + layout::UI_GAP;
            assert!((button_end - header_hide(narrow).x).abs() < 0.01);
            // And once settled, the next frame is the same frame.
            panel.settle_strip(&mut atlas, narrow);
            assert_eq!(panel.strip_scroll, strip.scroll);
            assert_eq!(check_strip(&panel, &mut atlas, narrow, "settled"), strip);
        }
        // The last is active and the strip at its end. The wheel takes it
        // back to the start, where it stays though the active tab is out
        // of sight, until another tab becomes the active one.
        let most = strip(&panel, &mut atlas, narrow).max_scroll;
        assert!((panel.strip_scroll - most).abs() < 0.01);
        panel.scroll_strip(-10_000.0);
        panel.settle_strip(&mut atlas, narrow);
        assert_eq!(panel.strip_scroll, 0.0);
        let scrolled = strip(&panel, &mut atlas, narrow);
        assert_eq!(scrolled.tabs[11].rect.width, 0.0);
        assert_eq!(scrolled.tabs[0].rect, scrolled.tabs[0].whole);
        panel.scroll_strip(10_000.0);
        panel.settle_strip(&mut atlas, narrow);
        assert!(
            (panel.strip_scroll - most).abs() < 0.01,
            "held to what there is"
        );
        panel.scroll_strip(-10_000.0);
        panel.active = 10;
        panel.settle_strip(&mut atlas, narrow);
        let back = check_strip(&panel, &mut atlas, narrow, "another tab");
        assert!(back.scroll > 0.0 && !panel.strip_free);
        // Narrower than one tab: nothing past the strip, even so.
        check_strip(&panel, &mut atlas, header(120.0), "tiny");
    }

    /// The header drawn with long titles, a waiting tab and review counts,
    /// fitting, shrunk and scrolled: every label whole or ending in an
    /// ellipsis, and nothing drawn outside the header.
    #[test]
    fn the_header_cuts_no_label_and_draws_inside_itself() {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let theme = Theme::default();
        let names: Vec<String> = (0..9)
            .map(|i| format!("harvest log and preserving notes for the autumn, part {i}"))
            .collect();
        let mut names: Vec<&str> = names.iter().map(String::as_str).collect();
        names.insert(2, "zsh");
        let mut panel = panel_of(&names);
        panel.tabs[1].review_files = 3;
        panel.tabs[4].attention = Some("done".into());
        panel.tabs[4].review_files = 120;
        for width in [1800.0, 900.0, 500.0, 260.0] {
            for active in [0, 4, 9] {
                panel.active = active;
                let rect = Viewport {
                    x: 40.0,
                    y: 300.0,
                    width,
                    height: 240.0,
                };
                let (header, _) = split(rect);
                panel.settle_strip(&mut atlas, header);
                layout::take_cut_labels();
                let mut out = Vec::new();
                draw(&panel, None, &mut atlas, rect, &theme, &mut out);
                let faults = layout::frame_faults(&out, rect);
                assert!(faults.is_empty(), "{width} wide, tab {active}: {faults:?}");
                // A drag under way draws its line inside the strip too.
                panel.tab_drag = Some(TabDrag {
                    tab: active,
                    start_x: 0.0,
                    x: header.x + header.width - 60.0,
                    y: header.y + 10.0,
                    active: true,
                    before: None,
                    pane: false,
                    drop: None,
                });
                let mut dragging = Vec::new();
                draw(&panel, None, &mut atlas, rect, &theme, &mut dragging);
                let faults = layout::frame_faults(&dragging, rect);
                assert!(
                    faults.is_empty(),
                    "{width} wide, dragging {active}: {faults:?}"
                );
                assert!(
                    dragging
                        .iter()
                        .any(|q| q.size == [2.0, HEADER - 8.0] && q.color == theme.accent),
                    "{width} wide, dragging {active}: no line"
                );
                panel.tab_drag = None;
            }
        }
    }

    /// Where a dragged tab would land, from the strip as it is laid out,
    /// and which way it scrolls a strip that does not fit.
    #[test]
    fn a_drag_lands_in_the_gap_nearest_the_pointer() {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let mut panel = panel_of(&["o0", "one", "two", "o1", "three"]);
        panel.shown = Some(vec![1, 2, 4]);
        panel.active = 4;
        let bar = header(900.0);
        let laid = strip(&panel, &mut atlas, bar);
        let listed = panel.listed();
        let mid = |i: usize| laid.tabs[i].whole.x + laid.tabs[i].whole.width * 0.5;
        assert_eq!(drop_slot(&laid, &listed, bar.x), 0);
        assert_eq!(drop_slot(&laid, &listed, mid(1) - 1.0), 0);
        assert_eq!(drop_slot(&laid, &listed, mid(1) + 1.0), 1);
        assert_eq!(drop_slot(&laid, &listed, mid(2) + 1.0), 2);
        assert_eq!(drop_slot(&laid, &listed, mid(4) + 1.0), 3);
        assert_eq!(drop_slot(&laid, &listed, bar.x + 880.0), 3);
        assert_eq!(drop_line(&laid, &listed, 1), Some(laid.tabs[2].whole.x));
        let last = laid.tabs[4].whole;
        assert_eq!(drop_line(&laid, &listed, 3), Some(last.x + last.width));
        // It fits: no end scrolls it.
        assert_eq!(drag_scroll(&laid, bar.x - 40.0), 0.0);
        // The third before the first.
        assert!(panel.drop_tab(&listed, 4, drop_slot(&laid, &listed, mid(1) - 1.0)));
        assert_eq!(titles(&panel), ["o0", "three", "one", "two", "o1"]);
        assert_eq!(panel.tabs[panel.active].title, "three");
        // Twelve in a narrow strip: its ends scroll it, the middle not.
        let names: Vec<String> = (0..12).map(|i| format!("bed {i}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let panel = panel_of(&names);
        let narrow = header(400.0);
        let laid = strip(&panel, &mut atlas, narrow);
        assert!(laid.max_scroll > 0.0);
        assert_eq!(drag_scroll(&laid, narrow.x + 4.0), -1.0);
        assert_eq!(drag_scroll(&laid, narrow.x - 30.0), -1.0);
        assert_eq!(drag_scroll(&laid, narrow.x + 200.0), 0.0);
        assert_eq!(drag_scroll(&laid, laid.area.x + laid.area.width - 4.0), 1.0);
        assert_eq!(drag_scroll(&laid, narrow.x + 500.0), 1.0);
    }

    #[test]
    fn the_report_names_the_tabs_in_the_strips_order() {
        let mut panel = panel_of(&["one", "two", "three", "other"]);
        panel.shown = Some(vec![0, 1, 2]);
        panel.active = 2;
        assert_eq!(
            panel.report(),
            "one,two,*three,(other) panes=- scrolled=no drag=-"
        );
        // The panes by title, the one with the keyboard starred: the
        // same two sessions after a move, by other places.
        assert!(panel.split(Edge::Right, 0));
        assert_eq!(panel.panes_text(|tab| tab.to_string()), "h(0.5,2,*0)");
        panel.move_tab(2, 0);
        assert_eq!(
            panel.report(),
            "three,*one,two,(other) panes=h(0.5,three,*one) scrolled=no drag=-"
        );
        assert_eq!(panel.panes_text(|tab| tab.to_string()), "h(0.5,0,*1)");
        assert!(panel.split(Edge::Bottom, 2));
        assert_eq!(
            panel.report(),
            "three,one,*two,(other) panes=h(0.5,three,v(0.5,one,*two)) scrolled=no drag=-"
        );
    }

    // ---- panes ---------------------------------------------------------------

    /// What must hold of the layouts whatever was done: a layout has two
    /// panes or more, each a live tab of the layout's workspace and in no
    /// other layout; no workspace has two layouts; a layout's keyboard is
    /// in one of its panes; and of the panes showing, the one with the
    /// keyboard is the active tab.
    fn check_panes(panel: &Panel, what: &str) {
        let mut seen: Vec<PaneId> = Vec::new();
        for (at, panes) in panel.layouts.iter().enumerate() {
            let leaves = panes.tree.leaves();
            assert!(leaves.len() > 1, "{what}: a layout of one pane");
            for id in leaves {
                let tab = panel
                    .tabs
                    .iter()
                    .find(|tab| tab.view.id == *id)
                    .unwrap_or_else(|| panic!("{what}: a pane for a tab that has gone"));
                assert_eq!(
                    tab.view.group, panes.group,
                    "{what}: {} is in another workspace's layout",
                    tab.title
                );
                assert!(!seen.contains(id), "{what}: {} is in two panes", tab.title);
                seen.push(*id);
            }
            assert!(
                panes.tree.contains(&panes.focus),
                "{what}: the keyboard is in no pane"
            );
            assert!(
                !panel.layouts[..at].iter().any(|p| p.group == panes.group),
                "{what}: two layouts for {:?}",
                panes.group
            );
        }
        if let Some(panes) = panel.panes() {
            assert_eq!(
                panel.index_of(panes.focus),
                Some(panel.active),
                "{what}: the pane with the keyboard is not the active tab"
            );
        }
        if !panel.tabs.is_empty() {
            assert!(panel.visible(true).contains(&panel.active), "{what}");
        }
    }

    /// Garden's g0 to g3 with sheds' s0 and s1 among them, as two
    /// workspaces' terminals lie in the order they were started, each
    /// told its workspace and garden's listed.
    fn two_workspaces() -> Panel {
        let mut panel = panel_of(&["g0", "s0", "g1", "g2", "s1", "g3"]);
        regroup(&mut panel);
        list(&mut panel, "garden");
        panel
    }

    /// As the window says it: a tab's workspace by the first letter of
    /// its title.
    fn regroup(panel: &mut Panel) {
        panel.regroup(|tab| {
            Some(match tab.title.chars().next() {
                Some('g') => "garden".to_owned(),
                Some('s') => "sheds".to_owned(),
                _ => "elsewhere".to_owned(),
            })
        });
    }

    /// The header lists the tabs of workspace `group`, as a frame leaves it.
    fn list(panel: &mut Panel, group: &str) {
        panel.shown = Some(
            (0..panel.tabs.len())
                .filter(|i| panel.tabs[*i].view.group.as_deref() == Some(group))
                .collect(),
        );
    }

    fn at(panel: &Panel, title: &str) -> usize {
        panel
            .tabs
            .iter()
            .position(|tab| tab.title == title)
            .unwrap_or_else(|| panic!("no tab {title}"))
    }

    fn panes_of(panel: &Panel) -> String {
        panel.panes_text(|i| panel.tabs[i].title.clone())
    }

    fn front(panel: &Panel) -> &str {
        &panel.tabs[panel.active].title
    }

    #[test]
    fn a_split_cuts_the_pane_with_the_keyboard_and_refuses_what_it_cannot_show() {
        let mut panel = two_workspaces();
        assert_eq!(panes_of(&panel), "-");
        assert!(panel.split(Edge::Right, at(&panel, "g1")));
        assert_eq!(panes_of(&panel), "h(0.5,g0,*g1)");
        assert_eq!(front(&panel), "g1", "the new pane has the keyboard");
        check_panes(&panel, "split right");
        assert!(panel.split(Edge::Bottom, at(&panel, "g2")));
        assert_eq!(panes_of(&panel), "h(0.5,g0,v(0.5,g1,*g2))");
        check_panes(&panel, "split down");

        let before = (panel.layouts.clone(), panel.active);
        let (g1, g2, s0) = (at(&panel, "g1"), at(&panel, "g2"), at(&panel, "s0"));
        assert!(!panel.split(Edge::Right, g2), "the session showing");
        assert!(!panel.split(Edge::Right, g1), "in a pane already");
        assert!(!panel.split(Edge::Right, s0), "another workspace's");
        assert!(!panel.split(Edge::Right, 99), "no such tab");
        assert_eq!((panel.layouts.clone(), panel.active), before);

        // Left and top put the new pane first.
        let mut panel = two_workspaces();
        assert!(panel.split(Edge::Left, at(&panel, "g1")));
        assert!(panel.split(Edge::Top, at(&panel, "g2")));
        assert_eq!(panes_of(&panel), "h(0.5,v(0.5,*g2,g1),g0)");
        check_panes(&panel, "left and top");

        // A workspace that could not be split leaves no layout behind.
        let mut one = panel_of(&["only"]);
        assert!(!one.split(Edge::Right, 0));
        assert!(one.layouts.is_empty());
    }

    #[test]
    fn a_tab_of_the_strip_shows_in_the_pane_with_the_keyboard() {
        let mut panel = two_workspaces();
        assert!(panel.split(Edge::Right, at(&panel, "g1")));
        assert!(panel.split(Edge::Bottom, at(&panel, "g2")));
        // g3 is in no pane: it takes the place of g2, which goes back to
        // being a tab of the strip alone.
        panel.show(at(&panel, "g3"));
        assert_eq!(panes_of(&panel), "h(0.5,g0,v(0.5,g1,*g3))");
        assert_eq!(panel.visible(true), [0, 2, 5]);
        check_panes(&panel, "g3 shown");
        // A tab that is a pane gets the keyboard where it is.
        panel.show(at(&panel, "g0"));
        assert_eq!(panes_of(&panel), "h(0.5,*g0,v(0.5,g1,g3))");
        check_panes(&panel, "g0 focused");
        // And the one it replaced comes back the same way.
        panel.show(at(&panel, "g2"));
        assert_eq!(panes_of(&panel), "h(0.5,*g2,v(0.5,g1,g3))");
        check_panes(&panel, "g2 shown");
        panel.show(99);
        assert_eq!(front(&panel), "g2", "no such tab: nothing moves");
    }

    #[test]
    fn the_keyboard_goes_round_the_panes_and_to_the_one_beside() {
        let mut panel = two_workspaces();
        assert!(!panel.focus_step(1), "one session: nowhere to go");
        assert!(panel.split(Edge::Right, at(&panel, "g1")));
        assert!(panel.split(Edge::Bottom, at(&panel, "g2")));
        // g0 | g1 over g2, the keyboard in g2.
        let mut order = Vec::new();
        for _ in 0..4 {
            assert!(panel.focus_step(1));
            order.push(front(&panel).to_owned());
            check_panes(&panel, "next");
        }
        assert_eq!(order, ["g0", "g1", "g2", "g0"]);
        assert!(panel.focus_step(-1));
        assert_eq!(front(&panel), "g2", "back round the end");
        assert!(panel.focus_step(-1));
        assert_eq!(front(&panel), "g1");

        let screen = Viewport {
            x: 8.0,
            y: 34.0,
            width: 1017.0,
            height: 617.0,
        };
        let mut go = |toward: Edge| {
            let moved = panel.focus_toward(screen, toward);
            check_panes(&panel, "toward");
            (moved, front(&panel).to_owned())
        };
        assert_eq!(go(Edge::Bottom), (true, "g2".to_owned()));
        assert_eq!(go(Edge::Bottom), (false, "g2".to_owned()), "the last row");
        assert_eq!(go(Edge::Left), (true, "g0".to_owned()));
        assert_eq!(go(Edge::Left), (false, "g0".to_owned()));
        assert_eq!(go(Edge::Top), (false, "g0".to_owned()));
        assert_eq!(go(Edge::Right), (true, "g1".to_owned()), "the top one");
        assert_eq!(go(Edge::Top), (false, "g1".to_owned()));
        assert_eq!(panes_of(&panel), "h(0.5,g0,v(0.5,*g1,g2))");
    }

    #[test]
    fn closing_a_pane_keeps_its_tab_and_closing_its_tab_frees_the_pane() {
        let mut panel = two_workspaces();
        assert!(!panel.close_pane(), "one session showing is no pane");
        assert!(panel.split(Edge::Right, at(&panel, "g1")));
        assert!(panel.split(Edge::Bottom, at(&panel, "g2")));
        // Close Pane on g2: g1 beside it has its room and the keyboard,
        // and g2 is still a tab.
        assert!(panel.close_pane());
        assert_eq!(panes_of(&panel), "h(0.5,g0,*g1)");
        assert_eq!(titles(&panel), ["g0", "s0", "g1", "g2", "s1", "g3"]);
        check_panes(&panel, "pane closed");
        // The last split closed leaves one session and no layout.
        assert!(panel.close_pane());
        assert_eq!((panes_of(&panel).as_str(), front(&panel)), ("-", "g0"));
        assert!(panel.layouts.is_empty());
        check_panes(&panel, "unsplit");

        // The tab of the pane with the keyboard closes (Cmd-W, or its
        // program ended): the pane beside it has the keyboard, not
        // whichever tab slid into its place.
        let mut panel = two_workspaces();
        assert!(panel.split(Edge::Right, at(&panel, "g3")));
        assert!(panel.split(Edge::Bottom, at(&panel, "g1")));
        assert_eq!(panes_of(&panel), "h(0.5,g0,v(0.5,g3,*g1))");
        panel.close_tab(at(&panel, "g1"));
        assert_eq!(titles(&panel), ["g0", "s0", "g2", "s1", "g3"]);
        assert_eq!(front(&panel), "g3", "not g2, which slid into its place");
        assert_eq!(panes_of(&panel), "h(0.5,g0,*g3)");
        assert_eq!(panel.closed_active, None, "its workspace still shows");
        check_panes(&panel, "focused tab closed");
        // A pane without the keyboard closes: the keyboard stays.
        panel.close_tab(at(&panel, "g0"));
        assert_eq!((panes_of(&panel).as_str(), front(&panel)), ("-", "g3"));
        assert_eq!(panel.closed_active, None);
        check_panes(&panel, "other pane closed");
        // With no pane left it closes as a tab always did: the window is
        // told, to show that workspace's next.
        let closed = at(&panel, "g3");
        panel.close_tab(closed);
        assert!(matches!(&panel.closed_active, Some((_, _, place)) if *place == closed));
        check_panes(&panel, "last closed");

        // A tab of the strip closes beside the panes: nothing of them moves.
        let mut panel = two_workspaces();
        assert!(panel.split(Edge::Right, at(&panel, "g1")));
        for title in ["s0", "g3", "g2", "s1"] {
            panel.close_tab(at(&panel, title));
            assert_eq!(panes_of(&panel), "h(0.5,g0,*g1)", "{title} closed");
            assert_eq!(front(&panel), "g1", "{title} closed");
            check_panes(&panel, title);
        }
        // Every tab closed, one by one, from every pane.
        while !panel.tabs.is_empty() {
            panel.close_tab(panel.active);
            check_panes(&panel, "closing all");
        }
        assert!(panel.layouts.is_empty() && !panel.open);
    }

    #[test]
    fn each_workspace_keeps_its_own_panes_and_the_keyboard_where_it_was() {
        let mut panel = two_workspaces();
        assert!(panel.split(Edge::Right, at(&panel, "g1")));
        assert!(panel.split(Edge::Bottom, at(&panel, "g2")));
        panel.show(at(&panel, "g0"));
        let garden = panes_of(&panel);
        assert_eq!(garden, "h(0.5,*g0,v(0.5,g1,g2))");

        // To sheds: its one session alone, garden's panes as they were.
        list(&mut panel, "sheds");
        panel.show(at(&panel, "s0"));
        assert_eq!(panes_of(&panel), "-");
        assert_eq!(panel.visible(true), [at(&panel, "s0")]);
        assert_eq!(panel.layouts.len(), 1);
        check_panes(&panel, "in sheds");
        // Sheds splits too: two layouts, a workspace each.
        assert!(panel.split(Edge::Bottom, at(&panel, "s1")));
        assert_eq!(panes_of(&panel), "v(0.5,s0,*s1)");
        assert!(
            !panel.split(Edge::Right, at(&panel, "g3")),
            "garden's tab is not for sheds' panes"
        );
        check_panes(&panel, "sheds split");

        // Back to garden, by the pane that had the keyboard.
        list(&mut panel, "garden");
        let focused = panel.focused_in(Some("garden")).expect("garden's panes");
        assert_eq!(panel.tabs[focused].title, "g0");
        panel.show(focused);
        assert_eq!(panes_of(&panel), garden);
        check_panes(&panel, "back in garden");
        // A tab of garden's strip shown from sheds takes garden's pane
        // with the keyboard, and none of sheds'.
        list(&mut panel, "sheds");
        panel.show(at(&panel, "s0"));
        list(&mut panel, "garden");
        panel.show(at(&panel, "g3"));
        assert_eq!(panes_of(&panel), "h(0.5,*g3,v(0.5,g1,g2))");
        assert_eq!(panel.focused_in(Some("sheds")), Some(at(&panel, "s0")));
        assert_eq!(panel.focused_in(Some("nowhere")), None);
        check_panes(&panel, "g3 from sheds");
    }

    #[test]
    fn a_session_that_leaves_its_workspace_leaves_its_pane() {
        let mut panel = two_workspaces();
        assert!(panel.split(Edge::Right, at(&panel, "g1")));
        assert!(panel.split(Edge::Bottom, at(&panel, "g2")));
        // g2's shell went to a folder of its own: the window says so.
        let moved = at(&panel, "g2");
        panel.tabs[moved].title = "e0".into();
        regroup(&mut panel);
        list(&mut panel, "elsewhere");
        assert_eq!(front(&panel), "e0", "it is still the one typed into");
        assert_eq!(panes_of(&panel), "-", "alone in its new workspace");
        assert_eq!(panel.layouts.len(), 1);
        assert_eq!(
            panel.focused_in(Some("garden")),
            Some(at(&panel, "g1")),
            "the pane beside it has garden's keyboard"
        );
        check_panes(&panel, "left");
        // Back in garden, it shows where garden's keyboard is.
        panel.tabs[moved].title = "g2".into();
        regroup(&mut panel);
        list(&mut panel, "garden");
        panel.settle();
        assert_eq!(panes_of(&panel), "h(0.5,g0,*g2)");
        check_panes(&panel, "back");
        // Both of a layout's panes gone to other workspaces: no layout.
        for title in ["g0", "g2"] {
            let tab = at(&panel, title);
            panel.tabs[tab].title = format!("x{title}");
        }
        regroup(&mut panel);
        assert!(panel.layouts.is_empty());
        check_panes(&panel, "emptied");
    }

    /// Everything a window does to the panes, over and over in an order
    /// made up from a seed: after each, the layouts hold together.
    #[test]
    fn the_layouts_hold_together_through_any_run_of_changes() {
        let mut seed = 0x9e37_79b9_u32;
        let mut next = move |below: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as usize % below.max(1)
        };
        let edges = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];
        let screen = Viewport {
            x: 8.0,
            y: 34.0,
            width: 1017.0,
            height: 617.0,
        };
        let mut made = 0;
        let mut panel = two_workspaces();
        for step in 0..4000 {
            let len = panel.tabs.len();
            let what = match next(9) {
                _ if len < 3 => {
                    made += 1;
                    let group = ["g", "s"][next(2)];
                    panel.tabs.push(Tab {
                        title: format!("{group}n{made}"),
                        ..tab()
                    });
                    regroup(&mut panel);
                    panel.show(panel.tabs.len() - 1);
                    "open"
                }
                0 => {
                    panel.split(edges[next(4)], next(len));
                    "split"
                }
                1 => {
                    panel.show(next(len));
                    "show"
                }
                2 => {
                    panel.close_pane();
                    "close pane"
                }
                3 => {
                    panel.close_tab(next(len));
                    "close tab"
                }
                4 => {
                    panel.move_tab(next(len), next(len));
                    "move"
                }
                5 => {
                    panel.focus_step(if next(2) == 0 { 1 } else { -1 });
                    "step"
                }
                6 => {
                    panel.focus_toward(screen, edges[next(4)]);
                    "toward"
                }
                7 => {
                    // A shell cds into the other workspace's folder.
                    let tab = next(len);
                    let title = panel.tabs[tab].title.clone();
                    let other = if title.starts_with('g') { "s" } else { "g" };
                    panel.tabs[tab].title = format!("{other}{}", &title[1..]);
                    regroup(&mut panel);
                    "regroup"
                }
                _ => {
                    made += 1;
                    panel.tabs.push(Tab {
                        title: format!("gm{made}"),
                        ..tab()
                    });
                    regroup(&mut panel);
                    panel.show(panel.tabs.len() - 1);
                    "open"
                }
            };
            // A frame: the tabs of the workspace showing are listed.
            if let Some(group) = panel.active_tab().and_then(|tab| tab.view.group.clone()) {
                list(&mut panel, &group);
            }
            panel.settle();
            check_panes(&panel, &format!("step {step}, {what}"));
            // Whatever is typed goes to the pane drawn with the keyboard.
            if let Some(panes) = panel.panes() {
                let focused = panel.index_of(panes.focus);
                assert_eq!(focused, Some(panel.active), "step {step}, {what}");
                assert!(
                    panel
                        .screens(screen)
                        .iter()
                        .any(|(tab, _)| *tab == panel.active),
                    "step {step}, {what}: the active tab has no screen"
                );
            }
        }
    }

    #[test]
    fn each_pane_scrolls_and_selects_on_its_own() {
        let mut panel = two_workspaces();
        assert!(panel.split(Edge::Right, at(&panel, "g1")));
        let (g0, g1, g2) = (at(&panel, "g0"), at(&panel, "g1"), at(&panel, "g2"));
        let picked = Some(((4, 1), (6, 9)));
        panel.tabs[g1].view.back = 12;
        panel.tabs[g1].view.selection = picked;
        assert_eq!((panel.back(), panel.selection()), (12, picked));
        // The keyboard to the other pane: this one stays as it was, and
        // the other has its own.
        panel.show(g0);
        assert_eq!((panel.back(), panel.selection()), (0, None));
        assert_eq!(panel.tabs[g1].view.back, 12);
        assert_eq!(panel.tabs[g1].view.selection, picked);
        panel.tabs[g0].view.back = 3;
        panel.tabs[g0].view.selection = Some(((1, 0), (1, 5)));
        panel.focus_step(1);
        assert_eq!(
            (panel.active, panel.back(), panel.selection()),
            (g1, 12, picked)
        );
        assert_eq!(panel.tabs[g0].view.back, 3);
        // A drag in one pane is that pane's: letting go ends it there and
        // touches nothing of the other.
        panel.tabs[g0].view.selecting = true;
        panel.tabs[g0].view.drag_point = Some((40.0, 9.0));
        assert_eq!(panel.selecting(), Some(g0));
        assert!(panel.selecting_dragged());
        panel.end_selecting();
        assert_eq!(panel.selecting(), None);
        assert!(!panel.selecting_dragged());
        assert_eq!(panel.tabs[g0].view.selection, Some(((1, 0), (1, 5))));
        assert_eq!(panel.tabs[g1].view.selection, picked);
        // A press that never moved selected nothing, in its pane only.
        panel.tabs[g0].view.selection = Some(((2, 7), (2, 7)));
        panel.tabs[g0].view.selecting = true;
        panel.end_selecting();
        assert_eq!(panel.tabs[g0].view.selection, None);
        assert_eq!(panel.tabs[g1].view.selection, picked);
        // A tab shown from the strip starts at its output; the pane it
        // did not take is as it was.
        panel.tabs[g2].view.back = 40;
        panel.show(g2);
        assert_eq!(panes_of(&panel), "h(0.5,g0,*g2)");
        assert_eq!((panel.back(), panel.selection()), (0, None));
        assert_eq!(panel.tabs[g0].view.back, 3);
        // The one that left keeps nothing to come back to stale: shown
        // again it starts at its output too.
        panel.show(g1);
        assert_eq!((panel.back(), panel.selection()), (0, None));
        // Closing a pane's tab leaves the other pane's view alone.
        panel.tabs[g0].view.back = 5;
        panel.close_tab(panel.active);
        assert_eq!((front(&panel), panel.back()), ("g0", 5));
        // No two tabs are ever called the same.
        let mut ids: Vec<PaneId> = panel.tabs.iter().map(|tab| tab.view.id).collect();
        ids.dedup();
        assert_eq!(ids.len(), panel.tabs.len());
    }

    /// The panel's screen in the tests of sizes: 1000 by 600 inside its
    /// padding.
    fn wide_screen() -> Viewport {
        Viewport {
            x: 8.0,
            y: 34.0,
            width: 1000.0,
            height: 600.0,
        }
    }

    fn resizes(panel: &Panel) -> Vec<usize> {
        panel.tabs.iter().map(|tab| tab.session.resizes()).collect()
    }

    fn grids(panel: &Panel) -> Vec<(usize, usize)> {
        panel
            .tabs
            .iter()
            .map(|tab| {
                let term = tab.session.term.lock().unwrap();
                (term.cols(), term.rows())
            })
            .collect()
    }

    #[test]
    fn a_change_of_layout_tells_each_session_it_touches_its_size_once() {
        let atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let screen = wide_screen();
        let whole = grid_size(&atlas, screen);
        let mut panel = panel_of(&["g0", "g1", "g2", "s0"]);
        regroup(&mut panel);
        // The first frame: every session the size of the panel.
        panel.fit(&atlas, screen);
        assert_eq!(resizes(&panel), [1, 1, 1, 1]);
        assert_eq!(grids(&panel), [whole; 4]);
        for _ in 0..3 {
            panel.fit(&atlas, screen);
        }
        assert_eq!(resizes(&panel), [1, 1, 1, 1], "frames alone tell nothing");

        // Split right: the two panes hear their size, once, and the tabs
        // in no pane hear nothing.
        assert!(panel.split(Edge::Right, 1));
        for _ in 0..3 {
            panel.fit(&atlas, screen);
        }
        assert_eq!(resizes(&panel), [2, 2, 1, 1]);
        let panes = panel.screens(screen);
        assert_eq!(grids(&panel)[0], grid_size(&atlas, panes[0].1));
        assert_eq!(grids(&panel)[1], grid_size(&atlas, panes[1].1));
        assert!(grids(&panel)[0].0 < whole.0 / 2 + 1);
        assert_eq!(grids(&panel)[2], whole);

        // Split down in the right pane: the one cut and the new one. The
        // left pane is as it was and hears nothing.
        assert!(panel.split(Edge::Bottom, 2));
        for _ in 0..3 {
            panel.fit(&atlas, screen);
        }
        assert_eq!(resizes(&panel), [2, 3, 2, 1]);

        // To the other workspace and back: its panes keep their sizes
        // while they do not show, so nobody is told anything.
        panel.show(3);
        panel.fit(&atlas, screen);
        assert_eq!(panel.visible(true), [3]);
        panel.show(0);
        panel.fit(&atlas, screen);
        assert_eq!(resizes(&panel), [2, 3, 2, 1]);

        // Close Pane: the pane beside it grows, once; the session that
        // left its pane is the size of the panel again, once.
        panel.show(2);
        assert!(panel.close_pane());
        for _ in 0..3 {
            panel.fit(&atlas, screen);
        }
        assert_eq!(resizes(&panel), [2, 4, 3, 1]);
        assert_eq!(grids(&panel)[2], whole);
    }

    #[test]
    fn a_divider_dragged_tells_no_session_a_size_until_it_is_let_go() {
        let atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let screen = wide_screen();
        let mut panel = panel_of(&["g0", "g1", "g2"]);
        regroup(&mut panel);
        assert!(panel.split(Edge::Right, 1));
        assert!(panel.split(Edge::Bottom, 2));
        panel.fit(&atlas, screen);
        let told = resizes(&panel);
        let sized = grids(&panel);
        let was = panel.screens(screen);

        // The divider between g0 and the rest, pressed a little off its
        // middle and dragged left in steps, a frame after each.
        let bar = panel.dividers(screen)[0];
        let (x, y) = (bar.rect.x + 3.0, bar.rect.y + 200.0);
        assert_eq!(panel.divider_at(screen, x, y).map(|d| d.id), Some(0));
        panel.grab_divider(&bar, x, y);
        let mut moved = 0;
        for step in 1..=40 {
            if panel.drag_divider(&atlas, screen, x - step as f32 * 5.0, y) {
                moved += 1;
            }
            panel.fit(&atlas, screen);
            assert_eq!(resizes(&panel), told, "step {step}");
            assert_eq!(grids(&panel), sized, "step {step}");
        }
        assert_eq!(moved, 40, "the panes followed every step");
        let now = panel.screens(screen);
        assert_eq!(now[0].1.width, was[0].1.width - 200.0);
        assert_eq!(now[1].1.width, was[1].1.width + 200.0);
        check_panes(&panel, "dragged");

        // Let go: each pane whose room changed hears its size, once.
        panel.divider_drag = None;
        for _ in 0..3 {
            panel.fit(&atlas, screen);
        }
        assert_eq!(
            resizes(&panel),
            told.iter().map(|n| n + 1).collect::<Vec<_>>()
        );
        for (tab, pane) in panel.screens(screen) {
            assert_eq!(grids(&panel)[tab], grid_size(&atlas, pane));
        }

        // The stacked one moves up and down, and only its two panes hear.
        let told = resizes(&panel);
        let bar = panel.dividers(screen)[1];
        assert_eq!(bar.dir, Dir::Vertical);
        let (x, y) = (bar.rect.x + 50.0, bar.rect.y + 8.5);
        panel.grab_divider(&bar, x, y);
        assert!(panel.drag_divider(&atlas, screen, x + 300.0, y + 90.0));
        panel.fit(&atlas, screen);
        assert_eq!(resizes(&panel), told);
        panel.divider_drag = None;
        panel.fit(&atlas, screen);
        assert_eq!(resizes(&panel), [told[0], told[1] + 1, told[2] + 1]);
        let now = panel.screens(screen);
        assert_eq!(now[0].1, panel.screens(screen)[0].1);
        assert_eq!(now[1].1.height + now[2].1.height, 600.0 - PANE_GAP);
    }

    #[test]
    fn a_divider_stops_where_a_pane_would_be_too_small_and_a_double_click_evens_it() {
        let atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let screen = wide_screen();
        let advance = atlas.metrics.advance;
        let row = row_height(&atlas);
        let mut panel = panel_of(&["g0", "g1", "g2"]);
        regroup(&mut panel);
        assert!(panel.split(Edge::Right, 1));
        assert!(panel.split(Edge::Bottom, 2));
        let bar = panel.dividers(screen)[0];
        let middle = (bar.rect.x + bar.rect.width * 0.5, bar.rect.y + 9.0);
        panel.grab_divider(&bar, middle.0, middle.1);
        // Far past the left edge: the left pane keeps twenty columns.
        assert!(panel.drag_divider(&atlas, screen, -4000.0, 0.0));
        let panes = panel.screens(screen);
        assert_eq!(grid_size(&atlas, panes[0].1).0, PANE_MIN_COLS);
        assert!(panes[0].1.width >= PANE_MIN_COLS as f32 * advance);
        assert!(panes[0].1.width < (PANE_MIN_COLS + 1) as f32 * advance);
        assert!(
            !panel.drag_divider(&atlas, screen, -9000.0, 0.0),
            "held there"
        );
        // Far past the right edge: the two on the right keep theirs.
        assert!(panel.drag_divider(&atlas, screen, 9000.0, 0.0));
        for (tab, pane) in panel.screens(screen).into_iter().skip(1) {
            assert_eq!(grid_size(&atlas, pane).0, PANE_MIN_COLS, "tab {tab}");
        }
        // The stacked one keeps five rows on either side.
        let bar = panel.dividers(screen)[1];
        panel.grab_divider(&bar, bar.rect.x + 9.0, bar.rect.y + 8.5);
        assert!(panel.drag_divider(&atlas, screen, 0.0, -4000.0));
        let panes = panel.screens(screen);
        assert_eq!(grid_size(&atlas, panes[1].1).1, PANE_MIN_ROWS);
        assert!(panes[1].1.height < (PANE_MIN_ROWS + 1) as f32 * row);
        assert!(panel.drag_divider(&atlas, screen, 0.0, 4000.0));
        let panes = panel.screens(screen);
        assert_eq!(grid_size(&atlas, panes[2].1).1, PANE_MIN_ROWS);
        check_panes(&panel, "clamped");

        // A double click on each puts it back in the middle, and lets go
        // of a divider held.
        assert!(panel.center_divider(&atlas, screen, 1));
        assert!(panel.center_divider(&atlas, screen, 0));
        assert_eq!(panel.divider_drag, None);
        assert_eq!(panes_of(&panel), "h(0.5,g0,v(0.5,g1,*g2))");
        assert!(!panel.center_divider(&atlas, screen, 0), "already even");
        assert!(!panel.center_divider(&atlas, screen, 7), "no such divider");

        // A pane too small to cut in two is not split.
        assert!(panel.can_split(&atlas, screen, Edge::Right));
        let narrow = Viewport {
            width: PANE_MIN_COLS as f32 * advance * 4.0,
            height: PANE_MIN_ROWS as f32 * row * 3.0,
            ..screen
        };
        assert!(!panel.can_split(&atlas, narrow, Edge::Right));
        assert!(!panel.can_split(&atlas, narrow, Edge::Bottom));
        panel.show(0);
        assert!(panel.can_split(&atlas, narrow, Edge::Bottom));
        assert!(!panel.can_split(&atlas, narrow, Edge::Left));
    }

    #[test]
    fn a_drag_ends_when_the_panes_change_shape_or_another_workspace_shows() {
        let atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let screen = wide_screen();
        let hold = |panel: &mut Panel| {
            let bar = panel.dividers(screen)[0];
            panel.grab_divider(&bar, bar.rect.x + 8.0, bar.rect.y + 8.0);
            assert!(panel.divider_drag.is_some());
        };
        let mut panel = two_workspaces();
        assert!(panel.split(Edge::Right, at(&panel, "g1")));
        hold(&mut panel);
        assert!(panel.split(Edge::Bottom, at(&panel, "g2")));
        assert_eq!(panel.divider_drag, None, "a split");
        hold(&mut panel);
        panel.close_tab(at(&panel, "g0"));
        assert_eq!(panel.divider_drag, None, "a pane's program ended");
        hold(&mut panel);
        assert!(panel.close_pane());
        assert_eq!(panel.divider_drag, None, "a pane closed");

        // Held while another workspace comes to the front: its panes are
        // not the ones the divider is among, and nothing of theirs moves.
        let mut panel = two_workspaces();
        assert!(panel.split(Edge::Right, at(&panel, "g1")));
        hold(&mut panel);
        list(&mut panel, "sheds");
        panel.show(at(&panel, "s0"));
        assert!(panel.split(Edge::Right, at(&panel, "s1")));
        hold(&mut panel);
        let sheds = panel.screens(screen);
        list(&mut panel, "garden");
        panel.show(at(&panel, "g0"));
        let garden = panel.screens(screen);
        assert!(!panel.drag_divider(&atlas, screen, 100.0, 100.0));
        assert_eq!(panel.divider_drag, None);
        assert_eq!(panel.screens(screen), garden);
        panel.show(at(&panel, "s0"));
        assert_eq!(panel.screens(screen), sheds);
    }

    #[test]
    fn a_point_is_in_the_pane_under_it_or_the_nearest_and_a_gap_is_its_divider() {
        let screen = wide_screen();
        let mut panel = panel_of(&["g0", "g1", "g2"]);
        regroup(&mut panel);
        assert_eq!(panel.pane_at(screen, 500.0, 300.0), 0, "one session");
        assert!(panel.dividers(screen).is_empty());
        assert_eq!(panel.divider_at(screen, 500.0, 300.0), None);
        assert!(panel.split(Edge::Right, 1));
        assert!(panel.split(Edge::Bottom, 2));
        // g0 is 491 wide from 8; g1 and g2 start at 516, g2 under g1.
        let panes = panel.screens(screen);
        assert_eq!(panes[0].1.x + panes[0].1.width, 499.0);
        assert_eq!(panes[1].1.x, 516.0);
        assert_eq!(panel.pane_at(screen, 100.0, 300.0), 0);
        assert_eq!(panel.pane_at(screen, 700.0, 100.0), 1);
        assert_eq!(panel.pane_at(screen, 700.0, 500.0), 2);
        // In the gap: the nearer pane. Off the screen, in the padding:
        // the pane beside it.
        assert_eq!(panel.pane_at(screen, 503.0, 100.0), 0);
        assert_eq!(panel.pane_at(screen, 512.0, 100.0), 1);
        assert_eq!(panel.pane_at(screen, 512.0, 500.0), 2);
        assert_eq!(panel.pane_at(screen, 2.0, 640.0), 0);
        assert_eq!(panel.pane_at(screen, 1012.0, 30.0), 1);

        // A press anywhere in a gap, or two points into the panes beside
        // it, grabs its divider; further into a pane it is the pane's.
        let at = |x: f32, y: f32| panel.divider_at(screen, x, y).map(|d| d.id);
        assert_eq!(at(499.0, 300.0), Some(0));
        assert_eq!(at(515.9, 300.0), Some(0));
        assert_eq!(at(497.0, 300.0), Some(0), "in the slop");
        assert_eq!(at(517.9, 100.0), Some(0), "in the slop");
        assert_eq!(at(496.9, 300.0), None);
        assert_eq!(at(518.0, 100.0), None);
        let stacked = panel.dividers(screen)[1];
        assert_eq!(stacked.dir, Dir::Vertical);
        assert_eq!(at(700.0, stacked.rect.y + 8.0), Some(1));
        assert_eq!(at(700.0, stacked.rect.y - 2.0), Some(1));
        assert_eq!(at(700.0, stacked.rect.y - 2.1), None);
        assert_eq!(at(300.0, stacked.rect.y + 8.0), None, "g0 runs past it");
    }

    #[test]
    fn a_grid_larger_or_smaller_than_its_pane_is_drawn_inside_it() {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let theme = Theme::default();
        let rect = Viewport {
            x: 0.0,
            y: 0.0,
            width: 1016.0,
            height: 638.0,
        };
        let (_, screen) = split(rect);
        let solid = atlas.solid_uv();
        // Three sessions, each a full screen of letters at the size of
        // the whole panel, then one much smaller: none knows of its pane.
        let whole = grid_size(&atlas, screen);
        let mut panel = panel_of(&["g0", "g1", "g2"]);
        panel.focus = true;
        regroup(&mut panel);
        for (n, tab) in panel.tabs.iter().enumerate() {
            let (cols, rows) = if n == 2 { (12, 3) } else { whole };
            tab.session.resize(cols, rows);
            let line = "W".repeat(cols);
            for _ in 0..rows {
                tab.session.feed(format!("\r\n{line}").as_bytes());
            }
        }
        assert!(panel.split(Edge::Right, 1));
        assert!(panel.split(Edge::Bottom, 2));
        // A divider on its way, far from where the sessions were sized.
        let bar = panel.dividers(screen)[0];
        panel.grab_divider(&bar, bar.rect.x + 8.0, bar.rect.y + 8.0);
        assert!(panel.drag_divider(&atlas, screen, 700.0, 0.0));
        let mut out = Vec::new();
        draw(&panel, None, &mut atlas, rect, &theme, &mut out);
        let panes = panel.screens(screen);
        let (header, _) = split(rect);
        let mut letters = [0usize; 3];
        for quad in out.iter().filter(|q| q.uv != solid) {
            // The header's own labels are above the screens.
            if quad.pos[1] + quad.size[1] <= header.y + header.height {
                continue;
            }
            let (x0, x1) = (quad.pos[0], quad.pos[0] + quad.size[0]);
            let (y0, y1) = (quad.pos[1], quad.pos[1] + quad.size[1]);
            let home = panes.iter().position(|(_, pane)| {
                x0 >= pane.x - PAD * 0.5
                    && x1 <= pane.x + pane.width + PAD * 0.5
                    && y0 >= pane.y - 1.0
                    && y1 <= pane.y + pane.height + 1.0
            });
            match home {
                Some(pane) => letters[pane] += 1,
                None => panic!("a glyph outside every pane: {quad:?} in {panes:?}"),
            }
        }
        let advance = atlas.metrics.advance;
        let row = row_height(&atlas);
        // The large grids are cut to the cells their panes have room for.
        for pane in 0..2 {
            let (_, at) = panes[pane];
            let fits = (at.width / advance).floor() as usize * (at.height / row).floor() as usize;
            assert_eq!(letters[pane], fits, "pane {pane}");
        }
        // The small one is all there, with the rest of its pane empty.
        assert_eq!(letters[2], 12 * 3);
        // The divider held is drawn in the accent, the others not.
        let lines: Vec<_> = out
            .iter()
            .filter(|q| q.uv == solid && (q.size[0] == 1.0 || q.size[1] == 1.0))
            .filter(|q| q.pos[1] >= screen.y - PAD)
            .collect();
        assert!(
            lines
                .iter()
                .any(|q| q.color == theme.accent && q.size[0] == 1.0)
        );
        assert!(
            lines
                .iter()
                .any(|q| q.color == theme.hairline && q.size[1] == 1.0)
        );
    }

    #[test]
    fn the_lines_between_panes_meet_and_the_focus_line_is_over_its_pane() {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let theme = Theme::default();
        let rect = Viewport {
            x: 0.0,
            y: 0.0,
            width: 1016.0,
            height: 638.0,
        };
        let (_, screen) = split(rect);
        let mut panel = panel_of(&["g0", "g1", "g2"]);
        panel.focus = true;
        regroup(&mut panel);
        let solid = atlas.solid_uv();
        let row = row_height(&atlas);
        let rules = |panel: &Panel, atlas: &mut Atlas| -> Vec<([f32; 2], [f32; 2], [f32; 4])> {
            let mut out = Vec::new();
            draw(panel, None, atlas, rect, &theme, &mut out);
            out.iter()
                .filter(|q| q.uv == solid && q.pos[1] > rect.y + HEADER - 1.0)
                .filter(|q| q.color == theme.hairline || q.color == theme.accent)
                .filter(|q| q.size[0] <= 2.0 || q.size[1] <= 2.0)
                // Not a cursor, which is a row tall.
                .filter(|q| q.size[1] != row)
                .map(|q| (q.pos, q.size, q.color))
                .collect()
        };
        // One session: no line, no focus line.
        assert!(
            rules(&panel, &mut atlas)
                .iter()
                .all(|(_, size, _)| size[0] > 2.0)
        );
        assert!(panel.split(Edge::Right, 1));
        assert!(panel.split(Edge::Bottom, 2));
        let drawn = rules(&panel, &mut atlas);
        let panes = panel.screens(screen);
        let bars = panel.dividers(screen);
        // The upright line down the middle of its gap, the whole height
        // of the panel under its header.
        let upright = drawn
            .iter()
            .find(|(_, size, _)| size[0] == 1.0)
            .expect("the line between g0 and the rest");
        assert_eq!(upright.0[0], bars[0].rect.x + 8.0);
        assert_eq!(upright.0[1], screen.y - PAD * 0.5);
        assert_eq!(upright.1[1], screen.height + PAD);
        // The level one from that line to the panel's right edge.
        let level = drawn
            .iter()
            .find(|(_, size, colour)| {
                size[1] == 1.0 && *colour == theme.hairline && size[0] < 600.0
            })
            .expect("the line between g1 and g2");
        assert_eq!(level.0[1], bars[1].rect.y + 8.0);
        assert_eq!(
            level.0[0],
            upright.0[0] + 1.0,
            "it starts at the upright one"
        );
        assert_eq!(level.0[0] + level.1[0], rect.x + rect.width);
        // The focus line over the pane with the keyboard, in the accent
        // while the terminal has it: a thin rule against the pane's first
        // row, as wide as the pane, and the only one there.
        let over = |panel: &Panel, atlas: &mut Atlas, pane: Viewport| {
            let found: Vec<_> = rules(panel, atlas)
                .into_iter()
                .filter(|(pos, size, _)| size[1] == 1.0 && pos[1] == pane.y - 1.0)
                .collect();
            assert_eq!(found.len(), 1, "one focus line: {found:?}");
            found[0]
        };
        let focused = panes[2].1;
        let (pos, size, colour) = over(&panel, &mut atlas, focused);
        assert_eq!(pos, [focused.x, focused.y - 1.0]);
        assert_eq!(size, [focused.width, 1.0]);
        assert_eq!(colour, theme.accent);
        // No other pane has one.
        for (_, pane) in &panes[..2] {
            assert!(
                !rules(&panel, &mut atlas)
                    .iter()
                    .any(|(pos, _, _)| pos[1] == pane.y - 1.0)
            );
        }
        // Over a pane that touches the strip it is clear of the strip,
        // where the active tab's own rule is, and half that rule's weight.
        panel.show(0);
        let (pos, size, _) = over(&panel, &mut atlas, panes[0].1);
        assert_eq!((pos[0], size[0]), (panes[0].1.x, panes[0].1.width));
        assert_eq!(pos[1] - (rect.y + HEADER), 3.0);
        let mut out = Vec::new();
        draw(&panel, None, &mut atlas, rect, &theme, &mut out);
        let tab_rule = out
            .iter()
            .find(|q| q.color == theme.accent && q.pos[1] == rect.y + HEADER - 2.0)
            .expect("the active tab's rule");
        assert_eq!(tab_rule.size[1], 2.0);
        panel.focus = false;
        assert_eq!(over(&panel, &mut atlas, panes[0].1).2, theme.hairline);
    }

    // ---- a tab dragged onto a pane -------------------------------------------

    const CENTER: Zone = Zone::Center;
    const LEFT: Zone = Zone::Edge(Edge::Left);
    const RIGHT: Zone = Zone::Edge(Edge::Right);
    const TOP: Zone = Zone::Edge(Edge::Top);
    const BOTTOM: Zone = Zone::Edge(Edge::Bottom);

    /// The panel the drags below are in: its strip over [`wide_screen`].
    fn drag_rect() -> Viewport {
        Viewport {
            x: 0.0,
            y: 0.0,
            width: 1016.0,
            height: 638.0,
        }
    }

    /// A point in `zone` of the pane showing the session titled `over`.
    fn zone_point(panel: &Panel, over: &str, zone: Zone) -> (f32, f32) {
        let over = at(panel, over);
        let (_, pane) = panel
            .screens(wide_screen())
            .into_iter()
            .find(|(tab, _)| *tab == over)
            .expect("a pane showing it");
        let (fx, fy) = match zone {
            Zone::Center => (0.5, 0.5),
            Zone::Edge(Edge::Left) => (0.1, 0.5),
            Zone::Edge(Edge::Right) => (0.9, 0.5),
            Zone::Edge(Edge::Top) => (0.5, 0.1),
            Zone::Edge(Edge::Bottom) => (0.5, 0.9),
        };
        (pane.x + pane.width * fx, pane.y + pane.height * fy)
    }

    /// A press on the tab titled `tab`, as the window sends it, then the
    /// pointer taken to `zone` of the pane showing `over`. Where it is,
    /// and where the tab would land.
    fn lift(
        panel: &mut Panel,
        atlas: &Atlas,
        tab: &str,
        over: &str,
        zone: Zone,
    ) -> ((f32, f32), Option<PaneDrop>) {
        let (header, screen) = split(drag_rect());
        panel.press_tab(at(panel, tab), 60.0, 15.0);
        let (x, y) = zone_point(panel, over, zone);
        assert!(panel.drag_tab(atlas, header, screen, true, x, y));
        let drag = panel.tab_drag.expect("a tab held");
        assert!(drag.active && drag.pane);
        ((x, y), drag.drop)
    }

    /// [`lift`], then the button up there. Whether the panes changed.
    fn drop_on(panel: &mut Panel, atlas: &Atlas, tab: &str, over: &str, zone: Zone) -> bool {
        let ((x, y), drop) = lift(panel, atlas, tab, over, zone);
        let landed = panel.drop_pane(atlas, wide_screen(), x, y);
        assert_eq!(landed, drop.is_some(), "what lit up is what happened");
        assert_eq!(panel.tab_drag, None, "the drag is over");
        check_panes(panel, "after a drop");
        landed
    }

    #[test]
    fn a_held_tab_is_for_the_strip_until_it_is_taken_under_it() {
        let atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let (header, screen) = split(drag_rect());
        let mut panel = two_workspaces();
        panel.press_tab(at(&panel, "g1"), 60.0, 15.0);
        assert_eq!(front(&panel), "g1", "the press shows it");
        let state = |panel: &Panel| {
            let drag = panel.tab_drag.expect("a tab held");
            (drag.active, drag.pane)
        };
        assert_eq!(state(&panel), (false, false));
        // A point or two is a click still; past that it is on its way
        // along the strip.
        assert!(!panel.drag_tab(&atlas, header, screen, true, 62.0, 16.0));
        assert!(panel.drag_tab(&atlas, header, screen, true, 80.0, 16.0));
        assert_eq!(state(&panel), (true, false));
        assert_eq!(
            panel.report(),
            "g0,(s0),*g1,g2,(s1),g3 panes=- scrolled=no drag=g1"
        );
        // Just under the strip is still the strip's.
        let under = HEADER + PANE_DROP_AFTER;
        assert!(panel.drag_tab(&atlas, header, screen, true, 80.0, under));
        assert_eq!(state(&panel), (true, false));
        assert_eq!(panel.drop_report(), "-");
        // Further down it is on its way to a pane, and stays so until it
        // is level with the strip again.
        assert!(panel.drag_tab(&atlas, header, screen, true, 900.0, under + 1.0));
        assert_eq!(state(&panel), (true, true));
        assert!(panel.drag_tab(&atlas, header, screen, true, 900.0, HEADER + 2.0));
        assert_eq!(state(&panel), (true, true));
        assert_eq!(panel.drop_report(), "g1:top");
        assert!(panel.drag_tab(&atlas, header, screen, true, 900.0, HEADER - 1.0));
        assert_eq!(state(&panel), (true, false));
        assert_eq!(panel.drop_report(), "-", "nothing lit from the strip");
        assert_eq!(panel.tab_drag.and_then(|drag| drag.drop), None);
        // Let go on the strip it is a move along it, as before.
        let listed = panel.listed();
        assert!(panel.drop_tab(&listed, at(&panel, "g1"), 0));
        assert_eq!(titles(&panel), ["g1", "g0", "s0", "g2", "s1", "g3"]);
        assert_eq!(panes_of(&panel), "-");

        // Straight down from the press, never sideways: a drag all the
        // same.
        let mut panel = two_workspaces();
        panel.press_tab(at(&panel, "g1"), 60.0, 15.0);
        assert!(panel.drag_tab(&atlas, header, screen, true, 60.0, 300.0));
        assert_eq!(state(&panel), (true, true));
        // Where there are no panes to be had it never leaves the strip.
        let mut panel = two_workspaces();
        panel.press_tab(at(&panel, "g1"), 60.0, 15.0);
        assert!(!panel.drag_tab(&atlas, header, screen, false, 60.0, 300.0));
        assert_eq!(state(&panel), (false, false));
        assert!(panel.drag_tab(&atlas, header, screen, false, 900.0, 300.0));
        assert_eq!(state(&panel), (true, false));
        assert_eq!(panel.drop_at(&atlas, screen, 900.0, 300.0), None);
        assert!(!panel.drop_pane(&atlas, screen, 900.0, 300.0));
        assert_eq!(panes_of(&panel), "-");
    }

    #[test]
    fn a_tab_dropped_on_one_session_makes_two_panes_of_it() {
        let atlas = Atlas::build("SF Mono", 13.0, 2.0);
        // g0 shows alone. g1's tab is pressed, which shows it, and let go
        // at an edge of the screen: g0 is back, and g1 beside it there
        // with the keyboard.
        for (edge, panes) in [
            (Edge::Right, "h(0.5,g0,*g1)"),
            (Edge::Left, "h(0.5,*g1,g0)"),
            (Edge::Bottom, "v(0.5,g0,*g1)"),
            (Edge::Top, "v(0.5,*g1,g0)"),
        ] {
            let mut panel = two_workspaces();
            let (_, drop) = lift(&mut panel, &atlas, "g1", "g1", Zone::Edge(edge));
            let drop = drop.unwrap_or_else(|| panic!("{edge:?}: nowhere to land"));
            assert_eq!(drop.zone, Zone::Edge(edge));
            assert_eq!(
                drop.rect,
                zone_rect(wide_screen(), PANE_GAP, Zone::Edge(edge)),
                "{edge:?}: half the screen lights up"
            );
            assert_eq!(panes_of(&panel), "-", "{edge:?}: nothing moves while held");
            let mut panel = two_workspaces();
            assert!(drop_on(&mut panel, &atlas, "g1", "g1", Zone::Edge(edge)));
            assert_eq!(panes_of(&panel), panes, "{edge:?}");
            assert_eq!(front(&panel), "g1", "{edge:?}: it has the keyboard");
            assert_eq!(titles(&panel), ["g0", "s0", "g1", "g2", "s1", "g3"]);
        }
        // In the middle it shows there, which the press already did.
        let mut panel = two_workspaces();
        assert!(!drop_on(&mut panel, &atlas, "g1", "g1", CENTER));
        assert_eq!((panes_of(&panel).as_str(), front(&panel)), ("-", "g1"));
        // The session showing, dragged over its own screen: nothing.
        for zone in [CENTER, RIGHT, TOP] {
            let mut panel = two_workspaces();
            assert!(!drop_on(&mut panel, &atlas, "g0", "g0", zone));
            assert_eq!((panes_of(&panel).as_str(), front(&panel)), ("-", "g0"));
        }
        // Another workspace's panes are no part of it.
        let mut panel = two_workspaces();
        assert!(drop_on(&mut panel, &atlas, "g1", "g1", RIGHT));
        list(&mut panel, "sheds");
        panel.show(at(&panel, "s0"));
        assert!(drop_on(&mut panel, &atlas, "s1", "s1", BOTTOM));
        assert_eq!(panes_of(&panel), "v(0.5,s0,*s1)");
        list(&mut panel, "garden");
        panel.show(at(&panel, "g0"));
        assert_eq!(panes_of(&panel), "h(0.5,*g0,g1)");
    }

    #[test]
    fn a_tab_of_the_strip_dropped_on_a_pane_is_beside_it_or_in_its_place() {
        let atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let three = || {
            let mut panel = two_workspaces();
            assert!(panel.split(Edge::Right, at(&panel, "g1")));
            assert!(panel.split(Edge::Bottom, at(&panel, "g2")));
            assert_eq!(panes_of(&panel), "h(0.5,g0,v(0.5,g1,*g2))");
            panel
        };
        // g3 is in no pane. Its press shows it where g2 was; the drop
        // puts g2 back and g3 beside the pane it was let go on.
        let mut panel = three();
        assert!(drop_on(&mut panel, &atlas, "g3", "g0", BOTTOM));
        assert_eq!(panes_of(&panel), "h(0.5,v(0.5,g0,*g3),v(0.5,g1,g2))");
        assert_eq!(front(&panel), "g3");
        let mut panel = three();
        assert!(drop_on(&mut panel, &atlas, "g3", "g1", LEFT));
        assert_eq!(panes_of(&panel), "h(0.5,g0,v(0.5,h(0.5,*g3,g1),g2))");
        // On the pane the press showed it in, that is beside g2.
        let mut panel = three();
        assert!(drop_on(&mut panel, &atlas, "g3", "g3", RIGHT));
        assert_eq!(panes_of(&panel), "h(0.5,g0,v(0.5,g1,h(0.5,g2,*g3)))");
        let mut panel = three();
        assert!(drop_on(&mut panel, &atlas, "g3", "g3", TOP));
        assert_eq!(panes_of(&panel), "h(0.5,g0,v(0.5,g1,v(0.5,*g3,g2)))");
        // In the middle of another pane it shows in that one's place:
        // g2 has its pane back and g0 is a tab of the strip alone.
        let mut panel = three();
        assert!(drop_on(&mut panel, &atlas, "g3", "g0", CENTER));
        assert_eq!(panes_of(&panel), "h(0.5,*g3,v(0.5,g1,g2))");
        assert_eq!(panel.visible(true), [5, 2, 3]);
        // In the middle of the one it shows in: as the press left it.
        let mut panel = three();
        assert!(!drop_on(&mut panel, &atlas, "g3", "g3", CENTER));
        assert_eq!(panes_of(&panel), "h(0.5,g0,v(0.5,g1,*g3))");
        // The dividers keep their places round what was not cut.
        let mut panel = three();
        let bar = panel.dividers(wide_screen())[0];
        panel.grab_divider(&bar, bar.rect.x + 8.0, bar.rect.y + 8.0);
        assert!(panel.drag_divider(&atlas, wide_screen(), 300.0, 0.0));
        panel.divider_drag = None;
        let ratio = panes_of(&panel);
        let ratio = ratio[2..].split(',').next().expect("a ratio").to_owned();
        assert_ne!(ratio, "0.5");
        assert!(drop_on(&mut panel, &atlas, "g3", "g1", RIGHT));
        assert_eq!(
            panes_of(&panel),
            format!("h({ratio},g0,v(0.5,h(0.5,g1,*g3),g2))")
        );
    }

    #[test]
    fn a_pane_dragged_by_its_tab_moves_and_stays_put_on_itself() {
        let atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let three = || {
            let mut panel = two_workspaces();
            assert!(panel.split(Edge::Right, at(&panel, "g1")));
            assert!(panel.split(Edge::Bottom, at(&panel, "g2")));
            panel
        };
        // g2 to the left of g0: g1 has all of the right side.
        let mut panel = three();
        assert!(drop_on(&mut panel, &atlas, "g2", "g0", LEFT));
        assert_eq!(panes_of(&panel), "h(0.5,h(0.5,*g2,g0),g1)");
        assert_eq!(front(&panel), "g2");
        // g0, which did not have the keyboard, under g2: it has it there.
        let mut panel = three();
        assert!(drop_on(&mut panel, &atlas, "g0", "g2", BOTTOM));
        assert_eq!(panes_of(&panel), "v(0.5,g1,v(0.5,g2,*g0))");
        assert_eq!(front(&panel), "g0");
        // To the other side of the pane beside it: the two trade places.
        let mut panel = three();
        assert!(drop_on(&mut panel, &atlas, "g1", "g2", BOTTOM));
        assert_eq!(panes_of(&panel), "h(0.5,g0,v(0.5,g2,*g1))");
        // In the middle of another pane: it shows there, the pane it
        // left closes, and the session it replaced is a tab alone.
        let mut panel = three();
        assert!(drop_on(&mut panel, &atlas, "g2", "g0", CENTER));
        assert_eq!(panes_of(&panel), "h(0.5,*g2,g1)");
        let mut panel = two_workspaces();
        assert!(panel.split(Edge::Right, at(&panel, "g1")));
        assert!(drop_on(&mut panel, &atlas, "g0", "g1", CENTER));
        assert_eq!((panes_of(&panel).as_str(), front(&panel)), ("-", "g0"));
        assert!(panel.layouts.is_empty(), "one pane is no layout");

        // On its own pane, wherever: nothing, and nothing lights up.
        // The press alone gives it the keyboard, as a click does.
        for zone in [CENTER, LEFT, RIGHT, TOP, BOTTOM] {
            let mut panel = three();
            let before = panel.layouts[0].tree.clone();
            assert!(!drop_on(&mut panel, &atlas, "g1", "g1", zone), "{zone:?}");
            assert_eq!(panel.layouts[0].tree, before, "{zone:?}");
            assert_eq!(front(&panel), "g1");
        }
        // To where it already is, over g2 or beside g0: nothing, and the
        // dividers stay where they were put.
        let mut panel = three();
        let bar = panel.dividers(wide_screen())[1];
        panel.grab_divider(&bar, bar.rect.x + 8.0, bar.rect.y + 8.0);
        assert!(panel.drag_divider(&atlas, wide_screen(), 0.0, 200.0));
        panel.divider_drag = None;
        let before = panel.layouts[0].tree.clone();
        assert!(!drop_on(&mut panel, &atlas, "g1", "g2", TOP));
        assert!(!drop_on(&mut panel, &atlas, "g2", "g1", BOTTOM));
        assert_eq!(panel.layouts[0].tree, before);
        let mut panel = two_workspaces();
        assert!(panel.split(Edge::Right, at(&panel, "g1")));
        assert!(!drop_on(&mut panel, &atlas, "g0", "g1", LEFT));
        assert!(!drop_on(&mut panel, &atlas, "g1", "g0", RIGHT));
        assert_eq!(panes_of(&panel), "h(0.5,g0,*g1)");
    }

    #[test]
    fn a_drop_is_refused_where_a_pane_would_be_too_small_or_off_the_panel() {
        let atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let (header, _) = split(drag_rect());
        let advance = atlas.metrics.advance;
        let row = row_height(&atlas);
        // Room for three panes across and not four, for two rows of them
        // and not three.
        let screen = Viewport {
            width: PANE_MIN_COLS as f32 * advance * 3.5,
            height: PANE_MIN_ROWS as f32 * row * 2.5,
            ..wide_screen()
        };
        let mut panel = two_workspaces();
        assert!(panel.split(Edge::Right, at(&panel, "g1")));
        let pane = |panel: &Panel, title: &str| {
            let tab = at(panel, title);
            let (_, pane) = panel
                .screens(screen)
                .into_iter()
                .find(|(index, _)| *index == tab)
                .expect("a pane");
            pane
        };
        let hold = |panel: &mut Panel, x: f32, y: f32| {
            assert!(panel.drag_tab(&atlas, header, screen, true, x, y));
            panel.tab_drag.and_then(|drag| drag.drop)
        };
        // g2 from the strip, shown where g1 was by its press. Beside
        // either pane there is no room for it; stacked there is; and in
        // the middle of g0 it takes that pane as it is.
        panel.press_tab(at(&panel, "g2"), 60.0, 15.0);
        let left = pane(&panel, "g0");
        let mid = left.y + left.height * 0.5;
        assert_eq!(hold(&mut panel, left.x + 4.0, mid), None, "too narrow");
        assert_eq!(panel.drop_report(), "-");
        let low = hold(
            &mut panel,
            left.x + left.width * 0.5,
            left.y + left.height - 4.0,
        );
        assert_eq!(low.map(|drop| drop.zone), Some(BOTTOM));
        let middle = hold(&mut panel, left.x + left.width * 0.5, mid);
        assert_eq!(
            middle.map(|drop| (drop.zone, drop.rect)),
            Some((CENTER, left))
        );
        // Let go where it is refused: the panes are as the press left
        // them, and the drag is over.
        let before = panel.layouts.clone();
        assert!(!panel.drop_pane(&atlas, screen, left.x + 4.0, mid));
        assert_eq!(panel.layouts, before);
        assert_eq!(panel.tab_drag, None);
        check_panes(&panel, "refused");

        // The gaps and the padding are the pane's beside them; past the
        // panel's edges, and on the strip, there is nowhere to land.
        let screen = wide_screen();
        let mut panel = two_workspaces();
        assert!(panel.split(Edge::Right, at(&panel, "g1")));
        panel.press_tab(at(&panel, "g2"), 60.0, 15.0);
        let hold = |panel: &mut Panel, x: f32, y: f32| {
            assert!(panel.drag_tab(&atlas, header, screen, true, x, y));
            panel.tab_drag.and_then(|drag| drag.drop)
        };
        let in_gap = hold(&mut panel, 503.0, 300.0).expect("g0's side of the gap");
        assert_eq!(panel.index_of(in_gap.pane), Some(at(&panel, "g0")));
        assert_eq!(in_gap.zone, RIGHT);
        let in_pad = hold(&mut panel, 2.0, 636.0).expect("the padding by g0");
        assert_eq!(panel.index_of(in_pad.pane), Some(at(&panel, "g0")));
        assert_eq!(hold(&mut panel, 1017.0, 300.0), None, "past the right edge");
        assert_eq!(hold(&mut panel, 500.0, 639.0), None, "under the panel");
        assert_eq!(hold(&mut panel, -1.0, 300.0), None);
        let before = (panel.layouts.clone(), panel.active);
        assert!(
            !panel.drop_pane(&atlas, screen, 500.0, 639.0),
            "let go outside"
        );
        assert_eq!((panel.layouts.clone(), panel.active), before);
        assert_eq!(panel.tab_drag, None);

        // Given up with Escape, which is the drag forgotten: as a click.
        let mut panel = two_workspaces();
        assert!(panel.split(Edge::Right, at(&panel, "g1")));
        let (_, drop) = lift(&mut panel, &atlas, "g2", "g0", BOTTOM);
        assert!(drop.is_some());
        panel.tab_drag = None;
        assert_eq!(panes_of(&panel), "h(0.5,g0,*g2)");
        assert_eq!(panel.drop_report(), "-");
        check_panes(&panel, "given up");

        // The session the press took the pane from closes while the tab
        // is held: the drag ends with it, as any does.
        let mut panel = two_workspaces();
        assert!(panel.split(Edge::Right, at(&panel, "g1")));
        lift(&mut panel, &atlas, "g2", "g0", BOTTOM);
        panel.close_tab(at(&panel, "g1"));
        assert_eq!(panel.tab_drag, None);
        check_panes(&panel, "closed while held");
    }

    #[test]
    fn a_tab_held_tells_no_session_a_size_until_it_is_let_go() {
        let atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let (header, screen) = split(drag_rect());
        let whole = grid_size(&atlas, screen);
        let mut panel = panel_of(&["g0", "g1", "g2", "g3", "s0"]);
        regroup(&mut panel);
        panel.fit(&atlas, screen);
        assert_eq!(resizes(&panel), [1, 1, 1, 1, 1]);
        let frames = |panel: &Panel| {
            for _ in 0..3 {
                panel.fit(&atlas, screen);
            }
            resizes(panel)
        };

        // g1 onto the right of g0's screen: nobody hears while it is
        // held, over the strip or the screen; let go, the two panes do.
        panel.press_tab(1, 60.0, 15.0);
        assert_eq!(frames(&panel), [1, 1, 1, 1, 1], "pressed");
        for step in 0..30 {
            let (x, y) = (60.0 + step as f32 * 28.0, 15.0 + step as f32 * 10.0);
            assert_eq!(panel.drag_tab(&atlas, header, screen, true, x, y), step > 0);
            assert_eq!(frames(&panel), [1, 1, 1, 1, 1], "step {step}");
        }
        assert_eq!(panel.drop_report(), "g1:right");
        assert!(panel.drop_pane(&atlas, screen, 872.0, 305.0));
        assert_eq!(panes_of(&panel), "h(0.5,g0,*g1)");
        assert_eq!(frames(&panel), [2, 2, 1, 1, 1]);
        let panes = panel.screens(screen);
        assert_eq!(grids(&panel)[0], grid_size(&atlas, panes[0].1));
        assert_eq!(grids(&panel)[1], grid_size(&atlas, panes[1].1));

        // g2 from the strip to under g1. Its press shows it in g1's
        // pane, which neither is told; let go, g1 is back there at the
        // height it has now, g2 under it, and g0 hears nothing.
        let ((x, y), drop) = lift(&mut panel, &atlas, "g2", "g2", BOTTOM);
        assert!(drop.is_some());
        assert_eq!(panes_of(&panel), "h(0.5,g0,*g2)");
        assert_eq!(frames(&panel), [2, 2, 1, 1, 1], "held");
        assert!(panel.drop_pane(&atlas, screen, x, y));
        assert_eq!(panes_of(&panel), "h(0.5,g0,v(0.5,g1,*g2))");
        assert_eq!(frames(&panel), [2, 3, 2, 1, 1]);

        // g2 moved to the left of g0: every pane's room changed.
        assert!(drop_on(&mut panel, &atlas, "g2", "g0", LEFT));
        assert_eq!(panes_of(&panel), "h(0.5,h(0.5,*g2,g0),g1)");
        assert_eq!(frames(&panel), [3, 4, 3, 1, 1]);

        // A drop that changes nothing tells nobody anything, and neither
        // does a drag given up where its press changed nothing.
        assert!(!drop_on(&mut panel, &atlas, "g1", "g1", CENTER));
        assert_eq!(frames(&panel), [3, 4, 3, 1, 1]);
        lift(&mut panel, &atlas, "g0", "g1", TOP);
        panel.tab_drag = None;
        assert_eq!(frames(&panel), [3, 4, 3, 1, 1]);

        // A click on g3's tab, no drag: it shows where g0 was, and the
        // two that changed places hear once the button is up.
        panel.press_tab(3, 60.0, 15.0);
        assert_eq!(panes_of(&panel), "h(0.5,h(0.5,g2,*g3),g1)");
        assert_eq!(frames(&panel), [3, 4, 3, 1, 1], "pressed");
        panel.tab_drag = None;
        assert_eq!(frames(&panel), [4, 4, 3, 2, 1]);
        assert_eq!(grids(&panel)[0], whole);
    }

    #[test]
    fn a_tab_over_a_pane_lights_its_zone_and_goes_with_the_pointer() {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let theme = Theme::default();
        let rect = drag_rect();
        let (header, screen) = split(rect);
        let solid = atlas.solid_uv();
        let wash = layout::faded(theme.accent, 0.14);
        let mut panel = two_workspaces();
        panel.focus = true;
        for tab in &panel.tabs {
            tab.session.feed(b"beans and peas");
        }
        assert!(panel.split(Edge::Right, at(&panel, "g1")));
        let frame = |panel: &Panel, atlas: &mut Atlas| {
            layout::take_cut_labels();
            let mut out = Vec::new();
            draw(panel, None, atlas, rect, &theme, &mut out);
            let faults = layout::frame_faults(&out, rect);
            assert!(faults.is_empty(), "{faults:?}");
            out
        };
        let washes = |out: &[GlyphInstance]| -> Vec<Viewport> {
            out.iter()
                .filter(|q| q.uv == solid && q.color == wash)
                .map(|q| Viewport {
                    x: q.pos[0],
                    y: q.pos[1],
                    width: q.size[0],
                    height: q.size[1],
                })
                .collect()
        };
        // The floating panel a name is on: the only rounded quad in the
        // palette's border colour.
        let chips = |out: &[GlyphInstance]| -> Vec<Viewport> {
            out.iter()
                .filter(|q| q.flags == crate::render::metal::ROUNDED)
                .filter(|q| q.color == theme.palette_border)
                .map(|q| Viewport {
                    x: q.pos[0],
                    y: q.pos[1],
                    width: q.size[0],
                    height: q.size[1],
                })
                .collect()
        };
        let plain = frame(&panel, &mut atlas);
        assert!(washes(&plain).is_empty() && chips(&plain).is_empty());

        // Held over the strip: the line between two tabs, and nothing
        // over the screens.
        panel.press_tab(at(&panel, "g2"), 60.0, 15.0);
        assert!(panel.drag_tab(&atlas, header, screen, true, 200.0, 15.0));
        let along = frame(&panel, &mut atlas);
        assert!(washes(&along).is_empty() && chips(&along).is_empty());
        assert!(
            along
                .iter()
                .any(|q| q.size == [2.0, HEADER - 8.0] && q.color == theme.accent)
        );

        // Under g0: its lower half lit, a little into the padding and
        // short of the strip and the line between the panes, with its
        // outline; and the tab's name by the pointer, over the wash.
        let (x, y) = zone_point(&panel, "g0", BOTTOM);
        assert!(panel.drag_tab(&atlas, header, screen, true, x, y));
        let drop = panel.tab_drag.and_then(|drag| drag.drop).expect("a drop");
        let over = frame(&panel, &mut atlas);
        let lit = washes(&over);
        assert_eq!(lit.len(), 1);
        assert_eq!(
            lit[0],
            Viewport {
                x: drop.rect.x - 3.0,
                y: drop.rect.y - 3.0,
                width: drop.rect.width + 6.0,
                height: drop.rect.height + 6.0,
            }
        );
        let bar = panel.dividers(screen)[0].rect;
        assert!(lit[0].x + lit[0].width < bar.x + bar.width * 0.5);
        assert!(lit[0].y + lit[0].height <= rect.y + rect.height);
        let outline = layout::faded(theme.accent, 0.75);
        let edges = over
            .iter()
            .filter(|q| q.uv == solid && q.color == outline)
            .count();
        assert_eq!(edges, 4);
        assert!(
            !over
                .iter()
                .any(|q| q.size == [2.0, HEADER - 8.0] && q.color == theme.accent),
            "no line in the strip"
        );
        let chip = chips(&over);
        assert_eq!(chip.len(), 1);
        assert!(chip[0].x > x && chip[0].y > y, "under and right of {x},{y}");
        assert!(chip[0].width < 80.0, "as wide as a short name: {chip:?}");
        let wash_at = over.iter().position(|q| q.color == wash).expect("the wash");
        let chip_at = over
            .iter()
            .position(|q| q.color == theme.palette_border)
            .expect("the chip");
        assert!(wash_at < chip_at, "the name is over what lights up");
        let cells = over
            .iter()
            .position(|q| q.uv != solid && q.pos[1] >= screen.y)
            .unwrap_or(0);
        assert!(cells < wash_at, "and that over the screens");
        // Held there, and moved about the zone: the frames that follow
        // shape no label again.
        let shaped = atlas.ui_shapes();
        for step in 0..5 {
            let x = x + step as f32 * 7.0;
            assert!(panel.drag_tab(&atlas, header, screen, true, x, y));
            assert_eq!(washes(&frame(&panel, &mut atlas)), lit, "step {step}");
        }
        assert_eq!(atlas.ui_shapes(), shaped, "a held tab shapes nothing");

        // In the corner of the panel the name stays inside it.
        assert!(panel.drag_tab(&atlas, header, screen, true, 1012.0, 634.0));
        let corner = chips(&frame(&panel, &mut atlas));
        assert_eq!(corner.len(), 1);
        assert!(corner[0].x + corner[0].width <= rect.x + rect.width);
        assert!(corner[0].y + corner[0].height <= rect.y + rect.height);
        // Where it cannot land, its name alone.
        let (x, y) = zone_point(&panel, "g2", CENTER);
        assert!(panel.drag_tab(&atlas, header, screen, true, x, y));
        let own = frame(&panel, &mut atlas);
        assert!(washes(&own).is_empty());
        assert_eq!(chips(&own).len(), 1);
    }
}
