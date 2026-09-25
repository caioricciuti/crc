//! A terminal: the screen a program running in a pseudo-terminal draws.
//!
//! Bytes from the program go through [`Term::advance`], an escape sequence
//! parser in the shape of the DEC VT500 state machine, into a grid of cells
//! with a cursor, a scroll region, an alternate screen and scrollback. What
//! the program asks back (the cursor position, which terminal this is) is
//! queued in [`Term::take_replies`] for the caller to write to the program.
//!
//! The target is what `claude`, shells and the usual full-screen tools use:
//! xterm's cursor and editing controls, SGR colours up to 24-bit, bracketed
//! paste, the alternate screen, DEC line drawing, and mode queries. Mouse
//! reporting is recorded but not generated yet. There is no reflow on
//! resize; programs redraw on SIGWINCH.

pub mod keys;
pub mod pty;

use std::collections::VecDeque;

use crate::text::columns::display_width;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    /// The 256-colour palette: 0-15 the named colours, then the cube and
    /// the greys.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

pub const BOLD: u16 = 1;
pub const DIM: u16 = 1 << 1;
pub const ITALIC: u16 = 1 << 2;
pub const UNDERLINE: u16 = 1 << 3;
pub const INVERSE: u16 = 1 << 4;
pub const HIDDEN: u16 = 1 << 5;
pub const STRIKE: u16 = 1 << 6;
/// The right half of a two-column character; drawn by the left half.
pub const WIDE_TAIL: u16 = 1 << 7;
/// On a line's last cell: the text went on to the next line by wrapping,
/// not by a newline, so copying joins the two.
pub const WRAPPED: u16 = 1 << 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: u16,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            fg: Color::Default,
            bg: Color::Default,
            flags: 0,
        }
    }
}

impl Cell {
    /// An empty cell in the current background, as erasing leaves.
    fn blank(pen: &Cell) -> Cell {
        Cell {
            bg: pen.bg,
            ..Cell::default()
        }
    }
}

/// Lines of history kept above the primary screen.
const SCROLLBACK: usize = 10_000;
/// The longest OSC string kept; the rest is dropped.
const MAX_OSC: usize = 4096;
/// Parameters beyond this many in one sequence are ignored.
const MAX_PARAMS: usize = 32;

#[derive(Clone, Copy, Debug, Default)]
struct Saved {
    row: usize,
    col: usize,
    pen: Cell,
    origin: bool,
    line_drawing: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum State {
    Ground,
    Escape,
    /// ESC followed by an intermediate byte: `(`, `)`, `#`, ` ` and so on.
    EscapeIntermediate(u8),
    Csi,
    Osc,
    /// A string to skip: DCS, SOS, PM, APC. Ends at ST or BEL.
    Ignore,
}

/// Modes a program switches with `CSI ? n h` and `l`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modes {
    pub app_cursor: bool,
    pub origin: bool,
    pub autowrap: bool,
    pub cursor_visible: bool,
    pub bracketed_paste: bool,
    pub focus_events: bool,
    pub mouse: bool,
    pub sgr_mouse: bool,
    pub insert: bool,
    pub sync: bool,
}

pub struct Term {
    cols: usize,
    rows: usize,
    screen: Vec<Vec<Cell>>,
    /// The other screen: the primary while the alternate is showing.
    other: Vec<Vec<Cell>>,
    alternate: bool,
    scrollback: VecDeque<Vec<Cell>>,
    /// Lines ever dropped from the front of history. Lines are numbered
    /// from the first one kept, so a selection stays on its text while
    /// output pushes it up.
    dropped: u64,
    row: usize,
    col: usize,
    /// The cursor sits past the last column; the next character wraps.
    pending_wrap: bool,
    pen: Cell,
    saved: Saved,
    saved_alternate: Saved,
    top: usize,
    bottom: usize,
    tabs: Vec<bool>,
    pub modes: Modes,
    /// G0 is DEC Special Graphics: `q` draws a line.
    line_drawing: bool,
    pub title: String,
    replies: Vec<u8>,
    state: State,
    params: Vec<Vec<u16>>,
    private: Option<u8>,
    intermediate: Option<u8>,
    osc: Vec<u8>,
    /// ESC seen inside an OSC or ignored string, which may be ST.
    string_escape: bool,
    /// Bytes of a UTF-8 character split across reads.
    utf8: Vec<u8>,
    /// Bumped by every change, so a drawer can tell a frame is stale.
    pub generation: u64,
}

impl Term {
    pub fn new(cols: usize, rows: usize) -> Term {
        let (cols, rows) = (cols.max(2), rows.max(1));
        let mut term = Term {
            cols,
            rows,
            screen: vec![vec![Cell::default(); cols]; rows],
            other: vec![vec![Cell::default(); cols]; rows],
            alternate: false,
            scrollback: VecDeque::new(),
            dropped: 0,
            row: 0,
            col: 0,
            pending_wrap: false,
            pen: Cell::default(),
            saved: Saved::default(),
            saved_alternate: Saved::default(),
            top: 0,
            bottom: rows - 1,
            tabs: Vec::new(),
            modes: Modes {
                autowrap: true,
                cursor_visible: true,
                ..Modes::default()
            },
            line_drawing: false,
            title: String::new(),
            replies: Vec::new(),
            state: State::Ground,
            params: Vec::new(),
            private: None,
            intermediate: None,
            osc: Vec::new(),
            string_escape: false,
            utf8: Vec::new(),
            generation: 0,
        };
        term.reset_tabs();
        term
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Where the cursor is, row then column.
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    pub fn is_alternate(&self) -> bool {
        self.alternate
    }

    pub fn scrollback_len(&self) -> usize {
        if self.alternate {
            0
        } else {
            self.scrollback.len()
        }
    }

    /// Row `index` of the view scrolled `back` lines into history.
    pub fn visible_row(&self, index: usize, back: usize) -> &[Cell] {
        let back = back.min(self.scrollback_len());
        if index < back {
            &self.scrollback[self.scrollback.len() - back + index]
        } else {
            &self.screen[index - back]
        }
    }

    /// What the program has asked to be told, to write back to it.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    /// The screen as text, trailing blanks trimmed. For tests and dumps.
    pub fn screen_text(&self) -> String {
        let lines: Vec<String> = self
            .screen
            .iter()
            .map(|row| {
                let line: String = row
                    .iter()
                    .filter(|c| c.flags & WIDE_TAIL == 0)
                    .map(|c| c.ch)
                    .collect();
                line.trim_end().to_owned()
            })
            .collect();
        let end = lines
            .iter()
            .rposition(|l| !l.is_empty())
            .map_or(0, |i| i + 1);
        lines[..end].join("\n")
    }

    /// The number of screen row 0, counting from the first line kept.
    fn screen_base(&self) -> u64 {
        self.dropped + self.scrollback.len() as u64
    }

    /// The number of row `row` of the view scrolled `back` lines.
    pub fn view_line(&self, row: usize, back: usize) -> u64 {
        self.screen_base() - back.min(self.scrollback_len()) as u64 + row as u64
    }

    /// Line `line` by number, if it is still kept.
    pub fn line(&self, line: u64) -> Option<&[Cell]> {
        let index = line.checked_sub(self.dropped)? as usize;
        match index.checked_sub(self.scrollback.len()) {
            None => self.scrollback.get(index).map(Vec::as_slice),
            Some(row) => self.screen.get(row).map(Vec::as_slice),
        }
    }

    /// The text from `start` to `end`, each a line number and a column
    /// boundary, in either order. Lines the terminal wrapped are joined;
    /// others end in a newline, without their trailing blanks.
    pub fn text_between(&self, start: (u64, usize), end: (u64, usize)) -> String {
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let mut out = String::new();
        for number in start.0..=end.0 {
            let Some(cells) = self.line(number) else {
                continue;
            };
            let from = if number == start.0 { start.1 } else { 0 };
            let to = if number == end.0 { end.1 } else { cells.len() };
            let piece: String = cells[from.min(cells.len())..to.min(cells.len())]
                .iter()
                .filter(|c| c.flags & WIDE_TAIL == 0)
                .map(|c| c.ch)
                .collect();
            let wrapped = to >= cells.len() && cells.last().is_some_and(|c| c.flags & WRAPPED != 0);
            if number == end.0 || wrapped {
                out.push_str(if number == end.0 {
                    piece.trim_end()
                } else {
                    &piece
                });
            } else {
                out.push_str(piece.trim_end());
                out.push('\n');
            }
        }
        out
    }

    /// The columns `[from, to)` of the word or path under column `col`, for
    /// a double click: letters, digits and the characters paths and
    /// addresses are made of. Any other character is a word of one.
    pub fn word_at(&self, line: u64, col: usize) -> Option<(usize, usize)> {
        let cells = self.line(line)?;
        let col = col.min(cells.len().checked_sub(1)?);
        let col = if cells[col].flags & WIDE_TAIL != 0 {
            col.saturating_sub(1)
        } else {
            col
        };
        let wordy = |c: &Cell| c.flags & WIDE_TAIL != 0 || is_word_char(c.ch);
        if !wordy(&cells[col]) {
            return Some((col, col + 1));
        }
        let from = (0..col)
            .rev()
            .take_while(|&i| wordy(&cells[i]))
            .last()
            .unwrap_or(col);
        let to = (col..cells.len())
            .take_while(|&i| wordy(&cells[i]))
            .last()
            .unwrap_or(col)
            + 1;
        Some((from, to))
    }

    /// Line `line` as text, with the column each character starts at.
    pub fn line_chars(&self, line: u64) -> Vec<(usize, char)> {
        self.line(line)
            .map(|cells| {
                cells
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.flags & WIDE_TAIL == 0)
                    .map(|(i, c)| (i, c.ch))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Changes the size. The cursor's line stays on screen: when rows are
    /// lost, the lines above it go into history, as xterm does.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let (cols, rows) = (cols.max(2), rows.max(1));
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        for screen in [&mut self.screen, &mut self.other] {
            for line in screen.iter_mut() {
                line.resize(cols, Cell::default());
            }
        }
        if rows < self.rows {
            let lost = (self.row + 1).saturating_sub(rows);
            for line in self.screen.drain(..lost) {
                if !self.alternate && push_history(&mut self.scrollback, line) {
                    self.dropped += 1;
                }
            }
            self.screen.truncate(rows);
            self.row -= lost;
            let lost_other = self.other.len() - rows;
            self.other.drain(..lost_other);
        } else {
            self.screen.resize(rows, vec![Cell::default(); cols]);
            self.other.resize(rows, vec![Cell::default(); cols]);
        }
        self.cols = cols;
        self.rows = rows;
        self.top = 0;
        self.bottom = rows - 1;
        self.row = self.row.min(rows - 1);
        self.col = self.col.min(cols - 1);
        self.pending_wrap = false;
        self.reset_tabs();
        self.generation += 1;
    }

    /// Feeds the program's output.
    pub fn advance(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.byte(byte);
        }
        self.generation += 1;
    }

    fn byte(&mut self, byte: u8) {
        // Strings end at ST (ESC \) or BEL and take everything else.
        if matches!(self.state, State::Osc | State::Ignore) {
            if self.string_escape {
                self.string_escape = false;
                if byte == b'\\' {
                    self.end_string();
                    return;
                }
                // ESC and anything but a backslash: the string was cut.
                self.end_string();
                self.state = State::Escape;
                self.escape(byte);
                return;
            }
            match byte {
                0x07 => self.end_string(),
                0x1B => self.string_escape = true,
                0x18 | 0x1A => self.state = State::Ground,
                _ if self.state == State::Osc && self.osc.len() < MAX_OSC => self.osc.push(byte),
                _ => {}
            }
            return;
        }
        // C0 controls act anywhere, even inside a sequence.
        match byte {
            0x1B => {
                self.utf8.clear();
                self.state = State::Escape;
                return;
            }
            0x18 | 0x1A => {
                self.state = State::Ground;
                return;
            }
            0x00..=0x1F => {
                self.control(byte);
                return;
            }
            _ => {}
        }
        match self.state.clone() {
            State::Ground => self.ground(byte),
            State::Escape => self.escape(byte),
            State::EscapeIntermediate(i) => {
                self.escape_intermediate(i, byte);
                self.state = State::Ground;
            }
            State::Csi => self.csi_byte(byte),
            State::Osc | State::Ignore => unreachable!("handled above"),
        }
    }

    fn ground(&mut self, byte: u8) {
        if byte < 0x80 {
            self.utf8.clear();
            if byte != 0x7F {
                self.print(byte as char);
            }
            return;
        }
        // UTF-8: collect the bytes of one character.
        if byte & 0xC0 != 0x80 {
            self.utf8.clear();
        }
        self.utf8.push(byte);
        let expected = match self.utf8[0] {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => {
                self.utf8.clear();
                self.print(char::REPLACEMENT_CHARACTER);
                return;
            }
        };
        if self.utf8.len() < expected {
            return;
        }
        let ch = std::str::from_utf8(&self.utf8)
            .ok()
            .and_then(|s| s.chars().next())
            .unwrap_or(char::REPLACEMENT_CHARACTER);
        self.utf8.clear();
        self.print(ch);
    }

    fn control(&mut self, byte: u8) {
        match byte {
            0x08 => {
                self.col = self.col.saturating_sub(1);
                self.pending_wrap = false;
            }
            0x09 => self.tab_forward(1),
            0x0A..=0x0C => self.linefeed(),
            0x0D => {
                self.col = 0;
                self.pending_wrap = false;
            }
            // SO and SI shift to G1 and back; G1 is never line drawing here.
            0x0E | 0x0F => {}
            _ => {}
        }
    }

    fn escape(&mut self, byte: u8) {
        self.state = State::Ground;
        match byte {
            b'[' => {
                self.params.clear();
                self.params.push(Vec::new());
                self.private = None;
                self.intermediate = None;
                self.state = State::Csi;
            }
            b']' => {
                self.osc.clear();
                self.state = State::Osc;
            }
            b'P' | b'X' | b'^' | b'_' => self.state = State::Ignore,
            b'(' | b')' | b'*' | b'+' | b'#' | b' ' | b'%' => {
                self.state = State::EscapeIntermediate(byte)
            }
            b'7' => self.save_cursor(),
            b'8' => self.restore_cursor(),
            b'D' => self.linefeed(),
            b'E' => {
                self.col = 0;
                self.linefeed();
            }
            b'M' => self.reverse_index(),
            b'H' => {
                if let Some(stop) = self.tabs.get_mut(self.col) {
                    *stop = true;
                }
            }
            b'c' => self.reset(),
            // Keypad application and numeric modes: keys are sent the same.
            b'=' | b'>' => {}
            _ => {}
        }
    }

    fn escape_intermediate(&mut self, intermediate: u8, byte: u8) {
        if intermediate == b'(' {
            self.line_drawing = byte == b'0';
        }
    }

    fn csi_byte(&mut self, byte: u8) {
        match byte {
            b'0'..=b'9' => {
                let params = self.params.last_mut().expect("a CSI starts with one");
                if params.is_empty() {
                    params.push(0);
                }
                let last = params.last_mut().expect("just pushed");
                *last = last.saturating_mul(10).saturating_add((byte - b'0') as u16);
            }
            b';' => {
                if self.params.len() < MAX_PARAMS {
                    self.params.push(Vec::new());
                }
            }
            b':' => {
                let params = self.params.last_mut().expect("a CSI starts with one");
                if params.is_empty() {
                    params.push(0);
                }
                params.push(0);
            }
            b'<' | b'=' | b'>' | b'?' => self.private = Some(byte),
            0x20..=0x2F => self.intermediate = Some(byte),
            0x40..=0x7E => {
                self.state = State::Ground;
                self.csi(byte);
            }
            _ => self.state = State::Ground,
        }
    }

    /// Parameter `i`, or `default` when absent or zero.
    fn param(&self, i: usize, default: usize) -> usize {
        match self.params.get(i).and_then(|p| p.first()) {
            Some(&0) | None => default,
            Some(&n) => n as usize,
        }
    }

    fn csi(&mut self, command: u8) {
        let private = self.private;
        let intermediate = self.intermediate;
        let n = self.param(0, 1);
        match (private, intermediate, command) {
            (None, None, b'A') => self.move_rows_clamped(-(n as isize)),
            (None, None, b'B' | b'e') => self.move_rows_clamped(n as isize),
            (None, None, b'C' | b'a') => self.set_col(self.col + n),
            (None, None, b'D') => self.set_col(self.col.saturating_sub(n)),
            (None, None, b'E') => {
                self.move_rows_clamped(n as isize);
                self.set_col(0);
            }
            (None, None, b'F') => {
                self.move_rows_clamped(-(n as isize));
                self.set_col(0);
            }
            (None, None, b'G' | b'`') => self.set_col(n - 1),
            (None, None, b'H' | b'f') => {
                let (row, col) = (self.param(0, 1) - 1, self.param(1, 1) - 1);
                self.goto(row, col);
            }
            (None, None, b'd') => {
                let col = self.col;
                self.goto(n - 1, col);
            }
            (None | Some(b'?'), None, b'J') => self.erase_display(self.param(0, 0)),
            (None | Some(b'?'), None, b'K') => self.erase_line(self.param(0, 0)),
            (None, None, b'X') => {
                let blank = Cell::blank(&self.pen);
                let (row, col) = (self.row, self.col);
                let end = (col + n).min(self.cols);
                self.screen[row][col..end].fill(blank);
                self.pending_wrap = false;
            }
            (None, None, b'@') => {
                let blank = Cell::blank(&self.pen);
                let (row, col, cols) = (self.row, self.col, self.cols);
                let n = n.min(cols - col);
                let line = &mut self.screen[row];
                line[col..].rotate_right(n);
                line[col..col + n].fill(blank);
            }
            (None, None, b'P') => {
                let blank = Cell::blank(&self.pen);
                let (row, col, cols) = (self.row, self.col, self.cols);
                let n = n.min(cols - col);
                let line = &mut self.screen[row];
                line[col..].rotate_left(n);
                line[cols - n..].fill(blank);
                self.pending_wrap = false;
            }
            (None, None, b'L') => {
                if (self.top..=self.bottom).contains(&self.row) {
                    self.scroll_down_from(self.row, n);
                    self.col = 0;
                }
            }
            (None, None, b'M') => {
                if (self.top..=self.bottom).contains(&self.row) {
                    self.scroll_up_from(self.row, n, false);
                    self.col = 0;
                }
            }
            (None, None, b'S') => self.scroll_up_from(self.top, n, true),
            (None, None, b'T') => self.scroll_down_from(self.top, n),
            (None, None, b'I') => self.tab_forward(n),
            (None, None, b'Z') => {
                for _ in 0..n {
                    self.col = (0..self.col).rev().find(|&c| self.tabs[c]).unwrap_or(0);
                }
                self.pending_wrap = false;
            }
            (None, None, b'g') => match self.param(0, 0) {
                0 => {
                    if let Some(stop) = self.tabs.get_mut(self.col) {
                        *stop = false;
                    }
                }
                3 => self.tabs.fill(false),
                _ => {}
            },
            (None, None, b'r') => {
                let top = self.param(0, 1) - 1;
                let bottom = self.param(1, self.rows).min(self.rows) - 1;
                if top < bottom {
                    self.top = top;
                    self.bottom = bottom;
                    self.goto(0, 0);
                }
            }
            (None, None, b's') => self.save_cursor(),
            (None, None, b'u') => self.restore_cursor(),
            (None, None, b'm') => self.sgr(),
            (None, None, b'n') => match self.param(0, 0) {
                5 => self.replies.extend_from_slice(b"\x1b[0n"),
                6 => {
                    let row = if self.modes.origin {
                        self.row - self.top
                    } else {
                        self.row
                    };
                    let reply = format!("\x1b[{};{}R", row + 1, self.col + 1);
                    self.replies.extend_from_slice(reply.as_bytes());
                }
                _ => {}
            },
            // Primary device attributes: a VT220 with ANSI colour.
            (None, None, b'c') if self.param(0, 0) == 0 => {
                self.replies.extend_from_slice(b"\x1b[?62;22c")
            }
            (Some(b'>'), None, b'c') => self.replies.extend_from_slice(b"\x1b[>0;0;0c"),
            (None, None, b'h') => self.set_ansi_modes(true),
            (None, None, b'l') => self.set_ansi_modes(false),
            (Some(b'?'), None, b'h') => self.set_private_modes(true),
            (Some(b'?'), None, b'l') => self.set_private_modes(false),
            // DECRQM: is this mode set?
            (Some(b'?'), Some(b'$'), b'p') => {
                let mode = self.param(0, 0);
                let state = match self.private_mode(mode) {
                    Some(true) => 1,
                    Some(false) => 2,
                    None => 0,
                };
                let reply = format!("\x1b[?{mode};{state}$y");
                self.replies.extend_from_slice(reply.as_bytes());
            }
            // Everything else, including the kitty keyboard queries (`CSI ?
            // u`), which unanswered tell a program to use legacy keys.
            _ => {}
        }
    }

    fn set_ansi_modes(&mut self, on: bool) {
        for i in 0..self.params.len() {
            if self.param(i, 0) == 4 {
                self.modes.insert = on;
            }
        }
    }

    fn private_mode(&self, mode: usize) -> Option<bool> {
        Some(match mode {
            1 => self.modes.app_cursor,
            6 => self.modes.origin,
            7 => self.modes.autowrap,
            25 => self.modes.cursor_visible,
            47 | 1047 | 1049 => self.alternate,
            1000 | 1002 | 1003 => self.modes.mouse,
            1004 => self.modes.focus_events,
            1006 => self.modes.sgr_mouse,
            2004 => self.modes.bracketed_paste,
            2026 => self.modes.sync,
            _ => return None,
        })
    }

    fn set_private_modes(&mut self, on: bool) {
        for i in 0..self.params.len() {
            match self.param(i, 0) {
                1 => self.modes.app_cursor = on,
                6 => {
                    self.modes.origin = on;
                    self.goto(0, 0);
                }
                7 => self.modes.autowrap = on,
                25 => self.modes.cursor_visible = on,
                47 | 1047 => self.switch_screen(on, false),
                1049 => self.switch_screen(on, true),
                1000 | 1002 | 1003 => self.modes.mouse = on,
                1004 => self.modes.focus_events = on,
                1006 => self.modes.sgr_mouse = on,
                2004 => self.modes.bracketed_paste = on,
                2026 => self.modes.sync = on,
                _ => {}
            }
        }
    }

    /// Into or out of the alternate screen, which has no history.
    fn switch_screen(&mut self, on: bool, save_cursor: bool) {
        if on == self.alternate {
            return;
        }
        if on && save_cursor {
            self.saved_alternate = self.snapshot();
        }
        std::mem::swap(&mut self.screen, &mut self.other);
        self.alternate = on;
        if on {
            let blank = Cell::blank(&self.pen);
            for line in &mut self.screen {
                line.fill(blank);
            }
        } else if save_cursor {
            let saved = self.saved_alternate;
            self.apply(saved);
        }
        self.pending_wrap = false;
    }

    fn sgr(&mut self) {
        let params = std::mem::take(&mut self.params);
        let mut i = 0;
        let flat = |p: &Vec<u16>| p.first().copied().unwrap_or(0);
        while i < params.len() {
            let p = &params[i];
            let code = flat(p);
            match code {
                0 => {
                    self.pen.fg = Color::Default;
                    self.pen.bg = Color::Default;
                    self.pen.flags = 0;
                }
                1 => self.pen.flags |= BOLD,
                2 => self.pen.flags |= DIM,
                3 => self.pen.flags |= ITALIC,
                // `4:0` is no underline; any other style is an underline.
                4 => {
                    if p.get(1) == Some(&0) {
                        self.pen.flags &= !UNDERLINE;
                    } else {
                        self.pen.flags |= UNDERLINE;
                    }
                }
                7 => self.pen.flags |= INVERSE,
                8 => self.pen.flags |= HIDDEN,
                9 => self.pen.flags |= STRIKE,
                21 => self.pen.flags |= UNDERLINE,
                22 => self.pen.flags &= !(BOLD | DIM),
                23 => self.pen.flags &= !ITALIC,
                24 => self.pen.flags &= !UNDERLINE,
                27 => self.pen.flags &= !INVERSE,
                28 => self.pen.flags &= !HIDDEN,
                29 => self.pen.flags &= !STRIKE,
                30..=37 => self.pen.fg = Color::Indexed((code - 30) as u8),
                39 => self.pen.fg = Color::Default,
                40..=47 => self.pen.bg = Color::Indexed((code - 40) as u8),
                49 => self.pen.bg = Color::Default,
                90..=97 => self.pen.fg = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => self.pen.bg = Color::Indexed((code - 100 + 8) as u8),
                38 | 48 | 58 => {
                    // Either `38:5:n` / `38:2::r:g:b` in one parameter, or
                    // `38;5;n` / `38;2;r;g;b` across several.
                    let (color, used) = if p.len() > 1 {
                        (extended(&p[1..]), 0)
                    } else {
                        let rest: Vec<u16> = params[i + 1..].iter().map(flat).collect();
                        match rest.first() {
                            Some(5) => (extended(&rest[..rest.len().min(2)]), 2),
                            Some(2) => (extended(&rest[..rest.len().min(4)]), 4),
                            _ => (None, 0),
                        }
                    };
                    i += used;
                    if let Some(color) = color {
                        match code {
                            38 => self.pen.fg = color,
                            48 => self.pen.bg = color,
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn print(&mut self, ch: char) {
        if is_zero_width(ch) {
            return;
        }
        let ch = if self.line_drawing {
            line_drawing(ch)
        } else {
            ch
        };
        let width = display_width(ch).min(2);
        if self.pending_wrap && self.modes.autowrap {
            let last = self.cols - 1;
            self.screen[self.row][last].flags |= WRAPPED;
            self.col = 0;
            self.linefeed();
        }
        self.pending_wrap = false;
        if width == 2 && self.col + 1 >= self.cols {
            if self.modes.autowrap {
                let blank = Cell::blank(&self.pen);
                self.screen[self.row][self.col] = Cell {
                    flags: WRAPPED,
                    ..blank
                };
                self.col = 0;
                self.linefeed();
            } else {
                self.col = self.cols - 2;
            }
        }
        let (row, col) = (self.row, self.col);
        if self.modes.insert {
            let line = &mut self.screen[row];
            line[col..].rotate_right(width);
        }
        // Writing over half of a wide character leaves a blank of the other.
        self.clear_wide_at(row, col);
        if width == 2 {
            self.clear_wide_at(row, col + 1);
        }
        self.screen[row][col] = Cell {
            ch,
            flags: self.pen.flags & !WIDE_TAIL,
            ..self.pen
        };
        if width == 2 {
            self.screen[row][col + 1] = Cell {
                ch: ' ',
                flags: WIDE_TAIL,
                ..self.pen
            };
        }
        if col + width >= self.cols {
            self.col = self.cols - 1;
            self.pending_wrap = true;
        } else {
            self.col = col + width;
        }
    }

    fn clear_wide_at(&mut self, row: usize, col: usize) {
        let Some(cell) = self.screen[row].get(col).copied() else {
            return;
        };
        if cell.flags & WIDE_TAIL != 0 && col > 0 {
            self.screen[row][col - 1].ch = ' ';
        } else if col + 1 < self.cols && self.screen[row][col + 1].flags & WIDE_TAIL != 0 {
            self.screen[row][col + 1] = Cell {
                flags: 0,
                ..self.screen[row][col + 1]
            };
        }
    }

    fn linefeed(&mut self) {
        self.pending_wrap = false;
        if self.row == self.bottom {
            self.scroll_up_from(self.top, 1, true);
        } else if self.row + 1 < self.rows {
            self.row += 1;
        }
    }

    fn reverse_index(&mut self) {
        self.pending_wrap = false;
        if self.row == self.top {
            self.scroll_down_from(self.top, 1);
        } else {
            self.row = self.row.saturating_sub(1);
        }
    }

    /// Scrolls rows `from..=bottom` up by `n`. Lines leaving the top of the
    /// whole primary screen go into history when `keep` is set.
    fn scroll_up_from(&mut self, from: usize, n: usize, keep: bool) {
        let n = n.min(self.bottom + 1 - from);
        let blank = vec![Cell::blank(&self.pen); self.cols];
        for _ in 0..n {
            let line = self.screen.remove(from);
            if keep && from == 0 && !self.alternate && push_history(&mut self.scrollback, line) {
                self.dropped += 1;
            }
            self.screen.insert(self.bottom, blank.clone());
        }
    }

    fn scroll_down_from(&mut self, from: usize, n: usize) {
        let n = n.min(self.bottom + 1 - from);
        let blank = vec![Cell::blank(&self.pen); self.cols];
        for _ in 0..n {
            self.screen.remove(self.bottom);
            self.screen.insert(from, blank.clone());
        }
    }

    fn erase_display(&mut self, mode: usize) {
        let blank = Cell::blank(&self.pen);
        let (row, col) = (self.row, self.col);
        match mode {
            0 => {
                self.screen[row][col..].fill(blank);
                for line in &mut self.screen[row + 1..] {
                    line.fill(blank);
                }
            }
            1 => {
                for line in &mut self.screen[..row] {
                    line.fill(blank);
                }
                self.screen[row][..=col].fill(blank);
            }
            2 => {
                for line in &mut self.screen {
                    line.fill(blank);
                }
            }
            3 => {
                self.dropped += self.scrollback.len() as u64;
                self.scrollback.clear();
            }
            _ => {}
        }
        self.pending_wrap = false;
    }

    fn erase_line(&mut self, mode: usize) {
        let blank = Cell::blank(&self.pen);
        let (row, col) = (self.row, self.col);
        let line = &mut self.screen[row];
        match mode {
            0 => line[col..].fill(blank),
            1 => line[..=col].fill(blank),
            2 => line.fill(blank),
            _ => {}
        }
        self.pending_wrap = false;
    }

    /// Moves within the scroll region when the cursor is in it, as CUU and
    /// CUD do, and within the screen otherwise.
    fn move_rows_clamped(&mut self, delta: isize) {
        let (low, high) = if (self.top..=self.bottom).contains(&self.row) {
            (self.top, self.bottom)
        } else {
            (0, self.rows - 1)
        };
        self.row = self.row.saturating_add_signed(delta).clamp(low, high);
        self.pending_wrap = false;
    }

    fn set_col(&mut self, col: usize) {
        self.col = col.min(self.cols - 1);
        self.pending_wrap = false;
    }

    /// CUP: in origin mode, relative to the scroll region.
    fn goto(&mut self, row: usize, col: usize) {
        let (offset, last) = if self.modes.origin {
            (self.top, self.bottom)
        } else {
            (0, self.rows - 1)
        };
        self.row = (row + offset).min(last);
        self.set_col(col);
    }

    fn tab_forward(&mut self, n: usize) {
        for _ in 0..n {
            self.col = (self.col + 1..self.cols)
                .find(|&c| self.tabs[c])
                .unwrap_or(self.cols - 1);
        }
        self.pending_wrap = false;
    }

    fn reset_tabs(&mut self) {
        self.tabs = (0..self.cols).map(|c| c > 0 && c % 8 == 0).collect();
    }

    fn snapshot(&self) -> Saved {
        Saved {
            row: self.row,
            col: self.col,
            pen: self.pen,
            origin: self.modes.origin,
            line_drawing: self.line_drawing,
        }
    }

    fn apply(&mut self, saved: Saved) {
        self.row = saved.row.min(self.rows - 1);
        self.col = saved.col.min(self.cols - 1);
        self.pen = saved.pen;
        self.modes.origin = saved.origin;
        self.line_drawing = saved.line_drawing;
        self.pending_wrap = false;
    }

    fn save_cursor(&mut self) {
        self.saved = self.snapshot();
    }

    fn restore_cursor(&mut self) {
        let saved = self.saved;
        self.apply(saved);
    }

    fn end_string(&mut self) {
        if self.state == State::Osc {
            let text = String::from_utf8_lossy(&self.osc).into_owned();
            if let Some((kind, value)) = text.split_once(';')
                && matches!(kind, "0" | "2")
            {
                self.title = value.to_owned();
            }
        }
        self.osc.clear();
        self.state = State::Ground;
    }

    fn reset(&mut self) {
        let mut fresh = Term::new(self.cols, self.rows);
        std::mem::swap(&mut fresh.scrollback, &mut self.scrollback);
        fresh.dropped = self.dropped;
        fresh.generation = self.generation;
        *self = fresh;
    }
}

/// Adds a line to history; whether the oldest had to go to make room.
fn push_history(history: &mut VecDeque<Vec<Cell>>, line: Vec<Cell>) -> bool {
    let full = history.len() == SCROLLBACK;
    if full {
        history.pop_front();
    }
    history.push_back(line);
    full
}

/// A colour from the parameters after 38 or 48: `5;n` or `2;r;g;b`, where
/// the colon form may carry an empty colour-space id before r, g, b.
fn extended(p: &[u16]) -> Option<Color> {
    match p {
        [5, n, ..] => Some(Color::Indexed((*n).min(255) as u8)),
        [2, _, r, g, b, ..] | [2, r, g, b] => Some(Color::Rgb(
            (*r).min(255) as u8,
            (*g).min(255) as u8,
            (*b).min(255) as u8,
        )),
        _ => None,
    }
}

/// Marks that attach to the character before them. The grid holds one
/// character per cell, so they are dropped rather than given a cell.
fn is_zero_width(ch: char) -> bool {
    matches!(ch as u32,
        0x0300..=0x036F | 0x200B..=0x200F | 0x2060..=0x2064 | 0xFE00..=0xFE0F
        | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xE0100..=0xE01EF)
}

/// DEC Special Graphics, for programs that draw boxes with `ESC ( 0`.
fn line_drawing(ch: char) -> char {
    match ch {
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'q' => '─',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'a' => '▒',
        '`' => '◆',
        'f' => '°',
        'g' => '±',
        '~' => '·',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        _ => ch,
    }
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || "/._-~:@+#%=?&$".contains(ch)
}

/// A file reference under character `index` of `text`: a path, optionally
/// followed by `:line` and `:column`, as compilers and `claude` print them.
/// Quotes, brackets and trailing punctuation around it are not part of it.
pub fn path_at(text: &[char], index: usize) -> Option<(String, Option<usize>, Option<usize>)> {
    let stop = |c: &char| c.is_whitespace() || "\"'`()[]{}<>,;|".contains(*c);
    if index >= text.len() || stop(&text[index]) {
        return None;
    }
    let from = (0..index)
        .rev()
        .take_while(|&i| !stop(&text[i]))
        .last()
        .unwrap_or(index);
    let to = (index..text.len())
        .take_while(|&i| !stop(&text[i]))
        .last()
        .unwrap_or(index)
        + 1;
    let token: String = text[from..to].iter().collect();
    let token = token.trim_end_matches(['.', ':', '!', '?']);
    let token = token.strip_prefix("file://").unwrap_or(token);
    let mut parts = token.split(':');
    let path = parts.next().filter(|p| !p.is_empty())?;
    if !path.contains('/') && !path.contains('.') {
        return None;
    }
    let line = parts
        .next()
        .and_then(|p| p.parse().ok())
        .filter(|&n: &usize| n > 0);
    let column = line
        .and_then(|_| parts.next())
        .and_then(|p| p.parse().ok())
        .filter(|&n: &usize| n > 0);
    Some((path.to_owned(), line, column))
}

/// The colour of a palette entry, 0 to 255, for the 16 named colours given
/// by the theme and the rest computed as xterm does.
pub fn palette(index: u8, named: &[[f32; 4]; 16]) -> [f32; 4] {
    match index {
        0..=15 => named[index as usize],
        16..=231 => {
            let i = index - 16;
            let level = |v: u8| {
                if v == 0 {
                    0.0
                } else {
                    (55.0 + 40.0 * v as f32) / 255.0
                }
            };
            [level(i / 36), level((i / 6) % 6), level(i % 6), 1.0]
        }
        232..=255 => {
            let v = (8.0 + 10.0 * (index - 232) as f32) / 255.0;
            [v, v, v, 1.0]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(input: &str) -> Term {
        let mut t = Term::new(20, 5);
        t.advance(input.as_bytes());
        t
    }

    #[test]
    fn text_wraps_and_scrolls_into_history() {
        let t = term("0123456789012345678901\r\nline2\r\n3\r\n4\r\n5\r\n6");
        assert_eq!(t.scrollback_len(), 2);
        assert_eq!(t.screen_text(), "line2\n3\n4\n5\n6");
        let first: String = t.visible_row(0, 2).iter().map(|c| c.ch).collect();
        assert_eq!(first.trim_end(), "01234567890123456789");
        let second: String = t.visible_row(1, 2).iter().map(|c| c.ch).collect();
        assert_eq!(second.trim_end(), "01");
    }

    #[test]
    fn the_last_column_waits_before_wrapping() {
        // Twenty characters fill the line without moving to the next; CR
        // then returns on the same line, which is what prompts rely on.
        let t = term("abcdefghijklmnopqrst\rX");
        assert_eq!(t.screen_text(), "Xbcdefghijklmnopqrst");
        assert_eq!(t.cursor(), (0, 1));
    }

    #[test]
    fn cursor_movement_and_erasing() {
        let t = term("hello world\x1b[1;7HWORLD\x1b[1;3H\x1b[K");
        assert_eq!(t.screen_text(), "he");
        let t = term("aaaa\r\nbbbb\r\ncccc\x1b[2;2H\x1b[1J");
        assert_eq!(t.screen_text(), "\n  bb\ncccc");
        let t = term("abcdef\x1b[1;2H\x1b[2P");
        assert_eq!(t.screen_text(), "adef");
        let t = term("abcdef\x1b[1;2H\x1b[2@");
        assert_eq!(t.screen_text(), "a  bcdef");
        let t = term("abcdef\x1b[1;2H\x1b[3X");
        assert_eq!(t.screen_text(), "a   ef");
        let t = term("x\x1b[5;5H\x1b[2A\x1b[3Cy");
        assert_eq!(t.cursor(), (2, 8));
    }

    #[test]
    fn ink_style_redraw_moves_up_and_erases() {
        // What a React-for-terminals app does between frames: up to the top
        // of its last output, erase each line, write the new frame.
        let mut t = term("> old input\r\n  status\r\n");
        t.advance(b"\x1b[2K\x1b[1A\x1b[2K\x1b[1A\x1b[2K\x1b[G> new\r\n  done");
        assert_eq!(t.screen_text(), "> new\n  done");
    }

    #[test]
    fn scroll_regions_insert_and_delete_lines() {
        let mut t = term("1\r\n2\r\n3\r\n4\r\n5");
        t.advance(b"\x1b[2;4r\x1b[4;1H\n");
        assert_eq!(t.screen_text(), "1\n3\n4\n\n5");
        assert_eq!(t.scrollback_len(), 0, "a region scroll keeps no history");
        t.advance(b"\x1b[2;1H\x1b[L");
        assert_eq!(t.screen_text(), "1\n\n3\n4\n5");
        t.advance(b"\x1b[M");
        assert_eq!(t.screen_text(), "1\n3\n4\n\n5");
        t.advance(b"\x1b[r\x1b[1;1H\x1bM");
        assert_eq!(t.screen_text(), "\n1\n3\n4");
    }

    #[test]
    fn colours_and_attributes() {
        let t =
            term("\x1b[1;31;42ma\x1b[38;5;208mb\x1b[38;2;1;2;3;48:2::4:5:6mc\x1b[0md\x1b[7;4:0m");
        let row = t.visible_row(0, 0);
        assert_eq!(row[0].fg, Color::Indexed(1));
        assert_eq!(row[0].bg, Color::Indexed(2));
        assert_eq!(row[0].flags, BOLD);
        assert_eq!(row[1].fg, Color::Indexed(208));
        assert_eq!(row[2].fg, Color::Rgb(1, 2, 3));
        assert_eq!(row[2].bg, Color::Rgb(4, 5, 6));
        assert_eq!(
            row[3],
            Cell {
                ch: 'd',
                ..Cell::default()
            }
        );
        assert_eq!(t.pen.flags, INVERSE);
    }

    #[test]
    fn utf8_split_across_reads_and_wide_characters() {
        let mut t = Term::new(6, 2);
        let bytes = "é漢✻".as_bytes();
        for byte in bytes {
            t.advance(std::slice::from_ref(byte));
        }
        assert_eq!(t.screen_text(), "é漢✻");
        assert_eq!(t.visible_row(0, 0)[2].flags, WIDE_TAIL);
        assert_eq!(t.cursor(), (0, 4));
        // A wide character that does not fit wraps whole.
        t.advance("x漢".as_bytes());
        assert_eq!(t.screen_text(), "é漢✻x\n漢");
        // Combining marks and variation selectors take no cell.
        let t = term("e\u{301}\u{fe0f}!");
        assert_eq!(t.screen_text(), "e!");
    }

    #[test]
    fn replies_to_queries() {
        let mut t = term("ab\x1b[6n\x1b[c\x1b[>c\x1b[?2004h\x1b[?2004$p\x1b[?9999$p");
        assert_eq!(
            String::from_utf8(t.take_replies()).unwrap(),
            "\x1b[1;3R\x1b[?62;22c\x1b[>0;0;0c\x1b[?2004;1$y\x1b[?9999;0$y"
        );
        assert!(t.take_replies().is_empty());
        assert!(t.modes.bracketed_paste);
    }

    #[test]
    fn alternate_screen_keeps_the_primary() {
        let mut t = term("shell$ ");
        t.advance(b"\x1b[?1049h\x1b[H\x1b[2Jfull screen");
        assert!(t.is_alternate());
        assert_eq!(t.screen_text(), "full screen");
        t.advance(b"\x1b[?1049l");
        assert_eq!(t.screen_text(), "shell$");
        assert_eq!(t.cursor(), (0, 7));
    }

    #[test]
    fn strings_are_skipped_and_titles_kept() {
        let t = term("\x1b]0;my title\x07a\x1b]8;;http://x\x1b\\b\x1bP1$r\x1b\\c\x1b_Gi=1\x1b\\d");
        assert_eq!(t.title, "my title");
        assert_eq!(t.screen_text(), "abcd");
    }

    #[test]
    fn line_drawing_and_tabs() {
        let t = term("\x1b(0lqk\x1b(B\tx");
        assert_eq!(t.screen_text(), "┌─┐     x");
        let t = term("a\x1b[3Ib");
        assert_eq!(t.cursor(), (0, 19));
    }

    #[test]
    fn saved_cursor_and_reset() {
        let mut t = term("\x1b[3;4H\x1b[31m\x1b7\x1b[H\x1b[0m\x1b8x");
        assert_eq!(t.cursor(), (2, 4));
        assert_eq!(t.visible_row(2, 0)[3].fg, Color::Indexed(1));
        t.advance(b"\x1bc");
        assert_eq!(t.screen_text(), "");
        assert_eq!(t.cursor(), (0, 0));
    }

    #[test]
    fn resize_keeps_the_cursor_line() {
        let mut t = term("1\r\n2\r\n3\r\n4\r\n5");
        t.resize(10, 2);
        assert_eq!(t.screen_text(), "4\n5");
        assert_eq!(t.cursor(), (1, 1));
        assert_eq!(t.scrollback_len(), 3);
        t.resize(30, 4);
        assert_eq!((t.cols(), t.rows()), (30, 4));
        t.advance(b"\x1b[4;30Hz");
        assert_eq!(t.cursor(), (3, 29));
    }

    #[test]
    fn garbage_never_panics() {
        // A fixed LCG over bytes weighted towards escape characters.
        let mut seed = 0x9E3779B97F4A7C15u64;
        let mut t = Term::new(7, 3);
        for round in 0..20_000 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let pick = (seed >> 33) as usize;
            let byte = match pick % 6 {
                0 => 0x1B,
                1 => b"[]();?:0123456789$>"[pick / 7 % 19],
                2 => b"ABCDGHJKLMPSTXZ@dfghlmnrsu`cq"[pick / 7 % 29],
                3 => (pick / 7 % 256) as u8,
                _ => b'a' + (pick / 7 % 26) as u8,
            };
            t.advance(&[byte]);
            if round % 997 == 0 {
                t.resize(2 + pick % 9, 1 + pick % 5);
            }
            let (row, col) = t.cursor();
            assert!(row < t.rows() && col < t.cols());
        }
    }

    #[test]
    fn copied_text_joins_wrapped_lines_and_follows_scrolling() {
        let mut t = Term::new(10, 4);
        // One long line that wraps, then two short ones.
        t.advance(b"abcdefghijklm\r\nshort  \r\nlast");
        let first = t.view_line(0, 0);
        assert_eq!(
            t.text_between((first, 0), (first + 3, 4)),
            "abcdefghijklm\nshort\nlast"
        );
        assert_eq!(t.text_between((first + 2, 2), (first, 8)), "ijklm\nsh");
        // Output pushes everything up; the same numbers find the same text.
        t.advance(b"\r\nmore\r\nand more");
        assert_eq!(t.text_between((first, 0), (first + 1, 10)), "abcdefghijklm");
        assert_eq!(t.view_line(0, 0), first + 2);
    }

    #[test]
    fn line_numbers_survive_history_dropping_off() {
        let mut t = Term::new(4, 1);
        for n in 0..SCROLLBACK + 5 {
            t.advance(format!("\r\n{}", n % 10).as_bytes());
        }
        let oldest = t.view_line(0, t.scrollback_len());
        assert!(t.line(oldest - 1).is_none(), "dropped lines are gone");
        assert!(t.line(oldest).is_some());
        assert!(
            t.line(t.view_line(0, 0) + 1).is_none(),
            "nothing below the screen"
        );
    }

    #[test]
    fn words_and_paths_under_a_click() {
        let t = term("see src/main.rs:42 now");
        let line = t.view_line(0, 0);
        assert_eq!(t.word_at(line, 6), Some((4, 18)));
        assert_eq!(t.word_at(line, 3), Some((3, 4)), "a blank is a word of one");
        let chars: Vec<char> = t.line_chars(line).into_iter().map(|(_, c)| c).collect();
        assert_eq!(
            path_at(&chars, 8),
            Some(("src/main.rs".into(), Some(42), None))
        );
        assert_eq!(path_at(&chars, 1), None, "a word without a slash or dot");
        let text: Vec<char> = "(at /tmp/a b.rs:3:9.) 'x/y.txt'".chars().collect();
        assert_eq!(path_at(&text, 6), Some(("/tmp/a".into(), None, None)));
        assert_eq!(path_at(&text, 12), Some(("b.rs".into(), Some(3), Some(9))));
        assert_eq!(path_at(&text, 10), None, "a blank is no path");
        assert_eq!(path_at(&text, 26), Some(("x/y.txt".into(), None, None)));
        let url: Vec<char> = "file:///p/a.rs:7".chars().collect();
        assert_eq!(path_at(&url, 9), Some(("/p/a.rs".into(), Some(7), None)));
    }

    #[test]
    fn the_palette_matches_xterm() {
        let named = [[0.5; 4]; 16];
        assert_eq!(palette(3, &named), [0.5; 4]);
        assert_eq!(palette(16, &named), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(palette(231, &named), [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(palette(232, &named)[0], 8.0 / 255.0);
    }
}
