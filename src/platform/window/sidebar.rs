//! The sidebar: the file tree and its drags, the Source Control view's
//! clicks and keys, and opening a folder with its project scan.

use super::*;

impl EditorView {
    /// Tracks a sidebar drag and works out whether it could be dropped here.
    pub(super) fn tree_drag_moved(&self, x: f32, y: f32) {
        let Some(rect) = self.chrome().sidebar else {
            return;
        };
        let (changed, active) = 'drag: {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let row = layout::sidebar_row_at(&state.tree, sidebar_field(&state), rect, y);
            // The destination directory: the folder under the pointer, the
            // parent of a file under it, or the project root below the tree.
            let destination = match row.and_then(|index| state.tree.rows().get(index)) {
                Some(entry) if entry.is_dir => Some(entry.path.clone()),
                Some(entry) => entry.path.parent().map(Path::to_path_buf),
                None => state.tree.root().map(Path::to_path_buf),
            };
            let Some(Drag::Tree(drag)) = &mut state.drag else {
                break 'drag (false, false);
            };
            if !drag.active {
                let far = (x - drag.origin.0).abs() > 4.0 || (y - drag.origin.1).abs() > 4.0;
                if !far {
                    break 'drag (false, false);
                }
                drag.active = true;
            }
            let valid = rect.contains(x, y)
                && destination
                    .as_deref()
                    .is_some_and(|dir| valid_drop(&drag.path, dir));
            let changed = drag.over != row || drag.valid != valid;
            drag.over = row;
            drag.valid = valid;
            (changed, true)
        };
        if active && changed {
            self.request_redraw();
            self.pump();
        }
    }

    /// Moves the dragged item into the folder it was dropped on.
    pub(super) fn drop_tree_item(&self, drag: TreeDrag) {
        let Some(rect) = self.chrome().sidebar else {
            return;
        };
        let destination = {
            let Some(state) = self.state() else {
                return;
            };
            match drag.over.and_then(|index| state.tree.rows().get(index)) {
                Some(entry) if entry.is_dir => Some(entry.path.clone()),
                Some(entry) => entry.path.parent().map(Path::to_path_buf),
                None => state.tree.root().map(Path::to_path_buf),
            }
        };
        let _ = rect;
        let Some(destination) = destination else {
            return;
        };
        let Some(name) = drag.path.file_name() else {
            return;
        };
        let target = destination.join(name);
        // Canonical identity before the move, for the same reason rename
        // captures it: afterwards the old path cannot be canonicalised.
        let source_key = crate::platform::canonical(&drag.path);
        // Unsaved work under the item would be stranded at a path that no
        // longer exists, the same guard Move to Trash uses.
        if self
            .ivars()
            .state
            .borrow()
            .docs
            .has_dirty_under(&source_key)
        {
            if let Some(mut state) = self.state_mut() {
                state.message = Some((
                    "save the affected tabs before moving this item".into(),
                    Instant::now(),
                ));
            }
            self.request_redraw();
            self.pump();
            return;
        }
        match move_without_replace(&drag.path, &target) {
            Ok(()) => {
                {
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    for docs in all_docs_mut(&mut state) {
                        docs.rename_path(&source_key, &target);
                    }
                    state.message = Some((
                        format!(
                            "moved {} to {}",
                            name.to_string_lossy(),
                            destination.display()
                        ),
                        Instant::now(),
                    ));
                }
                self.refresh_project_after_disk_change();
                self.sync_title();
                self.reparse();
            }
            Err(error) => {
                if let Some(mut state) = self.state_mut() {
                    state.message = Some((format!("move failed: {error}"), Instant::now()));
                }
            }
        }
        self.request_redraw();
        self.pump();
    }

    /// Closes every expanded directory in the tree.
    pub(super) fn collapse_tree(&self) {
        if let Some(mut state) = self.state_mut() {
            state.tree.collapse_all();
        }
        self.invalidate_tab_cursors();
        self.request_redraw();
        self.pump();
    }

    /// Re-reads the tree from disk. Nothing watches the filesystem, so a file
    /// created outside the editor needs asking for.
    pub(super) fn refresh_tree(&self) {
        self.refresh_project_after_disk_change();
    }

    pub(super) fn open_git(&self) {
        let Some(scm) = self.state().map(|state| state.git_open) else {
            return;
        };
        self.set_sidebar_view(!scm);
    }

    /// Switches the sidebar between the file tree and source control.
    ///
    /// Source control is a view of the project, not a modal: opening it does
    /// not take the keyboard, and leaving it gives the editor column back to
    /// the active document.
    pub(super) fn set_sidebar_view(&self, scm: bool) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.extensions = None;
            state.git_open = scm;
            // Typing belongs to the editor until the message field is asked
            // for. Focusing it here would swallow the next keystroke.
            state.git_focus = false;
            if scm {
                state.palette = None;
                state.goto = None;
                state.git.refresh();
            }
        }
        // A sidebar that has been hidden cannot show either view.
        if scm && !self.state().is_some_and(|state| state.sidebar) {
            self.action_toggle_sidebar(sel!(toggleSidebar:), None);
        }
        self.request_redraw();
        self.resume_display_link();
        self.pump();
    }

    /// A click inside the source-control column. Returns whether it landed on
    /// something; the caller falls through to the file tree otherwise.
    pub(super) fn git_click(&self, column: Viewport, x: f32, y: f32) -> bool {
        use crate::platform::git_panel::{Entry, Group, Sidebar};
        let Some(mut state) = self.state_mut() else {
            return false;
        };
        let g = Sidebar::new(column);
        let mut handled = true;
        let mut conflict_file = None;
        if g.refresh.contains(x, y) {
            state.git.refresh();
        } else if g.commit.contains(x, y) {
            state.git.commit();
        } else if g.message.contains(x, y) {
            state.git_focus = true;
            let State { git, renderer, .. } = &mut *state;
            place_field_caret(
                &mut renderer.atlas,
                &mut git.message,
                x - g.message.x - crate::platform::git_panel::MESSAGE_PAD,
            );
        } else if let Some((entry, rect)) = state.git.entry_at(g, x, y) {
            match entry {
                // A conflict is resolved in the file, not read as a diff.
                Entry::File {
                    change,
                    group: Group::Conflicts,
                } => {
                    state.git_focus = false;
                    conflict_file = state
                        .git
                        .snapshot
                        .as_ref()
                        .and_then(|s| s.changes.get(change).map(|c| s.root.join(&c.path)));
                }
                Entry::File { change, group } => {
                    let staged = group == Group::Staged;
                    // The staging control is on the row, so a click near the
                    // trailing edge stages rather than selects.
                    if g.toggle(rect).contains(x, y) {
                        state.git.stage_index(change, !staged);
                    } else {
                        state.git_focus = false;
                        state.git.select(change);
                        open_diff_tab(&mut state, change, staged);
                    }
                }
                Entry::Section { .. } => handled = false,
            }
        } else {
            handled = false;
        }
        drop(state);
        if let Some(path) = conflict_file
            && self.load_path(&path.to_string_lossy())
        {
            self.step_conflict(true, true);
        }
        self.request_redraw();
        self.resume_display_link();
        self.pump();
        handled
    }

    /// Keys while source control is showing. Only the commit message field
    /// claims them, and only once it has been clicked: a docked view that ate
    /// every keystroke would make the editor beside it unusable.
    pub(super) fn handle_git_key(&self, event: &NSEvent) -> bool {
        let flags = event.modifierFlags();
        if flags.contains(NSEventModifierFlags::Command) {
            // Commits from the message field; in a document, Cmd-Return is
            // the document's (a .http file sends its request).
            if event.keyCode() == key::RETURN && self.state().is_some_and(|state| state.git_focus) {
                if let Some(mut state) = self.state_mut() {
                    state.git.commit();
                }
                self.resume_display_link();
                return true;
            }
            return false;
        }
        if event.keyCode() == 53 {
            // Escape steps back out: first the diff, then the field, then the
            // view itself. Extra cursors in the document go first.
            {
                let Some(state) = self.state() else {
                    return false;
                };
                if !state.git_focus
                    && (state.docs.active().cursor_count() > 1 || state.lsp.signature.is_some())
                {
                    return false;
                }
            }
            let Some(mut state) = self.state_mut() else {
                return false;
            };
            if state.git_focus {
                state.git_focus = false;
            } else if diffing(&state) {
                let index = state.docs.active_index();
                state.docs.close(index);
                reveal_active_tab(&mut state);
                drop(state);
                self.sync_title();
                self.request_redraw();
                return true;
            } else {
                drop(state);
                self.set_sidebar_view(false);
                return true;
            }
            drop(state);
            self.request_redraw();
            return true;
        }
        if !self.state().is_some_and(|state| state.git_focus) {
            return false;
        }
        let Some(mut state) = self.state_mut() else {
            return false;
        };
        match event.keyCode() {
            key::RETURN | key::TAB => return true,
            code if field_key(&mut state.git.message, code, flags) => {}
            _ => {
                drop(state);
                return self.interpret(event);
            }
        }
        drop(state);
        self.resume_display_link();
        true
    }

    /// Handles a click in the sidebar: select, and toggle or open.
    pub(super) fn sidebar_click(&self, y: f32, rect: Viewport) {
        let index = {
            let Some(state) = self.state() else {
                return;
            };
            layout::sidebar_row_at(&state.tree, sidebar_field(&state), rect, y)
        };
        let Some(index) = index else {
            return;
        };

        let (path, is_dir, expanded, depth) = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(entry) = state.tree.select(index) else {
                return;
            };
            let result = (
                entry.path.clone(),
                entry.is_dir,
                entry.expanded,
                entry.depth,
            );
            state.tree_version += 1;
            // A folder keeps the keyboard in the tree; a file opens, and
            // the document has it.
            state.sidebar_keys = result.1;
            result
        };

        if is_dir {
            // Single click toggles a folder: a tree where you have to
            // double-click to see inside is needlessly slow to browse.
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if expanded {
                state.tree.toggle(index);
            } else if state.watch.children_pending.insert(path.clone()) {
                let root = state.tree.root().map(Path::to_path_buf);
                let tx = state.watch.children_tx.clone();
                std::thread::spawn(move || {
                    if let Some(root) = root {
                        let children = Tree::children(&path, depth + 1);
                        let _ = tx.send((root, path, children));
                    }
                });
            } else {
                // A second click before the worker finishes cancels expansion.
                state.watch.children_pending.remove(&path);
            }
            drop(state);
            self.resume_display_link();
        } else {
            // Opening a file now adds a tab rather than replacing what is
            // showing, so nothing has to be saved or discarded first.
            self.load_path(&path.to_string_lossy());
            self.sync_title();
            self.reparse();
        }

        self.request_redraw();
        self.pump();
    }

    /// Prompts for a directory and loads it as the project root.
    pub(super) fn open_folder(&self) -> bool {
        // A test instance never shows the panel: it opens on the person's
        // real folders, and scripted keys meant for the editor would pick
        // one and index it.
        if self.ivars().testing {
            if let Some(mut state) = self.state_mut() {
                state.message = Some(("would show the Open Folder panel".into(), Instant::now()));
            }
            return true;
        }
        let Some(path) = choose_path(MainThreadMarker::from(self), true, None) else {
            return false;
        };
        self.load_folder_path(&path.to_string_lossy());
        true
    }

    pub(super) fn load_folder_path(&self, path: &str) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        set_project_root(&mut state, Path::new(path));
        let recent = std::mem::take(&mut state.recent_projects);
        state.recent_projects = with_recent(recent, state.tree.root());
        state.sidebar = true;
        // A project is open now, which is worth coming back to.
        state.ephemeral_session = false;
        state.message = Some(("Folder opened".to_string(), Instant::now()));
        state.lsp.servers.clear();
        state.lsp.unavailable.clear();
        state.completion = None;
        drop(state);
        self.watch_project();
        self.lsp_sync_open();
        self.request_redraw();
        self.pump();
        self.resume_display_link();
    }

    pub(super) fn poll_project_index(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some(rx) = &state.watch.index_rx else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                state.watch.index_rx = None;
                return;
            }
        };
        state.watch.index_rx = None;
        let ProjectIndexResult {
            root,
            mut tree,
            finder,
            tree_version,
        } = result;
        if state.tree.root() != Some(root.as_path()) {
            return;
        }
        if state.tree_version != tree_version {
            state.finder = finder;
            state.watch.index_rx = Some(spawn_project_refresh(
                state.tree.clone(),
                state.tree_version,
            ));
            return;
        }
        if let Some(path) = state.docs.active().path.as_deref() {
            tree.reveal(path);
        }
        // The worker's tree carries the ignore set from when it started;
        // keep the newer one, then ask Git again, since whatever changed
        // the tree may have changed what is ignored.
        tree.set_ignored(state.tree.ignored_set());
        state.tree = tree;
        // Said once per scan: Go to File does not list everything.
        if finder.is_truncated() && !state.finder.is_truncated() {
            state.message = Some((
                format!(
                    "Go to File lists the first {} files of this project",
                    finder.len()
                ),
                Instant::now(),
            ));
        }
        state.finder = finder;
        state.tree_version += 1;
        state.watch.ignored_rx = Some(spawn_ignored(root));
        drop(state);
        self.request_redraw();
        self.resume_display_link();
    }

    /// Takes Git's answer about ignored paths, if it is for this root.
    pub(super) fn poll_ignored(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some(rx) = &state.watch.ignored_rx else {
            return;
        };
        let (root, ignored) = match rx.try_recv() {
            Ok(answer) => answer,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                state.watch.ignored_rx = None;
                return;
            }
        };
        state.watch.ignored_rx = None;
        if state.tree.root() == Some(root.as_path()) {
            state.tree.set_ignored(std::sync::Arc::new(ignored));
            drop(state);
            self.request_redraw();
        }
    }

    pub(super) fn poll_tree_children(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let mut changed = false;
        while let Ok((root, path, children)) = state.watch.children_rx.try_recv() {
            if state.tree.root() != Some(root.as_path())
                || !state.watch.children_pending.remove(&path)
            {
                continue;
            }
            if state.tree.install_children(&path, children) {
                state.tree_version += 1;
                changed = true;
            }
        }
        drop(state);
        if changed {
            self.request_redraw();
        }
    }
}
