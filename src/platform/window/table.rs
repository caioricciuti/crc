//! CSV and TSV documents in the editor: the grid's keys and the Grid or
//! Text switch. The view itself is `crate::platform::table`.

use super::*;
use crate::platform::table::{Cells, TableView};

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

    /// Keys while the grid shows. `None` to handle the key as usual.
    pub(super) fn handle_table_key(&self, event: &NSEvent) -> Option<bool> {
        const ESCAPE: u16 = 53;
        let flags = event.modifierFlags();
        let command = flags.contains(NSEventModifierFlags::Command);
        let control = flags.contains(NSEventModifierFlags::Control);
        let text = self.chrome().text;
        let delta = |page: isize| match event.keyCode() {
            key::UP => Some(-1),
            key::DOWN => Some(1),
            key::PAGE_UP => Some(-page),
            key::PAGE_DOWN => Some(page),
            key::HOME => Some(isize::MIN / 2),
            key::END => Some(isize::MAX / 2),
            _ => None,
        };
        if event.keyCode() == ESCAPE {
            self.set_table_mode(false);
            return Some(true);
        }
        {
            let mut state = self.state_mut()?;
            let State {
                docs,
                tables,
                renderer,
                ..
            } = &mut *state;
            let cells = Cells::of(&renderer.atlas);
            let page =
                (crate::platform::table::rows_visible(text, cells.line_height) as isize - 1).max(1);
            let Some(delta) = delta(page) else {
                // Menus still work; anything else would type into a grid
                // that cannot take it yet.
                return if command || control { None } else { Some(true) };
            };
            if let Some(view) = tables.get_mut(&docs.active().id()) {
                view.scroll_rows(delta, cells, text);
            }
        }
        self.request_redraw();
        self.pump();
        Some(true)
    }
}
