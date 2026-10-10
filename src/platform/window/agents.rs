//! Agents mode in the window: the switch, the task list's clicks and menu,
//! and the work on tasks (new worktree, merge, push, remove) on workers.

use super::*;
use crate::platform::agents::{Action, Done, task_of};
use crate::platform::terminal::Activity;

impl EditorView {
    /// Editor or Agents. Agents gives the terminal the column and the
    /// sidebar to the tasks, starting Claude when nothing runs yet; the
    /// editor gets back its panel as it was.
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
                if state.agents.selected.is_none() {
                    state.agents.selected = state
                        .terminal
                        .active_tab()
                        .map(|t| t.folder.clone())
                        .or(dir.as_ref().map(|d| crate::platform::canonical(d)));
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
            self.spawn_terminal(true);
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
            // The selection is a folder; once the list is in, it is the
            // task that holds it.
            let tasks = state.agents.tasks.clone();
            state.agents.selected = state
                .agents
                .selected
                .as_deref()
                .and_then(|folder| task_of(&tasks, folder))
                .or((!tasks.is_empty()).then_some(0))
                .map(|i| tasks[i].path.clone());
        }
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
        if let Some(dir) = start_in {
            if let Some(mut state) = self.state_mut() {
                state.agents.selected = Some(crate::platform::canonical(&dir));
            }
            self.spawn_terminal_in(true, Some(&dir));
        }
        if changed {
            self.request_redraw();
        }
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
            Action::Select(index) => self.select_task(index),
            Action::Session(tab) => self.show_session(tab),
            Action::NewClaude(index) => {
                let Some(dir) = self.task_path(index) else {
                    return;
                };
                if let Some(mut state) = self.state_mut() {
                    state.agents.selected = Some(dir.clone());
                }
                self.spawn_terminal_in(true, Some(&dir));
            }
            Action::Menu(index) => {
                if let Some(mut state) = self.state_mut() {
                    state.agents.menu_task = Some(index);
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
    /// has, else a new Claude session in its folder.
    pub(super) fn select_task(&self, index: usize) {
        let found = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(task) = state.agents.tasks.get(index) else {
                return;
            };
            let path = task.path.clone();
            state.agents.selected = Some(path.clone());
            let tasks = state.agents.tasks.clone();
            let mine: Vec<usize> = (0..state.terminal.tabs.len())
                .filter(|i| task_of(&tasks, &state.terminal.tabs[*i].folder) == Some(index))
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
                self.spawn_terminal_in(true, Some(&path));
            }
        }
    }

    /// A session to the front, with the keyboard, in Agents mode.
    pub(super) fn show_session(&self, tab: usize) {
        if let Some(mut state) = self.state_mut() {
            if tab >= state.terminal.tabs.len() {
                return;
            }
            state.terminal.active = tab;
            state.terminal.back = 0;
            state.terminal.selection = None;
            let folder = state.terminal.tabs[tab].folder.clone();
            let tasks = state.agents.tasks.clone();
            if let Some(task) = task_of(&tasks, &folder) {
                state.agents.selected = Some(tasks[task].path.clone());
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

    /// The palette, taking a new task's name.
    pub(super) fn new_task_prompt(&self) {
        let has_repo = {
            let Some(state) = self.state() else {
                return;
            };
            state.agents.repo.is_some() || state.tree.root().is_some()
        };
        if !has_repo {
            if let Some(mut state) = self.state_mut() {
                state.say(
                    layout::Feedback::Failure,
                    "Open a Git repository first: a task is a branch of it",
                );
            }
            self.request_redraw();
            return;
        }
        self.open_palette_with("");
        if let Some(mut state) = self.state_mut() {
            state.task_prompt = true;
        }
        self.request_redraw();
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
        let (main, branch, base) = self.state().map_or((true, None, None), |state| {
            let task = state.agents.tasks.get(index);
            (
                task.is_none_or(|t| t.main),
                task.and_then(|t| t.branch.clone()),
                state
                    .agents
                    .tasks
                    .iter()
                    .find(|t| t.main)
                    .and_then(|t| t.branch.clone()),
            )
        });
        let add = |title: &str, action: Sel| menu.addItem(&menu_item(mtm, title, action));
        add("New Claude Session", sel!(agentNewClaude:));
        add("New Shell", sel!(agentNewShell:));
        add("Open in Editor", sel!(agentOpenInEditor:));
        if !main && let Some(branch) = branch {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            let into = base.unwrap_or_else(|| "the main checkout".into());
            add(&format!("Merge {branch} into {into}"), sel!(agentMerge:));
            add("Push and Open Pull Request", sel!(agentPushPullRequest:));
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            add("Remove Task\u{2026}", sel!(agentRemove:));
        }
        menu
    }

    /// The task the menu was opened on.
    fn menu_task(&self) -> Option<crate::project::worktree::Worktree> {
        let state = self.state()?;
        state.agents.tasks.get(state.agents.menu_task?).cloned()
    }

    pub(super) fn agent_menu_new_session(&self, claude: bool) {
        let Some(task) = self.menu_task() else {
            return;
        };
        if let Some(mut state) = self.state_mut() {
            state.agents.selected = Some(task.path.clone());
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

    /// Pushes the task's branch and opens a pull request with `gh`, typed
    /// into a shell of its own so sign-in prompts and answers show.
    pub(super) fn agent_push_pull_request(&self) {
        let Some(task) = self.menu_task() else {
            return;
        };
        if let Some(mut state) = self.state_mut() {
            state.agents.selected = Some(task.path.clone());
        }
        self.set_agents_mode(true);
        self.spawn_terminal_in(false, Some(&task.path));
        if let Some(mut state) = self.state_mut()
            && let Some(tab) = state.terminal.active_tab_mut()
        {
            tab.session
                .write(b"git push -u origin HEAD && gh pr create --fill\n");
        }
    }

    /// Removes the task's worktree once asked: its sessions close, Git
    /// refuses while it has uncommitted changes, and an unmerged branch is
    /// kept.
    pub(super) fn agent_remove(&self) {
        let Some(task) = self.menu_task() else {
            return;
        };
        let sessions: Vec<usize> = self.state().map_or_else(Vec::new, |state| {
            (0..state.terminal.tabs.len())
                .filter(|i| state.terminal.tabs[*i].folder.starts_with(&task.path))
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
        if state.agents.selected.as_deref() == Some(task.path.as_path()) {
            state.agents.selected = Some(main.clone());
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
