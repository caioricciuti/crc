//! Git from the menus and the status bar: the branch picker, switching
//! and creating branches, fetch, pull and push, and blame.

use super::*;

impl EditorView {
    /// Opens a file named in the terminal, at its line and column when it
    /// gave them. Relative paths are tried against the session's folder,
    /// then the project's.
    /// Git > Switch Branch: the palette lists the local branches, and a
    /// name that is not one offers to create it.
    pub(super) fn open_branch_picker(&self, intent: BranchIntent) {
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
            let read = || -> Result<BranchPick, String> {
                let branches = crate::project::git::branches(&root)?;
                // What a delete would lose, so the list can say so.
                let unmerged = if intent == BranchIntent::Delete {
                    crate::project::git::unmerged_branches(&root)?
                } else {
                    Vec::new()
                };
                Ok(BranchPick {
                    branches,
                    intent,
                    unmerged,
                })
            };
            let _ = tx.send(read());
        });
        if let Some(mut state) = self.state_mut() {
            state.branch_rx = Some(rx);
        }
        self.resume_display_link();
    }

    /// Git > Switch Repository: the palette lists the workspace's
    /// repositories; the chosen one fills Source Control.
    pub(super) fn open_repo_picker(&self) {
        let rows = {
            let Some(state) = self.state() else {
                return;
            };
            let ws = &state.workspace;
            if !ws.has_several() {
                return;
            }
            ws.repos()
                .iter()
                .map(|repo| RepoRow {
                    label: ws.label(repo),
                    path: repo.clone(),
                    current: repo == ws.git_dir(),
                })
                .collect::<Vec<_>>()
        };
        self.open_palette_with("");
        if let Some(mut state) = self.state_mut() {
            state.repo_list = Some(rows);
        }
        self.request_redraw();
        self.pump();
    }

    /// Shows `repo` in Source Control.
    pub(super) fn select_repo(&self, repo: &Path) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if state.workspace.git_dir() == repo || !state.workspace.select(repo) {
                return;
            }
            state.git = git_panel_for(&state.workspace);
        }
        self.resume_display_link();
        self.request_redraw();
        self.pump();
    }

    /// Takes Home's workspace sections when the worker has read them.
    pub(super) fn poll_home(&self) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(rx) = &state.home_rx else { return };
            match rx.try_recv() {
                Ok(summary) => {
                    state.home_rx = None;
                    if state.home.as_ref() == Some(&summary) {
                        return;
                    }
                    state.home = Some(summary);
                }
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    state.home_rx = None;
                    return;
                }
            }
        }
        self.request_redraw();
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
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
            Ok(pick) => {
                // A rename starts from the old name, all of it selected.
                let old = match &pick.intent {
                    BranchIntent::RenameTo(old) => old.clone(),
                    _ => String::new(),
                };
                self.open_palette_with(&old);
                if let Some(mut state) = self.state_mut() {
                    if let Some((query, _)) = state.palette.as_mut() {
                        query.select_all();
                    }
                    state.branch_list = Some(pick);
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

    /// Deletes `name`, asking first when it has commits HEAD does not.
    pub(super) fn delete_branch(&self, name: String, unmerged: bool) {
        if unmerged {
            let detail = "It has commits that are on no other branch here. Deleting it \
                          loses them, except from Git's reflog.";
            let delete = if self.ivars().testing {
                // A test instance cannot answer a modal: it says what it
                // would have asked, and takes the answer from the environment.
                eprintln!("crc: delete branch prompt: {name}: {detail}");
                std::env::var("CRC_DELETE_BRANCH_ANSWER").is_ok_and(|a| a == "delete")
            } else {
                ask(
                    MainThreadMarker::from(self),
                    &format!("Delete \u{201c}{name}\u{201d}?"),
                    detail,
                    &["Delete", "Cancel"],
                ) == 0
            };
            if !delete {
                return;
            }
        }
        if let Some(mut state) = self.state_mut() {
            state.git.delete_branch(name, unmerged);
        }
        self.resume_display_link();
        self.request_redraw();
    }

    pub(super) fn rename_branch(&self, old: String, new: String) {
        if let Some(mut state) = self.state_mut() {
            state.git.rename_branch(old, new);
        }
        self.resume_display_link();
        self.request_redraw();
    }

    /// Gives up the merge, rebase, cherry-pick or revert under way, once
    /// asked: it throws away whatever was resolved so far.
    pub(super) fn git_abort(&self) {
        let Some(what) = self.state().and_then(|state| state.git.in_progress()) else {
            return;
        };
        let command = what.command();
        let detail = "The branch and its files go back to how they were before it began. \
                      Conflicts resolved so far are lost.";
        let abort = if self.ivars().testing {
            eprintln!("crc: abort prompt: {command}: {detail}");
            std::env::var("CRC_ABORT_ANSWER").is_ok_and(|a| a == "abort")
        } else {
            ask(
                MainThreadMarker::from(self),
                &format!("Abort the {command}?"),
                detail,
                &["Abort", "Cancel"],
            ) == 0
        };
        if !abort {
            return;
        }
        if let Some(mut state) = self.state_mut() {
            state.git.abort(what);
        }
        self.resume_display_link();
        self.request_redraw();
    }

    /// A right-click in Source Control: the row's menu, or the Git menu.
    pub(super) fn git_context_menu(&self, column: Viewport, x: f32, y: f32) -> Retained<NSMenu> {
        use crate::platform::git_panel::{Entry, Group};
        let mtm = MainThreadMarker::from(self);
        let entry = self.state().and_then(|state| {
            let g = state.git.sidebar(column);
            state.git.entry_at(g, x, y).map(|(e, _)| e)
        });
        match entry {
            Some(Entry::File { change, group }) => {
                if let Some(mut state) = self.state_mut() {
                    state.context_change = Some((change, group));
                }
                git_change_menu(mtm, group == Group::Staged, group == Group::Conflicts)
            }
            Some(Entry::Section { group, .. }) if group != Group::Conflicts => {
                git_section_menu(mtm, group == Group::Staged)
            }
            _ => git_actions_menu(mtm),
        }
    }

    /// Puts `text` on the clipboard and says so.
    pub(super) fn copy_and_say(&self, text: &str) {
        clipboard::write_text(text);
        if let Some(mut state) = self.state_mut() {
            state.message = Some((format!("copied {text}"), Instant::now()));
        }
        self.request_redraw();
        self.pump();
    }

    /// Source Control's "more" button: the Git commands as a menu under it.
    pub(super) fn pop_up_git_menu(&self, below: Viewport) {
        let mtm = MainThreadMarker::from(self);
        let menu = git_actions_menu(mtm);
        let at = NSPoint::new(below.x as f64, (below.y + below.height + 4.0) as f64);
        layout::set_pressed(None);
        menu.popUpMenuPositioningItem_atLocation_inView(None, at, Some(self));
        self.request_redraw();
    }

    /// The change a Source Control context menu was opened on: its path in
    /// the repository and on disk.
    pub(super) fn context_change_paths(&self) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
        let state = self.state()?;
        let (index, _) = state.context_change?;
        let snapshot = state.git.snapshot.as_ref()?;
        let change = snapshot.changes.get(index)?;
        Some((change.path.clone(), snapshot.root.join(&change.path)))
    }

    pub(super) fn git_remote(&self, what: crate::project::git::Remote) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if state.git.root().is_none() {
            state.message = Some(("not a Git repository".into(), Instant::now()));
        } else {
            let sock = state.ssh_auth_sock.clone();
            // The status line shows a spinner and the verb for as long as
            // the command runs, and the outcome once it is done.
            state.git.remote(what, sock);
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
        if let Some(rx) = &state.blame.rx {
            match rx.try_recv() {
                Ok((id, line, text)) => {
                    state.blame.rx = None;
                    state.blame.shown = Some((id, line, text));
                    self.ivars().needs_redraw.set(true);
                }
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => state.blame.rx = None,
            }
        }
        let buffer = state.docs.active();
        let (id, line) = (buffer.id(), buffer.cursor_position().0);
        if state
            .blame
            .shown
            .as_ref()
            .is_some_and(|(i, l, _)| (*i, *l) == (id, line))
        {
            return;
        }
        match state.blame.want {
            Some((i, l, at)) if (i, l) == (id, line) => {
                if at.elapsed() < REST {
                    return;
                }
            }
            _ => {
                state.blame.want = Some((id, line, Instant::now()));
                if state.blame.shown.take().is_some() {
                    self.ivars().needs_redraw.set(true);
                }
                return;
            }
        }
        state.blame.want = None;
        // Only files Git knows: their HEAD text was found for the gutter.
        let tracked = state
            .gutter
            .docs
            .get(&id)
            .is_some_and(|g| g.head.as_ref().is_some_and(|h| !h.is_empty()));
        let buffer = state.docs.active();
        let Some(path) = buffer.path.clone() else {
            return;
        };
        if !tracked || buffer.rope.len_bytes() > GUTTER_MAX_BYTES {
            return;
        }
        let text = buffer.rope.to_string();
        let (tx, rx) = mpsc::channel();
        state.blame.rx = Some(rx);
        std::thread::spawn(move || {
            // The repository the file is in, which in a workspace of
            // several is not always the one Source Control shows.
            let Some(root) = path
                .parent()
                .and_then(|dir| crate::project::git::toplevel(dir).ok())
            else {
                let _ = tx.send((id, line, String::new()));
                return;
            };
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
                // Said where the blame would be, so a Git that cannot
                // answer is not mistaken for a line nobody touched.
                Err(e) => format!(
                    "blame unavailable: {}",
                    crate::platform::git_panel::headline(&e)
                ),
            };
            let _ = tx.send((id, line, note));
        });
    }

    /// Git > Blame Heat: the strip on or off until crc quits. Off drops
    /// every blame, and any still running is dropped when it lands.
    pub(super) fn toggle_blame_heat(&self) {
        if let Some(mut state) = self.state_mut() {
            if state.heat.on {
                state.heat.turn_off();
            } else {
                state.heat.on = true;
            }
        }
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
        self.resume_display_link();
        self.request_redraw();
        self.pump();
    }

    /// Keeps the blame heat strip of the documents on screen current:
    /// takes blames that landed, and asks for one when a document has
    /// none, HEAD may have moved, or typing stopped a second ago. Runs from
    /// the display link.
    pub(super) fn heat_refresh(&self) {
        const PAUSE: Duration = Duration::from_secs(1);
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let state = &mut *state;
        let now = crate::platform::unix_seconds() as i64;
        while let Ok((id, generation, epoch, text, blamed)) = state.heat.channel.1.try_recv() {
            let rope = buffer_by_id(&state.docs, &state.panes, id).map(|b| &b.rope);
            // An answer Git could not give leaves the document without a
            // strip, rather than asking again every frame.
            let blamed = (id, generation, epoch, text, blamed.unwrap_or_default());
            if state.heat.land(blamed, state.gutter.epoch, rope, now) {
                self.ivars().needs_redraw.set(true);
                if let Some(window) = self.window() {
                    window.invalidateCursorRectsForView(self);
                }
            }
        }
        // Signatures checked: the tooltips say what was found.
        while let Ok((sha, check)) = state.heat.checked.1.try_recv() {
            state.heat.land_check(sha, check, now);
            self.ivars().needs_redraw.set(true);
        }
        if !state.heat.on {
            return;
        }
        // The documents on screen. Others are looked at when next shown.
        let shown: Vec<u64> = std::iter::once(state.docs.active())
            .chain(state.panes.iter().map(|p| p.docs.active()))
            .map(Buffer::id)
            .collect();
        for id in shown {
            let Some(buffer) = buffer_by_id(&state.docs, &state.panes, id) else {
                continue;
            };
            match heat_wanted(&state.gutter, buffer) {
                // Not known yet: the gutter has not read its HEAD text.
                None => continue,
                Some(false) => {
                    state.heat.docs.remove(&id);
                    state.heat.dirty.remove(&id);
                    continue;
                }
                Some(true) => {}
            }
            let epoch = state.gutter.epoch;
            let heat = &mut state.heat;
            let ask = match heat.follow(id, &buffer.rope) {
                Some(doc) => doc.epoch != epoch,
                None => true,
            };
            if ask && !heat.pending.contains(&id) && !heat.dirty.contains_key(&id) {
                start_heat(heat, epoch, id, buffer);
            }
        }
        // Typing stopped.
        for id in due(&state.heat.dirty, PAUSE) {
            if state.heat.pending.contains(&id) {
                continue;
            }
            state.heat.dirty.remove(&id);
            let Some(buffer) = buffer_by_id(&state.docs, &state.panes, id) else {
                continue;
            };
            if heat_wanted(&state.gutter, buffer) == Some(true) {
                start_heat(&mut state.heat, state.gutter.epoch, id, buffer);
            }
        }
    }
}

impl EditorView {
    /// The tooltip `tip` is about to show: when it is the heat strip's for
    /// a signed commit not checked yet, the signature is checked on a
    /// worker, and the tooltip says what was found the next time it shows.
    pub(super) fn check_heat_tip(&self, tip: &str) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let state = &mut *state;
            let Some((id, sha)) = state.heat.check_for_tip(tip) else {
                return;
            };
            let path = buffer_by_id(&state.docs, &state.panes, id).and_then(|b| b.path.clone());
            let tx = state.heat.checked.0.clone();
            std::thread::spawn(move || {
                let check = path
                    .as_deref()
                    .and_then(Path::parent)
                    .ok_or_else(|| "no parent directory".to_string())
                    .and_then(crate::project::git::toplevel)
                    .and_then(|root| crate::project::git::signature_check(&root, &sha))
                    .ok();
                let _ = tx.send((sha, check));
            });
        }
        self.resume_display_link();
    }

    /// A click on the blame heat strip beside `line` of the active
    /// document: File History at the commit that last changed the line,
    /// the caret where the line was then.
    pub(super) fn click_heat(&self, line: usize) {
        let goto = {
            let Some(state) = self.state() else {
                return;
            };
            let buffer = state.docs.active();
            let Some(doc) = state.heat.docs.get(&buffer.id()) else {
                return;
            };
            match doc
                .map
                .lines
                .get(line)
                .map(|&c| doc.map.commits.get(c as usize))
            {
                Some(Some(commit)) => {
                    let origin = doc.map.origins.get(line).map_or(line, |&o| o as usize);
                    // The line stays where it is on screen.
                    let offset = line.saturating_sub(buffer.scroll_line);
                    Ok(super::scrub::Goto {
                        commit: commit.sha.clone(),
                        line: origin,
                        top: origin.saturating_sub(offset),
                    })
                }
                _ => Err(format!("line {} is not committed yet", line + 1)),
            }
        };
        match goto {
            Ok(goto) => self.open_history_at(Some(goto)),
            Err(said) => {
                if let Some(mut state) = self.state_mut() {
                    state.message = Some((said, Instant::now()));
                }
                self.resume_display_link();
                self.request_redraw();
            }
        }
    }
}

/// The line of the active document whose blame heat a click at `(x, y)`
/// in the view is on, if it opens File History: a Command click anywhere
/// in the strip's cell beside a line the blame holds, a plain one only
/// where the strip is drawn (a change bar there was offered the click
/// first). Shift and Option clicks select from there, as elsewhere.
pub(super) fn heat_under(state: &State, event: &NSEvent, x: f32, y: f32) -> Option<usize> {
    let flags = event.modifierFlags();
    if !state.heat.on
        || flags.intersects(NSEventModifierFlags::Shift | NSEventModifierFlags::Option)
    {
        return None;
    }
    let chrome = chrome_of(state);
    if !chrome.text.contains(x, y) {
        return None;
    }
    let (tx, ty) = chrome.to_text(x, y);
    let buffer = state.docs.active();
    let doc = state.heat.docs.get(&buffer.id())?;
    let strip = doc.strip();
    let line = layout::heat_strip_at(buffer, &state.renderer.atlas, &strip, tx, ty)?;
    let drawn = strip
        .lines
        .get(line)
        .and_then(|&c| strip.alpha.get(c as usize))
        .is_some_and(|&a| a > 0.0);
    (drawn || flags.contains(NSEventModifierFlags::Command)).then_some(line)
}

/// Whether `buffer` gets a blame heat strip: a saved file Git tracks, of
/// at most 2 MB, not a preview or a version from File History. `None`
/// until the gutter has read its HEAD text, which says whether Git has it.
fn heat_wanted(gutter: &Gutter, buffer: &Buffer) -> Option<bool> {
    if buffer.path.is_none()
        || buffer.is_preview_file()
        || buffer.is_frozen()
        || buffer.rope.len_bytes() > GUTTER_MAX_BYTES
    {
        return Some(false);
    }
    let entry = gutter.docs.get(&buffer.id())?;
    Some(entry.head.as_ref().is_some_and(|h| !h.is_empty()))
}

/// Blames `buffer`'s text on a worker; the answer lands in `heat`'s
/// channel.
fn start_heat(heat: &mut Heat, epoch: u64, id: u64, buffer: &Buffer) {
    let Some(path) = buffer.path.clone() else {
        return;
    };
    heat.pending.insert(id);
    let snapshot = buffer.rope.clone();
    let tx = heat.channel.0.clone();
    let generation = heat.generation;
    std::thread::spawn(move || {
        // The repository the file is in, which in a workspace of several
        // is not always the one Source Control shows.
        let blamed = path
            .parent()
            .ok_or_else(|| "no parent directory".to_string())
            .and_then(crate::project::git::toplevel)
            .and_then(|root| {
                let relative = path.strip_prefix(&root).unwrap_or(&path);
                crate::project::git::blame_file(&root, relative, &snapshot.to_string())
            });
        let _ = tx.send((id, generation, epoch, snapshot, blamed));
    });
}
