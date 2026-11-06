//! The project in the window: watching it for changes, and the sidebar's
//! new file, new folder and rename fields.

use super::explorer::Did;
use super::*;
use crate::project::fileops::Op;
use crate::project::naming::{self, Problem};

/// What the tree's name field knows about the name in it.
#[derive(Default)]
pub(super) struct NameCheck {
    /// The text the verdict is for; `None` before the first, and to have
    /// the next one asked whatever the text.
    text: Option<String>,
    problem: Option<Problem>,
    /// Whether the verdict is said under the field: once the text has
    /// been edited, or Return refused. A field just opened says nothing
    /// about the name it starts with.
    said: bool,
}

impl SidebarEdit {
    /// Brings the verdict up to date with the field's text, and answers
    /// the text. The folder is looked at once for each text the field
    /// comes to hold, however many frames draw it: the same text again
    /// asks nothing.
    pub(super) fn check_name(&mut self) -> &str {
        let text = self.field.rope.to_string();
        if self.check.text.as_deref() != Some(text.as_str()) {
            let ask = naming::Ask {
                parent: &self.parent,
                nested: !self.replaces(),
                renaming: match &self.kind {
                    SidebarEditKind::Rename(path) => Some(path),
                    _ => None,
                },
            };
            self.check.problem = naming::check(&text, &ask);
            self.check.said |= self.check.text.is_some();
            self.check.text = Some(text);
        }
        self.name()
    }

    /// The text the verdict is for: the field's, as of the last
    /// [`SidebarEdit::check_name`].
    pub(super) fn name(&self) -> &str {
        self.check.text.as_deref().unwrap_or_default()
    }

    /// Whether the name would be refused, asked of the folder as it is
    /// now rather than as it was when the text last changed: for Return
    /// and for a click elsewhere, not for a frame.
    fn refuses_now(&mut self) -> bool {
        let said = self.check.said;
        self.check.text = None;
        self.check_name();
        self.check.said = said;
        self.check.problem.is_some_and(Problem::blocks)
    }

    /// What is wrong with the name, or odd about it, once that is said.
    pub(super) fn problem(&self) -> Option<Problem> {
        self.check.problem.filter(|_| self.check.said)
    }

    /// The problem as the field shows it under itself.
    pub(super) fn note(&self) -> Option<layout::FieldNote<'static>> {
        let problem = self.problem()?;
        Some(layout::FieldNote {
            text: problem.said(),
            tone: if problem.blocks() {
                layout::NoteTone::Error
            } else {
                layout::NoteTone::Warning
            },
        })
    }
}

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
        // A copy that is running lands its items one by one: the tree is
        // read again once, when it is over.
        if at.elapsed() < SETTLE || state.watch.index_rx.is_some() || !state.explorer.idle() {
            return;
        }
        state.watch.tree_changed_at = None;
        super::review::note_unhooked_writes(&mut state);
        if let Some(indexer) = &state.watch.indexer {
            indexer.poke();
        }
        start_project_refresh(&mut state);
        start_home_summary(&mut state);
        // The status is on show with Source Control closed too: the marks
        // on the Explorer's rows and the count on the activity bar. A file
        // another program changed gets its letter once the watcher has
        // settled, not when the panel is next opened: one status on a
        // worker for each settled batch, and one waiting at most.
        state.git.refresh();
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
        let mut edit = SidebarEdit {
            kind,
            parent,
            row,
            depth,
            field,
            check: NameCheck::default(),
        };
        // The name the field starts with is where its edits are counted
        // from: nothing is said about it until it changes.
        edit.check_name();
        state.sidebar_edit = Some(edit);
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
            key::RETURN => self.submit_sidebar_edit(),
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

    /// Return in the name field: the name is taken, unless something is
    /// wrong with it. Then the field stays open and says what, and Return
    /// has done nothing.
    pub(super) fn submit_sidebar_edit(&self) {
        let refused = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(edit) = state.sidebar_edit.as_mut() else {
                return;
            };
            let refused = edit.refuses_now();
            // Said from here on, even about a name not yet edited: an
            // empty field just opened.
            edit.check.said |= refused;
            refused
        };
        if refused {
            self.request_redraw();
            self.pump();
        } else {
            self.finish_sidebar_edit(true);
        }
    }

    /// Ends the inline field: creates, renames, or does nothing. A name
    /// that will not do is dropped without a word: this is also where a
    /// click elsewhere ends up, and that is no time to fail.
    pub(super) fn finish_sidebar_edit(&self, commit: bool) {
        let Some(mut edit) = self
            .state_mut()
            .and_then(|mut state| state.sidebar_edit.take())
        else {
            return;
        };
        if !commit || edit.refuses_now() {
            self.request_redraw();
            return;
        }
        // As typed: spaces at either end are the name's, and the field
        // has said so.
        let name = edit.check_name().to_owned();
        let name = name.as_str();
        let relative = Path::new(name);
        let target = edit.parent.join(relative);
        match edit.kind {
            SidebarEditKind::NewFile => {
                if target.exists() {
                    if let Some(mut state) = self.state_mut() {
                        state.message = Some((format!("{name} already exists"), Instant::now()));
                    }
                } else {
                    // Never truncates: a file that appeared since the
                    // check above is an error, not an empty file.
                    match self.file_op(Op::NewFile(&target)) {
                        Ok(_) => self.open_created_file(&target),
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
                let result = self.file_op(Op::NewFolder(&target));
                if let Some(mut state) = self.state_mut() {
                    state.message = Some((
                        match &result {
                            Ok(_) => format!("created folder {name}"),
                            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                                "could not create folder: already exists".to_string()
                            }
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
                        Op::Rename {
                            from: &path,
                            to: &target,
                        },
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

    /// Renames or moves a project item as `op` says, by a rename in the
    /// Explorer, with its tabs and its local history. Says `done` when it
    /// has moved, or `failed` and why.
    pub(super) fn move_item(&self, op: Op<'_>, done: String, failed: &str) {
        let step = self.begin_file_step();
        let moved = self.relocate(op, false, step);
        if let Some(mut state) = self.state_mut() {
            state.message = Some((
                match &moved {
                    Ok(Did {
                        original_left: Some(why),
                        ..
                    }) => format!("{done}; the original could not be removed: {why}"),
                    Ok(_) => done,
                    Err(error) => format!("{failed}: {error}"),
                },
                Instant::now(),
            ));
        }
        if moved.is_ok() {
            self.refresh_project_after_disk_change();
            self.sync_title();
            self.reparse();
        }
    }

    /// The one path for an item that moves inside the project, `op` a
    /// rename or a move: the item on disk, the tabs open on it in every
    /// pane, its local history and the Explorer's selection. Neither says
    /// anything nor reads the tree again: the caller does both, once.
    /// With `free`, a destination taken since it was chosen gives way to
    /// the next free name. What was done is kept as one of the operations
    /// of `step`. Answers where the item is now.
    pub(super) fn relocate(&self, op: Op<'_>, free: bool, step: u64) -> std::io::Result<Did> {
        let Some(mut state) = self.state_mut() else {
            return Err(std::io::Error::other("the window is busy"));
        };
        let State {
            docs,
            panes,
            history,
            explorer,
            tree,
            ..
        } = &mut *state;
        let open = std::iter::once(docs).chain(panes.iter_mut().map(|p| &mut p.docs));
        let did = move_project_item(open, history, explorer, step, op, free)?;
        if let (Op::Rename { from, .. } | Op::Move { from, .. }, Some(to)) = (op, &did.path) {
            // The Explorer's selection stays on the item.
            tree.follow_move(from, to);
        }
        Ok(did)
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

        let created = match self.file_op(Op::NewFile(&path)) {
            Ok(_) => Ok(()),
            // The panel asked, and the answer was to replace it.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                std::fs::write(&path, "")
            }
            Err(error) => Err(error),
        };
        if let Err(e) = created {
            if let Some(mut state) = self.state_mut() {
                state.message = Some((format!("could not create: {e}"), Instant::now()));
            }
            return true;
        }

        self.open_created_file(&path);
        true
    }
}

/// An item is renamed or moved inside the project, as `op` says, and
/// what is kept by its path goes with it: the tabs open on it or under it
/// are re-keyed, and local history follows the file, or every file in the
/// folder. Nothing at the destination is replaced, and a symbolic link
/// moves as the link. What was done is kept in `files`, as one of the
/// operations of `step`; with `free`, a destination taken since it was
/// chosen gives way to the next free name.
fn move_project_item<'a>(
    open: impl Iterator<Item = &'a mut Documents>,
    history: &mut crate::history::recorder::Recorder,
    files: &mut explorer::Files,
    step: u64,
    op: Op<'_>,
    free: bool,
) -> std::io::Result<Did> {
    let (Op::Rename { from: source, .. } | Op::Move { from: source, .. }) = op else {
        return Err(std::io::ErrorKind::InvalidInput.into());
    };
    // Open buffers use canonical identities. Capture the source key
    // before the move makes it impossible to canonicalise.
    let source_key = crate::platform::canonical(source);
    let did = explorer::file_op(files, step, op, free)?;
    let Some(destination) = did.path.as_deref() else {
        return Ok(did);
    };
    follow_moved(open, history, &source_key, destination);
    Ok(did)
}

/// What is kept by an item's path goes with the item that has moved to
/// `destination` from `source_key`, its canonical path while it was still
/// there: the tabs open on it or under it, and its local history. An undo
/// that moves an item back comes here too.
pub(super) fn follow_moved<'a>(
    open: impl Iterator<Item = &'a mut Documents>,
    history: &mut crate::history::recorder::Recorder,
    source_key: &Path,
    destination: &Path,
) {
    for docs in open {
        docs.rename_path(source_key, destination);
    }
    let moved = crate::platform::canonical(destination);
    if moved.is_dir() {
        history.rename_folder(source_key, &moved);
    } else {
        history.rename(source_key, &moved);
    }
}

/// Moves the item at `path` to the Trash, a symbolic link as the link, and
/// answers where it is there, for putting it back: the Trash renames what
/// would clash. `None` when the system does not say.
pub(super) fn trash_item(path: &Path) -> Result<Option<std::path::PathBuf>, String> {
    // Said outright: asked to find out, the URL follows a link to a folder.
    let folder = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
    if std::env::var_os("CRC_SELFTEST").is_some() {
        return scripted_trash(path, folder);
    }
    // In a pool of its own: a copy that replaces an item asks from its
    // worker, which has none.
    objc2::rc::autoreleasepool(|_| {
        let url = NSURL::fileURLWithPath_isDirectory(
            &NSString::from_str(&path.to_string_lossy()),
            folder,
        );
        let mut landed = None;
        NSFileManager::defaultManager()
            .trashItemAtURL_resultingItemURL_error(&url, Some(&mut landed))
            .map_err(|error| error.localizedDescription().to_string())?;
        Ok(landed
            .and_then(|url| url.path())
            .map(|path| std::path::PathBuf::from(path.to_string())))
    })
}

/// A test instance's Trash: the folder `CRC_SELFTEST_TRASH` names, where
/// the item takes the next free name as the Trash renames what would
/// clash. With no such folder nothing is moved: no test instance reaches
/// the real Trash.
fn scripted_trash(path: &Path, folder: bool) -> Result<Option<std::path::PathBuf>, String> {
    use crate::project::fileops;
    let (Some(trash), Some(name)) = (
        std::env::var_os("CRC_SELFTEST_TRASH").map(std::path::PathBuf::from),
        path.file_name(),
    ) else {
        return Err("a test instance has no Trash".into());
    };
    std::fs::create_dir_all(&trash).map_err(|error| error.to_string())?;
    let landed = trash.join(fileops::free_name(&trash, name, folder));
    fileops::move_item(path, &landed)
        .map(|moved| Some(moved.path))
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::move_project_item;
    use crate::history::recorder::{Recorder, Timing};
    use crate::history::store::{Reason, Store};
    use crate::platform::window::explorer::Files;
    use crate::project::TempTree;
    use crate::project::fileops::Op;
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
        let mut files = Files::default();
        let step = files.begin();

        // A file, open in a tab, with a version kept.
        let file = root.join("notes.txt");
        let target = root.join("archive/notes.txt");
        history.edited_on_disk(&file, &Rope::from_text("one\n"));
        let mut docs = Documents::new(Buffer::open(file.clone()).unwrap());
        move_project_item(
            std::iter::once(&mut docs),
            &mut history,
            &mut files,
            step,
            Op::Move {
                from: &file,
                to: &target,
            },
            false,
        )
        .unwrap();
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
        move_project_item(
            std::iter::once(&mut docs),
            &mut history,
            &mut files,
            step,
            Op::Rename {
                from: &folder,
                to: &moved,
            },
            false,
        )
        .unwrap();
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
        let clash = Op::Move {
            from: &neighbour,
            to: &taken,
        };
        let error = move_project_item(
            std::iter::once(&mut docs),
            &mut history,
            &mut files,
            step,
            clash,
            false,
        )
        .err()
        .expect("the name is taken");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "mine\n");

        // A paste's move takes the next free name instead, and the tab
        // and the history follow the item to where it really went.
        let mut docs = Documents::new(Buffer::open(neighbour.clone()).unwrap());
        let did = move_project_item(
            std::iter::once(&mut docs),
            &mut history,
            &mut files,
            step,
            clash,
            true,
        )
        .unwrap();
        let landed = root.join("archive/taken-copy.md");
        assert_eq!(did.path.as_deref(), Some(landed.as_path()));
        history.finish(std::iter::empty(), wait);
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "mine\n");
        assert_eq!(std::fs::read_to_string(&landed).unwrap(), "k\n");
        assert_eq!(docs.active().path.as_deref(), Some(landed.as_path()));
        assert_eq!(kept(&db, &taken), []);
        assert_eq!(kept(&db, &neighbour), []);
        assert_eq!(kept(&db, &landed).len(), 1);
    }
}

/// The name field's verdict, as Return and the frame ask for it.
#[cfg(test)]
mod name_field_tests {
    use super::*;
    use crate::project::TempTree;

    fn notebook(name: &str) -> TempTree {
        TempTree::new(
            name,
            &[
                ("bread.md", "flour\n"),
                ("soup.md", "leeks\n"),
                ("plots/", ""),
            ],
        )
    }

    /// The field as `start_sidebar_edit` opens it, on `text`.
    fn field(kind: SidebarEditKind, parent: &Path, text: &str) -> SidebarEdit {
        let mut edit = SidebarEdit {
            kind,
            parent: parent.to_path_buf(),
            row: 0,
            depth: 0,
            field: Buffer::from_text(text),
            check: NameCheck::default(),
        };
        edit.check_name();
        edit
    }

    /// The text typed over, and a frame drawn.
    fn retype(edit: &mut SidebarEdit, text: &str) {
        edit.field = Buffer::from_text(text);
        edit.check_name();
    }

    #[test]
    fn return_is_refused_while_the_name_will_not_do() {
        let files = notebook("name-field-refused");
        let bread = files.0.join("bread.md");
        let mut edit = field(SidebarEditKind::Rename(bread), &files.0, "bread.md");
        // The name it has: nothing to say, and Return closes the field.
        assert_eq!(edit.note(), None);
        assert!(!edit.refuses_now());

        retype(&mut edit, "soup.md");
        assert_eq!(edit.problem(), Some(Problem::Taken));
        assert_eq!(
            edit.note(),
            Some(layout::FieldNote {
                text: "This name is already taken",
                tone: layout::NoteTone::Error,
            })
        );
        // Asked again, as often as Return is pressed, the answer holds
        // and the field keeps its text.
        for _ in 0..3 {
            assert!(edit.refuses_now());
        }
        assert_eq!(edit.name(), "soup.md");
        assert_eq!(edit.problem(), Some(Problem::Taken));

        // Put right, Return goes through.
        retype(&mut edit, "stew.md");
        assert_eq!(edit.note(), None);
        assert!(!edit.refuses_now());

        // A slash is a path for a new item and no name for a rename.
        retype(&mut edit, "plots/stew.md");
        assert_eq!(edit.problem(), Some(Problem::Slash));
        assert!(edit.refuses_now());

        // Spaces at an end are said, in the other colour, and allowed.
        retype(&mut edit, " stew.md");
        assert_eq!(
            edit.note().map(|note| note.tone),
            Some(layout::NoteTone::Warning)
        );
        assert!(!edit.refuses_now());
        assert_eq!(edit.name(), " stew.md", "the name is taken as typed");
    }

    #[test]
    fn a_change_of_case_alone_is_not_in_its_own_way() {
        let files = notebook("name-field-case");
        let bread = files.0.join("bread.md");
        let mut edit = field(SidebarEditKind::Rename(bread), &files.0, "bread.md");
        retype(&mut edit, "Bread.md");
        assert_eq!(edit.problem(), None);
        assert!(!edit.refuses_now());
        retype(&mut edit, "BREAD.MD");
        assert_eq!(edit.problem(), None);
        assert!(!edit.refuses_now());
    }

    /// An empty field just opened says nothing: Return is refused all the
    /// same, and from then on the field says why.
    #[test]
    fn a_new_name_is_required_but_not_said_until_asked_for() {
        let files = notebook("name-field-empty");
        for kind in [SidebarEditKind::NewFile, SidebarEditKind::NewFolder] {
            let mut edit = field(kind, &files.0, "");
            assert_eq!(edit.note(), None, "nothing typed yet, nothing said");
            let refused = edit.refuses_now();
            assert!(refused);
            assert_eq!(edit.note(), None, "asking does not say it");
            // What `submit_sidebar_edit` does with a refusal.
            edit.check.said |= refused;
            assert_eq!(edit.problem(), Some(Problem::Empty));

            // Typed and deleted again, it is said without Return.
            let mut edit = field(SidebarEditKind::NewFile, &files.0, "");
            retype(&mut edit, "s");
            assert_eq!(edit.note(), None);
            retype(&mut edit, "");
            assert_eq!(edit.problem(), Some(Problem::Empty));
            // A new item takes a path, and each part is checked.
            retype(&mut edit, "plots/2027/beds.md");
            assert_eq!(edit.problem(), None);
            assert!(!edit.refuses_now());
            retype(&mut edit, "plots/../beds.md");
            assert_eq!(edit.problem(), Some(Problem::Reserved));
            retype(&mut edit, "soup.md/beds.md");
            assert_eq!(edit.problem(), Some(Problem::Blocked));
            // A colon is the name's to have; a path that ends in a slash
            // names nothing.
            retype(&mut edit, "notes:today.md");
            assert_eq!(edit.note(), None);
            assert!(!edit.refuses_now());
            retype(&mut edit, "plots/");
            assert_eq!(edit.problem(), Some(Problem::EmptyPart));
            assert!(edit.refuses_now());
        }
    }

    /// The verdict is for the text: frames drawn with the same text ask
    /// the folder nothing, so what changes on disk meanwhile is seen at
    /// the next edit, or when Return asks.
    #[test]
    fn the_folder_is_asked_when_the_text_changes_not_every_frame() {
        let files = notebook("name-field-once");
        let mut edit = field(SidebarEditKind::NewFile, &files.0, "");
        retype(&mut edit, "stew.md");
        assert_eq!(edit.problem(), None);
        // Something else makes the file.
        std::fs::write(files.0.join("stew.md"), "").unwrap();
        for _ in 0..5 {
            edit.check_name();
        }
        assert_eq!(edit.problem(), None, "the same text asked nothing");
        // Return asks the folder as it is now.
        assert!(edit.refuses_now());
        assert_eq!(edit.problem(), Some(Problem::Taken));
        // And so does the next edit.
        std::fs::remove_file(files.0.join("stew.md")).unwrap();
        edit.check_name();
        assert_eq!(edit.problem(), Some(Problem::Taken), "still the same text");
        retype(&mut edit, "stew.m");
        retype(&mut edit, "stew.md");
        assert_eq!(edit.problem(), None);
    }
}
