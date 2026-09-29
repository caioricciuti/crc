//! The Claude Code bridge in the window: its server following the project,
//! the selection it is told about, and the review of a proposed change.

use super::*;

impl EditorView {
    /// The wake-up the IDE server's threads use: a main-thread poll.
    pub(super) fn claude_wake(&self) -> crate::ide::ws::Wake {
        self.wake(EditorView::poll_claude)
    }

    /// After every frame: the bridge follows the project root, and a moved
    /// selection is noted so Claude hears about it once it settles. A
    /// comparison or two when nothing changed, which keeps it off the
    /// typing budget.
    pub(super) fn claude_after_frame(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let root = state.tree.root().map(Path::to_path_buf);
        if let Some(new_root) = root.as_ref().filter(|_| state.claude_tried != root) {
            let new_root = new_root.clone();
            state.claude_tried = root;
            drop(state);
            self.claude_follow_root(&new_root);
            state = match self.state_mut() {
                Some(state) => state,
                None => return,
            };
        }
        let buffer = state.docs.active();
        let range = buffer
            .selection()
            .unwrap_or(buffer.cursor()..buffer.cursor());
        let key = (buffer.id(), range.start, range.end);
        let Some(bridge) = state.claude.as_mut() else {
            return;
        };
        if bridge.selection_seen == Some(key) {
            return;
        }
        bridge.selection_seen = Some(key);
        if !bridge.is_connected() {
            return;
        }
        bridge.selection_changed_at = Some(Instant::now());
        drop(state);
        self.resume_display_link();
    }

    /// Starts the bridge for `root`, or points the running one at it.
    pub(super) fn claude_follow_root(&self, root: &Path) {
        if std::env::var_os("CRC_NO_CLAUDE").is_some() {
            return;
        }
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let result = match state.claude.as_mut() {
            Some(bridge) if bridge.serves(root) => Ok(()),
            Some(bridge) => bridge.set_root(root),
            None => crate::platform::claude::Bridge::start(root, self.claude_wake())
                .map(|bridge| state.claude = Some(bridge)),
        };
        if let Err(e) = result {
            state.message = Some((format!("Claude Code cannot connect: {e}"), Instant::now()));
            drop(state);
            self.resume_display_link();
        }
    }

    /// Sends the selection once it has been still for 100 ms, if it is not
    /// what Claude already has.
    pub(super) fn claude_flush_selection(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let due = state
            .claude
            .as_ref()
            .and_then(|c| c.selection_changed_at)
            .is_some_and(|at| at.elapsed() >= Duration::from_millis(100));
        if !due {
            return;
        }
        let selection = claude_selection(&state);
        let Some(bridge) = state.claude.as_mut() else {
            return;
        };
        bridge.selection_changed_at = None;
        if let Some(selection) = selection
            && bridge.is_connected()
            && bridge.selection_sent.as_ref() != Some(&selection)
        {
            bridge.send(&crate::ide::mcp::selection_changed(&selection));
            bridge.selection_sent = Some(selection);
        }
    }

    /// Handles what the IDE server's threads delivered.
    pub(super) fn poll_claude(&self) {
        use crate::ide::ws::Event;
        loop {
            let event = {
                let Some(mut state) = self.state_mut() else {
                    // Busy: the next wake or frame tries again.
                    self.resume_display_link();
                    return;
                };
                match state.claude.as_mut() {
                    Some(bridge) => bridge.next_event(),
                    None => None,
                }
            };
            let Some(event) = event else {
                break;
            };
            match event {
                Event::Connected(_) => self.claude_note("Claude Code connected"),
                Event::Closed(_) => {
                    let gone = self
                        .ivars()
                        .state
                        .borrow()
                        .claude
                        .as_ref()
                        .is_some_and(|c| !c.is_connected());
                    if gone {
                        self.claude_note("Claude Code disconnected");
                    }
                }
                Event::Text(id, text) => {
                    let current = self
                        .ivars()
                        .state
                        .borrow()
                        .claude
                        .as_ref()
                        .is_some_and(|c| c.is_current(id));
                    if !current {
                        continue;
                    }
                    let reply = crate::ide::mcp::handle(&text, &mut ClaudeHost(self));
                    if let Some(reply) = reply {
                        let Some(state) = self.state() else {
                            return;
                        };
                        if let Some(bridge) = &state.claude {
                            bridge.send(&reply);
                        }
                    }
                }
            }
        }
        self.request_redraw();
        self.pump();
    }

    /// On the way out: Claude hears no to anything still waiting, and the
    /// lock file goes with the bridge so `claude` stops offering this window.
    pub(super) fn claude_shutdown(&self) {
        let Some(bridge) = self.state_mut().map(|mut state| state.claude.take()) else {
            return;
        };
        if let Some(mut bridge) = bridge {
            bridge.reject_all();
        }
    }

    pub(super) fn claude_note(&self, text: &str) {
        if let Some(mut state) = self.state_mut() {
            state.message = Some((text.to_owned(), Instant::now()));
        }
        self.resume_display_link();
    }

    /// Answers the review in the active tab.
    pub(super) fn claude_decide(&self, accept: bool) {
        let answered = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let id = state.docs.active().id();
            state
                .claude
                .as_mut()
                .is_some_and(|bridge| bridge.decide(id, accept))
        };
        if !answered {
            return;
        }
        self.claude_note(if accept {
            "Accepted. Claude writes the file."
        } else {
            "Rejected. The file is unchanged."
        });
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
        self.request_redraw();
        self.pump();
    }

    /// Keys in a review tab. `None` to handle the key as usual.
    pub(super) fn handle_review_key(&self, event: &NSEvent) -> Option<bool> {
        const ESCAPE: u16 = 53;
        let flags = event.modifierFlags();
        let command = flags.contains(NSEventModifierFlags::Command);
        let control = flags.contains(NSEventModifierFlags::Control);
        let text = self.chrome().text;
        let page = ((text.height / crate::platform::git_panel::DIFF_LINE) as isize - 2).max(1);
        let delta = match event.keyCode() {
            key::RETURN | key::KEYPAD_ENTER if command => {
                self.claude_decide(true);
                return Some(true);
            }
            ESCAPE => {
                self.claude_decide(false);
                return Some(true);
            }
            key::UP => -1,
            key::DOWN => 1,
            key::PAGE_UP => -page,
            key::PAGE_DOWN => page,
            key::HOME => isize::MIN / 2,
            key::END => isize::MAX / 2,
            // Shortcuts keep working: closing the tab, switching tabs.
            _ if command || control => return None,
            // Anything else would type into Claude's text.
            _ => return Some(true),
        };
        {
            let mut state = self.state_mut()?;
            let id = state.docs.active().id();
            if let Some(review) = state.claude.as_mut().and_then(|c| c.reviews.get_mut(&id)) {
                review.scroll_by(delta, text);
            }
        }
        self.request_redraw();
        self.pump();
        Some(true)
    }

    /// Closes the tab of the review `id`, wherever it is.
    pub(super) fn claude_close_review(&self, id: u64) {
        let focused = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if let Some(bridge) = state.claude.as_mut() {
                bridge.decide(id, false);
                bridge.reviews.remove(&id);
            }
            let focused = state.docs.iter().position(|b| b.id() == id);
            if focused.is_none() {
                let found =
                    state.panes.iter().enumerate().find_map(|(p, pane)| {
                        Some((p, pane.docs.iter().position(|b| b.id() == id)?))
                    });
                if let Some((p, index)) = found {
                    if state.panes[p].docs.len() > 1 {
                        state.panes[p].docs.close(index);
                    } else {
                        // The review was all the pane held: the pane goes,
                        // as it would for its last tab closed by hand. A
                        // review is never dirty, so nothing is asked.
                        state.panes.remove(p);
                        if p < state.focused_pane {
                            state.focused_pane -= 1;
                        }
                    }
                }
            }
            focused
        };
        if let Some(index) = focused {
            self.close_tab(index);
        }
        self.request_redraw();
        self.pump();
    }
}
