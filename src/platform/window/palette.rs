//! The palette and go to line: opening it for a mode, its keys, clicks and
//! wheel, and running what was picked.

use super::*;

impl EditorView {
    /// Opens the go-to-line field.
    pub(super) fn open_goto(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        close_fields(&mut state);
        state.goto = Some(Buffer::new());
        drop(state);
        self.request_redraw();
        self.pump();
    }

    /// Keys while the go-to-line field is open.
    pub(super) fn handle_goto_key(&self, event: &NSEvent) -> bool {
        const ESCAPE: u16 = 53;
        if event
            .modifierFlags()
            .contains(NSEventModifierFlags::Command)
        {
            return false;
        }
        let code = match event.keyCode() {
            key::KEYPAD_ENTER => key::RETURN,
            code => code,
        };

        match code {
            ESCAPE => {
                if let Some(mut state) = self.state_mut() {
                    state.goto = None;
                }
            }
            key::RETURN => {
                let target = {
                    let Some(state) = self.state() else {
                        return false;
                    };
                    state
                        .goto
                        .as_ref()
                        .and_then(|b| b.rope.to_string().trim().parse::<usize>().ok())
                };
                {
                    let Some(mut state) = self.state_mut() else {
                        return false;
                    };
                    state.goto = None;
                    if let Some(line) = target {
                        // People count lines from one; the buffer counts from
                        // zero.
                        state.docs.active_mut().goto_line(line.saturating_sub(1));
                    }
                }
                let (rows, cols) = self.grid();
                self.ivars()
                    .state
                    .borrow_mut()
                    .docs
                    .active_mut()
                    .scroll_to_cursor(rows, cols);
            }
            key::DELETE | key::FORWARD_DELETE | key::LEFT | key::RIGHT | key::HOME | key::END => {
                let Some(mut state) = self.state_mut() else {
                    return false;
                };
                if let Some(b) = &mut state.goto {
                    field_key(b, code, event.modifierFlags());
                }
            }
            // Text, through the input system like everywhere else.
            // `commit_text` keeps only the digits for this field.
            _ => return self.interpret(event),
        }
        self.request_redraw();
        self.pump();
        true
    }

    /// Opens the Cmd-P palette. The project index arrives from its worker.
    pub(super) fn open_palette(&self) {
        self.open_palette_with("");
    }

    /// Opens the palette with `prefix` typed: `@` for the document's
    /// symbols, `#` for the project's.
    pub(super) fn open_palette_with(&self, prefix: &str) {
        if let Some(mut state) = self.state_mut() {
            close_fields(&mut state);
        }
        // Before the field takes the keyboard: validation asks the editor
        // which commands apply, and a focused field changes the answer.
        let commands = commands::from_menu(MainThreadMarker::from(self), |item| {
            let Some(action) = item.action() else {
                return false;
            };
            let handles: bool = unsafe { msg_send![self, respondsToSelector: action] };
            // Hide, Quit and Minimize belong to the app and the window.
            !handles || unsafe { msg_send![self, validateMenuItem: item] }
        });
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.commands = commands;
            let document = {
                let buffer = state.docs.active();
                buffer
                    .extension()
                    .and_then(|e| Language::from_extension(&e))
                    .map(|language| (language, buffer.rope.clone()))
            };
            let root = state.tree.root().map(Path::to_path_buf);
            state.symbols.reset(document, root);
            state.branch_list = None;
            state.repo_list = None;
            state.lsp.action_list = None;
            state.mcp_url_prompt = false;
            let mut query = Buffer::new();
            if !prefix.is_empty() {
                query.insert(prefix);
            }
            state.palette = Some((query, 0));
            state.palette_scroll = 0;
        }
        self.request_redraw();
        self.pump();
    }

    pub(super) fn palette_click(&self, x: f32, y: f32) {
        let chosen = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let rows = open_palette_rows(&state);
            let count = rows.len();
            let rect = layout::palette_rect(state.viewport, count);
            if !rect.contains(x, y) || layout::palette_escape(rect).contains(x, y) {
                drop(state);
                self.close_palette();
                return;
            }
            if y < rect.y + 52.0 {
                let State {
                    palette, renderer, ..
                } = &mut *state;
                if let Some((query, _)) = palette {
                    place_field_caret(
                        &mut renderer.atlas,
                        query,
                        x - rect.x - layout::PALETTE_INPUT_PAD,
                    );
                }
                drop(state);
                self.request_redraw();
                self.pump();
                return;
            }
            let Some(row) = layout::palette_row_at(rect, x, y) else {
                return;
            };
            let first = state.palette_scroll.min(layout::palette_max_scroll(
                count,
                layout::palette_visible_rows(rect),
            ));
            rows.into_iter().nth(first + row).map(|(_, pick)| pick)
        };
        if let Some(pick) = chosen {
            self.close_palette();
            self.run_pick(pick);
        }
    }

    /// Does what a palette row offers, clicked or chosen with Return.
    pub(super) fn run_pick(&self, pick: Pick) {
        match pick {
            Pick::Action(server, action) => self.run_code_action(server, action),
            Pick::Branch(name) => self.switch_branch(name, false),
            Pick::NewBranch(name) => self.switch_branch(name, true),
            Pick::DeleteBranch(name, unmerged) => self.delete_branch(name, unmerged),
            Pick::RenameBranch(name) => self.open_branch_picker(BranchIntent::RenameTo(name)),
            Pick::RenameBranchTo(old, new) => self.rename_branch(old, new),
            Pick::McpUrl(url) => self.add_mcp_url(url),
            Pick::Repo(path) => self.select_repo(&path),
            Pick::Symbol(path, line) => self.go_to_symbol(path, line),
            Pick::Command(at, tag) => self.run_command(at, tag),
            Pick::File(path) => {
                self.load_path(&path.to_string_lossy());
                // The title follows after the frame that shows the file:
                // setTitle can take several ms (PERF-001).
                if let Some(mut state) = self.state_mut() {
                    state.title_sync_pending = true;
                }
                self.reparse();
                if let Some(mut state) = self.state_mut() {
                    state.tree.reveal(&path);
                }
                self.request_redraw();
                self.pump();
            }
        }
    }

    /// Sends a command's action as its menu item would: to the editor when
    /// it handles it, otherwise down the responder chain to the window and
    /// the app. The editor first, because it is where the palette lives
    /// whether or not its window is key.
    pub(super) fn run_command(&self, action: Sel, tag: isize) {
        // The palette has no menu item to send: an extension command is
        // told by its tag.
        if action == sel!(runExtensionCommand:) {
            self.run_extension(tag);
            return;
        }
        let handles: bool = unsafe { msg_send![self, respondsToSelector: action] };
        if handles {
            // It returns the action's result as an object, void or not.
            let _: *mut AnyObject =
                unsafe { msg_send![self, performSelector: action, withObject: None::<&AnyObject>] };
        } else {
            let app = NSApplication::sharedApplication(MainThreadMarker::from(self));
            // SAFETY: the action is a menu item's, taking one sender argument
            // as every action does; a nil target is the responder chain.
            unsafe { app.sendAction_to_from(action, None, None) };
        }
    }

    /// Wheel and trackpad scrolling of the palette's list, a row at a time,
    /// keeping the part of a trackpad delta that has not made a row yet.
    pub(super) fn palette_wheel(&self, event: &NSEvent) {
        self.palette_wheel_by(event.scrollingDeltaY(), event.hasPreciseScrollingDeltas());
    }

    pub(super) fn palette_wheel_by(&self, dy: f64, precise: bool) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if state.palette.is_none() {
            return;
        }
        let count = state.palette_count;
        let visible = layout::palette_visible_rows(layout::palette_rect(state.viewport, count));
        state.palette_scroll_carry -= if precise {
            dy / (layout::PALETTE_ROW as f64 * 0.5)
        } else {
            dy
        };
        let rows = state.palette_scroll_carry.trunc() as isize;
        state.palette_scroll_carry -= rows as f64;
        let max = layout::palette_max_scroll(count, visible);
        let scroll = layout::scroll_clamped(state.palette_scroll.min(max), rows, count, visible);
        if scroll != state.palette_scroll {
            state.palette_scroll = scroll;
            drop(state);
            self.request_redraw();
            self.pump();
        }
    }

    pub(super) fn close_palette(&self) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.palette = None;
            state.branch_list = None;
            state.repo_list = None;
            state.lsp.action_list = None;
            state.mcp_url_prompt = false;
        }
        self.request_redraw();
        self.pump();
    }

    /// Keys while the palette is open. Returns whether it consumed them.
    pub(super) fn handle_palette_key(&self, event: &NSEvent) -> bool {
        const ESCAPE: u16 = 53;
        let flags = event.modifierFlags();
        let code = match event.keyCode() {
            key::KEYPAD_ENTER => key::RETURN,
            code => code,
        };
        if flags.contains(NSEventModifierFlags::Command) {
            // Cmd-Delete clears the field to the left of the caret. It has no
            // menu item, so returning false here left it doing nothing at all;
            // everything else with Command still belongs to the menus.
            if code == key::DELETE {
                let Some(mut state) = self.state_mut() else {
                    return false;
                };
                if let Some((query, selected)) = &mut state.palette {
                    if query.selection().is_none() {
                        query.move_line_start(Motion::Extend);
                    }
                    query.backspace();
                    *selected = 0;
                    state.palette_scroll = 0;
                    drop(state);
                    self.request_redraw();
                    self.pump();
                    return true;
                }
            }
            return false;
        }

        match code {
            ESCAPE => {
                self.close_palette();
                return true;
            }
            key::RETURN => {
                let chosen = {
                    let Some(state) = self.state() else {
                        return false;
                    };
                    let Some(selected) = state.palette.as_ref().map(|(_, s)| *s) else {
                        return false;
                    };
                    open_palette_rows(&state)
                        .into_iter()
                        .nth(selected)
                        .map(|(_, pick)| pick)
                };
                self.close_palette();
                if let Some(pick) = chosen {
                    self.run_pick(pick);
                }
                return true;
            }
            key::UP | key::DOWN => {
                let Some(mut state) = self.state_mut() else {
                    return false;
                };
                let count = open_palette_rows(&state).len();
                let visible =
                    layout::palette_visible_rows(layout::palette_rect(state.viewport, count));
                if let Some((_, selected)) = &mut state.palette {
                    if code == key::DOWN {
                        *selected = (*selected + 1).min(count.saturating_sub(1));
                    } else {
                        *selected = selected.saturating_sub(1);
                    }
                    let selected = *selected;
                    state.palette_scroll =
                        layout::palette_follow(state.palette_scroll, selected, count, visible);
                }
                drop(state);
                self.request_redraw();
                self.pump();
                return true;
            }
            _ => {}
        }

        {
            let mut as_text = false;
            let Some(mut state) = self.state_mut() else {
                return false;
            };
            let Some((query, selected)) = &mut state.palette else {
                return false;
            };
            // The field used to ignore modifiers on every key, so Option-
            // Delete removed one character instead of a word and Cmd-Delete
            // did the same. A query field is a text field; the standard
            // editing combinations have to reach it.
            let option = flags.contains(NSEventModifierFlags::Option);
            match code {
                key::DELETE if flags.contains(NSEventModifierFlags::Control) && !option => {
                    if query.selection().is_none() {
                        query.select_all();
                    }
                    query.backspace();
                }
                key::TAB => return true,
                _ => as_text = !field_key(query, code, flags),
            }
            if as_text {
                drop(state);
                return self.interpret(event);
            }
            // Any change to the query invalidates which row was selected.
            *selected = 0;
        }
        self.request_redraw();
        self.pump();
        true
    }
}
