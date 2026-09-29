//! Git from the menus and the status bar: the branch picker, switching
//! and creating branches, fetch, pull and push, and blame.

use super::*;

impl EditorView {
    /// Opens a file named in the terminal, at its line and column when it
    /// gave them. Relative paths are tried against the session's folder,
    /// then the project's.
    /// Git > Switch Branch: the palette lists the local branches, and a
    /// name that is not one offers to create it.
    pub(super) fn open_branch_picker(&self) {
        let root = self
            .ivars()
            .state
            .borrow()
            .git
            .root()
            .map(Path::to_path_buf);
        let Some(root) = root else {
            if let Some(mut state) = self.state_mut() {
                state.message = Some(("not a Git repository".into(), Instant::now()));
            }
            self.request_redraw();
            return;
        };
        // Read on a worker, like every other Git command: the palette
        // opens when the list arrives.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::project::git::branches(&root));
        });
        if let Some(mut state) = self.state_mut() {
            state.branch_rx = Some(rx);
        }
        self.resume_display_link();
    }

    pub(super) fn poll_branches(&self) {
        let reply = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(rx) = &state.branch_rx else { return };
            let reply = match rx.try_recv() {
                Ok(reply) => reply,
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => Err("could not list branches".into()),
            };
            state.branch_rx = None;
            reply
        };
        match reply {
            Ok(branches) => {
                self.open_palette_with("");
                if let Some(mut state) = self.state_mut() {
                    state.branch_list = Some(branches);
                }
            }
            Err(error) => {
                if let Some(mut state) = self.state_mut() {
                    state.message = Some((error, Instant::now()));
                }
            }
        }
        self.request_redraw();
        self.pump();
    }

    pub(super) fn switch_branch(&self, name: String, create: bool) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let current = state.git.branch();
        if !create && name == current {
            state.message = Some((format!("already on {name}"), Instant::now()));
        } else if create {
            state.git.create_branch(name);
        } else {
            state.git.switch_branch(name);
        }
        drop(state);
        self.resume_display_link();
        self.request_redraw();
    }

    pub(super) fn git_remote(&self, what: crate::project::git::Remote) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if state.git.root().is_none() {
            state.message = Some(("not a Git repository".into(), Instant::now()));
        } else {
            let sock = state.ssh_auth_sock.clone();
            state.git.remote(what, sock);
            state.message = Some((format!("{}…", what.verb()), Instant::now()));
        }
        drop(state);
        self.resume_display_link();
        self.request_redraw();
    }

    /// Blame for the caret's line, once the caret has rested on it. Runs
    /// from the display link; the answer lands in `blame`.
    pub(super) fn blame_refresh(&self) {
        const REST: Duration = Duration::from_millis(400);
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if let Some(rx) = &state.blame_rx {
            match rx.try_recv() {
                Ok((id, line, text)) => {
                    state.blame_rx = None;
                    state.blame = Some((id, line, text));
                    self.ivars().needs_redraw.set(true);
                }
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => state.blame_rx = None,
            }
        }
        let buffer = state.docs.active();
        let (id, line) = (buffer.id(), buffer.cursor_position().0);
        if state
            .blame
            .as_ref()
            .is_some_and(|(i, l, _)| (*i, *l) == (id, line))
        {
            return;
        }
        match state.blame_want {
            Some((i, l, at)) if (i, l) == (id, line) => {
                if at.elapsed() < REST {
                    return;
                }
            }
            _ => {
                state.blame_want = Some((id, line, Instant::now()));
                if state.blame.take().is_some() {
                    self.ivars().needs_redraw.set(true);
                }
                return;
            }
        }
        state.blame_want = None;
        // Only files Git knows: their HEAD text was found for the gutter.
        let tracked = state
            .gutter
            .get(&id)
            .is_some_and(|g| g.head.as_ref().is_some_and(|h| !h.is_empty()));
        let buffer = state.docs.active();
        let (Some(root), Some(path)) =
            (state.git.root().map(Path::to_path_buf), buffer.path.clone())
        else {
            return;
        };
        if !tracked || buffer.rope.len_bytes() > GUTTER_MAX_BYTES {
            return;
        }
        let text = buffer.rope.to_string();
        let (tx, rx) = mpsc::channel();
        state.blame_rx = Some(rx);
        std::thread::spawn(move || {
            let relative = path.strip_prefix(&root).unwrap_or(&path).to_path_buf();
            let note = match crate::project::git::blame_line(&root, &relative, line, &text) {
                Ok(blame) => match blame.author {
                    None => "not committed yet".to_string(),
                    Some(author) => {
                        let now = crate::platform::unix_seconds() as i64;
                        format!(
                            "{author}, {}: {}",
                            crate::project::git::ago(blame.time, now),
                            blame.summary
                        )
                    }
                },
                Err(_) => String::new(),
            };
            let _ = tx.send((id, line, note));
        });
    }
}
