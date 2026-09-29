//! Merge conflicts in the editor: stepping through them, taking a side,
//! the side-by-side columns, and marking a file resolved.

use super::*;

impl EditorView {
    /// Puts the caret at the start of `line`, with a few lines of context
    /// above it on screen.
    pub(super) fn caret_to_line(&self, line: usize) {
        let (rows, _) = self.grid();
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let buffer = state.docs.active_mut();
            let line = line.min(buffer.rope.len_lines().saturating_sub(1));
            let at = buffer.rope.line_to_byte(line);
            buffer.place_cursor(at, Motion::Move);
            buffer.scroll_to(line.saturating_sub(rows / 4), rows);
        }
        self.request_redraw();
        self.pump();
    }

    /// Git > Next Conflict and Previous Conflict, and the strip's buttons:
    /// the caret to the next conflict's opening marker, wrapping around the
    /// file. `from_top` goes to the first.
    pub(super) fn step_conflict(&self, forward: bool, from_top: bool) {
        let target = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            sync_conflicts(&mut state);
            let line = state.docs.active().cursor_position().0;
            active_conflicts(&state).and_then(|view| {
                let index = if from_top {
                    (!view.conflicts.is_empty()).then_some(0)
                } else {
                    crate::project::conflict::step(&view.conflicts, line, forward)
                }?;
                Some((
                    index,
                    view.conflicts.len(),
                    view.conflicts[index].start_line,
                ))
            })
        };
        let Some((index, count, line)) = target else {
            if let Some(mut state) = self.state_mut() {
                state.message = Some(("no conflicts in this file".to_string(), Instant::now()));
            }
            self.request_redraw();
            self.pump();
            return;
        };
        self.caret_to_line(line);
        let text = self.chrome().text;
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let total = state.docs.active().rope.len_lines();
            if let Some(view) = active_conflicts_mut(&mut state) {
                crate::platform::conflicts::show_in_side(view, total, index, text);
            }
            state.message = Some((format!("conflict {} of {count}", index + 1), Instant::now()));
        }
        self.request_redraw();
        self.pump();
    }

    /// Resolves conflict `index` of the active document to `take`: one edit,
    /// one undo step, the file left unsaved.
    pub(super) fn take_conflict(&self, index: usize, take: crate::project::conflict::Take) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            sync_conflicts(&mut state);
            let Some((conflict, count)) = active_conflicts(&state).and_then(|v| {
                v.conflicts
                    .get(index)
                    .cloned()
                    .map(|c| (c, v.conflicts.len()))
            }) else {
                return;
            };
            if !conflict.offers(take) {
                return;
            }
            let buffer = state.docs.active_mut();
            let text = conflict.resolution(&buffer.rope, take);
            // Its own undo step, never merged into typing just before it.
            let caret = buffer.cursor();
            buffer.place_cursor(caret, Motion::Move);
            if buffer.replace_ranges(&[(conflict.range.clone(), text)]) == 0 {
                state.message = Some(("this document is read-only".to_string(), Instant::now()));
                drop(state);
                self.request_redraw();
                self.pump();
                return;
            }
            let buffer = state.docs.active_mut();
            buffer.place_cursor(conflict.range.start, Motion::Move);
            let left = count - 1;
            state.message = Some((
                match left {
                    0 => "no conflicts left".to_string(),
                    1 => format!("{}; 1 conflict left", take.label()),
                    n => format!("{}; {n} conflicts left", take.label()),
                },
                Instant::now(),
            ));
        }
        self.after_edit();
    }

    /// Accept Current and friends from the menu: the conflict the caret is
    /// in, or else the next one.
    pub(super) fn take_conflict_at_caret(&self, take: crate::project::conflict::Take) {
        let index = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            sync_conflicts(&mut state);
            let line = state.docs.active().cursor_position().0;
            active_conflicts(&state).and_then(|view| {
                crate::project::conflict::at_line(&view.conflicts, line)
                    .or_else(|| crate::project::conflict::step(&view.conflicts, line, true))
            })
        };
        if let Some(index) = index {
            self.take_conflict(index, take);
        }
    }

    /// Inline or side by side. The columns open on the conflict the caret
    /// is in, or the next one.
    pub(super) fn set_conflict_side(&self, side: bool) {
        let text = self.chrome().text;
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.conflict_side = side;
            let line = state.docs.active().cursor_position().0;
            let total = state.docs.active().rope.len_lines();
            if side && let Some(view) = active_conflicts_mut(&mut state) {
                let index = crate::project::conflict::at_line(&view.conflicts, line)
                    .or_else(|| crate::project::conflict::step(&view.conflicts, line, true));
                if let Some(index) = index {
                    crate::platform::conflicts::show_in_side(view, total, index, text);
                }
            }
        }
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
        self.request_redraw();
        self.pump();
    }

    /// Git > Mark Resolved: saves the file if it has to, then `git add`,
    /// which is what ends a conflict for Git. Refused while markers remain,
    /// since that would commit them.
    pub(super) fn mark_conflict_resolved(&self) {
        let (path, ready, left, dirty) = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            sync_conflicts(&mut state);
            let buffer = state.docs.active();
            let (path, dirty) = (buffer.path.clone(), buffer.is_dirty());
            match active_conflicts(&state) {
                Some(view) => (path, view.can_resolve(), view.conflicts.len(), dirty),
                None => (path, false, 0, dirty),
            }
        };
        let Some(path) = path.filter(|_| ready) else {
            if let Some(mut state) = self.state_mut() {
                state.message = Some((
                    match left {
                        0 => "Git does not list this file as in conflict".to_string(),
                        1 => "1 conflict left; resolve it first".to_string(),
                        n => format!("{n} conflicts left; resolve them first"),
                    },
                    Instant::now(),
                ));
            }
            self.request_redraw();
            self.pump();
            return;
        };
        if dirty && !self.save(false) {
            return;
        }
        if let Some(mut state) = self.state_mut() {
            state.git.mark_resolved(path);
        }
        self.resume_display_link();
        self.request_redraw();
        self.pump();
    }

    /// Keys while the columns show. `None` to handle the key as usual.
    pub(super) fn handle_conflict_side_key(&self, event: &NSEvent) -> Option<bool> {
        const ESCAPE: u16 = 53;
        let flags = event.modifierFlags();
        let command = flags.contains(NSEventModifierFlags::Command);
        let control = flags.contains(NSEventModifierFlags::Control);
        let text = self.chrome().text;
        let page = (crate::platform::conflicts::side_rows_visible(text) as isize - 2).max(1);
        let delta = match event.keyCode() {
            ESCAPE => {
                self.set_conflict_side(false);
                return Some(true);
            }
            key::UP => -1,
            key::DOWN => 1,
            key::PAGE_UP => -page,
            key::PAGE_DOWN => page,
            key::HOME => isize::MIN / 2,
            key::END => isize::MAX / 2,
            _ if command || control => return None,
            // The columns are not editable; typing would land unseen.
            _ => return Some(true),
        };
        {
            let mut state = self.state_mut()?;
            let total = state.docs.active().rope.len_lines();
            if let Some(view) = active_conflicts_mut(&mut state) {
                view.scroll = view.scroll.saturating_add_signed(delta);
                crate::platform::conflicts::clamp_side_scroll(view, total, text);
            }
        }
        self.request_redraw();
        self.pump();
        Some(true)
    }
}
