//! The terminal panel in the window: its session and grid, keys, mouse
//! selection, and what it does after each frame.

use super::*;

impl EditorView {
    /// Whether keys go to the terminal: it is open with the keyboard, and
    /// no field (palette, find, go to line) has taken it.
    pub(super) fn terminal_has_keys(&self) -> bool {
        let Some(state) = self.state() else {
            return false;
        };
        state.terminal.has_keys() && !field_has_keys(&state)
    }

    pub(super) fn terminal_wake(&self) -> crate::term::pty::Wake {
        self.wake(EditorView::poll_terminal)
    }

    /// A session printed something, or its program exited.
    pub(super) fn poll_terminal(&self) {
        let Some(state) = self.state() else {
            self.resume_display_link();
            return;
        };
        let mut changed = false;
        let mut noticed = Vec::new();
        for (index, tab) in state.terminal.tabs.iter().enumerate() {
            if tab.session.drain_wake() {
                changed = true;
                let (notices, status) = {
                    let mut term = tab.session.term.lock().unwrap_or_else(|e| e.into_inner());
                    (term.take_notices(), term.status.root().cloned())
                };
                noticed.push(Noticed {
                    index,
                    notice: notices.into_iter().last(),
                    printed: true,
                    status,
                });
            }
        }
        drop(state);
        let printed: Vec<usize> = noticed.iter().map(|n| n.index).collect();
        if !noticed.is_empty() {
            self.take_notices(noticed);
        }
        // A prompt after a `cd`: where the terminal showing is now. Read on
        // every burst it prints, so the prompt right after the `cd` is
        // never missed.
        let moved = self.state_mut().is_some_and(|mut state| {
            // Into or out of ssh, any tab: its row and title say so.
            for index in &printed {
                if let Some(tab) = state.terminal.tabs.get_mut(*index) {
                    tab.read_remote();
                }
            }
            let active = state.terminal.active;
            printed.contains(&active)
                && state
                    .terminal
                    .tabs
                    .get_mut(active)
                    .is_some_and(|tab| tab.read_cwd())
        });
        if moved {
            self.follow_cwd();
        }
        let Some(state) = self.state() else {
            return;
        };
        let exited = state
            .terminal
            .tabs
            .iter()
            .any(|tab| tab.session.has_exited());
        drop(state);
        // A program that exits takes its tab with it, and the last tab
        // takes the panel, as in any terminal.
        if exited {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            while let Some(index) = state
                .terminal
                .tabs
                .iter()
                .position(|tab| tab.session.has_exited())
            {
                // Said in the status line: a tab that vanishes with its
                // program otherwise looks like a crash of the panel.
                let tab = &state.terminal.tabs[index];
                let (kind, said) = match tab.session.exit_code() {
                    Some(0) => (
                        layout::Feedback::Info,
                        format!("Terminal: {} ended", tab.title),
                    ),
                    Some(code) => (
                        layout::Feedback::Failure,
                        format!("Terminal: {} exited with code {code}", tab.title),
                    ),
                    None => (
                        layout::Feedback::Info,
                        format!("Terminal: {} ended", tab.title),
                    ),
                };
                state.terminal.close_tab(index);
                state.say(kind, said);
            }
            drop(state);
            self.after_terminal_layout();
            return;
        }
        if changed {
            self.request_redraw();
            self.pump();
        }
    }

    /// Shows the panel with the keyboard: on the Claude Code session when
    /// `claude`, started if there is none, or on the current session,
    /// started if there is none.
    pub(super) fn open_terminal(&self, claude: bool) {
        let existing = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.terminal.open = true;
            state.terminal.focus = true;
            state.sidebar_keys = false;
            let found = if claude {
                state.terminal.tabs.iter().position(|tab| tab.claude)
            } else {
                (!state.terminal.tabs.is_empty()).then_some(state.terminal.active)
            };
            if let Some(index) = found {
                state.terminal.active = index;
            }
            found
        };
        if existing.is_none() {
            self.spawn_terminal(claude);
        }
        self.after_terminal_layout();
    }

    /// What a new session runs: the login shell, or `claude` through it so
    /// it finds what the shell profile puts on PATH. Claude Code is told
    /// this window's IDE port, so it connects without `/ide`. In a remote
    /// workspace (`remote`: its host and folder) it is ssh to that folder,
    /// run from home, and the shell or `claude` runs there without any of
    /// this window: no IDE port, no review hooks. `prompt` is what `claude`
    /// is first told, here, as one argument quoted for the shell.
    pub(super) fn terminal_launch(
        &self,
        claude: bool,
        cwd: Option<&Path>,
        remote: Option<(&str, &Path)>,
        prompt: Option<&str>,
    ) -> Option<crate::platform::terminal::Launch> {
        let state = self.state()?;
        let shell = std::env::var("CRC_TERMINAL_SHELL")
            .or_else(|_| std::env::var("SHELL"))
            .unwrap_or_else(|_| "/bin/zsh".into());
        let home = std::env::var_os("HOME").map_or_else(|| "/".into(), std::path::PathBuf::from);
        let cwd = match cwd {
            _ if remote.is_some() => home,
            Some(dir) => dir.to_path_buf(),
            None => state.tree.root().map_or(home, Path::to_path_buf),
        };
        let mut env = vec![
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("COLORTERM".to_owned(), "truecolor".to_owned()),
            ("TERM_PROGRAM".to_owned(), "crc".to_owned()),
            (
                "TERM_PROGRAM_VERSION".to_owned(),
                env!("CARGO_PKG_VERSION").to_owned(),
            ),
        ];
        // Each session gets its own review folder, which the agent's hooks
        // write into; created by the first hook, not here. Not one on a
        // host: what it changes is there, and nothing of this Mac's is its.
        if remote.is_none()
            && let Some(dir) = crate::project::review::root()
        {
            env.push((
                crate::project::review::SESSION_VAR.to_owned(),
                dir.join(crate::project::review::new_id())
                    .to_string_lossy()
                    .into_owned(),
            ));
        }
        // A Claude Code session that started this editor (from its own
        // terminal, say) left its markers in our environment. Passed on,
        // they make the `claude` here think it is that session's child.
        let unset = std::env::vars_os()
            .filter_map(|(name, _)| name.into_string().ok())
            .filter(|name| name == "CLAUDECODE" || name.starts_with("CLAUDE_CODE_"))
            .collect();
        if let Some((host, path)) = remote {
            // ssh sends none of these variables but what the person's own
            // config says to; TERM it sends as the terminal's type. It runs
            // under sh, so a host it cannot reach leaves its error on screen.
            let control = crate::platform::remote::control_for(host);
            let args = crate::platform::remote::ssh_args(
                host,
                &path.to_string_lossy(),
                claude.then_some("claude"),
                control.as_deref(),
            );
            let args = crate::platform::remote::kept_open_args(
                &crate::platform::remote::program(),
                args,
                host,
                &crate::platform::remote::known_hosts(),
            );
            return Some(crate::platform::terminal::Launch {
                program: "/bin/sh".into(),
                args,
                cwd,
                env,
                unset,
            });
        }
        // Every session knows this window's IDE port, so `claude` typed in
        // any of them connects without `/ide`.
        if let Some(bridge) = &state.claude {
            env.push(("CLAUDE_CODE_SSE_PORT".to_owned(), bridge.port().to_string()));
            env.push(("ENABLE_IDE_INTEGRATION".to_owned(), "true".to_owned()));
        }
        let args = if claude {
            // A command given in CRC_CLAUDE_COMMAND runs as given, with
            // the prompt after it as for `claude`. Plain `claude` gets the
            // review hooks as extra settings: Claude merges them with the
            // person's own, and nothing in ~/.claude is touched.
            let command = match std::env::var("CRC_CLAUDE_COMMAND") {
                Ok(command) => command,
                Err(_) => match state
                    .agent_review
                    .then(crate::project::review::claude_settings_arg)
                    .flatten()
                {
                    Some(settings) => format!("claude --settings {settings}"),
                    None => "claude".into(),
                },
            };
            let command = crate::platform::agents::with_prompt(&command, prompt);
            vec!["-l".to_owned(), "-i".to_owned(), "-c".to_owned(), command]
        } else {
            vec!["-l".to_owned()]
        };
        Some(crate::platform::terminal::Launch {
            program: shell.into(),
            args,
            cwd,
            env,
            unset,
        })
    }

    /// The panel's grid, for the window as it is now.
    pub(super) fn terminal_grid(&self) -> (usize, usize) {
        let chrome = self.chrome();
        let Some(state) = self.state() else {
            return (80, 24);
        };
        chrome.terminal.map_or((80, 24), |rect| {
            let (_, screen) = crate::platform::terminal::split(rect);
            crate::platform::terminal::grid_size(&state.renderer.atlas, screen)
        })
    }

    /// A new session in the open folder, or in Agents mode in the selected
    /// workspace's folder.
    pub(super) fn spawn_terminal(&self, claude: bool) {
        if self.state().is_some_and(|state| state.agents.on) {
            self.ensure_workspace();
        }
        let task = self.state().and_then(|state| {
            state
                .agents
                .on
                .then(|| state.agents.selected_index())
                .flatten()
                .map(|i| state.agents.tasks[i].path.clone())
        });
        self.spawn_terminal_in(claude, task.as_deref());
    }

    pub(super) fn spawn_terminal_in(&self, claude: bool, cwd: Option<&Path>) {
        self.spawn_terminal_into(claude, cwd, None);
    }

    /// As [`Self::spawn_terminal_in`], in workspace `into` when given
    /// rather than the selected one.
    pub(super) fn spawn_terminal_into(
        &self,
        claude: bool,
        cwd: Option<&Path>,
        into: Option<String>,
    ) {
        self.spawn_terminal_on(claude, cwd, into, None, None);
    }

    /// A shell over ssh in `dir` on `host`, in the workspace `id`: a
    /// worktree just made there, before the list has its row.
    pub(super) fn spawn_remote_terminal(&self, host: &str, dir: &Path, id: String) {
        self.spawn_terminal_on(false, Some(dir), Some(id), Some(host.to_owned()), None);
    }

    /// Claude in `dir` on this Mac, in the workspace `id`, first told
    /// `prompt`: an issue's task, whose row the list may not have yet.
    pub(super) fn spawn_claude_told(&self, dir: &Path, id: String, prompt: &str) {
        self.spawn_terminal_on(true, Some(dir), Some(id), None, Some(prompt));
    }

    /// As [`Self::spawn_terminal_into`]; on `host`, in `cwd` there, when
    /// the workspace's row does not say so yet; Claude first told `prompt`.
    fn spawn_terminal_on(
        &self,
        claude: bool,
        cwd: Option<&Path>,
        into: Option<String>,
        host: Option<String>,
        prompt: Option<&str>,
    ) {
        // Every new terminal says it: one opened later fails the same way.
        self.say_if_replaced(true);
        // In Agents mode a session started in the selected workspace's
        // folder (or in no particular one) belongs to that workspace.
        let (workspace, remote) = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.terminal.open = true;
            state.terminal.focus = true;
            let workspace = into.or_else(|| {
                state
                    .agents
                    .on
                    .then(|| state.agents.selected_index())
                    .flatten()
                    .map(|i| &state.agents.tasks[i])
                    .filter(|t| {
                        cwd.is_none_or(|dir| match t.host {
                            Some(_) => dir == t.path,
                            None => crate::platform::canonical(dir) == t.path,
                        })
                    })
                    .map(|t| t.id())
            });
            // A remote workspace's terminal is ssh to its folder.
            let remote = workspace
                .as_deref()
                .and_then(|id| state.agents.by_id(id))
                .map(|i| &state.agents.tasks[i])
                .and_then(|t| Some((t.host.clone()?, t.path.clone())))
                .or_else(|| Some((host?, cwd?.to_path_buf())));
            (workspace, remote)
        };
        let Some(launch) = self.terminal_launch(
            claude,
            cwd,
            remote
                .as_ref()
                .map(|(host, path)| (host.as_str(), path.as_path())),
            prompt,
        ) else {
            return;
        };
        let (cols, rows) = self.terminal_grid();
        let title = if claude {
            "✻ Claude".to_owned()
        } else if let Some((host, _)) = &remote {
            format!("ssh {host}")
        } else {
            launch
                .program
                .file_name()
                .map_or_else(|| "shell".into(), |n| n.to_string_lossy().into_owned())
        };
        let review = launch
            .env
            .iter()
            .find(|(k, _)| k == crate::project::review::SESSION_VAR)
            .map(|(_, v)| std::path::PathBuf::from(v))
            .unwrap_or_default();
        let project = self
            .state()
            .and_then(|state| state.tree.root().map(Path::to_path_buf))
            .unwrap_or_default();
        // A helper holds the program, so it outlives this window; without
        // one (no binary to run it, a folder that cannot be written) the
        // program runs here, as it always did, and the status line says so.
        let spec = crate::term::hold::Spec {
            program: launch.program.clone(),
            args: launch.args.clone(),
            cwd: launch.cwd.clone(),
            env: launch.env.clone(),
            unset: launch.unset.clone(),
            cols,
            rows,
            title: title.clone(),
            claude,
            review: review.clone(),
            project,
            workspace: workspace.clone().unwrap_or_default(),
        };
        // A window opened on one file lets its sessions end at quit, so it
        // runs them itself, as it always ended them, and says nothing.
        let ephemeral = self.state().is_none_or(|state| state.ephemeral_session);
        let held = self
            .held_dir()
            .and_then(|dir| {
                let helper = std::env::current_exe()?;
                crate::term::hold::start(&dir, &spec, &helper)
            })
            .and_then(|(id, socket)| {
                crate::term::pty::Session::attach(socket, self.terminal_wake()).map(|s| (id, s))
            });
        let (spawned, held, said) = match held {
            Ok((id, session)) => (Ok(session), Some(id), None),
            Err(e) => {
                let args: Vec<&str> = launch.args.iter().map(String::as_str).collect();
                let env: Vec<(&str, &str)> = launch
                    .env
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                let unset: Vec<&str> = launch.unset.iter().map(String::as_str).collect();
                let spawned = crate::term::pty::Session::spawn(
                    &launch.program,
                    &args,
                    &launch.cwd,
                    &env,
                    &unset,
                    cols,
                    rows,
                    self.terminal_wake(),
                );
                (
                    spawned,
                    None,
                    (!ephemeral)
                        .then(|| format!("Terminal: this session ends with the window: {e}")),
                )
            }
        };
        let Some(mut state) = self.state_mut() else {
            return;
        };
        match spawned {
            Ok(session) => {
                let tab = crate::platform::terminal::Tab {
                    session,
                    title,
                    claude,
                    attention: None,
                    last_output: None,
                    review,
                    events_read: 0,
                    review_files: 0,
                    folder: crate::platform::canonical(&launch.cwd),
                    workspace,
                    alive: std::sync::Arc::new(()),
                    cwd: crate::platform::canonical(&launch.cwd),
                    program_pid: None,
                    remote: None,
                    launch,
                    held,
                    status: None,
                };
                self.add_terminal_tab(&mut state, tab);
                state.terminal.active = state.terminal.tabs.len() - 1;
                state.terminal.back = 0;
                if let Some(said) = said {
                    state.say(layout::Feedback::Failure, said);
                }
            }
            Err(e) => {
                state.message = Some((
                    format!("could not start {}: {e}", launch.program.display()),
                    Instant::now(),
                ));
            }
        }
        drop(state);
        self.after_terminal_layout();
    }

    /// Adds a tab and starts watching its agent's hook events.
    fn add_terminal_tab(&self, state: &mut State, tab: crate::platform::terminal::Tab) {
        state.terminal.tabs.push(tab);
        let tab = state.terminal.tabs.last().expect("just pushed");
        // A tab on a host has no review folder: no hooks to hear.
        if !tab.tracks_review() {
            return;
        }
        watch_events(
            tab.review.join("events"),
            std::sync::Arc::downgrade(&tab.alive),
            self.wake(EditorView::poll_agent_events),
        );
    }

    /// Starts again the sessions lost since crc last ran (a reboot ends
    /// their helpers): a shell in the same folder and workspace, and where
    /// an agent ran, `claude --continue` typed into it, so the conversation
    /// in that folder carries on in plain sight.
    fn restore_lost(&self, lost: Vec<crate::term::hold::Found>) {
        let mut restored = 0;
        for (_, spec, left) in lost {
            if !spec.cwd.is_dir() {
                continue;
            }
            let workspace = [&left.workspace, &spec.workspace]
                .into_iter()
                .find(|w| !w.is_empty())
                .cloned();
            self.spawn_terminal_into(false, Some(&spec.cwd), workspace);
            restored += 1;
            // A Claude tab crc started counts even when crc never got to
            // write down what ran (a crash, a forced quit).
            if (left.agent || spec.claude)
                && let Some(mut state) = self.state_mut()
                && let Some(tab) = state.terminal.tabs.last_mut()
            {
                tab.session.write(b"claude --continue\n");
            }
        }
        if restored > 0
            && let Some(mut state) = self.state_mut()
        {
            let said = match restored {
                1 => "Started again 1 terminal that ended with the last run".to_owned(),
                n => format!("Started again {n} terminals that ended with the last run"),
            };
            state.say(layout::Feedback::Info, said.clone());
            state.restore_note = Some(said);
        }
    }

    /// The folder of the session helpers, once this process holds them
    /// ([`crate::term::hold::claim`]), claimed on first use. A window
    /// opened on one file never claims them: a crc opened on the project
    /// later must still get its sessions back.
    fn held_dir(&self) -> std::io::Result<std::path::PathBuf> {
        let dir = crate::term::hold::dir()
            .ok_or_else(|| std::io::Error::other("no folder for session helpers"))?;
        let mut state = self
            .state_mut()
            .ok_or_else(|| std::io::Error::other("the window is busy"))?;
        if state.held_lock.is_none() && !state.ephemeral_session {
            state.held_lock = crate::term::hold::claim(&dir);
        }
        if state.held_lock.is_none() {
            return Err(std::io::Error::other(
                "another crc running keeps the sessions",
            ));
        }
        Ok(dir)
    }

    /// Takes back the sessions helpers kept from the last window, in every
    /// workspace: their screens as the programs left them, the tab that had
    /// the keyboard active again. At launch, before the first frame. Only
    /// the crc that holds the helpers does: another one running at the same
    /// time leaves them where they are.
    pub(super) fn reattach_held(&self) {
        // A window opened on one file keeps no session and lets nothing go
        // at quit; it takes nothing back either.
        if self.state().is_none_or(|state| state.ephemeral_session) {
            return;
        }
        let Ok(dir) = self.held_dir() else {
            return;
        };
        let mut taken = Vec::new();
        let (live, mut lost) = crate::term::hold::scan(&dir);
        for (id, spec, left) in live {
            let session = crate::term::hold::connect(&dir, &id)
                .and_then(|socket| crate::term::pty::Session::attach(socket, self.terminal_wake()));
            let Ok(session) = session else {
                // A pid taken by another process after a reboot, and a
                // socket left in /tmp: as lost as a helper that is gone.
                crate::term::hold::forget(&dir, &id);
                lost.push((id, spec, left));
                continue;
            };
            let launch = crate::platform::terminal::Launch {
                program: spec.program,
                args: spec.args,
                cwd: spec.cwd,
                env: spec.env,
                unset: spec.unset,
            };
            taken.push((
                crate::platform::terminal::Tab {
                    session,
                    title: spec.title,
                    claude: spec.claude,
                    attention: None,
                    last_output: None,
                    review: spec.review,
                    events_read: left.events_read,
                    review_files: 0,
                    folder: crate::platform::canonical(&launch.cwd),
                    workspace: [&left.workspace, &spec.workspace]
                        .into_iter()
                        .find(|w| !w.is_empty())
                        .cloned(),
                    alive: std::sync::Arc::new(()),
                    status: None,
                    cwd: crate::platform::canonical(&launch.cwd),
                    program_pid: None,
                    remote: None,
                    launch,
                    held: Some(id),
                },
                left.active,
            ));
        }
        lost.sort_by(|a, b| a.0.cmp(&b.0));
        self.restore_lost(lost);
        if taken.is_empty() {
            return;
        }
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let mut active = None;
        for (tab, was_active) in taken {
            self.add_terminal_tab(&mut state, tab);
            if was_active {
                active = Some(state.terminal.tabs.len() - 1);
            }
        }
        state.terminal.active = active.unwrap_or(state.terminal.tabs.len() - 1);
        state.terminal.open = true;
        state.terminal.back = 0;
        super::review::refresh_review(&mut state);
        drop(state);
        self.after_terminal_layout();
    }

    /// The panel appeared, went, or changed sessions: the editor column
    /// changed size, and so did the cursor rectangles.
    pub(super) fn after_terminal_layout(&self) {
        let refocused = self.state_mut().is_some_and(|mut state| {
            let refocused = state.terminal.closed_active.is_some();
            super::agents::forget_emptied(&mut state);
            refocused
        });
        // The terminal now showing may be elsewhere: the workspace follows.
        if refocused {
            if let Some(mut state) = self.state_mut() {
                let active = state.terminal.active;
                if let Some(tab) = state.terminal.tabs.get_mut(active) {
                    tab.read_cwd();
                }
            }
            self.follow_cwd();
        }
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
        self.request_redraw();
        self.pump();
    }

    /// Keeps every session's grid the size of the panel.
    pub(super) fn terminal_after_frame(&self) {
        let Some(state) = self.state() else {
            return;
        };
        if !state.terminal.open || state.terminal.tabs.is_empty() {
            return;
        }
        let Some(rect) = chrome_of(&state).terminal else {
            return;
        };
        let (_, screen) = crate::platform::terminal::split(rect);
        let atlas = &state.renderer.atlas;
        let (cols, rows) = crate::platform::terminal::grid_size(atlas, screen);
        let pair = state.terminal.shown_pair();
        let (half, _) = crate::platform::terminal::halves(screen);
        let (half_cols, half_rows) = crate::platform::terminal::grid_size(atlas, half);
        for (index, tab) in state.terminal.tabs.iter().enumerate() {
            if pair.is_some_and(|(l, r)| index == l || index == r) {
                tab.session.resize(half_cols, half_rows);
            } else {
                tab.session.resize(cols, rows);
            }
        }
    }

    /// Agents mode's Cmd-\: the session showing and another of its task
    /// side by side, a new shell in its folder when it has no other.
    pub(super) fn split_terminal(&self) {
        let start_in = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if state.terminal.shown_pair().is_some() {
                return;
            }
            let active = state.terminal.active;
            let Some(folder) = state.terminal.active_tab().map(|t| t.folder.clone()) else {
                return;
            };
            let tasks = state.agents.tasks.clone();
            let task = crate::platform::agents::task_of(&tasks, &state.terminal.tabs[active]);
            let other = (0..state.terminal.tabs.len()).find(|i| {
                *i != active
                    && crate::platform::agents::task_of(&tasks, &state.terminal.tabs[*i]) == task
            });
            match other {
                Some(other) => {
                    state.terminal.pair = Some((active, other));
                    state.terminal.show(other);
                    None
                }
                None => Some((active, task.map_or(folder, |t| tasks[t].path.clone()))),
            }
        };
        if let Some((left, dir)) = start_in {
            self.spawn_terminal_in(false, Some(&dir));
            if let Some(mut state) = self.state_mut() {
                let right = state.terminal.active;
                if right != left {
                    state.terminal.pair = Some((left, right));
                }
            }
        }
        self.after_terminal_layout();
    }

    /// Agents mode's Close Pane: the pair goes, the session with the
    /// keyboard stays, and the other keeps running in its tab.
    pub(super) fn unsplit_terminal(&self) {
        if let Some(mut state) = self.state_mut() {
            state.terminal.pair = None;
        }
        self.after_terminal_layout();
    }

    /// Clears the active session's screen the way Control-L does in a
    /// shell: the program redraws its prompt at the top.
    pub(super) fn clear_terminal(&self) {
        self.terminal_write(b"\x0c");
        self.request_redraw();
    }

    pub(super) fn terminal_write(&self, bytes: &[u8]) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        state.terminal.back = 0;
        state.terminal.selection = None;
        let mut answered = false;
        if let Some(tab) = state.terminal.active_tab_mut() {
            // Typing into it is the answer to what it asked. A result
            // it reported is seen now; a block stays until it says so.
            answered = tab.attention.take().is_some();
            if tab
                .session
                .term
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .status
                .acknowledge()
            {
                tab.status = None;
                answered = true;
            }
            tab.session.write(bytes);
        }
        if answered {
            super::agents::sync_dock_badge(&state, MainThreadMarker::from(self));
        }
    }

    /// An agent's hook wrote an event while its terminal was quiet: an
    /// agent that ends its turn need not print anything after.
    pub(super) fn poll_agent_events(&self) {
        let count = self.state().map_or(0, |state| state.terminal.tabs.len());
        self.take_notices(
            (0..count)
                .map(|index| Noticed {
                    index,
                    notice: None,
                    printed: false,
                    status: None,
                })
                .collect(),
        );
        self.request_redraw();
        self.pump();
    }

    /// Output arrived in these tabs, or their hooks wrote, with the newest
    /// notification each carried and what the program says of itself. A
    /// notification or a block marks its tab; the status line says it when
    /// the tab is not the one being typed into.
    fn take_notices(&self, noticed: Vec<Noticed>) {
        use crate::term::status::State;
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let now = Instant::now();
        let mut said = None;
        let watching = state.terminal.has_keys();
        let active = state.terminal.active;
        let mut wrote = false;
        let mut asked = false;
        let mut team_events = Vec::new();
        let mut written = Vec::new();
        for Noticed {
            index,
            mut notice,
            printed,
            status,
        } in noticed
        {
            let Some(tab) = state.terminal.tabs.get_mut(index) else {
                continue;
            };
            if printed {
                tab.last_output = Some(now);
                // Its own word: a new block, result or failure is an ask;
                // back at work, nothing of the old ask is left to answer.
                let was = tab.status.as_ref().map(|s| s.state);
                let is = status.as_ref().map(|s| s.state);
                if is != was {
                    match is {
                        Some(State::Blocked | State::Done | State::Error) => {
                            asked = true;
                            tab.status = status;
                            if !(watching && index == active) {
                                said = Some(format!("{}: {}", tab.name(), tab.state()));
                            }
                        }
                        Some(State::Working) => {
                            tab.attention = None;
                            tab.status = status;
                        }
                        Some(State::Idle) | None => tab.status = status,
                    }
                } else {
                    tab.status = status;
                }
            }
            // What its agent's hooks wrote since the last look: an agent
            // writes files and asks questions while it prints.
            if !tab.tracks_review() {
                continue;
            }
            let (events, read) = crate::project::review::read_events(&tab.review, tab.events_read);
            tab.events_read = read;
            for event in events {
                match event {
                    crate::project::review::Event::Wrote { path, .. } => {
                        crate::project::review::describe(
                            &tab.review,
                            &tab.base_name(),
                            &tab.launch.cwd,
                        );
                        wrote = true;
                        written.push(path);
                    }
                    crate::project::review::Event::Asked(text) => notice = Some(text),
                    crate::project::review::Event::Failed(text) => {
                        said = Some(format!("{}: review hook: {text}", tab.name()));
                    }
                    event @ (crate::project::review::Event::Task { .. }
                    | crate::project::review::Event::TeammateIdle(_)) => {
                        team_events.push((tab.review.clone(), event));
                    }
                }
            }
            if let Some(text) = notice {
                asked |= tab.attention.is_none();
                tab.attention = Some(text);
                if !(watching && index == active) {
                    said = Some(format!("{}: {}", tab.name(), tab.state()));
                }
            }
        }
        for (review, event) in team_events {
            state.agents.teams.entry(review).or_default().apply(&event);
        }
        // A worktree an agent writes in says so in its row.
        for path in written {
            state.agents.note_write(&path);
        }
        if wrote {
            super::review::refresh_review(&mut state);
        }
        if let Some(said) = said {
            state.message = Some((said, now));
        }
        if asked {
            let mtm = MainThreadMarker::from(self);
            super::agents::sync_dock_badge(&state, mtm);
            // Somewhere else: a bounce of the Dock icon says an agent
            // waits, once, as Mail does for a message.
            let app = NSApplication::sharedApplication(mtm);
            if !app.isActive() && std::env::var_os("CRC_SELFTEST").is_none() {
                app.requestUserAttention(NSRequestUserAttentionType::InformationalRequest);
            }
        }
    }

    /// The active session's screen rect and grid size, for mapping points.
    pub(super) fn terminal_screen(&self) -> Option<(Viewport, usize, usize)> {
        let rect = self.chrome().terminal?;
        let (_, screen) = crate::platform::terminal::split(rect);
        let state = self.state()?;
        let active = state.terminal.active;
        let screen = state
            .terminal
            .screens(screen)
            .into_iter()
            .find(|(tab, _)| *tab == active)
            .map_or(screen, |(_, s)| s);
        let tab = state.terminal.active_tab()?;
        let term = tab.session.term.lock().unwrap_or_else(|e| e.into_inner());
        Some((screen, term.cols(), term.rows()))
    }

    /// A press in the terminal's screen: starts a selection, selects a word
    /// or path on a double click and the line on a triple, and with Command
    /// opens the file reference under the pointer.
    pub(super) fn terminal_press(&self, event: &NSEvent, x: f32, y: f32) {
        // A press in the other pane of a pair gives it the keyboard first.
        if let Some(rect) = self.chrome().terminal
            && let Some(mut state) = self.state_mut()
            && state.terminal.shown_pair().is_some()
        {
            let (_, screen) = crate::platform::terminal::split(rect);
            let tab = state.terminal.screen_at(screen, x);
            if tab != state.terminal.active {
                state.terminal.show(tab);
            }
            state.terminal.focus = true;
        }
        let Some((screen, cols, rows)) = self.terminal_screen() else {
            return;
        };
        let command = event
            .modifierFlags()
            .contains(NSEventModifierFlags::Command);
        let clicks = event.clickCount();
        let target = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let (row, boundary, cell) =
                crate::platform::terminal::point(&state.renderer.atlas, screen, x, y, cols, rows);
            let back = state.terminal.back;
            let Some(tab) = state.terminal.active_tab() else {
                return;
            };
            let term = tab.session.term.lock().unwrap_or_else(|e| e.into_inner());
            let line = term.view_line(row, back);
            if command {
                let chars = term.line_chars(line);
                let index = chars.iter().rposition(|(col, _)| *col <= cell).unwrap_or(0);
                let text: Vec<char> = chars.iter().map(|(_, c)| *c).collect();
                let cwd = tab.launch.cwd.clone();
                drop(term);
                crate::term::path_at(&text, index).map(|found| (found, cwd))
            } else {
                let selection = match clicks {
                    2 => term
                        .word_at(line, cell)
                        .map(|(a, b)| ((line, a), (line, b))),
                    n if n >= 3 => Some(((line, 0), (line, cols))),
                    _ => Some(((line, boundary), (line, boundary))),
                };
                drop(term);
                state.terminal.selection = selection;
                state.terminal.selecting = clicks < 2;
                None
            }
        };
        if let Some(((path, line, column), cwd)) = target {
            self.open_reference(&path, line, column, &cwd);
        }
        self.request_redraw();
        self.pump();
    }

    pub(super) fn terminal_drag(&self, x: f32, y: f32) {
        let Some((screen, cols, rows)) = self.terminal_screen() else {
            return;
        };
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.terminal.drag_point = Some((x, y));
            // Pulled inside the screen first: above it selects up to the
            // top row, not a row computed from a negative distance.
            let y = y.clamp(screen.y, screen.y + (screen.height - 1.0).max(0.0));
            let (row, boundary, _) =
                crate::platform::terminal::point(&state.renderer.atlas, screen, x, y, cols, rows);
            let back = state.terminal.back;
            let line = state.terminal.active_tab().map(|tab| {
                tab.session
                    .term
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .view_line(row, back)
            });
            if let (Some(line), Some((_, head))) = (line, state.terminal.selection.as_mut()) {
                *head = (line, boundary);
            }
        }
        self.request_redraw();
        self.pump();
    }

    /// A drag held past the top or bottom of the screen scrolls the
    /// history under it, by the frame, and the selection follows. Returns
    /// whether it scrolled, which is to say whether to ask again next frame.
    pub(super) fn terminal_autoscroll(&self) -> bool {
        let Some((screen, _, _)) = self.terminal_screen() else {
            return false;
        };
        let moved = {
            let Some(mut state) = self.state_mut() else {
                return false;
            };
            if !state.terminal.selecting {
                return false;
            }
            let Some((x, y)) = state.terminal.drag_point else {
                return false;
            };
            let past = if y < screen.y {
                y - screen.y
            } else if y > screen.y + screen.height {
                y - (screen.y + screen.height)
            } else {
                0.0
            };
            if past == 0.0 {
                state.autoscroll_carry = 0.0;
                return false;
            }
            let speed = past.signum() * (8.0 + past.abs() * 1.5).min(240.0);
            state.autoscroll_carry += speed * state.frame_interval.as_secs_f32();
            let lines = state.autoscroll_carry.trunc();
            state.autoscroll_carry -= lines;
            let history = state.terminal.active_tab().map_or(0, |tab| {
                tab.session
                    .term
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .scrollback_len()
            });
            // Up is into history: a negative distance raises `back`.
            let back = state.terminal.back as isize - lines as isize;
            state.terminal.back = back.clamp(0, history as isize) as usize;
            (x, y)
        };
        self.terminal_drag(moved.0, moved.1);
        true
    }

    /// Everything the session has: its history and its screen.
    pub(super) fn terminal_select_all(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some(tab) = state.terminal.active_tab() else {
            return;
        };
        let term = tab.session.term.lock().unwrap_or_else(|e| e.into_inner());
        let first = term.view_line(0, term.scrollback_len());
        let last = term.view_line(term.rows().saturating_sub(1), 0);
        let cols = term.cols();
        drop(term);
        state.terminal.selection = Some(((first, 0), (last, cols)));
        state.terminal.selecting = false;
        drop(state);
        self.request_redraw();
        self.pump();
    }

    /// The terminal's selection as text; with `one_line`, as one line
    /// for Copy as One Line.
    pub(super) fn terminal_selected_text(&self, one_line: bool) -> Option<String> {
        let state = self.state()?;
        let (a, b) = state.terminal.selection?;
        let tab = state.terminal.active_tab()?;
        let term = tab.session.term.lock().unwrap_or_else(|e| e.into_inner());
        let text = if one_line {
            term.one_line_between(a, b)
        } else {
            term.text_between(a, b)
        };
        (!text.is_empty()).then_some(text)
    }

    /// A key while the terminal has the keyboard. Always handled: anything
    /// unhandled would otherwise reach the text system and the document.
    pub(super) fn terminal_key(&self, event: &NSEvent) -> bool {
        let flags = event.modifierFlags();
        let code = event.keyCode();
        if flags.contains(NSEventModifierFlags::Command) {
            // The Mac line-editing shortcuts, as the shell's own keys.
            let bytes: &[u8] = match code {
                key::DELETE => b"\x15",
                key::LEFT => b"\x01",
                key::RIGHT => b"\x05",
                _ => return true,
            };
            self.terminal_write(bytes);
            return true;
        }
        let text = |s: Option<Retained<NSString>>| s.map(|s| s.to_string()).unwrap_or_default();
        let key = crate::term::keys::Key {
            code,
            chars: text(event.characters()),
            bare: text(event.charactersIgnoringModifiers()),
            shift: flags.contains(NSEventModifierFlags::Shift),
            control: flags.contains(NSEventModifierFlags::Control),
            option: flags.contains(NSEventModifierFlags::Option),
        };
        let app_cursor = self
            .ivars()
            .state
            .borrow()
            .terminal
            .active_tab()
            .is_some_and(|tab| {
                tab.session
                    .term
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .modes
                    .app_cursor
            });
        if let Some(bytes) = crate::term::keys::encode(&key, app_cursor) {
            self.terminal_write(&bytes);
        }
        true
    }
}

/// What a look at a tab found: its newest notification, whether it
/// printed, and the program's own word on its state when it did.
struct Noticed {
    index: usize,
    notice: Option<String>,
    printed: bool,
    status: Option<crate::term::status::Status>,
}

/// Watches a session's hook events file and wakes the window when it
/// grows, until the tab is gone. Once a second: a stat, nothing more.
fn watch_events(
    events: std::path::PathBuf,
    alive: std::sync::Weak<()>,
    wake: crate::platform::dispatch::Wake,
) {
    std::thread::spawn(move || {
        let mut seen = 0;
        while alive.strong_count() > 0 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let len = std::fs::metadata(&events).map_or(0, |m| m.len());
            if len != seen {
                seen = len;
                wake();
            }
        }
    });
}
