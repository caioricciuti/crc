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
                let notices = tab
                    .session
                    .term
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take_notices();
                noticed.push((index, notices.into_iter().last()));
            }
        }
        drop(state);
        if !noticed.is_empty() {
            self.take_notices(noticed);
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
    /// this window's IDE port, so it connects without `/ide`.
    pub(super) fn terminal_launch(
        &self,
        claude: bool,
    ) -> Option<crate::platform::terminal::Launch> {
        let state = self.state()?;
        let shell = std::env::var("CRC_TERMINAL_SHELL")
            .or_else(|_| std::env::var("SHELL"))
            .unwrap_or_else(|_| "/bin/zsh".into());
        let home = std::env::var_os("HOME").map_or_else(|| "/".into(), std::path::PathBuf::from);
        let cwd = state.tree.root().map_or(home, Path::to_path_buf);
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
        // write into; created by the first hook, not here.
        if let Some(dir) = crate::project::review::root() {
            env.push((
                crate::project::review::SESSION_VAR.to_owned(),
                dir.join(crate::project::review::new_id())
                    .to_string_lossy()
                    .into_owned(),
            ));
        }
        // Every session knows this window's IDE port, so `claude` typed in
        // any of them connects without `/ide`.
        if let Some(bridge) = &state.claude {
            env.push(("CLAUDE_CODE_SSE_PORT".to_owned(), bridge.port().to_string()));
            env.push(("ENABLE_IDE_INTEGRATION".to_owned(), "true".to_owned()));
        }
        let args = if claude {
            // A command given in CRC_CLAUDE_COMMAND runs exactly as given.
            // Plain `claude` gets the review hooks as extra settings: Claude
            // merges them with the person's own, and nothing in ~/.claude
            // is touched.
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
            vec!["-l".to_owned(), "-i".to_owned(), "-c".to_owned(), command]
        } else {
            vec!["-l".to_owned()]
        };
        // A Claude Code session that started this editor (from its own
        // terminal, say) left its markers in our environment. Passed on,
        // they make the `claude` here think it is that session's child.
        let unset = std::env::vars_os()
            .filter_map(|(name, _)| name.into_string().ok())
            .filter(|name| name == "CLAUDECODE" || name.starts_with("CLAUDE_CODE_"))
            .collect();
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

    pub(super) fn spawn_terminal(&self, claude: bool) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.terminal.open = true;
            state.terminal.focus = true;
        }
        let Some(launch) = self.terminal_launch(claude) else {
            return;
        };
        let (cols, rows) = self.terminal_grid();
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
        let Some(mut state) = self.state_mut() else {
            return;
        };
        match spawned {
            Ok(session) => {
                let title = if claude {
                    "✻ Claude".to_owned()
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
                state.terminal.tabs.push(crate::platform::terminal::Tab {
                    session,
                    title,
                    claude,
                    launch,
                    attention: None,
                    last_output: None,
                    review,
                    events_read: 0,
                    review_files: 0,
                });
                state.terminal.active = state.terminal.tabs.len() - 1;
                state.terminal.back = 0;
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

    /// The panel appeared, went, or changed sessions: the editor column
    /// changed size, and so did the cursor rectangles.
    pub(super) fn after_terminal_layout(&self) {
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
        let (cols, rows) = crate::platform::terminal::grid_size(&state.renderer.atlas, screen);
        for tab in &state.terminal.tabs {
            tab.session.resize(cols, rows);
        }
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
        if let Some(tab) = state.terminal.active_tab_mut() {
            // Typing into it is the answer to what it asked.
            tab.attention = None;
            tab.session.write(bytes);
        }
    }

    /// Output arrived in these tabs, with the newest notification each
    /// carried. A notification marks its tab; the status line says it
    /// when the tab is not the one being typed into.
    fn take_notices(&self, noticed: Vec<(usize, Option<String>)>) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let now = Instant::now();
        let mut said = None;
        let watching = state.terminal.has_keys();
        let active = state.terminal.active;
        let mut wrote = false;
        for (index, mut notice) in noticed {
            let Some(tab) = state.terminal.tabs.get_mut(index) else {
                continue;
            };
            tab.last_output = Some(now);
            // What its agent's hooks wrote since the last look: an agent
            // writes files and asks questions while it prints.
            let (events, read) = crate::project::review::read_events(&tab.review, tab.events_read);
            tab.events_read = read;
            for event in events {
                match event {
                    crate::project::review::Event::Wrote { .. } => {
                        crate::project::review::describe(
                            &tab.review,
                            &tab.base_name(),
                            &tab.launch.cwd,
                        );
                        wrote = true;
                    }
                    crate::project::review::Event::Asked(text) => notice = Some(text),
                    crate::project::review::Event::Failed(text) => {
                        said = Some(format!("{}: review hook: {text}", tab.name()));
                    }
                }
            }
            if let Some(text) = notice {
                tab.attention = Some(text);
                if !(watching && index == active) {
                    said = Some(format!("{}: {}", tab.name(), tab.state()));
                }
            }
        }
        if wrote {
            super::review::refresh_review(&mut state);
        }
        if let Some(said) = said {
            state.message = Some((said, now));
        }
    }

    /// The active session's screen rect and grid size, for mapping points.
    pub(super) fn terminal_screen(&self) -> Option<(Viewport, usize, usize)> {
        let rect = self.chrome().terminal?;
        let (_, screen) = crate::platform::terminal::split(rect);
        let state = self.state()?;
        let tab = state.terminal.active_tab()?;
        let term = tab.session.term.lock().unwrap_or_else(|e| e.into_inner());
        Some((screen, term.cols(), term.rows()))
    }

    /// A press in the terminal's screen: starts a selection, selects a word
    /// or path on a double click and the line on a triple, and with Command
    /// opens the file reference under the pointer.
    pub(super) fn terminal_press(&self, event: &NSEvent, x: f32, y: f32) {
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

    pub(super) fn terminal_selected_text(&self) -> Option<String> {
        let state = self.state()?;
        let (a, b) = state.terminal.selection?;
        let tab = state.terminal.active_tab()?;
        let text = tab
            .session
            .term
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .text_between(a, b);
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
