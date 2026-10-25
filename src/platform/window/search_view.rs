//! The Search view: opening it, its fields' keys and clicks, running
//! the project search on a worker, listing what it found, opening a
//! result and replacing in every file listed. Find References lands here
//! too. The view's state and drawing are `platform::search_panel`.

use super::*;
use crate::platform::search_panel::{Field, Row};

impl EditorView {
    /// Cmd-Shift-F: the Search view with the keys in its query, seeded
    /// from a one-line selection when there is one; otherwise the query
    /// as it was, selected, so typing replaces it and Return repeats it.
    pub(super) fn open_search(&self) {
        let seed = self.state().and_then(|state| {
            if state.terminal.has_keys() {
                return None;
            }
            state
                .docs
                .active()
                .selected_text()
                .filter(|t| !t.contains('\n') && t.len() < 200)
        });
        let seed = seed.or_else(|| {
            self.terminal_selected_text(false)
                .filter(|t| !t.contains('\n') && t.len() < 200)
        });
        self.show_search_view();
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if let Some(seed) = seed {
            state.search.query = Buffer::from_text(&seed);
            state.search.references = false;
        }
        state.search.query.select_all();
        state.search_focus = Some(Field::Query);
        if state.search.query.rope.len_bytes() > 0 && !state.search.answered {
            state.search.due = Some(Instant::now());
        }
        drop(state);
        self.resume_display_link();
        self.request_redraw();
        self.pump();
    }

    /// The sidebar on the Search view, in the editor.
    pub(super) fn show_search_view(&self) {
        {
            let Some(state) = self.state_mut() else {
                return;
            };
            if state.agents.on {
                drop(state);
                self.set_agents_mode(false);
            }
        }
        let Some(mut state) = self.state_mut() else {
            return;
        };
        close_fields(&mut state);
        state.extensions = None;
        state.mcp.open = false;
        state.git_open = false;
        state.git_focus = false;
        state.search_open = true;
        state.sidebar = true;
        // The document's find bar keeps its matches drawn, but the keys
        // are the view's.
        if let Some(bar) = &mut state.find {
            bar.has_keys = false;
        }
    }

    /// After a field changed: the project is searched for the query once
    /// typing pauses; the last results stay until then, and go with an
    /// empty query. A new replacement changes nothing until it is used.
    pub(super) fn search_changed(&self, field: Field) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if field == Field::Replace {
            return;
        }
        state.search.references = false;
        state.search.answered = false;
        if state.search.query.rope.len_bytes() == 0 {
            state.search.clear();
            state.search.due = None;
            state.search.searching = false;
            Self::stop_search(&mut state);
        } else {
            state.search.due = Some(Instant::now() + crate::platform::search_panel::DEBOUNCE);
        }
        drop(state);
        self.resume_display_link();
        self.request_redraw();
    }

    /// Stops a search running, whose answer is then dropped.
    fn stop_search(state: &mut State) {
        if let Some(cancel) = state.project_search.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        state.project_search.rx = None;
    }

    /// Searches the project for the query on a worker, with the view's
    /// options and filters.
    pub(super) fn start_search(&self) {
        let (query, options, filter, root) = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.search.due = None;
            let filter = search::Filter::parse(
                &state.search.include.rope.to_string(),
                &state.search.exclude.rope.to_string(),
            );
            (
                state.search.query.rope.to_string(),
                state.search.options,
                filter,
                state.tree.root().map(Path::to_path_buf),
            )
        };
        let Some(root) = root else {
            if let Some(mut state) = self.state_mut() {
                state.say(layout::Feedback::Info, "open a folder to search it");
            }
            return;
        };
        if query.is_empty() {
            return;
        }
        if let Err(error) = search::find("", &query, "", options) {
            if let Some(mut state) = self.state_mut() {
                state.say(layout::Feedback::Failure, format!("invalid regex: {error}"));
            }
            return;
        }
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        std::thread::spawn(move || {
            if let Some(result) =
                search::search_tree(root, &query, options, &filter, &worker_cancel)
            {
                let _ = tx.send(result);
            }
        });
        let Some(mut state) = self.state_mut() else {
            return;
        };
        Self::stop_search(&mut state);
        state.project_search.cancel = Some(cancel);
        state.project_search.rx = Some(rx);
        state.project_search.references = false;
        state.search.references = false;
        state.search.searching = true;
        drop(state);
        self.resume_display_link();
        self.request_redraw();
    }

    /// Starts a search due after a pause in typing, and takes a worker's
    /// answer once it is there: the view lists it.
    pub(super) fn poll_project_search(&self) {
        let due = self
            .state()
            .is_some_and(|state| state.search.due.is_some_and(|at| Instant::now() >= at));
        if due {
            self.start_search();
        }
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some(rx) = &state.project_search.rx else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("project search stopped".into()),
        };
        state.project_search.rx = None;
        state.project_search.cancel = None;
        let references = state.project_search.references;
        match result {
            Ok(results) => {
                let count = results.len();
                state.search.references = references;
                state.search.take_results(results);
                if references {
                    state.say(
                        layout::Feedback::Info,
                        format!("{count} reference{}", if count == 1 { "" } else { "s" }),
                    );
                }
            }
            Err(error) => {
                state.search.searching = false;
                state.say(
                    layout::Feedback::Failure,
                    format!("project search: {error}"),
                );
            }
        }
        drop(state);
        self.request_redraw();
    }

    /// Opens the hit row `index` leads to, selecting the match in its
    /// file. A file row opens the file at its first match.
    pub(super) fn open_search_row(&self, index: usize) {
        let target = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(row) = state.search.rows().get(index) else {
                return;
            };
            let hit = &state.search.results[row.hit()];
            let target = (hit.path.clone(), hit.line, hit.column, hit.range.len());
            state.search.selected = index;
            target
        };
        let (path, line, column, len) = target;
        if !self.load_path(&path.to_string_lossy()) {
            return;
        }
        let (rows, cols) = self.grid();
        {
            // By line and column: the open document may have been edited
            // above the hit since the search read the file from disk.
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let buffer = state.docs.active_mut();
            let line = line.min(buffer.rope.len_lines().saturating_sub(1));
            let line_start = buffer.rope.line_to_byte(line);
            let line_end = crate::text::wrap::line_end(&buffer.rope, line);
            let start = (line_start + column).min(line_end);
            let end = (start + len).min(buffer.rope.len_bytes());
            buffer.select_range(start, end);
            let covered = super::find::find_covered_rows(&state);
            state
                .docs
                .active_mut()
                .scroll_to_cursor_below(rows, cols, covered);
        }
        self.sync_title();
        self.reparse();
        self.request_redraw();
        self.pump();
    }

    /// Keys while a field of the view has them. Returns whether it
    /// consumed them.
    pub(super) fn handle_search_key(&self, event: &NSEvent) -> bool {
        self.poll_project_search();
        const ESCAPE: u16 = 53;
        let flags = event.modifierFlags();
        let command = flags.contains(NSEventModifierFlags::Command);
        let shift = flags.contains(NSEventModifierFlags::Shift);
        let code = match event.keyCode() {
            key::KEYPAD_ENTER => key::RETURN,
            code => code,
        };
        let Some(focus) = self.state().and_then(|state| state.search_focus) else {
            return false;
        };
        match code {
            ESCAPE => {
                // The keys go back to the document; the view stays.
                if let Some(mut state) = self.state_mut() {
                    state.search_focus = None;
                }
                self.request_redraw();
                self.pump();
                return true;
            }
            key::TAB => {
                let Some(mut state) = self.state_mut() else {
                    return false;
                };
                let fields: Vec<Field> = [
                    Some(Field::Query),
                    state.search.show_replace.then_some(Field::Replace),
                    state.search.show_filters.then_some(Field::Include),
                    state.search.show_filters.then_some(Field::Exclude),
                ]
                .into_iter()
                .flatten()
                .collect();
                let at = fields.iter().position(|f| *f == focus).unwrap_or(0);
                let next = if shift {
                    (at + fields.len() - 1) % fields.len()
                } else {
                    (at + 1) % fields.len()
                };
                state.search_focus = Some(fields[next]);
                drop(state);
                self.request_redraw();
                self.pump();
                return true;
            }
            key::UP | key::DOWN if !command => {
                let Some(mut state) = self.state_mut() else {
                    return false;
                };
                let visible = self
                    .chrome()
                    .sidebar
                    .map_or(0, |rect| state.search.geometry(rect).visible_rows());
                state.search.select_step(code == key::DOWN, visible);
                drop(state);
                self.request_redraw();
                self.pump();
                return true;
            }
            key::RETURN => {
                // A search still to start (the query just changed) starts
                // now; otherwise, from the query, the selected row opens.
                let selected = {
                    let Some(mut state) = self.state_mut() else {
                        return false;
                    };
                    if state.search.due.take().is_some() || focus != Field::Query {
                        None
                    } else {
                        Some(state.search.selected).filter(|i| *i < state.search.rows().len())
                    }
                };
                match selected {
                    Some(index) => self.open_search_row(index),
                    None => self.start_search(),
                }
                self.request_redraw();
                self.pump();
                return true;
            }
            _ => {}
        }

        if command {
            let letter = event
                .charactersIgnoringModifiers()
                .and_then(|s| s.to_string().chars().next())
                .map(|c| c.to_ascii_lowercase());
            let changed = {
                let Some(mut state) = self.state_mut() else {
                    return false;
                };
                let field = search_field(&mut state, focus);
                field_shortcut(field, code, letter, shift)
            };
            let Some(changed) = changed else {
                return false;
            };
            if changed {
                self.search_changed(focus);
            }
            self.request_redraw();
            self.pump();
            return true;
        }

        let as_text = {
            let Some(mut state) = self.state_mut() else {
                return false;
            };
            let field = search_field(&mut state, focus);
            !field_key(field, code, flags)
        };
        if as_text {
            // The borrow has to end before the input system is asked,
            // because it answers by calling back into this view.
            return self.interpret(event);
        }
        self.request_redraw();
        self.pump();
        true
    }

    /// A click in the view. Returns whether it landed on something.
    pub(super) fn search_click(&self, column: Viewport, x: f32, y: f32) -> bool {
        let Some(mut state) = self.state_mut() else {
            return false;
        };
        let g = state.search.geometry(column);
        let mut changed = None;
        let mut open = None;
        if g.actions[0].contains(x, y) {
            state.search.collapse_all();
        } else if g.actions[1].contains(x, y) {
            Self::stop_search(&mut state);
            state.search.query = Buffer::new();
            state.search.clear();
            state.search.due = None;
            state.search.searching = false;
            state.search.references = false;
            state.search_focus = Some(Field::Query);
        } else if g.actions[2].contains(x, y) {
            state.search.show_filters = !state.search.show_filters;
            if !state.search.show_filters
                && matches!(state.search_focus, Some(Field::Include | Field::Exclude))
            {
                state.search_focus = Some(Field::Query);
            }
        } else if g.toggle.contains(x, y) {
            state.search.show_replace = !state.search.show_replace;
            if !state.search.show_replace && state.search_focus == Some(Field::Replace) {
                state.search_focus = Some(Field::Query);
            }
        } else if let Some(slot) = g.chips.iter().position(|chip| chip.contains(x, y)) {
            match slot {
                0 => state.search.options.case_sensitive = !state.search.options.case_sensitive,
                1 => state.search.options.whole_word = !state.search.options.whole_word,
                _ => state.search.options.regex = !state.search.options.regex,
            }
            changed = Some(Field::Query);
        } else if g.replace_all.contains(x, y) && state.search.show_replace {
            drop(state);
            self.replace_in_project();
            return true;
        } else if let Some(field) = g.field_at(x, y) {
            // Clicking a field focuses it and puts the caret where the
            // pointer is, measured through the same shaping that drew it.
            state.search_focus = Some(field);
            let State {
                search, renderer, ..
            } = &mut *state;
            let rect = g.field(field);
            let buffer = match field {
                Field::Query => &mut search.query,
                Field::Replace => &mut search.replacement,
                Field::Include => &mut search.include,
                Field::Exclude => &mut search.exclude,
            };
            let at =
                crate::platform::search_panel::byte_at(&mut renderer.atlas, buffer, x - rect.x);
            buffer.place_cursor(at, Motion::Move);
        } else if let Some(index) = state.search.row_at(g, y) {
            match state.search.rows().get(index) {
                Some(Row::File { .. }) => state.search.toggle_file(index),
                Some(Row::Line { .. }) => open = Some(index),
                None => {}
            }
        } else {
            drop(state);
            return false;
        }
        drop(state);
        if let Some(field) = changed {
            self.search_changed(field);
        }
        if let Some(index) = open {
            self.open_search_row(index);
        }
        self.request_redraw();
        self.pump();
        true
    }

    /// Replace All: every file in the results, searched again now so the
    /// edits fit the text as it is. Open documents are edited in place,
    /// one undo step each, and left unsaved; other files are written
    /// through the normal save path. Refused when the results were cut,
    /// since files past the cut would be missed.
    pub(super) fn replace_in_project(&self) {
        let (needle, replacement, options, files, matches) = {
            let Some(state) = self.state() else {
                return;
            };
            let search = &state.search;
            let mut files: Vec<std::path::PathBuf> = Vec::new();
            for hit in &search.results {
                if !files.contains(&hit.path) {
                    files.push(hit.path.clone());
                }
            }
            (
                search.query.rope.to_string(),
                search.replacement.rope.to_string(),
                search.options,
                files,
                search.results.len(),
            )
        };
        let say = |kind: layout::Feedback, text: String| {
            if let Some(mut state) = self.state_mut() {
                state.say(kind, text);
            }
            self.request_redraw();
        };
        if needle.is_empty() || files.is_empty() {
            return say(
                layout::Feedback::Info,
                "search the project first; Replace All uses its results".into(),
            );
        }
        if matches >= search::PROJECT_HITS {
            return say(
                layout::Feedback::Failure,
                format!(
                    "over {} matches: narrow the search before replacing",
                    search::PROJECT_HITS
                ),
            );
        }
        if !self.ivars().testing {
            let question = format!(
                "Replace {matches} match{} in {} file{}?",
                if matches == 1 { "" } else { "es" },
                files.len(),
                if files.len() == 1 { "" } else { "s" }
            );
            let answer = ask(
                MainThreadMarker::from(self),
                &question,
                "Open files are changed in their tabs and can be undone there. \
                 Files that are not open are saved to disk.",
                &["Replace", "Cancel"],
            );
            if answer != 0 {
                return;
            }
        }
        let mut replaced = 0;
        let mut outcome = EditOutcome::default();
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let mut touched = Vec::new();
            let edit = |_: &Path, buffer: &mut Buffer| -> Option<usize> {
                let text = buffer.rope.to_string();
                let found = search::find(&text, &needle, &replacement, options).ok()?;
                let edits: Vec<_> = found
                    .into_iter()
                    .map(|m| (m.range, m.replacement))
                    .collect();
                Some(if edits.is_empty() {
                    0
                } else {
                    buffer.replace_ranges(&edits)
                })
            };
            let mut on_disk = Vec::new();
            for path in &files {
                let found = open_doc_index(&all_docs(&state).collect::<Vec<_>>(), path);
                let Some((d, i)) = found else {
                    on_disk.push(path.clone());
                    continue;
                };
                let buffer = all_docs_mut(&mut state)
                    .into_iter()
                    .nth(d)
                    .and_then(|docs| docs.iter_mut().nth(i));
                let Some(buffer) = buffer else { continue };
                match edit(path, buffer) {
                    Some(0) => {}
                    Some(n) => {
                        replaced += n;
                        outcome.open += 1;
                        touched.push(buffer.id());
                    }
                    None => outcome.failed.push(path.clone()),
                }
            }
            for (path, done) in on_disk
                .iter()
                .zip(edit_on_disk(&on_disk, edit, &mut state.history))
            {
                match done {
                    Some(0) => {}
                    Some(n) => {
                        replaced += n;
                        outcome.written += 1;
                    }
                    None => outcome.failed.push(path.clone()),
                }
            }
            note_documents_edited(&mut state, touched);
            if outcome.written > 0 {
                note_files_written(&mut state);
            }
            // The list described text that is gone. Not searched again:
            // the search reads the disk, and the open files' changes are
            // not saved.
            state.search.clear();
        }
        self.lsp_flush_changes();
        let note = format!("replaced {replaced} in {}", outcome.describe());
        self.reparse();
        self.sync_title();
        say(layout::Feedback::Success, note);
        self.pump();
    }
}

/// The view's field `field`.
pub(super) fn search_field(state: &mut State, field: Field) -> &mut Buffer {
    match field {
        Field::Query => &mut state.search.query,
        Field::Replace => &mut state.search.replacement,
        Field::Include => &mut state.search.include,
        Field::Exclude => &mut state.search.exclude,
    }
}

/// The editing shortcuts of a one-line field, which are real editors:
/// handled here as well as through menu actions, because AppKit does not
/// send every key equivalent through the menu on every keyboard layout.
/// `None` when the key is not one of them; otherwise whether the text
/// changed.
pub(super) fn field_shortcut(
    field: &mut Buffer,
    code: u16,
    letter: Option<char>,
    shift: bool,
) -> Option<bool> {
    let motion = if shift { Motion::Extend } else { Motion::Move };
    Some(match (code, letter) {
        (key::DELETE, _) => {
            field.delete_to_line_start();
            true
        }
        (key::FORWARD_DELETE, _) => {
            field.delete_to_line_end();
            true
        }
        (key::LEFT, _) => {
            field.move_line_start(motion);
            false
        }
        (key::RIGHT, _) => {
            field.move_line_end(motion);
            false
        }
        (_, Some('a')) => {
            field.select_all();
            false
        }
        (_, Some('c')) => {
            if let Some(text) = field.selected_text() {
                clipboard::write_text(&text);
            }
            false
        }
        (_, Some('x')) => match field.selected_text() {
            Some(text) => {
                clipboard::write_text(&text);
                field.backspace();
                true
            }
            None => false,
        },
        (_, Some('v')) => match clipboard::read_text() {
            Some(text) => {
                field.insert(&single_line(&text));
                true
            }
            None => false,
        },
        (_, Some('z')) => {
            if shift {
                field.redo();
            } else {
                field.undo();
            }
            true
        }
        _ => return None,
    })
}
