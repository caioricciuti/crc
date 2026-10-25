//! Moving the caret by character, word and row, folding, and scrolling.

use super::*;

impl Buffer {
    /// After the primary caret was put somewhere: a click, a found match.
    /// A plain one is how you get back to one cursor.
    pub(super) fn after_move(&mut self, motion: Motion) {
        if motion == Motion::Move {
            self.anchor = self.cursor;
            self.extra.clear();
        }
        // Any movement ends an edit run for undo purposes.
        self.last_edit = None;
        self.reveal_cursors();
    }
    /// Moves every cursor: `to` takes one's `(anchor, head)` and goal
    /// column and gives where its caret goes and the goal it keeps. A plain
    /// motion leaves each a bare caret there, Shift keeps each anchor and so
    /// extends every selection. Cursors that meet become one.
    pub(super) fn move_all(
        &mut self,
        motion: Motion,
        to: impl Fn(&Self, (usize, usize), Option<usize>) -> (usize, Option<usize>),
    ) {
        (self.cursor, self.goal_column) = to(self, (self.anchor, self.cursor), self.goal_column);
        if !self.extra.is_empty() {
            let goals = self.extra_goals();
            let moved: Vec<_> = self
                .extra
                .iter()
                .zip(goals)
                .map(|(&(anchor, head), goal)| (anchor, to(self, (anchor, head), goal)))
                .collect();
            self.extra = moved.iter().map(|&(a, (h, _))| (a, h)).collect();
            self.extra_goals = moved.into_iter().map(|(_, (_, goal))| goal).collect();
        }
        if motion == Motion::Move {
            self.anchor = self.cursor;
            for cursor in &mut self.extra {
                cursor.0 = cursor.1;
            }
        }
        self.merge_cursors();
        self.last_edit = None;
        self.reveal_cursors();
    }
    /// Opens every fold that hides a cursor. Go to line, find, go to
    /// definition and a click can all land inside a fold, and a caret on a
    /// hidden line has no screen row to move from.
    pub(super) fn reveal_cursors(&mut self) {
        if self.folds.is_empty() {
            return;
        }
        let lines: Vec<usize> = std::iter::once(self.cursor)
            .chain(self.extra.iter().map(|(_, head)| *head))
            .map(|at| self.rope.byte_to_line(at))
            .collect();
        for line in lines {
            if self.is_hidden(line) {
                self.unfold(line);
            }
        }
    }
    pub fn move_left(&mut self, motion: Motion) {
        // Plain left with a selection collapses to its start, which is what
        // every other editor does.
        self.move_all(motion, |b, (anchor, head), _| {
            if motion == Motion::Move && anchor != head {
                (anchor.min(head), None)
            } else {
                (b.prev_boundary(head), None)
            }
        });
    }
    pub fn move_right(&mut self, motion: Motion) {
        self.move_all(motion, |b, (anchor, head), _| {
            if motion == Motion::Move && anchor != head {
                (anchor.max(head), None)
            } else {
                (b.next_boundary(head), None)
            }
        });
    }
    pub fn move_up(&mut self, motion: Motion) {
        self.move_vertically(-1, motion);
    }
    pub fn move_down(&mut self, motion: Motion) {
        self.move_vertically(1, motion);
    }
    /// Up or down a line, or a screen row while wrapping or folded, at
    /// every cursor, each aiming for its own goal column.
    fn move_vertically(&mut self, delta: isize, motion: Motion) {
        if self.row_mode() {
            // A caret on a hidden line has no row to move from.
            self.reveal_cursors();
            self.move_all(motion, |b, (_, head), goal| b.row_step(head, goal, delta));
        } else {
            self.move_all(motion, |b, (_, head), goal| b.line_step(head, goal, delta));
        }
    }
    /// Where Up (`delta` -1) or Down (1) takes a caret at `at` aiming for
    /// `goal`, by lines: to the start of the text from the first line and
    /// to its end from the last, as everywhere on macOS.
    fn line_step(&self, at: usize, goal: Option<usize>, delta: isize) -> (usize, Option<usize>) {
        let (line, column) = self.visual_position(at);
        if delta < 0 && line == 0 {
            return (0, goal);
        }
        if delta > 0 && line + 1 >= self.rope.len_lines() {
            return (self.rope.len_bytes(), goal);
        }
        let goal = goal.unwrap_or(column);
        let target = if delta < 0 { line - 1 } else { line + 1 };
        (self.byte_at_visual(target, goal), Some(goal))
    }
    /// The line of `at` and its column on screen: tabs to their stops,
    /// wide characters as two. The goal column of Up and Down is kept in
    /// these, as it is while wrapping, so the caret does not zig-zag past
    /// tabs and CJK and does the same with wrapping on or off.
    pub(super) fn visual_position(&self, at: usize) -> (usize, usize) {
        let line = self.rope.byte_to_line(at);
        let start = self.rope.line_to_byte(line);
        (line, wrap::column_in_row(&self.rope, start, at))
    }
    pub(super) fn byte_at_visual(&self, line: usize, column: usize) -> usize {
        let start = self.rope.line_to_byte(line);
        let end = wrap::line_end(&self.rope, line);
        wrap::byte_at_column(&self.rope, start, end, column)
    }
    /// Whether screen rows differ from lines: wrapping, or lines folded away.
    pub fn row_mode(&self) -> bool {
        self.wrap.is_some() || !self.folds.is_empty()
    }
    /// Whether `line` is folded away.
    pub fn is_hidden(&self, line: usize) -> bool {
        let at = self.folds.partition_point(|(_, b)| *b < line);
        self.folds.get(at).is_some_and(|(a, _)| *a <= line)
    }
    /// The last line of the fold hiding `line`, if one does.
    pub fn hidden_until(&self, line: usize) -> Option<usize> {
        let at = self.folds.partition_point(|(_, b)| *b < line);
        self.folds
            .get(at)
            .filter(|(a, _)| *a <= line)
            .map(|(_, b)| *b)
    }
    /// Leading whitespace of `line` in columns, or `None` for a blank line.
    pub fn indent_columns(&self, line: usize) -> Option<usize> {
        let start = self.rope.line_to_byte(line);
        let end = self.line_end(line).min(start + 1024);
        let mut column = 0;
        for chunk in self.rope.chunks_in(start..end) {
            for ch in chunk.chars() {
                match ch {
                    ' ' | '\t' => column = crate::text::columns::advance(column, ch),
                    '\r' => {}
                    _ => return Some(column),
                }
            }
        }
        None
    }
    /// Whether the next non-blank line after `line` is indented deeper:
    /// the cheap test for a fold, run for every visible line.
    pub fn can_fold(&self, line: usize) -> bool {
        let Some(base) = self.indent_columns(line) else {
            return false;
        };
        let total = self.rope.len_lines();
        (line + 1..total.min(line + 64))
            .find_map(|l| self.indent_columns(l))
            .is_some_and(|next| next > base)
    }
    /// The lines folding `line` hides: everything below it indented deeper,
    /// down to the last such non-blank line. A closing bracket at the
    /// block's own indent stays visible, as do blank lines after the block.
    pub fn fold_range(&self, line: usize) -> Option<(usize, usize)> {
        if !self.can_fold(line) {
            return None;
        }
        let base = self.indent_columns(line)?;
        let mut last = line;
        // A block longer than this (a whole generated JSON under its first
        // brace) is not worth the scan on the main thread to fold.
        let limit = self.rope.len_lines().min(line + 1 + FOLD_ALL_MAX_LINES);
        for l in line + 1..limit {
            if l + 1 == limit && limit < self.rope.len_lines() {
                return None;
            }
            match self.indent_columns(l) {
                Some(indent) if indent <= base => break,
                Some(_) => last = l,
                None => {}
            }
        }
        (last > line).then_some((line + 1, last))
    }
    /// Folds the block that starts at `line`. The caret, if it was inside,
    /// moves to the end of `line`.
    pub fn fold(&mut self, line: usize) -> bool {
        let Some((a, b)) = self.fold_range(line) else {
            return false;
        };
        // A fold swallows any inside it.
        self.folds.retain(|(x, y)| !(*x >= a && *y <= b));
        let at = self.folds.partition_point(|(x, _)| *x < a);
        self.folds.insert(at, (a, b));
        let caret = self.rope.byte_to_line(self.cursor);
        if (a..=b).contains(&caret) {
            self.cursor = self.line_end(line);
            self.anchor = self.cursor;
            self.extra.clear();
        }
        true
    }
    /// Opens the fold that `line` heads, or the one hiding `line`.
    pub fn unfold(&mut self, line: usize) -> bool {
        let before = self.folds.len();
        self.folds
            .retain(|(a, b)| *a != line + 1 && !(*a <= line && line <= *b));
        before != self.folds.len()
    }
    /// Whether `line` heads a fold that is closed.
    pub fn is_folded_at(&self, line: usize) -> bool {
        self.folds
            .binary_search_by_key(&(line + 1), |(a, _)| *a)
            .is_ok()
    }
    /// Folds every top-level block whose lines are indented deeper. Refused
    /// past [`FOLD_ALL_MAX_LINES`]: reading every line's indent on the main
    /// thread would stall the window for seconds.
    pub fn fold_all(&mut self) -> bool {
        if self.rope.len_lines() > FOLD_ALL_MAX_LINES {
            return false;
        }
        let caret = self.rope.byte_to_line(self.cursor);
        let mut line = 0;
        let total = self.rope.len_lines();
        let mut folds = Vec::new();
        while line < total {
            match self.fold_range(line) {
                Some((a, b)) => {
                    folds.push((a, b));
                    line = b + 1;
                }
                None => line += 1,
            }
        }
        self.folds = folds;
        if self.is_hidden(caret) {
            let at = self.folds.partition_point(|(_, b)| *b < caret);
            let head = self.folds[at].0 - 1;
            self.cursor = self.line_end(head);
            self.anchor = self.cursor;
        }
        true
    }
    /// Where each screen row of `line` starts: one row unless wrapping,
    /// none when the line is folded away.
    pub fn row_starts(&self, line: usize) -> Vec<usize> {
        if !self.folds.is_empty() && self.is_hidden(line) {
            return Vec::new();
        }
        match self.wrap {
            Some(columns) => wrap::row_starts(&self.rope, line, columns),
            None => vec![self.rope.line_to_byte(line)],
        }
    }
    /// The caret's line and its row within that line.
    pub fn cursor_row(&self) -> (usize, usize) {
        let line = self.rope.byte_to_line(self.cursor);
        (line, wrap::row_of(&self.row_starts(line), self.cursor))
    }
    /// Where one screen row up or down takes a caret at `at` while
    /// wrapping or folded, keeping `goal` measured from the start of the
    /// row. The caret's line must not be folded away.
    fn row_step(&self, at: usize, goal: Option<usize>, delta: isize) -> (usize, Option<usize>) {
        let line = self.rope.byte_to_line(at);
        let starts = self.row_starts(line);
        let row = wrap::row_of(&starts, at);
        let Some(&row_start) = starts.get(row) else {
            return (at, goal);
        };
        let column = wrap::column_in_row(&self.rope, row_start, at);
        let (target_line, target_row) = self.step_rows((line, row), delta);
        if (target_line, target_row) == (line, row) {
            return (if delta < 0 { 0 } else { self.rope.len_bytes() }, goal);
        }
        let goal = goal.unwrap_or(column);
        let starts = if target_line == line {
            starts
        } else {
            self.row_starts(target_line)
        };
        let start = starts[target_row];
        let end = starts
            .get(target_row + 1)
            .copied()
            .unwrap_or_else(|| wrap::line_end(&self.rope, target_line));
        let mut at = wrap::byte_at_column(&self.rope, start, end, goal);
        // The end of a row that continues is the next row's start: stop
        // before it, or the caret would jump down a row.
        if target_row + 1 < starts.len() && at == end {
            at = self.prev_boundary(end).max(start);
        }
        (at, Some(goal))
    }
    /// `delta` screen rows from `(line, row)`, stopping at either end of the
    /// document.
    pub fn step_rows(
        &self,
        (mut line, mut row): (usize, usize),
        mut delta: isize,
    ) -> (usize, usize) {
        let total = self.rope.len_lines();
        if !self.row_mode() {
            let line = (line as isize + delta).clamp(0, total.saturating_sub(1) as isize);
            return (line as usize, 0);
        }
        // The next line with rows, from `line` in `step` direction.
        let shown = |mut l: isize, step: isize| -> Option<(usize, usize)> {
            while l >= 0 && (l as usize) < total {
                let count = self.row_starts(l as usize).len();
                if count > 0 {
                    return Some((l as usize, count));
                }
                l += step;
            }
            None
        };
        let Some((start, count)) = shown(line as isize, -1).or_else(|| shown(line as isize, 1))
        else {
            return (0, 0);
        };
        if start != line {
            (line, row) = (start, count - 1);
        }
        while delta > 0 {
            let count = self.row_starts(line).len().max(1);
            let left = (count - 1 - row.min(count - 1)) as isize;
            if delta <= left {
                row += delta as usize;
                delta = 0;
            } else if let Some((next, _)) = shown(line as isize + 1, 1) {
                delta -= left + 1;
                line = next;
                row = 0;
            } else {
                row = count - 1;
                delta = 0;
            }
        }
        while delta < 0 {
            if (-delta) as usize <= row {
                row -= (-delta) as usize;
                delta = 0;
            } else if let Some((previous, count)) =
                shown(line as isize - 1, -1).filter(|_| line > 0)
            {
                delta += row as isize + 1;
                line = previous;
                row = count - 1;
            } else {
                row = 0;
                delta = 0;
            }
        }
        (line, row)
    }
    /// Whether a view `rows` tall starting at `line` could reach past the
    /// end, which is when [`Buffer::max_scroll_row`] is worth its cost:
    /// every visible line has at least one row, so only the last `rows`
    /// lines can. Folded lines have none, so with folds it always could.
    pub(super) fn near_end(&self, line: usize, rows: usize) -> bool {
        !self.folds.is_empty() || line + rows.max(1) >= self.rope.len_lines()
    }
    /// The furthest scroll position, with the last row at the bottom of a
    /// view `rows` tall.
    pub(super) fn max_scroll_row(&self, rows: usize) -> (usize, usize) {
        let last = self.rope.len_lines().saturating_sub(1);
        let end = (last, self.row_starts(last).len().saturating_sub(1));
        // step_rows first finds the last line with rows, if `last` has none.
        let end = self.step_rows(end, 0);
        self.step_rows(end, -(rows.max(1) as isize - 1))
    }
    pub fn move_line_start(&mut self, motion: Motion) {
        self.move_all(motion, |b, (_, head), _| {
            (b.rope.line_to_byte(b.rope.byte_to_line(head)), None)
        });
    }
    pub fn move_line_end(&mut self, motion: Motion) {
        self.move_all(motion, |b, (_, head), _| {
            (b.line_end(b.rope.byte_to_line(head)), None)
        });
    }
    /// Every cursor goes, and so they all become one, unless Shift keeps
    /// their anchors apart.
    pub fn move_buffer_start(&mut self, motion: Motion) {
        self.move_all(motion, |_, _, _| (0, None));
    }
    pub fn move_buffer_end(&mut self, motion: Motion) {
        self.move_all(motion, |b, _, _| (b.rope.len_bytes(), None));
    }

    // ---- scrolling -------------------------------------------------------
    /// [`Self::scroll_to_cursor`], then on down so the cursor is not in
    /// the top `covered` rows either: under the find bar's panel there.
    pub fn scroll_to_cursor_below(&mut self, rows: usize, cols: usize, covered: usize) {
        self.scroll_to_cursor(rows, cols);
        if covered == 0 || covered >= rows {
            return;
        }
        let top = (self.scroll_line, self.scroll_row);
        let caret = if self.row_mode() {
            self.cursor_row()
        } else {
            (self.cursor_position().0, 0)
        };
        if self.step_rows(top, covered as isize) > caret {
            (self.scroll_line, self.scroll_row) = self.step_rows(caret, -(covered as isize));
            self.scroll_fraction = 0.0;
        }
    }

    /// Scrolls so the cursor is visible, given a viewport of `rows` lines and
    /// `cols` columns.
    ///
    /// `cols` of zero means "do not track horizontally", which is what the
    /// callers that only know the row count pass.
    pub fn scroll_to_cursor(&mut self, rows: usize, cols: usize) {
        if self.row_mode() {
            if self.wrap.is_some() {
                self.scroll_column = 0;
                self.scroll_x_fraction = 0.0;
            }
            let caret = self.cursor_row();
            let top = (self.scroll_line, self.scroll_row);
            if caret < top || (caret == top && self.scroll_fraction > 0.0) {
                (self.scroll_line, self.scroll_row) = caret;
                self.scroll_fraction = 0.0;
            } else if rows > 0 && self.step_rows(top, rows as isize - 1) < caret {
                (self.scroll_line, self.scroll_row) = self.step_rows(caret, -(rows as isize - 1));
                self.scroll_fraction = 0.0;
            }
            return;
        }
        let (line, column) = self.cursor_position();
        // The top line is only partly in view while a fraction is scrolled
        // off, so a caret on it counts as above the view.
        if line < self.scroll_line || (line == self.scroll_line && self.scroll_fraction > 0.0) {
            self.scroll_line = line;
            self.scroll_fraction = 0.0;
        } else if rows > 0 && line >= self.scroll_line + rows {
            self.scroll_line = line + 1 - rows;
            self.scroll_fraction = 0.0;
        }

        if cols == 0 {
            return;
        }
        // A few columns of lead, so the caret is not pinned to the very edge
        // while you type toward it.
        const MARGIN: usize = 4;
        if column < self.scroll_column + MARGIN {
            self.scroll_column = column.saturating_sub(MARGIN);
            self.scroll_x_fraction = 0.0;
        } else if column >= self.scroll_column + cols {
            self.scroll_column = column + 1 + MARGIN - cols;
            self.scroll_x_fraction = 0.0;
        }
    }
    /// How far the view is scrolled sideways, in columns: the whole ones
    /// and the part a trackpad has moved past them. Everything that places
    /// text by its column subtracts this times the advance.
    pub fn scroll_x(&self) -> f32 {
        self.scroll_column as f32 + self.scroll_x_fraction
    }
    /// Scrolls horizontally, clamped so you cannot drift off into empty
    /// space to the right of the longest visible line. A fraction left by a
    /// trackpad stays put, except where the clamp stops the scroll.
    ///
    /// The bound is the longest line *in view*, not in the document: finding
    /// the longest line of a 100MB file is a full scan, and the answer would
    /// only be used to stop a gesture.
    pub fn scroll_columns_by(&mut self, columns: isize, rows: usize) {
        if columns == 0 {
            return;
        }
        let longest = self.longest_visible_line(rows);
        let next = self.scroll_column as isize + columns;
        self.scroll_column = next.clamp(0, longest as isize) as usize;
        if next <= 0 || next >= longest as isize {
            self.scroll_x_fraction = 0.0;
        }
    }
    /// Scrolls by a part of a column, for a trackpad: `columns` is the
    /// gesture's points over the advance. Clamped to the same range as
    /// [`Buffer::scroll_columns_by`].
    pub fn scroll_columns_smooth_by(&mut self, columns: f32, rows: usize) {
        if !columns.is_finite() || columns == 0.0 {
            return;
        }
        let longest = self.longest_visible_line(rows) as f64;
        let at = (self.scroll_column as f64 + self.scroll_x_fraction as f64 + columns as f64)
            .clamp(0.0, longest);
        self.scroll_column = at.floor() as usize;
        self.scroll_x_fraction = (at - at.floor()) as f32;
    }
    /// Pulls the scroll position back inside what there is to show.
    ///
    /// Scrolling clamps itself, but the limits move without any scrolling:
    /// the view gets taller, lines are deleted, a vertical scroll leaves the
    /// long line that a horizontal one was measured against. Called every
    /// frame with that frame's size.
    pub fn clamp_scroll(&mut self, rows: usize, cols: usize) {
        if self.row_mode() {
            if self.wrap.is_some() {
                self.scroll_column = 0;
                self.scroll_x_fraction = 0.0;
            }
            // A top line folded away: the view starts at the fold's head.
            let mut top = self
                .scroll_line
                .min(self.rope.len_lines().saturating_sub(1));
            while top > 0 && self.row_starts(top).is_empty() {
                top -= 1;
                self.scroll_row = usize::MAX;
            }
            self.scroll_line = top;
            let count = self.row_starts(top).len().max(1);
            self.scroll_row = self.scroll_row.min(count - 1);
            self.scroll_by(0, rows);
            return;
        }
        self.scroll_row = 0;
        self.scroll_by(0, rows);
        if self.scroll_column == 0 && self.scroll_x_fraction == 0.0 {
            return;
        }
        // Keep the end of the longest visible line reachable, and no more:
        // further right than this the whole view is blank.
        let longest = self.longest_visible_line(rows);
        let reach = longest.saturating_sub(cols.saturating_sub(1).min(longest));
        // The caret is allowed to hold the view out there, since typing past
        // the right edge is how it got there.
        let caret = self.cursor_position().1;
        let limit = reach.max(caret.saturating_sub(cols.saturating_sub(1)));
        if self.scroll_column > limit
            || (self.scroll_column == limit && self.scroll_x_fraction > 0.0)
        {
            self.scroll_column = limit;
            self.scroll_x_fraction = 0.0;
        }
    }
    /// Characters in the longest line of the current viewport.
    pub(super) fn longest_visible_line(&self, rows: usize) -> usize {
        let total = self.rope.len_lines();
        let first = self.scroll_line.min(total.saturating_sub(1));
        let last = (first + rows.max(1)).min(total);
        (first..last)
            .map(|line| {
                let start = self.rope.line_to_byte(line);
                let end = self.line_end(line);
                self.rope.byte_to_char(end) - self.rope.byte_to_char(start)
            })
            .max()
            .unwrap_or(0)
    }
    /// Scrolls by whole lines. A fraction left by a trackpad stays put, so a
    /// Page Down in the middle of a gesture does not snap the view; at the
    /// last position there is none, since nothing is below to reveal.
    pub fn scroll_by(&mut self, lines: isize, rows: usize) {
        if self.row_mode() {
            let mut at = self.step_rows((self.scroll_line, self.scroll_row), lines);
            if self.near_end(at.0, rows) {
                let max = self.max_scroll_row(rows);
                at = at.min(max);
                if at >= max {
                    self.scroll_fraction = 0.0;
                }
            }
            (self.scroll_line, self.scroll_row) = at;
            return;
        }
        let max = self.rope.len_lines().saturating_sub(rows.max(1));
        let next = self.scroll_line as isize + lines;
        self.scroll_line = next.clamp(0, max as isize) as usize;
        if self.scroll_line >= max {
            self.scroll_fraction = 0.0;
        }
    }
    /// Scrolls by a part of a line, for a trackpad: `lines` is the gesture's
    /// points over the line height. Clamped to the same range as
    /// [`Buffer::scroll_by`].
    pub fn scroll_smooth_by(&mut self, lines: f32, rows: usize) {
        if !lines.is_finite() {
            return;
        }
        if self.row_mode() {
            // Rows here, not lines: a wrapped paragraph scrolls row by row.
            let total = self.scroll_fraction as f64 + lines as f64;
            let whole = total.floor();
            let from = (self.scroll_line, self.scroll_row);
            let mut to = self.step_rows(from, whole as isize);
            let max = if self.near_end(to.0, rows) {
                self.max_scroll_row(rows)
            } else {
                (usize::MAX, usize::MAX)
            };
            to = to.min(max);
            (self.scroll_line, self.scroll_row) = to;
            self.scroll_fraction = (total - whole) as f32;
            // Stopped short at either end: nothing more to reveal.
            let short_of_top =
                whole < 0.0 && to == (0, 0) && self.step_rows(to, -whole as isize) != from;
            if to >= max || short_of_top {
                self.scroll_fraction = 0.0;
            }
            return;
        }
        let max = self.rope.len_lines().saturating_sub(rows.max(1)) as f64;
        let at =
            (self.scroll_line as f64 + self.scroll_fraction as f64 + lines as f64).clamp(0.0, max);
        self.scroll_line = at.floor() as usize;
        self.scroll_fraction = (at - at.floor()) as f32;
    }
    /// Puts `line` at the top of a view `rows` tall, as far as the text allows.
    pub fn scroll_to(&mut self, line: usize, rows: usize) {
        self.scroll_row = 0;
        if self.row_mode() {
            let max = self.max_scroll_row(rows);
            (self.scroll_line, self.scroll_row) = (line, 0).min(max);
            self.scroll_fraction = 0.0;
            return;
        }
        let max = self.rope.len_lines().saturating_sub(rows.max(1));
        self.scroll_line = line.min(max);
        self.scroll_fraction = 0.0;
    }

    // ---- internals -------------------------------------------------------
}
