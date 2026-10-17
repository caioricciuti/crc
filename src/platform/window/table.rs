//! CSV and TSV documents in the editor: the grid's keys and presses, and
//! the Grid or Text switch. The view itself is `crate::platform::table`.

use super::*;
use crate::platform::table::{Cells, Move, TableView};

/// Brings the active document's table up to date with its text. Cheap
/// when nothing changed.
pub(super) fn sync_tables(state: &mut State) {
    let State { docs, tables, .. } = state;
    crate::platform::table::sync(tables, docs.active());
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

    /// A press on the grid selects the cell under it, or the record whose
    /// number is under it.
    pub(super) fn table_press(&self, x: f32, y: f32) {
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
            if let Some(view) = tables.get_mut(&docs.active().id()) {
                let cell = view.cell_for_press(cells, text, x, y);
                view.select(cell, cells, text);
            }
        }
        self.request_redraw();
        self.pump();
    }

    /// Keys while the grid shows. `None` to handle the key as usual.
    ///
    /// With a cell selected the navigation keys move the selection; without
    /// one they scroll the records as a text view would. Escape drops the
    /// selection first and leaves for the text second. Return is kept for
    /// editing the cell, which comes next.
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
                (key::RETURN | key::KEYPAD_ENTER, _) => return Some(true),
                (_, Some(step)) if view.selected.is_some() => {
                    view.move_selection(step, page, cells, text);
                }
                (_, Some(_)) => {
                    if let Some(delta) = scroll(page as isize) {
                        view.scroll_rows(delta, cells, text);
                    }
                }
                // Menus still work; anything else would type into a grid
                // that cannot take it yet.
                (_, None) if command || control => return None,
                (_, None) => return Some(true),
            }
        }
        self.request_redraw();
        self.pump();
        Some(true)
    }
}
