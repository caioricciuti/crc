//! The sidebar: the file tree and its drags, the Source Control view's
//! clicks and keys, and opening a folder with its project scan.

use super::*;
use crate::project::fileops::Op;

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
        // Unsaved work under the item would be stranded at a path that no
        // longer exists, the same guard Move to Trash uses. Tabs are keyed
        // by canonical paths.
        let source_key = crate::platform::canonical(&drag.path);
        if self
            .state()
            .is_some_and(|state| dirty_tabs_under(&state, &source_key))
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
        self.move_item(
            Op::Move {
                from: &drag.path,
                to: &target,
            },
            format!(
                "moved {} to {}",
                name.to_string_lossy(),
                destination.display()
            ),
            "move failed",
        );
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
            state.mcp.open = false;
            state.search_open = false;
            state.search_focus = None;
            state.git_open = scm;
            // Typing belongs to the editor until the message field is asked
            // for. Focusing it here would swallow the next keystroke.
            state.git_focus = false;
            if scm {
                state.palette = None;
                state.goto = None;
                let labels = super::review::agent_labels(&state);
                state.git.set_agent_labels(labels);
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
        use crate::platform::git_panel::{Entry, Group};
        let Some(mut state) = self.state_mut() else {
            return false;
        };
        let g = state.git.sidebar(column);
        let mut handled = true;
        let mut conflict_file = None;
        let mut open_file = None;
        if g.repo.contains(x, y) && state.git.repo.is_some() {
            drop(state);
            self.open_repo_picker();
            return true;
        } else if g.more.contains(x, y) {
            drop(state);
            self.pop_up_git_menu(g.more);
            return true;
        } else if g.refresh.contains(x, y) {
            state.git.refresh();
        } else if state.git.snapshot.is_none() {
            // No repository: nothing below the header is a control.
            handled = false;
        } else if g.branch.contains(x, y) && !state.git.branch().is_empty() {
            drop(state);
            self.open_branch_picker(BranchIntent::Switch);
            return true;
        } else if g.pull.contains(x, y) || g.push.contains(x, y) {
            let what = if g.pull.contains(x, y) {
                crate::project::git::Remote::Pull
            } else {
                crate::project::git::Remote::Push
            };
            if state.git.busy() {
                return true;
            }
            drop(state);
            self.git_remote(what);
            return true;
        } else if g.commit.contains(x, y) {
            state.git.commit();
        } else if g.amend.contains(x, y) {
            let on = !state.git.amend;
            state.git.set_amend(on);
        } else if g.message.contains(x, y) {
            state.git_focus = true;
            let State { git, renderer, .. } = &mut *state;
            let at = git.message_byte_at(&mut renderer.atlas, g, x, y);
            git.message.place_cursor(at, Motion::Move);
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
                    // The row's controls are at its trailing edge, so a click
                    // there acts rather than selects.
                    if g.toggle(rect).contains(x, y) {
                        state.git.stage_index(change, !staged);
                    } else if g.open(rect).contains(x, y) {
                        state.git_focus = false;
                        open_file = change_path(&state, change);
                    } else {
                        state.git_focus = false;
                        state.git.select(change);
                        open_diff_tab(&mut state, change, staged);
                    }
                }
                Entry::Section { group, .. } => {
                    if group != Group::Conflicts && g.section_action(rect).contains(x, y) {
                        state.git.stage_group(group);
                    } else {
                        state.git.toggle_group(group);
                    }
                }
                // An agent's file opens as the change Git lists it under,
                // the working tree's unless only the index has it.
                Entry::Agent { index } => {
                    state.git_focus = false;
                    let agent_path = state.git.agents.get(index).map(|a| a.file.path.clone());
                    if g.open(rect).contains(x, y) {
                        open_file = agent_path;
                    } else if let Some(change) = state.git.agent_change(index) {
                        let staged = state
                            .git
                            .snapshot
                            .as_ref()
                            .and_then(|s| s.changes.get(change))
                            .is_some_and(|c| !c.unstaged());
                        state.git.select(change);
                        open_diff_tab(&mut state, change, staged);
                    } else {
                        // Kept, committed, or put back by hand: Git has
                        // nothing to show, so the file itself opens.
                        let name = state
                            .git
                            .agents
                            .get(index)
                            .map_or_else(String::new, |a| a.name());
                        state.say(
                            layout::Feedback::Info,
                            format!("{name}: no difference from HEAD"),
                        );
                        open_file = agent_path;
                    }
                }
                Entry::Agents { .. } => state.git.toggle_agents(),
            }
        } else {
            handled = false;
        }
        drop(state);
        if let Some(path) = open_file {
            self.load_path(&path.to_string_lossy());
        }
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
        // The diff has the column and the field does not have the keyboard:
        // Space stages the hunk or the selected lines, Up and Down move the
        // cursor, with Shift selecting. In a document Space types as ever.
        let code = event.keyCode();
        if matches!(code, key::SPACE | key::UP | key::DOWN)
            && !flags.contains(NSEventModifierFlags::Option)
            && !flags.contains(NSEventModifierFlags::Control)
            && self
                .state()
                .is_some_and(|state| !state.git_focus && column_of(&state) == Column::GitDiff)
        {
            let visible = (self.chrome().text.height / crate::platform::git_panel::DIFF_LINE)
                .max(1.0) as usize;
            let Some(mut state) = self.state_mut() else {
                return false;
            };
            let extend = flags.contains(NSEventModifierFlags::Shift);
            match code {
                key::SPACE => state.git.stage_selection(),
                key::UP => state.git.move_diff_cursor(-1, extend, visible),
                _ => state.git.move_diff_cursor(1, extend, visible),
            }
            drop(state);
            self.request_redraw();
            self.resume_display_link();
            return true;
        }
        if !self.state().is_some_and(|state| state.git_focus) {
            return false;
        }
        let Some(mut state) = self.state_mut() else {
            return false;
        };
        match event.keyCode() {
            // Tab takes a commit type the field is offering, typed in as
            // if by hand; without one it does nothing.
            key::TAB => {
                let hint = state.git.commit_type_hint();
                drop(state);
                if let Some(rest) = hint {
                    self.commit_text(rest, NSRange::new(NSNotFound as usize, 0));
                    self.resume_display_link();
                }
                return true;
            }
            // Return and Shift-Return break the line, typed in like any
            // other text; Cmd-Return committed above.
            key::RETURN => {
                drop(state);
                self.commit_text("\n", NSRange::new(NSNotFound as usize, 0));
                self.resume_display_link();
                return true;
            }
            key::UP | key::DOWN
                if !flags.contains(NSEventModifierFlags::Option)
                    && !flags.contains(NSEventModifierFlags::Control) =>
            {
                let motion = if flags.contains(NSEventModifierFlags::Shift) {
                    Motion::Extend
                } else {
                    Motion::Move
                };
                if code == key::UP {
                    state.git.message.move_up(motion);
                } else {
                    state.git.message.move_down(motion);
                }
            }
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

    /// A press in the Explorer. The row under the pointer is selected:
    /// alone, added to the selection or taken out of it with Command, or as
    /// the end of a range with Shift. A plain press opens a file or toggles
    /// a folder at once, except on one of several selected rows, where it
    /// waits for the release: a drag from there has to find them all still
    /// selected. The tree takes the keyboard, unless the press opened a
    /// file, which has it then.
    pub(super) fn tree_press(&self, event: &NSEvent, x: f32, y: f32, rect: Viewport) {
        // No folder: the panel is its empty state, whose one button opens one.
        let on_open = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.tree.root().is_none()
                && layout::sidebar_empty_button(&mut state.renderer.atlas, rect).is_some_and(
                    |button| {
                        y >= button.y
                            && y < button.y + button.height
                            && layout::pointer().is_none_or(|(px, _)| {
                                px >= button.x && px < button.x + button.width
                            })
                    },
                )
        };
        if on_open {
            self.open_folder();
            self.request_redraw();
            return;
        }
        let flags = event.modifierFlags();
        let command = flags.contains(NSEventModifierFlags::Command);
        let shift = flags.contains(NSEventModifierFlags::Shift);
        let act = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if state.tree.root().is_none() {
                return;
            }
            let row = layout::sidebar_row_at(&state.tree, sidebar_field(&state), rect, y);
            take_tree_keys(&mut state);
            match row {
                // Below the last row: nothing is selected, and New File is
                // for the project root. The row of an open name field is
                // not a row either, and a press there changes nothing.
                None => {
                    if state.sidebar_edit.is_none() {
                        state.tree.clear_selection();
                    }
                    None
                }
                Some(index) if shift => {
                    state.tree.select_to(index);
                    None
                }
                Some(index) if command => {
                    state.tree.toggle_selected(index);
                    None
                }
                Some(index) => {
                    // Remember what is under the pointer in case this turns
                    // into a drag.
                    let deferred = state.tree.selection_len() > 1 && state.tree.is_selected(index);
                    state.drag = state.tree.rows().get(index).map(|entry| {
                        Drag::Tree(TreeDrag {
                            path: entry.path.clone(),
                            origin: (x, y),
                            active: false,
                            over: None,
                            valid: false,
                            deferred,
                        })
                    });
                    (!deferred).then_some(index)
                }
            }
        };
        match act {
            // A file opened by a click has the keyboard: what is typed next
            // is for it, and Cmd-Delete edits its line.
            Some(index) => self.activate_tree_row(index, true),
            None => {
                self.request_redraw();
                self.pump();
            }
        }
    }

    /// The release of a press in the tree that came to something: a drag
    /// dropped where it may be, or the click a press on one of several
    /// selected rows was waiting to be.
    pub(super) fn tree_release(&self, drag: TreeDrag) {
        if drag.active {
            self.drop_tree_item(drag);
            return;
        }
        let index = self
            .state()
            .and_then(|state| state.tree.rows().iter().position(|e| e.path == drag.path));
        if let Some(index) = index {
            self.activate_tree_row(index, true);
        }
    }

    /// Focus Explorer: shows the tree when the sidebar is hidden or on
    /// another view, gives it the keyboard, and puts the cursor on the
    /// active document's row, or on the first row when it has none.
    pub(super) fn focus_explorer(&self) {
        let Some(elsewhere) = self.state().and_then(|state| {
            (state.tree.root().is_some() && !state.agents.on).then(|| {
                state.git_open || state.search_open || state.mcp.open || state.extensions.is_some()
            })
        }) else {
            return;
        };
        if elsewhere {
            self.activate(0);
        }
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.sidebar = true;
            close_fields(&mut state);
            take_tree_keys(&mut state);
            state.terminal.focus = false;
            let key = self.key_window();
            terminal::report_focus(&mut state, key);
            let row = state
                .docs
                .active()
                .path
                .as_deref()
                .and_then(|open| tree_path_of(&state.tree, open));
            if let Some(path) = row {
                state.tree.reveal(&path);
            }
            if state.tree.selected_path().is_none() {
                state.tree.select(0);
            }
            let rows = chrome_of(&state).sidebar.map_or(0, layout::sidebar_rows);
            state.tree.scroll_to_selection(rows);
            // A scan under way read the tree before any folder opened here.
            state.tree_version += 1;
        }
        self.request_redraw();
        self.resume_display_link();
        self.pump();
    }

    /// Selects the tree row at `index` alone and acts on it: a folder opens
    /// or closes, a file opens in a tab. The file has the keyboard when
    /// `to_editor` (a click, Cmd-Down); otherwise the tree keeps it (Space).
    pub(super) fn activate_tree_row(&self, index: usize, to_editor: bool) {
        let file = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(entry) = state.tree.select(index) else {
                return;
            };
            let (path, is_dir) = (entry.path.clone(), entry.is_dir);
            if is_dir {
                // Single click toggles a folder: a tree where you have to
                // double-click to see inside is needlessly slow to browse.
                set_tree_folder(&mut state, index, None);
                None
            } else {
                Some(path)
            }
        };
        match file {
            None => self.resume_display_link(),
            Some(path) => {
                // Opening a file now adds a tab rather than replacing what is
                // showing, so nothing has to be saved or discarded first.
                self.load_path(&path.to_string_lossy());
                if let Some(mut state) = self.state_mut() {
                    // Not a find bar left open, whoever has the keys now.
                    if let Some(bar) = &mut state.find {
                        bar.has_keys = false;
                    }
                    state.sidebar_keys = !to_editor;
                }
                self.sync_title();
                self.reparse();
            }
        }
        self.request_redraw();
        self.pump();
    }

    /// Keys while the Explorer's tree has the keyboard. Every key without
    /// Command is the tree's then, used or not, so none of them reaches the
    /// document behind it. Whether the key was taken.
    pub(super) fn handle_tree_key(&self, event: &NSEvent) -> bool {
        const ESCAPE: u16 = 53;
        const A: u16 = 0;
        if !self.state().is_some_and(|state| tree_has_keys(&state)) {
            return false;
        }
        let flags = event.modifierFlags();
        let command = flags.contains(NSEventModifierFlags::Command);
        let shift = flags.contains(NSEventModifierFlags::Shift);
        let code = match event.keyCode() {
            key::KEYPAD_ENTER => key::RETURN,
            code => code,
        };
        let page = self.chrome().sidebar.map_or(1, layout::sidebar_rows).max(1);
        // What needs the state let go of before it can be done.
        enum Then {
            Nothing,
            Activate(usize, bool),
            Rename(std::path::PathBuf),
            Trash,
        }
        let then = {
            let Some(mut state) = self.state_mut() else {
                return false;
            };
            let cursor = state.tree.cursor_row();
            let entry = cursor
                .and_then(|row| state.tree.rows().get(row))
                .map(|entry| (entry.is_dir, entry.expanded));
            let then = match code {
                // Command keys are the menu's, apart from the two that open
                // and trash, which arrive here when no menu took them.
                key::DOWN if command => match cursor {
                    Some(row) => Then::Activate(row, true),
                    None => Then::Nothing,
                },
                key::DELETE if command => Then::Trash,
                A if command => {
                    state.tree.select_all();
                    Then::Nothing
                }
                // The other Command keys that move or delete in a document
                // stop here and do nothing; the rest are the menu's.
                key::UP
                | key::LEFT
                | key::RIGHT
                | key::HOME
                | key::END
                | key::PAGE_UP
                | key::PAGE_DOWN
                | key::FORWARD_DELETE
                    if command =>
                {
                    Then::Nothing
                }
                _ if command => return false,
                // A copy that is running stops, or a cut that is waiting is
                // called off. With neither, several selected become the
                // cursor's row alone; from one, the keys go back to the
                // document.
                ESCAPE => {
                    if !explorer::escape(&mut state) && !state.tree.reduce_selection() {
                        state.sidebar_keys = false;
                    }
                    Then::Nothing
                }
                key::UP => {
                    state.tree.step_cursor(-1, shift);
                    Then::Nothing
                }
                key::DOWN => {
                    state.tree.step_cursor(1, shift);
                    Then::Nothing
                }
                key::PAGE_UP => {
                    state.tree.step_cursor(-(page as isize), shift);
                    Then::Nothing
                }
                key::PAGE_DOWN => {
                    state.tree.step_cursor(page as isize, shift);
                    Then::Nothing
                }
                key::HOME => {
                    state.tree.move_cursor(0, shift);
                    Then::Nothing
                }
                key::END => {
                    let last = state.tree.len().saturating_sub(1);
                    state.tree.move_cursor(last, shift);
                    Then::Nothing
                }
                // Left closes an open folder; from anything else it goes to
                // the folder it is in.
                key::LEFT => {
                    if let Some(row) = cursor {
                        if entry == Some((true, true)) {
                            state.tree.select(row);
                            set_tree_folder(&mut state, row, Some(false));
                        } else if let Some(parent) = state.tree.parent_row(row) {
                            state.tree.select(parent);
                        }
                    }
                    Then::Nothing
                }
                // Right opens a closed folder; in an open one it goes to
                // the first item inside.
                key::RIGHT => {
                    if let Some(row) = cursor {
                        if entry == Some((true, false)) {
                            state.tree.select(row);
                            set_tree_folder(&mut state, row, Some(true));
                        } else if let Some(child) = state.tree.first_child_row(row) {
                            state.tree.select(child);
                        }
                    }
                    Then::Nothing
                }
                // The name of the one selected item, in place.
                key::RETURN | key::F2 => match state.tree.selected_path() {
                    Some(path) if state.tree.selection_len() == 1 => Then::Rename(path),
                    _ => Then::Nothing,
                },
                // Space opens what Cmd-Down opens and stays in the tree,
                // unless a name is being typed, which it is then part of.
                key::SPACE if !state.tree.typing_ahead(Instant::now()) => match cursor {
                    Some(row) => Then::Activate(row, false),
                    None => Then::Nothing,
                },
                _ => {
                    let typed = event
                        .characters()
                        .map(|characters| typed_text(&characters.to_string()))
                        .unwrap_or_default();
                    let mut letters = typed.chars();
                    if let (Some(letter), None) = (letters.next(), letters.next())
                        && !flags.contains(NSEventModifierFlags::Control)
                        && !flags.contains(NSEventModifierFlags::Option)
                    {
                        state.tree.type_ahead(letter, Instant::now());
                    }
                    Then::Nothing
                }
            };
            state.tree.scroll_to_selection(page);
            then
        };
        match then {
            Then::Nothing => {}
            Then::Activate(row, to_editor) => self.activate_tree_row(row, to_editor),
            Then::Rename(path) => self.start_sidebar_edit(SidebarEditKind::Rename(path)),
            Then::Trash => self.trash_selection(),
        }
        self.request_redraw();
        self.resume_display_link();
        true
    }

    /// Move to Trash for everything selected in the Explorer, a folder
    /// taking what is in it: one question that says how many, and nothing
    /// moved while a tab on any of them holds unsaved work, in any pane.
    pub(super) fn trash_selection(&self) {
        let Some(paths) = self.state().map(|state| {
            if tree_has_keys(&state) {
                state.tree.selected_roots()
            } else {
                Vec::new()
            }
        }) else {
            return;
        };
        if paths.is_empty() {
            return;
        }
        let keys: Vec<std::path::PathBuf> = paths
            .iter()
            .map(|path| crate::platform::canonical(path))
            .collect();
        let one = paths.len() == 1;
        let dirty = self
            .state()
            .is_some_and(|state| keys.iter().any(|key| dirty_tabs_under(&state, key)));
        let question = trash_question(&paths);
        // A test instance shows no alert, which would wait for a person,
        // and moves nothing to the real Trash: it says what it would do.
        if self.ivars().testing {
            if let Some(mut state) = self.state_mut() {
                let said = if dirty {
                    "would refuse: unsaved changes are open".to_string()
                } else {
                    format!("would ask: {question}")
                };
                state.message = Some((said, Instant::now()));
            }
            self.request_redraw();
            self.pump();
            return;
        }
        if dirty {
            ask(
                MainThreadMarker::from(self),
                "Unsaved changes are open",
                if one {
                    "Save or close the affected tabs before moving this item to Trash."
                } else {
                    "Save or close the affected tabs before moving these items to Trash."
                },
                &["OK"],
            );
            return;
        }
        let answer = ask(
            MainThreadMarker::from(self),
            &question,
            if one {
                "The item can be recovered from the Trash."
            } else {
                "The items can be recovered from the Trash."
            },
            &["Move to Trash", "Cancel"],
        );
        if answer != 0 {
            return;
        }

        let mut moved = 0;
        let mut failed = None;
        for (path, key) in paths.iter().zip(&keys) {
            match self.file_op(Op::Trash(path)) {
                // Where it is in the Trash is kept with what was done: an
                // undo will need it.
                Ok(_) => {
                    moved += 1;
                    if let Some(mut state) = self.state_mut() {
                        for docs in all_docs_mut(&mut state) {
                            docs.close_under(key);
                        }
                    }
                }
                Err(error) => {
                    failed = Some(format!("{}: {error}", item_name(path)));
                    break;
                }
            }
        }
        if let Some(mut state) = self.state_mut() {
            reveal_active_tab(&mut state);
            let said = match (&failed, moved) {
                (None, 1) => format!("moved {} to Trash", item_name(&paths[0])),
                (None, _) => format!("moved {moved} items to Trash"),
                (Some(why), 0) => format!("could not move to Trash: {why}"),
                (Some(why), _) => format!(
                    "moved {moved} of {} items to Trash, then could not move {why}",
                    paths.len()
                ),
            };
            state.message = Some((said, Instant::now()));
        }
        if moved > 0 {
            self.refresh_project_after_disk_change();
            self.sync_title();
            self.reparse();
        }
        self.request_redraw();
        self.pump();
    }

    /// Copy Path and Copy Relative Path for the selection: one path to a
    /// line, top to bottom as the tree shows them.
    pub(super) fn copy_selected_paths(&self, relative: bool) {
        let Some(paths) = self.state().map(|state| {
            let root = state.tree.root().filter(|_| relative);
            state
                .tree
                .selected_paths()
                .iter()
                .map(|path| {
                    root.and_then(|root| path.strip_prefix(root).ok())
                        .unwrap_or(path)
                        .to_string_lossy()
                        .into_owned()
                })
                .collect::<Vec<_>>()
        }) else {
            return;
        };
        match paths.as_slice() {
            [] => {}
            [path] => self.copy_and_say(path),
            _ => {
                clipboard::write_text(&paths.join("\n"));
                if let Some(mut state) = self.state_mut() {
                    state.message = Some((format!("copied {} paths", paths.len()), Instant::now()));
                }
                self.request_redraw();
                self.pump();
            }
        }
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

    /// File > New Workspace: picks a folder, writes the notes a workspace
    /// starts with where they are missing, and opens it. A test instance
    /// takes the folder from `CRC_NEW_WORKSPACE` instead of a panel.
    pub(super) fn new_workspace(&self) {
        let path = if self.ivars().testing {
            std::env::var_os("CRC_NEW_WORKSPACE").map(std::path::PathBuf::from)
        } else {
            choose_path(
                MainThreadMarker::from(self),
                true,
                Some("Choose or create the folder that holds the repositories and their notes"),
            )
        };
        let Some(path) = path else {
            return;
        };
        match crate::project::workspace::scaffold(&path) {
            Ok(written) => {
                self.load_folder_path(&path.to_string_lossy());
                if let Some(mut state) = self.state_mut() {
                    let said = match written.len() {
                        0 => "Workspace opened; its notes were all there".to_string(),
                        1 => "Workspace ready: 1 note written".to_string(),
                        n => format!("Workspace ready: {n} notes written"),
                    };
                    state.message = Some((said, Instant::now()));
                }
            }
            Err(error) => {
                if let Some(mut state) = self.state_mut() {
                    state.message = Some((error, Instant::now()));
                }
            }
        }
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
        let said = state
            .restore_note
            .take()
            .unwrap_or_else(|| "Folder opened".to_string());
        state.message = Some((said, Instant::now()));
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
            error,
        } = result;
        if state.tree.root() != Some(root.as_path()) {
            return;
        }
        if let Some(error) = error {
            state.say(layout::Feedback::Failure, error);
        }
        if state.tree_version != tree_version {
            state.finder = finder;
            state.watch.index_rx = Some(spawn_project_refresh(
                state.tree.clone(),
                state.tree_version,
            ));
            return;
        }
        // What was selected, and scrolled to, while the worker read. The
        // active document's row is shown and selected as before, unless
        // the tree is being worked in: a cursor moved by the keyboard, or
        // several items picked, must not jump because a file changed.
        tree.adopt_selection(&state.tree);
        if !(tree_has_keys(&state) || tree.selection_len() > 1)
            && let Some(path) = state.docs.active().path.as_deref()
        {
            tree.reveal(path);
        }
        // The worker's tree carries the ignore set from when it started;
        // keep the newer one, then ask Git again, since whatever changed
        // the tree may have changed what is ignored.
        tree.set_ignored(state.tree.ignored_set());
        state.tree = tree;
        // What a paste or a duplicate made has rows now.
        explorer::select_made(&mut state);
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

/// Whether the sidebar is showing the Explorer's tree, not another view.
pub(super) fn explorer_showing(state: &State) -> bool {
    state.sidebar
        && !state.agents.on
        && !state.git_open
        && !state.search_open
        && !state.mcp.open
        && state.extensions.is_none()
}

/// Whether keys go to the Explorer's tree: it was pressed last, it is what
/// the sidebar shows, and neither a field (a name being typed, the palette,
/// the find bar) nor the terminal has them. Drawing, the key handler and
/// the menu's enabled state all ask this, so they cannot disagree.
pub(super) fn tree_has_keys(state: &State) -> bool {
    state.sidebar_keys
        && explorer_showing(state)
        && state.tree.root().is_some()
        && !field_has_keys(state)
        && !state.terminal.has_keys()
}

/// The path the tree shows the open document `open` under. Documents keep
/// their paths resolved, and the project root may have been given through
/// a symbolic link.
fn tree_path_of(tree: &Tree, open: &Path) -> Option<std::path::PathBuf> {
    let root = tree.root()?;
    if open.starts_with(root) {
        return Some(open.to_path_buf());
    }
    let rest = open.strip_prefix(crate::platform::canonical(root)).ok()?;
    Some(root.join(rest))
}

/// The tree takes the keyboard: from the document, and from a find bar
/// left open, which keeps its place and gets the keys back with a click.
pub(super) fn take_tree_keys(state: &mut State) {
    state.sidebar_keys = true;
    if let Some(bar) = &mut state.find {
        bar.has_keys = false;
    }
}

/// Whether a tab on `path`, or on anything under it, holds unsaved work,
/// in any pane: the split ones as well as the one with the keyboard.
pub(super) fn dirty_tabs_under(state: &State, path: &Path) -> bool {
    all_docs(state).any(|docs| docs.has_dirty_under(path))
}

/// Opens the folder at tree row `index` (`Some(true)`), closes it
/// (`Some(false)`), or whichever it is not (`None`, a click). Closing is at
/// once; the children of one that opens are read on a worker.
fn set_tree_folder(state: &mut State, index: usize, open: Option<bool>) {
    let Some(entry) = state.tree.rows().get(index) else {
        return;
    };
    if !entry.is_dir {
        return;
    }
    let (path, depth, expanded) = (entry.path.clone(), entry.depth, entry.expanded);
    // A scan under way read the tree as it was before this.
    state.tree_version += 1;
    if expanded {
        if open != Some(true) {
            state.tree.toggle(index);
        }
    } else if open == Some(false) {
        state.watch.children_pending.remove(&path);
    } else if state.watch.children_pending.insert(path.clone()) {
        let root = state.tree.root().map(Path::to_path_buf);
        let tx = state.watch.children_tx.clone();
        std::thread::spawn(move || {
            if let Some(root) = root {
                let children = Tree::children(&path, depth + 1);
                let _ = tx.send((root, path, children));
            }
        });
    } else if open.is_none() {
        // A second click before the worker finishes cancels expansion.
        state.watch.children_pending.remove(&path);
    }
}

/// A project item's name, as a message says it.
pub(super) fn item_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// What Move to Trash asks before it moves `paths`: the name of the one
/// item, or how many there are.
fn trash_question(paths: &[std::path::PathBuf]) -> String {
    match paths {
        [path] => format!("Move \u{201c}{}\u{201d} to Trash?", item_name(path)),
        _ => format!("Move {} items to Trash?", paths.len()),
    }
}

/// The tree as a `dump` reports it: whether it has the keyboard, its rows
/// by their path in the project (a folder with a slash, a row cut and not
/// yet pasted after a `~`, a selected row starred, the cursor's after a
/// `>`), what a command on the selection would act on, the first row
/// shown, and the file work: see [`explorer::report`].
pub(super) fn tree_report(state: &State) -> String {
    let tree = &state.tree;
    let shown = |path: &Path| {
        tree.root()
            .and_then(|root| path.strip_prefix(root).ok())
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    };
    let rows: Vec<String> = tree
        .rows()
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            format!(
                "{}{}{}{}{}",
                if tree.is_cut(&entry.path) { "~" } else { "" },
                if tree.is_selected(index) { "*" } else { "" },
                if tree.cursor_row() == Some(index) {
                    ">"
                } else {
                    ""
                },
                shown(&entry.path),
                if entry.is_dir { "/" } else { "" },
            )
        })
        .collect();
    let acts_on: Vec<String> = tree
        .selected_roots()
        .iter()
        .map(|path| shown(path))
        .collect();
    format!(
        "tree_keys: {}\ntree_rows: {}\ntree_acts_on: {}\ntree_scroll: {}\n{}",
        tree_has_keys(state),
        rows.join("|"),
        acts_on.join("|"),
        tree.scroll,
        explorer::report(state),
    )
}

#[cfg(test)]
mod tests {
    use super::trash_question;
    use std::path::PathBuf;

    #[test]
    fn the_trash_question_names_one_item_and_counts_several() {
        assert_eq!(
            trash_question(&[PathBuf::from("/p/notes/today.md")]),
            "Move \u{201c}today.md\u{201d} to Trash?"
        );
        assert_eq!(
            trash_question(&[PathBuf::from("/p/a.txt"), PathBuf::from("/p/docs")]),
            "Move 2 items to Trash?"
        );
    }
}
