//! The find bar and project search: matching in the document, replacing,
//! searching the project, and opening a result.

use super::*;

impl EditorView {
    /// Keys while the find bar has focus. Returns whether it consumed them.
    pub(super) fn handle_find_key(&self, event: &NSEvent) -> bool {
        self.poll_project_search();
        const ESCAPE: u16 = 53;
        let flags = event.modifierFlags();
        let command = flags.contains(NSEventModifierFlags::Command);
        let shift = flags.contains(NSEventModifierFlags::Shift);
        let code = match event.keyCode() {
            key::KEYPAD_ENTER => key::RETURN,
            code => code,
        };

        const TAB: u16 = 48;
        if matches!(code, key::UP | key::DOWN)
            && self
                .ivars()
                .state
                .borrow()
                .find
                .as_ref()
                .is_some_and(|b| b.project && !b.results.is_empty())
        {
            let Some(mut state) = self.state_mut() else {
                return false;
            };
            let bar = state.find.as_mut().expect("checked above");
            bar.selected = if code == key::DOWN {
                (bar.selected + 1).min(bar.results.len() - 1)
            } else {
                bar.selected.saturating_sub(1)
            };
            bar.follow_selection();
            drop(state);
            self.request_redraw();
            self.pump();
            return true;
        }
        match code {
            ESCAPE => {
                self.close_find();
                return true;
            }
            TAB => {
                // Tab crosses between Find and Replace rather than inserting.
                let Some(mut state) = self.state_mut() else {
                    return false;
                };
                if let Some(find) = &mut state.find {
                    find.replacing = !find.replacing;
                }
                drop(state);
                self.request_redraw();
                self.pump();
                return true;
            }
            key::RETURN => {
                let (replacing, project) = self
                    .ivars()
                    .state
                    .borrow()
                    .find
                    .as_ref()
                    .map(|f| (f.replacing, f.project))
                    .unwrap_or((false, false));
                if project {
                    let selected = {
                        let Some(state) = self.state() else {
                            return false;
                        };
                        state
                            .find
                            .as_ref()
                            .and_then(|bar| (!bar.results.is_empty()).then_some(bar.selected))
                    };
                    if let Some(index) = selected {
                        self.open_project_result(index);
                    } else {
                        self.search_project();
                    }
                } else if replacing {
                    self.replace_one();
                } else {
                    self.find_step(!shift);
                }
                return true;
            }
            _ => {}
        }

        // The find fields are real editors. Handle the familiar shortcuts
        // here as well as through menu actions, because AppKit does not send
        // every key equivalent through the menu on every keyboard layout.
        if command {
            let letter = event
                .charactersIgnoringModifiers()
                .and_then(|s| s.to_string().chars().next())
                .map(|c| c.to_ascii_lowercase());
            let Some(mut state) = self.state_mut() else {
                return false;
            };
            let Some(bar) = &mut state.find else {
                return false;
            };
            let replacing = bar.replacing;
            let field = if replacing {
                &mut bar.replacement
            } else {
                &mut bar.query
            };
            let mut changed = false;
            match (code, letter) {
                (key::DELETE, _) => {
                    field.delete_to_line_start();
                    changed = true;
                }
                (key::FORWARD_DELETE, _) => {
                    field.delete_to_line_end();
                    changed = true;
                }
                (key::LEFT, _) => {
                    field.move_line_start(if shift { Motion::Extend } else { Motion::Move })
                }
                (key::RIGHT, _) => {
                    field.move_line_end(if shift { Motion::Extend } else { Motion::Move })
                }
                (_, Some('a')) => field.select_all(),
                (_, Some('c')) => {
                    if let Some(text) = field.selected_text() {
                        clipboard::write_text(&text);
                    }
                }
                (_, Some('x')) => {
                    if let Some(text) = field.selected_text() {
                        clipboard::write_text(&text);
                        field.backspace();
                        changed = true;
                    }
                }
                (_, Some('v')) => {
                    if let Some(text) = clipboard::read_text() {
                        field.insert(&single_line(&text));
                        changed = true;
                    }
                }
                (_, Some('z')) => {
                    if shift {
                        field.redo();
                    } else {
                        field.undo();
                    }
                    changed = true;
                }
                _ => return false,
            }
            drop(state);
            if changed && !replacing {
                self.refresh_find();
            }
            self.request_redraw();
            self.pump();
            return true;
        }

        let motion = if shift { Motion::Extend } else { Motion::Move };
        {
            let mut as_text = false;
            let Some(mut state) = self.state_mut() else {
                return false;
            };
            let Some(bar) = &mut state.find else {
                return false;
            };
            let replacing = bar.replacing;
            let field = if replacing {
                &mut bar.replacement
            } else {
                &mut bar.query
            };
            match code {
                // One line: Up and Down go to its ends.
                key::UP => field.move_line_start(motion),
                key::DOWN => field.move_line_end(motion),
                _ => as_text = !field_key(field, code, flags),
            }
            if as_text {
                // The borrow has to end before the input system is asked,
                // because it answers by calling back into this view.
                drop(state);
                return self.interpret(event);
            }
            // Editing the replacement does not move the match.
            if replacing {
                drop(state);
                self.request_redraw();
                self.pump();
                return true;
            }
        }

        self.refresh_find();
        true
    }

    /// Re-runs the search from the top of the current match, so the selection
    /// tracks the query as it changes, however it changed.
    /// The find bar's matches in the active document, for an action: the
    /// needle, and the matches or `None` when there is nothing to do. Find
    /// reuses the matches drawing keeps; a replacement is expanded per
    /// match, so replacing searches afresh. No size limit here: an action
    /// runs once, drawing runs every frame. An invalid pattern says so.
    pub(super) fn action_matches(
        &self,
        replacement: Option<&str>,
    ) -> Option<(String, Vec<search::Match>)> {
        let mut state = self.state_mut()?;
        let state = &mut *state;
        let bar = state.find.as_ref()?;
        let needle = bar.query.rope.to_string();
        if needle.is_empty() {
            return None;
        }
        let buffer = state.docs.active();
        let found = match replacement {
            None => fill_find_cache(&mut state.find_cache, bar, buffer).map(<[_]>::to_vec),
            Some(with) => search::find(&buffer.rope.to_string(), &needle, with, bar.options),
        };
        match found {
            Ok(matches) => Some((needle, matches)),
            Err(error) => {
                state.message = Some((format!("invalid regex: {error}"), Instant::now()));
                None
            }
        }
    }

    pub(super) fn refresh_find(&self) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if let Some(cancel) = state.project_search.cancel.take() {
                cancel.store(true, Ordering::Relaxed);
            }
            state.project_search.rx = None;
            if let Some(bar) = &mut state.find
                && bar.project
            {
                bar.reset_results();
                bar.searching = false;
                return;
            }
        }
        let at = {
            let Some(state) = self.state() else {
                return;
            };
            let active = state.docs.active();
            active.selection().map_or(active.cursor(), |r| r.start)
        };
        let Some((_, matches)) = self.action_matches(None) else {
            return;
        };
        if let Some(found) = matches
            .iter()
            .find(|m| m.range.start >= at)
            .or_else(|| matches.first())
        {
            let (rows, cols) = self.grid();
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state
                .docs
                .active_mut()
                .select_range(found.range.start, found.range.end);
            state.docs.active_mut().scroll_to_cursor(rows, cols);
        }
    }

    /// Opens the find bar, seeding it from the selection when there is one.
    pub(super) fn open_find(&self) {
        let seed = {
            let Some(state) = self.state() else {
                return;
            };
            state
                .docs
                .active()
                .selected_text()
                .filter(|t| !t.contains('\n') && t.len() < 200)
        };
        let mut query = Buffer::new();
        if let Some(seed) = &seed {
            query.insert(seed);
            query.select_all();
        }
        if let Some(mut state) = self.state_mut() {
            close_fields(&mut state);
        }
        if let Some(mut state) = self.state_mut()
            && let Some(bar) = &mut state.find
        {
            // Already open: the keys come back to it, with its options, its
            // replacement and its results; a selection becomes the query.
            if seed.is_some() {
                bar.query = query;
            } else {
                bar.query.select_all();
            }
            bar.has_keys = true;
            bar.replacing = false;
            drop(state);
            self.refresh_find();
            self.request_redraw();
            self.pump();
            return;
        }
        if let Some(mut state) = self.state_mut() {
            state.find = Some(FindBar {
                query,
                replacement: Buffer::new(),
                replacing: false,
                has_keys: true,
                options: SearchOptions::default(),
                project: false,
                results: Vec::new(),
                selected: 0,
                result_scroll: 0,
                searching: false,
            });
        }
        self.request_redraw();
        self.pump();
    }

    pub(super) fn close_find(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if let Some(cancel) = state.project_search.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        state.project_search.rx = None;
        state.find = None;
        drop(state);
        self.request_redraw();
        self.pump();
    }

    pub(super) fn search_project(&self) {
        let (query, options, root) = {
            let Some(state) = self.state() else {
                return;
            };
            let Some(bar) = &state.find else { return };
            (
                bar.query.rope.to_string(),
                bar.options,
                state.tree.root().map(Path::to_path_buf),
            )
        };
        let Some(root) = root else { return };
        if query.is_empty() {
            return;
        }
        if let Err(error) = search::find("", &query, "", options) {
            if let Some(mut state) = self.state_mut() {
                state.message = Some((format!("invalid regex: {error}"), Instant::now()));
            }
            return;
        }
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        std::thread::spawn(move || {
            if let Some(result) = search::search_tree(root, &query, options, &worker_cancel) {
                let _ = tx.send(result);
            }
        });
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if let Some(old) = state.project_search.cancel.replace(cancel) {
            old.store(true, Ordering::Relaxed);
        }
        state.project_search.rx = Some(rx);
        state.project_search.references = false;
        if let Some(bar) = &mut state.find {
            bar.reset_results();
            bar.searching = true;
        }
        drop(state);
        self.request_redraw();
        self.pump();
    }

    pub(super) fn poll_project_search(&self) {
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
        if let Some(bar) = &mut state.find {
            bar.searching = false;
            match result {
                Ok(results) => {
                    let count = results.len();
                    bar.results = results;
                    bar.selected = 0;
                    bar.result_scroll = 0;
                    let text = if state.project_search.references {
                        format!("{count} reference{}", if count == 1 { "" } else { "s" })
                    } else {
                        format!(
                            "{count} project matches{}",
                            if count == 500 { " (first 500)" } else { "" }
                        )
                    };
                    state.message = Some((text, Instant::now()));
                }
                Err(error) => {
                    state.message = Some((format!("project search: {error}"), Instant::now()))
                }
            }
        }
        drop(state);
        self.request_redraw();
    }

    pub(super) fn open_project_result(&self, index: usize) {
        let target = {
            let Some(state) = self.state() else {
                return;
            };
            state
                .find
                .as_ref()
                .and_then(|bar| bar.results.get(index))
                .map(|hit| (hit.path.clone(), hit.line, hit.column, hit.range.len()))
        };
        let Some((path, line, column, len)) = target else {
            return;
        };
        self.load_path(&path.to_string_lossy());
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
            state.docs.active_mut().scroll_to_cursor(rows, cols);
        }
        self.sync_title();
        self.reparse();
        self.request_redraw();
        self.pump();
    }

    pub(super) fn find_click(&self, x: f32, y: f32, rect: Viewport) {
        // The same rectangles the bar was drawn from. This used to re-derive
        // them: `right - 180.0` here and `right - 180.0` there, option slots
        // 66pt wide in the handler and 62pt wide on screen, so a click beside
        // a chip still toggled it.
        let g = layout::FindGeometry::new(rect);
        let project = self
            .ivars()
            .state
            .borrow()
            .find
            .as_ref()
            .is_some_and(|bar| bar.project);

        for (slot, hit) in g.options.iter().enumerate() {
            if !hit.contains(x, y) {
                continue;
            }
            {
                let Some(mut state) = self.state_mut() else {
                    return;
                };
                if let Some(bar) = &mut state.find {
                    match slot {
                        0 => bar.options.case_sensitive = !bar.options.case_sensitive,
                        1 => bar.options.whole_word = !bar.options.whole_word,
                        2 => bar.options.regex = !bar.options.regex,
                        _ => bar.project = !bar.project,
                    }
                    bar.reset_results();
                }
            }
            self.refresh_find();
            self.request_redraw();
            self.pump();
            return;
        }

        if g.close.contains(x, y) {
            self.close_find();
        } else if g.previous.contains(x, y) || g.next.contains(x, y) {
            let forward = g.next.contains(x, y);
            if project {
                let Some(mut state) = self.state_mut() else {
                    return;
                };
                if let Some(bar) = &mut state.find
                    && !bar.results.is_empty()
                {
                    bar.selected = if forward {
                        (bar.selected + 1).min(bar.results.len() - 1)
                    } else {
                        bar.selected.saturating_sub(1)
                    };
                    bar.follow_selection();
                }
            } else {
                self.find_step(forward);
            }
        } else if g.replace_one.contains(x, y) {
            if project {
                self.search_project();
            } else {
                self.replace_one();
            }
        } else if g.replace_all.contains(x, y) {
            if project {
                self.replace_in_project();
            } else {
                self.replace_all();
            }
        } else if let Some(row) = g.result_row(y).filter(|_| project) {
            let selected = {
                let Some(state) = self.state() else {
                    return;
                };
                state
                    .find
                    .as_ref()
                    .filter(|bar| bar.result_scroll + row < bar.results.len())
                    .map(|bar| bar.result_scroll + row)
            };
            if let Some(index) = selected {
                self.open_project_result(index);
            }
            return;
        } else if g.find_field.contains(x, y) || g.replace_field.contains(x, y) {
            // Clicking a field focuses it and puts the caret where the
            // pointer is, measured through the same shaping that drew it.
            let replace = g.replace_field.contains(x, y);
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let State { find, renderer, .. } = &mut *state;
            if let Some(bar) = find {
                bar.replacing = replace;
                bar.has_keys = true;
                let box_rect = if replace {
                    g.replace_field
                } else {
                    g.find_field
                };
                let field = if replace {
                    &mut bar.replacement
                } else {
                    &mut bar.query
                };
                place_field_caret(&mut renderer.atlas, field, x - box_rect.x - FIND_FIELD_PAD);
            }
        }
        self.request_redraw();
        self.pump();
    }

    /// Replaces the current match, then advances to the next one.
    pub(super) fn replace_one(&self) {
        let (replacement, selected) = {
            let Some(state) = self.state() else {
                return;
            };
            let Some(bar) = &state.find else {
                return;
            };
            (
                bar.replacement.rope.to_string(),
                state.docs.active().selection(),
            )
        };
        let Some((_, matches)) = self.action_matches(Some(&replacement)) else {
            return;
        };
        let replaced = selected
            .and_then(|range| matches.iter().find(|m| m.range == range))
            .is_some_and(|found| {
                self.ivars()
                    .state
                    .borrow_mut()
                    .docs
                    .active_mut()
                    .insert(&found.replacement);
                true
            });

        // Whether or not this one matched, move on: pressing Return in the
        // replace field should always make progress through the file.
        self.find_step(true);
        if replaced {
            self.reparse();
        }
        self.request_redraw();
        self.pump();
    }

    /// Replace All in project mode: every file in the results, searched
    /// again now so the edits fit the text as it is. Open documents are
    /// edited in place, one undo step each, and left unsaved; other files
    /// are written through the normal save path. Refused when the results
    /// were cut at 500, since files past the cut would be missed.
    pub(super) fn replace_in_project(&self) {
        let (needle, replacement, options, files, matches) = {
            let Some(state) = self.state() else {
                return;
            };
            let Some(bar) = &state.find else { return };
            let mut files: Vec<std::path::PathBuf> = Vec::new();
            for hit in &bar.results {
                if !files.contains(&hit.path) {
                    files.push(hit.path.clone());
                }
            }
            (
                bar.query.rope.to_string(),
                bar.replacement.rope.to_string(),
                bar.options,
                files,
                bar.results.len(),
            )
        };
        let say = |text: String| {
            if let Some(mut state) = self.state_mut() {
                state.message = Some((text, Instant::now()));
            }
            self.request_redraw();
        };
        if needle.is_empty() || files.is_empty() {
            return say("search the project first; Replace All uses its results".into());
        }
        if matches >= 500 {
            return say("over 500 matches: narrow the search before replacing".into());
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
            for (path, done) in on_disk.iter().zip(edit_on_disk(&on_disk, edit)) {
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
        }
        self.lsp_flush_changes();
        let note = format!("replaced {replaced} in {}", outcome.describe());
        // The list described text that is gone. Not searched again: project
        // search reads the disk, and the open files' changes are not saved.
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if let Some(bar) = &mut state.find {
                bar.reset_results();
            }
        }
        self.reparse();
        self.sync_title();
        say(note);
        self.pump();
    }

    /// Replaces every match in the active document.
    pub(super) fn replace_all(&self) {
        let replacement = {
            let Some(state) = self.state() else {
                return;
            };
            let Some(bar) = &state.find else { return };
            bar.replacement.rope.to_string()
        };
        let Some((needle, matches)) = self.action_matches(Some(&replacement)) else {
            return;
        };
        let edits: Vec<_> = matches
            .into_iter()
            .map(|m| (m.range, m.replacement))
            .collect();

        let count = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let n = state.docs.active_mut().replace_ranges(&edits);
            state.message = Some((
                match n {
                    0 => format!("no matches for {needle:?}"),
                    1 => "replaced 1 occurrence".to_string(),
                    n => format!("replaced {n} occurrences"),
                },
                Instant::now(),
            ));
            n
        };
        if count > 0 {
            self.reparse();
        }
        self.request_redraw();
        self.pump();
    }

    /// Moves the cursor to the next or previous match and selects it.
    pub(super) fn find_step(&self, forward: bool) {
        let (from, selected) = {
            let Some(state) = self.state() else {
                return;
            };
            (
                state.docs.active().cursor(),
                state.docs.active().selection(),
            )
        };
        let Some((query, matches)) = self.action_matches(None) else {
            return;
        };
        let (rows, cols) = self.grid();
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let found = if forward {
            let start = selected.map_or(from, |r| r.start.saturating_add(1));
            matches
                .iter()
                .find(|m| m.range.start >= start)
                .or_else(|| matches.first())
        } else {
            let before = selected.map_or(from, |r| r.start);
            matches
                .iter()
                .rev()
                .find(|m| m.range.start < before)
                .or_else(|| matches.last())
        };

        let Some(found) = found else {
            state.message = Some((format!("no match for {query:?}"), Instant::now()));
            return;
        };

        state
            .docs
            .active_mut()
            .select_range(found.range.start, found.range.end);
        state.docs.active_mut().scroll_to_cursor(rows, cols);
    }
}
