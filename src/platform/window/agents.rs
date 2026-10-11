//! Agents mode in the window: the switch, the workspace list's clicks and
//! menus, and the work on tasks (new worktree, merge, push, remove) on
//! workers.

use super::*;
use crate::platform::agents::{Action, Done, task_of};
use crate::platform::terminal::Activity;

impl EditorView {
    /// Editor or Agents. Agents gives the terminal the column and the
    /// sidebar to the workspaces, starting a shell when nothing runs yet;
    /// the editor gets back its panel as it was.
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
            if on {
                state.agents.editor_terminal = state.terminal.open;
                state.terminal.open = true;
                state.terminal.focus = true;
                state.sidebar_keys = false;
                state.palette = None;
                state.completion = None;
                let dir = state
                    .tree
                    .root()
                    .map(Path::to_path_buf)
                    .or_else(|| state.terminal.active_tab().map(|t| t.folder.clone()));
                if let Some(dir) = &dir {
                    state.agents.refresh(dir);
                }
                if state.agents.selected_index().is_none() {
                    let tasks = &state.agents.tasks;
                    state.agents.selected = state
                        .terminal
                        .active_tab()
                        .and_then(|t| task_of(tasks, t))
                        .map(|i| tasks[i].id());
                }
                state.terminal.tabs.is_empty()
            } else {
                state.terminal.open =
                    state.agents.editor_terminal && !state.terminal.tabs.is_empty();
                state.terminal.focus = false;
                false
            }
        };
        if start {
            self.spawn_terminal(false);
        }
        if on {
            self.arm_agents_tick();
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

    pub(super) fn poll_agents(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let mut changed = state.agents.poll();
        if changed && state.agents.selected_index().is_none() {
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
        let done = state.agents.take_done();
        let mut start_in = None;
        if let Some(done) = done {
            changed = true;
            match done {
                Ok(Done {
                    said,
                    start_in: dir,
                }) => {
                    state.say(layout::Feedback::Success, said);
                    start_in = dir;
                }
                Err(e) => state.say(layout::Feedback::Failure, e),
            }
            if let Some(dir) = state
                .agents
                .repo
                .clone()
                .or_else(|| state.tree.root().map(Path::to_path_buf))
            {
                state.agents.refresh(&dir);
            }
            state.git.refresh();
        }
        drop(state);
        if ensure {
            self.ensure_workspace();
        }
        self.want_inspector();
        if let Some(dir) = start_in {
            // A worktree's id is its path: selectable before the list
            // comes back with it.
            if let Some(mut state) = self.state_mut() {
                state.agents.selected = Some(
                    crate::platform::canonical(&dir)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            self.spawn_terminal_in(false, Some(&dir));
        }
        if changed {
            self.request_redraw();
        }
    }

    /// Every terminal lives in a workspace. Once the list is known, the
    /// selection is the workspace holding the open folder, and with none
    /// (a folder outside Git, nothing made yet) one is made on the folder,
    /// named after it. Returns whether a workspace is selected.
    pub(super) fn ensure_workspace(&self) -> bool {
        let folder = self.workspace_folder();
        let Some(mut state) = self.state_mut() else {
            return false;
        };
        if !state.agents.listed() {
            return false;
        }
        if state.agents.selected_index().is_some() {
            return true;
        }
        let Some(folder) = folder else {
            return false;
        };
        let folder = crate::platform::canonical(&folder);
        let tasks = state.agents.tasks.clone();
        let found = state
            .terminal
            .active_tab()
            .and_then(|t| task_of(&tasks, t))
            .or_else(|| crate::platform::agents::folder_task(&tasks, &folder));
        let id = match found {
            Some(i) => tasks[i].id(),
            None => {
                let name = folder
                    .file_name()
                    .map_or_else(|| "workspace".into(), |n| n.to_string_lossy().into_owned());
                let id = state.agents.add_folder(&name, folder);
                // The sessions already running in the folder are its own.
                for tab in &mut state.terminal.tabs {
                    if tab.workspace.is_none() {
                        tab.workspace = Some(id.clone());
                    }
                }
                id
            }
        };
        state.agents.selected = Some(id);
        true
    }

    /// A double-click on the empty part of the list: a new folder workspace
    /// on the open folder, named after it, with a terminal in it.
    fn new_blank_workspace(&self) {
        let Some(folder) = self.workspace_folder() else {
            return;
        };
        let folder = crate::platform::canonical(&folder);
        let name = folder
            .file_name()
            .map_or_else(|| "workspace".into(), |n| n.to_string_lossy().into_owned());
        if let Some(mut state) = self.state_mut() {
            let id = state.agents.add_folder(&name, folder.clone());
            state.agents.selected = Some(id);
        }
        self.spawn_terminal_in(false, Some(&folder));
    }

    pub(super) fn agents_click(&self, event: &NSEvent, x: f32, y: f32) {
        let Some(action) = self.state().and_then(|state| state.agents.hit(x, y)) else {
            return;
        };
        match action {
            Action::NewTask => self.new_task_prompt(),
            Action::Refresh => {
                if let Some(mut state) = self.state_mut()
                    && let Some(dir) = state
                        .agents
                        .repo
                        .clone()
                        .or_else(|| state.tree.root().map(Path::to_path_buf))
                {
                    state.agents.refresh(&dir);
                }
                self.resume_display_link();
            }
            // A double-click on a folder workspace renames it.
            Action::Select(index) if event.clickCount() >= 2 => {
                let folder = self
                    .state()
                    .is_some_and(|state| state.agents.tasks.get(index).is_some_and(|t| t.folder));
                if folder {
                    if let Some(mut state) = self.state_mut() {
                        state.agents.menu_task = state.agents.tasks.get(index).map(|t| t.id());
                    }
                    self.agent_rename_workspace();
                }
            }
            Action::Select(index) => self.select_task(index),
            Action::Session(tab) => self.show_session(tab),
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
            Action::Menu(index) => {
                if let Some(mut state) = self.state_mut() {
                    state.agents.menu_task = state.agents.tasks.get(index).map(|t| t.id());
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
        state.agents.tasks.get(index).map(|t| t.path.clone())
    }

    /// Shows a task: its waiting session first, else the newest one it
    /// has, else a new shell in its folder.
    pub(super) fn select_task(&self, index: usize) {
        let found = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(task) = state.agents.tasks.get(index) else {
                return;
            };
            let path = task.path.clone();
            state.agents.select(index);
            let tasks = state.agents.tasks.clone();
            let mine: Vec<usize> = (0..state.terminal.tabs.len())
                .filter(|i| task_of(&tasks, &state.terminal.tabs[*i]) == Some(index))
                .collect();
            let pick = mine
                .iter()
                .copied()
                .find(|i| state.terminal.tabs[*i].activity() == Activity::Waiting)
                .or_else(|| {
                    mine.contains(&state.terminal.active)
                        .then_some(state.terminal.active)
                })
                .or_else(|| mine.last().copied());
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
    }

    /// Control-1 to 9: the task in that place.
    pub(super) fn jump_to_task(&self, number: usize) {
        let known = self
            .state()
            .is_some_and(|state| number >= 1 && number <= state.agents.tasks.len());
        if known {
            self.select_task(number - 1);
        } else {
            self.set_agents_mode(true);
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

    /// The palette, taking a new workspace's name: a folder workspace on
    /// the open folder, or (in a repository) a task on a new branch.
    pub(super) fn new_task_prompt(&self) {
        self.open_palette_with("");
        if let Some(mut state) = self.state_mut() {
            state.task_prompt = true;
            state.agents.renaming = None;
        }
        self.request_redraw();
    }

    /// The folder a new workspace is made on: the repository's main
    /// checkout, else the open folder, else the session showing's.
    fn workspace_folder(&self) -> Option<std::path::PathBuf> {
        let state = self.state()?;
        state
            .agents
            .repo
            .clone()
            .or_else(|| state.tree.root().map(Path::to_path_buf))
            .or_else(|| state.terminal.active_tab().map(|t| t.folder.clone()))
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
                            tab.workspace = Some(new_id.clone());
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
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.agents.on = true;
            let id = state.agents.add_folder(&name, dir.clone());
            state.agents.selected = Some(id);
        }
        self.set_agents_mode(true);
        self.spawn_terminal_in(false, Some(&dir));
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
            Some(Action::Select(index) | Action::NewSession(index) | Action::Menu(index)) => {
                if let Some(mut state) = self.state_mut() {
                    state.agents.menu_task = state.agents.tasks.get(index).map(|t| t.id());
                }
                self.task_menu(index)
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
                    tasks[index].path.clone()
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

    pub(super) fn refresh_agents(&self) {
        if let Some(mut state) = self.state_mut()
            && let Some(dir) = state
                .agents
                .repo
                .clone()
                .or_else(|| state.tree.root().map(Path::to_path_buf))
        {
            state.agents.refresh(&dir);
        }
        self.resume_display_link();
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

    /// Forgets a folder workspace once asked; its sessions close. Nothing
    /// on disk changes.
    pub(super) fn agent_remove_workspace(&self) {
        let Some(task) = self.menu_task().filter(|t| t.folder) else {
            return;
        };
        let id = task.id();
        let sessions: Vec<usize> = self.state().map_or_else(Vec::new, |state| {
            let tasks = &state.agents.tasks;
            (0..state.terminal.tabs.len())
                .filter(|i| {
                    task_of(tasks, &state.terminal.tabs[*i]).map(|t| tasks[t].id())
                        == Some(id.clone())
                })
                .collect()
        });
        let detail = match sessions.len() {
            0 => "The folder and its files stay as they are.".to_owned(),
            1 => "Its session is closed. The folder and its files stay as they are.".to_owned(),
            n => format!("Its {n} sessions are closed. The folder and its files stay as they are."),
        };
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

    /// The palette's answer: a worktree on a new branch, then Claude in it.
    pub(super) fn create_task(&self, name: String) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some(dir) = state
            .agents
            .repo
            .clone()
            .or_else(|| state.tree.root().map(Path::to_path_buf))
        else {
            return;
        };
        state.agents.on = true;
        let branch = crate::project::worktree::branch_name(&name);
        state
            .agents
            .start(&format!("Making {branch}\u{2026}"), move || {
                let main = crate::project::worktree::list(&dir)
                    .ok()
                    .and_then(|list| list.into_iter().find(|w| w.main))
                    .map_or(dir, |w| w.path);
                let made = crate::project::worktree::add(&main, &name)?;
                Ok(Done {
                    said: format!("Task {branch} ready in its own worktree"),
                    start_in: Some(made),
                })
            });
        drop(state);
        self.after_terminal_layout();
        self.resume_display_link();
    }

    fn task_menu(&self, index: usize) -> Retained<NSMenu> {
        let mtm = MainThreadMarker::from(self);
        let menu = context_menu_new(mtm);
        let (main, folder, branch, base, pr) =
            self.state()
                .map_or((true, false, None, None, None), |state| {
                    let task = state.agents.tasks.get(index);
                    (
                        task.is_none_or(|t| t.main),
                        task.is_some_and(|t| t.folder),
                        task.and_then(|t| t.branch.clone()),
                        state
                            .agents
                            .tasks
                            .iter()
                            .find(|t| t.main)
                            .and_then(|t| t.branch.clone()),
                        task.and_then(|t| state.agents.info.get(&t.path))
                            .and_then(|info| info.pull_request.as_ref())
                            .map(|pr| pr.number),
                    )
                });
        let add = |title: &str, action: Sel| menu.addItem(&menu_item(mtm, title, action));
        add("New Terminal", sel!(agentNewShell:));
        add("New Claude Session", sel!(agentNewClaude:));
        add("Open in Editor", sel!(agentOpenInEditor:));
        if folder {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            add("Rename Workspace\u{2026}", sel!(agentRenameWorkspace:));
            add("Remove Workspace\u{2026}", sel!(agentRemoveWorkspace:));
        }
        if !folder {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            add("Commit All\u{2026}", sel!(agentCommitAll:));
            add("Pull", sel!(agentPull:));
            add("Push", sel!(agentPush:));
        }
        if !main && let Some(branch) = branch {
            let into = base.unwrap_or_else(|| "the main checkout".into());
            add(&format!("Merge {into} In"), sel!(agentUpdateFromBase:));
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            add(&format!("Merge {branch} into {into}"), sel!(agentMerge:));
            match pr {
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
            add("Remove Task\u{2026}", sel!(agentRemove:));
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
        self.spawn_terminal_in(claude, Some(&task.path));
    }

    /// The task's folder as the editor's project. Its sessions keep
    /// running; Cmd-Shift-A comes back to them.
    pub(super) fn agent_open_in_editor(&self) {
        let Some(task) = self.menu_task() else {
            return;
        };
        self.set_agents_mode(false);
        let same = self
            .state()
            .and_then(|state| state.tree.root().map(crate::platform::canonical))
            .is_some_and(|root| root == task.path);
        if !same {
            self.load_folder_path(&task.path.to_string_lossy());
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
        let Some(main) = state.agents.repo.clone() else {
            return;
        };
        state
            .agents
            .start(&format!("Merging {branch}\u{2026}"), move || {
                Ok(Done {
                    said: crate::project::worktree::merge(&main, &branch)?,
                    start_in: None,
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
            let pr = state.agents.info.get(&task.path)?.pull_request.as_ref()?;
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

    /// Removes the task's worktree once asked: its sessions close, Git
    /// refuses while it has uncommitted changes, and an unmerged branch is
    /// kept.
    pub(super) fn agent_remove(&self) {
        let Some(id) = self.menu_task().map(|t| t.id()) else {
            return;
        };
        let Some(worktree) = self.menu_task().and_then(|t| t.worktree()) else {
            return;
        };
        let task = worktree;
        let sessions: Vec<usize> = self.state().map_or_else(Vec::new, |state| {
            let tasks = &state.agents.tasks;
            (0..state.terminal.tabs.len())
                .filter(|i| {
                    task_of(tasks, &state.terminal.tabs[*i]).map(|t| tasks[t].id())
                        == Some(id.clone())
                        || state.terminal.tabs[*i].folder.starts_with(&task.path)
                })
                .collect()
        });
        let detail = match sessions.len() {
            0 => "Its folder is deleted. Git refuses if it has uncommitted changes; an unmerged branch is kept.".to_owned(),
            1 => "Its session is closed and its folder deleted. Git refuses if it has uncommitted changes; an unmerged branch is kept.".to_owned(),
            n => format!("Its {n} sessions are closed and its folder deleted. Git refuses if it has uncommitted changes; an unmerged branch is kept."),
        };
        let yes = if std::env::var_os("CRC_SELFTEST").is_some() {
            true
        } else {
            ask(
                MainThreadMarker::from(self),
                &format!("Remove the task {}?", task.name()),
                &detail,
                &["Remove", "Cancel"],
            ) == 0
        };
        if !yes {
            return;
        }
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some(main) = state.agents.repo.clone() else {
            return;
        };
        // Dirty worktrees are refused before anything closes.
        match crate::project::worktree::is_dirty(&task.path) {
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
            state.agents.selected = Some(main.to_string_lossy().into_owned());
        }
        let name = task.name();
        state
            .agents
            .start(&format!("Removing {name}\u{2026}"), move || {
                Ok(Done {
                    said: crate::project::worktree::remove(&main, &task)?,
                    start_in: None,
                })
            });
        drop(state);
        self.after_terminal_layout();
        self.resume_display_link();
    }
}

/// The number of sessions waiting on you, on the Dock icon; nothing when
/// none is.
pub(super) fn sync_dock_badge(state: &State, mtm: MainThreadMarker) {
    let waiting = state
        .terminal
        .tabs
        .iter()
        .filter(|t| t.activity() == Activity::Waiting)
        .count();
    let tile = NSApplication::sharedApplication(mtm).dockTile();
    let label = (waiting > 0).then(|| NSString::from_str(&waiting.to_string()));
    tile.setBadgeLabel(label.as_deref());
}
