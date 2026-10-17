//! A CSV or TSV document as a grid.
//!
//! The grid is a view of the document, not a copy. The text stays in the
//! buffer, the parse ([`crate::text::csv`]) maps every cell to a byte range
//! of it, and only the cells on screen are turned into strings when a frame
//! is drawn. The first record is the header and stays in view while the
//! body scrolls. Column widths are sampled from the first records and
//! capped, so one long note does not push the other columns off screen.
//! The strip above the text names the file's shape and holds the Grid and
//! Text switch.
//!
//! One cell can be selected, by a click or from the keys, and the selection
//! outlives a reparse as long as its record and column still exist. Editing
//! a cell comes later.

use crate::render::{
    font::Atlas,
    layout::{self, Hit, Theme, Viewport},
    metal::GlyphInstance,
};
use crate::text::{
    buffer::Buffer,
    columns::display_width,
    csv::{self, Cell, Dialect, Table},
    rope::Rope,
};
use std::collections::HashMap;

/// Bigger than this and the document stays text: the parse keeps a range
/// per cell, and the grid is for looking at data, not at a dump.
pub const MAX_BYTES: usize = 32 * 1024 * 1024;

/// Column widths are sampled from this many records.
const SAMPLE_ROWS: usize = 200;
/// A column is never narrower or wider than this, in code cells.
const MIN_WIDTH: usize = 3;
const MAX_WIDTH: usize = 48;
/// Air left and right of a cell's text, in points.
const CELL_PAD: f32 = 8.0;

/// The measurements the grid is laid out in, from the code font.
#[derive(Clone, Copy, Debug)]
pub struct Cells {
    pub line_height: f32,
    pub advance: f32,
}

impl Cells {
    pub fn of(atlas: &Atlas) -> Self {
        Self {
            line_height: atlas.metrics.line_height,
            advance: atlas.metrics.advance,
        }
    }
}

/// Whether `buffer` is shown as a grid: a CSV or TSV file of a size the
/// parse handles in a blink.
pub fn is_table_file(buffer: &Buffer) -> bool {
    matches!(buffer.extension().as_deref(), Some("csv" | "tsv"))
        && buffer.rope.len_bytes() <= MAX_BYTES
        && !buffer.is_preview_file()
}

/// One document's parse and how it is looked at.
pub struct TableView {
    /// The text the table was parsed from, for the stale check.
    rope: Rope,
    pub dialect: Dialect,
    pub table: Table,
    /// Each column's width in code cells, from the first records.
    pub widths: Vec<usize>,
    /// Records with fewer cells than the widest.
    pub short_rows: usize,
    /// The first body record on screen, counted from the one after the
    /// header.
    pub scroll_row: usize,
    /// How far the columns are scrolled left, in points.
    pub scroll_x: f32,
    /// The grid rather than the text.
    pub mode_grid: bool,
    /// The selected cell as record and column; the header is record 0.
    pub selected: Option<(usize, usize)>,
}

/// One step of the selection from the keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    Up,
    Down,
    Left,
    Right,
    /// Tab: right, and on to the next record from the last column.
    Next,
    /// Shift-Tab: left, and back to the previous record from the first.
    Previous,
    /// Home and End: the record's first and last column.
    RowStart,
    RowEnd,
    /// Cmd-Home and Cmd-End: the first and the last record.
    First,
    Last,
    PageUp,
    PageDown,
}

impl TableView {
    fn parse(rope: &Rope) -> Self {
        let text = rope.to_string();
        let dialect = Dialect::detect(&text);
        let table = csv::parse(&text, &dialect);
        let widths = sample_widths(&text, &table);
        let short_rows = table
            .rows
            .iter()
            .filter(|row| row.cells.len() < widths.len())
            .count();
        Self {
            rope: rope.clone(),
            dialect,
            table,
            widths,
            short_rows,
            scroll_row: 0,
            scroll_x: 0.0,
            mode_grid: true,
            selected: None,
        }
    }

    /// The last record and the last column, when there are any.
    fn last_cell(&self) -> Option<(usize, usize)> {
        Some((
            self.table.rows.len().checked_sub(1)?,
            self.column_count().checked_sub(1)?,
        ))
    }

    /// Keeps the selection on a cell that exists: after a reparse that
    /// removed records or columns, the selection moves in to the last.
    fn clamp_selection(&mut self) {
        self.selected = match (self.selected, self.last_cell()) {
            (Some((row, column)), Some((last_row, last_column))) => {
                Some((row.min(last_row), column.min(last_column)))
            }
            _ => None,
        };
    }

    /// Selects `cell`, or nothing, and brings it on screen.
    pub fn select(&mut self, cell: Option<(usize, usize)>, cells: Cells, rect: Viewport) {
        self.selected = cell;
        self.clamp_selection();
        self.scroll_to_selection(cells, rect);
    }

    /// Moves the selection one `step`, clamped at the grid's edges, and
    /// brings it on screen. Nothing happens without a selection: the keys
    /// scroll then. `page` is how many records a page key moves by.
    pub fn move_selection(
        &mut self,
        step: Move,
        page: usize,
        cells: Cells,
        rect: Viewport,
    ) -> bool {
        let (Some((row, column)), Some((last_row, last_column))) =
            (self.selected, self.last_cell())
        else {
            return false;
        };
        let target = match step {
            Move::Up => (row.saturating_sub(1), column),
            Move::Down => (row + 1, column),
            Move::Left => (row, column.saturating_sub(1)),
            Move::Right => (row, column + 1),
            Move::Next if column < last_column => (row, column + 1),
            Move::Next if row < last_row => (row + 1, 0),
            Move::Next => (row, column),
            Move::Previous if column > 0 => (row, column - 1),
            Move::Previous if row > 0 => (row - 1, last_column),
            Move::Previous => (row, column),
            Move::RowStart => (row, 0),
            Move::RowEnd => (row, last_column),
            Move::First => (0, column),
            Move::Last => (last_row, column),
            Move::PageUp => (row.saturating_sub(page), column),
            Move::PageDown => (row + page, column),
        };
        // The header is always on screen, so going to it would leave the
        // body where it was; the first record means the top of the file.
        if step == Move::First {
            self.scroll_row = 0;
        }
        self.select(Some(target), cells, rect);
        true
    }

    /// Scrolls so the selected cell is whole on screen: by records down,
    /// by points sideways. A cell wider than the view shows its start.
    pub fn scroll_to_selection(&mut self, cells: Cells, rect: Viewport) {
        let Some((row, column)) = self.selected else {
            return;
        };
        if row > 0 {
            let visible = rows_visible(rect, cells.line_height);
            let body = row - 1;
            if body < self.scroll_row {
                self.scroll_row = body;
            } else if body >= self.scroll_row + visible {
                self.scroll_row = body + 1 - visible;
            }
        }
        let edges = self.column_edges(cells);
        let content = (rect.width - self.gutter_width(cells)).max(0.0);
        if edges[column] < self.scroll_x {
            self.scroll_x = edges[column];
        } else if edges[column + 1] > self.scroll_x + content {
            self.scroll_x = (edges[column + 1] - content).min(edges[column]);
        }
        self.clamp_scroll(cells, rect);
    }

    /// The cell a press at a point selects: the one under it, or the first
    /// of the record whose number is under it in the gutter.
    pub fn cell_for_press(
        &self,
        cells: Cells,
        rect: Viewport,
        x: f32,
        y: f32,
    ) -> Option<(usize, usize)> {
        if rect.contains(x, y) && x < rect.x + self.gutter_width(cells) {
            return Some((self.row_at(cells, rect, y)?, 0));
        }
        self.cell_at(cells, rect, x, y)
    }

    /// Whether the parse is of the text as it is now.
    pub fn is_current(&self, rope: &Rope) -> bool {
        self.rope.same_as(rope)
    }

    /// Records below the header.
    pub fn body_rows(&self) -> usize {
        self.table.rows.len().saturating_sub(1)
    }

    pub fn column_count(&self) -> usize {
        self.widths.len()
    }

    /// The row numbers' column, sized like the editor's gutter.
    pub fn gutter_width(&self, cells: Cells) -> f32 {
        (digits(self.table.rows.len()) + 2) as f32 * cells.advance
    }

    /// Where each column starts, in points from the first one's left edge,
    /// with the right edge of the last one at the end.
    fn column_edges(&self, cells: Cells) -> Vec<f32> {
        let mut edges = Vec::with_capacity(self.widths.len() + 1);
        let mut x = 0.0;
        edges.push(x);
        for width in &self.widths {
            x += *width as f32 * cells.advance + 2.0 * CELL_PAD;
            edges.push(x);
        }
        edges
    }

    /// The width of every column together.
    pub fn grid_width(&self, cells: Cells) -> f32 {
        self.widths
            .iter()
            .map(|w| *w as f32 * cells.advance + 2.0 * CELL_PAD)
            .sum()
    }

    /// Keeps the scroll inside the records and the columns.
    pub fn clamp_scroll(&mut self, cells: Cells, rect: Viewport) {
        let visible = rows_visible(rect, cells.line_height);
        self.scroll_row = self
            .scroll_row
            .min(self.body_rows().saturating_sub(visible));
        let content = (rect.width - self.gutter_width(cells)).max(0.0);
        let farthest = (self.grid_width(cells) - content).max(0.0);
        self.scroll_x = self.scroll_x.clamp(0.0, farthest);
    }

    pub fn scroll_rows(&mut self, delta: isize, cells: Cells, rect: Viewport) {
        self.scroll_row = self.scroll_row.saturating_add_signed(delta);
        self.clamp_scroll(cells, rect);
    }

    pub fn scroll_points(&mut self, dx: f32, cells: Cells, rect: Viewport) {
        self.scroll_x += dx;
        self.clamp_scroll(cells, rect);
    }

    /// The record under a height in `rect`: the header is record 0.
    fn row_at(&self, cells: Cells, rect: Viewport, y: f32) -> Option<usize> {
        if y < rect.y || y >= rect.y + rect.height {
            return None;
        }
        let row = if y < rect.y + cells.line_height {
            0
        } else {
            1 + self.scroll_row + ((y - rect.y - cells.line_height) / cells.line_height) as usize
        };
        (row < self.table.rows.len()).then_some(row)
    }

    /// The record and column under a point in `rect`: the header is record
    /// 0. Nothing over the gutter or past the last column.
    pub fn cell_at(&self, cells: Cells, rect: Viewport, x: f32, y: f32) -> Option<(usize, usize)> {
        let gutter = self.gutter_width(cells);
        if !rect.contains(x, y) || x < rect.x + gutter {
            return None;
        }
        let gx = x - rect.x - gutter + self.scroll_x;
        let edges = self.column_edges(cells);
        let column = edges.windows(2).position(|w| gx >= w[0] && gx < w[1])?;
        Some((self.row_at(cells, rect, y)?, column))
    }
}

fn digits(n: usize) -> usize {
    n.max(1).ilog10() as usize + 1
}

/// A cell's value as one line: a line break or a tab inside it is a space.
fn one_line(value: &str) -> impl Iterator<Item = char> + '_ {
    value
        .chars()
        .filter(|ch| *ch != '\r')
        .map(|ch| if matches!(ch, '\n' | '\t') { ' ' } else { ch })
}

/// The widest value in each of the first records, between the bounds.
fn sample_widths(text: &str, table: &Table) -> Vec<usize> {
    let mut widths = vec![MIN_WIDTH; table.column_count()];
    for row in table.rows.iter().take(SAMPLE_ROWS) {
        for (c, cell) in row.cells.iter().enumerate() {
            let value = Table::cell_text(text, cell);
            let width: usize = one_line(&value).map(display_width).sum();
            widths[c] = widths[c].max(width.min(MAX_WIDTH));
        }
    }
    widths
}

/// Brings `tables` up to date for `buffer`. A document that is not CSV or
/// TSV has no entry. The parse is redone when the text changed and the grid
/// is showing; typing in text mode leaves the last parse in place until the
/// grid is asked for again, so a keystroke in a big file costs nothing here.
pub fn sync(tables: &mut HashMap<u64, TableView>, buffer: &Buffer) {
    let id = buffer.id();
    if !is_table_file(buffer) {
        tables.remove(&id);
        return;
    }
    match tables.get_mut(&id) {
        None => {
            tables.insert(id, TableView::parse(&buffer.rope));
        }
        Some(view) if view.mode_grid && !view.is_current(&buffer.rope) => {
            // The scroll stays where it was; the next draw clamps it.
            let fresh = TableView::parse(&buffer.rope);
            view.rope = fresh.rope;
            view.dialect = fresh.dialect;
            view.table = fresh.table;
            view.widths = fresh.widths;
            view.short_rows = fresh.short_rows;
            view.clamp_selection();
        }
        Some(_) => {}
    }
}

/// One line for the self-test dump.
pub fn dump(view: Option<&TableView>) -> String {
    match view {
        None => "none".to_string(),
        Some(view) if !view.mode_grid => "text".to_string(),
        Some(view) => format!(
            "grid rows={} cols={} scroll={} sel={}",
            view.table.rows.len(),
            view.column_count(),
            view.scroll_row,
            view.selected
                .map_or("-".to_string(), |(row, column)| format!("{row},{column}"))
        ),
    }
}

// ---- the strip ------------------------------------------------------------

const MODES: [&str; 2] = ["Grid", "Text"];

/// The mode segments, where they are.
pub fn strip_hits(atlas: &mut Atlas, strip: Viewport) -> Vec<(Hit, Viewport)> {
    let mut x = strip.x + 16.0;
    MODES
        .iter()
        .enumerate()
        .map(|(index, label)| {
            let rect = layout::strip_button(atlas, strip, x, label, 24.0);
            x += rect.width;
            (Hit::TableMode(index == 0), rect)
        })
        .collect()
}

fn delimiter_name(delimiter: u8) -> &'static str {
    match delimiter {
        b'\t' => "tab-separated",
        b';' => "semicolon-separated",
        b'|' => "pipe-separated",
        _ => "comma-separated",
    }
}

/// The file's shape and the switch.
pub fn draw_strip(
    view: &TableView,
    file: &str,
    atlas: &mut Atlas,
    strip: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    let pill = layout::strip_pill(out, atlas, strip, "Table", theme.accent);
    let rows = view.table.rows.len();
    let columns = view.column_count();
    let mut facts = format!(
        "{rows} {} and {columns} {} in {file}, {}",
        if rows == 1 { "row" } else { "rows" },
        if columns == 1 { "column" } else { "columns" },
        delimiter_name(view.dialect.delimiter),
    );
    if view.short_rows > 0 {
        facts.push_str(&format!(
            "; {} {} fewer cells",
            view.short_rows,
            if view.short_rows == 1 {
                "row has"
            } else {
                "rows have"
            }
        ));
    }
    layout::strip_facts(out, atlas, strip, pill, &facts, theme);
    let hits = strip_hits(atlas, strip);
    // The two modes read as one control: a track, and the chosen half lit.
    if let (Some((_, first)), Some((_, last))) = (hits.first(), hits.last()) {
        layout::push_rounded_rect(
            out,
            Viewport {
                width: last.x + last.width - first.x,
                ..*first
            },
            layout::UI_RADIUS,
            theme.tab_hover,
        );
    }
    for (hit, rect) in hits {
        let Hit::TableMode(grid) = hit else {
            continue;
        };
        let chosen = grid == view.mode_grid;
        let inner = Viewport {
            x: rect.x + 2.0,
            y: rect.y + 2.0,
            width: rect.width - 4.0,
            height: rect.height - 4.0,
        };
        if chosen {
            layout::push_rounded_rect(out, inner, 5.0, theme.palette_selected);
        } else if layout::hovered(rect) {
            layout::push_rounded_rect(out, inner, 5.0, theme.control_hover);
        }
        layout::hotspot(rect, layout::Cursor::Pointing, None);
        layout::push_ui_text_centered(
            out,
            atlas,
            rect,
            MODES[usize::from(!grid)],
            if chosen {
                theme.text
            } else {
                theme.status_text
            },
        );
    }
    layout::strip_hairline(out, atlas, strip, theme);
}

// ---- the grid -------------------------------------------------------------

/// The rows under the header.
pub fn body_rect(rect: Viewport, line_height: f32) -> Viewport {
    Viewport {
        y: rect.y + line_height,
        height: (rect.height - line_height).max(0.0),
        ..rect
    }
}

/// How many body records fit whole.
pub fn rows_visible(rect: Viewport, line_height: f32) -> usize {
    if line_height <= 0.0 {
        return 1;
    }
    (body_rect(rect, line_height).height / line_height).max(1.0) as usize
}

/// The document as a grid.
pub fn draw(
    view: &mut TableView,
    rope: &Rope,
    atlas: &mut Atlas,
    rect: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    let cells = Cells::of(atlas);
    view.clamp_scroll(cells, rect);
    let line_height = cells.line_height;
    let gutter = view.gutter_width(cells);
    let body = body_rect(rect, line_height);
    let visible = rows_visible(rect, line_height);
    let edges = view.column_edges(cells);
    // Where the columns start and stop on screen.
    let left = rect.x + gutter;
    let right = rect.x + rect.width;
    let scroll_x = view.scroll_x;
    let on_screen: Vec<usize> = (0..view.widths.len())
        .filter(|c| edges[c + 1] - scroll_x > 0.0 && edges[*c] - scroll_x < right - left)
        .collect();

    // The header's band, the rule under it, and the gutter's.
    layout::push_rect(
        out,
        atlas,
        [rect.x, rect.y],
        [rect.width, line_height],
        theme.tab_bar,
    );
    // The selected cell's row, when it is on screen: its band, under the
    // rules and the text; the cell's own outline goes on last.
    let selected_y = view.selected.and_then(|(row, _)| match row {
        0 => Some(rect.y),
        _ if (view.scroll_row..view.scroll_row + visible).contains(&(row - 1)) => {
            Some(body.y + (row - 1 - view.scroll_row) as f32 * line_height)
        }
        _ => None,
    });
    if let Some(y) = selected_y {
        layout::push_rect(
            out,
            atlas,
            [rect.x, y],
            [rect.width, line_height],
            theme.row_hover,
        );
    }
    layout::push_rect(
        out,
        atlas,
        [rect.x, rect.y + line_height - 1.0],
        [rect.width, 1.0],
        theme.hairline,
    );
    layout::push_rect(
        out,
        atlas,
        [left - 1.0, rect.y],
        [1.0, rect.height],
        theme.hairline,
    );
    // Column rules, one each, the height of the grid.
    for c in on_screen.iter().copied().filter(|c| *c > 0) {
        let x = left + edges[c] - scroll_x;
        if x >= left {
            layout::push_rect(out, atlas, [x, rect.y], [1.0, rect.height], theme.hairline);
        }
    }

    // The body records on screen, and the rule under each.
    let shown: Vec<(usize, f32)> = (0..visible)
        .map(|n| (1 + view.scroll_row + n, body.y + n as f32 * line_height))
        .take_while(|(index, _)| *index < view.table.rows.len())
        .collect();
    for (_, y) in &shown {
        layout::push_rect(
            out,
            atlas,
            [rect.x, y + line_height - 1.0],
            [rect.width, 1.0],
            theme.hairline,
        );
    }

    let mut draw_row = |index: usize, y: f32, colour: [f32; 4], out: &mut Vec<GlyphInstance>| {
        let Some(row) = view.table.rows.get(index) else {
            return;
        };
        let number = (index + 1).to_string();
        layout::push_text(
            out,
            atlas,
            left - (number.len() as f32 + 1.0) * cells.advance,
            y,
            &number,
            theme.gutter_text,
        );
        for c in &on_screen {
            let Some(cell) = row.cells.get(*c) else {
                continue;
            };
            let x = left + edges[*c] - scroll_x + CELL_PAD;
            let text = cell_label(rope, cell, view.widths[*c]);
            let from = out.len();
            layout::push_text(out, atlas, x, y, &text, colour);
            for quad in &mut out[from..] {
                layout::clip_horizontal(quad, left, right);
            }
        }
    };
    draw_row(0, rect.y, theme.status_text, out);
    for (index, y) in shown {
        draw_row(index, y, theme.text, out);
    }

    // The selected cell: a one point outline on the rules around it (the
    // header has none above, and the gutter's is left alone), cut where
    // the cell runs under the gutter or off the right.
    if let (Some(y), Some((row, column))) = (selected_y, view.selected) {
        let x = left + edges[column] - scroll_x;
        let width = edges[column + 1] - edges[column] + 1.0;
        let top = if row == 0 { y } else { y - 1.0 };
        let height = y + line_height - top;
        let from = out.len();
        for (pos, size) in [
            ([x, top], [width, 1.0]),
            ([x, y + line_height - 1.0], [width, 1.0]),
            ([x, top], [1.0, height]),
            ([x + width - 1.0, top], [1.0, height]),
        ] {
            layout::push_rect(out, atlas, pos, size, theme.accent);
        }
        for quad in &mut out[from..] {
            layout::clip_horizontal(quad, left, right);
        }
    }
}

/// A cell's value as drawn: quotes folded, one line, cut to the column.
fn cell_label(rope: &Rope, cell: &Cell, width: usize) -> String {
    let raw = rope.slice_to_string(cell.range.clone());
    let value = Table::cell_text(
        &raw,
        &Cell {
            range: 0..raw.len(),
            quoted: cell.quoted,
        },
    );
    crate::text::columns::fit_columns(one_line(&value), width)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A buffer on a file of its own, so parallel tests never share one.
    fn buffer(name: &str, text: &str) -> Buffer {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("crc-table-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.join(format!("f{n}-{name}"));
        std::fs::write(&path, text).unwrap();
        Buffer::open(path).unwrap()
    }

    const CELLS: Cells = Cells {
        line_height: 19.0,
        advance: 8.0,
    };

    fn rows(n: usize) -> String {
        let mut text = String::from("id,name,note,amount\n");
        for i in 1..n {
            text.push_str(&format!("{i},Person {i},\"says \"\"hi\"\"\",{i}.50\n"));
        }
        text
    }

    #[test]
    fn widths_come_from_the_first_rows_and_stay_in_bounds() {
        let long = "x".repeat(200);
        let text = format!("a,b,c,d\n1,{long},,\n2,\"q\nr\",\"日本\",short\n3\n");
        let view = TableView::parse(&Rope::from_text(&text));
        assert_eq!(view.column_count(), 4);
        assert_eq!(view.widths, vec![MIN_WIDTH, MAX_WIDTH, 4, 5]);
        assert_eq!(view.short_rows, 1);
        assert_eq!(view.body_rows(), 3);
        // Beyond the sample, a wide value does not widen its column.
        let mut text = rows(SAMPLE_ROWS + 5);
        text.push_str(&format!("{},{long},{long},{long}\n", SAMPLE_ROWS + 5));
        let view = TableView::parse(&Rope::from_text(&text));
        assert_eq!(view.widths[1], "Person 204".len());
        assert_eq!(view.widths[2], "says \"hi\"".len());
        // Three digits of row numbers, and two cells of air.
        assert_eq!(view.gutter_width(CELLS), 5.0 * CELLS.advance);
    }

    #[test]
    fn scroll_clamps_at_both_ends() {
        let mut view = TableView::parse(&Rope::from_text(&rows(300)));
        // 19 rows: the header and 18 of the body.
        let rect = Viewport {
            x: 0.0,
            y: 0.0,
            width: 200.0,
            height: 19.0 * 19.0,
        };
        assert_eq!(rows_visible(rect, CELLS.line_height), 18);
        view.scroll_rows(isize::MAX / 2, CELLS, rect);
        assert_eq!(view.scroll_row, 299 - 18);
        view.scroll_rows(-5, CELLS, rect);
        assert_eq!(view.scroll_row, 299 - 23);
        view.scroll_rows(isize::MIN / 2, CELLS, rect);
        assert_eq!(view.scroll_row, 0);
        // Sideways, in points: the grid is wider than the 160 points
        // left of the gutter.
        let grid = view.grid_width(CELLS);
        assert!(grid > 160.0, "{grid}");
        view.scroll_points(10_000.0, CELLS, rect);
        assert_eq!(view.scroll_x, grid - (200.0 - view.gutter_width(CELLS)));
        view.scroll_points(-10_000.0, CELLS, rect);
        assert_eq!(view.scroll_x, 0.0);
        // A grid narrower than the view never scrolls sideways.
        let wide = Viewport {
            width: 2000.0,
            ..rect
        };
        view.scroll_points(50.0, CELLS, wide);
        assert_eq!(view.scroll_x, 0.0);
        // A short file scrolls nowhere.
        let mut small = TableView::parse(&Rope::from_text("a,b\n1,2\n"));
        small.scroll_rows(10, CELLS, rect);
        assert_eq!(small.scroll_row, 0);
    }

    #[test]
    fn cells_are_found_under_the_pointer() {
        let mut view = TableView::parse(&Rope::from_text(&rows(300)));
        let rect = Viewport {
            x: 100.0,
            y: 50.0,
            width: 800.0,
            height: 19.0 * 19.0,
        };
        let gutter = view.gutter_width(CELLS);
        // Column 0 is 3 cells wide plus the padding: 24 + 16 points.
        assert_eq!(
            view.cell_at(CELLS, rect, 100.0 + gutter + 1.0, 55.0),
            Some((0, 0))
        );
        assert_eq!(
            view.cell_at(CELLS, rect, 100.0 + gutter + 41.0, 55.0),
            Some((0, 1))
        );
        assert_eq!(
            view.cell_at(CELLS, rect, 100.0 + gutter + 1.0, 50.0 + 19.0),
            Some((1, 0))
        );
        assert_eq!(
            view.cell_at(CELLS, rect, 100.0 + 2.0, 60.0),
            None,
            "the gutter"
        );
        assert_eq!(
            view.cell_at(CELLS, rect, 850.0, 60.0),
            None,
            "past the last column"
        );
        view.scroll_rows(10, CELLS, rect);
        assert_eq!(
            view.cell_at(CELLS, rect, 100.0 + gutter + 1.0, 50.0 + 19.0),
            Some((11, 0))
        );
        // Scrolled a column to the left, in a view too narrow for the grid.
        let narrow = Viewport {
            width: 200.0,
            ..rect
        };
        view.scroll_points(40.0, CELLS, narrow);
        assert_eq!(view.scroll_x, 40.0);
        assert_eq!(
            view.cell_at(CELLS, narrow, 100.0 + gutter + 1.0, 55.0),
            Some((0, 1))
        );
    }

    #[test]
    fn sync_keeps_the_parse_until_the_text_changes() {
        let mut b = buffer("t.csv", &rows(4));
        let mut tables = HashMap::new();
        sync(&mut tables, &b);
        let view = &tables[&b.id()];
        assert!(view.mode_grid);
        assert_eq!(view.table.rows.len(), 4);
        let parsed = view.table.rows.as_ptr();
        sync(&mut tables, &b);
        assert_eq!(
            tables[&b.id()].table.rows.as_ptr(),
            parsed,
            "the same text: no reparse"
        );
        b.insert("9,Z,z,0\n");
        sync(&mut tables, &b);
        let view = &tables[&b.id()];
        assert_eq!(view.table.rows.len(), 5, "the text changed: reparsed");
        assert!(view.is_current(&b.rope));
        // In text mode the parse waits for the grid to be asked for again.
        tables.get_mut(&b.id()).unwrap().mode_grid = false;
        b.insert("8,Y,y,0\n");
        sync(&mut tables, &b);
        let view = &tables[&b.id()];
        assert_eq!(view.table.rows.len(), 5);
        assert!(!view.is_current(&b.rope));
        assert_eq!(dump(Some(view)), "text");
        tables.get_mut(&b.id()).unwrap().mode_grid = true;
        sync(&mut tables, &b);
        let view = &tables[&b.id()];
        assert_eq!(view.table.rows.len(), 6);
        assert_eq!(dump(Some(view)), "grid rows=6 cols=4 scroll=0 sel=-");
        assert_eq!(dump(None), "none");
        // Not a table file: nothing kept.
        let plain = buffer("notes.txt", "a,b\n1,2\n");
        sync(&mut tables, &plain);
        assert!(!tables.contains_key(&plain.id()));
        let tsv = buffer("t.tsv", "a\tb\n1\t2\n");
        sync(&mut tables, &tsv);
        assert_eq!(tables[&tsv.id()].dialect.delimiter, b'\t');
    }

    /// 19 rows (the header and 18 of the body) by 200 points: 160 of them
    /// right of the gutter, less than the grid's width.
    const RECT: Viewport = Viewport {
        x: 0.0,
        y: 0.0,
        width: 200.0,
        height: 19.0 * 19.0,
    };

    #[test]
    fn the_selection_steps_and_stops_at_the_edges() {
        let mut view = TableView::parse(&Rope::from_text(&rows(300)));
        // Without a selection the keys do nothing here.
        assert!(!view.move_selection(Move::Down, 17, CELLS, RECT));
        assert_eq!(view.selected, None);
        view.select(Some((1, 1)), CELLS, RECT);
        assert!(view.move_selection(Move::Up, 17, CELLS, RECT));
        assert_eq!(view.selected, Some((0, 1)), "up to the header");
        view.move_selection(Move::Up, 17, CELLS, RECT);
        assert_eq!(view.selected, Some((0, 1)), "the header is the top");
        view.move_selection(Move::Left, 17, CELLS, RECT);
        view.move_selection(Move::Left, 17, CELLS, RECT);
        assert_eq!(view.selected, Some((0, 0)));
        view.move_selection(Move::RowEnd, 17, CELLS, RECT);
        assert_eq!(view.selected, Some((0, 3)));
        view.move_selection(Move::Right, 17, CELLS, RECT);
        assert_eq!(view.selected, Some((0, 3)), "the last column is the edge");
        view.move_selection(Move::Last, 17, CELLS, RECT);
        assert_eq!(view.selected, Some((299, 3)));
        view.move_selection(Move::Down, 17, CELLS, RECT);
        view.move_selection(Move::PageDown, 17, CELLS, RECT);
        assert_eq!(view.selected, Some((299, 3)), "the last record is the edge");
        view.move_selection(Move::PageUp, 17, CELLS, RECT);
        assert_eq!(view.selected, Some((282, 3)));
        view.move_selection(Move::RowStart, 17, CELLS, RECT);
        assert_eq!(view.selected, Some((282, 0)));
        view.move_selection(Move::First, 17, CELLS, RECT);
        assert_eq!(view.selected, Some((0, 0)));
        view.move_selection(Move::PageUp, 17, CELLS, RECT);
        assert_eq!(view.selected, Some((0, 0)));
        // A selection past the grid is pulled in.
        view.select(Some((500, 9)), CELLS, RECT);
        assert_eq!(view.selected, Some((299, 3)));
        view.select(None, CELLS, RECT);
        assert_eq!(view.selected, None);
    }

    #[test]
    fn tab_wraps_to_the_next_record_and_stops_at_the_last_cell() {
        let mut view = TableView::parse(&Rope::from_text(&rows(4)));
        view.select(Some((1, 2)), CELLS, RECT);
        view.move_selection(Move::Next, 1, CELLS, RECT);
        assert_eq!(view.selected, Some((1, 3)));
        view.move_selection(Move::Next, 1, CELLS, RECT);
        assert_eq!(view.selected, Some((2, 0)), "on to the next record");
        view.move_selection(Move::Previous, 1, CELLS, RECT);
        assert_eq!(view.selected, Some((1, 3)), "and back");
        view.select(Some((3, 3)), CELLS, RECT);
        view.move_selection(Move::Next, 1, CELLS, RECT);
        assert_eq!(view.selected, Some((3, 3)), "the last cell stays");
        view.select(Some((0, 0)), CELLS, RECT);
        view.move_selection(Move::Previous, 1, CELLS, RECT);
        assert_eq!(view.selected, Some((0, 0)), "so does the first");
    }

    #[test]
    fn the_selection_scrolls_into_view() {
        let mut view = TableView::parse(&Rope::from_text(&rows(300)));
        let edges = view.column_edges(CELLS);
        // Below the 18 body rows on screen: the record becomes the last
        // one shown. Right of the 160 points: the column's end comes to
        // the right edge.
        view.select(Some((30, 3)), CELLS, RECT);
        assert_eq!(view.scroll_row, 30 - 18);
        assert_eq!(view.scroll_x, edges[4] - 160.0);
        // Up and left again: the record becomes the first shown and the
        // column starts at the left edge.
        view.select(Some((5, 1)), CELLS, RECT);
        assert_eq!(view.scroll_row, 4);
        assert_eq!(view.scroll_x, edges[1]);
        // The header is always on screen: no vertical scroll for it.
        view.select(Some((0, 0)), CELLS, RECT);
        assert_eq!(view.scroll_row, 4);
        assert_eq!(view.scroll_x, 0.0);
        // A column wider than the view shows its start.
        let wide = Viewport {
            width: view.gutter_width(CELLS) + 20.0,
            ..RECT
        };
        view.select(Some((0, 2)), CELLS, wide);
        assert_eq!(view.scroll_x, edges[2]);
        // A step from the keys scrolls too.
        view.select(Some((1, 0)), CELLS, RECT);
        assert_eq!(view.scroll_row, 0);
        view.select(Some((18, 0)), CELLS, RECT);
        assert_eq!(view.scroll_row, 0, "the last row that fits");
        view.move_selection(Move::Down, 17, CELLS, RECT);
        assert_eq!(view.selected, Some((19, 0)));
        assert_eq!(view.scroll_row, 1);
        view.move_selection(Move::PageDown, 17, CELLS, RECT);
        assert_eq!(view.selected, Some((36, 0)));
        assert_eq!(view.scroll_row, 18);
        // The first record is the header, which is always on screen: the
        // body goes back to the top all the same.
        view.move_selection(Move::First, 17, CELLS, RECT);
        assert_eq!(view.selected, Some((0, 0)));
        assert_eq!(view.scroll_row, 0);
    }

    #[test]
    fn a_press_in_the_gutter_selects_the_record() {
        let view = TableView::parse(&Rope::from_text(&rows(300)));
        let rect = Viewport {
            x: 100.0,
            y: 50.0,
            ..RECT
        };
        let gutter = view.gutter_width(CELLS);
        assert_eq!(view.cell_for_press(CELLS, rect, 102.0, 55.0), Some((0, 0)));
        assert_eq!(
            view.cell_for_press(CELLS, rect, 102.0, 50.0 + 19.0 * 3.0 + 1.0),
            Some((3, 0))
        );
        assert_eq!(
            view.cell_for_press(CELLS, rect, 100.0 + gutter + 41.0, 55.0),
            Some((0, 1))
        );
        assert_eq!(view.cell_for_press(CELLS, rect, 102.0, 40.0), None);
        assert_eq!(
            view.cell_for_press(CELLS, rect, 102.0, 50.0 + 19.0 * 19.0),
            None
        );
        // Below the last record, in a short file.
        let small = TableView::parse(&Rope::from_text("a,b\n1,2\n"));
        assert_eq!(
            small.cell_for_press(CELLS, rect, 102.0, 50.0 + 19.0 * 5.0),
            None
        );
    }

    #[test]
    fn the_selection_survives_a_reparse_and_is_clamped_to_what_is_left() {
        let mut b = buffer("sel.csv", &rows(6));
        let mut tables = HashMap::new();
        sync(&mut tables, &b);
        tables
            .get_mut(&b.id())
            .unwrap()
            .select(Some((5, 3)), CELLS, RECT);
        // Text changed, the cell still there: the selection stays.
        b.insert("9,Z,z,0\n");
        sync(&mut tables, &b);
        assert_eq!(tables[&b.id()].selected, Some((5, 3)));
        // Rows and a column gone: the selection moves in.
        b.rope = Rope::from_text("a,b\n1,2\n3,4\n");
        sync(&mut tables, &b);
        assert_eq!(tables[&b.id()].selected, Some((2, 1)));
        assert_eq!(
            dump(Some(&tables[&b.id()])),
            "grid rows=3 cols=2 scroll=0 sel=2,1"
        );
        // Text mode and back keeps it.
        tables.get_mut(&b.id()).unwrap().mode_grid = false;
        sync(&mut tables, &b);
        tables.get_mut(&b.id()).unwrap().mode_grid = true;
        sync(&mut tables, &b);
        assert_eq!(tables[&b.id()].selected, Some((2, 1)));
        // Nothing left: nothing selected.
        b.rope = Rope::from_text("");
        sync(&mut tables, &b);
        assert_eq!(tables[&b.id()].selected, None);
    }

    #[test]
    fn cell_labels_fold_quotes_and_line_breaks() {
        let text = "a,b\n\"says \"\"hi\"\"\",\"one\r\ntwo\tthree\"\n";
        let rope = Rope::from_text(text);
        let table = csv::parse(text, &Dialect::detect(text));
        assert_eq!(
            cell_label(&rope, table.cell(1, 0).unwrap(), 48),
            "says \"hi\""
        );
        assert_eq!(
            cell_label(&rope, table.cell(1, 1).unwrap(), 48),
            "one two three"
        );
        assert_eq!(cell_label(&rope, table.cell(1, 1).unwrap(), 5), "one t");
    }
}
