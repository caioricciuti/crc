//! Documents and the disk: opening, saving and Save As, reverting, the
//! checks for files changed on disk, and the session kept across launches.

use super::*;

impl EditorView {
    /// Saves a document the formatter just changed, without formatting it
    /// again on the way out.
    pub(super) fn save_formatted(&self) {
        if let Some(mut state) = self.state_mut() {
            state.saving_formatted = true;
        }
        self.save(false);
        if let Some(mut state) = self.state_mut() {
            state.saving_formatted = false;
        }
    }

    /// Runs the open panel and opens the chosen file in a tab.
    pub(super) fn open_file(&self) -> bool {
        // As open_folder: a test instance never shows the panel.
        if self.ivars().testing {
            if let Some(mut state) = self.state_mut() {
                state.message = Some(("would show the Open panel".into(), Instant::now()));
            }
            return true;
        }
        match choose_path(MainThreadMarker::from(self), false, None) {
            Some(path) => self.load_path(&path.to_string_lossy()),
            None => false,
        }
    }

    /// Snapshots what is open, for the next launch.
    pub(super) fn capture_session(&self) -> Option<Session> {
        let state = self.state()?;
        let frame = self.window().map(|w| {
            let f = w.frame();
            (f.origin.x, f.origin.y, f.size.width, f.size.height)
        });
        Some(Session {
            frame,
            sidebar_width: state.sidebar_width,
            folder: state.tree.root().map(Path::to_path_buf),
            // Every pane's files, focused pane first; they come back in one
            // pane. Splits are a way of looking, not something to restore.
            files: all_docs(&state)
                .flat_map(|d| d.iter().filter_map(|b| b.path.clone()))
                .collect(),
            // An index into `files`, which leaves out untitled documents, so
            // count only the named ones in front of the active tab. An
            // untitled active tab is not in the list: the named one before
            // it stands in, not the one after.
            active: {
                let before = state
                    .docs
                    .iter()
                    .take(state.docs.active_index())
                    .filter(|b| b.path.is_some())
                    .count();
                if state.docs.active().path.is_some() {
                    before
                } else {
                    before.saturating_sub(1)
                }
            },
            sidebar: state.sidebar,
            recent: state.recent_projects.clone(),
        })
    }

    /// Asks about every document with unsaved changes, one at a time, showing
    /// each as it is asked about. Returns whether the window may go.
    ///
    /// Closing the window and quitting take every tab with them, not only the
    /// one in front, so asking about the active document alone let the rest
    /// vanish without a word.
    pub(super) fn confirm_discard_all(&self) -> bool {
        if self.state().is_some_and(|state| state.discard_confirmed) {
            return true;
        }
        // Where the person was: the pane, and the document by identity,
        // since the loop moves between panes and closes tabs.
        let (original_pane, original_doc) = {
            let Some(state) = self.state() else {
                return false;
            };
            (state.focused_pane, state.docs.active().id())
        };
        if let Some(session) = self.capture_session()
            && let Some(mut state) = self.state_mut()
        {
            state.quit_session = Some(session);
        }
        loop {
            // Whichever pane has a dirty document comes to the front first.
            let elsewhere = {
                let Some(state) = self.state() else {
                    return false;
                };
                (0..pane_count(&state)).find(|p| {
                    *p != state.focused_pane
                        && state.panes[p - usize::from(*p > state.focused_pane)]
                            .docs
                            .iter()
                            .any(|b| b.is_dirty())
                })
            };
            let next = {
                let Some(state) = self.state() else {
                    return false;
                };
                state.docs.iter().position(|b| b.is_dirty())
            };
            let index = match (next, elsewhere) {
                (Some(index), _) => index,
                (None, Some(pane)) => {
                    self.focus_pane(pane);
                    continue;
                }
                (None, None) => break,
            };

            // The alert names the active document and Save acts on it, so
            // bring the one in question to the front. It spins a nested run
            // loop, which is why nothing is borrowed across it.
            self.activate_tab(index);
            self.request_redraw();
            self.pump();

            match self.ask_about_active() {
                Discard::Cancel => {
                    if let Some(mut state) = self.state_mut() {
                        state.quit_session = None;
                    }
                    self.focus_pane(original_pane);
                    let at = {
                        let Some(state) = self.state() else {
                            return false;
                        };
                        state.docs.iter().position(|b| b.id() == original_doc)
                    };
                    if let Some(at) = at {
                        self.activate_tab(at);
                    }
                    self.request_redraw();
                    self.pump();
                    return false;
                }
                // Saved: no longer dirty, so the scan moves on by itself.
                Discard::Saved => {}
                // Not saved and not wanted. Closing the tab is what stops the
                // scan finding it again, and the window is going anyway.
                Discard::Dropped => {
                    if let Some(mut state) = self.state_mut() {
                        state.docs.close(index);
                    }
                }
            }
        }
        if let Some(mut state) = self.state_mut() {
            state.discard_confirmed = true;
        }
        true
    }

    /// Asks about unsaved changes in the active document. Returns whether it
    /// is safe to proceed.
    ///
    /// This is the only thing standing between a stray Cmd-W and losing an
    /// afternoon's work, so it is a real modal, not a status-line note.
    pub(super) fn confirm_discard(&self) -> bool {
        !matches!(self.ask_about_active(), Discard::Cancel)
    }

    /// Save found the file changed (or gone) since it was opened.
    pub(super) fn ask_about_conflict(&self, missing: bool) -> Conflict {
        let name = self
            .state()
            .map(|state| state.docs.active().display_name())
            .unwrap_or_default();
        let mtm = MainThreadMarker::from(self);
        let answer = if missing {
            ask(
                mtm,
                &format!("\u{201c}{name}\u{201d} was deleted on disk."),
                "Saving will create the file again with the text in this tab.",
                &["Save Anyway", "Cancel"],
            )
        } else {
            ask(
                mtm,
                &format!("\u{201c}{name}\u{201d} has changed on disk since you opened it."),
                "Overwrite keeps the text in this tab. Reload takes the version on \
                 disk and drops your changes; Undo brings them back.",
                &["Overwrite", "Cancel", "Reload"],
            )
        };
        match answer {
            0 => Conflict::Overwrite,
            2 if !missing => Conflict::Reload,
            _ => Conflict::Cancel,
        }
    }

    /// File > Revert to Saved: back to what is on disk, asking first when
    /// that drops unsaved changes.
    pub(super) fn revert_to_saved(&self) {
        let (has_path, dirty) = {
            let Some(state) = self.state() else {
                return;
            };
            let active = state.docs.active();
            (active.path.is_some(), active.is_dirty())
        };
        if !has_path {
            return;
        }
        if dirty && !self.confirm_revert() {
            return;
        }
        let Some(result) = self
            .state_mut()
            .map(|mut state| state.docs.active_mut().reload())
        else {
            return;
        };
        self.after_reload(true);
        let Some(mut state) = self.state_mut() else {
            return;
        };
        state.message = Some((
            match result {
                Ok(()) => "reverted to the saved version".to_string(),
                Err(e) => format!("revert failed: {e}"),
            },
            Instant::now(),
        ));
        drop(state);
        self.sync_title();
    }

    pub(super) fn confirm_revert(&self) -> bool {
        let Some(name) = self.state().map(|state| state.docs.active().display_name()) else {
            return false;
        };
        ask(
            MainThreadMarker::from(self),
            &format!("Revert \u{201c}{name}\u{201d} to the saved version?"),
            "Your unsaved changes will be replaced by the file on disk. Undo brings them back.",
            &["Revert", "Cancel"],
        ) == 0
    }

    /// Looks at every open file and takes in what changed behind the editor.
    ///
    /// A clean tab follows the disk: this is what makes an edit by Claude
    /// Code, a `git checkout` or another editor show up instead of sitting
    /// stale until a save fails. A tab with unsaved changes is left alone
    /// and told, since choosing between two versions is the user's call,
    /// which Save then asks. A deleted file marks its tab unsaved: the text
    /// in the tab is now the only copy.
    pub(super) fn check_open_files(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let active_id = state.docs.active().id();
        let mut background = Vec::new();
        let mut reloaded = Vec::new();
        let mut conflicts = Vec::new();
        let mut missing = Vec::new();
        let mut lsp_dirty = Vec::new();
        let mut active_changed = false;
        for docs in all_docs_mut(&mut state) {
            for buffer in docs.iter_mut() {
                let Some(name) = buffer
                    .path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                else {
                    continue;
                };
                match buffer.disk_state() {
                    DiskState::Unchanged => {}
                    DiskState::Changed if buffer.is_dirty() => {
                        if !buffer.conflict_noticed {
                            buffer.conflict_noticed = true;
                            conflicts.push(name);
                        }
                    }
                    // A large file is read on a worker; its tab is replaced
                    // when the read is done (`poll_reloads`).
                    DiskState::Changed
                        if buffer.path.as_deref().is_some_and(|p| {
                            std::fs::metadata(p).is_ok_and(|m| m.len() > RELOAD_INLINE_BYTES)
                        }) =>
                    {
                        if let Some(path) = buffer.path.clone() {
                            background.push((buffer.id(), path));
                        }
                    }
                    DiskState::Changed => match buffer.reload() {
                        Ok(()) => {
                            buffer.conflict_noticed = false;
                            reloaded.push(name);
                            lsp_dirty.push(buffer.id());
                            active_changed |= buffer.id() == active_id;
                        }
                        Err(_) => {
                            // Unreadable right now, mid-write most likely.
                            // The next batch or save tries again.
                        }
                    },
                    DiskState::Missing => {
                        if !buffer.is_dirty() || !buffer.conflict_noticed {
                            buffer.note_missing_on_disk();
                            buffer.conflict_noticed = true;
                            missing.push(name);
                        }
                    }
                }
            }
        }
        for id in lsp_dirty {
            state.lsp.dirty.insert(id, Instant::now());
            state.gutter.dirty.insert(id, Instant::now());
        }
        for (id, path) in background {
            if !state.reloads.pending.insert(id) {
                continue;
            }
            let tx = state.reloads.channel.0.clone();
            std::thread::spawn(move || {
                let _ = tx.send((id, Buffer::read_disk(&path)));
            });
        }
        let message = match (reloaded.len(), conflicts.len(), missing.len()) {
            (0, 0, 0) => None,
            (1, 0, 0) => Some(format!("{} changed on disk, reloaded", reloaded[0])),
            (n, 0, 0) => Some(format!("{n} files changed on disk, reloaded")),
            (_, 1, 0) => Some(format!(
                "{} changed on disk; you have unsaved changes, Save will ask",
                conflicts[0]
            )),
            (_, 0, 1) => Some(format!("{} was deleted on disk", missing[0])),
            _ => Some("files changed on disk; see each tab".to_string()),
        };
        if let Some(message) = message {
            state.message = Some((message, Instant::now()));
        }
        drop(state);
        if active_changed {
            self.after_reload(false);
        }
        self.sync_title();
        self.resume_display_link();
        self.request_redraw();
    }

    /// Takes the large files read for a reload by `check_open_files`, into
    /// tabs that are still clean and whose file has not changed again.
    pub(super) fn poll_reloads(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let mut names = Vec::new();
        let mut active_changed = false;
        let active_id = state.docs.active().id();
        while let Ok((id, read)) = state.reloads.channel.1.try_recv() {
            state.reloads.pending.remove(&id);
            let Ok(read) = read else { continue };
            let mut all = all_docs_mut(&mut state);
            let Some(buffer) = all
                .iter_mut()
                .flat_map(|d| d.iter_mut())
                .find(|b| b.id() == id)
            else {
                continue;
            };
            let current = buffer
                .path
                .as_deref()
                .and_then(|p| crate::text::buffer::DiskStamp::of(p).ok());
            if buffer.is_dirty() || current != read.stamp() {
                continue;
            }
            buffer.apply_disk(read);
            if let Some(name) = buffer.path.as_ref().and_then(|p| p.file_name()) {
                names.push(name.to_string_lossy().into_owned());
            }
            active_changed |= id == active_id;
            drop(all);
            state.lsp.dirty.insert(id, Instant::now());
            state.gutter.dirty.insert(id, Instant::now());
        }
        if names.is_empty() {
            return;
        }
        state.message = Some((
            match names.as_slice() {
                [one] => format!("{one} changed on disk, reloaded"),
                many => format!("{} files changed on disk, reloaded", many.len()),
            },
            Instant::now(),
        ));
        drop(state);
        if active_changed {
            self.after_reload(false);
        }
        self.sync_title();
        self.request_redraw();
    }

    /// The active document's text was replaced from disk: everything keyed
    /// on it starts over. `announce` is for the explicit reloads, which
    /// also drop the completion popup and any live search.
    pub(super) fn after_reload(&self, announce: bool) {
        self.reparse();
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let active = state.docs.active();
            let id = active.id();
            if active.path.is_some() {
                state.lsp.dirty.insert(id, Instant::now());
                state.gutter.dirty.insert(id, Instant::now());
            }
            state.completion = None;
            if announce {}
        }
        self.request_redraw();
    }

    pub(super) fn ask_about_active(&self) -> Discard {
        let (dirty, name) = {
            let Some(state) = self.state() else {
                return Discard::Cancel;
            };
            (
                state.docs.active().is_dirty(),
                state.docs.active().display_name(),
            )
        };
        if !dirty {
            return Discard::Saved;
        }

        // Save first: the first button is the default.
        let answer = ask(
            MainThreadMarker::from(self),
            &format!("Do you want to save the changes to \u{201c}{name}\u{201d}?"),
            "Your changes will be lost if you don't save them.",
            &["Save", "Cancel", "Don't Save"],
        );
        match answer {
            // A save that was cancelled from its panel, or failed, is a
            // cancel: the work is still only in memory.
            0 if self.save(false) => Discard::Saved,
            2 => Discard::Dropped,
            _ => Discard::Cancel,
        }
    }

    /// Where a save goes: `Some(None)` to the document's own file,
    /// `Some(Some(path))` where the save panel said, `None` when it was
    /// cancelled. The panel shows for Save As and for an untitled document.
    pub(super) fn choose_save_path(&self, force_panel: bool) -> Option<Option<std::path::PathBuf>> {
        let untitled = self.state()?.docs.active().path.is_none();
        if force_panel || untitled {
            let mtm = MainThreadMarker::from(self);
            let panel = NSSavePanel::savePanel(mtm);
            let suggested = {
                let state = self.state()?;
                state
                    .docs
                    .active()
                    .path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Untitled.txt".to_string())
            };
            panel.setNameFieldStringValue(&NSString::from_str(&suggested));
            if panel.runModal() != MODAL_RESPONSE_OK {
                return None;
            }
            let path = panel.URL()?.path()?;
            Some(Some(std::path::PathBuf::from(path.to_string())))
        } else {
            Some(None)
        }
    }

    /// What follows a save: settings and ignore rules re-read when the file
    /// was one of those, the servers told, and format or organize on save.
    pub(super) fn after_saved(&self, saved_path: Option<std::path::PathBuf>, ok: bool) {
        if saved_path.is_some() && saved_path == crate::platform::settings::Settings::path() {
            self.apply_settings_file();
        }
        // The watcher ignores this process's own writes, so a .gitignore
        // saved here would otherwise leave the Explorer showing the old rules.
        if saved_path.as_deref().is_some_and(changes_ignore_rules) {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if let Some(root) = state.tree.root().map(Path::to_path_buf) {
                state.watch.ignored_rx = Some(spawn_ignored(root));
            }
        }
        if ok {
            self.resume_display_link();
        }
        if let Some(path) = saved_path {
            self.lsp_flush_changes();
            self.lsp_sync_open();
            let Some(mut state) = self.state_mut() else {
                return;
            };
            for server in state.lsp.servers.values_mut() {
                server.did_save(&path);
            }
            // Saved first, formatted after: a server that never answers
            // cannot hold a save hostage. The formatted text is saved again.
            let format = state.format_on_save && !state.saving_formatted;
            let organize = state.organize_on_save && !state.saving_formatted;
            drop(state);
            // Organize first; it goes on to format once its edits are in.
            if !(organize && self.organize_imports(true)) && format {
                self.format_document(true);
            }
        }
    }

    /// Saves, falling back to a Save As panel when there is no path yet.
    /// Returns whether the file actually reached disk.
    pub(super) fn save(&self, force_panel: bool) -> bool {
        let Some(chosen) = self.choose_save_path(force_panel) else {
            return false;
        };

        let Some(mut state) = self.state_mut() else {
            return false;
        };
        let mut result = state.docs.active_mut().save(chosen.as_deref());
        // Save As onto the tab's own file is a plain save, conflicts and all.
        let own_file = match (&chosen, &state.docs.active().path) {
            (None, _) => true,
            (Some(chosen), Some(current)) => same_file(chosen, current),
            (Some(_), None) => false,
        };
        if let Err(e) = &result
            && matches!(
                e.kind(),
                std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::NotFound
            )
            && own_file
        {
            // Someone else changed or removed the file. The alert spins a
            // nested run loop, so the borrow goes first.
            let missing = e.kind() == std::io::ErrorKind::NotFound;
            drop(state);
            match self.ask_about_conflict(missing) {
                Conflict::Overwrite => {
                    let Some(again) = self.state_mut() else {
                        return false;
                    };
                    state = again;
                    result = state.docs.active_mut().save_overwriting(chosen.as_deref());
                }
                Conflict::Reload => {
                    let Some(again) = self.state_mut() else {
                        return false;
                    };
                    state = again;
                    result = state.docs.active_mut().reload();
                    drop(state);
                    self.after_reload(true);
                    let Some(mut state) = self.state_mut() else {
                        return false;
                    };
                    state.message = Some((
                        match &result {
                            Ok(()) => "reloaded from disk".to_string(),
                            Err(e) => format!("reload failed: {e}"),
                        },
                        Instant::now(),
                    ));
                    drop(state);
                    self.sync_title();
                    return false;
                }
                Conflict::Cancel => {
                    let Some(again) = self.state_mut() else {
                        return false;
                    };
                    state = again;
                    state.message = Some(("not saved".to_string(), Instant::now()));
                    return false;
                }
            }
        }
        let ok = result.is_ok();
        state.message = Some((
            match result {
                Ok(()) => match &state.docs.active_mut().path {
                    Some(p) => format!("saved {}", p.display()),
                    None => "saved".to_string(),
                },
                Err(e) => format!("save failed: {e}"),
            },
            Instant::now(),
        ));
        if ok {
            note_files_written(&mut state);
        }
        let saved_path = ok.then(|| state.docs.active().path.clone()).flatten();
        drop(state);
        self.after_saved(saved_path, ok);
        self.sync_title();
        ok
    }
}
