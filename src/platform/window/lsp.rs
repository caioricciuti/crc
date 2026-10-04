//! The language servers in the window: starting them for open documents,
//! sending changes, reading what they answer, and the change marks.

use super::*;

impl EditorView {
    /// The wake-up a server's reader thread uses: a main-thread poll.
    pub(super) fn lsp_wake(&self) -> crate::lsp::transport::Wake {
        self.wake(EditorView::poll_lsp)
    }

    /// Starts servers for the languages of open documents and tells each
    /// ready server about the documents it does not know yet. Safe to call
    /// often; it only sends what is new.
    pub(super) fn lsp_sync_open(&self) {
        let root = self
            .ivars()
            .state
            .borrow()
            .tree
            .root()
            .map(Path::to_path_buf);
        let Some(root) = root else {
            return;
        };
        // Which languages are open, across panes.
        let mut wanted: Vec<(Language, std::path::PathBuf, u64)> = Vec::new();
        {
            let Some(state) = self.state() else {
                return;
            };
            for docs in all_docs(&state) {
                for buffer in docs.iter() {
                    if let Some(path) = &buffer.path
                        && let Some(language) = lsp_language(buffer)
                    {
                        wanted.push((language, path.clone(), buffer.id()));
                    }
                }
            }
        }
        for (language, _, _) in &wanted {
            let key = crate::lsp::servers::server_key(*language);
            let known = {
                let Some(state) = self.state() else {
                    return;
                };
                state.lsp.servers.contains_key(&key) || state.lsp.unavailable.contains_key(&key)
            };
            if known {
                continue;
            }
            match crate::lsp::servers::launch_for(*language) {
                Ok(launch) => {
                    let args: Vec<&str> = launch.args.iter().map(String::as_str).collect();
                    let started = crate::lsp::client::Server::start(
                        &launch.name,
                        &launch.program,
                        &args,
                        &root,
                        self.lsp_wake(),
                    );
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    match started {
                        Ok(server) => {
                            state.message =
                                Some((format!("{} starting", launch.name), Instant::now()));
                            state.lsp.servers.insert(key, server);
                        }
                        Err(error) => {
                            let reason = format!("{} failed to start: {error}", launch.name);
                            state.message = Some((reason.clone(), Instant::now()));
                            state.lsp.unavailable.insert(key, reason);
                        }
                    }
                }
                Err(reason) => {
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    // Said once per language, in the status line, and then
                    // the editor is simply an editor for that file. Not over
                    // something said just now (a save's outcome): that is
                    // news, this is not.
                    if state
                        .message
                        .as_ref()
                        .is_none_or(|(_, at)| at.elapsed() > Duration::from_secs(1))
                    {
                        state.message = Some((reason.clone(), Instant::now()));
                    }
                    state.lsp.unavailable.insert(key, reason);
                }
            }
        }
        // Open documents in ready servers.
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let State {
            lsp: Lsp { servers: lsp, .. },
            docs,
            panes,
            ..
        } = &mut *state;
        for (language, path, id) in wanted {
            let key = crate::lsp::servers::server_key(language);
            let Some(server) = lsp.get_mut(&key) else {
                continue;
            };
            if !server.is_ready() || server.knows(&path) {
                continue;
            }
            let text = buffer_by_id(docs, panes, id).map(|b| b.rope.to_string());
            if let Some(text) = text {
                server.did_open(&path, crate::lsp::servers::language_id(language), &text);
            }
        }
        drop(state);
        self.request_redraw();
    }

    /// Sends the text of buffers that changed once typing has paused.
    /// Keeps the Git gutter marks of open files current: fetches the HEAD
    /// text of the active file when it has none yet, and re-diffs files
    /// edited since their marks were computed, once typing pauses.
    pub(super) fn gutter_refresh(&self) {
        const PAUSE: Duration = Duration::from_millis(200);
        let Some(mut state) = self.state_mut() else {
            return;
        };
        // Marks worked out on a worker, for text that has not moved on.
        while let Ok((id, text, marks)) = state.gutter.computed.1.try_recv() {
            state.gutter.diffing.remove(&id);
            let current = all_docs(&state)
                .flat_map(|d| d.iter())
                .find(|b| b.id() == id)
                .is_some_and(|b| b.rope.same_as(&text));
            if current && let Some(entry) = state.gutter.docs.get_mut(&id) {
                entry.marks = marks;
                self.ivars().needs_redraw.set(true);
            }
        }
        // Fetched HEAD texts landing.
        while let Ok((id, head)) = state.gutter.heads.1.try_recv() {
            state.gutter.pending.remove(&id);
            let has_head = head.is_some();
            state.gutter.docs.insert(
                id,
                GutterState {
                    head: head.map(std::sync::Arc::new),
                    marks: Vec::new(),
                },
            );
            if has_head {
                state.gutter.dirty.insert(id, overdue());
            }
            self.ivars().needs_redraw.set(true);
        }
        // The active file, if it has never been looked at.
        {
            let active = state.docs.active();
            let id = active.id();
            if let Some(path) = active.path.clone()
                && !active.is_preview_file()
                && active.rope.len_bytes() <= GUTTER_MAX_BYTES
                && !state.gutter.docs.contains_key(&id)
                && !state.gutter.pending.contains(&id)
            {
                state.gutter.pending.insert(id);
                let tx = state.gutter.heads.0.clone();
                std::thread::spawn(move || {
                    let head = path
                        .parent()
                        .ok_or_else(|| "no parent directory".to_string())
                        .and_then(crate::project::git::toplevel)
                        .and_then(|root| crate::project::git::head_text(&root, &path))
                        .ok()
                        .map(|text| text.unwrap_or_default());
                    let _ = tx.send((id, head));
                });
            }
        }
        // Files edited a moment ago.
        let due = due(&state.gutter.dirty, PAUSE);
        if due.is_empty() {
            return;
        }
        let State {
            gutter:
                Gutter {
                    docs: gutter,
                    dirty: gutter_dirty,
                    diffing: gutter_diffing,
                    computed: marks_channel,
                    ..
                },
            docs,
            panes,
            ..
        } = &mut *state;
        for id in due {
            // One diff per document at a time; the next pause asks again.
            if gutter_diffing.contains(&id) {
                continue;
            }
            gutter_dirty.remove(&id);
            let Some(entry) = gutter.get_mut(&id) else {
                continue;
            };
            let Some(head) = entry.head.clone() else {
                continue;
            };
            let buffer = buffer_by_id(docs, panes, id);
            let Some(buffer) = buffer else {
                continue;
            };
            if buffer.rope.len_bytes() > GUTTER_MAX_BYTES {
                entry.marks.clear();
                continue;
            }
            // Myers over a couple of megabytes with hundreds of changes is
            // tens to hundreds of milliseconds: not on the main thread.
            let snapshot = buffer.rope.clone();
            let tx = marks_channel.0.clone();
            gutter_diffing.insert(id);
            std::thread::spawn(move || {
                let text = snapshot.to_string();
                let marks = crate::project::git::marks(&crate::ide::diff::diff(&head, &text));
                let _ = tx.send((id, snapshot, marks));
            });
        }
    }

    pub(super) fn lsp_flush_changes(&self) {
        const PAUSE: Duration = Duration::from_millis(150);
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if state.lsp.dirty.is_empty() {
            return;
        }
        let due = due(&state.lsp.dirty, PAUSE);
        if due.is_empty() {
            return;
        }
        let State {
            lsp:
                Lsp {
                    servers: lsp,
                    dirty: lsp_dirty,
                    ..
                },
            docs,
            panes,
            ..
        } = &mut *state;
        for id in due {
            lsp_dirty.remove(&id);
            let buffer = buffer_by_id(docs, panes, id);
            let Some(buffer) = buffer else {
                continue;
            };
            // Grown past the limit: the servers that have it let it go.
            if buffer.rope.len_bytes() > LSP_MAX_BYTES
                && let Some(path) = &buffer.path
            {
                for server in lsp.values_mut().filter(|s| s.knows(path)) {
                    server.did_close(path);
                }
                continue;
            }
            let (Some(path), Some(language)) = (&buffer.path, lsp_language(buffer)) else {
                continue;
            };
            if let Some(server) = lsp.get_mut(&crate::lsp::servers::server_key(language))
                && server.is_ready()
            {
                if server.knows(path) {
                    server.did_change(path, &buffer.rope.to_string());
                } else {
                    server.did_open(
                        path,
                        crate::lsp::servers::language_id(language),
                        &buffer.rope.to_string(),
                    );
                }
            }
        }
    }

    /// Main thread, whenever a server has something to say.
    pub(super) fn poll_lsp(&self) {
        let Some(mut state) = self.state_mut() else {
            // Busy: the display link will try again.
            self.resume_display_link();
            return;
        };
        let mut events = Vec::new();
        for (key, server) in state.lsp.servers.iter_mut() {
            events.extend(server.poll().into_iter().map(|event| (*key, event)));
        }
        drop(state);
        let mut sync = false;
        for (key, event) in events {
            use crate::lsp::client::Event;
            match event {
                Event::Ready => sync = true,
                Event::Diagnostics(path) => {
                    // New problems can mean new fixes.
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    if state
                        .lsp
                        .bulb
                        .as_ref()
                        .is_some_and(|b| same_file(&b.path, &path))
                    {
                        state.lsp.bulb = None;
                    }
                }
                Event::CodeActions {
                    request, actions, ..
                } => self.code_actions_arrived(key, request, actions),
                Event::Action(steps) => {
                    let Some(save) = self.state_mut().map(|mut state| state.lsp.resolving.take())
                    else {
                        return;
                    };
                    if let Some(save) = save {
                        self.run_steps(key, steps, save);
                    }
                }
                Event::ApplyEdit { id, edits } => {
                    let outcome = self.apply_workspace_edit(key, &edits);
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    if let Some(server) = state.lsp.servers.get_mut(&key) {
                        server.answer_apply_edit(&id, outcome.failed.is_empty());
                    }
                    if outcome.open + outcome.written > 0 || !outcome.failed.is_empty() {
                        state.message =
                            Some((format!("changed {}", outcome.describe()), Instant::now()));
                    }
                }
                Event::Completions { items, request, .. } => {
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    let prefix = state
                        .completion
                        .as_ref()
                        .filter(|popup| popup.request == request)
                        .and_then(|popup| typed_since(state.docs.active(), popup));
                    if let (Some(prefix), Some(popup)) = (prefix, state.completion.as_mut()) {
                        popup.items = items;
                        popup.refilter(&prefix);
                    }
                }
                Event::Definition(locations) => {
                    let Some(location) = locations.into_iter().next() else {
                        if let Some(mut state) = self.state_mut() {
                            state.message = Some(("no definition found".into(), Instant::now()));
                        }
                        continue;
                    };
                    self.load_path(&location.path.to_string_lossy());
                    let (rows, cols) = self.grid();
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    let buffer = state.docs.active_mut();
                    if buffer.path.as_deref() == Some(location.path.as_path()) {
                        let offset = crate::lsp::offset_of(&buffer.rope, location.start);
                        buffer.place_cursor(offset, Motion::Move);
                        buffer.scroll_to_cursor(rows, cols);
                    }
                    drop(state);
                    self.sync_title();
                    self.reparse();
                }
                Event::Hover { text, .. } => {
                    let first: String = text.lines().take(2).collect::<Vec<_>>().join("  ");
                    if let Some(mut state) = self.state_mut() {
                        state.message = Some((first, Instant::now()));
                    }
                }
                Event::References(locations) => self.show_references(locations),
                Event::Rename(files) => self.apply_rename(key, files),
                Event::Formatting { path, edits, save } => self.apply_format(&path, &edits, save),
                Event::Signature { path, signature } => {
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    let buffer = state.docs.active();
                    let here = buffer.path.as_deref() == Some(path.as_path());
                    let (id, caret) = (buffer.id(), buffer.cursor());
                    state.lsp.signature =
                        signature.filter(|_| here).map(|signature| SignatureTip {
                            buffer: id,
                            anchor: caret,
                            signature,
                        });
                }
                Event::Refused(reason) => {
                    if let Some(mut state) = self.state_mut() {
                        state.message = Some((reason, Instant::now()));
                    }
                }
                Event::Failed(reason) => {
                    // A server that died is dropped (its process with it),
                    // not kept as a dead entry: it gets another start, up to
                    // a few, and nothing more is written to its closed pipe
                    // nor read from its last diagnostics.
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    let restarts = state.lsp.restarts.entry(key).or_insert(0);
                    *restarts += 1;
                    let restarts = *restarts;
                    // Dropping it kills a process that is still there (one
                    // that never answered initialize). One that died soon
                    // after starting will only die again: not restarted.
                    let short_lived = state
                        .lsp
                        .servers
                        .remove(&key)
                        .is_some_and(|server| server.uptime() < LSP_RESTART_AFTER);
                    if short_lived || restarts >= LSP_MAX_RESTARTS {
                        state
                            .lsp
                            .unavailable
                            .insert(key, format!("{reason}; not started again"));
                    } else {
                        sync = true;
                    }
                    state.message = Some((reason, Instant::now()));
                }
            }
        }
        if sync {
            self.lsp_sync_open();
        }
        self.request_redraw();
        self.pump();
    }

    /// After any key reached the document: remember the edit for the
    /// server, and keep the completion list honest about the caret.
    pub(super) fn lsp_after_key(&self, edited: bool) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let id = state.docs.active().id();
        if let Some(tip) = &state.lsp.signature {
            let buffer = state.docs.active();
            let caret = buffer.cursor();
            let line = |at: usize| buffer.rope.byte_to_line(at.min(buffer.rope.len_bytes()));
            if tip.buffer != id || caret < tip.anchor || line(caret) != line(tip.anchor) {
                state.lsp.signature = None;
            }
        }
        if edited && state.docs.active().path.is_some() {
            state.lsp.dirty.insert(id, Instant::now());
        }
        let still = follow_completion(&mut state);
        drop(state);
        if edited {
            // A shorter prefix can match more than the worker was asked
            // about; ask again. Typing forward is asked by `lsp_typed`.
            if still {
                self.ask_worker();
            }
            self.resume_display_link();
        }
    }

    /// After text was typed into the document: ask for completions when
    /// the character is part of a word, one the server asked to hear, or
    /// the text before the caret has become a path.
    pub(super) fn lsp_typed(&self, text: &str) {
        let mut chars = text.chars();
        let (Some(ch), None) = (chars.next(), chars.next()) else {
            return;
        };
        let (trigger, path, signature) = {
            let Some(state) = self.state() else {
                return;
            };
            let buffer = state.docs.active();
            let server = lsp_server_for(&state, buffer);
            let trigger = server.is_some_and(|s| s.trigger_characters.iter().any(|t| t == text));
            let signature =
                server.is_some_and(|s| s.signature_triggers().iter().any(|t| t == text));
            (trigger, completion_path_query(&state).is_some(), signature)
        };
        if signature {
            self.request_signature();
        } else if ch == ')'
            && let Some(mut state) = self.state_mut()
        {
            state.lsp.signature = None;
        }
        if crate::complete::is_word_char(ch) || trigger || path {
            self.request_completion(false);
        } else {
            if let Some(mut state) = self.state_mut() {
                state.completion = None;
            }
        }
    }

    /// Sends pending changes without waiting for the pause, so a request
    /// that follows sees the current text.
    pub(super) fn lsp_flush_now(&self) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            for at in state.lsp.dirty.values_mut() {
                *at = overdue();
            }
        }
        self.lsp_flush_changes();
    }

    /// Asks where the symbol at `offset` (or the caret) is defined.
    pub(super) fn goto_definition(&self, offset: Option<usize>) {
        self.ask_server(offset, |server, path, at| {
            server.definition(path, at);
        });
        self.request_redraw();
    }

    pub(super) fn show_hover(&self) {
        self.ask_server(None, |server, path, at| {
            server.hover(path, at);
        });
        self.request_redraw();
    }
}
