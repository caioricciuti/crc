//! What the language servers do on request: references, rename, code
//! actions and the light bulb, formatting, signature help, symbols, and
//! the breadcrumb menu.

use super::*;

impl EditorView {
    /// Sends a request to the server for the active document, about
    /// `offset` or the caret. Changes typed so far go first, so it answers
    /// about the text on screen. Says so in the status line when there is
    /// no server.
    pub(super) fn ask_server(
        &self,
        offset: Option<usize>,
        ask: impl FnOnce(&mut crate::lsp::client::Server, &Path, crate::lsp::Position),
    ) -> bool {
        self.lsp_flush_now();
        let Some(mut state) = self.state_mut() else {
            return false;
        };
        let buffer = state.docs.active();
        let (Some(path), Some(language)) = (buffer.path.clone(), lsp_language(buffer)) else {
            state.message = Some(("no language server for this file".into(), Instant::now()));
            return false;
        };
        let at = crate::lsp::position_of(&buffer.rope, offset.unwrap_or(buffer.cursor()));
        let key = crate::lsp::servers::server_key(language);
        match ready_server(&mut state.lsp.servers, key).filter(|s| s.knows(&path)) {
            Some(server) => {
                ask(server, &path, at);
                true
            }
            None => {
                state.message = Some(("no language server for this file".into(), Instant::now()));
                false
            }
        }
    }

    pub(super) fn find_references(&self) {
        if self.ask_server(None, |server, path, at| {
            server.references(path, at);
        }) && let Some(mut state) = self.state_mut()
        {
            state.message = Some(("finding references…".into(), Instant::now()));
        }
        self.request_redraw();
    }

    /// The references as project search results: the find bar in project
    /// mode, the name as its query, one row per place.
    pub(super) fn show_references(&self, locations: Vec<crate::lsp::Location>) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if locations.is_empty() {
            state.message = Some(("no references found".into(), Instant::now()));
            drop(state);
            self.request_redraw();
            return;
        }
        // The references' lines are read on a worker: a symbol used in a
        // few hundred files not open would otherwise be read from disk here.
        // Open documents are passed as they stand, so the lines match.
        let open: Vec<(std::path::PathBuf, crate::text::rope::Rope)> = all_docs(&state)
            .flat_map(|d| d.iter())
            .filter_map(|b| Some((b.path.clone()?, b.rope.clone())))
            .collect();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut texts: HashMap<std::path::PathBuf, Option<crate::text::rope::Rope>> =
                HashMap::new();
            let mut results = Vec::new();
            for location in &locations {
                if worker_cancel.load(Ordering::Relaxed) {
                    return;
                }
                let rope = texts.entry(location.path.clone()).or_insert_with(|| {
                    open.iter()
                        .find(|(p, _)| same_file(p, &location.path))
                        .map(|(_, rope)| rope.clone())
                        .or_else(|| {
                            let bytes = std::fs::read(&location.path).ok()?;
                            let (text, _) = crate::text::file_format::decode(&bytes).ok()?;
                            Some(crate::text::rope::Rope::from_text(&text))
                        })
                });
                let Some(rope) = rope else {
                    continue;
                };
                let start = crate::lsp::offset_of(rope, location.start);
                let end = crate::lsp::offset_of(rope, location.end).max(start);
                let line = rope.byte_to_line(start);
                let line_start = rope.line_to_byte(line);
                let line_end = rope.line_range(line).end;
                results.push(ProjectHit {
                    path: location.path.clone(),
                    range: start..end,
                    line,
                    column: start - line_start,
                    snippet: rope
                        .slice_to_string(line_start..line_end)
                        .trim()
                        .chars()
                        .take(100)
                        .collect(),
                });
            }
            let _ = tx.send(Ok(results));
        });
        // One search at a time: a project search still running would
        // otherwise land over the references.
        if let Some(old) = state.project_search.cancel.replace(cancel) {
            old.store(true, Ordering::Relaxed);
        }
        state.project_search.rx = Some(rx);
        state.project_search.references = true;
        let buffer = state.docs.active();
        let name = buffer
            .rope
            .slice_to_string(buffer.word_range_at(buffer.cursor()));
        let mut query = Buffer::new();
        query.insert(&name);
        state.find = Some(FindBar {
            query,
            replacement: Buffer::new(),
            replacing: false,
            has_keys: true,
            options: SearchOptions {
                case_sensitive: true,
                whole_word: true,
                regex: false,
            },
            project: true,
            results: Vec::new(),
            selected: 0,
            result_scroll: 0,
            searching: true,
        });
        drop(state);
        self.resume_display_link();
        self.request_redraw();
        self.pump();
    }

    /// F2: the rename field in the status line, holding the current name.
    pub(super) fn start_rename(&self) {
        self.lsp_flush_now();
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let buffer = state.docs.active();
        let (Some(path), Some(language)) = (buffer.path.clone(), lsp_language(buffer)) else {
            state.message = Some(("no language server for this file".into(), Instant::now()));
            return;
        };
        let word = buffer.word_range_at(buffer.cursor());
        let name = buffer.rope.slice_to_string(word.clone());
        if name.trim().is_empty() {
            state.message = Some(("nothing to rename here".into(), Instant::now()));
            return;
        }
        let at = crate::lsp::position_of(&buffer.rope, word.start);
        let mut field = Buffer::new();
        field.insert(&name);
        field.select_all();
        close_fields(&mut state);
        state.rename = Some(RenameField {
            field,
            path,
            at,
            language,
        });
        drop(state);
        self.request_redraw();
        self.pump();
    }

    pub(super) fn handle_rename_key(&self, event: &NSEvent) -> bool {
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
                    state.rename = None;
                }
            }
            key::RETURN => {
                let Some(mut state) = self.state_mut() else {
                    return false;
                };
                let Some(rename) = state.rename.take() else {
                    return true;
                };
                let name = rename.field.rope.to_string().trim().to_owned();
                let key = crate::lsp::servers::server_key(rename.language);
                let sent = !name.is_empty()
                    && ready_server(&mut state.lsp.servers, key)
                        .map(|server| server.rename(&rename.path, rename.at, &name))
                        .is_some();
                state.message = Some((
                    if sent {
                        format!("renaming to {name}…")
                    } else {
                        "rename not sent".into()
                    },
                    Instant::now(),
                ));
            }
            key::DELETE | key::FORWARD_DELETE | key::LEFT | key::RIGHT | key::HOME | key::END => {
                let Some(mut state) = self.state_mut() else {
                    return false;
                };
                if let Some(rename) = &mut state.rename {
                    field_key(&mut rename.field, code, event.modifierFlags());
                }
            }
            _ => return self.interpret(event),
        }
        self.request_redraw();
        self.pump();
        true
    }

    /// A rename's edits. Open documents are edited in place, one undo step
    /// each, and left unsaved; files that are not open are edited on disk
    /// through the same save path as everything else.
    pub(super) fn apply_rename(&self, server: Language, files: Vec<crate::lsp::FileEdits>) {
        let outcome = self.apply_workspace_edit(server, &files);
        if let Some(mut state) = self.state_mut() {
            state.message = Some((
                if files.is_empty() {
                    "the server had nothing to rename".into()
                } else {
                    format!("renamed in {}", outcome.describe())
                },
                Instant::now(),
            ));
        }
        self.request_redraw();
        self.pump();
    }

    /// Applies a server's edits by file: open documents in place, one undo
    /// step each, left unsaved; other files on disk. An open document typed
    /// into since the server computed its edits is left alone and counted
    /// as failed: its positions would land on shifted text.
    pub(super) fn apply_workspace_edit(
        &self,
        server: Language,
        files: &[crate::lsp::FileEdits],
    ) -> EditOutcome {
        let (rows, cols) = self.grid();
        let mut outcome = EditOutcome::default();
        {
            let Some(mut state) = self.state_mut() else {
                return outcome;
            };
            let mut touched = Vec::new();
            let mut on_disk: Vec<&crate::lsp::FileEdits> = Vec::new();
            for file in files {
                let (path, edits) = (&file.path, &file.edits);
                if edits.is_empty() {
                    continue;
                }
                let found = open_doc_index(&all_docs(&state).collect::<Vec<_>>(), path);
                let Some((d, i)) = found else {
                    on_disk.push(file);
                    continue;
                };
                let open = all_docs(&state)
                    .nth(d)
                    .and_then(|docs| docs.iter().nth(i))
                    .map(|b| (b.id(), b.path.clone()));
                if let Some((id, own_path)) = open {
                    let sent = own_path
                        .as_deref()
                        .and_then(|p| state.lsp.servers.get(&server).and_then(|s| s.version_of(p)));
                    let moved = file.version.is_some() && sent.is_some() && file.version != sent;
                    if moved || state.lsp.dirty.contains_key(&id) {
                        outcome.failed.push(path.clone());
                        continue;
                    }
                }
                let buffer = all_docs_mut(&mut state)
                    .into_iter()
                    .nth(d)
                    .and_then(|docs| docs.iter_mut().nth(i));
                let Some(buffer) = buffer else { continue };
                match crate::lsp::edit_ranges(&buffer.rope, edits) {
                    Some(ranges) if buffer.replace_ranges(&ranges) > 0 => {
                        touched.push(buffer.id());
                        outcome.open += 1;
                    }
                    _ => outcome.failed.push(path.clone()),
                }
            }
            let paths: Vec<std::path::PathBuf> = on_disk.iter().map(|f| f.path.clone()).collect();
            let done = edit_on_disk(&paths, |path, buffer| {
                let file = on_disk.iter().find(|f| f.path == path)?;
                let ranges = crate::lsp::edit_ranges(&buffer.rope, &file.edits)?;
                let n = buffer.replace_ranges(&ranges);
                (n > 0).then_some(n)
            });
            for (path, done) in paths.into_iter().zip(done) {
                match done {
                    Some(_) => outcome.written += 1,
                    None => outcome.failed.push(path),
                }
            }
            note_documents_edited(&mut state, touched);
            state.docs.active_mut().scroll_to_cursor(rows, cols);
            if outcome.written > 0 {
                note_files_written(&mut state);
            }
        }
        self.lsp_flush_changes();
        self.reparse();
        self.sync_title();
        outcome
    }

    /// Asks the server for the active document's code actions at the caret
    /// or selection, or over the whole document for one `only` kind. The
    /// request, the server, the document and its text then. Says why not in
    /// the status line when `invoked`.
    pub(super) fn request_code_actions(
        &self,
        only: Option<&str>,
        invoked: bool,
    ) -> Option<(u64, Language, std::path::PathBuf, crate::text::rope::Rope)> {
        self.lsp_flush_now();
        let mut state = self.state_mut()?;
        let buffer = state.docs.active();
        let asked = match (buffer.path.clone(), lsp_language(buffer)) {
            (Some(path), Some(language)) => {
                let range = match only {
                    Some(_) => 0..buffer.rope.len_bytes(),
                    None => buffer
                        .selection()
                        .unwrap_or(buffer.cursor()..buffer.cursor()),
                };
                let start = crate::lsp::position_of(&buffer.rope, range.start);
                let end = crate::lsp::position_of(&buffer.rope, range.end);
                Some((path, language, start, end, buffer.rope.clone()))
            }
            _ => None,
        };
        let why = "no language server for this file";
        let Some((path, language, start, end, rope)) = asked else {
            if invoked {
                state.message = Some((why.into(), Instant::now()));
            }
            return None;
        };
        let key = crate::lsp::servers::server_key(language);
        let server = ready_server(&mut state.lsp.servers, key).filter(|s| s.knows(&path));
        let (sent, why) = match server {
            Some(server) if server.offers_code_actions() => {
                let diagnostics: Vec<crate::lsp::Diagnostic> = server
                    .diagnostics
                    .get(&path)
                    .map(|list| {
                        list.iter()
                            .filter(|d| d.start.line <= end.line && d.end.line >= start.line)
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                let id = server.code_actions(&path, (start, end), &diagnostics, only, invoked);
                (Some(id), why)
            }
            Some(_) => (None, "this language server has no code actions"),
            None => (None, why),
        };
        if sent.is_none() && invoked {
            state.message = Some((why.into(), Instant::now()));
        }
        Some((sent?, key, path, rope))
    }

    /// Cmd-.: the code actions at the caret, in the palette. The bulb's
    /// when it has them for this caret; otherwise the server is asked.
    pub(super) fn quick_fix(&self) {
        let ready = {
            let Some(state) = self.state() else {
                return;
            };
            let buffer = state.docs.active();
            state
                .lsp
                .bulb
                .as_ref()
                .filter(|b| {
                    b.buffer == buffer.id()
                        && b.caret == buffer.cursor()
                        && buffer.selection().is_none()
                        && b.shows()
                        && !state.lsp.dirty.contains_key(&b.buffer)
                })
                .map(|b| (b.server, b.actions.clone()))
        };
        if let Some((server, actions)) = ready {
            self.open_action_list(server, actions);
            return;
        }
        if let Some((request, ..)) = self.request_code_actions(None, true) {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.lsp.quick_fix_request = Some(request);
            state.message = Some(("looking for code actions\u{2026}".into(), Instant::now()));
        }
        self.request_redraw();
        self.pump();
    }

    /// The palette, listing `actions`: the preferred ones first and the
    /// ones that cannot run last, otherwise in the server's order.
    pub(super) fn open_action_list(
        &self,
        server: Language,
        mut actions: Vec<crate::lsp::CodeAction>,
    ) {
        actions.sort_by_key(|a| (a.disabled.is_some(), !a.preferred));
        self.open_palette_with("");
        if let Some(mut state) = self.state_mut() {
            state.lsp.action_list = Some((server, actions));
        }
        self.request_redraw();
        self.pump();
    }

    /// Runs a code action picked from the list: resolved first when the
    /// server holds its edit back.
    pub(super) fn run_code_action(&self, server: Language, action: crate::lsp::CodeAction) {
        if let Some(reason) = &action.disabled {
            if let Some(mut state) = self.state_mut() {
                state.message = Some((format!("{}: {reason}", action.title), Instant::now()));
            }
            self.request_redraw();
            return;
        }
        self.lsp_flush_now();
        let steps = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let steps =
                ready_server(&mut state.lsp.servers, server).map(|s| s.action_steps(&action));
            match &steps {
                None => {
                    state.message = Some(("the language server stopped".into(), Instant::now()))
                }
                Some(None) => {
                    state.lsp.resolving = Some(false);
                    state.message = Some((format!("{}\u{2026}", action.title), Instant::now()));
                }
                Some(Some(_)) => {}
            }
            steps.flatten()
        };
        if let Some(steps) = steps {
            self.run_steps(server, steps, false);
        }
        self.request_redraw();
        self.pump();
    }

    /// A code action's edits, then its command on the server. `save` when
    /// Organize Imports ran for a save, which goes on to save and format.
    pub(super) fn run_steps(
        &self,
        server: Language,
        steps: crate::lsp::client::ActionSteps,
        save: bool,
    ) {
        let outcome = self.apply_workspace_edit(server, &steps.edits);
        let changed = outcome.open + outcome.written > 0;
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if let Some(command) = &steps.command
                && let Some(server) = ready_server(&mut state.lsp.servers, server)
            {
                server.execute_command(command);
            }
            if !save {
                let title = if steps.title.is_empty() {
                    "code action"
                } else {
                    &steps.title
                };
                state.message = Some((
                    if changed || !outcome.failed.is_empty() {
                        format!("{title}: changed {}", outcome.describe())
                    } else if steps.command.is_some() {
                        format!("{title}\u{2026}")
                    } else {
                        format!("{title}: nothing to change")
                    },
                    Instant::now(),
                ));
            }
        }
        if save {
            self.continue_save(changed);
        }
        self.request_redraw();
        self.pump();
    }

    /// Code > Organize Imports, and before format-on-save. `false` when the
    /// file has no server that offers it, so a save goes straight on.
    pub(super) fn organize_imports(&self, save: bool) -> bool {
        let Some((request, _, path, snapshot)) =
            self.request_code_actions(Some("source.organizeImports"), !save)
        else {
            return false;
        };
        let Some(mut state) = self.state_mut() else {
            return false;
        };
        state.lsp.organizing = Some(Organizing {
            request,
            path,
            snapshot,
            save,
        });
        if !save {
            state.message = Some(("organizing imports\u{2026}".into(), Instant::now()));
        }
        true
    }

    /// After Organize Imports ran for a save: save what it changed, then
    /// format when that is on, which saves again.
    pub(super) fn continue_save(&self, changed: bool) {
        if changed {
            self.save_formatted();
        }
        if self.state().is_some_and(|state| state.format_on_save) {
            self.format_document(true);
        }
    }

    /// A server's answer to a code action request: the palette's list, an
    /// Organize Imports to run, or the bulb. Stale answers are dropped.
    pub(super) fn code_actions_arrived(
        &self,
        server: Language,
        request: u64,
        actions: Vec<crate::lsp::CodeAction>,
    ) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if state.lsp.quick_fix_request == Some(request) {
            state.lsp.quick_fix_request = None;
            if actions.is_empty() {
                state.message = Some(("no code actions here".into(), Instant::now()));
                return;
            }
            state.message = None;
            drop(state);
            self.open_action_list(server, actions);
            return;
        }
        if state
            .lsp
            .organizing
            .as_ref()
            .is_some_and(|o| o.request == request)
        {
            let Some(asked) = state.lsp.organizing.take() else {
                return;
            };
            let key = crate::platform::canonical(&asked.path);
            let unchanged = all_docs(&state)
                .flat_map(|d| d.iter())
                .find(|b| {
                    b.path
                        .as_deref()
                        .is_some_and(|p| is_open_path(p, &asked.path, &key))
                })
                .is_some_and(|b| b.rope.same_text(&asked.snapshot));
            let action = actions
                .into_iter()
                .find(|a| a.disabled.is_none() && a.kind.starts_with("source.organizeImports"));
            let steps = match (&action, unchanged) {
                (Some(action), true) => {
                    ready_server(&mut state.lsp.servers, server).map(|s| s.action_steps(action))
                }
                _ => None,
            };
            let note = match (&action, unchanged, &steps) {
                (None, _, _) => Some("nothing to organize"),
                (Some(_), false, _) => Some("organize imports skipped: the text changed"),
                (Some(_), true, None) => Some("the language server stopped"),
                (Some(_), true, Some(None)) => {
                    state.lsp.resolving = Some(asked.save);
                    None
                }
                (Some(_), true, Some(Some(_))) => None,
            };
            if let Some(note) = note
                && !asked.save
            {
                state.message = Some((note.into(), Instant::now()));
            }
            drop(state);
            match steps {
                Some(Some(steps)) => self.run_steps(server, steps, asked.save),
                Some(None) => {}
                None if asked.save => self.continue_save(false),
                None => {}
            }
            return;
        }
        if let Some((asked, buffer, caret)) = state.lsp.bulb_request
            && asked == request
        {
            state.lsp.bulb_request = None;
            let path = all_docs(&state)
                .flat_map(|d| d.iter())
                .find(|b| b.id() == buffer)
                .and_then(|b| b.path.clone());
            if let Some(path) = path {
                state.lsp.bulb = Some(Bulb {
                    buffer,
                    path,
                    caret,
                    server,
                    actions,
                });
                self.ivars().needs_redraw.set(true);
            }
        }
    }

    /// A click on the icon strip. The panel the sidebar already shows hides
    /// the sidebar, and brings it back; any other panel is shown.
    /// A click on a part of the breadcrumb path: what is in that folder,
    /// or for the file itself, what is beside it, as a menu under it.
    pub(super) fn breadcrumb_menu(&self, index: usize, event: &NSEvent) {
        let crumbs = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let rect = chrome_of(&state).breadcrumbs;
            let State {
                docs,
                tree,
                renderer,
                ..
            } = &mut *state;
            layout::breadcrumb_segments(docs.active(), tree, &mut renderer.atlas, rect)
        };
        let Some(crumb) = crumbs.get(index) else {
            return;
        };
        let (folder, current) = if crumb.path.is_dir() {
            (crumb.path.clone(), None)
        } else {
            match crumb.path.parent() {
                Some(parent) => (parent.to_path_buf(), Some(crumb.path.clone())),
                None => return,
            }
        };
        if self.ivars().testing {
            // A real popup runs its own event loop, which a script cannot
            // step past; the dump says which folder it would list.
            if let Some(mut state) = self.state_mut() {
                state.crumb_menu_requested = Some(folder.display().to_string());
            }
            return;
        }
        let mtm = MainThreadMarker::from(self);
        let mut paths = Vec::new();
        let menu = crumb_folder_menu(mtm, &folder, current.as_deref(), 1, &mut paths);
        if let Some(mut state) = self.state_mut() {
            state.crumb_paths = paths;
        }
        let _ = event;
        let at = NSPoint::new(
            crumb.rect.x as f64,
            (crumb.rect.y + crumb.rect.height) as f64,
        );
        menu.popUpMenuPositioningItem_atLocation_inView(None, at, Some(self));
    }

    /// Opens a file picked from a breadcrumb menu, or shows a folder in the
    /// Explorer.
    pub(super) fn open_crumb_path(&self, path: &Path) {
        if path.is_dir() {
            let explorer = {
                let Some(state) = self.state() else {
                    return;
                };
                state.extensions.is_none() && !state.git_open
            };
            if !explorer {
                self.activate(0);
            }
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.sidebar = true;
            state.tree.reveal(path);
            let rows = chrome_of(&state).sidebar.map_or(0, layout::sidebar_rows);
            state.tree.scroll_to_selection(rows);
            drop(state);
        } else {
            self.load_path(&path.to_string_lossy());
            self.sync_title();
            self.reparse();
            if let Some(mut state) = self.state_mut() {
                state.tree.reveal(path);
            }
        }
        self.request_redraw();
        self.pump();
    }

    pub(super) fn activate(&self, index: usize) {
        let (showing, current) = {
            let Some(state) = self.state() else {
                return;
            };
            let current = if state.mcp.open {
                3
            } else if state.extensions.is_some() {
                2
            } else if state.git_open {
                1
            } else {
                0
            };
            (state.sidebar, current)
        };
        if index == current {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.sidebar = !showing;
            drop(state);
            self.request_redraw();
            self.pump();
            return;
        }
        match index {
            0 => self.set_sidebar_view(false),
            1 => self.set_sidebar_view(true),
            2 => self.open_extensions(),
            _ => self.open_mcp(),
        }
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if !state.sidebar {
            state.sidebar = true;
        }
        drop(state);
        self.request_redraw();
        self.pump();
    }

    /// The bulb follows the caret: gone when it moves or the text changes,
    /// asked for again once it rests. Runs from the display link.
    pub(super) fn bulb_refresh(&self) {
        const REST: Duration = Duration::from_millis(400);
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let buffer = state.docs.active();
        let (id, caret) = (buffer.id(), buffer.cursor());
        let selecting = buffer.selection().is_some();
        let dirty = state.lsp.dirty.contains_key(&id);
        let here = |b: &Bulb| (b.buffer, b.caret) == (id, caret);
        if state.lsp.bulb.as_ref().is_some_and(|b| !here(b) || dirty) {
            state.lsp.bulb = None;
            self.ivars().needs_redraw.set(true);
        }
        if state.lsp.bulb.is_some()
            || selecting
            || dirty
            || state.palette.is_some()
            || state
                .lsp
                .bulb_request
                .is_some_and(|(_, b, c)| (b, c) == (id, caret))
        {
            state.lsp.bulb_want = None;
            return;
        }
        match state.lsp.bulb_want {
            Some((b, c, at)) if (b, c) == (id, caret) => {
                if at.elapsed() < REST {
                    return;
                }
            }
            _ => {
                state.lsp.bulb_want = Some((id, caret, Instant::now()));
                return;
            }
        }
        state.lsp.bulb_want = None;
        // Only where a server offers them; asking costs nothing then.
        let offers =
            lsp_server_for(&state, state.docs.active()).is_some_and(|s| s.offers_code_actions());
        drop(state);
        if !offers {
            return;
        }
        if let Some((request, ..)) = self.request_code_actions(None, false)
            && let Some(mut state) = self.state_mut()
        {
            state.lsp.bulb_request = Some((request, id, caret));
        }
    }

    /// Asks the server to format the active document. `save` when a save
    /// asked, which saves again once the edits are in.
    pub(super) fn format_document(&self, save: bool) {
        let (spaces, tab) = {
            let Some(state) = self.state() else {
                return;
            };
            let buffer = state.docs.active();
            match buffer.indent_style {
                Some(style) => (!style.tabs, style.width as u32),
                None => indent_style(&buffer.rope),
            }
        };
        let Some(snapshot) = self.state().map(|state| state.docs.active().rope.clone()) else {
            return;
        };
        let mut asked = None;
        let sent = self.ask_server(None, |server, path, _| {
            if server.formats() {
                server.formatting(path, tab, spaces, save);
                asked = Some(path.to_path_buf());
            }
        });
        let Some(mut state) = self.state_mut() else {
            return;
        };
        match asked {
            Some(path) => state.lsp.formatting = Some((path, snapshot)),
            None if sent && !save => {
                state.message = Some((
                    "this language server does not format".into(),
                    Instant::now(),
                ))
            }
            None => {}
        }
    }

    pub(super) fn apply_format(&self, path: &Path, edits: &[crate::lsp::TextEdit], save: bool) {
        let (rows, cols) = self.grid();
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some((asked, snapshot)) = state.lsp.formatting.take() else {
            return;
        };
        let buffer = state.docs.active_mut();
        let here = asked == path && buffer.path.as_deref() == Some(path);
        if !here || !buffer.rope.same_text(&snapshot) {
            state.message = Some(("format skipped: the text changed".into(), Instant::now()));
            return;
        }
        let changed = match crate::lsp::edit_ranges(&buffer.rope, edits) {
            Some(ranges) if !ranges.is_empty() => buffer.replace_ranges(&ranges) > 0,
            _ => false,
        };
        buffer.scroll_to_cursor(rows, cols);
        let id = buffer.id();
        if changed {
            state.lsp.dirty.insert(id, Instant::now());
        }
        state.message = Some((
            if changed {
                "formatted"
            } else {
                "already formatted"
            }
            .into(),
            Instant::now(),
        ));
        drop(state);
        if changed {
            self.after_edit();
            if save {
                self.save_formatted();
            }
        }
        self.request_redraw();
        self.pump();
    }

    pub(super) fn request_signature(&self) {
        self.ask_server(None, |server, path, at| {
            server.signature_help(path, at);
        });
    }

    /// Jumps to a palette symbol: its line in the active document, or opens
    /// its file first.
    pub(super) fn go_to_symbol(&self, path: Option<std::path::PathBuf>, line: u32) {
        if let Some(path) = path {
            let cwd = path.parent().map(Path::to_path_buf).unwrap_or_default();
            self.open_reference(&path.to_string_lossy(), Some(line as usize + 1), None, &cwd);
            if let Some(mut state) = self.state_mut() {
                state.tree.reveal(&path);
            }
        } else {
            let (rows, cols) = self.grid();
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let buffer = state.docs.active_mut();
            buffer.goto_line(line as usize);
            buffer.scroll_to_cursor(rows, cols);
        }
        self.request_redraw();
        self.pump();
    }

    pub(super) fn open_reference(
        &self,
        path: &str,
        line: Option<usize>,
        column: Option<usize>,
        cwd: &Path,
    ) {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let root = self
            .ivars()
            .state
            .borrow()
            .tree
            .root()
            .map(Path::to_path_buf);
        let given = Path::new(path);
        let candidates: Vec<std::path::PathBuf> = if given.is_absolute() {
            vec![given.to_path_buf()]
        } else if let (Some(rest), Some(home)) = (path.strip_prefix("~/"), home) {
            vec![home.join(rest)]
        } else {
            std::iter::once(cwd.join(given))
                .chain(root.map(|root| root.join(given)))
                .collect()
        };
        let Some(found) = candidates.into_iter().find(|p| p.is_file()) else {
            if let Some(mut state) = self.state_mut() {
                state.message = Some((format!("no file {path}"), Instant::now()));
            }
            self.resume_display_link();
            return;
        };
        if !self.load_path(&found.to_string_lossy()) {
            return;
        }
        if let Some(line) = line {
            let (rows, cols) = self.grid();
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let buffer = state.docs.active_mut();
            buffer.goto_line(line - 1);
            if let Some(column) = column {
                let start = buffer.cursor();
                let text = buffer
                    .rope
                    .slice_to_string(start..buffer.rope.len_bytes().min(start + 4096));
                let offset: usize = text
                    .chars()
                    .take_while(|c| *c != '\n')
                    .take(column - 1)
                    .map(char::len_utf8)
                    .sum();
                buffer.select_range(start + offset, start + offset);
            }
            buffer.scroll_to_cursor(rows, cols);
        }
        // The editor has the file now, so it has the keys too.
        if let Some(mut state) = self.state_mut() {
            state.terminal.focus = false;
        }
        self.sync_title();
        self.reparse();
    }
}
