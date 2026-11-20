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

/// How often the periodic read ticks with crc in the background: the
/// list, and the pull requests that are due, every five minutes instead of
/// every few seconds, so badges still move on.
const AWAY_EVERY: f64 = 300.0;
/// How long after a push is noticed its pull request is read: a push
/// moves several refs, and GitHub needs a moment to see it.
const PUSH_SETTLE: f64 = 2.0;
/// How often the terminals are looked at for ssh, in Agents mode.
const SESSIONS_EVERY: f64 = 2.0;
/// The most commits a handoff lists: the newest.
const HANDOFF_COMMITS: usize = 50;

/// The most files a restore's question names; the rest are counted.
const RESTORE_NAMED: usize = 8;

/// `names` joined, the first [`RESTORE_NAMED`] of them, and how many more.
fn names(names: impl Iterator<Item = String>) -> String {
    let all: Vec<String> = names.collect();
    let mut said = all
        .iter()
        .take(RESTORE_NAMED)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if all.len() > RESTORE_NAMED {
        said.push_str(&format!(" and {} more", all.len() - RESTORE_NAMED));
    }
    said
}

/// "1 file", "2 files".
fn plural_files(n: usize) -> String {
    if n == 1 {
        "1 file".to_owned()
    } else {
        format!("{n} files")
    }
}

impl EditorView {
    pub(super) fn inspector_click(&self, event: &NSEvent, x: f32, y: f32) {
        let Some(action) = self
            .state()
            .and_then(|state| state.agents.inspector.hit(x, y))
        else {
            return;
        };
        let (action, repo) = action;
        // An issue row, Show all, Load Issues and its hide, a repository's
        // fold and its Check: in the repository (for the issues a main
        // checkout) on the host of the workspace shown.
        if let (
            Action::OpenIssue(_)
            | Action::ToggleIssues
            | Action::LoadIssues
            | Action::HideIssues
            | Action::FoldRepo
            | Action::CheckRemote,
            Some(main),
        ) = (action, &repo)
        {
            let host = self.state().and_then(|state| {
                let i = state.agents.selected_index()?;
                state.agents.tasks[i].host.clone()
            });
            let key = (host, main.clone());
            match action {
                Action::OpenIssue(number) => self.open_issue(key, number),
                Action::LoadIssues => {
                    if let Some(mut state) = self.state_mut() {
                        state.agents.load_issues(key);
                    }
                    self.resume_display_link();
                }
                Action::HideIssues => {
                    if let Some(mut state) = self.state_mut() {
                        state.agents.hide_issues(&key);
                    }
                }
                // A fetch on a worker of its own, beside any operation.
                Action::CheckRemote => {
                    if let Some(mut state) = self.state_mut() {
                        let sock = state.ssh_auth_sock.clone();
                        state.agents.inspector.check_remote(key, sock);
                    }
                    self.resume_display_link();
                }
                // Opened, it is read again now; folded, it is left out.
                Action::FoldRepo => {
                    if let Some(mut state) = self.state_mut() {
                        state.agents.toggle_repo_folded(key);
                    }
                    self.want_inspector();
                    self.resume_display_link();
                }
                _ => {
                    if let Some(mut state) = self.state_mut() {
                        state.agents.inspector.toggle_issues(main);
                    }
                }
            }
            self.request_redraw();
            self.pump();
            return;
        }
        // A worktree strip's buttons act on that worktree's workspace: one
        // on the host of the workspace shown, when it is remote.
        if let (
            Action::OpenWorktree | Action::WorktreeTerminal | Action::CleanUpWorktree,
            Some(path),
        ) = (action, &repo)
        {
            let host = self.state().and_then(|state| {
                let i = state.agents.selected_index()?;
                state.agents.tasks[i].host.clone()
            });
            let id = crate::platform::agents::row_id(host.as_deref(), path);
            match action {
                Action::OpenWorktree => self.open_worktree(&id),
                Action::CleanUpWorktree => self.clean_up_worktree(&id),
                _ => self.worktree_terminal(&id),
            }
            self.request_redraw();
            self.pump();
            return;
        }
        // Rewind's title folds it, a turn's row selects it, and a file
        // under it opens at the turn's version.
        if matches!(
            action,
            Action::FoldRewind | Action::RewindTurn(_) | Action::RewindFile | Action::RewindRestore
        ) {
            match (action, repo) {
                (Action::RewindFile, Some(path)) => self.rewind_file(path),
                (Action::RewindRestore, _) => self.rewind_restore(),
                (Action::RewindTurn(n), _) => {
                    if let Some(mut state) = self.state_mut() {
                        state.agents.inspector.select_turn(n);
                    }
                }
                (Action::FoldRewind, _) => {
                    if let Some(mut state) = self.state_mut() {
                        let inspector = &mut state.agents.inspector;
                        inspector.rewind_folded = !inspector.rewind_folded;
                    }
                }
                _ => {}
            }
            self.request_redraw();
            self.pump();
            return;
        }
        // The buttons act on the workspace shown, in the repository whose
        // card they are on.
        if let Some(mut state) = self.state_mut() {
            state.agents.menu_task = state.agents.selected.clone();
            state.agents.target = repo;
        }
        match action {
            Action::Close => self.toggle_inspector(),
            Action::Refresh => {
                self.check_all_repos();
                self.refresh_agents();
            }
            Action::CommitAll => self.agent_commit_prompt(),
            Action::Pull => self.agent_remote(crate::project::git::Remote::Pull),
            Action::Push => self.agent_remote(crate::project::git::Remote::Push),
            Action::UpdateFromBase => self.agent_update_from_base(),
            Action::CreatePullRequest => self.agent_create_pull_request(),
            Action::MergePullRequest => {
                let menu = self.merge_strategy_menu();
                layout::set_pressed(None);
                crate::platform::jank::modal(|| {
                    NSMenu::popUpContextMenu_withEvent_forView(&menu, event, self)
                });
            }
            Action::OpenPullRequest => self.agent_open_pull_request(),
            Action::CopyHandoff => self.copy_handoff(),
            Action::OpenWorktree
            | Action::WorktreeTerminal
            | Action::CleanUpWorktree
            | Action::OpenIssue(_)
            | Action::ToggleIssues
            | Action::LoadIssues
            | Action::HideIssues
            | Action::FoldRepo
            | Action::CheckRemote
            | Action::FoldRewind
            | Action::RewindTurn(_)
            | Action::RewindFile
            | Action::RewindRestore => {}
        }
        self.request_redraw();
        self.pump();
    }

    /// The Receipt card's Copy Handoff: the receipt the card shows, the
    /// files its sessions wrote and the commits made in the workspace's
    /// folder since the first of them started, as text on the clipboard.
    /// Git is read here, at the click, never for a frame; a remote
    /// workspace has no commits listed.
    fn copy_handoff(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some(task) = state
            .agents
            .selected_index()
            .map(|i| state.agents.tasks[i].clone())
        else {
            return;
        };
        // As the card is drawn: the terminal showing, when it is this
        // workspace's, after its closed ones.
        let selected = state.agents.selected_index();
        let live = state
            .terminal
            .active_tab()
            .filter(|t| crate::platform::agents::task_of(&state.agents.tasks, t) == selected);
        let live_dir = live.map(|t| t.review.clone());
        let live = live.map(|t| t.receipt.clone());
        let folder = (task.host.is_none() && !task.folderless()).then(|| task.path.clone());
        let id = task.id();
        let receipt =
            crate::project::review::workspace_receipt(Some(&id), folder.as_deref(), live.as_ref())
                .map(std::borrow::Cow::into_owned)
                .unwrap_or_default();
        let trail = crate::project::review::workspace_trail(
            Some(&id),
            folder.as_deref(),
            live_dir.as_deref(),
        );
        let commits = match (&folder, trail.started) {
            (Some(folder), Some(started)) => {
                crate::project::git::commits_from(folder, started, HANDOFF_COMMITS)
                    .unwrap_or_default()
            }
            _ => Vec::new(),
        };
        // Written files are kept resolved, so the folder is too.
        let resolved = folder.as_deref().map(crate::platform::canonical);
        let text = crate::project::receipt::handoff_text(
            &receipt,
            &commits,
            &trail.wrote,
            resolved.as_deref(),
        );
        if text.is_empty() {
            state.say(layout::Feedback::Info, "Nothing to hand off yet");
        } else {
            clipboard::write_text(&text);
            let lines = text.lines().count();
            state.say(
                layout::Feedback::Success,
                format!("Copied the handoff, {lines} lines"),
            );
        }
    }

    /// A file under the selected Rewind turn: the version the turn's
    /// agent left of it is looked up in local history on a worker, and
    /// [`Self::poll_rewind`] opens it. Nothing reads the store here.
    fn rewind_file(&self, path: PathBuf) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some((dir, turn, next)) = state.agents.inspector.selected_turn() else {
            return;
        };
        let Some(session) = dir.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let file = crate::platform::canonical(&path);
            let found = crate::history::store::default_path()
                .and_then(|db| {
                    crate::history::store::Store::open_to_read(&db)
                        .ok()
                        .flatten()
                })
                .and_then(|store| store.list(&file).ok())
                .and_then(|entries| {
                    crate::project::review::turn_version(&entries, &session, turn.started, next)
                });
            let _ = tx.send((dir, turn.index, path, found));
        });
        state.agents.inspector.rewind_rx = Some(rx);
        drop(state);
        self.resume_display_link();
    }

    /// Takes what [`Self::rewind_file`] found: the file opens, in File
    /// History at the turn's version when it kept one. When it kept none
    /// the file opens as it is, and its row and the status line say so.
    pub(super) fn poll_rewind(&self) {
        let (dir, index, path, version) = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(rx) = &state.agents.inspector.rewind_rx else {
                return;
            };
            match rx.try_recv() {
                Ok(found) => {
                    state.agents.inspector.rewind_rx = None;
                    found
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    state.agents.inspector.rewind_rx = None;
                    return;
                }
            }
        };
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        if !path.is_file() {
            if let Some(mut state) = self.state_mut() {
                state.say(
                    layout::Feedback::Info,
                    format!("{name} is no longer on disk"),
                );
            }
            self.request_redraw();
            return;
        }
        self.load_path(&path.to_string_lossy());
        let opened = self.state().is_some_and(|state| {
            state
                .docs
                .active()
                .path
                .as_deref()
                .is_some_and(|p| same_file(p, &path))
        });
        match version {
            Some(id) if opened => self.open_history_at(Some(super::scrub::Goto::local(id))),
            Some(_) => {}
            None => {
                if let Some(mut state) = self.state_mut() {
                    let missing = (dir, index, path);
                    let inspector = &mut state.agents.inspector;
                    if !inspector.rewind_missing.contains(&missing) {
                        inspector.rewind_missing.push(missing);
                    }
                    state.say(
                        layout::Feedback::Info,
                        format!("No saved agent version of {name} for turn {index}"),
                    );
                }
            }
        }
        self.request_redraw();
        self.resume_display_link();
    }

    /// Restore Files to Turn N, under the selected turn: what it would do
    /// is read on a worker (local history and the files), and
    /// [`Self::poll_restore`] asks and does it.
    fn rewind_restore(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let inspector = &mut state.agents.inspector;
        if inspector.restore_rx.is_some() {
            return;
        }
        let Some((dir, index)) = inspector.rewind_turn.clone() else {
            return;
        };
        let Some(session) = inspector.rewind.sessions.iter().find(|s| s.dir == dir) else {
            return;
        };
        let Some(at) = session.turns.iter().position(|t| t.index == index) else {
            return;
        };
        let (turns, live) = (session.turns.clone(), session.live);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let store = crate::history::store::default_path().and_then(|db| {
                crate::history::store::Store::open_to_read(&db)
                    .ok()
                    .flatten()
            });
            let plan = crate::project::review::restore_plan(&dir, &turns, at, live, store.as_ref());
            let _ = tx.send(plan);
        });
        inspector.restore_rx = Some(rx);
        drop(state);
        self.resume_display_link();
    }

    /// Takes what [`Self::rewind_restore`] found: asks, naming the files
    /// changed since their agent left them, the ones with nothing to go
    /// back to and an agent maybe still at work, then puts the files back,
    /// each one's text before kept in local history. A file open with
    /// unsaved changes stops it: those are saved or reverted first.
    pub(super) fn poll_restore(&self) {
        let plan = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(rx) = &state.agents.inspector.restore_rx else {
                return;
            };
            match rx.try_recv() {
                Ok(plan) => {
                    state.agents.inspector.restore_rx = None;
                    plan
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    state.agents.inspector.restore_rx = None;
                    return;
                }
            }
        };
        let name = |path: &Path| {
            path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            )
        };
        let turn = plan.turn;
        if plan.files.is_empty() {
            if let Some(mut state) = self.state_mut() {
                let note = if plan.refused.is_empty() {
                    format!("Every file is already as turn {turn} left it")
                } else {
                    format!(
                        "Nothing to restore to turn {turn}: no saved version of {}",
                        names(plan.refused.iter().map(|(p, _)| name(p)))
                    )
                };
                state.say(layout::Feedback::Info, note);
            }
            self.request_redraw();
            return;
        }
        let unsaved: Vec<String> = self.state().map_or_else(Vec::new, |state| {
            plan.files
                .iter()
                .filter(|f| {
                    all_docs(&state).any(|docs| {
                        docs.iter().any(|b| {
                            b.is_dirty() && b.path.as_deref().is_some_and(|p| same_file(p, &f.path))
                        })
                    })
                })
                .map(|f| name(&f.path))
                .collect()
        });
        if !unsaved.is_empty() {
            if let Some(mut state) = self.state_mut() {
                state.say(
                    layout::Feedback::Failure,
                    format!(
                        "Not restored: save or revert {} first",
                        names(unsaved.into_iter())
                    ),
                );
            }
            self.request_redraw();
            return;
        }
        let count = plural_files(plan.files.len());
        let message = format!("Restore {count} to how turn {turn} left them?");
        let mut detail = vec![names(plan.files.iter().map(|f| name(&f.path)))];
        let changed: Vec<String> = plan
            .files
            .iter()
            .filter(|f| f.changed)
            .map(|f| name(&f.path))
            .collect();
        if !changed.is_empty() {
            detail.push(format!(
                "Changed since the agent left them, and that change is replaced: {}",
                names(changed.into_iter())
            ));
        }
        if !plan.refused.is_empty() {
            let left: Vec<String> = plan
                .refused
                .iter()
                .map(|(p, why)| format!("{} ({why})", name(p)))
                .collect();
            detail.push(format!("Left as they are: {}", left.join(", ")));
        }
        if plan.running {
            detail.push("The agent in this terminal may still be working.".into());
        }
        detail.push("The text each file has now is kept in File History.".into());
        let detail = detail.join("\n\n");
        let go = if std::env::var_os("CRC_SELFTEST").is_some() {
            // A test instance cannot answer a modal: it says what it would
            // have asked, and takes the answer from the environment.
            eprintln!(
                "crc: restore to turn: {message} {}",
                detail.replace('\n', " ")
            );
            std::env::var("CRC_REWIND_ANSWER").map_or(true, |a| a != "cancel")
        } else {
            ask(
                MainThreadMarker::from(self),
                &message,
                &detail,
                &["Restore", "Cancel"],
            ) == 0
        };
        if !go {
            return;
        }
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let mut failed = Vec::new();
        let mut restored = 0;
        for file in &plan.files {
            match crate::project::review::restore_file(file) {
                Ok(before) => {
                    restored += 1;
                    let text = before
                        .as_deref()
                        .and_then(|b| crate::text::file_format::decode(b).ok());
                    if let Some((text, _)) = text {
                        state.history.restored_on_disk(
                            &file.path,
                            &crate::text::rope::Rope::from_text(&text),
                        );
                    }
                }
                Err(e) => failed.push(e),
            }
        }
        if restored > 0 {
            note_files_written(&mut state);
        }
        if failed.is_empty() {
            state.say(
                layout::Feedback::Success,
                format!("Restored {} to turn {turn}", plural_files(restored)),
            );
        } else {
            state.say(
                layout::Feedback::Failure,
                format!(
                    "Restored {restored} of {} to turn {turn}: {}",
                    plural_files(plan.files.len()),
                    failed.join("; ")
                ),
            );
        }
        drop(state);
        self.request_redraw();
        self.pump();
    }

    /// View > Show Rewind: the inspector opened, Rewind unfolded and
    /// scrolled to. A workspace with no agent turns says so.
    pub(super) fn show_rewind(&self) {
        let hidden = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(task) = state
                .agents
                .selected_index()
                .filter(|_| state.agents.on)
                .map(|i| state.agents.tasks[i].clone())
            else {
                return;
            };
            // As the inspector reads it, for when it has not been drawn.
            let selected = state.agents.selected_index();
            let live = state
                .terminal
                .active_tab()
                .filter(|t| crate::platform::agents::task_of(&state.agents.tasks, t) == selected);
            let rewind = crate::project::review::workspace_rewind(
                Some(&task.id()),
                task.host.is_none().then_some(task.path.as_path()),
                live.map(|t| t.review.as_path()),
                live.map_or(0, |t| t.events_read),
            );
            let inspector = &mut state.agents.inspector;
            inspector.rewind = rewind;
            inspector.rewind_folded = false;
            inspector.reveal_rewind = !inspector.rewind.sessions.is_empty();
            if !inspector.reveal_rewind {
                state.say(
                    layout::Feedback::Info,
                    "No agent turns in this workspace yet",
                );
            }
            state.agents.inspector.hidden
        };
        if hidden {
            self.toggle_inspector();
        }
        self.request_redraw();
        self.resume_display_link();
    }

    /// The header's refresh: a Check of every repository card of the
    /// workspace shown that has an upstream, each on its own worker.
    fn check_all_repos(&self) {
        if let Some(mut state) = self.state_mut() {
            let Some(task) = state
                .agents
                .selected_index()
                .map(|i| state.agents.tasks[i].clone())
            else {
                return;
            };
            let Some(repos) = state.agents.read_repos_of(&task) else {
                return;
            };
            let sock = state.ssh_auth_sock.clone();
            state
                .agents
                .inspector
                .check_all(task.host.as_deref(), &repos, sock);
        }
    }

    /// Issue `number` of the repository `repo`, clicked in the inspector:
    /// read whole, body and comments, on a worker.
    pub(super) fn open_issue(&self, repo: crate::platform::agents::RepoKey, number: u64) {
        if let Some(mut state) = self.state_mut() {
            state.agents.request_issue(repo, number);
            state.say(
                layout::Feedback::Info,
                format!("Loading issue #{number}\u{2026}"),
            );
        }
        self.resume_display_link();
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
    /// showing and the one in hand is old or another workspace's, and its
    /// loaded issues when they are due. A folded repository's card, and
    /// its worktrees' strips, are not read.
    /// Nothing is read with crc in the background: coming back reads what
    /// is due (see [`Self::came_to_front`]).
    pub(super) fn want_inspector(&self) {
        let front = self.in_front();
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if !state.agents.on || state.agents.inspector.hidden {
            return;
        }
        if !front {
            state.agents.away = true;
            return;
        }
        // Its issues, once a workspace is selected or its repositories are
        // found; after that every few minutes.
        state.agents.refresh_issues();
        let Some(task) = state
            .agents
            .selected_index()
            .map(|i| state.agents.tasks[i].clone())
        else {
            return;
        };
        let Some(repos) = state.agents.read_repos_of(&task) else {
            return;
        };
        // A worktree is compared with its main checkout's branch; a folder's
        // repositories are main checkouts, which show their latest tag, and
        // after them their worktrees, for the strips under each.
        let strips: Vec<_> = if task.folder {
            state
                .agents
                .worktrees_of(task.host.as_deref(), &repos)
                .into_iter()
                .map(|w| (w.path, w.base, false))
                .collect()
        } else {
            Vec::new()
        };
        let wanted: Vec<_> = repos
            .into_iter()
            .map(|path| (path, task.base.clone(), task.folder))
            .chain(strips)
            .collect();
        // A remote workspace's are read on its host, over ssh.
        state.agents.inspector.want(task.host.as_deref(), &wanted);
    }

    /// Schedules the quick look at the terminals, once: a session that
    /// went into ssh without printing anything after still gets its title.
    pub(super) fn arm_sessions_tick(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if state.agents.sessions_ticking {
            return;
        }
        state.agents.sessions_ticking = true;
        drop(state);
        let _: () = unsafe {
            msg_send![self, performSelector: sel!(sessionsTick:),
                withObject: None::<&AnyObject>, afterDelay: SESSIONS_EVERY]
        };
    }

    /// Each terminal: into or out of ssh, and where the one showing is.
    pub(super) fn sessions_tick(&self) {
        let (on, changed, moved) = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.agents.sessions_ticking = false;
            let mut changed = false;
            for tab in &mut state.terminal.tabs {
                changed |= tab.read_remote();
            }
            let active = state.terminal.active;
            let moved = state
                .terminal
                .tabs
                .get_mut(active)
                .is_some_and(|tab| tab.read_cwd());
            (state.agents.on, changed, moved)
        };
        if moved {
            self.follow_cwd();
        }
        if changed {
            self.request_redraw();
        }
        if on {
            self.arm_sessions_tick();
        }
    }

    /// Schedules the periodic read of the list, once: in a few seconds
    /// with crc in front, in [`AWAY_EVERY`] behind other apps.
    pub(super) fn arm_agents_tick(&self) {
        let front = self.in_front();
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if state.agents.ticking {
            return;
        }
        state.agents.ticking = true;
        drop(state);
        let every = if front {
            crate::platform::agents::pr_tick().as_secs_f64()
        } else {
            AWAY_EVERY
        };
        let _: () = unsafe {
            msg_send![self, performSelector: sel!(agentsTick:),
                withObject: None::<&AnyObject>, afterDelay: every]
        };
    }

    /// Whether crc is in front: the active app, its window not covered. A
    /// self-test instance is never the active app; it counts as in front
    /// unless its script says otherwise (`front off`).
    pub(super) fn in_front(&self) -> bool {
        if std::env::var_os("CRC_SELFTEST").is_some() {
            return self.state().is_none_or(|s| s.agents.front != Some(false));
        }
        let app = NSApplication::sharedApplication(MainThreadMarker::from(self));
        app.isActive()
            && self.window().is_some_and(|w| {
                w.occlusionState()
                    .contains(objc2_app_kit::NSWindowOcclusionState::Visible)
            })
    }

    /// crc came to the front: activated, or its window uncovered. After
    /// time in the background, what came due meanwhile is read now, not
    /// at the next tick, and the inspector's status soon after.
    pub(super) fn came_to_front(&self) {
        if !self.in_front() {
            return;
        }
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if !state.agents.on || !std::mem::take(&mut state.agents.away) {
                return;
            }
            state.agents.inspector.touch_all();
            state.agents.ticking = false;
        }
        // The tick waiting out the background's long wait goes; this one
        // arms the next at the usual one.
        let _: () = unsafe {
            msg_send![
                objc2::class!(NSObject),
                cancelPreviousPerformRequestsWithTarget: self,
                selector: sel!(agentsTick:),
                object: None::<&AnyObject>
            ]
        };
        self.agents_tick();
    }

    /// The periodic read while Agents mode is on. In front: the selected
    /// workspace's worktrees every minute and every folder's every five,
    /// when nothing else is reading them; the pull requests that are due,
    /// and, with the inspector showing, the selected workspace's loaded
    /// issues. In the background the same at [`AWAY_EVERY`], every folder
    /// read each time, and no status for the inspector.
    pub(super) fn agents_tick(&self) {
        let front = self.in_front();
        let on = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.agents.ticking = false;
            if state.agents.on {
                if !front {
                    state.agents.away = true;
                }
                state.agents.refresh_due(front);
                state.agents.refresh_pull_requests();
                if !state.agents.inspector.hidden {
                    state.agents.refresh_issues();
                }
            }
            state.agents.on
        };
        if on {
            self.arm_agents_tick();
            // The rows' relative times ("quiet · 5 min") move on.
            self.request_redraw();
            self.resume_display_link();
        }
    }

    /// Reads the pushed rows' pull requests once the push has settled,
    /// replacing any read already scheduled: a push heard again waits on.
    pub(super) fn arm_pushed_tick(&self) {
        let _: () = unsafe {
            msg_send![
                objc2::class!(NSObject),
                cancelPreviousPerformRequestsWithTarget: self,
                selector: sel!(agentsPushedTick:),
                object: None::<&AnyObject>
            ]
        };
        let _: () = unsafe {
            msg_send![self, performSelector: sel!(agentsPushedTick:),
                withObject: None::<&AnyObject>, afterDelay: PUSH_SETTLE]
        };
    }

    pub(super) fn pushed_tick(&self) {
        let again = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            // A little early is settled: the timer is not exact.
            state
                .agents
                .read_pushed(std::time::Duration::from_secs_f64(PUSH_SETTLE - 0.1))
        };
        self.resume_display_link();
        if again {
            self.arm_pushed_tick();
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
    /// answers. Returns the folder it runs in, when it started.
    fn agent_operation(
        &self,
        doing: impl FnOnce(&crate::platform::agents::Workspace) -> String,
        work: impl FnOnce(PathBuf) -> Result<String, String> + Send + 'static,
    ) -> Option<PathBuf> {
        let task = self.menu_task()?;
        let mut state = self.state_mut()?;
        if state.agents.op.is_some() {
            state.say(
                layout::Feedback::Info,
                "Another workspace operation is still running",
            );
            drop(state);
            self.request_redraw();
            return None;
        }
        let path = state
            .agents
            .target
            .take()
            .unwrap_or_else(|| task.path.clone());
        let task = crate::platform::agents::Workspace {
            name: repo_label(&task, &path),
            ..task
        };
        let folder = path.clone();
        state.agents.start(&doing(&task), move || {
            Ok(Done {
                said: work(path)?,
                start_in: None,
                claude: None,
            })
        });
        drop(state);
        self.resume_display_link();
        self.request_redraw();
        Some(folder)
    }

    /// Its pull request is read again when the operation running in
    /// `folder` (of the menu's workspace, on its host when remote) answers.
    fn read_pull_request_after(&self, folder: Option<PathBuf>) {
        let host = self.menu_host();
        if let (Some(folder), Some(mut state)) = (folder, self.state_mut()) {
            state.agents.pr_after = Some(crate::platform::agents::row_id(host.as_deref(), &folder));
        }
    }

    /// The host of the menu's workspace, when it is remote: its Git
    /// operations run there, over ssh.
    fn menu_host(&self) -> Option<String> {
        self.menu_task().and_then(|t| t.host)
    }

    /// Pull (fast-forward only) or push, the way Source Control does them;
    /// on a remote workspace's host, with the same flags.
    pub(super) fn agent_remote(&self, what: crate::project::git::Remote) {
        let sock = self.state().and_then(|state| state.ssh_auth_sock.clone());
        let host = self.menu_host();
        let remote = host.is_some();
        let on = host.clone();
        let folder = self.agent_operation(
            |t| format!("{} {}\u{2026}", what.verb(), t.name()),
            move |path| {
                let said = match on {
                    Some(host) => {
                        let runner = crate::platform::agents::runner(Some(&host));
                        crate::project::worktree::remote_on(&runner, &path, what)
                    }
                    None => crate::project::git::remote(&path, what, sock.as_deref()),
                }?;
                // Git said nothing: the status line still says it is done.
                Ok(if !said.is_empty() {
                    said
                } else if what == crate::project::git::Remote::Push {
                    "Pushed".into()
                } else {
                    "Pulled".into()
                })
            },
        );
        // A pull fetches: its repository's Remote row says checked after.
        if what == crate::project::git::Remote::Pull
            && let (Some(folder), Some(mut state)) = (folder.clone(), self.state_mut())
        {
            state.agents.inspector.pull_after = Some((host, folder));
        }
        // A host's pushes are not heard here (its Git is not watched): its
        // pull request is read again when the push answers.
        if remote && what == crate::project::git::Remote::Push {
            self.read_pull_request_after(folder);
        }
    }

    /// Merges the main checkout's branch into the workspace's.
    pub(super) fn agent_update_from_base(&self) {
        let Some(base) = self.menu_task().and_then(|t| t.base) else {
            return;
        };
        let doing = format!("Merging {base}\u{2026}");
        let host = self.menu_host();
        self.agent_operation(
            |_| doing,
            move |path| {
                let runner = crate::platform::agents::runner(host.as_deref());
                crate::project::worktree::update_from_on(&runner, &path, &base)
            },
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
            let label = state
                .agents
                .target
                .as_deref()
                .map_or_else(|| task.name(), |path| repo_label(&task, path));
            state.commit_prompt = Some(label);
        }
        self.request_redraw();
    }

    /// The palette's answer: stage everything and commit it.
    pub(super) fn agent_commit_all(&self, message: String) {
        let host = self.menu_host();
        // The workspace's private markers, as Source Control checks them,
        // and the key agent's socket for the commit's signature.
        let (guard, sock) = self
            .state()
            .map(|state| (state.git.guard.clone(), state.ssh_auth_sock.clone()))
            .unwrap_or_default();
        self.agent_operation(
            |t| format!("Committing in {}\u{2026}", t.name()),
            move |path| match host {
                Some(host) => {
                    let runner = crate::platform::agents::runner(Some(&host));
                    crate::project::worktree::commit_all_on(
                        &runner,
                        &path,
                        &message,
                        &guard,
                        sock.as_deref(),
                    )
                }
                None => {
                    crate::project::worktree::commit_all(&path, &message, &guard, sock.as_deref())
                }
            },
        );
    }

    /// Pushes the branch, then opens a pull request from its commits. On
    /// a host: pushed there, its title and body read there, and opened by
    /// `gh` here on the repository's GitHub name.
    pub(super) fn agent_create_pull_request(&self) {
        let sock = self.state().and_then(|state| state.ssh_auth_sock.clone());
        let task = self.menu_task();
        let remote = task.as_ref().and_then(|t| {
            Some((
                t.host.clone()?,
                t.github.clone()?,
                t.branch.clone()?,
                t.base.clone(),
            ))
        });
        if task.as_ref().is_some_and(|t| t.host.is_some()) && remote.is_none() {
            return;
        }
        let folder = self.agent_operation(
            |t| format!("Opening a pull request for {}\u{2026}", t.name()),
            move |path| {
                use crate::project::git::Remote;
                let Some((host, repo, branch, base)) = remote else {
                    crate::project::git::remote(&path, Remote::Push, sock.as_deref())?;
                    return crate::project::worktree::create_pull_request(&path);
                };
                let runner = crate::platform::agents::runner(Some(&host));
                crate::project::worktree::remote_on(&runner, &path, Remote::Push)?;
                let (title, body) = crate::project::worktree::pull_request_text_on(
                    &runner,
                    &path,
                    base.as_deref(),
                    &branch,
                )?;
                crate::project::worktree::create_pull_request_in(&repo, &branch, &title, &body)
            },
        );
        self.read_pull_request_after(folder);
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
                .get(&task.id())
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
        // A host's is merged by `gh` here, told the repository's name.
        let repo = task.github.clone().filter(|_| task.host.is_some());
        if task.host.is_some() && repo.is_none() {
            return;
        }
        let folder = self.agent_operation(
            |_| format!("Merging #{number}\u{2026}"),
            move |path| {
                let on = match repo {
                    Some(repo) => crate::project::worktree::GitHub::Repo(repo),
                    None => crate::project::worktree::GitHub::Dir(path),
                };
                crate::project::worktree::merge_pull_request_in(&on, number, strategy)
            },
        );
        self.read_pull_request_after(folder);
    }
}

/// What an operation calls the repository at `path` in `workspace`: the
/// workspace's name, or for a repository in a folder workspace other than
/// the folder itself, the repository's folder name.
fn repo_label(workspace: &crate::platform::agents::Workspace, path: &Path) -> String {
    if path == workspace.path {
        workspace.name()
    } else {
        path.file_name()
            .map_or_else(|| workspace.name(), |n| n.to_string_lossy().into_owned())
    }
}

fn plural_commits(n: usize) -> String {
    if n == 1 {
        "1 commit".into()
    } else {
        format!("{n} commits")
    }
}
