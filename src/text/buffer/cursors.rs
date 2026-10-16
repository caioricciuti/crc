//! Several carets at once: adding them, and editing at all of them.

use super::*;

/// One cursor's edit: the range it replaces, the text it puts there, and
/// how far into that text its caret lands.
pub(super) type CursorEdit = (std::ops::Range<usize>, String, usize);

/// A corner of a box selection: a screen row, as its line and its row
/// within that line (0 unless wrapping), and a column on that row in
/// display columns, tabs to their stops and wide characters as two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoxCorner {
    pub line: usize,
    pub row: usize,
    pub column: usize,
}

fn changes_text((range, text, _): &CursorEdit) -> bool {
    !range.is_empty() || !text.is_empty()
}

impl Buffer {
    /// How many cursors are active, including the primary.
    pub fn cursor_count(&self) -> usize {
        self.extra.len() + 1
    }
    /// Every selection as a normalised `(start, end)`, ascending, with
    /// overlaps merged.
    ///
    /// Merging matters: two cursors that have grown into each other must
    /// become one, or an edit would be applied twice to the same text.
    /// Selections that merely touch stay apart, since `ab` selected twice in
    /// `abab` is two selections, but a bare caret on the edge of a selection
    /// has nothing of its own to edit and folds into it.
    pub(super) fn all_selections(&self) -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = std::iter::once((self.anchor, self.cursor))
            .chain(self.extra.iter().copied())
            .map(|(a, h)| (a.min(h), a.max(h)))
            .collect();
        out.sort_by_key(|r| r.0);

        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(out.len());
        for range in out {
            match merged.last_mut() {
                Some(last)
                    if range.0 < last.1
                        || (range.0 == last.1 && (range.0 == range.1 || last.0 == last.1)) =>
                {
                    last.1 = last.1.max(range.1)
                }
                _ => merged.push(range),
            }
        }
        merged
    }
    /// Every selection as `(start, end)`, ascending and merged. Public so
    /// the renderer can draw all of them.
    pub fn selections(&self) -> Vec<(usize, usize)> {
        self.all_selections()
    }
    /// Every caret position, ascending. Empty selections included.
    pub fn caret_positions(&self) -> Vec<usize> {
        let mut out: Vec<usize> = std::iter::once(self.cursor)
            .chain(self.extra.iter().map(|&(_, h)| h))
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }
    /// Adds a cursor, ignoring one that duplicates an existing position.
    pub fn add_cursor(&mut self, anchor: usize, head: usize) {
        let (anchor, head) = (self.char_floor(anchor), self.char_floor(head));
        if (self.anchor, self.cursor) == (anchor, head) || self.extra.contains(&(anchor, head)) {
            return;
        }
        self.extra.push((anchor, head));
        self.extra_goals.clear();
    }
    /// Drops every cursor but the primary.
    pub fn collapse_cursors(&mut self) -> bool {
        if self.extra.is_empty() {
            return false;
        }
        self.extra.clear();
        self.extra_goals.clear();
        true
    }
    /// The goal column of each extra cursor, in the order of `extra`.
    pub(super) fn extra_goals(&self) -> Vec<Option<usize>> {
        if self.extra_goals.len() == self.extra.len() {
            self.extra_goals.clone()
        } else {
            vec![None; self.extra.len()]
        }
    }
    /// Folds cursors that a motion brought together into one, by the rule
    /// of [`Buffer::all_selections`]: a caret on another, or selections
    /// that overlap. What they become keeps the primary's direction and
    /// goal when the primary is one of them, and stays the primary.
    pub(super) fn merge_cursors(&mut self) {
        if self.extra.is_empty() {
            return;
        }
        // (anchor, head, goal, is the primary)
        type Cursor = (usize, usize, Option<usize>, bool);
        let mut all: Vec<Cursor> =
            std::iter::once((self.anchor, self.cursor, self.goal_column, true))
                .chain(
                    self.extra
                        .iter()
                        .zip(self.extra_goals())
                        .map(|(&(a, h), goal)| (a, h, goal, false)),
                )
                .collect();
        let count = all.len();
        all.sort_by_key(|c| c.0.min(c.1));
        let span = |c: &Cursor| (c.0.min(c.1), c.0.max(c.1));
        let mut merged: Vec<Cursor> = Vec::with_capacity(count);
        for next in all {
            let (start, end) = span(&next);
            match merged.last_mut() {
                Some(last)
                    if start < span(last).1
                        || (start == span(last).1 && (start == end || span(last).0 == start)) =>
                {
                    // The primary decides, else whichever is a selection.
                    let keep = if last.3 || (!next.3 && next.0 == next.1) {
                        *last
                    } else {
                        next
                    };
                    let (from, to) = (span(last).0, span(last).1.max(end));
                    let primary = last.3 || next.3;
                    *last = if keep.1 < keep.0 {
                        (to, from, keep.2, primary)
                    } else {
                        (from, to, keep.2, primary)
                    };
                }
                _ => merged.push(next),
            }
        }
        if merged.len() == count {
            return;
        }
        self.extra.clear();
        self.extra_goals.clear();
        for (anchor, head, goal, primary) in merged {
            if primary {
                (self.anchor, self.cursor, self.goal_column) = (anchor, head, goal);
            } else {
                self.extra.push((anchor, head));
                self.extra_goals.push(goal);
            }
        }
    }
    /// What Copy puts on the clipboard: the selection, or with several
    /// cursors every selection in order, a line each. Bare carets add
    /// nothing. `None` when nothing is selected anywhere.
    pub fn copied_text(&self) -> Option<String> {
        if self.extra.is_empty() {
            return self.selected_text();
        }
        let parts: Vec<String> = self
            .all_selections()
            .into_iter()
            .filter(|(start, end)| end > start)
            .map(|(start, end)| self.rope.slice_to_string(start..end))
            .collect();
        (!parts.is_empty()).then(|| parts.join("\n"))
    }
    /// Whether any cursor has a selection, which is what Copy and Cut need.
    pub fn has_selection(&self) -> bool {
        self.selection().is_some() || self.extra.iter().any(|(anchor, head)| anchor != head)
    }
    /// Cut: [`Buffer::copied_text`], and every selection deleted as one
    /// undo step. Bare carets stay where they are.
    pub fn cut(&mut self) -> Option<String> {
        let text = self.copied_text()?;
        if self.extra.is_empty() {
            self.backspace();
        } else if !self.is_locked() {
            self.last_edit = None;
            self.edit_each_cursor(EditKind::Delete, |_, start, end| {
                (end > start).then(|| (start..end, String::new(), 0))
            });
            self.last_edit = None;
        }
        Some(text)
    }
    /// Paste: with as many lines as cursors, one line at each in order,
    /// the way a multi-cursor copy came out; otherwise all of it at every
    /// cursor. A trailing line break does not count as a line.
    pub fn paste(&mut self, text: &str) {
        let text = normalize_newlines(text);
        if self.extra.is_empty() || self.is_locked() {
            return self.insert(&text);
        }
        let selections = self.all_selections();
        let lines: Vec<&str> = text
            .strip_suffix('\n')
            .unwrap_or(&text)
            .split('\n')
            .collect();
        if lines.len() != selections.len() {
            return self.insert(&text);
        }
        self.last_edit = None;
        self.edit_each_cursor(EditKind::Insert, |_, start, end| {
            let line = lines[selections.binary_search_by_key(&start, |s| s.0).ok()?];
            Some((start..end, line.to_owned(), line.len()))
        });
        self.last_edit = None;
    }
    /// Cmd-D: selects the word at the cursor, or adds the next occurrence of
    /// the current selection as another cursor.
    pub fn select_next_occurrence(&mut self) -> bool {
        let Some(range) = self.selection() else {
            // Nothing selected yet: select the word under the caret, which is
            // what makes the first press useful. A caret at the end of a
            // word still means that word, not the delimiter after it.
            let is_word = |c: Option<char>| c.is_some_and(|c| class_of(c) == CharClass::Word);
            let start =
                if !is_word(self.char_at(self.cursor)) && is_word(self.char_before(self.cursor)) {
                    self.prev_word_boundary(self.cursor)
                } else {
                    self.prev_word_boundary(self.next_boundary(self.cursor))
                };
            let end = self.next_word_boundary(start);
            if end <= start {
                return false;
            }
            self.anchor = start;
            self.cursor = end;
            return true;
        };

        let needle = self.rope.slice_to_string(range.clone());
        if needle.is_empty() {
            return false;
        }

        // Search forward from the furthest selection, wrapping once, and
        // skipping occurrences that already have a cursor: after the wrap the
        // first hit is usually one of those, and stopping at it would leave
        // everything between it and the starting point unreachable.
        let selected = self.all_selections();
        let from = selected.last().map_or(range.end, |r| r.1);
        let mut at = from;
        let mut wrapped = false;
        loop {
            match self.rope.find_from(&needle, at) {
                Some(found) if wrapped && found >= from => return false,
                Some(found) if selected.iter().any(|r| r.0 == found) => {
                    at = found + needle.len();
                }
                Some(found) => {
                    self.add_cursor(found, found + needle.len());
                    return true;
                }
                None if wrapped => return false,
                None => {
                    wrapped = true;
                    at = 0;
                }
            }
        }
    }
    /// Where `at` is as a corner of a box selection: its screen row and its
    /// column on that row.
    pub fn box_corner_of(&self, at: usize) -> BoxCorner {
        let at = self.char_floor(at);
        let line = self.rope.byte_to_line(at);
        let starts = self.row_starts(line);
        let row = wrap::row_of(&starts, at);
        let start = starts
            .get(row)
            .copied()
            .unwrap_or_else(|| self.rope.line_to_byte(line));
        BoxCorner {
            line,
            row,
            column: wrap::column_in_row(&self.rope, start, at),
        }
    }
    /// The bytes screen row `row` of `line` covers, and how far a caret on
    /// it can go: the end of a row that continues is the next row's start,
    /// so a caret stops before its last character instead.
    fn row_span(&self, line: usize, row: usize) -> Option<(usize, usize, usize)> {
        let starts = self.row_starts(line);
        let start = *starts.get(row)?;
        Some(match starts.get(row + 1) {
            Some(&end) => (start, end, self.prev_boundary(end).max(start)),
            None => {
                let end = wrap::line_end(&self.rope, line);
                (start, end, end)
            }
        })
    }
    /// The cursors of a box selection dragged from `from` to `to`: one per
    /// screen row between them, in that order, each from `from`'s column to
    /// `to`'s, so the last is the row under the pointer.
    ///
    /// A row that ends before the box's left edge gets no cursor, unless
    /// none reaches it: then every row gets a caret at its end, so a box
    /// drawn past the end of every line still gives a caret per row. A row
    /// ending exactly at the left edge gets a caret there.
    pub fn box_cursors(&self, from: BoxCorner, to: BoxCorner) -> Vec<(usize, usize)> {
        let target = (to.line, to.row);
        let step = if target < (from.line, from.row) {
            -1
        } else {
            1
        };
        let mut at = (from.line, from.row);
        let mut rows = vec![at];
        while at != target {
            let next = self.step_rows(at, step);
            if next == at || (step > 0 && next > target) || (step < 0 && next < target) {
                break;
            }
            at = next;
            rows.push(at);
        }
        let spans: Vec<_> = rows
            .into_iter()
            .filter_map(|(line, row)| self.row_span(line, row))
            .collect();
        let left = from.column.min(to.column);
        let reaches = |&&(start, end, _): &&(usize, usize, usize)| {
            wrap::column_in_row(&self.rope, start, end) >= left
        };
        if !spans.iter().any(|s| reaches(&s)) {
            return spans.iter().map(|&(_, _, cap)| (cap, cap)).collect();
        }
        spans
            .iter()
            .filter(reaches)
            .map(|&(start, end, cap)| {
                let at = |column| wrap::byte_at_column(&self.rope, start, end, column).min(cap);
                (at(from.column), at(to.column))
            })
            .collect()
    }
    /// Replaces every cursor with a box selection, as Option-drag makes
    /// one; the primary is the row under the pointer.
    pub fn select_box(&mut self, from: BoxCorner, to: BoxCorner) {
        let mut cursors = self.box_cursors(from, to);
        let Some((anchor, head)) = cursors.pop() else {
            return;
        };
        (self.anchor, self.cursor) = (anchor, head);
        self.extra = cursors;
        self.goal_column = None;
        self.extra_goals.clear();
        self.last_edit = None;
    }
    /// Replaces every selection with `replacement`; every cursor lands at
    /// the end of its replacement.
    pub(super) fn edit_at_all_cursors(&mut self, replacement: &str, reach: Reach) {
        let selections = self.all_selections();
        let measured = selections
            .iter()
            .map(|&(start, end)| {
                let range = match reach {
                    _ if end > start => start..end,
                    Reach::Nothing => start..end,
                    Reach::Back => self.prev_boundary(start)..end,
                    Reach::Both => self.prev_boundary(start)..self.next_boundary(end),
                };
                Some((range, replacement.to_owned(), replacement.len()))
            })
            .collect();
        self.apply_at_cursors(&selections, measured);
    }
    /// One edit at every cursor, each measured by `edit` from that cursor's
    /// own selection as `(start, end)`: Return carries the indentation of
    /// the caret's own line, not the primary's. `None`, or an edit that
    /// changes nothing, leaves that cursor where it is. When anything
    /// changes it is one undo step of `kind`; returns whether it did.
    pub(super) fn edit_each_cursor(
        &mut self,
        kind: EditKind,
        edit: impl Fn(&Self, usize, usize) -> Option<CursorEdit>,
    ) -> bool {
        let selections = self.all_selections();
        let measured: Vec<_> = selections
            .iter()
            .map(|&(start, end)| edit(self, start, end))
            .collect();
        if !measured.iter().flatten().any(changes_text) {
            return false;
        }
        self.checkpoint_keeping_cursors(kind);
        self.apply_at_cursors(&selections, measured);
        true
    }
    /// Applies `measured`, one edit or none for each of `selections` (as
    /// [`Buffer::all_selections`] gave them), and puts every cursor that
    /// made an edit where that edit lands.
    ///
    /// Every edit is measured before any is applied, against the text as it
    /// is. How far a caret reaches depends on the character next to *that*
    /// caret: one width for all of them deleted half of an accent under one
    /// cursor because another sat after an ASCII letter.
    fn apply_at_cursors(
        &mut self,
        selections: &[(usize, usize)],
        measured: Vec<Option<CursorEdit>>,
    ) {
        let mut edits: Vec<CursorEdit> = Vec::new();
        // owner[i]: the edit the cursors of `selections[i]` follow.
        let mut owner: Vec<Option<usize>> = Vec::with_capacity(measured.len());
        for one in measured {
            let Some((range, text, land)) = one.filter(changes_text) else {
                owner.push(None);
                continue;
            };
            // Reaching can run into the neighbour, and two edits over the
            // same bytes would delete past what the first one left. Only
            // deletions reach, so the merged edit keeps the first's text.
            match edits.last_mut() {
                Some(last) if range.start < last.0.end => {
                    last.0.start = last.0.start.min(range.start);
                    last.0.end = last.0.end.max(range.end);
                }
                _ => edits.push((range, text, land)),
            }
            owner.push(Some(edits.len() - 1));
        }
        // Which edit each cursor follows, from where it is before any lands.
        let follows = |(a, h): (usize, usize)| {
            let at = a.min(h);
            let i = selections.partition_point(|s| s.0 <= at).saturating_sub(1);
            owner.get(i).copied().flatten()
        };
        let primary = follows((self.anchor, self.cursor));
        let extra: Vec<_> = self.extra.iter().map(|&c| follows(c)).collect();

        let applied: Vec<_> = edits
            .iter()
            .map(|(range, text, _)| (range.clone(), text.as_str()))
            .collect();
        self.apply_edits(&applied, AtInsert::After);

        // Where each edit's caret lands in the text as it now is.
        let mut shift = 0isize;
        let landings: Vec<usize> = edits
            .iter()
            .map(|(range, text, land)| {
                let at = (range.start as isize + shift) as usize + land;
                shift += text.len() as isize - range.len() as isize;
                at
            })
            .collect();
        if let Some(e) = primary {
            self.cursor = landings[e];
            self.anchor = self.cursor;
        }
        for (cursor, follows) in self.extra.iter_mut().zip(extra) {
            if let Some(e) = follows {
                *cursor = (landings[e], landings[e]);
            }
        }
        self.clamp_positions();
        self.goal_column = None;
        self.extra_goals.clear();
        // Cursors whose edits merged now share a place; keep one there.
        let mut seen = std::collections::HashSet::from([self.cursor]);
        self.extra.retain(|&(_, at)| seen.insert(at));
    }
    /// Applies `edits` within the current undo step. They are in order, do
    /// not overlap, and are measured against the text as it is. Each goes
    /// in back to front and is recorded for the parser, so the tree updates
    /// incrementally, and every caret and anchor moves with the text around
    /// it: before an edit it stays, after one it shifts by the change,
    /// inside a replaced range it lands at the end of the replacement, and
    /// exactly at an insertion it goes to `at_insert`'s side.
    pub(super) fn apply_edits(
        &mut self,
        edits: &[(std::ops::Range<usize>, &str)],
        at_insert: AtInsert,
    ) {
        for (range, text) in edits.iter().rev() {
            let old_end_point = self.point_of(range.end);
            if !range.is_empty() {
                self.rope.delete(range.clone());
            }
            if !text.is_empty() {
                self.rope.insert(range.start, text);
            }
            self.record_edit(
                range.start,
                range.end,
                old_end_point,
                range.start + text.len(),
            );
        }
        // shifts[i]: how far the edits before `i` move what follows them.
        let mut shifts = Vec::with_capacity(edits.len() + 1);
        let mut shift = 0isize;
        shifts.push(0);
        for (range, text) in edits {
            shift += text.len() as isize - range.len() as isize;
            shifts.push(shift);
        }
        let map = |pos: usize| -> usize {
            let passed = edits.partition_point(|(range, _)| {
                pos > range.end
                    || (pos == range.end && (!range.is_empty() || at_insert == AtInsert::After))
            });
            let moved = match edits.get(passed) {
                Some((range, text)) if range.start < pos => {
                    range.start as isize + shifts[passed] + text.len() as isize
                }
                _ => pos as isize + shifts[passed],
            };
            moved.max(0) as usize
        };
        self.cursor = map(self.cursor);
        self.anchor = map(self.anchor);
        for cursor in &mut self.extra {
            *cursor = (map(cursor.0), map(cursor.1));
        }
        self.clamp_positions();
    }
    /// Whether every cursor is a bare caret for which `test` holds.
    pub(super) fn every_caret(&self, test: impl Fn(&Self, usize) -> bool) -> bool {
        self.all_selections()
            .iter()
            .all(|&(start, end)| start == end && test(self, start))
    }
    /// Moves every caret by `by` bytes, collapsing each to a bare caret.
    pub(super) fn shift_carets(&mut self, by: isize) {
        let shift = |p: usize| (p as isize + by).max(0) as usize;
        self.cursor = shift(self.cursor);
        self.anchor = self.cursor;
        for cursor in &mut self.extra {
            cursor.1 = shift(cursor.1);
            cursor.0 = cursor.1;
        }
        self.clamp_positions();
    }

    // ---- bracket pairing -------------------------------------------------
}
