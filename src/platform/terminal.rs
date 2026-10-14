//! The terminal panel: sessions under the editor, Claude Code among them.
//!
//! The emulator and the pseudo-terminal are in [`crate::term`]; this is the
//! panel the window shows. A header of session tabs, then the screen of the
//! active one, drawn cell by cell through the same glyph pipeline as the
//! editor. Rows are as tall as a glyph cell rather than the editor's looser
//! lines, so box drawing joins up: `claude` draws its input in a box.

use std::path::PathBuf;

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

impl Tab {
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

    /// How Home and the status line say what the tab is doing.
    pub fn state(&self) -> String {
        use crate::term::status::State;
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

    /// The title without the review count, for naming its session.
    pub fn base_name(&self) -> String {
        title_of(self)
    }
}

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
    /// A press reported to a program that asked for the mouse: its tab,
    /// the button, and the cell (row, column) last reported, which motion
    /// and the release follow wherever the pointer goes.
    pub mouse_press: Option<(usize, term::mouse::Button, (usize, usize))>,
    /// The tab and cell the pointer was last reported over, so motion is
    /// reported only when it reaches another cell.
    pub mouse_cell: Option<(usize, (usize, usize))>,
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
            mouse_press: None,
            mouse_cell: None,
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

/// The find bar over the session with the keyboard, in the panel `rect`:
/// one row across the top of its pane, over the grid rather than taking
/// room from it, which would resize the program's terminal.
pub fn find_bar_rect(panel: &Panel, rect: Viewport) -> Viewport {
    let (header, screen) = split(rect);
    let pane = panel
        .screens(screen)
        .into_iter()
        .find(|(tab, _)| *tab == panel.active)
        .map_or(screen, |(_, s)| s);
    Viewport {
        x: pane.x - PAD,
        y: header.y + header.height,
        width: pane.width + PAD * 2.0,
        height: layout::FIND_ROW_HEIGHT,
    }
}

/// The screen rows the find bar hides, at the top.
pub fn find_bar_rows(atlas: &Atlas) -> usize {
    ((layout::FIND_ROW_HEIGHT - PAD * 0.5) / row_height(atlas)).ceil() as usize
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
    let mut x = header.x + 8.0;
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
        let dot = if tab.attention.is_some() { 5.0 } else { 0.0 };
        let width = layout::ui_text_width(atlas, &label(tab)) + 44.0 + dot;
        let rect = Viewport {
            x,
            y: header.y + 4.0,
            width,
            height: HEADER - 8.0,
        };
        let close = Viewport {
            x: rect.x + rect.width - 22.0,
            width: 18.0,
            ..rect
        };
        tabs.push((rect, close));
        x += width + 4.0;
    }
    let new = Viewport {
        x,
        y: header.y + 4.0,
        width: HEADER - 8.0,
        height: HEADER - 8.0,
    };
    (tabs, new)
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
        let active = index == panel.active;
        if active {
            layout::push_rounded_rect(out, rect, layout::UI_RADIUS_SM, theme.tab_hover);
        } else if layout::hovered(rect) {
            layout::push_rounded_rect(out, rect, layout::UI_RADIUS_SM, theme.row_hover);
        }
        layout::hotspot(rect, layout::Cursor::Pointing, None);
        // A tab waiting on the person: a dot before its title.
        if tab.attention.is_some() {
            layout::push_rounded_rect(
                out,
                Viewport {
                    x: rect.x + 5.0,
                    y: rect.y + rect.height / 2.0 - 3.0,
                    width: 6.0,
                    height: 6.0,
                },
                3.0,
                theme.accent,
            );
        }
        let colour = if tab.claude {
            theme.accent
        } else if active {
            theme.text
        } else {
            theme.status_text
        };
        layout::push_ui_text(
            out,
            atlas,
            Viewport {
                x: rect.x + if tab.attention.is_some() { 15.0 } else { 10.0 },
                width: (rect.width - 34.0).max(0.0),
                ..rect
            },
            &label(tab),
            colour,
        );
        // The close cross on the tab that is showing and the one under the
        // pointer, the way editor tabs do it.
        if active || layout::hovered(rect) {
            let square = Viewport {
                y: close.y + ((close.height - 18.0) * 0.5).floor(),
                height: 18.0,
                ..close
            };
            Button::new(square)
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
        let said = "No terminal in this workspace. \u{2318}T starts one.";
        let width = layout::ui_text_width(atlas, said);
        layout::push_ui_text(
            out,
            atlas,
            Viewport {
                x: screen.x + ((screen.width - width) * 0.5).max(0.0).floor(),
                y: screen.y + (screen.height * 0.4).floor(),
                width: width.min(screen.width),
                height: 20.0,
            },
            said,
            theme.gutter_text,
        );
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
    let (rows, cols, back, visible, cursor, cursor_visible) = {
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
        // Find's matches, as the editor draws them: the current one also
        // in the selection's colour, as the editor selects it.
        for &(_, from, to, current) in found.iter().filter(|f| f.0 == line) {
            let to = to.min(cols);
            if to <= from {
                continue;
            }
            let x = screen.x + from as f32 * advance;
            let width = (to - from) as f32 * advance;
            layout::push_rect(out, atlas, [x, y], [width, row_h], theme.find_match);
            if current {
                layout::push_rect(out, atlas, [x, y], [width, row_h], theme.selection);
            }
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
        let pill = Viewport {
            x: screen.x + screen.width - width,
            y: screen.y,
            width,
            height: 22.0,
        };
        layout::push_rounded_rect(out, pill, 6.0, theme.tab_hover);
        layout::push_ui_text_centered(out, atlas, pill, &note, theme.status_text);
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

    fn hit(line: u64, col: usize) -> Hit {
        Hit {
            start: (line, col),
            spans: vec![(line, col, col + 1)],
        }
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
}
