//! Several carets at once: adding them, and editing at all of them.

use super::*;

/// One cursor's edit: the range it replaces, the text it puts there, and
/// how far into that text its caret lands.
pub(super) type CursorEdit = (std::ops::Range<usize>, String, usize);

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
