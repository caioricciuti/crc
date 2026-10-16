//! Edits over lines and blocks: paired brackets, replace, indent,
//! comments, duplicating and moving lines.

use super::*;

impl Buffer {
    /// Types `ch`, auto-closing brackets and quotes where it helps.
    ///
    /// Three behaviours, all of which exist because their absence is
    /// immediately irritating:
    ///   - typing an opener inserts the pair and sits between them
    ///   - typing a closer that is already there steps over it instead of
    ///     inserting a second one
    ///   - with text selected, an opener wraps the selection
    ///
    /// Auto-close is suppressed when the next character is a letter or digit,
    /// because typing `(` before an existing word almost always means calling
    /// it, not wrapping it.
    pub fn insert_char_paired(&mut self, ch: char) {
        if self.is_locked() {
            return;
        }
        let closer = match ch {
            '(' => Some(')'),
            '[' => Some(']'),
            '{' => Some('}'),
            '"' => Some('"'),
            '\'' => Some('\''),
            '`' => Some('`'),
            _ => None,
        };

        // Wrap a selection rather than replacing it.
        if let (Some(closer), Some(range)) = (closer, self.selection()) {
            // The wrapped text is the primary selection's. Other cursors may
            // hold something else, or nothing, so this is a one-cursor edit.
            self.extra.clear();
            let text = self.rope.slice_to_string(range.clone());
            self.insert(&format!("{ch}{text}{closer}"));
            // Leave the wrapped text selected, which is what makes wrapping
            // twice work.
            self.anchor = range.start + ch.len_utf8();
            self.cursor = self.anchor + text.len();
            return;
        }

        // Step over a closer that is already there.
        if matches!(ch, ')' | ']' | '}' | '"' | '\'' | '`')
            && self.every_caret(|b, at| b.char_at(at) == Some(ch))
        {
            // At every caret or at none: stepping over at some and typing at
            // others would leave the cursors disagreeing about what happened.
            self.shift_carets(ch.len_utf8() as isize);
            self.last_edit = None;
            self.goal_column = None;
            return;
        }

        let Some(closer) = closer else {
            self.insert(&ch.to_string());
            return;
        };

        let next_is_word = self
            .char_at(self.cursor)
            .is_some_and(crate::complete::is_word_char);
        if next_is_word {
            self.insert(&ch.to_string());
            return;
        }

        // A quote immediately after a word is a closing quote or an
        // apostrophe, not the start of a new string.
        if matches!(ch, '"' | '\'' | '`')
            && self
                .char_before(self.cursor)
                .is_some_and(crate::complete::is_word_char)
        {
            self.insert(&ch.to_string());
            return;
        }

        let mut pair = String::with_capacity(2);
        pair.push(ch);
        pair.push(closer);
        // One keystroke, so it undoes with the typing around it.
        self.insert_as(&pair, true);
        // Every cursor typed the pair, so every cursor sits inside its own.
        self.shift_carets(-(closer.len_utf8() as isize));
    }
    /// Backspace that removes both halves of an empty pair.
    pub fn backspace_paired(&mut self) {
        if self.is_locked() {
            return;
        }
        const PAIRS: [(char, char); 6] = [
            ('(', ')'),
            ('[', ']'),
            ('{', '}'),
            ('"', '"'),
            ('\'', '\''),
            ('`', '`'),
        ];
        if !self.extra.is_empty() {
            let in_empty_pair = |b: &Self, at: usize| matches!((b.char_before(at), b.char_at(at)), (Some(o), Some(c)) if PAIRS.contains(&(o, c)));
            if self.every_caret(in_empty_pair) {
                self.checkpoint_keeping_cursors(EditKind::Delete);
                self.edit_at_all_cursors("", Reach::Both);
            } else {
                self.backspace();
            }
            return;
        }
        if self.selection().is_none() {
            let before = self.char_before(self.cursor);
            let after = self.char_at(self.cursor);
            let empty_pair = matches!(
                (before, after),
                (Some('('), Some(')'))
                    | (Some('['), Some(']'))
                    | (Some('{'), Some('}'))
                    | (Some('"'), Some('"'))
                    | (Some('\''), Some('\''))
                    | (Some('`'), Some('`'))
            );
            if empty_pair {
                let start = self.prev_boundary(self.cursor);
                let end = self.next_boundary(self.cursor);
                self.checkpoint(EditKind::Delete);
                self.delete_range_recorded(start..end);
                return;
            }
        }
        self.backspace();
    }

    // ---- search and replace ----------------------------------------------
    /// Replaces the selection with `text` if it matches `needle`, then finds
    /// the next occurrence. Returns whether anything was replaced.
    pub fn replace_current(&mut self, needle: &str, text: &str) -> bool {
        if self.is_locked() {
            return false;
        }
        let Some(range) = self.selection() else {
            return false;
        };
        if self.rope.slice_to_string(range.clone()) != needle {
            return false;
        }
        // The match is the primary's selection. Other cursors would each
        // get the replacement too, whatever they hold.
        self.collapse_cursors();
        self.insert(text);
        true
    }
    /// Replaces every occurrence of `needle`, returning how many.
    ///
    /// One undo step for the whole operation, which is the only thing that
    /// makes a mistaken replace-all recoverable.
    pub fn replace_all(&mut self, needle: &str, text: &str) -> usize {
        if self.is_locked() {
            return 0;
        }
        if needle.is_empty() {
            return 0;
        }
        let mut offsets = Vec::new();
        let mut at = 0;
        while let Some(found) = self.rope.find_from(needle, at) {
            offsets.push(found);
            at = found + needle.len();
        }
        if offsets.is_empty() {
            return 0;
        }
        let text = &*normalize_newlines(text);
        let edits: Vec<_> = offsets
            .iter()
            .map(|&offset| (offset..offset + needle.len(), text))
            .collect();
        self.checkpoint(EditKind::Insert);
        self.apply_edits(&edits, AtInsert::After);
        self.anchor = self.cursor;
        offsets.len()
    }
    /// Applies already-resolved ranges as one undoable edit: regex captures,
    /// case-insensitive matches, a language server's edits. Ranges are in
    /// order and do not overlap; an empty one is an insertion. The caret
    /// keeps its place in the text around the edits.
    pub fn replace_ranges(&mut self, replacements: &[(std::ops::Range<usize>, String)]) -> usize {
        if self.is_locked() {
            return 0;
        }
        if replacements.is_empty() {
            return 0;
        }
        // A formatter on a CRLF file answers in CRLF; the rope holds LF
        // only and the file's own endings are restored on save.
        let normalized: Vec<(std::ops::Range<usize>, String)>;
        let replacements = if replacements.iter().any(|(_, t)| t.contains('\r')) {
            normalized = replacements
                .iter()
                .map(|(r, t)| (r.clone(), normalize_newlines(t).into_owned()))
                .collect();
            &normalized[..]
        } else {
            replacements
        };
        let mut previous_end = 0;
        for (range, _) in replacements {
            if range.start < previous_end
                || range.start > range.end
                || range.end > self.rope.len_bytes()
            {
                return 0;
            }
            previous_end = range.end;
        }
        let edits: Vec<_> = replacements
            .iter()
            .map(|(range, text)| (range.clone(), text.as_str()))
            .collect();
        self.checkpoint(EditKind::Insert);
        self.apply_edits(&edits, AtInsert::After);
        self.anchor = self.cursor;
        replacements.len()
    }

    // ---- line and block operations ---------------------------------------
    /// The leading whitespace of `line`, as a string.
    pub(super) fn indent_of(&self, line: usize) -> String {
        let start = self.rope.line_to_byte(line);
        let end = self.line_end(line);
        let text = self.rope.slice_to_string(start..end);
        text.chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect()
    }
    /// One indent level, matching whatever the surrounding line already uses.
    ///
    /// Guessing from context rather than from a setting: a file that is
    /// indented with tabs should stay indented with tabs, and there is no
    /// preferences system yet to say otherwise.
    pub(super) fn indent_unit(&self, line: usize) -> String {
        if let Some(style) = self.indent_style {
            return style.unit();
        }
        let indent = self.indent_of(line);
        if indent.contains('\t') {
            "\t".to_string()
        } else {
            "    ".to_string()
        }
    }
    /// Tab with nothing selected: a tab character, or spaces to the next
    /// stop when the document indents with spaces.
    pub fn insert_tab(&mut self) {
        match self.indent_style {
            Some(style) if !style.tabs => {
                if self.is_locked() {
                    return;
                }
                // Each caret to its own next stop, so it is its own undo
                // step, like any insert of more than one character.
                let width = style.width.max(1);
                self.last_edit = None;
                self.edit_each_cursor(EditKind::Insert, |b, start, end| {
                    let (_, column) = b.position_of(start);
                    let spaces = " ".repeat(width - column % width);
                    let land = spaces.len();
                    Some((start..end, spaces, land))
                });
                self.last_edit = None;
            }
            _ => self.insert("\t"),
        }
    }
    /// Inserts a newline at every cursor, carrying that cursor's line's
    /// indentation, and adding a level when the line being left ends in an
    /// opening bracket. One undo step of its own.
    pub fn insert_newline_indented(&mut self) {
        if self.is_locked() {
            return;
        }
        self.last_edit = None;
        self.edit_each_cursor(EditKind::Insert, |b, start, end| {
            let line = b.rope.byte_to_line(start);
            let indent = b.indent_of(line);

            // Look at the text before the cursor, not the whole line:
            // pressing Enter in the middle of `{ foo }` should not indent.
            let line_start = b.rope.line_to_byte(line);
            let before = b.rope.slice_to_string(line_start..start);
            let opens = before.trim_end().ends_with(['{', '[', '(']);

            // And whether a closing bracket sits immediately after, in which
            // case it gets a line of its own at the outer level.
            let after_is_close = b.char_at(end).is_some_and(|c| matches!(c, '}' | ']' | ')'));

            let unit = b.indent_unit(line);
            let mut text = String::with_capacity(2 * indent.len() + unit.len() + 2);
            text.push('\n');
            text.push_str(&indent);
            if opens {
                text.push_str(&unit);
            }
            // Put a closer right after on its own line, and land on the
            // blank line between them.
            let land = text.len();
            if opens && after_is_close {
                text.push('\n');
                text.push_str(&indent);
            }
            Some((start..end, text, land))
        });
        self.last_edit = None;
    }
    /// The lines a selection from `start` to `end` touches; a caret's own.
    fn lines_between(&self, start: usize, end: usize) -> std::ops::RangeInclusive<usize> {
        let first = self.rope.byte_to_line(start);
        // A selection ending exactly at a line start does not include that
        // line; otherwise selecting a whole line by dragging to the next one
        // would indent two.
        let mut last = self.rope.byte_to_line(end);
        if last > first && end == self.rope.line_to_byte(last) {
            last -= 1;
        }
        first..=last
    }
    /// Every line any cursor touches, each once, as runs of adjacent lines
    /// `(first, last)`, ascending. The line commands work on these, so two
    /// cursors on one line indent it once.
    fn cursor_line_runs(&self) -> Vec<(usize, usize)> {
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for (start, end) in self.all_selections() {
            let lines = self.lines_between(start, end);
            let (first, last) = (*lines.start(), *lines.end());
            match runs.last_mut() {
                Some(run) if first <= run.1 + 1 => run.1 = run.1.max(last),
                _ => runs.push((first, last)),
            }
        }
        runs
    }
    /// Every line any cursor touches, each once, ascending.
    fn cursor_lines(&self) -> Vec<usize> {
        self.cursor_line_runs()
            .into_iter()
            .flat_map(|(first, last)| first..=last)
            .collect()
    }
    /// Applies a line command's `edits` as one undo step of its own, every
    /// cursor moving with the text around it. Nothing to apply, no step.
    fn edit_lines(
        &mut self,
        kind: EditKind,
        edits: &[(std::ops::Range<usize>, &str)],
        at_insert: AtInsert,
    ) {
        if edits.is_empty() {
            return;
        }
        self.last_edit = None;
        self.checkpoint_keeping_cursors(kind);
        self.apply_edits(edits, at_insert);
        self.last_edit = None;
        // Carets in an indent that went away now share its edge.
        self.merge_cursors();
    }
    /// Adds one indent level to every line a cursor touches.
    pub fn indent(&mut self) {
        if self.is_locked() {
            return;
        }
        let lines = self.cursor_lines();
        let unit = self.indent_unit(lines[0]);
        // A caret at a line's start moves with the line's text.
        let edits: Vec<_> = lines
            .iter()
            .map(|&line| {
                let at = self.rope.line_to_byte(line);
                (at..at, unit.as_str())
            })
            .collect();
        self.edit_lines(EditKind::Insert, &edits, AtInsert::After);
    }
    /// Removes one indent level from every line a cursor touches that has
    /// one.
    pub fn outdent(&mut self) {
        if self.is_locked() {
            return;
        }
        let level = self.indent_style.map_or(4, |s| s.width.max(1));
        let mut edits = Vec::new();
        for line in self.cursor_lines() {
            let at = self.rope.line_to_byte(line);
            let indent = self.indent_of(line);
            // Take a tab, or up to one level of spaces: a line indented by
            // three spaces should still outdent rather than refusing.
            let take = if indent.starts_with('\t') {
                1
            } else {
                indent.chars().take_while(|c| *c == ' ').count().min(level)
            };
            if take > 0 {
                edits.push((at..at + take, ""));
            }
        }
        // A caret inside the removed indentation stops at the line's new
        // start rather than being pulled back onto the line above.
        self.edit_lines(EditKind::Delete, &edits, AtInsert::After);
    }
    /// Comments or uncomments the lines the cursors touch with `token`.
    ///
    /// Toggling on the whole block rather than per line: if any of the lines
    /// is uncommented, the whole block gets commented, which is what makes a
    /// second press restore exactly what you started with.
    pub fn toggle_comment(&mut self, token: &str) {
        if self.is_locked() {
            return;
        }
        let non_empty: Vec<usize> = self
            .cursor_lines()
            .into_iter()
            .filter(|&l| {
                let start = self.rope.line_to_byte(l);
                let end = self.line_end(l);
                !self.rope.slice_to_string(start..end).trim().is_empty()
            })
            .collect();
        if non_empty.is_empty() {
            return;
        }

        let all_commented = non_empty.iter().all(|&l| {
            let start = self.rope.line_to_byte(l);
            let end = self.line_end(l);
            self.rope
                .slice_to_string(start..end)
                .trim_start()
                .starts_with(token)
        });

        let insert = format!("{token} ");
        let edits: Vec<_> = non_empty
            .iter()
            .map(|&line| {
                let start = self.rope.line_to_byte(line);
                let text = self.rope.slice_to_string(start..self.line_end(line));
                let indent_len = text.len() - text.trim_start().len();
                let at = start + indent_len;
                if all_commented {
                    // The token and one following space if it is there.
                    let rest = &text[indent_len + token.len()..];
                    (
                        at..at + token.len() + usize::from(rest.starts_with(' ')),
                        "",
                    )
                } else {
                    (at..at, insert.as_str())
                }
            })
            .collect();
        let kind = if all_commented {
            EditKind::Delete
        } else {
            EditKind::Insert
        };
        // A selection that starts where the token goes takes it in, so a
        // second toggle restores exactly what was there.
        self.edit_lines(kind, &edits, AtInsert::Before);
    }
    /// Duplicates each run of lines the cursors touch below itself.
    ///
    /// The copy goes in at the run's start, which is the same text as one
    /// below it, so every cursor in the run moves down onto the copy, which
    /// is what makes repeated presses stack.
    pub fn duplicate_lines(&mut self) {
        if self.is_locked() {
            return;
        }
        let copies: Vec<(usize, String)> = self
            .cursor_line_runs()
            .into_iter()
            .map(|(first, last)| {
                let start = self.rope.line_to_byte(first);
                let mut text = self
                    .rope
                    .slice_to_string(start..self.rope.line_range(last).end);
                // A last line with no trailing newline still needs one
                // between it and its copy.
                if !text.ends_with('\n') {
                    text.push('\n');
                }
                (start, text)
            })
            .collect();
        let edits: Vec<_> = copies
            .iter()
            .map(|(at, text)| (*at..*at, text.as_str()))
            .collect();
        self.edit_lines(EditKind::Insert, &edits, AtInsert::After);
    }
    /// Moves each run of lines the cursors touch up or down by one. When a
    /// run is already at the edge none of them moves, so the runs never
    /// change places with each other.
    ///
    /// Each is a swap of two adjacent spans rather than a delete and a
    /// re-insert elsewhere, so it stays one edit for undo and for the
    /// incremental parser.
    pub fn move_lines(&mut self, down: bool) {
        if self.is_locked() {
            return;
        }
        let runs = self.cursor_line_runs();
        let total = self.rope.len_lines();
        // The empty "line" after a final newline is not one to swap with:
        // doing so moved the newline instead and grew the file a line per
        // press.
        let last_text = if total > 1 && self.rope.byte_at(self.rope.len_bytes() - 1) == Some(b'\n')
        {
            total - 2
        } else {
            total - 1
        };
        let stuck = |&(first, last): &(usize, usize)| {
            (down && last + 1 > last_text) || (!down && (first == 0 || first > last_text))
        };
        if runs.iter().any(stuck) {
            return;
        }

        // Per run: the span it swaps across, the swapped text, and how far
        // the run's cursors move with it.
        let swaps: Vec<(std::ops::Range<usize>, String, isize)> = runs
            .iter()
            .map(|&(first, last)| {
                // The span covering the block plus the line it swaps with,
                // and the offset where one ends and the other begins.
                let (span_first, span_last) = if down {
                    (first, last + 1)
                } else {
                    (first - 1, last)
                };
                let start = self.rope.line_to_byte(span_first);
                let end = self.rope.line_range(span_last).end;
                let split = self.rope.line_to_byte(if down { last + 1 } else { first });

                let head = self.rope.slice_to_string(start..split);
                let tail = self.rope.slice_to_string(split..end);

                // After the swap `tail` comes first and must end in a
                // newline, while `head` comes last and must only keep one if
                // the span originally did. The last line of a file with no
                // trailing newline is what makes this fiddly rather than a
                // plain concatenation. Either way the span keeps its length.
                let mut leading = tail;
                if !leading.ends_with('\n') {
                    leading.push('\n');
                }
                let mut trailing = head;
                if !self.rope.slice_to_string(start..end).ends_with('\n') {
                    trailing = trailing.strip_suffix('\n').unwrap_or(&trailing).to_string();
                }
                // Follow the block: moving down it now begins after
                // `leading`, moving up it begins where the span does.
                let delta = if down {
                    leading.len() as isize
                } else {
                    start as isize - split as isize
                };
                (start..end, format!("{leading}{trailing}"), delta)
            })
            .collect();

        // Each cursor follows the run its selection starts in.
        let follow = |b: &Self, (anchor, head): (usize, usize)| {
            let line = b.rope.byte_to_line(anchor.min(head));
            let run = runs.partition_point(|r| r.1 < line);
            let delta = swaps.get(run).map_or(0, |s| s.2);
            let shift = |p: usize| (p as isize + delta).max(0) as usize;
            (shift(anchor), shift(head))
        };
        let primary = follow(self, (self.anchor, self.cursor));
        let extra: Vec<_> = self.extra.iter().map(|&c| follow(self, c)).collect();

        let edits: Vec<_> = swaps
            .iter()
            .map(|(range, text, _)| (range.clone(), text.as_str()))
            .collect();
        self.last_edit = None;
        self.checkpoint_keeping_cursors(EditKind::Insert);
        self.apply_edits(&edits, AtInsert::After);
        self.last_edit = None;
        (self.anchor, self.cursor) = primary;
        self.extra = extra;
        self.clamp_positions();
    }
    /// Places the cursor at the start of `line`, 0-based and clamped.
    pub fn goto_line(&mut self, line: usize) {
        let line = line.min(self.rope.len_lines().saturating_sub(1));
        self.cursor = self.rope.line_to_byte(line);
        self.goal_column = None;
        self.after_move(Motion::Move);
    }
}
