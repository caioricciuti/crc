//! The terminal panel: sessions under the editor, Claude Code among them.
//!
//! The emulator and the pseudo-terminal are in [`crate::term`]; this is the
//! panel the window shows. A header of session tabs, then the screen of the
//! active one, drawn cell by cell through the same glyph pipeline as the
//! editor. Rows are as tall as a glyph cell rather than the editor's looser
//! lines, so box drawing joins up: `claude` draws its input in a box.

use std::path::PathBuf;

use crate::render::font::Atlas;
use crate::render::layout::{self, Theme, Viewport};
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

    pub fn close_tab(&mut self, index: usize) {
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
    for tab in &panel.tabs {
        let width = layout::ui_text_width(atlas, &label(tab)) + 44.0;
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
        width: 26.0,
        height: HEADER - 8.0,
    };
    (tabs, new)
}

/// The title the program set, when it set one (`claude` does, and so do
/// most shell prompts), otherwise the name the tab started with.
fn label(tab: &Tab) -> String {
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
        let active = index == panel.active;
        if active {
            layout::push_rounded_rect(out, rect, 5.0, theme.tab_hover);
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
                x: rect.x + 10.0,
                width: (rect.width - 34.0).max(0.0),
                ..rect
            },
            &label(tab),
            colour,
        );
        layout::push_ui_text_centered(out, atlas, close, "×", theme.status_text);
    }
    layout::push_ui_text_centered(out, atlas, new, "+", theme.status_text);

    let Some(tab) = panel.active_tab() else {
        return;
    };
    let term = tab.session.term.lock().unwrap_or_else(|e| e.into_inner());
    let m = atlas.metrics;
    let advance = m.advance;
    let row_h = row_height(atlas);
    // push_text centres a glyph in an editor line; move it up to sit in a
    // terminal row instead.
    let text_dy = -m.glyph_dy(m.line_height) + m.glyph_dy(row_h);
    let rows = term.rows().min((screen.height / row_h).floor() as usize);
    let cols = term.cols();
    let back = panel.back.min(term.scrollback_len());
    let mut run = String::new();
    for row in 0..rows {
        let cells = term.visible_row(row, back);
        let y = screen.y + row as f32 * row_h;
        // Backgrounds, merged into runs of one colour.
        let mut col = 0;
        while col < cols.min(cells.len()) {
            let (_, bg) = colours(&cells[col], theme, background);
            let start = col;
            while col < cols.min(cells.len()) && colours(&cells[col], theme, background).1 == bg {
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
        if let Some((a, b)) = panel.selection {
            let (start, end) = if a <= b { (a, b) } else { (b, a) };
            let line = term.view_line(row, back);
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
        while col < cols.min(cells.len()) {
            let first = cells[col];
            let (fg, _) = colours(&first, theme, background);
            let decoration = first.flags & (UNDERLINE | STRIKE);
            let start = col;
            run.clear();
            while col < cols.min(cells.len()) {
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
    let (row, col) = term.cursor();
    if back == 0 && term.modes.cursor_visible && row < rows && !tab.session.has_exited() {
        let x = screen.x + col as f32 * advance;
        let y = screen.y + row as f32 * row_h;
        if panel.focus {
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
