//! Several carets at once: adding them, and editing at all of them.

use super::*;

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
    }
    /// Drops every cursor but the primary.
    pub fn collapse_cursors(&mut self) -> bool {
        if self.extra.is_empty() {
            return false;
        }
        self.extra.clear();
        true
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
    /// Replaces every selection with `replacement`; every cursor lands at
    /// the end of its replacement.
    pub(super) fn edit_at_all_cursors(&mut self, replacement: &str, reach: Reach) {
        // Every range is measured here, against the text as it is, before any
        // of them is applied. How far a caret reaches depends on the
        // character next to *that* caret: one width for all of them deleted
        // half of an accent under one cursor because another sat after an
        // ASCII letter.
        let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
        for (start, end) in self.all_selections() {
            let range = match reach {
                _ if end > start => start..end,
                Reach::Nothing => start..end,
                Reach::Back => self.prev_boundary(start)..end,
                Reach::Both => self.prev_boundary(start)..self.next_boundary(end),
            };
            // Reaching can run into the neighbour, and two edits over the
            // same bytes would delete past what the first one left.
            match ranges.last_mut() {
                Some(last) if range.start < last.end => last.end = last.end.max(range.end),
                _ => ranges.push(range),
            }
        }
        // Each cursor as a caret at its selection's end, which the edits
        // carry to the end of its replacement.
        let end = |(a, b): (usize, usize)| (a.max(b), a.max(b));
        (self.anchor, self.cursor) = end((self.anchor, self.cursor));
        for cursor in &mut self.extra {
            *cursor = end(*cursor);
        }
        let edits: Vec<_> = ranges.into_iter().map(|r| (r, replacement)).collect();
        self.apply_edits(&edits, AtInsert::After);
        // Cursors whose ranges merged now share a place; keep one there.
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
