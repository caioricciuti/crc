//! The project in the window: watching it for changes, and the sidebar's
//! new file, new folder and rename fields.

use super::*;

impl EditorView {
    /// Starts, or restarts, the FSEvents watcher on the current root.
    pub(super) fn watch_project(&self) {
        let root = self
            .ivars()
            .state
            .borrow()
            .tree
            .root()
            .map(Path::to_path_buf);
        let Some(mut state) = self.state_mut() else {
            return;
        };
        state.watch.watcher = None;
        let Some(root) = root else {
            state.watch.indexer = None;
            return;
        };
        if state.watch.indexer.as_ref().is_none_or(|i| i.root != root) {
            state.watch.indexer = crate::index::store::Indexer::start(root.clone());
        }
        // The view lives for the process; the display link holds it the
        // same way. The handler only ever runs on the main thread.
        let view = self as *const EditorView;
        state.watch.watcher = crate::project::watch::Watcher::new(
            &root,
            Box::new(move |change| {
                let view = unsafe { &*view };
                view.project_changed(change);
            }),
        );
    }

    /// Main thread, from the watcher: something outside the editor touched
    /// the project.
    pub(super) fn project_changed(&self, change: crate::project::watch::Change) {
        {
            // A main-queue block also runs inside modal alerts, panels and
            // menu tracking, where the state may be borrowed.
            let Some(mut state) = self.state_mut() else {
                let (tree, git) = self.ivars().deferred_change.get();
                self.ivars().deferred_change.set(match change {
                    crate::project::watch::Change::Tree => (true, git),
                    crate::project::watch::Change::Git => (tree, true),
                });
                self.resume_display_link();
                return;
            };
            match change {
                crate::project::watch::Change::Tree => {
                    state.watch.tree_changed_at = Some(Instant::now());
                }
                crate::project::watch::Change::Git => {
                    // Read with the panel closed as well: a merge in the
                    // terminal that leaves conflicts is found here, and so
                    // are the status bar's branch and the gutter's HEAD.
                    // The panel's own operations land here too; re-reading
                    // after them costs one status, which writes nothing
                    // under .git (optional locks are off), so it cannot
                    // come back round.
                    state.watch.git_changed_at = Some(Instant::now());
                }
            }
        }
        self.resume_display_link();
    }

    /// Rebuilds the tree and finder once the watcher has been quiet for a
    /// moment, and never while a rebuild is already running.
    pub(super) fn refresh_after_watch(&self) {
        const SETTLE: Duration = Duration::from_millis(300);
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if state
            .review_scan
            .as_ref()
            .is_some_and(|rx| !matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)))
        {
            state.review_scan = None;
            super::review::refresh_review(&mut state);
            self.ivars().needs_redraw.set(true);
        }
        // While Git is busy the change waits: it may be someone else's,
        // arriving during the panel's own operation.
        if let Some(at) = state.watch.git_changed_at
            && at.elapsed() >= SETTLE
            && !state.git.busy()
        {
            state.watch.git_changed_at = None;
            state.gutter.mark_stale();
            state.git.refresh();
            start_home_summary(&mut state);
        }
        let Some(at) = state.watch.tree_changed_at else {
            return;
        };
        if at.elapsed() < SETTLE || state.watch.index_rx.is_some() {
            return;
        }
        state.watch.tree_changed_at = None;
        super::review::note_unhooked_writes(&mut state);
        if let Some(indexer) = &state.watch.indexer {
            indexer.poke();
        }
        start_project_refresh(&mut state);
        start_home_summary(&mut state);
        if state.git_open {
            state.git.refresh();
        }
        drop(state);
        self.check_open_files();
        self.resume_display_link();
    }

    pub(super) fn refresh_project_after_disk_change(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        start_project_refresh(&mut state);
        state.git.refresh();
        drop(state);
        self.resume_display_link();
    }

    /// Creates a new file next to the current selection and opens it.
    ///
    /// Uses the standard save panel rather than an inline text field: it
    /// already handles naming, overwrite confirmation, and picking a
    /// different folder, none of which exist here yet.
    /// New File: a name field in the tree when a project is open, the save
    /// panel when there is no tree to put one in.
    pub(super) fn new_file(&self) -> bool {
        if self
            .state()
            .is_some_and(|state| state.tree.root().is_some())
        {
            self.start_sidebar_edit(SidebarEditKind::NewFile);
            return true;
        }
        self.new_file_with_panel()
    }

    pub(super) fn new_folder(&self) -> bool {
        if self
            .state()
            .is_some_and(|state| state.tree.root().is_none())
        {
            return false;
        }
        self.start_sidebar_edit(SidebarEditKind::NewFolder);
        true
    }

    /// Opens a name field in the sidebar for `kind`, at the row where the
    /// item will appear.
    pub(super) fn start_sidebar_edit(&self, kind: SidebarEditKind) {
        // Source control shares the column; the field belongs to the tree.
        if self.state().is_some_and(|state| state.git_open) {
            self.set_sidebar_view(false);
        }
        let Some(mut state) = self.state_mut() else {
            return;
        };
        state.sidebar = true;
        close_fields(&mut state);
        let Some(root) = state.tree.root().map(Path::to_path_buf) else {
            return;
        };
        let (parent, row, depth, field) = match &kind {
            SidebarEditKind::Rename(path) => {
                let Some(index) = state.tree.rows().iter().position(|e| &e.path == path) else {
                    return;
                };
                let entry = &state.tree.rows()[index];
                let field = rename_field(&entry.name, entry.is_dir);
                let parent = path.parent().map_or(root.clone(), Path::to_path_buf);
                (parent, index, entry.depth, field)
            }
            SidebarEditKind::NewFile | SidebarEditKind::NewFolder => {
                let parent = state.tree.target_dir().unwrap_or(root.clone());
                let at = state
                    .tree
                    .rows()
                    .iter()
                    .position(|e| e.is_dir && e.path == parent);
                let (row, depth) = match at {
                    Some(index) => (index + 1, state.tree.rows()[index].depth + 1),
                    None => (0, 0),
                };
                (parent, row, depth, Buffer::new())
            }
        };
        // Scroll the row into view.
        let chrome = chrome_of(&state);
        if let Some(rect) = chrome.sidebar {
            let rows = layout::sidebar_rows(rect).max(1);
            if row < state.tree.scroll {
                state.tree.scroll = row;
            } else if row >= state.tree.scroll + rows {
                state.tree.scroll = row + 1 - rows;
            }
        }
        state.sidebar_edit = Some(SidebarEdit {
            kind,
            parent,
            row,
            depth,
            field,
        });
        drop(state);
        self.request_redraw();
        self.pump();
    }

    /// Keys while a name is being typed in the tree.
    pub(super) fn handle_sidebar_edit_key(&self, event: &NSEvent) -> bool {
        const ESCAPE: u16 = 53;
        let flags = event.modifierFlags();
        if flags.contains(NSEventModifierFlags::Command) {
            // Cmd-A, Cmd-V and friends arrive through the Edit menu.
            return false;
        }
        let motion = if flags.contains(NSEventModifierFlags::Shift) {
            Motion::Extend
        } else {
            Motion::Move
        };
        let option = flags.contains(NSEventModifierFlags::Option);
        let code = match event.keyCode() {
            key::KEYPAD_ENTER => key::RETURN,
            code => code,
        };
        match code {
            ESCAPE => self.finish_sidebar_edit(false),
            key::RETURN => self.finish_sidebar_edit(true),
            key::TAB => return true,
            key::DELETE => {
                let Some(mut state) = self.state_mut() else {
                    return false;
                };
                if let Some(edit) = &mut state.sidebar_edit {
                    if option {
                        edit.field.delete_word_backward();
                    } else {
                        edit.field.backspace();
                    }
                }
            }
            key::LEFT | key::RIGHT | key::HOME | key::END => {
                let Some(mut state) = self.state_mut() else {
                    return false;
                };
                if let Some(edit) = &mut state.sidebar_edit {
                    match code {
                        key::LEFT => edit.field.move_left(motion),
                        key::RIGHT => edit.field.move_right(motion),
                        key::HOME => edit.field.move_line_start(motion),
                        _ => edit.field.move_line_end(motion),
                    }
                }
            }
            key::UP | key::DOWN => return true,
            _ => return self.interpret(event),
        }
        self.request_redraw();
        self.pump();
        true
    }

    /// Ends the inline field: creates, renames, or does nothing.
    pub(super) fn finish_sidebar_edit(&self, commit: bool) {
        let Some(edit) = self
            .state_mut()
            .and_then(|mut state| state.sidebar_edit.take())
        else {
            return;
        };
        let name = edit.field.rope.to_string();
        let name = name.trim();
        if !commit || name.is_empty() {
            self.request_redraw();
            return;
        }
        let relative = Path::new(name);
        let single = relative.file_name().is_some_and(|part| part == name);
        let nested_ok = !name.starts_with('/')
            && relative.components().all(
                |c| matches!(c, std::path::Component::Normal(part) if part != "." && part != ".."),
            );
        let valid = match edit.kind {
            SidebarEditKind::NewFile | SidebarEditKind::NewFolder => nested_ok,
            SidebarEditKind::Rename(_) => single,
        };
        if !valid {
            if let Some(mut state) = self.state_mut() {
                state.message = Some((format!("not a valid name: {name}"), Instant::now()));
            }
            self.request_redraw();
            return;
        }
        let target = edit.parent.join(relative);
        match edit.kind {
            SidebarEditKind::NewFile => {
                if target.exists() {
                    if let Some(mut state) = self.state_mut() {
                        state.message = Some((format!("{name} already exists"), Instant::now()));
                    }
                } else {
                    let created = target
                        .parent()
                        .map_or(Ok(()), std::fs::create_dir_all)
                        // Never truncates: a file that appeared since the
                        // check above is an error, not an empty file.
                        .and_then(|()| std::fs::File::create_new(&target).map(drop));
                    match created {
                        Ok(()) => self.open_created_file(&target),
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                            if let Some(mut state) = self.state_mut() {
                                state.message =
                                    Some((format!("{name} already exists"), Instant::now()));
                            }
                        }
                        Err(error) => {
                            if let Some(mut state) = self.state_mut() {
                                state.message =
                                    Some((format!("could not create: {error}"), Instant::now()));
                            }
                        }
                    }
                }
            }
            SidebarEditKind::NewFolder => {
                let result = if target.exists() {
                    Err(std::io::Error::other("already exists"))
                } else {
                    std::fs::create_dir_all(&target)
                };
                if let Some(mut state) = self.state_mut() {
                    state.message = Some((
                        match &result {
                            Ok(()) => format!("created folder {name}"),
                            Err(error) => format!("could not create folder: {error}"),
                        },
                        Instant::now(),
                    ));
                }
                if result.is_ok() {
                    self.refresh_project_after_disk_change();
                }
            }
            SidebarEditKind::Rename(path) => {
                if path.file_name().is_some_and(|current| current != name) {
                    self.move_item(
                        &path,
                        &target,
                        format!("renamed to {}", target.display()),
                        "rename failed",
                    );
                }
            }
        }
        self.request_redraw();
        self.pump();
    }

    /// A file just written to disk gets a tab and the keyboard.
    pub(super) fn open_created_file(&self, path: &Path) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            match Buffer::open(path) {
                // add, not push: the file may already be open.
                Ok(buffer) => {
                    state.docs.add(buffer);
                    // Typing goes to the new file, not back to the tree
                    // its name was typed in.
                    state.sidebar_keys = false;
                    uncover_column(&mut state);
                    let said = format!("created {}", state.name_for(path));
                    state.tell(Said::new(layout::Feedback::Success, said).detail(shown_path(path)));
                }
                Err(e) => state.say(
                    layout::Feedback::Failure,
                    format!("created but not opened: {e}"),
                ),
            }
            reveal_active_tab(&mut state);
        }
        self.refresh_project_after_disk_change();
        self.sync_title();
        self.reparse();
        self.lsp_sync_open();
    }

    /// Moves the project item at `path` to `destination`, by a rename in
    /// the Explorer or a drag, with its tabs and its local history. Says
    /// `done` when it has moved, or `failed` and why.
    pub(super) fn move_item(&self, path: &Path, destination: &Path, done: String, failed: &str) {
        let moved = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let State {
                docs,
                panes,
                history,
                ..
            } = &mut *state;
            let open = std::iter::once(docs).chain(panes.iter_mut().map(|p| &mut p.docs));
            let moved = move_project_item(open, history, path, destination);
            if moved.is_ok() {
                // The Explorer's selection stays on the item.
                state.tree.follow_move(path, destination);
            }
            state.message = Some((
                match &moved {
                    Ok(()) => done,
                    Err(error) => format!("{failed}: {error}"),
                },
                Instant::now(),
            ));
            moved.is_ok()
        };
        if moved {
            self.refresh_project_after_disk_change();
            self.sync_title();
            self.reparse();
        }
    }

    pub(super) fn new_file_with_panel(&self) -> bool {
        // No unsaved-changes prompt: the new file gets its own tab.
        let Some(start_dir) = self.state().map(|state| state.tree.target_dir()) else {
            return false;
        };

        let mtm = MainThreadMarker::from(self);
        let Some(panel) = save_panel(mtm) else {
            if let Some(mut state) = self.state_mut() {
                state.message = Some((NO_PANEL.into(), Instant::now()));
            }
            return true;
        };
        panel.setNameFieldStringValue(&NSString::from_str("untitled.txt"));
        if let Some(dir) = &start_dir {
            let url = NSURL::fileURLWithPath(&NSString::from_str(&dir.to_string_lossy()));
            panel.setDirectoryURL(Some(&url));
        }
        if panel.runModal() != MODAL_RESPONSE_OK {
            return false;
        }
        let Some(url) = panel.URL() else {
            return false;
        };
        let Some(path) = url.path() else {
            return false;
        };
        let path = std::path::PathBuf::from(path.to_string());

        if let Err(e) = std::fs::write(&path, "") {
            if let Some(mut state) = self.state_mut() {
                state.message = Some((format!("could not create: {e}"), Instant::now()));
            }
            return true;
        }

        self.open_created_file(&path);
        true
    }
}

/// An item moves from `source` to `destination` inside the project, and
/// what is kept by its path goes with it: the tabs open on it or under it
/// are re-keyed, and local history follows the file, or every file in the
/// folder. Nothing at `destination` is replaced, and a symbolic link moves
/// as the link.
fn move_project_item<'a>(
    open: impl Iterator<Item = &'a mut Documents>,
    history: &mut crate::history::recorder::Recorder,
    source: &Path,
    destination: &Path,
) -> std::io::Result<()> {
    // Open buffers use canonical identities. Capture the source key
    // before the move makes it impossible to canonicalise.
    let source_key = crate::platform::canonical(source);
    move_without_replace(source, destination)?;
    for docs in open {
        docs.rename_path(&source_key, destination);
    }
    let moved = crate::platform::canonical(destination);
    if moved.is_dir() {
        history.rename_folder(&source_key, &moved);
    } else {
        history.rename(&source_key, &moved);
    }
    Ok(())
}

/// Moves the item at `path` to the Trash, a symbolic link as the link, and
/// answers where it is there, for putting it back: the Trash renames what
/// would clash. `None` when the system does not say.
pub(super) fn trash_item(path: &Path) -> Result<Option<std::path::PathBuf>, String> {
    // Said outright: asked to find out, the URL follows a link to a folder.
    let folder = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
    let url =
        NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&path.to_string_lossy()), folder);
    let mut landed = None;
    NSFileManager::defaultManager()
        .trashItemAtURL_resultingItemURL_error(&url, Some(&mut landed))
        .map_err(|error| error.localizedDescription().to_string())?;
    Ok(landed
        .and_then(|url| url.path())
        .map(|path| std::path::PathBuf::from(path.to_string())))
}

#[cfg(test)]
mod tests {
    use super::move_project_item;
    use crate::history::recorder::{Recorder, Timing};
    use crate::history::store::{Reason, Store};
    use crate::project::TempTree;
    use crate::text::buffer::Buffer;
    use crate::text::documents::Documents;
    use crate::text::rope::Rope;
    use std::path::Path;
    use std::time::Duration;

    /// The texts kept for `path`, oldest first.
    fn kept(db: &Path, path: &Path) -> Vec<(Reason, String)> {
        let store = Store::open(db).unwrap();
        let mut list: Vec<(Reason, String)> = store
            .list(path)
            .unwrap()
            .iter()
            .map(|entry| (entry.reason, store.read(entry.id).unwrap().unwrap()))
            .collect();
        list.reverse();
        list
    }

    /// A drag in the Explorer used to move the file and its tab and leave
    /// its local history under the old path, where nothing finds it again.
    #[test]
    fn a_moved_item_takes_its_tabs_and_its_local_history() {
        let tree = TempTree::new(
            "move-item-history",
            &[
                ("project/notes.txt", "two\n"),
                ("project/docs/guide.md", "new\n"),
                ("project/docs-old/keep.md", "k\n"),
                ("project/archive/", ""),
            ],
        );
        let db = tree.0.join("history.db");
        let root = crate::platform::canonical(&tree.0.join("project"));
        let wait = Duration::from_secs(10);
        let mut history = Recorder::new(Some(db.clone()), true, "", Timing::default());

        // A file, open in a tab, with a version kept.
        let file = root.join("notes.txt");
        let target = root.join("archive/notes.txt");
        history.edited_on_disk(&file, &Rope::from_text("one\n"));
        let mut docs = Documents::new(Buffer::open(file.clone()).unwrap());
        move_project_item(std::iter::once(&mut docs), &mut history, &file, &target).unwrap();
        history.finish(std::iter::empty(), wait);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "two\n");
        assert!(!file.exists());
        assert_eq!(docs.active().path.as_deref(), Some(target.as_path()));
        assert_eq!(kept(&db, &file), []);
        assert_eq!(
            kept(&db, &target),
            [(Reason::EditedOnDisk, "one\n".to_string())]
        );

        // A folder: every file in it, and not its neighbour's.
        let folder = root.join("docs");
        let inside = folder.join("guide.md");
        let neighbour = root.join("docs-old/keep.md");
        history.edited_on_disk(&inside, &Rope::from_text("old\n"));
        history.edited_on_disk(&neighbour, &Rope::from_text("j\n"));
        let mut docs = Documents::new(Buffer::open(inside.clone()).unwrap());
        let moved = root.join("archive/docs");
        move_project_item(std::iter::once(&mut docs), &mut history, &folder, &moved).unwrap();
        history.finish(std::iter::empty(), wait);
        assert_eq!(
            docs.active().path.as_deref(),
            Some(moved.join("guide.md").as_path())
        );
        assert_eq!(kept(&db, &inside), []);
        assert_eq!(
            kept(&db, &moved.join("guide.md")),
            [(Reason::EditedOnDisk, "old\n".to_string())]
        );
        assert_eq!(
            kept(&db, &neighbour),
            [(Reason::EditedOnDisk, "j\n".to_string())]
        );

        // Nothing is replaced, and nothing follows a move that did not
        // happen.
        let taken = root.join("archive/taken.md");
        std::fs::write(&taken, "mine\n").unwrap();
        let error = move_project_item(std::iter::once(&mut docs), &mut history, &neighbour, &taken)
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        history.finish(std::iter::empty(), wait);
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "mine\n");
        assert_eq!(kept(&db, &taken), []);
        assert_eq!(kept(&db, &neighbour).len(), 1);
    }
}
