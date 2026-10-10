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
    /// Its folder, resolved, for the task it belongs to.
    pub folder: PathBuf,
    /// Held while the tab lives: the thread that watches its agent's hook
    /// events stops when this goes.
    pub alive: std::sync::Arc<()>,
    /// The `crc --hold` helper running it, when one does (see
    /// [`crate::term::hold`]): the program outlives the window.
    pub held: Option<String>,
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
    pub fn activity(&self) -> Activity {
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
    /// The tabs the header lists, when not all: in Agents mode, the
    /// selected task's.
    pub shown: Option<Vec<usize>>,
    /// Two sessions side by side, left and right, in Agents mode. The
    /// active one is the pane with the keyboard.
    pub pair: Option<(usize, usize)>,
}

impl Default for Panel {
    fn default() -> Self {
        Panel {
            open: false,
            focus: false,
            height: DEFAULT_HEIGHT,
            tabs: Vec::new(),
            active: 0,
            back: 0,
            selection: None,
            selecting: false,
            shown: None,
            pair: None,
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
        if index < self.tabs.len() {
            // Dropping the session hangs up on its program.
            self.tabs.remove(index);
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
    match tab.review_files {
        0 => title_of(tab),
        n => format!("{} \u{b7} {n} to review", title_of(tab)),
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

/// The panel: header, then the active session's screen.
pub fn draw(
    panel: &Panel,
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
    for (index, screen) in screens {
        draw_screen(panel, index, screen, atlas, theme, background, out);
    }
}

/// One session's screen: its cells, the selection and the cursor when it
/// is the active one, and how far back it is scrolled.
fn draw_screen(
    panel: &Panel,
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
            let line = (1.0 / m.scale).max(1.0);
            if decoration & UNDERLINE != 0 {
                layout::push_rect(out, atlas, [x, y + row_h - line * 2.0], [width, line], fg);
            }
            if decoration & STRIKE != 0 {
                layout::push_rect(out, atlas, [x, y + row_h * 0.5], [width, line], fg);
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
