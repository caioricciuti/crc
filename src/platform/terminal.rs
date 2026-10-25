//! The terminal panel: sessions under the editor, Claude Code among them.
//!
//! The emulator and the pseudo-terminal are in [`crate::term`]; this is the
//! panel the window shows. A header of session tabs, then the screen of the
//! active one, drawn cell by cell through the same glyph pipeline as the
//! editor. Rows are as tall as a glyph cell rather than the editor's looser
//! lines, so box drawing joins up: `claude` draws its input in a box.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::project::icons;
use crate::render::font::Atlas;
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

    /// Whether a bell rung at `now` asks for attention. One right after a
    /// keystroke is the program answering the key (a shell given a key it
    /// has nothing bound to, a completion with nothing to offer, a
    /// backspace at an empty line), not a call for you: it marks nothing.
    pub fn heeds_bell(&self, now: Instant) -> bool {
        !self
            .last_typed
            .is_some_and(|at| now.saturating_duration_since(at) < BELL_ECHO)
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
    pub active: usize,
    /// Lines scrolled back into history; 0 follows the output.
    pub back: usize,
    /// Selected text of the active tab, from the press to the pointer:
    /// line numbers (see [`crate::term::Term::view_line`]) and column
    /// boundaries.
    pub selection: Option<((u64, usize), (u64, usize))>,
    /// A press in the screen is being dragged.
    pub selecting: bool,
    /// Where the drag last was, for scrolling the history under a pointer
    /// held past the screen's edge.
    pub drag_point: Option<(f32, f32)>,
    /// The tabs the header lists, when not all: in Agents mode, the
    /// selected task's.
    pub shown: Option<Vec<usize>>,
    /// Two sessions side by side, left and right, in Agents mode. The
    /// active one is the pane with the keyboard.
    pub pair: Option<(usize, usize)>,
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
            back: 0,
            selection: None,
            selecting: false,
            drag_point: None,
            shown: None,
            pair: None,
            pace: Pace::default(),
            mouse_press: None,
            mouse_cell: None,
            link: None,
            opened: Vec::new(),
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

    /// The pair, while both its tabs exist and one of them is active.
    pub fn shown_pair(&self) -> Option<(usize, usize)> {
        self.pair.filter(|(l, r)| {
            l != r
                && *l < self.tabs.len()
                && *r < self.tabs.len()
                && (self.active == *l || self.active == *r)
        })
    }

    /// Each session on screen and where its screen is: the active one, or
    /// both of a pair.
    pub fn screens(&self, screen: Viewport) -> Vec<(usize, Viewport)> {
        match self.shown_pair() {
            Some((left, right)) => {
                let (a, b) = halves(screen);
                vec![(left, a), (right, b)]
            }
            None => vec![(self.active, screen)],
        }
    }

    /// Shows `tab` with the keyboard: in a pair, the pane it is in, or in
    /// place of the session in the pane that has the keyboard.
    pub fn show(&mut self, tab: usize) {
        if tab >= self.tabs.len() {
            return;
        }
        if let Some((left, right)) = self.shown_pair()
            && tab != left
            && tab != right
        {
            self.pair = Some(if self.active == left {
                (tab, right)
            } else {
                (left, tab)
            });
        }
        self.active = tab;
        self.back = 0;
        self.selection = None;
    }

    /// The tabs whose screens are drawn: none while the panel is not
    /// `on_screen`, else the active one, or both of a pair.
    pub fn visible(&self, on_screen: bool) -> Vec<usize> {
        if !on_screen || self.tabs.is_empty() {
            return Vec::new();
        }
        match self.shown_pair() {
            Some((left, right)) => vec![left, right],
            None => vec![self.active],
        }
    }

    /// The other pane of the pair gets the keyboard.
    pub fn focus_other(&mut self) {
        if let Some((left, right)) = self.shown_pair() {
            self.show(if self.active == left { right } else { left });
        }
    }

    /// The session whose screen is under the point, in a pair.
    pub fn screen_at(&self, screen: Viewport, x: f32) -> usize {
        self.screens(screen)
            .into_iter()
            .find(|(_, s)| x < s.x + s.width + PAIR_GAP * 0.5)
            .map_or(self.active, |(tab, _)| tab)
    }

    pub fn close_tab(&mut self, index: usize) {
        // Indices after it move down; a pair is not worth renumbering.
        self.pair = None;
        self.mouse_press = None;
        self.mouse_cell = None;
        if index < self.tabs.len() {
            // Dropping the session hangs up on its program.
            let tab = self.tabs.remove(index);
            // Its agent review session goes with it: what its agent changed
            // is for the Git panel now. A tab left to a helper at quit is
            // detached, not closed, and keeps its session.
            if !tab.review.as_os_str().is_empty() {
                crate::project::review::remove(&tab.review);
            }
            if index == self.active {
                self.closed_active = Some((tab.workspace.clone(), tab.folder.clone(), index));
            } else if index < self.active {
                // The one showing slid down a place: it stays the one.
                self.active -= 1;
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
        self.back = 0;
        self.selection = None;
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

/// Find in the terminal: the active session's matches and the one the bar
/// is on. Searched again when the output, the query, the options or the
/// session change; see [`Find::stale`].
#[derive(Default)]
pub struct Find {
    /// The session (its terminal's address), its generation, the query
    /// and the options the matches are for.
    pub made_for: Option<(usize, u64, String, crate::platform::search::Options)>,
    pub matches: Vec<Hit>,
    pub current: Option<usize>,
    /// Where the current match starts, so it stays current while output
    /// arrives and the list is made again.
    pub anchor: Option<(u64, usize)>,
    /// An invalid pattern, in the search's words.
    pub error: Option<String>,
    pub searched: Option<std::time::Instant>,
}

impl Find {
    /// Takes the matches of a new search. The current one stays on its
    /// anchor, or moves to the next after it, or the last before it;
    /// with no anchor it is the last, the newest output.
    pub fn take(&mut self, matches: Vec<Hit>) {
        self.matches = matches;
        self.error = None;
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
}

/// The matches of `query` in a session's text, at most [`FIND_LIMIT`].
pub fn find_in(
    hay: &term::Haystack,
    query: &str,
    options: crate::platform::search::Options,
) -> Result<Vec<Hit>, String> {
    let found = crate::platform::search::find_first(&hay.text, query, "", options, FIND_LIMIT)?;
    Ok(found
        .into_iter()
        .filter_map(|m| {
            let spans = hay.spans(m.range);
            let (line, from, _) = *spans.first()?;
            Some(Hit {
                start: (line, from),
                spans,
            })
        })
        .collect())
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
    let chars: Vec<char> = text.chars().collect();
    let index = text[..at.min(text.len())].chars().count();
    let from = index
        .saturating_sub(SNIPPET_BEFORE)
        .min(chars.len().saturating_sub(SNIPPET));
    let to = (from + SNIPPET).min(chars.len());
    let shown: String = chars[from..to].iter().collect();
    let mut out = String::new();
    if from > 0 {
        out.push('\u{2026}');
    }
    out.push_str(shown.trim());
    if to < chars.len() {
        out.push('\u{2026}');
    }
    out
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
    let pane = panel
        .screens(screen)
        .into_iter()
        .find(|(tab, _)| *tab == panel.active)
        .map_or(screen, |(_, s)| s);
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

/// Between the two screens of a pair.
const PAIR_GAP: f32 = PAD * 2.0 + 1.0;

/// A screen cut in two, side by side, each on a whole point.
pub fn halves(screen: Viewport) -> (Viewport, Viewport) {
    let half = ((screen.width - PAIR_GAP) / 2.0).floor().max(0.0);
    (
        Viewport {
            width: half,
            ..screen
        },
        Viewport {
            x: screen.x + half + PAIR_GAP,
            width: (screen.width - half - PAIR_GAP).max(0.0),
            ..screen
        },
    )
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

/// Each tab and its close button, then the new-shell button.
pub fn header_hits(
    panel: &Panel,
    atlas: &mut Atlas,
    header: Viewport,
) -> (Vec<(Viewport, Viewport)>, Viewport) {
    let mut x = header.x;
    let mut tabs = Vec::new();
    for (index, tab) in panel.tabs.iter().enumerate() {
        if panel
            .shown
            .as_ref()
            .is_some_and(|shown| !shown.contains(&index))
        {
            tabs.push((
                Viewport {
                    width: 0.0,
                    ..header
                },
                Viewport {
                    width: 0.0,
                    ..header
                },
            ));
            continue;
        }
        // The editor tab's metrics: its padding, its close slot and the
        // square around the close glyph, so the two strips are one.
        let dot = if tab.attention.is_some() { 5.0 } else { 0.0 };
        let width = layout::ui_text_width(atlas, &label(tab))
            + layout::TAB_PADDING_X * 2.0
            + layout::TAB_CLOSE_SLOT
            + dot;
        let rect = Viewport { x, width, ..header };
        let centre = rect.x + rect.width - layout::TAB_PADDING_X - atlas.metrics.advance * 0.5;
        let close = Viewport {
            x: centre - layout::TAB_CLOSE_HIT * 0.5,
            y: header.y + ((header.height - layout::TAB_CLOSE_HIT) * 0.5).floor(),
            width: layout::TAB_CLOSE_HIT,
            height: layout::TAB_CLOSE_HIT,
        };
        tabs.push((rect, close));
        x += width;
    }
    let new = Viewport {
        x: x + layout::UI_GAP,
        y: header.y + ((header.height - layout::UI_CONTROL_SM) * 0.5).floor(),
        width: layout::UI_CONTROL_SM,
        height: layout::UI_CONTROL_SM,
    };
    (tabs, new)
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
    // Inside ssh the tab says where.
    let title = match &tab.remote {
        Some(remote) => format!("ssh {}", remote.destination),
        None => title_of(tab),
    };
    match tab.review_files {
        0 => title,
        n => format!("{title} \u{b7} {n} to review"),
    }
}

fn title_of(tab: &Tab) -> String {
    let term = tab.session.term.lock().unwrap_or_else(|e| e.into_inner());
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

    let (tabs, new) = header_hits(panel, atlas, header);
    for (index, (tab, (rect, close))) in panel.tabs.iter().zip(tabs).enumerate() {
        if rect.width <= 0.0 {
            continue;
        }
        // The same tab as the editor's: the strip's height, the page's
        // fill under the active one with the accent rule where it meets
        // the screen, a lift under the pointer, a hairline between the rest.
        let active = index == panel.active;
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
                [rect.x + layout::TAB_PADDING_X, rect.y + rect.height - 2.0],
                [rect.width - layout::TAB_PADDING_X * 2.0, 2.0],
                theme.accent,
            );
        } else if layout::hovered(rect) {
            layout::push_rect(
                out,
                atlas,
                [rect.x, rect.y],
                [rect.width, rect.height],
                theme.row_hover,
            );
        } else if index > 0 && index != panel.active + 1 {
            layout::push_rect(
                out,
                atlas,
                [rect.x, rect.y + 6.0],
                [1.0, rect.height - 12.0],
                theme.hairline,
            );
        }
        layout::hotspot(rect, layout::Cursor::Pointing, None);
        let mut label_x = rect.x + layout::TAB_PADDING_X;
        // A tab waiting on the person: a dot before its title.
        if tab.attention.is_some() {
            layout::push_rounded_rect(
                out,
                Viewport {
                    x: label_x - 5.0,
                    y: rect.y + rect.height / 2.0 - 3.0,
                    width: 6.0,
                    height: 6.0,
                },
                3.0,
                theme.accent,
            );
            label_x += 5.0;
        }
        let colour = if tab.claude {
            theme.accent
        } else if active {
            theme.tab_text
        } else {
            theme.tab_text_inactive
        };
        layout::push_ui_text(
            out,
            atlas,
            Viewport {
                x: label_x,
                width: (rect.x + rect.width
                    - layout::TAB_PADDING_X
                    - layout::TAB_CLOSE_SLOT
                    - label_x)
                    .max(0.0),
                ..rect
            },
            &label(tab),
            colour,
        );
        // The close cross on the tab that is showing and the one under the
        // pointer, the way editor tabs do it.
        if active || layout::hovered(rect) {
            Button::new(close)
                .icon(icons::CLOSE)
                .tone(Tone::Ghost)
                .radius(layout::UI_RADIUS_SM)
                .tip("Close Terminal")
                .draw(out, atlas, theme);
        }
    }
    Button::new(new)
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
    if let [(_, left), (_, right)] = screens.as_slice() {
        let x = ((left.x + left.width + right.x) * 0.5).floor();
        layout::push_rect(
            out,
            atlas,
            [x, screen.y - PAD * 0.5],
            [1.0, screen.height + PAD],
            theme.hairline,
        );
        // The pane with the keyboard: a line over its screen.
        let focused = if panel.active == screens[0].0 {
            left
        } else {
            right
        };
        layout::push_rect(
            out,
            atlas,
            [focused.x, screen.y - PAD * 0.5],
            [focused.width, 2.0],
            if panel.focus {
                theme.accent
            } else {
                theme.hairline
            },
        );
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
}

/// One session's screen: its cells, the selection and the cursor when it
/// is the active one, and how far back it is scrolled.
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
    let (rows, cols, back, visible, cursor, cursor_visible, kept) = {
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
        let back = if active {
            panel.back.min(term.scrollback_len())
        } else {
            0
        };
        let visible: Vec<(Vec<crate::term::Cell>, u64)> = (0..rows)
            .map(|row| {
                (
                    term.visible_row(row, back).to_vec(),
                    term.view_line(row, back),
                )
            })
            .collect();
        (
            rows,
            term.cols(),
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
                    term.view_line(0, back),
                )
            }),
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
        if let Some((a, b)) = panel.selection.filter(|_| active) {
            let (start, end) = if a <= b { (a, b) } else { (b, a) };
            if (start.0..=end.0).contains(&line) {
                let from = if line == start.0 { start.1 } else { 0 };
                let to = if line == end.0 { end.1 } else { cols };
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
    // The cursor, where the program left it, when it is showing.
    let (row, col) = cursor;
    if back == 0 && cursor_visible && row < rows && !tab.session.has_exited() {
        let x = screen.x + col as f32 * advance;
        let y = screen.y + row as f32 * row_h;
        if panel.focus && active {
            layout::push_rect(out, atlas, [x, y], [2.0, row_h], theme.cursor);
        } else {
            let c = theme.cursor;
            layout::push_rect(out, atlas, [x, y], [1.0, row_h], [c[0], c[1], c[2], 0.5]);
        }
    }
    if back > 0 {
        let note = format!("↑ {back} lines");
        let width = layout::ui_text_width(atlas, &note) + 20.0;
        // Under the find bar while it is open, which sits where the
        // pill does.
        let below_bar = if find.is_some() && active {
            FIND_BAR_MARGIN * 2.0 + layout::FIND_ROW_HEIGHT - PAD * 0.5
        } else {
            0.0
        };
        let pill = Viewport {
            x: screen.x + screen.width - width,
            y: screen.y + below_bar,
            width,
            height: 22.0,
        };
        layout::push_rounded_rect(out, pill, 6.0, theme.tab_hover);
        layout::push_ui_text_centered(out, atlas, pill, &note, theme.status_text);
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
            last_probe: None,
            probe_owed: false,
            retry: None,
            last_return: None,
            marks: Default::default(),
            seen: None,
        }
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
        panel.pair = Some((0, 1));
        assert_eq!(panel.visible(true), vec![0, 1]);
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
}
