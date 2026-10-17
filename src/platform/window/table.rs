//! CSV and TSV documents in the editor: the grid's keys and presses, the
//! field that edits a cell, and the Grid or Text switch. The view itself is
//! `crate::platform::table`.

use super::*;
use crate::platform::table::{CellEdit, Cells, Move, Op, TableView};

/// Brings the active document's table up to date with its text. Cheap
/// when nothing changed.
///
/// A cell's field lives only as long as its grid shows and its cell is
/// there: another tab in front, the Text mode, or an edit from elsewhere
/// that took the record away closes it, and what was typed goes with it.
pub(super) fn sync_tables(state: &mut State) {
    let State { docs, tables, .. } = state;
    crate::platform::table::sync(tables, docs.active());
    if let Some(edit) = &state.cell_edit {
        let alive = active_table(state).is_some_and(|view| {
            view.mode_grid
                && edit.record < view.table.rows.len()
                && edit.column < view.column_count()
        });
        if !alive {
            state.cell_edit = None;
        }
    }
}

/// The active document's table, when it is CSV or TSV and nothing else has
/// the editor column: a diff, a review or a merge conflict shows the text.
pub(super) fn active_table(state: &State) -> Option<&TableView> {
    if diffing(state) || active_review(state).is_some() || active_conflicts(state).is_some() {
        return None;
    }
    state.tables.get(&state.docs.active().id())
}

/// The grid is showing: grid mode, on a table document.
pub(super) fn table_grid(state: &State) -> bool {
    active_table(state).is_some_and(|view| view.mode_grid)
}

/// Whether a key event carries a character that would be typed: not a
/// control character, not one of the function keys AppKit reports in the
/// private-use block.
fn types_a_character(event: &NSEvent) -> bool {
    event.characters().map(|c| c.to_string()).is_some_and(|c| {
        c.chars()
            .next()
            .is_some_and(|c| !c.is_control() && !('\u{f700}'..='\u{f8ff}').contains(&c))
    })
}

impl EditorView {
    /// Grid or text, from the strip or Escape.
    pub(super) fn set_table_mode(&self, grid: bool) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            sync_tables(&mut state);
            let id = state.docs.active().id();
            if let Some(view) = state.tables.get_mut(&id) {
                view.mode_grid = grid;
            }
            // Back to the grid after typing in the text: the parse is of
            // the text as it is now, so the dump after this says so.
            sync_tables(&mut state);
        }
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
        self.request_redraw();
        self.pump();
    }

    /// A press on the grid: on the field being edited it places the caret;
    /// anywhere else it commits that field first, then selects the cell
    /// under the press, or the record whose number is under it. Two clicks
    /// open the cell.
    pub(super) fn table_press(&self, x: f32, y: f32, clicks: isize) {
        let text = self.chrome().text;
        let on_field = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let State {
                docs,
                tables,
                renderer,
                cell_edit,
                ..
            } = &mut *state;
            let cells = Cells::of(&renderer.atlas);
            match (cell_edit, tables.get(&docs.active().id())) {
                (Some(edit), Some(view)) => {
                    match view.cell_rect(cells, text, edit.record, edit.column) {
                        Some(cell) if cell.contains(x, y) => {
                            edit.place_caret(
                                x - cell.x - crate::platform::table::CELL_PAD,
                                cells,
                                cell.width,
                            );
                            true
                        }
                        _ => false,
                    }
                }
                _ => false,
            }
        };
        if !on_field {
            self.commit_cell_edit(None);
            let selected = {
                let Some(mut state) = self.state_mut() else {
                    return;
                };
                let State {
                    docs,
                    tables,
                    renderer,
                    ..
                } = &mut *state;
                let cells = Cells::of(&renderer.atlas);
                let Some(view) = tables.get_mut(&docs.active().id()) else {
                    return;
                };
                let cell = view.cell_for_press(cells, text, x, y);
                view.select(cell, cells, text);
                cell.is_some()
            };
            if selected && clicks >= 2 {
                self.start_cell_edit(false);
            }
        }
        self.request_redraw();
        self.pump();
    }

    /// Opens the field on the selected cell: with the cell's value and the
    /// caret at its end, or empty when the first typed character is about
    /// to replace the value. Whether a field opened.
    pub(super) fn start_cell_edit(&self, replace: bool) -> bool {
        let text = self.chrome().text;
        let Some(mut state) = self.state_mut() else {
            return false;
        };
        sync_tables(&mut state);
        let State {
            docs,
            tables,
            renderer,
            ..
        } = &mut *state;
        let cells = Cells::of(&renderer.atlas);
        let Some(view) = tables.get_mut(&docs.active().id()) else {
            return false;
        };
        let Some((record, column)) = view.selected else {
            return false;
        };
        // Wheeled away since it was selected: the cell comes back on screen.
        view.select(Some((record, column)), cells, text);
        let value = if replace {
            String::new()
        } else {
            view.cell_value(record, column)
        };
        close_fields(&mut state);
        state.cell_edit = Some(CellEdit::new(record, column, &value));
        true
    }

    /// Puts the field's text in its cell, encoded for the dialect, as one
    /// edit and one undo step, then moves the selection by `then`. Nothing
    /// is edited when the cell holds that value already.
    pub(super) fn commit_cell_edit(&self, then: Option<Move>) {
        let text = self.chrome().text;
        let edited = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(edit) = state.cell_edit.take() else {
                return;
            };
            // The range comes from a parse of the text as it is now, never
            // from one the text has moved on from.
            sync_tables(&mut state);
            let State {
                docs,
                tables,
                renderer,
                message,
                ..
            } = &mut *state;
            let buffer = docs.active_mut();
            let Some(view) = tables.get_mut(&buffer.id()) else {
                return;
            };
            let cells = Cells::of(&renderer.atlas);
            let value = edit.field.rope.to_string();
            let mut edited = false;
            if view.is_current(&buffer.rope)
                && let Some((range, encoded)) =
                    view.cell_replacement(edit.record, edit.column, &value)
            {
                // Its own undo step, never merged into typing just before
                // it, and the caret at the cell for the text behind the grid.
                buffer.place_cursor(range.start, Motion::Move);
                if buffer.replace_ranges(&[(range, encoded)]) == 0 {
                    *message = Some(("this document is read-only".to_string(), Instant::now()));
                } else {
                    edited = true;
                }
            }
            if let Some(step) = then {
                let page =
                    (crate::platform::table::rows_visible(text, cells.line_height) - 1).max(1);
                view.move_selection(step, page, cells, text);
            }
            edited
        };
        if edited {
            self.after_edit();
        } else {
            self.request_redraw();
            self.pump();
        }
    }

    /// One of the Table menu's commands on the selected cell's record or
    /// column, as one edit of the text behind the grid and one undo step.
    /// An open field is committed first, the ranges come from a parse of
    /// the text as it is now, and the selection lands on the new record's
    /// first cell, the new column's cell, or the cell now where a deleted
    /// one was. The caret follows, for the text behind the grid.
    pub(super) fn table_edit(&self, op: Op) {
        self.commit_cell_edit(None);
        let text = self.chrome().text;
        let edited = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if !table_grid(&state) {
                return;
            }
            sync_tables(&mut state);
            let id = state.docs.active().id();
            let target = {
                let State {
                    docs,
                    tables,
                    message,
                    ..
                } = &mut *state;
                let buffer = docs.active_mut();
                let Some(view) = tables.get(&id) else {
                    return;
                };
                let Some((record, column)) = view.selected else {
                    return;
                };
                if !view.is_current(&buffer.rope) {
                    return;
                }
                let Some(edits) = view.edit(op, record, column) else {
                    return;
                };
                // A column edit is a range per record: one range instead.
                let edits = view.coalesce(edits);
                // Its own undo step, never merged into typing just before.
                buffer.place_cursor(edits[0].0.start, Motion::Move);
                if buffer.replace_ranges(&edits) == 0 {
                    *message = Some(("this document is read-only".to_string(), Instant::now()));
                    None
                } else {
                    Some(op.selection_after(record, column))
                }
            };
            if let Some(target) = target {
                // The parse of the new text, then the selection on its cell.
                sync_tables(&mut state);
                let State {
                    docs,
                    tables,
                    renderer,
                    ..
                } = &mut *state;
                let cells = Cells::of(&renderer.atlas);
                if let Some(view) = tables.get_mut(&id) {
                    view.select(Some(target), cells, text);
                    if let Some(cell) = view
                        .selected
                        .and_then(|(record, column)| view.table.cell(record, column))
                    {
                        docs.active_mut()
                            .place_cursor(cell.range.start, Motion::Move);
                    }
                }
            }
            target.is_some()
        };
        if edited {
            self.after_edit();
        } else {
            self.request_redraw();
            self.pump();
        }
    }

    /// A secondary press on the grid, before its menu shows: the cell under
    /// it is selected, so the menu's commands act on what was pressed. An
    /// open field is committed first.
    pub(super) fn table_secondary_press(&self, x: f32, y: f32) {
        self.commit_cell_edit(None);
        let text = self.chrome().text;
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let State {
                docs,
                tables,
                renderer,
                ..
            } = &mut *state;
            let cells = Cells::of(&renderer.atlas);
            let Some(view) = tables.get_mut(&docs.active().id()) else {
                return;
            };
            if let Some(cell) = view.cell_for_press(cells, text, x, y) {
                view.select(Some(cell), cells, text);
            }
        }
        self.request_redraw();
        self.pump();
    }

    /// Keys while a cell's field is open. Return commits and stays on the
    /// cell, Tab and Shift-Tab commit and move, Escape drops the field with
    /// the cell as it was; the editing keys edit the field and the rest is
    /// typed into it. Command keys go to the menu.
    pub(super) fn handle_cell_key(&self, event: &NSEvent) -> bool {
        const ESCAPE: u16 = 53;
        let flags = event.modifierFlags();
        if flags.contains(NSEventModifierFlags::Command) {
            return false;
        }
        let shift = flags.contains(NSEventModifierFlags::Shift);
        let code = match event.keyCode() {
            key::KEYPAD_ENTER => key::RETURN,
            code => code,
        };
        match code {
            ESCAPE => {
                if let Some(mut state) = self.state_mut() {
                    state.cell_edit = None;
                }
            }
            key::RETURN => {
                self.commit_cell_edit(None);
                return true;
            }
            key::TAB => {
                self.commit_cell_edit(Some(if shift { Move::Previous } else { Move::Next }));
                return true;
            }
            key::DELETE | key::FORWARD_DELETE | key::LEFT | key::RIGHT | key::HOME | key::END => {
                let Some(mut state) = self.state_mut() else {
                    return false;
                };
                if let Some(edit) = &mut state.cell_edit {
                    field_key(&mut edit.field, code, flags);
                }
            }
            _ => return self.interpret(event),
        }
        self.request_redraw();
        self.pump();
        true
    }

    /// Keys while the grid shows and no field has them. `None` to handle
    /// the key as usual.
    ///
    /// With a cell selected the navigation keys move the selection, Return
    /// opens the cell and a typed character opens it empty and lands in the
    /// field; without a selection the keys scroll the records as a text
    /// view would. Escape drops the selection first and leaves for the text
    /// second.
    pub(super) fn handle_table_key(&self, event: &NSEvent) -> Option<bool> {
        const ESCAPE: u16 = 53;
        let flags = event.modifierFlags();
        let command = flags.contains(NSEventModifierFlags::Command);
        let control = flags.contains(NSEventModifierFlags::Control);
        let shift = flags.contains(NSEventModifierFlags::Shift);
        let text = self.chrome().text;
        let step = match event.keyCode() {
            key::UP => Some(Move::Up),
            key::DOWN => Some(Move::Down),
            key::LEFT => Some(Move::Left),
            key::RIGHT => Some(Move::Right),
            key::TAB if shift => Some(Move::Previous),
            key::TAB => Some(Move::Next),
            key::HOME if command => Some(Move::First),
            key::END if command => Some(Move::Last),
            key::HOME => Some(Move::RowStart),
            key::END => Some(Move::RowEnd),
            key::PAGE_UP => Some(Move::PageUp),
            key::PAGE_DOWN => Some(Move::PageDown),
            _ => None,
        };
        // Scrolling without a selection, in records.
        let scroll = |page: isize| match step {
            Some(Move::Up) => Some(-1),
            Some(Move::Down) => Some(1),
            Some(Move::PageUp) => Some(-page),
            Some(Move::PageDown) => Some(page),
            Some(Move::RowStart | Move::First) => Some(isize::MIN / 2),
            Some(Move::RowEnd | Move::Last) => Some(isize::MAX / 2),
            _ => None,
        };
        {
            let mut state = self.state_mut()?;
            let State {
                docs,
                tables,
                renderer,
                ..
            } = &mut *state;
            let view = tables.get_mut(&docs.active().id())?;
            let cells = Cells::of(&renderer.atlas);
            let page = (crate::platform::table::rows_visible(text, cells.line_height) - 1).max(1);
            match (event.keyCode(), step) {
                (ESCAPE, _) if view.selected.is_some() => view.select(None, cells, text),
                (ESCAPE, _) => {
                    drop(state);
                    self.set_table_mode(false);
                    return Some(true);
                }
                (key::RETURN | key::KEYPAD_ENTER, _) if view.selected.is_some() => {
                    drop(state);
                    self.start_cell_edit(false);
                }
                (key::RETURN | key::KEYPAD_ENTER, _) => return Some(true),
                (_, Some(step)) if view.selected.is_some() => {
                    view.move_selection(step, page, cells, text);
                }
                (_, Some(_)) => {
                    if let Some(delta) = scroll(page as isize) {
                        view.scroll_rows(delta, cells, text);
                    }
                }
                // Delete and Backspace empty the selected cell.
                (key::DELETE | key::FORWARD_DELETE, None)
                    if view.selected.is_some() && !command && !control =>
                {
                    drop(state);
                    self.table_edit(Op::ClearCell);
                    return Some(true);
                }
                // Menus still work.
                (_, None) if command || control => return None,
                // Typing on a cell replaces its value: the field opens
                // empty and the key goes through the input system into it.
                (_, None) if view.selected.is_some() && types_a_character(event) => {
                    drop(state);
                    if self.start_cell_edit(true) {
                        self.interpret(event);
                    }
                }
                (_, None) => return Some(true),
            }
        }
        self.request_redraw();
        self.pump();
        Some(true)
    }
}
