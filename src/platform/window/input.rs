//! Keys, text input and the pointer in the text: the key handler, the input
//! method's text, edits and what follows them, drag selection, scrolling.

use super::*;

impl EditorView {
    /// Scrolls whatever is under `x, y`: the tab strip, the sidebar, the
    /// Markdown blocks or the text. `precise` is a trackpad, whose deltas are
    /// points; otherwise they are wheel notches.
    pub(super) fn scroll_at(&self, x: f32, y: f32, dx: f64, dy: f64, precise: bool) {
        if dy.abs() < 0.01 && dx.abs() < 0.01 {
            return;
        }
        // Scrolling another pane scrolls it, which means focusing it:
        // there is one scroll position per focused document here.
        let over_pane = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            match frame_of(&mut state).hit(x, y) {
                Some(Hit::Pane(index)) => Some(*index),
                _ => None,
            }
        };
        if let Some(index) = over_pane {
            self.focus_pane(index);
        }
        let chrome = self.chrome();
        let (rows, _) = self.grid();

        let Some(mut state) = self.state_mut() else {
            return;
        };
        let m = state.renderer.atlas.metrics;
        if chrome.tabs.contains(x, y) {
            let delta = if dx.abs() > dy.abs() { dx } else { dy };
            state.tab_scroll_carry -= if precise {
                delta / 40.0
            } else {
                delta.signum()
            };
            let tabs = state.tab_scroll_carry.trunc() as isize;
            state.tab_scroll_carry -= tabs as f64;
            if tabs != 0 {
                state.tab_scroll =
                    layout::scroll_clamped(state.tab_scroll, tabs, state.docs.len(), 1);
                drop(state);
                self.request_redraw();
                self.pump();
            }
            return;
        }

        // A trackpad reports points, a wheel reports notches. Either way
        // the part that does not make a whole line is kept for the next
        // event rather than rounded away: rounding each event alone meant
        // a slow two-finger scroll, whose deltas are all under half a
        // line, never moved at all, and the tail of every flick was lost.
        let (per_line, per_column) = if precise {
            (m.line_height as f64, m.advance as f64)
        } else {
            (1.0 / WHEEL_LINES_PER_NOTCH, 1.0 / WHEEL_LINES_PER_NOTCH)
        };
        // A trackpad moves the text itself by points. Wheel notches, the
        // sidebar still goes a whole line at a time.
        let over_sidebar = chrome.sidebar.is_some_and(|r| r.contains(x, y));
        // The columns scroll by their own rows.
        if !over_sidebar && chrome.text.contains(x, y) && side_by_side(&state) {
            let per_row = if precise {
                crate::platform::conflicts::SIDE_LINE as f64
            } else {
                1.0 / WHEEL_LINES_PER_NOTCH
            };
            state.scroll_carry.1 -= dy / per_row;
            let rows = state.scroll_carry.1.trunc();
            state.scroll_carry.1 -= rows;
            let total = state.docs.active().rope.len_lines();
            if let Some(view) = active_conflicts_mut(&mut state) {
                view.scroll = view.scroll.saturating_add_signed(rows as isize);
                crate::platform::conflicts::clamp_side_scroll(view, total, chrome.text);
            }
            drop(state);
            self.request_redraw();
            self.pump();
            return;
        }
        if precise && !over_sidebar {
            state.scroll_carry.1 = 0.0;
            state
                .docs
                .active_mut()
                .scroll_smooth_by((-dy / m.line_height as f64) as f32, rows);
            state.scroll_carry.0 -= dx / per_column;
            let columns = state.scroll_carry.0.trunc();
            state.scroll_carry.0 -= columns;
            state
                .docs
                .active_mut()
                .scroll_columns_by(columns as isize, rows);
            drop(state);
            self.request_redraw();
            self.pump();
            return;
        }
        state.scroll_carry.0 -= dx / per_column;
        state.scroll_carry.1 -= dy / per_line;
        let (columns, lines) = (state.scroll_carry.0.trunc(), state.scroll_carry.1.trunc());
        state.scroll_carry.0 -= columns;
        state.scroll_carry.1 -= lines;
        let (columns, lines) = (columns as isize, lines as isize);
        if columns == 0 && lines == 0 {
            return;
        }

        if debug_scroll() {
            eprintln!(
                "scroll dy={dy:.1} precise={} | lines={lines} rows={rows} | scroll_line={} of {}",
                precise,
                state.docs.active().scroll_line,
                state.docs.active().rope.len_lines(),
            );
        }

        match chrome.sidebar {
            Some(rect) if rect.contains(x, y) => {
                state.tree.scroll_by(lines, layout::sidebar_rows(rect));
            }
            _ => {
                let buffer = state.docs.active_mut();
                buffer.scroll_by(lines, rows);
                buffer.scroll_columns_by(columns, rows);
            }
        }
        drop(state);
        self.request_redraw();
        self.pump();
    }

    /// Whole lines to scroll `target` for a wheel event, forward (down the
    /// content) positive. A trackpad's point deltas add up across events; a
    /// mouse wheel's notch is three lines, and a smooth wheel's fraction of
    /// a notch adds up too. Call with the state not borrowed.
    pub(super) fn wheel_lines(&self, event: &NSEvent, target: WheelTarget) -> isize {
        let dy = event.scrollingDeltaY();
        let (owner, rest) = self.ivars().wheel_rest.get();
        let rest = if owner == Some(target) { rest } else { 0.0 };
        let total = if event.hasPreciseScrollingDeltas() {
            let line = self
                .state()
                .map_or(16.0, |s| s.renderer.atlas.metrics.line_height as f64);
            rest - dy / line.max(1.0)
        } else {
            rest - dy * 3.0
        };
        let lines = total.trunc();
        self.ivars().wheel_rest.set((Some(target), total - lines));
        lines as isize
    }

    /// Records that a human pressed a key or clicked at `at`.
    ///
    /// Only the *oldest* unpresented input timestamp is kept: if three
    /// keystrokes land inside one refresh, the latency that matters is how
    /// long the first one waited to appear.
    pub(super) fn note_input(&self, at: Instant) {
        self.ivars().needs_redraw.set(true);
        let Some(mut state) = self.state_mut() else {
            return;
        };
        state.caret_since = at;
        if state.pending_input.is_none() {
            state.pending_input = Some(at);
        }
    }

    /// A press in the text: Cmd-click goes to a definition, a click on a
    /// completion chip takes it, and otherwise the caret or selection starts
    /// there, by character, word or line as the click count says.
    pub(super) fn text_press(&self, event: &NSEvent, started: Instant) {
        let offset = self.offset_for_event(event);
        let flags = event.modifierFlags();
        let shift = flags.contains(NSEventModifierFlags::Shift);
        let option = flags.contains(NSEventModifierFlags::Option);
        if flags.contains(NSEventModifierFlags::Command) && !shift && !option {
            // Cmd-click: go to the definition of what is under the pointer.
            if let Some(mut state) = self.state_mut() {
                state.docs.active_mut().place_cursor(offset, Motion::Move);
            }
            self.goto_definition(Some(offset));
            self.note_input(started);
            self.pump();
            return;
        }
        let chip = {
            let Some(state) = self.state() else {
                return;
            };
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            state
                .completion_chips
                .iter()
                .find(|(rect, _)| rect.contains(point.x as f32, point.y as f32))
                .map(|(_, index)| *index)
        };
        if let Some(index) = chip {
            self.accept_completion(Some(index));
            self.note_input(started);
            self.pump();
            return;
        }
        if let Some(mut state) = self.state_mut() {
            state.completion = None;
        }
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let buffer = state.docs.active_mut();
            // One click places the caret, two take a word, three a line,
            // and a drag that follows keeps selecting in the same unit.
            let pressed = match event.clickCount() {
                2 => buffer.word_range_at(offset),
                n if n >= 3 => buffer.line_range_at(offset),
                _ => offset..offset,
            };
            let unit = match event.clickCount() {
                2 => SelectUnit::Word,
                n if n >= 3 => SelectUnit::Line,
                _ => SelectUnit::Character,
            };
            if unit != SelectUnit::Character && !option && !shift {
                buffer.select_range(pressed.start, pressed.end);
                state.drag = Some(Drag::Select {
                    unit,
                    pressed,
                    point: None,
                });
                drop(state);
                self.note_input(started);
                self.pump();
                return;
            }
            state.drag = Some(Drag::Select {
                unit,
                pressed,
                point: None,
            });
        }
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if option {
                // Option-click drops an extra cursor instead of moving
                // the one you have.
                state.docs.active_mut().add_cursor(offset, offset);
            } else {
                // Shift-click extends an existing selection rather than
                // starting a new one, matching every other editor.
                let motion = if shift { Motion::Extend } else { Motion::Move };
                state.docs.active_mut().place_cursor(offset, motion);
            }
        }
        self.note_input(started);
        self.pump();
    }

    /// Applies a key event. Returns whether anything changed.
    pub(super) fn handle_key(&self, event: &NSEvent) -> bool {
        // Ctrl-` by its key, not its character: on layouts where that key
        // types something else the menu's shortcut never matches.
        const GRAVE: u16 = 50;
        let flags = event.modifierFlags();
        if event.keyCode() == GRAVE
            && flags.contains(NSEventModifierFlags::Control)
            && !flags.contains(NSEventModifierFlags::Command)
        {
            let _: () = unsafe { msg_send![self, toggleTerminal: None::<&AnyObject>] };
            return true;
        }
        if self.terminal_has_keys() {
            return self.terminal_key(event);
        }
        // Typing goes to the document, so the tree no longer has the keys.
        if !event
            .modifierFlags()
            .contains(NSEventModifierFlags::Command)
            && let Some(mut state) = self.state_mut()
        {
            state.sidebar_keys = false;
        }
        let reviewing = {
            let Some(state) = self.state() else {
                return false;
            };
            active_review(&state).is_some() && !field_has_keys(&state)
        };
        if reviewing && let Some(handled) = self.handle_review_key(event) {
            return handled;
        }
        let columns = {
            let Some(state) = self.state() else {
                return false;
            };
            side_by_side(&state) && !field_has_keys(&state)
        };
        if columns && let Some(handled) = self.handle_conflict_side_key(event) {
            return handled;
        }
        // Escape with no overlay open drops back to a single cursor, which
        // is the only way out of a multi-cursor edit.
        if event
            .modifierFlags()
            .contains(NSEventModifierFlags::Command | NSEventModifierFlags::Option)
            && event.keyCode() == 5
        {
            self.open_git();
            return true;
        }
        // Backspace with Command or Option in a field edits the field: to
        // the start of it, or one word. It never reaches the document behind
        // the field, and it never reaches the tree, where Cmd-Delete is Move
        // to Trash. The menu item is greyed out while a field has the keys,
        // which is what lets the key arrive here at all.
        {
            let flags = event.modifierFlags();
            let command = flags.contains(NSEventModifierFlags::Command);
            let option = flags.contains(NSEventModifierFlags::Option);
            if event.keyCode() == key::DELETE
                && (command || option)
                && !self.state().is_some_and(|state| field_has_keys(&state))
            {
                // The document's own handling below.
            } else if event.keyCode() == key::DELETE && (command || option) {
                let ((), focus) = self.edit_focused(|b, _| {
                    if command {
                        b.delete_to_line_start();
                    } else {
                        b.delete_word_backward();
                    }
                });
                self.after_focused_edit(focus);
                self.pump();
                return true;
            }
        }
        if self.state().is_some_and(|state| state.completion.is_some())
            && self.handle_completion_key(event)
        {
            return true;
        }
        {
            let flags = event.modifierFlags();
            let plain = !flags.contains(NSEventModifierFlags::Command)
                && !flags.contains(NSEventModifierFlags::Option);
            let Some(overlay) = self.state().map(|state| field_has_keys(&state)) else {
                return false;
            };
            let shift = flags.contains(NSEventModifierFlags::Shift);
            match event.keyCode() {
                // The menu has these too; handled here as well because a
                // key equivalent without Command is not always offered to
                // the menu first, and Shift-F12 must not fall to F12.
                key::F12 if plain && shift && !overlay => {
                    self.find_references();
                    return true;
                }
                key::F12 if plain && !overlay => {
                    self.goto_definition(None);
                    return true;
                }
                key::F2 if plain && !shift && !overlay => {
                    self.start_rename();
                    return true;
                }
                key::F
                    if shift
                        && flags.contains(NSEventModifierFlags::Option)
                        && !flags.contains(NSEventModifierFlags::Command)
                        && !overlay =>
                {
                    self.format_document(false);
                    return true;
                }
                key::O
                    if shift
                        && flags.contains(NSEventModifierFlags::Option)
                        && !flags.contains(NSEventModifierFlags::Command)
                        && !overlay =>
                {
                    self.organize_imports(false);
                    self.request_redraw();
                    return true;
                }
                key::F1 if plain && !overlay => {
                    self.show_hover();
                    return true;
                }
                key::SPACE if flags.contains(NSEventModifierFlags::Control) && !overlay => {
                    self.request_completion(true);
                    return true;
                }
                _ => {}
            }
        }
        if self
            .state()
            .is_some_and(|state| state.sidebar_edit.is_some())
        {
            return self.handle_sidebar_edit_key(event);
        }
        // Source Control claims its own keys (Escape, Cmd-Return, the
        // message field once clicked); everything else falls through to the
        // document beside it.
        let git_turn = {
            let Some(state) = self.state() else {
                return false;
            };
            // A field opened over it (the palette, find) keeps its keys.
            state.git_open && matches!(keys(&state), Keys::GitMessage | Keys::Document)
        };
        if git_turn && self.handle_git_key(event) {
            return true;
        }
        // The Extensions page has the column: Escape leaves its
        // confirmation, then the page; nothing types into the document
        // underneath. The palette, opened over it, keeps its own keys.
        if self.state().is_some_and(|state| ext_details(&state))
            && self.state().is_some_and(|state| state.palette.is_none())
            && !event
                .modifierFlags()
                .contains(NSEventModifierFlags::Command)
        {
            const ESCAPE: u16 = 53;
            if event.keyCode() == ESCAPE {
                let confirming = {
                    let Some(mut state) = self.state_mut() else {
                        return false;
                    };
                    let page = state.extensions.as_mut();
                    page.is_some_and(|p| p.confirm.take().is_some())
                };
                if !confirming {
                    let Some(mut state) = self.state_mut() else {
                        return false;
                    };
                    if let Some(page) = &mut state.extensions {
                        page.details = false;
                    }
                }
                self.request_redraw();
            }
            return true;
        }
        const ESCAPE_KEY: u16 = 53;
        if event.keyCode() == ESCAPE_KEY {
            let Some(state) = self.state() else {
                return false;
            };
            let idle = state.palette.is_none()
                && state.find.is_none()
                && state.goto.is_none()
                && state.rename.is_none();
            let tip = state.lsp.signature.is_some();
            drop(state);
            if idle && tip {
                if let Some(mut state) = self.state_mut() {
                    state.lsp.signature = None;
                }
                self.request_redraw();
                return true;
            }
            if idle {
                return self
                    .ivars()
                    .state
                    .borrow_mut()
                    .docs
                    .active_mut()
                    .collapse_cursors();
            }
        }

        // The palette is modal over everything, then the find bar. Only the
        // keys that mean something to each escape it.
        if self.state().is_some_and(|state| state.rename.is_some()) && self.handle_rename_key(event)
        {
            return true;
        }
        if self.state().is_some_and(|state| state.goto.is_some()) && self.handle_goto_key(event) {
            return true;
        }
        if self.state().is_some_and(|state| state.palette.is_some())
            && self.handle_palette_key(event)
        {
            return true;
        }
        if self.state().is_some_and(|state| {
            state
                .find
                .as_ref()
                .is_some_and(|bar| bar.has_keys || event.keyCode() == ESCAPE_KEY)
        }) && self.handle_find_key(event)
        {
            return true;
        }
        if self
            .state()
            .is_some_and(|state| state.docs.active().is_preview_file())
        {
            return false;
        }
        // Cmd-Return in a request file. A real keypress reaches this through
        // the Run menu's key equivalent; a scripted one arrives here.
        if event.keyCode() == key::RETURN
            && event
                .modifierFlags()
                .contains(NSEventModifierFlags::Command)
            && self.state().is_some_and(|state| can_send_from(&state))
        {
            self.send_request();
            return true;
        }

        let flags = event.modifierFlags();
        let command = flags.contains(NSEventModifierFlags::Command);
        let option = flags.contains(NSEventModifierFlags::Option);
        let control = flags.contains(NSEventModifierFlags::Control);
        let shift = flags.contains(NSEventModifierFlags::Shift);

        // Shift turns every motion into a selection extension. This is the
        // only place that mapping happens, so the buffer never has to know
        // what a modifier key is.
        let motion = if shift { Motion::Extend } else { Motion::Move };
        let code = match event.keyCode() {
            key::KEYPAD_ENTER => key::RETURN,
            code => code,
        };

        {
            let Some(mut state) = self.state_mut() else {
                return false;
            };
            let m = state.renderer.atlas.metrics;
            let gutter = layout::gutter_width(state.docs.active(), &state.renderer.atlas);
            // The text area's rows and columns, not the window's. Measured
            // against the whole window, the caret went three or four rows
            // under the last visible line before the view followed it.
            let text = chrome_of(&state).text;
            let rows = text.rows(m.line_height);
            let cols = text.columns(m.advance, gutter);
            let b = &mut state.docs.active_mut();

            // Navigation and deletion keys. Unlike letters, these take their
            // meaning from the modifiers, following the macOS conventions:
            // Option is word-wise, Command is line- or document-wise.
            let handled = match code {
                key::LEFT => {
                    if command {
                        b.move_line_start(motion)
                    } else if option {
                        b.move_word_left(motion)
                    } else {
                        b.move_left(motion)
                    }
                    true
                }
                key::RIGHT => {
                    if command {
                        b.move_line_end(motion)
                    } else if option {
                        b.move_word_right(motion)
                    } else {
                        b.move_right(motion)
                    }
                    true
                }
                key::UP => {
                    if command {
                        b.move_buffer_start(motion)
                    } else {
                        b.move_up(motion)
                    }
                    true
                }
                key::DOWN => {
                    if command {
                        b.move_buffer_end(motion)
                    } else {
                        b.move_down(motion)
                    }
                    true
                }
                key::DELETE => {
                    if command {
                        b.delete_to_line_start()
                    } else if option {
                        b.delete_word_backward()
                    } else {
                        b.backspace_paired()
                    }
                    true
                }
                key::FORWARD_DELETE => {
                    if command {
                        b.delete_to_line_end()
                    } else if option {
                        b.delete_word_forward()
                    } else {
                        b.delete_forward()
                    }
                    true
                }
                key::HOME => {
                    b.move_line_start(motion);
                    true
                }
                key::END => {
                    b.move_line_end(motion);
                    true
                }
                key::PAGE_UP => {
                    for _ in 0..rows.max(1) {
                        b.move_up(motion);
                    }
                    true
                }
                key::PAGE_DOWN => {
                    for _ in 0..rows.max(1) {
                        b.move_down(motion);
                    }
                    true
                }
                key::RETURN if !command => {
                    b.insert_newline_indented();
                    true
                }
                key::TAB if !command => {
                    // Tab indents a selection or a block; it only inserts a
                    // character when there is nothing to indent.
                    if b.selection().is_some() {
                        if shift { b.outdent() } else { b.indent() }
                    } else if shift {
                        b.outdent()
                    } else {
                        b.insert_tab()
                    }
                    true
                }
                _ => false,
            };

            if handled {
                b.scroll_to_cursor(rows, cols);
                return true;
            }

            // The Emacs-style Control bindings that every macOS text view
            // supports. Leaving these out is immediately noticeable to
            // anyone used to the platform.
            if control && !command {
                let base = event
                    .charactersIgnoringModifiers()
                    .and_then(|c| c.to_string().chars().next())
                    .map(|c| c.to_ascii_lowercase());
                let handled = match base {
                    Some('a') => {
                        b.move_line_start(motion);
                        true
                    }
                    Some('e') => {
                        b.move_line_end(motion);
                        true
                    }
                    Some('b') => {
                        b.move_left(motion);
                        true
                    }
                    Some('f') => {
                        b.move_right(motion);
                        true
                    }
                    Some('p') => {
                        b.move_up(motion);
                        true
                    }
                    Some('n') => {
                        b.move_down(motion);
                        true
                    }
                    Some('d') => {
                        b.delete_forward();
                        true
                    }
                    Some('k') => {
                        b.delete_to_line_end();
                        true
                    }
                    Some('h') => {
                        b.backspace();
                        true
                    }
                    _ => false,
                };
                if handled {
                    b.scroll_to_cursor(rows, cols);
                    return true;
                }
                return false;
            }
        }

        if command {
            // Cmd-1..9 jumps straight to a tab. Not menu items, because nine
            // of them would bury the rest of the Window menu.
            // The key's character, or the digit row's position where the
            // layout puts other characters there (AZERTY's `&é"'(§è!ç`).
            const DIGIT_ROW: [u16; 9] = [18, 19, 20, 21, 23, 22, 26, 28, 25];
            if let Some(digit) = event
                .charactersIgnoringModifiers()
                .and_then(|c| c.to_string().chars().next())
                .and_then(|c| c.to_digit(10))
                .or_else(|| {
                    DIGIT_ROW
                        .iter()
                        .position(|&code| code == event.keyCode())
                        .map(|i| i as u32 + 1)
                })
                .filter(|d| (1..=9).contains(d))
            {
                return self.activate_tab(digit as usize - 1);
            }
            // Every other Command shortcut is a menu item, and AppKit matches
            // those before the event reaches here.
            return false;
        }

        // Anything else is text, and text is the input system's to work out.
        // Reading `characters` off the event gets the key, not what the key
        // means: a dead key has no characters at all, so on a Portuguese or
        // Spanish layout the tilde, the circumflex and the backtick could not
        // be typed, and no input method could work.
        self.interpret(event)
    }

    /// Hands a key event to the system's text input machinery, which answers
    /// through the `NSTextInputClient` methods: `insertText:` for text,
    /// `setMarkedText:` for a composition in progress.
    pub(super) fn interpret(&self, event: &NSEvent) -> bool {
        // Nothing may be borrowed here. The answers arrive before this
        // returns, on this same stack.
        self.ivars().in_key_down.set(true);
        self.interpretKeyEvents(&NSArray::from_slice(&[event]));
        self.ivars().in_key_down.set(false);
        true
    }

    /// Puts committed text into whatever has the keyboard. Returns which.
    pub(super) fn commit_text(&self, text: &str, replacement: NSRange) -> Focus {
        let (rows, cols) = self.grid();
        let ((), focus) = self.edit_focused(|buffer, focus| {
            if focus == Focus::Document && buffer.is_preview_file() {
                return;
            }
            // A replacement range is the input system saying "instead of
            // that": the accent menu replacing the letter that was held.
            if replacement.location != NSNotFound as usize {
                buffer.select_input_range(replacement.location, replacement.length);
            }
            let text = match focus {
                Focus::Document => inserted_text(text),
                Focus::Goto => text.chars().filter(char::is_ascii_digit).collect(),
                Focus::FindQuery | Focus::Field => single_line(text),
            };
            if text.is_empty() {
                return;
            }
            // One character at a time through the pairing path; a longer
            // run came from an input method and goes in verbatim.
            let mut chars = text.chars();
            match (focus, chars.next(), chars.next()) {
                (Focus::Document, Some(ch), None) => buffer.insert_char_paired(ch),
                _ => buffer.insert(&text),
            }
            if focus == Focus::Document {
                buffer.scroll_to_cursor(rows, cols);
            }
        });
        match focus {
            Focus::FindQuery => self.refresh_find(),
            Focus::Field => {
                if let Some(mut state) = self.state_mut()
                    && let Some((_, selected)) = &mut state.palette
                {
                    *selected = 0;
                }
            }
            Focus::Document => {
                self.lsp_after_key(true);
                self.lsp_typed(text);
            }
            Focus::Goto => {}
        }
        focus
    }

    /// Gets a change that did not come from a key press onto the screen.
    pub(super) fn show_input_change(&self) {
        if self.ivars().in_key_down.get() {
            return;
        }
        self.reparse();
        self.sync_title();
        self.request_redraw();
        self.pump();
    }

    /// Runs `edit` on whatever text has the keyboard: an open overlay's field
    /// before the document, in the order `handle_key` offers them the keys.
    ///
    /// The Edit menu used to act on the document regardless, so Cmd-V with
    /// the find bar open pasted into the file behind it.
    pub(super) fn edit_focused<R>(&self, edit: impl FnOnce(&mut Buffer, Focus) -> R) -> (R, Focus) {
        // A review tab holds Claude's text, which is answered, not edited.
        // Menu edits land in a buffer nobody sees.
        let mut inert = Buffer::new();
        let reviewing = self
            .state()
            .is_some_and(|state| active_review(&state).is_some())
            || self.terminal_has_keys();
        let Some(mut state) = self.state_mut() else {
            return (edit(&mut inert, Focus::Goto), Focus::Goto);
        };
        let owner = keys(&state);
        let State {
            docs,
            find,
            palette,
            goto,
            git,
            sidebar_edit,
            rename,
            ..
        } = &mut *state;
        let field = match owner {
            Keys::SidebarEdit => sidebar_edit
                .as_mut()
                .map(|edit| (&mut edit.field, Focus::Field)),
            Keys::Rename => rename
                .as_mut()
                .map(|rename| (&mut rename.field, Focus::Field)),
            Keys::GitMessage => Some((&mut git.message, Focus::Field)),
            Keys::Goto => goto.as_mut().map(|field| (field, Focus::Goto)),
            Keys::Palette => palette.as_mut().map(|(query, _)| (query, Focus::Field)),
            Keys::FindQuery => find.as_mut().map(|bar| (&mut bar.query, Focus::FindQuery)),
            Keys::FindReplace => find
                .as_mut()
                .map(|bar| (&mut bar.replacement, Focus::Field)),
            Keys::Document => None,
        };
        let (buffer, focus) = match field {
            Some(field) => field,
            None if reviewing => (&mut inert, Focus::Goto),
            None => (docs.active_mut(), Focus::Document),
        };
        (edit(buffer, focus), focus)
    }

    /// What has to happen after [`Self::edit_focused`] changed some text.
    pub(super) fn after_focused_edit(&self, focus: Focus) {
        match focus {
            Focus::Document => return self.after_edit(),
            Focus::FindQuery => self.refresh_find(),
            Focus::Goto => {}
            Focus::Field => {
                // A different palette query invalidates the selected row.
                let Some(mut state) = self.state_mut() else {
                    return;
                };
                if let Some((_, selected)) = &mut state.palette {
                    *selected = 0;
                    state.palette_scroll = 0;
                }
            }
        }
        self.request_redraw();
        self.pump();
    }

    /// Shared tail for the menu actions: refresh the title and draw.
    pub(super) fn after_edit(&self) {
        self.reparse();
        let (rows, cols) = self.grid();
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let id = state.docs.active().id();
        if state.docs.active().path.is_some() {
            state.lsp.dirty.insert(id, Instant::now());
            state.gutter.dirty.insert(id, Instant::now());
        }
        follow_completion(&mut state);
        state.docs.active_mut().scroll_to_cursor(rows, cols);
        drop(state);
        self.resume_display_link();
        self.sync_title();
        self.request_redraw();
        self.pump();
    }

    /// Extends the selection to the last known drag point, scrolling first
    /// if that point is past the top or bottom of the text. Returns whether
    /// it is, which is to say whether this needs calling again next frame.
    pub(super) fn drag_select(&self) -> bool {
        let Some(mut state) = self.state_mut() else {
            return false;
        };
        let Some(Drag::Select {
            unit,
            pressed,
            point: Some((x, y)),
        }) = &state.drag
        else {
            return false;
        };
        let (unit, pressed, x, y) = (*unit, pressed.clone(), *x, *y);
        let chrome = chrome_of(&state);
        let text = chrome.text;
        let m = state.renderer.atlas.metrics;
        let rows = text.rows(m.line_height);

        // How far past the edge sets the speed: a nudge creeps, a long way
        // out runs. In lines per second, paid out by the frame.
        let past = if y < text.y {
            y - text.y
        } else if y > text.y + text.height {
            y - (text.y + text.height)
        } else {
            0.0
        };
        if past != 0.0 {
            let speed = past.signum() * (8.0 + past.abs() * 1.5).min(240.0);
            state.autoscroll_carry += speed * state.frame_interval.as_secs_f32();
            let lines = state.autoscroll_carry.trunc();
            state.autoscroll_carry -= lines;
            state.docs.active_mut().scroll_by(lines as isize, rows);
        } else {
            state.autoscroll_carry = 0.0;
        }

        // The point is pulled inside the text first, so dragging above the
        // window selects up to the first visible line, not to somewhere
        // computed from a negative row.
        let inside = (
            x.clamp(text.x, text.x + text.width - 1.0),
            y.clamp(text.y, text.y + (text.height - 1.0).max(0.0)),
        );
        let (tx, ty) = chrome.to_text(inside.0, inside.1);
        let offset = layout::offset_at_point(
            state.docs.active(),
            &state.renderer.atlas,
            &layout::Markdown::of(state.syntax.markdown(state.docs.active().id())),
            tx,
            ty,
        );

        let buffer = state.docs.active_mut();
        let reached = match unit {
            SelectUnit::Character => offset..offset,
            SelectUnit::Word => buffer.word_range_at(offset),
            SelectUnit::Line => buffer.line_range_at(offset),
        };
        // Whatever the press selected stays selected; the drag grows it
        // towards the pointer on whichever side the pointer is.
        if reached.start < pressed.start {
            buffer.select_range(pressed.end, reached.start);
        } else if unit == SelectUnit::Character {
            buffer.place_cursor(offset, Motion::Extend);
        } else {
            buffer.select_range(pressed.start, reached.end.max(pressed.end));
        }
        past != 0.0
    }

    /// Converts a mouse event to a byte offset in the buffer.
    pub(super) fn offset_for_event(&self, event: &NSEvent) -> usize {
        let window_point = event.locationInWindow();
        let point = self.convertPoint_fromView(window_point, None);
        let Some(state) = self.state() else {
            return 0;
        };
        // The hit test works in the text area's own coordinates. Handing it
        // window coordinates is what put every click two rows low and, with
        // the sidebar showing, a sidebar's width to the right.
        let (x, y) = chrome_of(&state).to_text(point.x as f32, point.y as f32);
        layout::offset_at_point(
            state.docs.active(),
            &state.renderer.atlas,
            &layout::Markdown::of(state.syntax.markdown(state.docs.active().id())),
            x,
            y,
        )
    }
}
