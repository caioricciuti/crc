//! The workspace inspector in the window: its clicks, and the Git and pull
//! request operations it and the workspace menu share. Each operation acts
//! on the workspace the menu was opened on (`Panel::menu_task`); the
//! inspector's buttons point that at the selected workspace first. They run
//! on workers through `Panel::start`, which reads the list again after.

use super::*;
use crate::platform::agents::Done;
use crate::platform::inspector::Action;
use crate::project::worktree::MergeStrategy;
use std::path::PathBuf;

/// How often the workspace list and pull requests are read again while
/// Agents mode is on.
const REFRESH_EVERY: f64 = 60.0;

impl EditorView {
    pub(super) fn inspector_click(&self, event: &NSEvent, x: f32, y: f32) {
        let Some(action) = self
            .state()
            .and_then(|state| state.agents.inspector.hit(x, y))
        else {
            return;
        };
        // The buttons act on the workspace shown.
        if let Some(mut state) = self.state_mut() {
            state.agents.menu_task = state.agents.selected.clone();
        }
        match action {
            Action::Close => self.toggle_inspector(),
            Action::Refresh => self.refresh_agents(),
            Action::CommitAll => self.agent_commit_prompt(),
            Action::Pull => self.agent_remote(crate::project::git::Remote::Pull),
            Action::Push => self.agent_remote(crate::project::git::Remote::Push),
            Action::UpdateFromBase => self.agent_update_from_base(),
            Action::CreatePullRequest => self.agent_create_pull_request(),
            Action::MergePullRequest => {
                let menu = self.merge_strategy_menu();
                layout::set_pressed(None);
                NSMenu::popUpContextMenu_withEvent_forView(&menu, event, self);
            }
            Action::OpenPullRequest => self.agent_open_pull_request(),
        }
        self.request_redraw();
        self.pump();
    }

    /// View > Workspace Inspector, and its close button.
    pub(super) fn toggle_inspector(&self) {
        if let Some(mut state) = self.state_mut() {
            state.agents.inspector.hidden = !state.agents.inspector.hidden;
        }
        self.after_terminal_layout();
        self.request_redraw();
    }

    /// Reads the selected workspace's status for the inspector, when it is
    /// showing and the one in hand is old or another workspace's.
    pub(super) fn want_inspector(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if !state.agents.on || state.agents.inspector.hidden {
            return;
        }
        let Some(task) = state
            .agents
            .selected_index()
            .map(|i| state.agents.tasks[i].clone())
        else {
            return;
        };
        let base = state.agents.base_branch();
        state.agents.inspector.want(&task.path, base, task.main);
    }

    /// Schedules the periodic read of the list, once.
    pub(super) fn arm_agents_tick(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if state.agents.ticking {
            return;
        }
        state.agents.ticking = true;
        drop(state);
        let _: () = unsafe {
            msg_send![self, performSelector: sel!(agentsTick:),
                withObject: None::<&AnyObject>, afterDelay: REFRESH_EVERY]
        };
    }

    /// The periodic read: worktrees, pull requests and the inspector's
    /// status, while Agents mode is on and nothing else is reading.
    pub(super) fn agents_tick(&self) {
        let on = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.agents.ticking = false;
            let due = state
                .agents
                .refreshed_at
                .is_none_or(|at| at.elapsed().as_secs_f64() >= REFRESH_EVERY - 1.0);
            if state.agents.on && due && !state.agents.busy() {
                let dir = state
                    .agents
                    .repo
                    .clone()
                    .or_else(|| state.tree.root().map(Path::to_path_buf));
                if let Some(dir) = dir {
                    state.agents.refresh(&dir);
                }
            }
            state.agents.on
        };
        if on {
            self.arm_agents_tick();
            self.resume_display_link();
        }
    }

    /// The popup under Merge: GitHub's three ways, each confirmed after.
    pub(super) fn merge_strategy_menu(&self) -> Retained<NSMenu> {
        let mtm = MainThreadMarker::from(self);
        let menu = context_menu_new(mtm);
        let add = |title: &str, action: Sel| menu.addItem(&menu_item(mtm, title, action));
        add("Squash and Merge", sel!(agentMergeSquash:));
        add("Create a Merge Commit", sel!(agentMergeCommit:));
        add("Rebase and Merge", sel!(agentMergeRebase:));
        menu
    }

    /// Runs `work` on the menu's workspace folder, saying `doing` until it
    /// answers.
    fn agent_operation(
        &self,
        doing: impl FnOnce(&crate::platform::agents::Workspace) -> String,
        work: impl FnOnce(PathBuf) -> Result<String, String> + Send + 'static,
    ) {
        let Some(task) = self.menu_task() else {
            return;
        };
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if state.agents.op.is_some() {
            state.say(
                layout::Feedback::Info,
                "Another workspace operation is still running",
            );
            drop(state);
            self.request_redraw();
            return;
        }
        let path = task.path.clone();
        state.agents.start(&doing(&task), move || {
            Ok(Done {
                said: work(path)?,
                start_in: None,
            })
        });
        drop(state);
        self.resume_display_link();
        self.request_redraw();
    }

    /// Pull (fast-forward only) or push, the way Source Control does them.
    pub(super) fn agent_remote(&self, what: crate::project::git::Remote) {
        let sock = self.state().and_then(|state| state.ssh_auth_sock.clone());
        self.agent_operation(
            |t| format!("{} {}\u{2026}", what.verb(), t.name()),
            move |path| crate::project::git::remote(&path, what, sock.as_deref()),
        );
    }

    /// Merges the main checkout's branch into the workspace's.
    pub(super) fn agent_update_from_base(&self) {
        let Some(base) = self.state().and_then(|state| state.agents.base_branch()) else {
            return;
        };
        let doing = format!("Merging {base}\u{2026}");
        self.agent_operation(
            |_| doing,
            move |path| crate::project::worktree::update_from(&path, &base),
        );
    }

    /// The palette, taking the message for committing everything in the
    /// menu's workspace.
    pub(super) fn agent_commit_prompt(&self) {
        let Some(task) = self.menu_task() else {
            return;
        };
        self.open_palette_with("");
        if let Some(mut state) = self.state_mut() {
            state.commit_prompt = Some(task.name());
        }
        self.request_redraw();
    }

    /// The palette's answer: stage everything and commit it.
    pub(super) fn agent_commit_all(&self, message: String) {
        self.agent_operation(
            |t| format!("Committing in {}\u{2026}", t.name()),
            move |path| crate::project::worktree::commit_all(&path, &message),
        );
    }

    /// Pushes the branch, then opens a pull request from its commits.
    pub(super) fn agent_create_pull_request(&self) {
        let sock = self.state().and_then(|state| state.ssh_auth_sock.clone());
        self.agent_operation(
            |t| format!("Opening a pull request for {}\u{2026}", t.name()),
            move |path| {
                crate::project::git::remote(
                    &path,
                    crate::project::git::Remote::Push,
                    sock.as_deref(),
                )?;
                crate::project::worktree::create_pull_request(&path)
            },
        );
    }

    /// Merges the menu's workspace's pull request on GitHub, once asked.
    pub(super) fn agent_merge_pull_request(&self, strategy: MergeStrategy) {
        let Some(task) = self.menu_task() else {
            return;
        };
        let Some(pr) = self.state().and_then(|state| {
            state
                .agents
                .info
                .get(&task.path)
                .and_then(|info| info.pull_request.clone())
        }) else {
            return;
        };
        let how = match strategy {
            MergeStrategy::Squash => format!(
                "Its {} squash into one commit on {}.",
                plural_commits(pr.commits),
                pr.base
            ),
            MergeStrategy::Merge => format!(
                "A merge commit on {} brings in its {}.",
                pr.base,
                plural_commits(pr.commits)
            ),
            MergeStrategy::Rebase => format!(
                "Its {} are replayed onto {} one by one.",
                plural_commits(pr.commits),
                pr.base
            ),
        };
        let yes = std::env::var_os("CRC_SELFTEST").is_some()
            || ask(
                MainThreadMarker::from(self),
                &format!("Merge pull request #{} into {}?", pr.number, pr.base),
                &format!(
                    "{how} This happens on GitHub and cannot be undone from crc. The branch and its worktree stay."
                ),
                &["Merge", "Cancel"],
            ) == 0;
        if !yes {
            return;
        }
        let number = pr.number;
        self.agent_operation(
            |_| format!("Merging #{number}\u{2026}"),
            move |path| crate::project::worktree::merge_pull_request(&path, number, strategy),
        );
    }
}

fn plural_commits(n: usize) -> String {
    if n == 1 {
        "1 commit".into()
    } else {
        format!("{n} commits")
    }
}
