//! The Review page in the window: opening it, what its buttons do, and
//! keeping Home's counts current as agents write.

use super::*;
use crate::platform::review_page::Action;
use crate::project::review;

/// The session folders this window's terminals write to.
fn running(state: &State) -> Vec<std::path::PathBuf> {
    state
        .terminal
        .tabs
        .iter()
        .map(|t| t.review.clone())
        .collect()
}

/// After anything that changes what is under review: Home's list, each
/// terminal tab's count, and the page itself when it is open.
pub(super) fn refresh_review(state: &mut State) {
    state.review_page.running = running(state);
    state.review_page.summary = review::root().map_or_else(Vec::new, |r| review::summary(&r));
    for tab in &mut state.terminal.tabs {
        tab.review_files = state
            .review_page
            .summary
            .iter()
            .find(|(dir, _, _)| *dir == tab.review)
            .map_or(0, |(_, _, n)| *n);
    }
    if state.review_page.open {
        state.review_page.reload();
    }
}

/// A terminal counts as working on a change for this long after it last
/// printed: an agent prints while it edits.
const WORKING: std::time::Duration = std::time::Duration::from_secs(10);

/// Files the watcher saw change that no hook recorded: each one under a
/// terminal that was working is noted in that terminal's session, with
/// Git's staged copy to compare with. On a worker, since that asks Git.
/// What the person saves from another editor meanwhile is noted as well;
/// the page says such a file was changed without the hook.
pub(super) fn note_unhooked_writes(state: &mut State) {
    // One scan at a time; what changes meanwhile waits in the watcher.
    if state.review_scan.is_some() {
        return;
    }
    let Some(watcher) = &state.watch.watcher else {
        return;
    };
    let changed = watcher.take_files();
    if changed.is_empty() {
        return;
    }
    let live: Vec<std::path::PathBuf> = state
        .terminal
        .tabs
        .iter()
        .map(|t| t.review.clone())
        .collect();
    let working: Vec<_> = state
        .terminal
        .tabs
        .iter()
        .filter(|t| t.last_output.is_some_and(|at| at.elapsed() < WORKING))
        .map(|t| {
            (
                t.review.clone(),
                t.base_name(),
                crate::platform::canonical(&t.launch.cwd),
                t.last_output,
            )
        })
        .collect();
    if working.is_empty() {
        return;
    }
    let mut notes = Vec::new();
    for path in changed {
        if !path.is_file() || live.iter().any(|dir| review::has(dir, &path)) {
            continue;
        }
        // The most recently busy terminal whose folder holds the file.
        let Some((dir, title, cwd, _)) = working
            .iter()
            .filter(|(_, _, cwd, _)| path.starts_with(cwd))
            .max_by_key(|(_, _, _, at)| *at)
        else {
            continue;
        };
        notes.push((dir.clone(), title.clone(), cwd.clone(), path));
    }
    if notes.is_empty() {
        return;
    }
    let (tx, rx) = mpsc::channel();
    state.review_scan = Some(rx);
    std::thread::spawn(move || {
        for (dir, title, cwd, path) in notes {
            let staged = match crate::project::git::index_bytes(&path) {
                Ok(crate::project::git::Indexed::Ignored) => continue,
                Ok(crate::project::git::Indexed::Tracked(bytes)) => Some(bytes),
                Ok(crate::project::git::Indexed::Untracked) | Err(_) => None,
            };
            if review::note_unhooked(&dir, &path, staged.as_deref()).is_ok() {
                review::describe(&dir, &title, &cwd);
            }
        }
        let _ = tx.send(());
    });
}

impl EditorView {
    /// The Review page in the editor column, read fresh.
    pub(super) fn open_review(&self) {
        if let Some(mut state) = self.state_mut() {
            state.review_page.open = true;
            refresh_review(&mut state);
            state.review_page.diff_scroll = 0;
            if let Some(page) = &mut state.extensions {
                page.details = false;
            }
            state.mcp.details = false;
            state.settings_page.open = false;
            state.palette = None;
            state.completion = None;
        }
        self.request_redraw();
        self.pump();
    }

    /// What a click on the Review page does.
    pub(super) fn review_action(&self, action: Action) {
        let said = self.review_do(action);
        let reload = matches!(
            action,
            Action::UndoHunk(_) | Action::UndoFile | Action::UndoSession(_)
        );
        if let Some(mut state) = self.state_mut() {
            refresh_review(&mut state);
            match said {
                Ok(Some(text)) => state.say(layout::Feedback::Success, text),
                Ok(None) => {}
                Err(text) => state.say(layout::Feedback::Failure, text),
            }
        }
        // crc's own writes do not come back through the watcher: open
        // documents are checked now. A clean one reloads, and its old text
        // goes on its undo stack, so Cmd-Z there brings the agent's back.
        if reload {
            self.check_open_files();
        }
        self.request_redraw();
        self.pump();
    }

    fn review_do(&self, action: Action) -> Result<Option<String>, String> {
        let Some(mut state) = self.state_mut() else {
            return Ok(None);
        };
        // A destructive action asks for a second click; anything else
        // clicked in between cancels it.
        let destructive = matches!(action, Action::UndoFile | Action::UndoSession(_));
        if destructive && state.review_page.confirm != Some(action) {
            state.review_page.confirm = Some(action);
            return Ok(None);
        }
        state.review_page.confirm = None;
        let page = &state.review_page;
        let shown = page.selected;
        let entry = page
            .sessions
            .get(shown.0)
            .and_then(|s| s.files.get(shown.1).map(|e| (s.dir.clone(), e)));
        let name = |file: &review::File| {
            file.path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
        };
        let dirty = |path: &std::path::Path| {
            all_docs(&state)
                .flat_map(|d| d.iter())
                .any(|b| b.path.as_deref() == Some(path) && b.is_dirty())
        };
        let refuse = |file: &review::File| {
            format!(
                "{} has unsaved edits in crc: save or revert them first",
                name(file)
            )
        };
        match action {
            Action::Close => {
                state.review_page.open = false;
                Ok(None)
            }
            Action::Select(s, f) => {
                state.review_page.selected = (s, f);
                state.review_page.diff_scroll = 0;
                Ok(None)
            }
            Action::KeepHunk(n) => {
                let Some((dir, e)) = entry else {
                    return Ok(None);
                };
                review::keep(&dir, &e.file, n, e.seen)?;
                Ok(Some(format!("Kept a change in {}", name(&e.file))))
            }
            Action::UndoHunk(n) => {
                let Some((dir, e)) = entry else {
                    return Ok(None);
                };
                if dirty(&e.file.path) {
                    return Err(refuse(&e.file));
                }
                let text = review::undone(&e.file, n, e.seen)?;
                review::write_undone(&dir, &e.file, &text)?;
                Ok(Some(format!("Undid a change in {}", name(&e.file))))
            }
            Action::KeepFile => {
                let Some((dir, e)) = entry else {
                    return Ok(None);
                };
                review::keep_all(&dir, &e.file)?;
                Ok(Some(format!("Kept every change in {}", name(&e.file))))
            }
            Action::UndoFile => {
                let Some((dir, e)) = entry else {
                    return Ok(None);
                };
                if dirty(&e.file.path) {
                    return Err(refuse(&e.file));
                }
                review::undo_all(&dir, &e.file)?;
                Ok(Some(format!("Put {} back as it was", name(&e.file))))
            }
            Action::KeepSession(s) | Action::UndoSession(s) => {
                let Some(session) = page.sessions.get(s) else {
                    return Ok(None);
                };
                let undo = matches!(action, Action::UndoSession(_));
                // Undo all leaves what has no copy to go back to.
                let acted: Vec<_> = session
                    .files
                    .iter()
                    .filter(|e| !undo || e.file.checkpoint.undoable())
                    .collect();
                if undo && let Some(e) = acted.iter().find(|e| dirty(&e.file.path)) {
                    return Err(refuse(&e.file));
                }
                for e in &acted {
                    if undo {
                        review::undo_all(&session.dir, &e.file)?;
                    } else {
                        review::keep_all(&session.dir, &e.file)?;
                    }
                }
                let n = acted.len();
                let files = if n == 1 { "file" } else { "files" };
                let left = session.files.len() - n;
                Ok(Some(match (undo, left) {
                    (true, 0) => format!("Put {n} {files} back as they were"),
                    (true, _) => format!(
                        "Put {n} {files} back as they were; {left} changed by a command can only be kept"
                    ),
                    (false, _) => format!("Kept every change in {n} {files}"),
                }))
            }
        }
    }
}
