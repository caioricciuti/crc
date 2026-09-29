//! Tabs and panes: switching, cycling, moving and closing tabs, and
//! splitting, focusing and closing panes.

use super::*;

impl EditorView {
    /// Brings tab `index` of the focused pane to the front: shown in the tab
    /// strip, named in the title, parsed. Whether it switched.
    pub(super) fn activate_tab(&self, index: usize) -> bool {
        let switched = {
            let Some(mut state) = self.state_mut() else {
                return false;
            };
            let switched = state.docs.switch(index);
            if switched {
                reveal_active_tab(&mut state);
            }
            switched
        };
        if switched {
            self.sync_title();
            self.reparse();
        }
        switched
    }

    /// The tab `step` along, round from the last to the first.
    pub(super) fn cycle_tab(&self, step: isize) {
        let Some(index) = self.state().and_then(|state| state.docs.cycled(step)) else {
            return;
        };
        self.activate_tab(index);
        self.request_redraw();
        self.pump();
    }

    /// Moves the keyboard `step` panes along, round from the last to the
    /// first.
    pub(super) fn cycle_pane(&self, step: isize) {
        let (focused, count) = {
            let Some(state) = self.state() else {
                return;
            };
            (state.focused_pane, pane_count(&state))
        };
        self.focus_pane((focused as isize + step).rem_euclid(count as isize) as usize);
    }

    /// Handles a click at `x` inside the tab bar.
    pub(super) fn tab_click(&self, x: f32) {
        let hit = self
            .ivars()
            .state
            .borrow()
            .tab_hits
            .iter()
            .find(|h| x >= h.x0 && x < h.x1)
            .copied();
        // The empty part of the bar does nothing, and does not fall through
        // to the text behind it either.
        let Some(hit) = hit else {
            return;
        };

        // The cross is drawn on the active tab and on the tab under the
        // pointer, which a clicked tab always is: a click on it closes.
        let on_cross = x >= hit.close_x0 && x < hit.close_x1;
        if on_cross {
            self.close_tab(hit.index);
        } else {
            self.activate_tab(hit.index);
        }
        self.request_redraw();
        self.pump();
    }

    pub(super) fn move_context_tab(&self, direction: isize) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some(from) = state.context_tab else {
            return;
        };
        let Some(to) = from.checked_add_signed(direction) else {
            return;
        };
        if state.docs.move_tab(from, to) {
            state.context_tab = Some(to);
            drop(state);
            self.request_redraw();
            self.pump();
        }
    }

    /// Closes a tab, asking about unsaved changes first.
    pub(super) fn close_tab(&self, index: usize) {
        // Closing a review is answering it.
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let id = state.docs.iter().nth(index).map(Buffer::id);
            if let Some(id) = id
                && let Some(bridge) = state.claude.as_mut()
            {
                bridge.decide(id, false);
                bridge.reviews.remove(&id);
            }
        }
        let dirty = {
            let Some(state) = self.state() else {
                return;
            };
            state.docs.iter().nth(index).is_some_and(|b| b.is_dirty())
        };
        let id = self
            .ivars()
            .state
            .borrow()
            .docs
            .iter()
            .nth(index)
            .map(Buffer::id);
        if dirty {
            // Show the prompt against the document being closed, which means
            // switching to it first so the alert names the right file. The
            // alert spins a nested run loop, so nothing may be borrowed
            // across it.
            self.activate_tab(index);
            if !self.confirm_discard() {
                return;
            }
        }
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            // By identity: the alert's run loop may have opened a tab (a
            // Claude proposal) and moved the one asked about.
            let Some(index) = id.and_then(|id| state.docs.iter().position(|b| b.id() == id)) else {
                return;
            };
            let closed = state.docs.close(index);
            if let Some(path) = closed.as_ref().and_then(|b| b.path.clone()) {
                for server in state.lsp.servers.values_mut() {
                    server.did_close(&path);
                }
            }
            if let Some(id) = closed.as_ref().map(Buffer::id) {
                forget_document(&mut state, id);
            }
            state.completion = None;
            reveal_active_tab(&mut state);
        }
        // A pane whose last tab just closed goes with it, unless it is
        // the only one.
        let emptied = {
            let Some(state) = self.state() else {
                return;
            };
            state.docs.is_home() && pane_count(&state) > 1
        };
        if emptied {
            let Some(focused) = self.state().map(|state| state.focused_pane) else {
                return;
            };
            self.close_pane(focused);
            return;
        }
        self.sync_title();
        self.reparse();
        self.request_redraw();
        self.pump();
    }

    /// Gives pane `index` the keyboard.
    pub(super) fn focus_pane(&self, index: usize) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if index == state.focused_pane || index >= pane_count(&state) {
                return;
            }
            let all = take_panes(&mut state);
            restore_panes(&mut state, all, index);
            state.find = None;
            state.goto = None;
            if matches!(state.drag, Some(Drag::Select { .. })) {
                state.drag = None;
            }
        }
        self.sync_title();
        self.reparse();
        self.request_redraw();
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
        self.pump();
    }

    /// Opens a new, empty pane to the right of the focused one and focuses
    /// it. A document lives in one pane only, so the new pane starts at
    /// Home; Cmd-P or the sidebar fills it.
    pub(super) fn split_pane(&self) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if pane_count(&state) >= 4 {
                state.message = Some(("four panes is the limit".into(), Instant::now()));
                drop(state);
                self.request_redraw();
                return;
            }
            let at = state.focused_pane + 1;
            let mut all = take_panes(&mut state);
            all.insert(
                at,
                PaneStore {
                    docs: Documents::new(Buffer::new()),
                    tab_scroll: 0,
                    tab_hits: Vec::new(),
                },
            );
            restore_panes(&mut state, all, at);
            state.find = None;
            state.goto = None;
        }
        self.sync_title();
        self.reparse();
        self.request_redraw();
        self.pump();
    }

    /// Closes pane `index`, asking about its unsaved documents first.
    pub(super) fn close_pane(&self, index: usize) {
        if self.state().is_some_and(|state| state.panes.is_empty()) {
            return;
        }
        self.focus_pane(index);
        // Its documents are asked about one by one, like closing tabs.
        loop {
            let dirty = {
                let Some(state) = self.state() else {
                    return;
                };
                state.docs.iter().position(|b| b.is_dirty())
            };
            let Some(at) = dirty else { break };
            self.activate_tab(at);
            if !self.confirm_discard() {
                return;
            }
            if let Some(mut state) = self.state_mut() {
                state.docs.close(at);
            }
        }
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if state.panes.is_empty() {
                return;
            }
            let focused = state.focused_pane;
            let mut all = take_panes(&mut state);
            all.remove(focused);
            restore_panes(&mut state, all, focused.saturating_sub(1));
            state.find = None;
            state.goto = None;
        }
        self.sync_title();
        self.reparse();
        self.request_redraw();
        self.pump();
    }
}
