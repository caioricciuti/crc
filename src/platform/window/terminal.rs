//! The terminal panel in the window: its session and grid, keys, mouse
//! selection, and what it does after each frame.

use super::*;
use crate::platform::terminal::When;

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

    /// A session printed something, or its program exited. Output on
    /// screen draws; output off screen draws only for what it changed in
    /// the tab's row, title or status, at most ten times a second.
    pub(super) fn poll_terminal(&self) {
        let Some(state) = self.state() else {
            self.resume_display_link();
            return;
        };
        let now = Instant::now();
        let visible = state
            .terminal
            .visible(state.agents.on || state.terminal.open);
        let mut shown_printed = false;
        // What each tab that printed was doing before.
        let mut was = Vec::new();
        let mut noticed = Vec::new();
        let mut copied = None;
        for (index, tab) in state.terminal.tabs.iter().enumerate() {
            if tab.session.drain_wake() {
                shown_printed |= visible.contains(&index);
                was.push((index, tab.activity()));
                let (notices, status) = {
                    let mut term = tab.session.term.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(text) = term.take_clipboard() {
                        copied = Some(text);
                    }
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
        // A program copied (OSC 52, as Claude Code does on a drag it was
        // sent): onto the pasteboard, as Cmd-C would.
        if let Some(text) = copied {
            clipboard::write_text(&text);
        }
        let printed: Vec<usize> = noticed.iter().map(|n| n.index).collect();
        let told = !noticed.is_empty() && self.take_notices(noticed);
        // A prompt after a `cd`: where the terminal showing is now. Read on
        // a burst it prints, twice a second at most; one held back is read
        // when its time comes, so the prompt right after the `cd` is never
        // missed.
        let mut owed = None;
        let mut changed = told;
        let moved = self.state_mut().is_some_and(|mut state| {
            let active = state.terminal.active;
            let mut moved = false;
            for index in &printed {
                let Some(tab) = state.terminal.tabs.get_mut(*index) else {
                    continue;
                };
                match tab.probe(now) {
                    // Into or out of ssh, any tab: its row and title say so.
                    When::Now => {
                        tab.read_remote();
                        moved |= *index == active && tab.read_cwd();
                    }
                    When::At(at) => owed = Some(owed.map_or(at, |o: Instant| o.min(at))),
                    When::Never => {}
                }
            }
            // Off screen, what its row, title or status shows; looked at
            // on screen too, so it is current when the tab goes off.
            for (index, activity) in &was {
                if let Some(tab) = state.terminal.tabs.get_mut(*index) {
                    changed |= tab.face_changed(*activity) && !visible.contains(index);
                }
            }
            moved
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
        let when = self.state_mut().map(|mut state| {
            state
                .terminal
                .pace
                .decide(shown_printed || moved, changed, now)
        });
        match when {
            Some(When::Now) => {
                self.request_redraw();
                self.pump();
            }
            Some(When::At(at)) => self.arm_terminal_settle(at),
            Some(When::Never) | None => {}
        }
        if let Some(at) = owed {
            self.arm_terminal_settle(at);
        }
    }

    /// Schedules [`EditorView::terminal_settle`] for `at`, unless one runs
    /// sooner.
    fn arm_terminal_settle(&self, at: Instant) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if !state.terminal.pace.arm(at) {
            return;
        }
        drop(state);
        let delay = at.saturating_duration_since(Instant::now()).as_secs_f64();
        let _: () = unsafe {
            msg_send![self, performSelector: sel!(terminalSettle:),
                withObject: None::<&AnyObject>, afterDelay: delay]
        };
    }

    /// What output held back is owed: the redraw for a change in a tab off
    /// screen, and the reads of where a tab is and whether it runs ssh.
    pub(super) fn terminal_settle(&self) {
        let now = Instant::now();
        let (mut redraw, moved, next) = {
            // Busy: again shortly, as the one scheduled is still marked.
            let Some(mut state) = self.state_mut() else {
                let _: () = unsafe {
                    msg_send![self, performSelector: sel!(terminalSettle:),
                        withObject: None::<&AnyObject>,
                        afterDelay: crate::platform::terminal::BACKGROUND_EVERY.as_secs_f64()]
                };
                return;
            };
            let panel = &mut state.terminal;
            panel.pace.armed = None;
            let redraw = panel.pace.take_due(now);
            let mut next = panel.pace.due;
            let active = panel.active;
            let (mut remote, mut moved) = (false, false);
            for (index, tab) in panel.tabs.iter_mut().enumerate() {
                if !tab.probe_owed {
                    continue;
                }
                match tab.probe(now) {
                    When::Now => {
                        remote |= tab.read_remote();
                        moved |= index == active && tab.read_cwd();
                    }
                    When::At(at) => next = Some(next.map_or(at, |n: Instant| n.min(at))),
                    When::Never => {}
                }
            }
            (redraw || remote, moved, next)
        };
        if moved {
            self.follow_cwd();
            redraw = true;
        }
        if redraw {
            self.request_redraw();
            self.pump();
        }
        if let Some(at) = next {
            self.arm_terminal_settle(at);
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
        // What an agent's shell that started this editor left in our
        // environment: its markers, NO_COLOR, PAGER=cat.
        let unset = crate::platform::terminal::inherited_unset(
            std::env::vars_os().filter_map(|(name, _)| name.into_string().ok()),
        );
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
            // review hooks as extra settings, or with review off only the
            // one auto-continue hears: Claude merges them with the person's
            // own, and nothing in ~/.claude is touched.
            let command = match std::env::var("CRC_CLAUDE_COMMAND") {
                Ok(command) => command,
                Err(_) => match (state.agent_review || state.auto_continue)
                    .then(|| crate::project::review::claude_settings_arg(state.agent_review))
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
                .map(|i| state.agents.tasks[i].dir())
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
                            None => crate::platform::canonical(dir) == t.dir(),
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
                    receipt: Default::default(),
                    review_files: 0,
                    folder: crate::platform::canonical(&launch.cwd),
                    workspace,
                    alive: std::sync::Arc::new(()),
                    cwd: crate::platform::canonical(&launch.cwd),
                    program_pid: None,
                    remote: None,
                    told_focus: false,
                    last_typed: None,
                    last_probe: None,
                    probe_owed: false,
                    retry: None,
                    last_return: None,
                    marks: Default::default(),
                    notified: false,
                    seen: None,
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
            // About the terminals, which both modes have: a switch to
            // Agents, where they show, must not clear it.
            state.tell(Said::new(layout::Feedback::Info, said.clone()).everywhere());
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
                    receipt: crate::project::review::receipt_until(&spec.review, left.events_read),
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
                    told_focus: false,
                    last_typed: None,
                    last_probe: None,
                    probe_owed: false,
                    retry: None,
                    last_return: None,
                    marks: Default::default(),
                    notified: false,
                    seen: None,
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

    /// Cmd-F with the terminal's keys: the find bar over its screen,
    /// seeded from a one-line selection, on the newest match.
    pub(super) fn open_terminal_find(&self) {
        let seed = self
            .terminal_selected_text(false)
            .filter(|t| !t.contains('\n') && t.len() < 200);
        self.terminal_find_bar(seed.as_deref());
        self.terminal_find_sync(true);
        self.terminal_find_reveal();
        self.request_redraw();
        self.pump();
    }

    /// A match Find in Terminals listed: the find bar over the active
    /// session with `query`, on the match at `start` (or the next after
    /// it, should the output have moved on).
    pub(super) fn open_terminal_find_at(&self, query: &str, start: (u64, usize)) {
        self.terminal_find_bar(Some(query));
        self.terminal_find_sync(true);
        // After the search starts: one for another session forgets the
        // anchor. The answer lands on it; matches already for this query
        // are placed on it now.
        if let Some(mut state) = self.state_mut()
            && let Some(find) = state.find.as_mut().and_then(|bar| bar.terminal.as_mut())
        {
            find.anchor = Some(start);
            if find.searching() {
                find.reveal = true;
            } else {
                let matches = std::mem::take(&mut find.matches);
                find.take(matches);
            }
        }
        self.terminal_find_reveal();
        self.request_redraw();
        self.pump();
    }

    /// The find bar over the terminal, with the keys, `seed` its query
    /// when given; the rest of it as it was.
    fn terminal_find_bar(&self, seed: Option<&str>) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        close_fields(&mut state);
        if let Some(cancel) = state.project_search.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        state.project_search.rx = None;
        // A document's bar moves over: its options come with it. A new
        // bar starts on the last search, as the document's does.
        let last = state.last_find.clone();
        let bar = state.find.get_or_insert_with(|| {
            let (query, options) = last.unwrap_or_default();
            FindBar {
                query: Buffer::from_text(&query),
                replacement: Buffer::new(),
                replacing: false,
                show_replace: false,
                has_keys: true,
                options,
                terminal: None,
            }
        });
        if let Some(seed) = seed {
            bar.query = Buffer::new();
            bar.query.insert(seed);
        }
        bar.query.select_all();
        bar.has_keys = true;
        bar.replacing = false;
        bar.terminal.get_or_insert_with(Default::default);
    }

    /// Searches the active session again when its output, the query, the
    /// options or the session changed since the last search, on a worker
    /// (see [`crate::platform::terminal::Find::sync`]); the answer is
    /// taken by [`EditorView::poll_terminal_find`]. Output alone is
    /// searched again at most every so often; until then a frame is asked
    /// for to look again. `forced` (the query or a step) waits for the
    /// session's lock rather than trying again next frame, has the current
    /// match scrolled into view once found, and says an invalid pattern
    /// in the status line.
    pub(super) fn terminal_find_sync(&self, forced: bool) {
        use crate::platform::terminal::Synced;
        let synced = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let State {
                find,
                terminal,
                message,
                ..
            } = &mut *state;
            let Some(bar) = find.as_mut() else {
                return;
            };
            let (query, options) = (bar.query.rope.to_string(), bar.options);
            let Some(find) = bar.terminal.as_mut() else {
                return;
            };
            let Some(tab) = terminal.active_tab() else {
                return;
            };
            // A frame does not wait while the reader holds the lock for a
            // burst of output: the next one looks again.
            let term = if forced {
                Some(tab.session.term.lock().unwrap_or_else(|e| e.into_inner()))
            } else {
                match tab.session.term.try_lock() {
                    Ok(term) => Some(term),
                    Err(std::sync::TryLockError::Poisoned(term)) => Some(term.into_inner()),
                    Err(std::sync::TryLockError::WouldBlock) => None,
                }
            };
            match term {
                None => Synced::Later,
                Some(term) => {
                    let key = (
                        Arc::as_ptr(&tab.session.term) as usize,
                        term.generation,
                        query,
                        options,
                    );
                    let synced = find.sync(key, &term, forced);
                    drop(term);
                    match &synced {
                        Synced::Started if forced => find.reveal = true,
                        Synced::Done(Some(error)) if forced => {
                            *message = Some((format!("invalid regex: {error}"), Instant::now()));
                        }
                        _ => {}
                    }
                    synced
                }
            }
        };
        match synced {
            Synced::Fresh => {}
            // The worker's answer is polled for by the display link.
            Synced::Started => self.resume_display_link(),
            Synced::Later | Synced::Done(_) => {
                self.request_redraw();
                self.resume_display_link();
            }
        }
    }

    /// Takes the terminal search's answer once it is there: drawn, with
    /// the current match scrolled into view when that was asked for, and a
    /// step that waited for it taken.
    pub(super) fn poll_terminal_find(&self) {
        let reveal = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let State { find, message, .. } = &mut *state;
            let Some(bar) = find.as_mut() else {
                return;
            };
            let query = bar.query.rope.to_string();
            let Some(find) = bar.terminal.as_mut() else {
                return;
            };
            let stepping = find.pending_step.is_some();
            if !find.poll() {
                // Taking a while: the bar says it is searching, once.
                if find.searching_shown() && !std::mem::replace(&mut find.said_searching, true) {
                    drop(state);
                    self.request_redraw();
                }
                return;
            }
            if stepping && find.matches.is_empty() && !query.is_empty() {
                *message = Some((format!("no match for {query:?}"), Instant::now()));
            }
            std::mem::take(&mut find.reveal)
        };
        if reveal {
            self.terminal_find_reveal();
        }
        self.request_redraw();
    }

    /// Scrolls the active session so its current match shows below the
    /// bar. Not while the matches are another query's: the new ones are
    /// shown when they come.
    pub(super) fn terminal_find_reveal(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let covered = crate::platform::terminal::find_bar_rows(&state.renderer.atlas);
        let State { find, terminal, .. } = &mut *state;
        let Some(line) = find
            .as_ref()
            .and_then(|bar| bar.terminal.as_ref())
            .filter(|find| !find.searching())
            .and_then(|find| find.current_hit())
            .map(|hit| hit.start.0)
        else {
            return;
        };
        let Some(tab) = terminal.active_tab() else {
            return;
        };
        let term = tab.session.term.lock().unwrap_or_else(|e| e.into_inner());
        let back = crate::platform::terminal::back_showing(
            line,
            term.view_line(0, 0),
            terminal.back,
            term.rows(),
            covered,
            term.scrollback_len(),
        );
        drop(term);
        terminal.back = back;
    }

    /// Enter, Cmd-G and the bar's arrows over the terminal: the next or
    /// previous match, around the ends, scrolled into view. While the
    /// matches of a new query are on their way, the step waits for them.
    pub(super) fn terminal_find_step(&self, forward: bool) {
        self.terminal_find_sync(true);
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let State { find, message, .. } = &mut *state;
            let Some(bar) = find.as_mut() else {
                return;
            };
            let query = bar.query.rope.to_string();
            let Some(find) = bar.terminal.as_mut() else {
                return;
            };
            if find.searching() {
                find.pending_step = Some(forward);
                return;
            }
            find.step(forward);
            if find.matches.is_empty() && find.error.is_none() && !query.is_empty() {
                *message = Some((format!("no match for {query:?}"), Instant::now()));
            }
        }
        self.terminal_find_reveal();
        self.request_redraw();
        self.pump();
    }

    /// Whether filter mode's list shows over the terminal.
    pub(super) fn terminal_filter_open(&self) -> bool {
        self.state().is_some_and(|state| {
            state
                .find
                .as_ref()
                .and_then(|bar| bar.terminal.as_ref())
                .is_some_and(|find| find.filter.is_some())
        })
    }

    /// Option-Return and the bar's Filter chip: the list of the lines
    /// with a match shows over the screen, on the current match's line,
    /// with the keys in the bar; or it goes.
    pub(super) fn toggle_terminal_filter(&self) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let visible = filter_visible(&state);
            let Some(bar) = state.find.as_mut() else {
                return;
            };
            bar.has_keys = true;
            bar.replacing = false;
            let Some(find) = bar.terminal.as_mut() else {
                return;
            };
            find.filter = match find.filter {
                Some(_) => None,
                None => {
                    let selected = find.current_row();
                    Some(crate::platform::terminal::Filter {
                        selected,
                        scroll: (selected + 1).saturating_sub(visible),
                    })
                }
            };
        }
        self.request_redraw();
        self.pump();
    }

    /// Up and Down in filter mode's list: the row chosen moves by `by`,
    /// kept in view.
    pub(super) fn terminal_filter_move(&self, by: isize) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let visible = filter_visible(&state).max(1);
            let Some(find) = state.find.as_mut().and_then(|bar| bar.terminal.as_mut()) else {
                return;
            };
            let last = find.rows.len().saturating_sub(1);
            let Some(filter) = find.filter.as_mut() else {
                return;
            };
            filter.selected = filter.selected.saturating_add_signed(by).min(last);
            if filter.selected < filter.scroll {
                filter.scroll = filter.selected;
            } else if filter.selected >= filter.scroll + visible {
                filter.scroll = filter.selected + 1 - visible;
            }
        }
        self.request_redraw();
        self.pump();
    }

    /// The wheel over filter mode's list: `lines` rows on, as far as it
    /// goes.
    pub(super) fn terminal_filter_wheel(&self, lines: isize) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let visible = filter_visible(&state);
            let Some(find) = state.find.as_mut().and_then(|bar| bar.terminal.as_mut()) else {
                return;
            };
            let most = find.rows.len().saturating_sub(visible);
            let Some(filter) = find.filter.as_mut() else {
                return;
            };
            filter.scroll = filter.scroll.saturating_add_signed(lines).min(most);
        }
        self.request_redraw();
        self.pump();
    }

    /// Return or a click on filter mode's row `row` (the chosen one when
    /// `None`): its line's first match is the current one, scrolled into
    /// view, and the list goes.
    pub(super) fn terminal_filter_pick(&self, row: Option<usize>) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(find) = state.find.as_mut().and_then(|bar| bar.terminal.as_mut()) else {
                return;
            };
            let Some(filter) = find.filter.take() else {
                return;
            };
            let row = row.unwrap_or(filter.selected);
            let Some(hit) = find.rows.get(row).map(|r| r.hit) else {
                return;
            };
            if let Some(start) = find.matches.get(hit).map(|h| h.start) {
                find.current = Some(hit);
                find.anchor = Some(start);
            }
        }
        self.terminal_find_reveal();
        self.request_redraw();
        self.pump();
    }

    /// After a frame: the terminal's find bar goes with the panel, and
    /// follows the output while it is open.
    fn terminal_find_after_frame(&self) {
        let gone = self.state().is_some_and(|state| {
            state
                .find
                .as_ref()
                .is_some_and(|bar| bar.terminal.is_some())
                && !(state.terminal.open && !state.terminal.tabs.is_empty())
        });
        if gone {
            if let Some(mut state) = self.state_mut()
                && let Some(bar) = state.find.take()
            {
                super::find::remember_find(&mut state, &bar);
            }
            self.request_redraw();
            self.resume_display_link();
            return;
        }
        self.terminal_find_sync(false);
    }

    /// Keeps every session's grid the size of the panel.
    pub(super) fn terminal_after_frame(&self) {
        self.terminal_find_after_frame();
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

    /// Cmd-K: the active session's history and screen go, its cursor's
    /// line moved to the top, as in Terminal. Done here, not by the
    /// program: nothing is typed into it, so a half-typed command or a
    /// running one is left alone.
    pub(super) fn clear_terminal(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if super::agents::active_hidden(&state) {
            return;
        }
        let panel = &mut state.terminal;
        panel.back = 0;
        panel.selection = None;
        panel.selecting = false;
        if let Some(tab) = panel.active_tab_mut() {
            let mut term = tab.session.term.lock().unwrap_or_else(|e| e.into_inner());
            term.clear_all();
            // Its prompt marks go with the history.
            tab.marks.trim(term.first_line());
        }
        drop(state);
        self.request_redraw();
        self.pump();
    }

    /// Cmd-Shift-Up and Down over a terminal: the view scrolled to the
    /// prompt sent to its agent before or after the one at its top, with
    /// what led to it above; past the last, back to the output. Nothing
    /// over a full-screen program, which has no history.
    pub(super) fn terminal_jump_prompt(&self, forward: bool) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if super::agents::active_hidden(&state) {
            return;
        }
        // Under the find bar, a prompt jumped to shows below it.
        let covered = if state
            .find
            .as_ref()
            .is_some_and(|bar| bar.terminal.is_some())
        {
            crate::platform::terminal::find_bar_rows(&state.renderer.atlas)
        } else {
            0
        };
        let panel = &mut state.terminal;
        let back = panel.back;
        let Some(tab) = panel.active_tab_mut() else {
            return;
        };
        let term = tab.session.term.lock().unwrap_or_else(|e| e.into_inner());
        if term.is_alternate() {
            return;
        }
        tab.marks.trim(term.first_line());
        let history = term.scrollback_len();
        let base = term.view_line(0, 0);
        // The row a prompt jumped to sits on.
        let at =
            term.view_line(0, back) + (crate::platform::terminal::PROMPT_CONTEXT + covered) as u64;
        let target = if forward {
            tab.marks.next(at)
        } else {
            tab.marks.prev(at)
        };
        let back = match target {
            Some(line) => crate::platform::terminal::prompt_back(line, base, history, covered),
            None if forward => 0,
            None => return,
        };
        drop(term);
        panel.back = back;
        drop(state);
        self.request_redraw();
        self.pump();
    }

    /// A press on a find tick beside the screen: the bar goes to that
    /// match, as stepping to it would. False when the press is not on one.
    fn terminal_find_tick_press(&self, x: f32, y: f32) -> bool {
        let Some((screen, _, _)) = self.terminal_screen() else {
            return false;
        };
        let track = crate::platform::terminal::mark_track(screen);
        if !track.contains(x, y) {
            return false;
        }
        {
            let Some(mut state) = self.state_mut() else {
                return false;
            };
            let State { find, terminal, .. } = &mut *state;
            let Some(find) = find.as_mut().and_then(|bar| bar.terminal.as_mut()) else {
                return false;
            };
            let Some(tab) = terminal.active_tab() else {
                return false;
            };
            let term = tab.session.term.lock().unwrap_or_else(|e| e.into_inner());
            if term.is_alternate() {
                return false;
            }
            let first = term.first_line();
            let total = term.scrollback_len() + term.rows();
            drop(term);
            let Some(index) =
                crate::platform::terminal::find_tick_at(track, y, &find.matches, first, total)
            else {
                return false;
            };
            find.current = Some(index);
            find.anchor = Some(find.matches[index].start);
        }
        self.terminal_find_reveal();
        self.request_redraw();
        self.pump();
        true
    }

    /// A press on a prompt's mark beside the screen: the view scrolled to
    /// that prompt, as Cmd-Shift-Up would. False when the press is not on
    /// one, to go on as any press does. A find tick under the press wins,
    /// while the bar is open.
    fn terminal_mark_press(&self, x: f32, y: f32) -> bool {
        if self.terminal_find_tick_press(x, y) {
            return true;
        }
        let Some((screen, _, _)) = self.terminal_screen() else {
            return false;
        };
        let track = crate::platform::terminal::mark_track(screen);
        if !track.contains(x, y) {
            return false;
        }
        let Some(mut state) = self.state_mut() else {
            return false;
        };
        // Under the find bar, a prompt jumped to shows below it.
        let covered = if state
            .find
            .as_ref()
            .is_some_and(|bar| bar.terminal.is_some())
        {
            crate::platform::terminal::find_bar_rows(&state.renderer.atlas)
        } else {
            0
        };
        let back = {
            let Some(tab) = state.terminal.active_tab().filter(|t| !t.marks.is_empty()) else {
                return false;
            };
            let term = tab.session.term.lock().unwrap_or_else(|e| e.into_inner());
            if term.is_alternate() {
                return false;
            }
            let first = term.first_line();
            let total = term.scrollback_len() + term.rows();
            let lines = tab.marks.iter().map(|m| m.line);
            let Some(line) = crate::platform::terminal::mark_at(track, y, lines, first, total)
            else {
                return false;
            };
            crate::platform::terminal::prompt_back(
                line,
                term.view_line(0, 0),
                term.scrollback_len(),
                covered,
            )
        };
        state.terminal.back = back;
        state.terminal.selection = None;
        drop(state);
        self.request_redraw();
        self.pump();
        true
    }

    pub(super) fn terminal_write(&self, bytes: &[u8]) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        // The selected workspace has no terminal: nothing goes to the
        // other workspace's behind its empty state.
        if super::agents::active_hidden(&state) {
            return;
        }
        state.terminal.back = 0;
        state.terminal.selection = None;
        let mut answered = false;
        if let Some(tab) = state.terminal.active_tab_mut() {
            answered = tab.type_in(bytes);
        }
        if answered {
            super::agents::sync_dock_badge(&state, MainThreadMarker::from(self));
        }
    }

    /// Schedules the next look at auto-continue's tries, in place of any:
    /// when the soonest is due. Nothing when none is.
    pub(super) fn arm_auto_continue(&self) {
        let _: () = unsafe {
            msg_send![
                objc2::class!(NSObject),
                cancelPreviousPerformRequestsWithTarget: self,
                selector: sel!(autoContinueTick:),
                object: None::<&AnyObject>
            ]
        };
        let Some(next) = self.state().and_then(|state| {
            state
                .terminal
                .tabs
                .iter()
                .filter_map(|tab| tab.retry?.next_look())
                .min()
        }) else {
            return;
        };
        let wait = next.saturating_duration_since(Instant::now()).as_secs_f64();
        let _: () = unsafe {
            msg_send![self, performSelector: sel!(autoContinueTick:),
                withObject: None::<&AnyObject>, afterDelay: wait.max(0.05)]
        };
    }

    /// Auto-continue: types "continue" into each session whose try came
    /// due, and its Return a moment after. Each is checked again right
    /// before: a session typed into since its error is the person's, and
    /// one at work or being typed into now waits a second more. What crc
    /// types is not the person typing: `last_typed` stays theirs.
    pub(super) fn auto_continue_tick(&self) {
        use crate::platform::terminal::Activity;
        let now = Instant::now();
        let mut changed = false;
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let showing = state.terminal.has_keys().then_some(state.terminal.active);
            for (index, tab) in state.terminal.tabs.iter_mut().enumerate() {
                let Some(mut retry) = tab.retry else {
                    continue;
                };
                if retry.typed_since(tab.last_typed) {
                    tab.retry = None;
                    changed = true;
                    continue;
                }
                if retry.enter_due(now, tab.last_typed) {
                    tab.session.write(b"\r");
                    retry.enter_at = None;
                }
                if retry.next_at.is_some_and(|at| now >= at) {
                    if !retry.due(now, tab.last_typed, showing == Some(index))
                        || tab.activity() == Activity::Working
                    {
                        retry.next_at = Some(now + Duration::from_secs(1));
                    } else {
                        let bracketed = tab
                            .session
                            .term
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .modes
                            .bracketed_paste;
                        tab.session.write(&crate::term::keys::paste(
                            crate::platform::retry::WORD,
                            bracketed,
                        ));
                        retry.sent(now);
                        // Answered, as typing would: no longer waiting.
                        tab.attention = None;
                        if tab
                            .session
                            .term
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .status
                            .acknowledge()
                        {
                            tab.status = None;
                        }
                        changed = true;
                    }
                }
                tab.retry = Some(retry);
            }
            if changed {
                super::agents::sync_dock_badge(&state, MainThreadMarker::from(self));
            }
        }
        self.arm_auto_continue();
        if changed {
            self.request_redraw();
            self.pump();
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
    /// the tab is not the one being typed into. Returns whether the
    /// window says something of it beyond the tabs' own rows: the status
    /// line, a workspace written in, a team's tasks.
    fn take_notices(&self, noticed: Vec<Noticed>) -> bool {
        use crate::term::status::State;
        let Some(mut state) = self.state_mut() else {
            return false;
        };
        let now = Instant::now();
        let mut said = None;
        let watching = state.terminal.has_keys();
        let active = state.terminal.active;
        let mut wrote = false;
        let mut asked = false;
        let mut team_events = Vec::new();
        let mut written = Vec::new();
        let mut retry = false;
        let mut ran_any = false;
        let first_delay = crate::platform::retry::first_delay();
        let auto_continue = state.auto_continue;
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
            // Its prompt marks: gone with the history they were in, and
            // moved onto their prompts as the agent prints them.
            if printed && !tab.marks.is_empty() {
                let term = tab.session.term.lock().unwrap_or_else(|e| e.into_inner());
                tab.marks.trim(term.first_line());
                tab.marks.resolve(&term);
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
                    crate::project::review::Event::Asked(text) => {
                        // A turn that ended well: the next error starts
                        // a new streak.
                        if text == crate::project::review::FINISHED {
                            tab.retry = None;
                        }
                        notice = Some(text);
                    }
                    crate::project::review::Event::ApiError { error, details } => {
                        // The turn ended, as Stop says, on an error: the
                        // tab waits, and a temporary one is tried again.
                        if auto_continue && crate::platform::retry::temporary(&error, &details) {
                            tab.retry = Some(crate::platform::retry::Retry::after_error(
                                tab.retry.as_ref(),
                                now,
                                first_delay,
                            ));
                            retry |= tab.retrying();
                        } else {
                            tab.retry = None;
                        }
                        notice = Some(if details.is_empty() {
                            format!("API error: {error}")
                        } else {
                            details
                        });
                    }
                    crate::project::review::Event::Failed(text) => {
                        said = Some(format!("{}: review hook: {text}", tab.name()));
                    }
                    crate::project::review::Event::Prompted(text) => {
                        // Where Return was typed, or failing that (a
                        // prompt crc typed) where the cursor is now.
                        let term = tab.session.term.lock().unwrap_or_else(|e| e.into_inner());
                        let anchor = tab
                            .last_return
                            .take()
                            .unwrap_or_else(|| term.view_line(term.cursor().0, 0));
                        let end = term.view_line(term.rows().saturating_sub(1), 0);
                        tab.marks.add(anchor, &text, end, term.rows());
                        tab.marks.trim(term.first_line());
                        tab.marks.resolve(&term);
                    }
                    crate::project::review::Event::Ran(ran) => {
                        tab.receipt.note(ran);
                        ran_any = true;
                    }
                    event @ (crate::project::review::Event::Task { .. }
                    | crate::project::review::Event::TeammateIdle(_)) => {
                        team_events.push((tab.review.clone(), event));
                    }
                }
            }
            // A bell right after a keystroke answers the key, not you.
            if let Some(text) = notice.filter(|text| !text.is_empty() || tab.heeds_bell(now)) {
                asked |= tab.attention.is_none();
                tab.attention = Some(text);
                if !(watching && index == active) {
                    said = Some(format!("{}: {}", tab.name(), tab.state()));
                }
            }
        }
        let told = said.is_some() || wrote || asked || ran_any || !team_events.is_empty();
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
        drop(state);
        if retry {
            self.arm_auto_continue();
        }
        told
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
        // Where its program asked for the mouse that is all it does, as in
        // other terminals: the program does not hear the press that only
        // chose its pane.
        let mut switched = false;
        if let Some(rect) = self.chrome().terminal
            && let Some(mut state) = self.state_mut()
            && state.terminal.shown_pair().is_some()
        {
            let (_, screen) = crate::platform::terminal::split(rect);
            let tab = state.terminal.screen_at(screen, x);
            if tab != state.terminal.active {
                state.terminal.show(tab);
                switched = true;
            }
            state.terminal.focus = true;
        }
        if switched && !mouse_kept(event.modifierFlags()) && self.mouse_pane(x, y).is_some() {
            self.request_redraw();
            self.pump();
            return;
        }
        // A prompt's mark, beside the grid: crc's, even over a program
        // that asked for the mouse.
        if self.terminal_mark_press(x, y) {
            return;
        }
        if self.terminal_mouse_press(event, x, y, crate::term::mouse::Button::Left) {
            return;
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
                if let Some(link) = term.link_at(line, cell) {
                    drop(term);
                    Some(Opened::Url(link.url))
                } else {
                    let chars = term.line_chars(line);
                    let index = chars.iter().rposition(|(col, _)| *col <= cell).unwrap_or(0);
                    let text: Vec<char> = chars.iter().map(|(_, c)| *c).collect();
                    let cwd = tab.launch.cwd.clone();
                    drop(term);
                    crate::term::path_at(&text, index).map(|found| Opened::Path(found, cwd))
                }
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
        match target {
            Some(Opened::Path((path, line, column), cwd)) => {
                self.open_reference(&path, line, column, &cwd);
            }
            Some(Opened::Url(url)) => self.open_terminal_link(url),
            None => {}
        }
        self.request_redraw();
        self.pump();
    }

    /// A link Command-clicked in a terminal, in the browser, or for a
    /// `file://` one in the app for its kind. A self-test opens nothing
    /// and keeps what it would have opened.
    fn open_terminal_link(&self, url: String) {
        if self.ivars().testing {
            if let Some(mut state) = self.state_mut() {
                state.terminal.opened.push(url);
            }
            return;
        }
        if let Err(e) = self.open(false, std::ffi::OsStr::new(&url))
            && let Some(mut state) = self.state_mut()
        {
            state.say(
                layout::Feedback::Failure,
                format!("Could not open {url}: {e}"),
            );
        }
    }

    /// With Command held, the link under the pointer, underlined and with
    /// the pointing hand, as other terminals show what a Command-click
    /// opens. Worked out when the pointer moves or the modifiers change,
    /// never per frame; without Command, or off the screen, there is none.
    pub(super) fn terminal_link_hover(&self, x: f32, y: f32, flags: NSEventModifierFlags) {
        let found = (flags.contains(NSEventModifierFlags::Command) && x.is_finite())
            .then(|| self.link_under(x, y))
            .flatten();
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if state.terminal.link == found {
            return;
        }
        let shown = found.is_some();
        let was = std::mem::replace(&mut state.terminal.link, found).is_some();
        drop(state);
        // The cursor rectangles follow with the frame's hotspots; the
        // cursor itself is set now, as the pointer may not move again.
        if shown {
            NSCursor::pointingHandCursor().set();
        } else if was && self.mouse_screen(x, y).is_some() {
            NSCursor::arrowCursor().set();
        }
        self.request_redraw();
        self.pump();
    }

    /// The link under a point in a terminal's screen, and its tab.
    fn link_under(&self, x: f32, y: f32) -> Option<(usize, crate::term::Link)> {
        let (tab, screen) = self.mouse_screen(x, y)?;
        let state = self.state()?;
        let session = state.terminal.tabs.get(tab)?;
        let term = session.session.term.lock();
        let term = term.unwrap_or_else(|e| e.into_inner());
        let (row, _, col) = crate::platform::terminal::point(
            &state.renderer.atlas,
            screen,
            x,
            y,
            term.cols(),
            term.rows(),
        );
        let back = if tab == state.terminal.active {
            state.terminal.back
        } else {
            0
        };
        let link = term.link_at(term.view_line(row, back), col)?;
        Some((tab, link))
    }

    /// The tab whose screen is under a point, and that screen, when the
    /// terminal shows one there.
    fn mouse_screen(&self, x: f32, y: f32) -> Option<(usize, Viewport)> {
        let chrome = self.chrome();
        let rect = chrome.terminal?;
        let state = self.state()?;
        if state.palette.is_some()
            || !state.terminal.showing()
            || super::agents::active_hidden(&state)
            || find_rect(&state, &chrome).is_some_and(|r| r.contains(x, y))
        {
            return None;
        }
        let (_, screen) = crate::platform::terminal::split(rect);
        state
            .terminal
            .screens(screen)
            .into_iter()
            .find(|(_, s)| s.contains(x, y))
    }

    /// The pane under a point whose program asked for the mouse: its tab,
    /// its screen and grid size. None when no program there did, when the
    /// view is scrolled back into history, or when the point is off the
    /// screen or on the find bar over it.
    fn mouse_pane(&self, x: f32, y: f32) -> Option<(usize, Viewport, usize, usize)> {
        let chrome = self.chrome();
        let rect = chrome.terminal?;
        let state = self.state()?;
        if state.palette.is_some()
            || super::agents::active_hidden(&state)
            || find_rect(&state, &chrome).is_some_and(|r| r.contains(x, y))
        {
            return None;
        }
        let (_, screen) = crate::platform::terminal::split(rect);
        let (tab, screen) = state
            .terminal
            .screens(screen)
            .into_iter()
            .find(|(_, s)| s.contains(x, y))?;
        if tab == state.terminal.active && state.terminal.back > 0 {
            return None;
        }
        let term = state.terminal.tabs.get(tab)?.session.term.lock();
        let term = term.unwrap_or_else(|e| e.into_inner());
        (term.modes.mouse != crate::term::mouse::Tracking::Off)
            .then(|| (tab, screen, term.cols(), term.rows()))
    }

    /// The cell (row, column) under a point in a pane's screen, clamped
    /// into its grid.
    fn mouse_cell(
        &self,
        screen: Viewport,
        x: f32,
        y: f32,
        cols: usize,
        rows: usize,
    ) -> Option<(usize, usize)> {
        let state = self.state()?;
        let (row, _, col) =
            crate::platform::terminal::point(&state.renderer.atlas, screen, x, y, cols, rows);
        Some((row, col))
    }

    /// Writes the report of a mouse event to a tab's program, in the
    /// encoding it asked for, if it asked for this kind of event.
    fn mouse_report(&self, tab: usize, event: crate::term::mouse::Event) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some(tab) = state.terminal.tabs.get_mut(tab) else {
            return;
        };
        let modes = tab
            .session
            .term
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .modes;
        if let Some(bytes) = crate::term::mouse::encode(&event, &modes) {
            tab.session.write(&bytes);
        }
    }

    /// A press in a pane whose program asked for the mouse goes to the
    /// program instead of starting a selection. Option or Shift keeps it
    /// for crc (Option-drag selects, Shift as in xterm), and so does
    /// Command, which opens the file reference under it. Returns whether
    /// the press was reported.
    pub(super) fn terminal_mouse_press(
        &self,
        event: &NSEvent,
        x: f32,
        y: f32,
        button: crate::term::mouse::Button,
    ) -> bool {
        let flags = event.modifierFlags();
        if mouse_kept(flags) {
            return false;
        }
        let Some((tab, screen, cols, rows)) = self.mouse_pane(x, y) else {
            return false;
        };
        // A press in the other pane of a pair only gives it the keyboard.
        if self
            .state()
            .is_none_or(|state| tab != state.terminal.active)
        {
            return false;
        }
        let Some(cell) = self.mouse_cell(screen, x, y, cols, rows) else {
            return false;
        };
        {
            let Some(mut state) = self.state_mut() else {
                return false;
            };
            state.terminal.focus = true;
            state.sidebar_keys = false;
            state.terminal.selection = None;
            state.terminal.selecting = false;
            state.terminal.mouse_press = Some((tab, button, cell));
            // The program hears it has the keyboard before the press.
            let key = self.key_window();
            report_focus(&mut state, key);
        }
        self.mouse_report(
            tab,
            mouse_event(crate::term::mouse::Action::Press, Some(button), cell, flags),
        );
        self.request_redraw();
        self.pump();
        true
    }

    /// The pointer moved with a reported press held: motion goes to the
    /// program, at 1002 and 1003, each time it reaches another cell, held
    /// to the pane's grid past its edges. Returns whether a press is held.
    pub(super) fn terminal_mouse_drag(&self, event: &NSEvent, x: f32, y: f32) -> bool {
        let Some((tab, button, last)) = self.state().and_then(|state| state.terminal.mouse_press)
        else {
            return false;
        };
        let Some((screen, cols, rows)) = self.pane_grid(tab) else {
            return true;
        };
        let Some(cell) = self.mouse_cell(screen, x, y, cols, rows) else {
            return true;
        };
        if cell == last {
            return true;
        }
        if let Some(mut state) = self.state_mut() {
            state.terminal.mouse_press = Some((tab, button, cell));
        }
        let flags = event.modifierFlags();
        self.mouse_report(
            tab,
            mouse_event(
                crate::term::mouse::Action::Motion,
                Some(button),
                cell,
                flags,
            ),
        );
        true
    }

    /// The button of a reported press went up: the release goes to the
    /// program that had the press, at the cell under the pointer held to
    /// its grid, even off its screen. Returns whether a press was held.
    pub(super) fn terminal_mouse_release(&self, event: &NSEvent, x: f32, y: f32) -> bool {
        let Some((tab, button, last)) = self
            .state_mut()
            .and_then(|mut state| state.terminal.mouse_press.take())
        else {
            return false;
        };
        let cell = self
            .pane_grid(tab)
            .and_then(|(screen, cols, rows)| self.mouse_cell(screen, x, y, cols, rows))
            .unwrap_or(last);
        let flags = event.modifierFlags();
        self.mouse_report(
            tab,
            mouse_event(
                crate::term::mouse::Action::Release,
                Some(button),
                cell,
                flags,
            ),
        );
        true
    }

    /// The pointer moved with no button down: a program that asked for
    /// all motion (1003) hears each new cell under it.
    pub(super) fn terminal_mouse_hover(&self, event: &NSEvent, x: f32, y: f32) {
        if self
            .state()
            .is_none_or(|state| state.terminal.mouse_press.is_some() || !state.terminal.open)
        {
            return;
        }
        let flags = event.modifierFlags();
        let target = (!mouse_kept(flags))
            .then(|| self.mouse_pane(x, y))
            .flatten()
            .and_then(|(tab, screen, cols, rows)| {
                Some((tab, self.mouse_cell(screen, x, y, cols, rows)?))
            });
        let Some((tab, cell)) = target else {
            if let Some(mut state) = self.state_mut() {
                state.terminal.mouse_cell = None;
            }
            return;
        };
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if state.terminal.mouse_cell == Some((tab, cell)) {
                return;
            }
            state.terminal.mouse_cell = Some((tab, cell));
        }
        self.mouse_report(
            tab,
            mouse_event(crate::term::mouse::Action::Motion, None, cell, flags),
        );
    }

    /// The wheel over the terminal, `lines` of it, negative up: reported
    /// as wheel buttons, one per line, to a program that asked for the
    /// mouse; as arrow keys, one per line, to one on the alternate screen
    /// that did not (xterm's alternate scroll, so a pager still moves);
    /// and otherwise a scroll through the history.
    pub(super) fn terminal_wheel(&self, x: f32, y: f32, lines: isize, flags: NSEventModifierFlags) {
        // Over filter mode's list, the wheel scrolls the list.
        let over_filter = self.state().is_some_and(|state| {
            filter_rect(&state, &chrome_of(&state)).is_some_and(|list| list.contains(x, y))
        });
        if over_filter {
            self.terminal_filter_wheel(lines);
            return;
        }
        let target = (lines != 0 && !mouse_kept(flags))
            .then(|| self.mouse_pane(x, y))
            .flatten()
            .and_then(|(tab, screen, cols, rows)| {
                Some((tab, self.mouse_cell(screen, x, y, cols, rows)?))
            });
        if let Some((tab, cell)) = target {
            use crate::term::mouse::{Action, Button};
            let button = if lines < 0 {
                Button::WheelUp
            } else {
                Button::WheelDown
            };
            for _ in 0..lines.unsigned_abs() {
                self.mouse_report(tab, mouse_event(Action::Press, Some(button), cell, flags));
            }
            return;
        }
        // The alternate screen has no history to scroll: the pane's program
        // gets the wheel as cursor keys, in the mode it set.
        let alternate = (lines != 0 && !mouse_kept(flags))
            .then(|| self.mouse_screen(x, y))
            .flatten()
            .map(|(tab, _)| tab);
        if let Some(tab) = alternate {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(tab) = state.terminal.tabs.get_mut(tab) else {
                return;
            };
            let app_cursor = {
                let term = tab.session.term.lock().unwrap_or_else(|e| e.into_inner());
                term.is_alternate().then_some(term.modes.app_cursor)
            };
            if let Some(app_cursor) = app_cursor {
                tab.session
                    .write(&crate::term::keys::alternate_scroll(lines, app_cursor));
                return;
            }
        }
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let history = state.terminal.active_tab().map_or(0, |tab| {
            tab.session
                .term
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .scrollback_len()
        });
        let back = state.terminal.back as isize;
        state.terminal.back = (back - lines).clamp(0, history as isize) as usize;
        drop(state);
        self.request_redraw();
        self.pump();
    }

    /// A tab's screen, if it is on screen, and its grid size.
    fn pane_grid(&self, tab: usize) -> Option<(Viewport, usize, usize)> {
        let rect = self.chrome().terminal?;
        let (_, screen) = crate::platform::terminal::split(rect);
        let state = self.state()?;
        let (_, screen) = state
            .terminal
            .screens(screen)
            .into_iter()
            .find(|(t, _)| *t == tab)?;
        let term = state.terminal.tabs.get(tab)?.session.term.lock();
        let term = term.unwrap_or_else(|e| e.into_inner());
        Some((screen, term.cols(), term.rows()))
    }

    /// Whether this window has the keyboard. A test instance counts as
    /// having it, since it often runs behind whatever is in front.
    pub(super) fn key_window(&self) -> bool {
        self.ivars().testing || self.window().is_some_and(|window| window.isKeyWindow())
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
        // The selected workspace's empty state takes the keys: none reach
        // another workspace's session behind it.
        if self
            .state()
            .is_some_and(|state| super::agents::active_hidden(&state))
        {
            return true;
        }
        let flags = event.modifierFlags();
        let code = event.keyCode();
        if flags.contains(NSEventModifierFlags::Command) {
            // The Mac line-editing shortcuts, as the shell's own keys.
            // The menu's shortcuts, for when the key never reached the menu.
            let shift = flags.contains(NSEventModifierFlags::Shift);
            let option = flags.contains(NSEventModifierFlags::Option);
            match code {
                key::K if !shift => {
                    self.clear_terminal();
                    return true;
                }
                key::F if !shift && option => {
                    self.open_terminal_search();
                    return true;
                }
                key::F if !shift => {
                    self.open_find();
                    return true;
                }
                key::UP | key::DOWN if shift && !option => {
                    self.terminal_jump_prompt(code == key::DOWN);
                    return true;
                }
                _ => {}
            }
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

/// What a Command-click in a terminal opens: a link, or a file reference
/// and the folder it is relative to.
enum Opened {
    Url(String),
    Path((String, Option<usize>, Option<usize>), std::path::PathBuf),
}

/// The find bar over the terminal, while it searches the terminal and the
/// panel shows a session.
/// Whether crc keeps a mouse event over a program that asked for the
/// mouse: Option or Shift held, or Command.
fn mouse_kept(flags: NSEventModifierFlags) -> bool {
    flags.intersects(
        NSEventModifierFlags::Option | NSEventModifierFlags::Shift | NSEventModifierFlags::Command,
    )
}

/// A mouse event at a cell, with the modifiers a report carries. Control
/// on a press of the right button is how a Mac makes it, so it is not
/// reported as held.
fn mouse_event(
    action: crate::term::mouse::Action,
    button: Option<crate::term::mouse::Button>,
    (row, col): (usize, usize),
    flags: NSEventModifierFlags,
) -> crate::term::mouse::Event {
    crate::term::mouse::Event {
        action,
        button,
        row,
        col,
        shift: flags.contains(NSEventModifierFlags::Shift),
        option: flags.contains(NSEventModifierFlags::Option),
        control: flags.contains(NSEventModifierFlags::Control)
            && button != Some(crate::term::mouse::Button::Right),
    }
}

/// Tells each program that asked for focus reports (mode 1004) when its
/// session gains or loses the keyboard, `CSI I` and `CSI O`, on a change
/// only: a click away from the terminal, another pane of a pair, another
/// tab, another window or app.
pub(super) fn report_focus(state: &mut State, key_window: bool) {
    let keys = key_window
        && state.terminal.has_keys()
        && state.terminal.showing()
        && !field_has_keys(state)
        && !super::agents::active_hidden(state);
    let active = state.terminal.active;
    for (index, tab) in state.terminal.tabs.iter_mut().enumerate() {
        let has = keys && index == active;
        if tab.told_focus == has {
            continue;
        }
        tab.told_focus = has;
        let asked = tab
            .session
            .term
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .modes
            .focus_events;
        if asked {
            tab.session.write(if has { b"\x1b[I" } else { b"\x1b[O" });
        }
    }
}

pub(super) fn find_rect(state: &State, chrome: &Chrome) -> Option<Viewport> {
    state.find.as_ref()?.terminal.as_ref()?;
    let rect = chrome.terminal?;
    (!state.terminal.tabs.is_empty())
        .then(|| crate::platform::terminal::find_bar_rect(&state.terminal, rect))
}

/// Filter mode's list over the terminal, while it shows.
pub(super) fn filter_rect(state: &State, chrome: &Chrome) -> Option<Viewport> {
    let find = state.find.as_ref()?.terminal.as_ref()?;
    find.filter?;
    let bar = find_rect(state, chrome)?;
    Some(crate::platform::terminal::filter_rect(
        &state.terminal,
        chrome.terminal?,
        bar,
        find.rows.len(),
        &state.renderer.atlas,
    ))
}

/// How many of filter mode's rows its list has room for, under the bar.
pub(super) fn filter_visible(state: &State) -> usize {
    let chrome = chrome_of(state);
    let (Some(bar), Some(rect)) = (find_rect(state, &chrome), chrome.terminal) else {
        return 0;
    };
    let list = crate::platform::terminal::filter_rect(
        &state.terminal,
        rect,
        bar,
        usize::MAX,
        &state.renderer.atlas,
    );
    crate::platform::terminal::filter_visible(list, &state.renderer.atlas)
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
