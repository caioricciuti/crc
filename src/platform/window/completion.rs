//! Completion in the window: asking the servers and the local worker,
//! the popup's keys, and accepting a candidate.

use super::*;

impl EditorView {
    /// Asks for completions at the caret: the server when the file has one,
    /// and the worker for words, the project index, paths and history.
    /// `manual` asks even when nothing has been typed yet.
    pub(super) fn request_completion(&self, manual: bool) {
        // Whatever was typed goes to the server first, so it answers about
        // the text as it is now.
        self.lsp_flush_now();
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let path = completion_path_query(&state);
        let buffer = state.docs.active();
        let caret = buffer.cursor();
        let id = buffer.id();
        let line_start = buffer.rope.line_to_byte(buffer.rope.byte_to_line(caret));
        let before = buffer.rope.slice_to_string(line_start..caret);
        let anchor = match &path {
            Some(query) => caret - query.partial.len(),
            None => caret - crate::complete::word_len(&before),
        };
        let prefix = buffer.rope.slice_to_string(anchor..caret);
        let context = crate::complete::context_before(&before[..anchor - line_start]);
        let language = completion_language(buffer);
        // A word needs two letters before it is worth a list of its own;
        // a path, a server trigger or a request by hand needs none.
        let worth = manual || path.is_some() || prefix.chars().count() >= 2;

        // The server, for words.
        let mut request = 0;
        if path.is_none()
            && let (Some(file), Some(lang)) = (buffer.path.clone(), lsp_language(buffer))
        {
            let at = crate::lsp::position_of(&buffer.rope, caret);
            let key = crate::lsp::servers::server_key(lang);
            if let Some(server) =
                ready_server(&mut state.lsp.servers, key).filter(|s| s.knows(&file))
            {
                request = server.completion(&file, at);
            }
        }
        if request == 0 && !worth {
            state.completion = None;
            return;
        }
        let previous = state
            .completion
            .take()
            .filter(|p| p.buffer == id && p.anchor == anchor && p.path == path.is_some());
        let (items, local, boosts, selected, chosen) = match previous {
            Some(p) => (p.items, p.local, p.boosts, p.selected, p.chosen),
            None => (Vec::new(), Vec::new(), Vec::new(), 0, false),
        };
        let mut popup = CompletionPopup {
            buffer: id,
            anchor,
            request,
            generation: 0,
            items,
            local,
            boosts,
            shown: Vec::new(),
            selected,
            chosen,
            path: path.is_some(),
            context,
            language,
        };
        popup.refilter(&prefix);
        state.completion = Some(popup);
        drop(state);
        if worth {
            self.ask_worker();
        }
    }

    /// Sends the worker the current question for the open list.
    pub(super) fn ask_worker(&self) {
        self.start_completer();
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let path = completion_path_query(&state);
        let root = project_key(&state);
        let buffer = state.docs.active();
        let caret = buffer.cursor();
        let Some(popup) = state
            .completion
            .as_ref()
            .filter(|p| p.buffer == buffer.id() && caret >= p.anchor)
        else {
            return;
        };
        let (anchor, context, language) =
            (popup.anchor, popup.context.clone(), popup.language.clone());
        let query = crate::complete::worker::Query {
            generation: state.completion_generation + 1,
            rope: buffer.rope.clone(),
            anchor,
            caret,
            prefix: buffer.rope.slice_to_string(anchor..caret),
            context,
            root,
            language,
            path,
        };
        state.completion_generation += 1;
        let generation = state.completion_generation;
        if let Some(popup) = state.completion.as_mut() {
            popup.generation = generation;
        }
        if let Some(worker) = &state.completer {
            worker.ask(query);
        }
    }

    pub(super) fn start_completer(&self) {
        if self.state().is_some_and(|state| state.completer.is_some()) {
            return;
        }
        let wake = self.wake(EditorView::poll_completion);
        let worker = crate::complete::worker::Worker::start(
            crate::complete::history::History::default_path(),
            wake,
        );
        if let Some(mut state) = self.state_mut() {
            state.completer = worker;
        }
    }

    /// Main thread, when the worker has answered.
    pub(super) fn poll_completion(&self) {
        let Some(mut state) = self.state_mut() else {
            self.resume_display_link();
            return;
        };
        if let Some(rows) = state.completer.as_ref().and_then(|w| w.forgotten()) {
            state.message = Some((
                format!(
                    "forgot {rows} remembered completion{} for this project",
                    if rows == 1 { "" } else { "s" }
                ),
                Instant::now(),
            ));
        }
        let Some(answer) = state.completer.as_ref().and_then(|w| w.take()) else {
            drop(state);
            self.request_redraw();
            self.pump();
            return;
        };
        let prefix = state
            .completion
            .as_ref()
            .filter(|p| p.generation == answer.generation)
            .and_then(|p| typed_since(state.docs.active(), p));
        if let (Some(prefix), Some(popup)) = (prefix, state.completion.as_mut()) {
            popup.local = answer.candidates;
            popup.boosts = answer.boosts;
            popup.refilter(&prefix);
        }
        drop(state);
        self.request_redraw();
        self.pump();
    }

    pub(super) fn handle_completion_key(&self, event: &NSEvent) -> bool {
        const ESCAPE: u16 = 53;
        // Option-1 to Option-9 take that suggestion, by key code: the
        // character Option makes depends on the keyboard layout.
        const DIGITS: [u16; 9] = [18, 19, 20, 21, 23, 22, 26, 28, 25];
        let flags = event.modifierFlags();
        if flags.contains(NSEventModifierFlags::Command) {
            return false;
        }
        let code = match event.keyCode() {
            key::KEYPAD_ENTER => key::RETURN,
            code => code,
        };
        let (count, chosen) = self
            .ivars()
            .state
            .borrow()
            .completion
            .as_ref()
            .map_or((0, false), |p| (p.shown.len(), p.chosen));
        if flags.contains(NSEventModifierFlags::Option)
            && let Some(n) = DIGITS.iter().position(|&d| d == code)
        {
            if n >= count {
                return false;
            }
            self.accept_completion(Some(n));
            self.request_redraw();
            self.pump();
            return true;
        }
        match code {
            ESCAPE => {
                if let Some(mut state) = self.state_mut() {
                    state.completion = None;
                }
            }
            key::UP | key::DOWN if count > 0 => {
                let Some(mut state) = self.state_mut() else {
                    return false;
                };
                if let Some(popup) = &mut state.completion {
                    popup.selected = if code == key::DOWN {
                        (popup.selected + 1) % count
                    } else {
                        (popup.selected + count - 1) % count
                    };
                    popup.chosen = true;
                }
            }
            key::TAB if count > 0 => self.accept_completion(None),
            key::RETURN if count > 0 && chosen => self.accept_completion(None),
            key::RETURN => {
                // Not chosen: a new line, as it would be with no list.
                if let Some(mut state) = self.state_mut() {
                    state.completion = None;
                }
                return false;
            }
            _ => return false,
        }
        self.request_redraw();
        self.pump();
        true
    }

    /// Replaces the prefix with the chosen suggestion (`index`, or the
    /// selected one), remembers the choice, and in a path goes on into a
    /// folder that was just completed.
    pub(super) fn accept_completion(&self, index: Option<usize>) {
        let (range, text, remember, into_folder) = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(popup) = state.completion.take() else {
                return;
            };
            let Some(candidate) = popup.shown.get(index.unwrap_or(popup.selected)).cloned() else {
                return;
            };
            let buffer = state.docs.active();
            let caret = buffer.cursor();
            let (range, text) = match candidate.server.and_then(|i| popup.items.get(i)) {
                Some(item) => (
                    crate::lsp::client::completion_range(item, &buffer.rope, caret),
                    crate::lsp::client::completion_text(item).to_owned(),
                ),
                None => (popup.anchor..caret, candidate.insert.clone()),
            };
            let remember = project_key(&state).map(|root| {
                (
                    root,
                    popup.language,
                    popup.context,
                    candidate.insert.clone(),
                )
            });
            (
                range,
                text,
                remember,
                popup.path && candidate.insert.ends_with('/'),
            )
        };
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let buffer = state.docs.active_mut();
            buffer.select_range(range.start, range.end);
            buffer.insert(&text);
        }
        self.after_edit();
        if let Some(mut state) = self.state_mut() {
            state.completion = None;
        }
        if let Some((root, language, context, text)) = remember
            && let Some(state) = self.state()
            && let Some(worker) = &state.completer
        {
            worker.accepted(root, language, context, text);
        }
        if into_folder {
            self.request_completion(false);
        }
    }

    /// Edit > Forget Completion History: this project's remembered picks.
    pub(super) fn forget_completion_history(&self) {
        self.start_completer();
        let Some(state) = self.state() else {
            return;
        };
        if let (Some(root), Some(worker)) = (project_key(&state), &state.completer) {
            worker.forget(root);
        }
    }
}
