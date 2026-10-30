//! Agents mode in the window: the switch, the workspace list's clicks and
//! menus, and the work on tasks (new worktree, merge, push, remove) on
//! workers.

use super::*;
use crate::platform::agents::{Action, Done, Follow, follow, task_of};
use crate::platform::terminal::Activity;

impl EditorView {
    /// Editor or Agents. Agents gives the terminal the column and the
    /// sidebar to the workspaces, starting a shell when nothing runs yet;
    /// the editor gets the column back with the terminal panel closed.
    pub(super) fn set_agents_mode(&self, on: bool) {
        let start = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if state.agents.on == on {
                drop(state);
                if on {
                    self.focus_agent_terminal();
                }
                return;
            }
            state.agents.on = on;
            // What the other mode said is not about this one.
            state.dismiss(layout::Dismissal::ModeSwitch);
            if on {
                state.terminal.open = true;
                state.terminal.focus = true;
                state.sidebar_keys = false;
                state.palette = None;
                state.completion = None;
                // A page over the editor column is left behind like any
                // other: hidden but kept open, it was over the tabs again
                // on every switch back to the editor.
                uncover_column(&mut state);
                state.agents.refresh();
                if state.agents.selected_index().is_none() {
                    let tasks = &state.agents.tasks;
                    state.agents.selected = state
                        .terminal
                        .active_tab()
                        .and_then(|t| task_of(tasks, t))
                        .map(|i| tasks[i].id());
                }
                // The selected workspace's newest terminal, when the one
                // showing is another's; with none, its empty state.
                if active_hidden(&state) {
                    let tasks = &state.agents.tasks;
                    let selected = state.agents.selected_index();
                    if let Some(last) = (0..state.terminal.tabs.len())
                        .rev()
                        .find(|i| task_of(tasks, &state.terminal.tabs[*i]) == selected)
                    {
                        state.terminal.show(last);
                    }
                }
                state.terminal.tabs.is_empty()
            } else {
                // The editor never opens the terminal for you: the sessions
                // keep running, a click on the terminal button shows them.
                state.terminal.open = false;
                state.terminal.focus = false;
                false
            }
        };
        if start {
            self.spawn_terminal(false);
        }
        if !on {
            self.editor_follows_agents();
        }
        if on {
            self.arm_agents_tick();
            self.arm_sessions_tick();
            if let Some(mut state) = self.state_mut() {
                let active = state.terminal.active;
                if let Some(tab) = state.terminal.tabs.get_mut(active) {
                    tab.read_cwd();
                }
            }
            self.follow_cwd();
        }
        self.after_terminal_layout();
        self.resume_display_link();
    }

    /// The keyboard to the terminal, as Agents mode has it.
    fn focus_agent_terminal(&self) {
        if let Some(mut state) = self.state_mut() {
            state.terminal.open = true;
            state.terminal.focus = true;
            state.sidebar_keys = false;
        }
        self.after_terminal_layout();
    }

    /// Watches every worktree in the list for writes, so one written in by
    /// an agent crc did not start (a background one, run from another
    /// session) still says so. Made again only when the worktrees change.
    pub(super) fn watch_worktrees(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let paths = state.agents.worktree_paths();
        if paths == state.agents.watched {
            return;
        }
        // The view lives for the process; the handlers run on the main
        // thread and only take the time.
        let view = self as *const EditorView;
        state.agents.watchers = paths
            .iter()
            .filter_map(|path| {
                let tree = path.clone();
                crate::project::watch::Watcher::tree(
                    path,
                    Box::new(move |_| {
                        let view = unsafe { &*view };
                        view.worktree_written(&tree);
                    }),
                )
            })
            .collect();
        state.agents.watched = paths;
        drop(state);
        self.watch_pushes();
    }

    /// Watches the Git directory of each main checkout with worktrees in
    /// the list, so a push of a worktree's branch, from crc or anywhere
    /// else, reads its pull request again. Made again only when the main
    /// checkouts change.
    fn watch_pushes(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let mains = state.agents.main_checkouts();
        if mains == state.agents.ref_watched {
            return;
        }
        let view = self as *const EditorView;
        state.agents.ref_watchers = mains
            .iter()
            .filter_map(|main| {
                // A main checkout keeps its Git directory in it.
                let git_dir = main.join(".git");
                if !git_dir.is_dir() {
                    return None;
                }
                let main = main.clone();
                crate::project::watch::Watcher::tree(
                    &git_dir,
                    Box::new(move |_| {
                        let view = unsafe { &*view };
                        view.refs_changed(&main);
                    }),
                )
            })
            .collect();
        state.agents.ref_watched = mains;
    }

    /// Main thread, from a main checkout's Git directory watcher: the
    /// inspector reads it again soon when it shows it, and if a worktree's
    /// branch was pushed, its pull request is read once the push settles.
    fn refs_changed(&self, main: &std::path::Path) {
        let shown = self
            .state_mut()
            .is_some_and(|mut state| state.agents.refs_moved(main));
        if shown {
            self.want_inspector();
            self.resume_display_link();
        }
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let git_dir = crate::platform::canonical(&main.join(".git"));
        let Some(files) = state
            .agents
            .ref_watchers
            .iter()
            .find(|w| w.root() == git_dir)
            .map(|w| w.take_files())
        else {
            return;
        };
        let branches = crate::platform::agents::pushed_branches(&git_dir, &files);
        if let Some(b) = &branches
            && b.is_empty()
        {
            return;
        }
        let pushed = state.agents.note_push(main, branches.as_deref());
        drop(state);
        if pushed {
            self.arm_pushed_tick();
        }
    }

    /// Main thread, from a worktree's watcher: a file in it changed.
    fn worktree_written(&self, tree: &std::path::Path) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let was = state.agents.wrote.get(tree).copied();
        // When it happened, not when the main thread got to it.
        let at = state
            .agents
            .watchers
            .iter()
            .find(|w| w.root() == tree)
            .and_then(|w| w.changed_at())
            .unwrap_or_else(std::time::Instant::now);
        state.agents.touched_at(tree, at);
        let on = state.agents.on;
        // Its status, when the inspector shows it, is read again soon:
        // within seconds, however often it is written to.
        let shown = state.agents.inspector.touch(tree);
        drop(state);
        if on && shown {
            self.want_inspector();
        }
        // A repaint when the row turns to writing; the spinner keeps
        // frames coming while it stays so.
        if on && was.is_none_or(|at| at.elapsed() >= crate::platform::agents::WRITING) {
            self.request_redraw();
            self.resume_display_link();
        }
    }

    pub(super) fn poll_agents(&self) {
        let front = self.in_front();
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let mut changed = state.agents.poll();
        // A read the wake missed (the state was busy then) lands here.
        let landed = state.agents.activity.poll();
        // The selected workspace's folder, once a minute in front, and one
        // just selected whose last read is older: read here, not only at
        // the tick, so selecting it reads it at once.
        if front && state.agents.on && state.agents.listed() {
            state.agents.refresh_due(true);
        }
        let coming = selection_coming(&state);
        if changed && !coming && state.agents.selected_index().is_none() {
            // Nothing selected that the list knows: the workspace of the
            // session showing, else the first.
            let tasks = state.agents.tasks.clone();
            state.agents.selected = state
                .terminal
                .active_tab()
                .and_then(|t| task_of(&tasks, t))
                .or((!tasks.is_empty()).then_some(0))
                .map(|i| tasks[i].id());
        }
        let ensure = changed && state.agents.on;
        // A tab clicked, a session started or closed: what shows now is
        // the latest place for the switcher.
        note_focus(&mut state);
        let done = state.agents.take_done();
        let issue = state.agents.take_issue_detail();
        let mut start_in = None;
        let mut prompt = None;
        if let Some(done) = done {
            changed = true;
            match done {
                Ok(Done {
                    said,
                    start_in: dir,
                    claude,
                }) => {
                    state.say(layout::Feedback::Success, said);
                    start_in = dir;
                    prompt = claude;
                }
                Err(e) => state.say(layout::Feedback::Failure, e),
            }
            state.agents.refresh();
            state.git.refresh();
            // A pull request opened or merged: its row says so now, not
            // at the next read.
            if let Some(folder) = state.agents.pr_after.take() {
                state.agents.refresh_pull_request(&folder);
            }
        }
        drop(state);
        if ensure {
            self.ensure_workspace();
        }
        if changed {
            self.watch_worktrees();
        }
        self.want_inspector();
        if let Some((repo, answer)) = issue {
            self.take_issue(repo, answer);
        }
        match start_in {
            // A worktree's id is its path: selectable before the list
            // comes back with it.
            Some((None, dir)) => {
                let id = crate::platform::canonical(&dir)
                    .to_string_lossy()
                    .into_owned();
                if let Some(mut state) = self.state_mut() {
                    state.agents.selected = Some(id.clone());
                }
                match prompt {
                    Some(prompt) => self.spawn_claude_told(&dir, id, &prompt),
                    None => self.spawn_terminal_in(false, Some(&dir)),
                }
            }
            // One on a host is `host:path`, its terminal ssh there.
            Some((Some(host), dir)) => {
                let id = crate::platform::agents::row_id(Some(&host), &dir);
                if let Some(mut state) = self.state_mut() {
                    state.agents.selected = Some(id.clone());
                }
                self.spawn_remote_terminal(&host, &dir, id);
            }
            None => {}
        }
        if changed || landed {
            self.request_redraw();
        }
    }

    /// From the Activity panel's worker: a read landed.
    pub(super) fn poll_activity(&self) {
        let landed = self
            .state_mut()
            .is_some_and(|mut state| state.agents.activity.poll());
        if landed {
            self.request_redraw();
            self.pump();
        }
    }

    /// After every frame: the Activity panel's worker runs while the panel
    /// was drawn unfolded, and stops after a frame without it (folded, the
    /// sidebar hidden, the editor back).
    pub(super) fn activity_after_frame(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let monitor = &mut state.agents.activity;
        if monitor.shown == monitor.running() {
            return;
        }
        if monitor.shown {
            monitor.start(self.wake(EditorView::poll_activity));
        } else {
            monitor.stop();
        }
    }

    /// The selected workspace follows the directory of the terminal
    /// showing (see [`follow`]): a folder workspace moves there; out of a
    /// worktree the terminal joins the workspace holding where it went, or
    /// a new one made on that folder, and a folder workspace it leaves
    /// empty goes. The inspector reads the repositories it is in and under.
    /// A terminal running ssh stays where it is.
    pub(super) fn follow_cwd(&self) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if !state.agents.on {
                return;
            }
            let active = state.terminal.active;
            let Some(tab) = state.terminal.tabs.get(active) else {
                return;
            };
            if tab.remote.is_some() {
                return;
            }
            let cwd = tab.cwd.clone();
            let tasks = state.agents.tasks.clone();
            let Some(index) = task_of(&tasks, tab) else {
                return;
            };
            // Another workspace's terminal, behind this one's empty state:
            // where it went never selects anything.
            if state.agents.selected_index().is_some_and(|s| s != index) {
                return;
            }
            let task = &tasks[index];
            let id = match follow(&tasks, index, &cwd) {
                Follow::Stay => return,
                Follow::Move => {
                    state.agents.move_folder(&task.id(), &cwd);
                    state.agents.refresh();
                    None
                }
                Follow::Join(h) => Some(tasks[h].id()),
                Follow::Make => {
                    if too_broad_to_adopt(&cwd) {
                        return;
                    }
                    let name = cwd
                        .file_name()
                        .map_or_else(|| "workspace".into(), |n| n.to_string_lossy().into_owned());
                    let id = state.agents.add_folder(&name, cwd.clone());
                    state.agents.refresh();
                    Some(id)
                }
            };
            if let Some(id) = id {
                adopt(&mut state.terminal.tabs[active], &id);
                state.agents.selected = Some(id);
                if task.folder {
                    state.terminal.closed_in.push(task.id());
                    forget_emptied(&mut state);
                }
            }
        }
        self.want_inspector();
        self.resume_display_link();
        self.request_redraw();
    }

    /// Every terminal lives in a workspace. Once the list is known, the
    /// selection is the workspace of the session showing, else the one
    /// holding the open folder, else the first; with no workspace at all,
    /// one is made on the open folder, named after it. Returns whether a
    /// workspace is selected.
    pub(super) fn ensure_workspace(&self) -> bool {
        let editor = self.editor_folder();
        let Some(mut state) = self.state_mut() else {
            return false;
        };
        if !state.agents.listed() {
            return false;
        }
        if selection_coming(&state) {
            return true;
        }
        let tasks = state.agents.tasks.clone();
        // The terminal showing, when no workspace holds it, gets one on its
        // own folder. Not one ssh'd into a host's workspace: its folder here
        // is only where ssh started, and its row comes when the host answers.
        let loose = state
            .terminal
            .active_tab()
            .filter(|t| task_of(&tasks, t).is_none())
            .filter(|t| {
                t.workspace.is_none()
                    || !crate::platform::remote::is_ssh_launch(&t.launch.program, &t.launch.args)
            })
            .map(|t| t.folder.clone());
        if loose.is_none() && state.agents.selected_index().is_some() {
            return true;
        }
        let found = state
            .terminal
            .active_tab()
            .and_then(|t| task_of(&tasks, t))
            .or_else(|| {
                editor.as_deref().and_then(|f| {
                    crate::platform::agents::folder_task(&tasks, &crate::platform::canonical(f))
                })
            })
            .or((!tasks.is_empty()).then_some(0));
        let id = match (loose, found) {
            (None, Some(i)) => tasks[i].id(),
            (loose, _) => {
                let Some(folder) = loose.or(editor) else {
                    return false;
                };
                let folder = crate::platform::canonical(&folder);
                let name = folder
                    .file_name()
                    .map_or_else(|| "workspace".into(), |n| n.to_string_lossy().into_owned());
                let id = state.agents.add_folder(&name, folder.clone());
                state.agents.refresh();
                // The terminals in its folder that no workspace held are
                // its own; the others stay where they are.
                for tab in &mut state.terminal.tabs {
                    if task_of(&tasks, tab).is_none()
                        && crate::platform::canonical(&tab.folder).starts_with(&folder)
                    {
                        adopt(tab, &id);
                    }
                }
                id
            }
        };
        state.agents.selected = Some(id);
        true
    }

    /// A double-click on the empty part of the list: a new folder workspace
    /// on the selected one's folder (see
    /// [`crate::platform::agents::Panel::blank_folder`]), else the open
    /// folder, named after it, with a terminal in it; with no folder at
    /// all, New Workspace.
    fn new_blank_workspace(&self) {
        let folder = self.state().and_then(|state| {
            state
                .agents
                .blank_folder(state.tree.root())
                .or_else(|| state.terminal.active_tab().map(|t| t.folder.clone()))
        });
        let Some(folder) = folder else {
            self.new_task_prompt();
            return;
        };
        let folder = crate::platform::canonical(&folder);
        let name = folder
            .file_name()
            .map_or_else(|| "workspace".into(), |n| n.to_string_lossy().into_owned());
        self.create_workspace_in(name, &folder);
    }

    pub(super) fn agents_click(&self, event: &NSEvent, x: f32, y: f32) {
        let Some(action) = self.state().and_then(|state| state.agents.hit(x, y)) else {
            return;
        };
        match action {
            Action::NewTask => self.new_task_prompt(),
            Action::Refresh => self.refresh_agents(),
            // A double-click names a workspace in its row.
            Action::Select(index) if event.clickCount() >= 2 => {
                let id = self
                    .state()
                    .and_then(|state| state.agents.tasks.get(index).map(|t| t.id()));
                if let Some(id) = id {
                    self.start_agents_edit(&id);
                }
            }
            Action::Select(index) => {
                // The press may become a drag to a new place in the list.
                if let Some(mut state) = self.state_mut()
                    && let Some(id) = state.agents.tasks.get(index).map(|t| t.id())
                {
                    state.agents.drag = Some(crate::platform::agents::RowDrag {
                        id,
                        start_y: y,
                        y,
                        active: false,
                    });
                }
                self.select_task(index);
            }
            Action::Session(tab) => self.show_session(tab),
            Action::Fold(index) => {
                if let Some(mut state) = self.state_mut()
                    && let Some(id) = state.agents.tasks.get(index).map(|t| t.id())
                {
                    state.agents.toggle_collapsed(&id);
                }
            }
            Action::Merged(index) => {
                if let Some(mut state) = self.state_mut()
                    && let Some(id) = state.agents.tasks.get(index).map(|t| t.id())
                {
                    state.agents.toggle_merged(&id);
                }
            }
            Action::NewSession(index) => {
                let Some(dir) = self.task_path(index) else {
                    return;
                };
                if let Some(mut state) = self.state_mut() {
                    state.agents.select(index);
                }
                self.spawn_terminal_in(false, Some(&dir));
            }
            // Only a double-click makes something: a stray click on the
            // empty part of the list does nothing.
            Action::Blank if event.clickCount() >= 2 => self.new_blank_workspace(),
            Action::Blank => {}
            Action::Activity => {
                if let Some(mut state) = self.state_mut() {
                    state.agents.toggle_activity();
                }
            }
            Action::Menu(index) => {
                if let Some(mut state) = self.state_mut() {
                    state.agents.menu_task = state.agents.tasks.get(index).map(|t| t.id());
                    state.agents.target = None;
                }
                let menu = self.task_menu(index);
                layout::set_pressed(None);
                NSMenu::popUpContextMenu_withEvent_forView(&menu, event, self);
            }
        }
        self.request_redraw();
        self.pump();
    }

    fn task_path(&self, index: usize) -> Option<std::path::PathBuf> {
        let state = self.state()?;
        state.agents.tasks.get(index).map(|t| t.dir())
    }

    /// Shows a task: the session you last had in front in it, else its
    /// waiting one, else the newest it has, else a new shell in its
    /// folder.
    pub(super) fn select_task(&self, index: usize) {
        let found = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(task) = state.agents.tasks.get(index) else {
                return;
            };
            let path = task.dir();
            let key = switcher::workspace_key(&task.id());
            state.agents.select(index);
            switcher::touch(&mut state.agents.focus, &key);
            let tasks = state.agents.tasks.clone();
            let mine: Vec<usize> = (0..state.terminal.tabs.len())
                .filter(|i| task_of(&tasks, &state.terminal.tabs[*i]) == Some(index))
                .collect();
            let tabs = &state.terminal.tabs;
            let pick = switcher::latest(&state.agents.focus, &mine, |i| {
                switcher::session_key(&tabs[i].id())
            })
            .or_else(|| {
                super::menubar::best_tab(
                    &mine,
                    |i| tabs[i].activity() == Activity::Waiting,
                    state.terminal.active,
                )
            });
            (pick, path)
        };
        match found {
            (Some(tab), _) => self.show_session(tab),
            (None, path) => {
                self.set_agents_mode(true);
                self.spawn_terminal_in(false, Some(&path));
            }
        }
    }

    /// A session to the front, with the keyboard, in Agents mode.
    pub(super) fn show_session(&self, tab: usize) {
        if let Some(mut state) = self.state_mut() {
            if tab >= state.terminal.tabs.len() {
                return;
            }
            state.terminal.show(tab);
            let tasks = state.agents.tasks.clone();
            if let Some(task) = task_of(&tasks, &state.terminal.tabs[tab]) {
                state.agents.select(task);
            }
        }
        self.set_agents_mode(true);
        self.focus_agent_terminal();
        if let Some(mut state) = self.state_mut() {
            note_focus(&mut state);
            let active = state.terminal.active;
            if let Some(tab) = state.terminal.tabs.get_mut(active) {
                tab.read_cwd();
            }
        }
        self.follow_cwd();
    }

    /// Command-1 to 9: the tab in that place, 9 the last. In Agents mode
    /// the selected workspace's terminals, as its tab bar lists them;
    /// in the editor its documents.
    pub(super) fn jump_to_tab(&self, number: usize) {
        let terminal = self.state().and_then(|state| {
            if !state.agents.on {
                return None;
            }
            let tasks = &state.agents.tasks;
            let selected = state.agents.selected_index();
            let mine: Vec<usize> = (0..state.terminal.tabs.len())
                .filter(|i| {
                    tasks.is_empty() || task_of(tasks, &state.terminal.tabs[*i]) == selected
                })
                .collect();
            Some(pick_numbered(&mine, number))
        });
        match terminal {
            Some(Some(tab)) => self.show_session(tab),
            Some(None) => {}
            None => {
                let count = self.state().map_or(0, |state| state.docs.len());
                let all: Vec<usize> = (0..count).collect();
                if let Some(index) = pick_numbered(&all, number) {
                    self.activate_tab(index);
                }
            }
        }
    }

    /// Control-1 to 9: the task in that place, as the list shows them
    /// (a worktree folded under its folder workspace has no place).
    pub(super) fn jump_to_task(&self, number: usize) {
        let index = self.state().and_then(|state| {
            number
                .checked_sub(1)
                .and_then(|n| state.agents.visible().get(n).copied())
        });
        match index {
            Some(index) => self.select_task(index),
            None => self.set_agents_mode(true),
        }
    }

    /// The inspector's Open on a worktree: its row, as a click on it.
    pub(super) fn open_worktree(&self, id: &str) {
        let index = self.state().and_then(|state| state.agents.by_id(id));
        if let Some(index) = index {
            self.select_task(index);
        }
    }

    /// The inspector's Terminal on a worktree: a new shell in it, which
    /// selects it, as Command-T does for the selected workspace.
    pub(super) fn worktree_terminal(&self, id: &str) {
        let found = self.state_mut().and_then(|mut state| {
            let index = state.agents.by_id(id)?;
            state.agents.select(index);
            Some(state.agents.tasks[index].dir())
        });
        if let Some(dir) = found {
            self.set_agents_mode(true);
            self.spawn_terminal_in(false, Some(&dir));
        }
    }

    /// The next session waiting on you, after the one showing.
    pub(super) fn next_waiting_agent(&self) {
        let next = self.state().and_then(|state| {
            let count = state.terminal.tabs.len();
            (1..=count)
                .map(|step| (state.terminal.active + step) % count)
                .find(|i| state.terminal.tabs[*i].activity() == Activity::Waiting)
        });
        match next {
            Some(tab) => self.show_session(tab),
            None => {
                if let Some(mut state) = self.state_mut() {
                    state.say(layout::Feedback::Info, "No agent is waiting on you");
                }
                self.request_redraw();
            }
        }
    }

    /// The palette, taking a new workspace's name: a folder workspace on a
    /// folder you choose or the open one (while no workspace has it), or a
    /// task on a new branch of the selected workspace's repository.
    pub(super) fn new_task_prompt(&self) {
        self.open_palette_with("");
        if let Some(mut state) = self.state_mut() {
            state.task_prompt = true;
            state.agents.renaming = None;
        }
        self.request_redraw();
    }

    /// The editor's open folder, else the session showing's.
    fn editor_folder(&self) -> Option<std::path::PathBuf> {
        let state = self.state()?;
        state
            .tree
            .root()
            .map(Path::to_path_buf)
            .or_else(|| state.terminal.active_tab().map(|t| t.folder.clone()))
    }

    /// The folder a new workspace is made on unless you choose one: the
    /// editor's open folder, else the session showing's, while no
    /// workspace has it (see [`crate::platform::agents::Panel::new_folder`]).
    fn workspace_folder(&self) -> Option<std::path::PathBuf> {
        let state = self.state()?;
        state.agents.new_folder(state.tree.root()).or_else(|| {
            state
                .terminal
                .active_tab()
                .map(|t| t.folder.clone())
                .filter(|f| !state.agents.has_workspace(f))
        })
    }

    /// The palette's answer when you choose the folder: a folder panel
    /// opening beside the selected workspace's folder, then the workspace.
    /// A test instance, with no panel to answer, takes the folder named as
    /// the workspace in the one the panel opens on, else that one.
    pub(super) fn create_workspace_chosen(&self, name: String) {
        let start = self
            .state()
            .and_then(|state| state.agents.pick_from(state.tree.root()));
        let dir = if std::env::var_os("CRC_SELFTEST").is_some() {
            start.map(|start| {
                let named = start.join(name.trim());
                if named.is_dir() { named } else { start }
            })
        } else {
            choose_folder_from(
                MainThreadMarker::from(self),
                start.as_deref(),
                &format!(
                    "Choose the folder for the workspace \u{201c}{}\u{201d}",
                    name.trim()
                ),
            )
        };
        if let Some(dir) = dir {
            self.create_workspace_in(name, &dir);
        }
    }

    /// The palette's answer for a folder workspace: made (or, while
    /// renaming, renamed) and selected, then a terminal in it.
    pub(super) fn create_workspace(&self, name: String) {
        let renaming = self.state().and_then(|state| state.agents.renaming.clone());
        if let Some(id) = renaming {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.agents.renaming = None;
            match state.agents.rename_folder(&id, &name) {
                Some(new_id) => {
                    for tab in &mut state.terminal.tabs {
                        if tab.workspace.as_deref() == Some(id.as_str()) {
                            adopt(tab, &new_id);
                        }
                    }
                    if state.agents.selected.as_deref() == Some(id.as_str()) {
                        state.agents.selected = Some(new_id);
                    }
                }
                None => state.say(
                    layout::Feedback::Failure,
                    format!("A workspace is already called {}", name.trim()),
                ),
            }
            drop(state);
            self.request_redraw();
            return;
        }
        let Some(dir) = self.workspace_folder() else {
            return;
        };
        self.create_workspace_in(name, &dir);
    }

    /// The palette's answer for a remote workspace: `path` on `host`, named
    /// after its folder, selected, and a terminal there over ssh.
    pub(super) fn create_remote_workspace(&self, host: String, path: String) {
        let id = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.agents.on = true;
            let name = crate::platform::remote::workspace_name(&host, &path);
            let id = state.agents.add_remote(&name, &host, &path);
            state.agents.selected = Some(id.clone());
            // Its repositories read now, not at the next periodic read.
            state.agents.refresh();
            id
        };
        self.set_agents_mode(true);
        self.spawn_terminal_into(false, Some(Path::new(&path)), Some(id));
    }

    /// A folder workspace on `dir`, selected, its repositories read, and a
    /// terminal in it. A folder holds any number of workspaces.
    fn create_workspace_in(&self, name: String, dir: &Path) {
        let dir = crate::platform::canonical(dir);
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.agents.on = true;
            let id = state.agents.add_folder(&name, dir.clone());
            state.agents.selected = Some(id);
            state.agents.refresh();
        }
        self.set_agents_mode(true);
        self.spawn_terminal_in(false, Some(&dir));
    }

    /// The palette's answer for a workspace without a folder: made from
    /// its name, selected, and a terminal in the home folder.
    pub(super) fn create_folderless_workspace(&self, name: String) {
        let (id, dir) = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.agents.on = true;
            let id = state.agents.add_folderless(&name);
            state.agents.selected = Some(id.clone());
            let dir = state.agents.by_id(&id).map(|i| state.agents.tasks[i].dir());
            (id, dir)
        };
        self.set_agents_mode(true);
        self.spawn_terminal_into(false, dir.as_deref(), Some(id));
    }

    /// The context menu for a point in the Agents sidebar: the session's,
    /// the workspace's, or the list's.
    pub(super) fn agents_context_menu(&self, x: f32, y: f32) -> Retained<NSMenu> {
        let mtm = MainThreadMarker::from(self);
        let hit = self.state().and_then(|state| state.agents.hit(x, y));
        match hit {
            Some(Action::Session(tab)) => {
                if let Some(mut state) = self.state_mut() {
                    state.agents.menu_session = Some(tab);
                }
                let menu = context_menu_new(mtm);
                let add = |title: &str, action: Sel| menu.addItem(&menu_item(mtm, title, action));
                add("New Terminal Here", sel!(agentSessionNewShell:));
                add("New Claude Session Here", sel!(agentSessionNewClaude:));
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                add("Close Session", sel!(agentCloseSession:));
                menu
            }
            Some(
                Action::Select(index)
                | Action::NewSession(index)
                | Action::Menu(index)
                | Action::Fold(index),
            ) => {
                if let Some(mut state) = self.state_mut() {
                    state.agents.menu_task = state.agents.tasks.get(index).map(|t| t.id());
                    state.agents.target = None;
                }
                self.task_menu(index)
            }
            Some(Action::Merged(index)) => {
                if let Some(mut state) = self.state_mut() {
                    state.agents.menu_task = state.agents.tasks.get(index).map(|t| t.id());
                    state.agents.target = None;
                }
                let menu = context_menu_new(mtm);
                menu.addItem(&menu_item(
                    mtm,
                    "Clean Up All Merged\u{2026}",
                    sel!(agentCleanUpMerged:),
                ));
                menu
            }
            _ => {
                let menu = context_menu_new(mtm);
                let add = |title: &str, action: Sel| menu.addItem(&menu_item(mtm, title, action));
                add("New Workspace\u{2026}", sel!(newTask:));
                add("Refresh", sel!(refreshAgents:));
                menu
            }
        }
    }

    /// A session's menu: a new session beside it, in its workspace.
    pub(super) fn agent_session_new(&self, claude: bool) {
        let Some(tab) = self.state().and_then(|state| state.agents.menu_session) else {
            return;
        };
        let dir = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if tab >= state.terminal.tabs.len() {
                return;
            }
            let tasks = state.agents.tasks.clone();
            match task_of(&tasks, &state.terminal.tabs[tab]) {
                Some(index) => {
                    state.agents.select(index);
                    tasks[index].dir()
                }
                None => state.terminal.tabs[tab].folder.clone(),
            }
        };
        self.set_agents_mode(true);
        self.spawn_terminal_in(claude, Some(&dir));
    }

    pub(super) fn agent_close_session(&self) {
        let Some(tab) = self.state().and_then(|state| state.agents.menu_session) else {
            return;
        };
        if let Some(mut state) = self.state_mut()
            && tab < state.terminal.tabs.len()
        {
            state.terminal.close_tab(tab);
        }
        self.after_terminal_layout();
    }

    /// Refresh, asked for: the list, every pull request, and the selected
    /// workspace's issues again.
    pub(super) fn refresh_agents(&self) {
        if let Some(mut state) = self.state_mut() {
            state.agents.refresh();
            state.agents.refresh_all_pull_requests();
            state.agents.want_issues_now();
        }
        self.resume_display_link();
    }

    /// Opens the name field in a workspace's row, its name selected.
    pub(super) fn start_agents_edit(&self, id: &str) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some(task) = state
            .agents
            .by_id(id)
            .map(|i| state.agents.tasks[i].clone())
        else {
            return;
        };
        close_fields(&mut state);
        state.palette = None;
        let mut field = Buffer::from_text(&state.agents.title(&task));
        field.select_all();
        state.agents_edit = Some((id.to_owned(), field));
        drop(state);
        self.request_redraw();
        self.pump();
    }

    /// Keys while a workspace's name is being edited in its row.
    pub(super) fn handle_agents_edit_key(&self, event: &NSEvent) -> bool {
        const ESCAPE: u16 = 53;
        let flags = event.modifierFlags();
        if flags.contains(NSEventModifierFlags::Command) {
            return false;
        }
        let motion = if flags.contains(NSEventModifierFlags::Shift) {
            Motion::Extend
        } else {
            Motion::Move
        };
        let code = match event.keyCode() {
            key::KEYPAD_ENTER => key::RETURN,
            code => code,
        };
        match code {
            ESCAPE => self.finish_agents_edit(false),
            key::RETURN => self.finish_agents_edit(true),
            key::TAB | key::UP | key::DOWN => return true,
            key::DELETE | key::LEFT | key::RIGHT | key::HOME | key::END => {
                let Some(mut state) = self.state_mut() else {
                    return false;
                };
                if let Some((_, field)) = &mut state.agents_edit {
                    match code {
                        key::DELETE if flags.contains(NSEventModifierFlags::Option) => {
                            field.delete_word_backward();
                        }
                        key::DELETE => field.backspace(),
                        key::LEFT => field.move_left(motion),
                        key::RIGHT => field.move_right(motion),
                        key::HOME => field.move_line_start(motion),
                        _ => field.move_line_end(motion),
                    }
                }
            }
            _ => return self.interpret(event),
        }
        self.request_redraw();
        self.pump();
        true
    }

    /// Ends the row's name field. A folder workspace takes the name as its
    /// own; any other keeps its branch and shows the name instead of it.
    /// An empty name gives a workspace back its own.
    pub(super) fn finish_agents_edit(&self, commit: bool) {
        let Some((id, field)) = self
            .state_mut()
            .and_then(|mut state| state.agents_edit.take())
        else {
            return;
        };
        let name = field.rope.to_string();
        let name = name.trim().to_owned();
        if commit {
            let folder = self.state().is_some_and(|state| {
                state
                    .agents
                    .by_id(&id)
                    .is_some_and(|i| state.agents.tasks[i].folder)
            });
            if folder && !name.is_empty() {
                if let Some(mut state) = self.state_mut() {
                    state.agents.renaming = Some(id);
                }
                self.create_workspace(name);
            } else if let Some(mut state) = self.state_mut() {
                state.agents.set_title(&id, &name);
            }
        }
        self.request_redraw();
        self.pump();
    }

    /// The palette, taking the folder workspace's new name.
    pub(super) fn agent_rename_workspace(&self) {
        let Some(task) = self.menu_task().filter(|t| t.folder) else {
            return;
        };
        self.open_palette_with(&task.name);
        if let Some(mut state) = self.state_mut() {
            state.task_prompt = true;
            state.agents.renaming = Some(task.id());
        }
        self.request_redraw();
    }

    /// Forgets a folder workspace once asked, and the worktrees under it;
    /// their sessions close. Nothing on disk changes.
    pub(super) fn agent_remove_workspace(&self) {
        let Some(task) = self.menu_task().filter(|t| t.folder) else {
            return;
        };
        let (worktrees, sessions) = self.state().map_or((0, Vec::new()), |state| {
            let tasks = &state.agents.tasks;
            let family = state.agents.family(&task.id());
            let sessions = (0..state.terminal.tabs.len())
                .filter(|i| {
                    task_of(tasks, &state.terminal.tabs[*i]).is_some_and(|t| family.contains(&t))
                })
                .collect();
            (family.len().saturating_sub(1), sessions)
        });
        let rows = match worktrees {
            0 => String::new(),
            1 => "Its worktree leaves the list. ".to_owned(),
            n => format!("Its {n} worktrees leave the list. "),
        };
        let closed = match sessions.len() {
            0 => String::new(),
            1 => "1 session is closed. ".to_owned(),
            n => format!("{n} sessions are closed. "),
        };
        let detail = format!("{rows}{closed}The folders and their files stay as they are.");
        let yes = std::env::var_os("CRC_SELFTEST").is_some()
            || ask(
                MainThreadMarker::from(self),
                &format!("Remove the workspace {}?", task.name()),
                &detail,
                &["Remove", "Cancel"],
            ) == 0;
        if !yes {
            return;
        }
        if let Some(mut state) = self.state_mut() {
            for index in sessions.into_iter().rev() {
                state.terminal.close_tab(index);
            }
            state.agents.remove_folder(&task.id());
            if state.agents.selected_index().is_none() {
                state.agents.select(0);
            }
        }
        self.after_terminal_layout();
    }

    /// The palette's answer: a worktree on a new branch of the selected
    /// workspace's repository, then a terminal in it; on its host, over
    /// ssh, for a remote one.
    pub(super) fn create_task(&self, name: String) {
        let Some((host, dir)) = self.state().and_then(|state| state.agents.task_repo()) else {
            return;
        };
        self.make_task(host, dir, name, None);
    }

    /// A worktree on a new branch `name` of the repository at `dir`, on
    /// `host` when it is on one, then a terminal in it. With `claude` (what
    /// to tell it), Claude starts there instead of a shell, and a worktree
    /// already on that branch is gone back to rather than made again.
    pub(super) fn make_task(
        &self,
        host: Option<String>,
        dir: std::path::PathBuf,
        name: String,
        claude: Option<String>,
    ) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        state.agents.on = true;
        let branch = crate::project::worktree::branch_name(&name);
        state
            .agents
            .start(&format!("Making {branch}\u{2026}"), move || {
                let runner = crate::platform::agents::runner(host.as_deref());
                let list = crate::project::worktree::list_all_on(&runner, &dir)
                    .map(|(list, _)| list)
                    .unwrap_or_default();
                if claude.is_some()
                    && let Some(there) = crate::platform::agents::worktree_on_branch(&list, &branch)
                {
                    return Ok(Done {
                        said: format!("Task {branch} is open in its worktree"),
                        start_in: Some((host, there)),
                        claude,
                    });
                }
                let main = list.into_iter().find(|w| w.main).map_or(dir, |w| w.path);
                let made = crate::project::worktree::add_on(&runner, &main, &name)?;
                Ok(Done {
                    said: format!("Task {branch} ready in its own worktree"),
                    start_in: Some((host, made)),
                    claude,
                })
            });
        drop(state);
        self.after_terminal_layout();
        self.resume_display_link();
    }

    fn task_menu(&self, index: usize) -> Retained<NSMenu> {
        let mtm = MainThreadMarker::from(self);
        let menu = context_menu_new(mtm);
        let (main, folder, remote, branch, base, pr, merged, prunable) =
            self.state()
                .map_or((true, false, false, None, None, None, false, 0), |state| {
                    let task = state.agents.tasks.get(index);
                    // Worktrees Git still records whose folder is gone, in the
                    // repositories a folder workspace holds.
                    let prunable = task.filter(|t| t.folder).map_or(0, |t| {
                        state.agents.repos_of(t).map_or(0, |repos| {
                            repos
                                .iter()
                                .filter_map(|r| {
                                    state.agents.prunable.get(&(t.host.clone(), r.clone()))
                                })
                                .sum()
                        })
                    });
                    (
                        task.is_none_or(|t| t.main),
                        task.is_some_and(|t| t.folder),
                        // Nothing for the editor: on a host, or no folder.
                        task.is_some_and(|t| t.host.is_some() || t.folderless()),
                        // The branch, and whether it can have pull requests:
                        // a host's repository not on GitHub has none here.
                        task.and_then(|t| {
                            Some((t.branch.clone()?, t.host.is_none() || t.github.is_some()))
                        }),
                        task.and_then(|t| t.base.clone()),
                        task.and_then(|t| state.agents.info.get(&t.id()))
                            .and_then(|info| info.pull_request.as_ref())
                            .map(|pr| pr.number),
                        task.is_some_and(|t| state.agents.is_merged(t)),
                        prunable,
                    )
                });
        let pinned = self.state().is_some_and(|state| {
            state
                .agents
                .tasks
                .get(index)
                .is_some_and(|t| state.agents.is_pinned(&t.id()))
        });
        let add = |title: &str, action: Sel| menu.addItem(&menu_item(mtm, title, action));
        add("New Terminal", sel!(agentNewShell:));
        add("New Claude Session", sel!(agentNewClaude:));
        // A remote workspace's folder is on its host, not for the editor.
        if !remote {
            add("Open in Editor", sel!(agentOpenInEditor:));
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        add(
            if pinned { "Unpin" } else { "Pin to Top" },
            sel!(agentTogglePin:),
        );
        add("Rename", sel!(agentRenameInPlace:));
        if folder {
            if prunable > 0 {
                add(
                    &match prunable {
                        1 => "Prune 1 Missing Worktree".to_owned(),
                        n => format!("Prune {n} Missing Worktrees"),
                    },
                    sel!(agentPrune:),
                );
            }
            add("Remove Workspace\u{2026}", sel!(agentRemoveWorkspace:));
        }
        if !folder {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            add("Commit All\u{2026}", sel!(agentCommitAll:));
            add("Pull", sel!(agentPull:));
            add("Push", sel!(agentPush:));
        }
        if !main && let Some((branch, github)) = branch {
            let into = base.unwrap_or_else(|| "the main checkout".into());
            add(&format!("Merge {into} In"), sel!(agentUpdateFromBase:));
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            if !remote {
                add(&format!("Merge {branch} into {into}"), sel!(agentMerge:));
            }
            match pr {
                _ if !github => {}
                Some(number) => {
                    add(
                        &format!("Open Pull Request #{number}"),
                        sel!(agentOpenPullRequest:),
                    );
                    let merge = menu_item(
                        mtm,
                        &format!("Merge Pull Request #{number}"),
                        sel!(agentMergeSquash:),
                    );
                    merge.setSubmenu(Some(&self.merge_strategy_menu()));
                    menu.addItem(&merge);
                }
                None => add("Create Pull Request", sel!(agentCreatePullRequest:)),
            }
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            add(
                if merged {
                    "Clean Up\u{2026}"
                } else {
                    "Remove Task\u{2026}"
                },
                sel!(agentRemove:),
            );
        }
        menu
    }

    /// The workspace the menu was opened on.
    pub(super) fn menu_task(&self) -> Option<crate::platform::agents::Workspace> {
        let state = self.state()?;
        let id = state.agents.menu_task.as_deref()?;
        state.agents.tasks.iter().find(|t| t.id() == id).cloned()
    }

    pub(super) fn agent_menu_new_session(&self, claude: bool) {
        let Some(task) = self.menu_task() else {
            return;
        };
        if let Some(mut state) = self.state_mut() {
            state.agents.selected = Some(task.id());
        }
        self.set_agents_mode(true);
        self.spawn_terminal_in(claude, Some(&task.dir()));
    }

    /// The task's folder as the editor's project. Its sessions keep
    /// running; Cmd-Shift-A comes back to them.
    pub(super) fn agent_open_in_editor(&self) {
        let Some(task) = self.menu_task() else {
            return;
        };
        if let Some(mut state) = self.state_mut() {
            state.agents.selected = Some(task.id());
        }
        self.set_agents_mode(false);
    }

    /// Agents leads: the editor opens on the selected workspace's folder,
    /// so what it shows is what the agents are working in. A folder gone
    /// from disk, or too broad to index, leaves the project as it was, and
    /// so does a remote workspace, whose folder is on its host.
    fn editor_follows_agents(&self) {
        let path = self.state().and_then(|state| {
            let task = &state.agents.tasks[state.agents.selected_index()?];
            if task.host.is_some() || task.folderless() {
                return None;
            }
            let path = crate::platform::canonical(&task.path);
            let root = state.tree.root().map(crate::platform::canonical);
            (root.as_deref() != Some(path.as_path()) && path.is_dir() && !too_broad_to_adopt(&path))
                .then_some(path)
        });
        if let Some(path) = path {
            self.load_folder_path(&path.to_string_lossy());
        }
    }

    pub(super) fn agent_merge(&self) {
        let Some(task) = self.menu_task() else {
            return;
        };
        let Some(branch) = task.branch.clone() else {
            return;
        };
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some(main) = task.repo.clone() else {
            return;
        };
        state
            .agents
            .start(&format!("Merging {branch}\u{2026}"), move || {
                Ok(Done {
                    said: crate::project::worktree::merge(&main, &branch)?,
                    start_in: None,
                    claude: None,
                })
            });
        drop(state);
        self.resume_display_link();
        self.request_redraw();
    }

    /// The task's pull request, in the browser.
    pub(super) fn agent_open_pull_request(&self) {
        let url = self.menu_task().and_then(|task| {
            let state = self.state()?;
            let pr = state.agents.info.get(&task.id())?.pull_request.as_ref()?;
            Some(pr.url.clone())
        });
        if let Some(url) = url.filter(|u| u.starts_with("https://"))
            && let Err(e) = self.open(false, std::ffi::OsStr::new(&url))
            && let Some(mut state) = self.state_mut()
        {
            state.say(
                layout::Feedback::Failure,
                format!("Could not open {url}: {e}"),
            );
        }
    }

    /// The inspector's Clean up on a merged worktree, by id: Remove Task
    /// on it.
    pub(super) fn clean_up_worktree(&self, id: &str) {
        if let Some(mut state) = self.state_mut() {
            state.agents.menu_task = Some(id.to_owned());
        }
        self.agent_remove();
    }

    /// The merged group's Clean Up All Merged: Clean up on each merged
    /// worktree under the folder workspace, asked once. One with
    /// uncommitted changes is skipped, as Clean up refuses it, and counted.
    pub(super) fn agent_clean_up_merged(&self) {
        let Some(parent) = self.menu_task() else {
            return;
        };
        let found: Vec<_> = self.state().map_or_else(Vec::new, |state| {
            let agents = &state.agents;
            agents
                .merged_under(&parent.id())
                .into_iter()
                .filter_map(|i| {
                    let t = &agents.tasks[i];
                    let pr_head = agents
                        .info
                        .get(&t.id())
                        .and_then(|i| i.pull_request.as_ref())
                        .filter(|pr| pr.state == "MERGED")
                        .map(|pr| pr.head_oid.clone());
                    Some((
                        t.id(),
                        t.worktree()?,
                        t.repo.clone()?,
                        t.base.clone(),
                        pr_head,
                    ))
                })
                .collect()
        });
        if found.is_empty() {
            return;
        }
        let worktrees = |n: usize| {
            if n == 1 {
                "1 merged worktree".to_owned()
            } else {
                format!("{n} merged worktrees")
            }
        };
        let yes = if std::env::var_os("CRC_SELFTEST").is_some() {
            true
        } else {
            ask(
                MainThreadMarker::from(self),
                &format!("Clean up {}?", worktrees(found.len())),
                "Their sessions are closed, and their folders and branches deleted. One with uncommitted changes is skipped.",
                &["Clean Up", "Cancel"],
            ) == 0
        };
        if !yes {
            return;
        }
        let Some(mut state) = self.state_mut() else {
            return;
        };
        // A host's are checked and removed there, over ssh.
        let runner = crate::platform::agents::runner(parent.host.as_deref());
        // Dirty worktrees are skipped before anything closes.
        let mut skipped = 0;
        let mut clean = Vec::new();
        for item in found {
            match crate::project::worktree::is_dirty_on(&runner, &item.1.path) {
                Ok(false) => clean.push(item),
                Ok(true) | Err(_) => skipped += 1,
            }
        }
        if clean.is_empty() {
            state.say(
                layout::Feedback::Failure,
                format!(
                    "Nothing cleaned up: {} uncommitted changes",
                    if skipped == 1 {
                        "1 merged worktree has".to_owned()
                    } else {
                        format!("{skipped} merged worktrees have")
                    }
                ),
            );
            drop(state);
            self.request_redraw();
            return;
        }
        let ids: Vec<String> = clean.iter().map(|c| c.0.clone()).collect();
        let sessions: Vec<usize> = {
            let tasks = &state.agents.tasks;
            (0..state.terminal.tabs.len())
                .filter(|i| {
                    let tab = &state.terminal.tabs[*i];
                    task_of(tasks, tab).is_some_and(|t| ids.contains(&tasks[t].id()))
                        || (parent.host.is_none()
                            && clean.iter().any(|c| tab.folder.starts_with(&c.1.path)))
                })
                .collect()
        };
        for index in sessions.into_iter().rev() {
            state.terminal.close_tab(index);
        }
        if state
            .agents
            .selected
            .as_ref()
            .is_some_and(|s| ids.contains(s))
        {
            state.agents.selected = Some(parent.id());
        }
        let count = clean.len();
        state.agents.start(
            &format!("Cleaning up {}\u{2026}", worktrees(count)),
            move || {
                let failed: Vec<String> = clean
                    .iter()
                    .filter_map(|(_, tree, main, base, pr_head)| {
                        crate::project::worktree::remove_on(
                            &runner,
                            main,
                            tree,
                            base.as_deref(),
                            pr_head.as_deref(),
                        )
                        .err()
                    })
                    .collect();
                let mut said = format!("Cleaned up {}", worktrees(count - failed.len()));
                if skipped > 0 {
                    said.push_str(&format!("; skipped {skipped} with uncommitted changes"));
                }
                if failed.is_empty() {
                    Ok(Done {
                        said,
                        start_in: None,
                        claude: None,
                    })
                } else {
                    Err(format!("{said}; {}", failed.join("; ")))
                }
            },
        );
        drop(state);
        self.after_terminal_layout();
        self.resume_display_link();
    }

    /// A folder workspace's Prune: Git forgets the worktrees of its
    /// repositories whose folder is gone.
    pub(super) fn agent_prune(&self) {
        let Some(task) = self.menu_task() else {
            return;
        };
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let mains: Vec<std::path::PathBuf> = state
            .agents
            .repos_of(&task)
            .unwrap_or_default()
            .into_iter()
            .filter(|r| {
                state
                    .agents
                    .prunable
                    .contains_key(&(task.host.clone(), r.clone()))
            })
            .collect();
        if mains.is_empty() {
            return;
        }
        let host = task.host.clone();
        state.agents.start("Pruning\u{2026}", move || {
            let runner = crate::platform::agents::runner(host.as_deref());
            let said: Vec<String> = mains
                .iter()
                .map(|main| crate::project::worktree::prune_on(&runner, main))
                .collect::<Result<_, _>>()?;
            Ok(Done {
                said: said.join("; "),
                start_in: None,
                claude: None,
            })
        });
        drop(state);
        self.resume_display_link();
        self.request_redraw();
    }

    /// Removes the task's worktree once asked: its sessions close, Git
    /// refuses while it has uncommitted changes, and an unmerged branch is
    /// kept. A merged one's branch goes too (Clean up).
    pub(super) fn agent_remove(&self) {
        let Some(id) = self.menu_task().map(|t| t.id()) else {
            return;
        };
        let Some(worktree) = self.menu_task().and_then(|t| t.worktree()) else {
            return;
        };
        let task = worktree;
        // A host's worktree is checked and removed there, over ssh; no
        // folder of this Mac is its.
        let host = self.menu_task().and_then(|t| t.host);
        let sessions: Vec<usize> = self.state().map_or_else(Vec::new, |state| {
            let tasks = &state.agents.tasks;
            (0..state.terminal.tabs.len())
                .filter(|i| {
                    task_of(tasks, &state.terminal.tabs[*i]).map(|t| tasks[t].id())
                        == Some(id.clone())
                        || (host.is_none()
                            && state.terminal.tabs[*i].folder.starts_with(&task.path))
                })
                .collect()
        });
        // Merged: the default branch to check it against again, and the
        // head of its merged pull request, read now.
        let (merged, base, pr_head) = self
            .menu_task()
            .and_then(|t| {
                let state = self.state()?;
                let pr_head = state
                    .agents
                    .info
                    .get(&t.id())
                    .and_then(|i| i.pull_request.as_ref())
                    .filter(|pr| pr.state == "MERGED")
                    .map(|pr| pr.head_oid.clone());
                Some((state.agents.is_merged(&t), t.base, pr_head))
            })
            .unwrap_or_default();
        let detail = if merged {
            "Its folder is deleted and, since it is merged, its branch too. Git refuses if it has uncommitted changes.".to_owned()
        } else {
            match sessions.len() {
                0 => "Its folder is deleted. Git refuses if it has uncommitted changes; an unmerged branch is kept.".to_owned(),
                1 => "Its session is closed and its folder deleted. Git refuses if it has uncommitted changes; an unmerged branch is kept.".to_owned(),
                n => format!("Its {n} sessions are closed and its folder deleted. Git refuses if it has uncommitted changes; an unmerged branch is kept."),
            }
        };
        let (title, button) = if merged {
            (format!("Clean up {}?", task.name()), "Clean Up")
        } else {
            (format!("Remove the task {}?", task.name()), "Remove")
        };
        let yes = if std::env::var_os("CRC_SELFTEST").is_some() {
            true
        } else {
            ask(
                MainThreadMarker::from(self),
                &title,
                &detail,
                &[button, "Cancel"],
            ) == 0
        };
        if !yes {
            return;
        }
        let repo = self.menu_task().and_then(|t| t.repo);
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some(main) = repo else {
            return;
        };
        let runner = crate::platform::agents::runner(host.as_deref());
        // Dirty worktrees are refused before anything closes.
        match crate::project::worktree::is_dirty_on(&runner, &task.path) {
            Ok(false) => {}
            Ok(true) => {
                state.say(
                    layout::Feedback::Failure,
                    format!(
                        "{} has uncommitted changes; commit or discard them first",
                        task.name()
                    ),
                );
                drop(state);
                self.request_redraw();
                return;
            }
            Err(e) => {
                state.say(layout::Feedback::Failure, e);
                drop(state);
                self.request_redraw();
                return;
            }
        }
        for index in sessions.into_iter().rev() {
            state.terminal.close_tab(index);
        }
        if state.agents.selected.as_deref() == Some(id.as_str()) {
            // The folder workspace holding its repository, if one does.
            let folders: Vec<_> = state
                .agents
                .tasks
                .iter()
                .filter(|t| t.folder)
                .cloned()
                .collect();
            state.agents.selected = match &host {
                // The folder workspace on its host whose row it was under.
                Some(_) => state.agents.parent_of_id(&id),
                None => {
                    crate::platform::agents::folder_task(&folders, &main).map(|i| folders[i].id())
                }
            };
        }
        let name = task.name();
        state
            .agents
            .start(&format!("Removing {name}\u{2026}"), move || {
                Ok(Done {
                    said: crate::project::worktree::remove_on(
                        &runner,
                        &main,
                        &task,
                        base.as_deref(),
                        pr_head.as_deref(),
                    )?,
                    start_in: None,
                    claude: None,
                })
            });
        drop(state);
        self.after_terminal_layout();
        self.resume_display_link();
    }
}

/// Whether the selection is a workspace not in the list yet that the
/// terminal showing is in: a worktree just made on a host, whose row comes
/// when the host's list is read again. It stays selected until then.
fn selection_coming(state: &State) -> bool {
    state.agents.selected.is_some()
        && state.agents.selected_index().is_none()
        && state
            .terminal
            .active_tab()
            .and_then(|t| t.workspace.as_ref())
            == state.agents.selected.as_ref()
}

/// Whether the session at the front is not the selected workspace's, in
/// Agents mode: that workspace has no terminal left, and its empty state
/// shows, so nothing typed or Cmd-W reaches the one behind it. Not while
/// a workspace about to be listed is selected (see [`selection_coming`]).
pub(super) fn active_hidden(state: &State) -> bool {
    let tasks = &state.agents.tasks;
    state.agents.on
        && !tasks.is_empty()
        && !selection_coming(state)
        && state
            .terminal
            .active_tab()
            .is_some_and(|t| task_of(tasks, t) != state.agents.selected_index())
}

/// A terminal into the workspace `id`, its holder told too so a relaunch
/// finds it there.
fn adopt(tab: &mut crate::platform::terminal::Tab, id: &str) {
    tab.workspace = Some(id.to_owned());
    if let (Some(dir), Some(held)) = (crate::term::hold::dir(), &tab.held) {
        crate::term::hold::set_workspace(&dir, held, id);
    }
}

/// The `number`th of `items` (1 based), 9 the last, as browsers have it.
fn pick_numbered(items: &[usize], number: usize) -> Option<usize> {
    if number == 9 {
        items.last().copied()
    } else {
        items.get(number.checked_sub(1)?).copied()
    }
}

/// A folder workspace a terminal left by `cd`, leaving it with none, goes.
/// One whose last terminal closed stays, as you made it. A worktree stays:
/// forgetting it would mean removing it from Git, which its menu asks
/// about first.
pub(super) fn forget_emptied(state: &mut State) {
    // The tab showing closed: the next one of its workspace shows, the one
    // after it or else the one before; with none left, that workspace stays
    // selected, empty. Never another workspace's.
    if let Some((id, folder, at)) = state.terminal.closed_active.take()
        && state.agents.on
    {
        let tasks = state.agents.tasks.clone();
        if let Some(owner) = crate::platform::agents::owner(&tasks, id.as_deref(), &folder) {
            let owners: Vec<Option<usize>> = state
                .terminal
                .tabs
                .iter()
                .map(|t| task_of(&tasks, t))
                .collect();
            if let Some(next) = crate::platform::agents::after_close(&owners, owner, at) {
                state.terminal.show(next);
            }
            state.agents.selected = Some(tasks[owner].id());
        }
    }
    if state.terminal.closed_in.is_empty() {
        return;
    }
    let closed = std::mem::take(&mut state.terminal.closed_in);
    let mut gone = false;
    for id in closed {
        // A remote workspace stays: its host and folder were typed, and a
        // dropped connection ends its terminal without asking.
        if state
            .agents
            .by_id(&id)
            .is_some_and(|i| state.agents.tasks[i].host.is_some())
        {
            continue;
        }
        // Not while a worktree under it has a terminal: it would go too.
        let family = state.agents.family(&id);
        if family.is_empty() {
            continue;
        }
        let tasks = &state.agents.tasks;
        let held = state
            .terminal
            .tabs
            .iter()
            .any(|tab| task_of(tasks, tab).is_some_and(|t| family.contains(&t)));
        if !held {
            state.agents.remove_folder(&id);
            gone = true;
        }
    }
    if gone && state.agents.selected_index().is_none() {
        // The workspace of the terminal showing, else the first.
        let tasks = state.agents.tasks.clone();
        let next = state
            .terminal
            .active_tab()
            .and_then(|t| task_of(&tasks, t))
            .or((!tasks.is_empty()).then_some(0));
        state.agents.selected = next.map(|i| tasks[i].id());
    }
}

/// The number of sessions waiting on you, on the Dock icon (nothing when
/// none is) and the menu bar item.
pub(super) fn sync_dock_badge(state: &State, mtm: MainThreadMarker) {
    let waiting = super::menubar::waiting_count(state.terminal.tabs.iter().map(|t| t.activity()));
    let tile = NSApplication::sharedApplication(mtm).dockTile();
    let label = (waiting > 0).then(|| NSString::from_str(&waiting.to_string()));
    tile.setBadgeLabel(label.as_deref());
    super::menubar::sync_status_item(waiting, mtm);
}
