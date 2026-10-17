//! The Home page's keys: Page Up and Down, Home and End scroll its
//! column while the Home tab has the keyboard. The wheel reaches it
//! through `scroll_at`, with the rest of the editor column.

use super::*;

impl EditorView {
    /// Scrolls Home by the page keys. True when the key was Home's: the
    /// Home tab is what is open, nothing else has the keys, and the key
    /// is plain.
    pub(super) fn handle_home_key(&self, event: &NSEvent) -> bool {
        let flags = event.modifierFlags();
        if flags.contains(NSEventModifierFlags::Command)
            || flags.contains(NSEventModifierFlags::Option)
            || flags.contains(NSEventModifierFlags::Control)
        {
            return false;
        }
        let Some(mut state) = self.state_mut() else {
            return false;
        };
        if !state.docs.is_home() || state.palette.is_some() || field_has_keys(&state) {
            return false;
        }
        let view = &mut state.home_view;
        match event.keyCode() {
            key::PAGE_UP => view.page_by(-1.0),
            key::PAGE_DOWN => view.page_by(1.0),
            key::HOME => view.to_top(),
            key::END => view.to_end(),
            _ => return false,
        }
        drop(state);
        self.request_redraw();
        true
    }
}
