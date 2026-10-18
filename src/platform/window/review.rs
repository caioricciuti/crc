//! Agents' sessions in the window: the terminal tabs' review counts, kept
//! current as agents write, and Keep and Undo from the Git panel's
//! "Changed by agents" rows.

use super::*;
use crate::project::review;

/// The agents' sessions being counted on a worker, for the terminal tabs.
#[derive(Default)]
pub(super) struct Review {
    /// The summary being read on a worker, and whether more changed since
    /// that read started, so one more follows it.
    pub counting: Option<mpsc::Receiver<Vec<(std::path::PathBuf, String, usize)>>>,
    pub recount: bool,
}

/// Which tab each live session belongs to, for the Git panel's "Changed
/// by agents" rows: the title without the review count, as the session's
/// own record has it.
pub(super) fn agent_labels(state: &State) -> Vec<(std::path::PathBuf, String)> {
    state
        .terminal
        .tabs
        .iter()
        .filter(|t| t.tracks_review())
        .map(|t| (t.review.clone(), t.base_name()))
        .collect()
}

/// Whether `path` is open in crc with unsaved edits: Undo leaves such a
/// file alone, since choosing between two versions is the person's call.
fn dirty_in_crc(state: &State, path: &std::path::Path) -> bool {
    all_docs(state)
        .flat_map(|d| d.iter())
        .any(|b| b.path.as_deref() == Some(path) && b.is_dirty())
}

/// How Undo refuses a file that is [`dirty_in_crc`].
fn unsaved_refusal(file: &review::File) -> String {
    format!(
        "{} has unsaved edits in crc: save or revert them first",
        file.path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    )
}

/// Every session with something to review, its title and file count.
fn read_summary() -> Vec<(std::path::PathBuf, String, usize)> {
    review::root().map_or_else(Vec::new, |r| review::summary(&r))
}

/// After anything that changes what is under review: the Git panel's
/// group and each terminal tab's count. The list is read on a worker,
/// since agents write often and the store can hold thousands of files; a
/// write while one is read asks for one more read after it.
/// [`poll_counts`] applies what it finds.
pub(super) fn refresh_review(state: &mut State) {
    // The Git panel lists the agents' files with the repository's changes.
    state.git.set_agent_labels(agent_labels(state));
    state.git.refresh();
    if state.review.counting.is_some() {
        state.review.recount = true;
        return;
    }
    let (tx, rx) = mpsc::channel();
    state.review.counting = Some(rx);
    std::thread::spawn(move || {
        let _ = tx.send(read_summary());
    });
}

/// The counts a worker read, once they are in. Says whether anything
/// changed on screen.
fn poll_counts(state: &mut State) -> bool {
    let Some(rx) = &state.review.counting else {
        return false;
    };
    let summary = match rx.try_recv() {
        Ok(summary) => Some(summary),
        Err(mpsc::TryRecvError::Empty) => return false,
        Err(mpsc::TryRecvError::Disconnected) => None,
    };
    state.review.counting = None;
    let applied = summary.is_some();
    if let Some(summary) = summary {
        apply_summary(state, &summary);
    }
    if std::mem::take(&mut state.review.recount) {
        refresh_review(state);
    }
    applied
}

/// The tabs' counts take the summary.
fn apply_summary(state: &mut State, summary: &[(std::path::PathBuf, String, usize)]) {
    for tab in &mut state.terminal.tabs {
        tab.review_files = summary
            .iter()
            .filter(|_| tab.tracks_review())
            .find(|(dir, _, _)| *dir == tab.review)
            .map_or(0, |(_, _, n)| *n);
    }
}

/// A terminal counts as working on a change for this long after it last
/// printed: an agent prints while it edits.
const WORKING: std::time::Duration = std::time::Duration::from_secs(10);

/// Files the watcher saw change that no hook recorded: each one under a
/// terminal that was working is noted in that terminal's session, with
/// Git's staged copy to compare with. On a worker, since that asks Git.
/// What the person saves from another editor meanwhile is noted as well;
/// the Git panel says such a file was changed without the hook.
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
    // A terminal in ssh writes on its host: nothing here is its.
    let working: Vec<_> = state
        .terminal
        .tabs
        .iter()
        .filter(|t| t.tracks_review())
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
    // Which terminal each file is under is known here; whether it is a
    // file at all and whether a session has it already ask the disk, so
    // the worker finds out.
    let notes: Vec<_> = changed
        .into_iter()
        .filter_map(|path| {
            // The most recently busy terminal whose folder holds the file.
            let (dir, title, cwd, _) = working
                .iter()
                .filter(|(_, _, cwd, _)| path.starts_with(cwd))
                .max_by_key(|(_, _, _, at)| *at)?;
            Some((dir.clone(), title.clone(), cwd.clone(), path))
        })
        .collect();
    if notes.is_empty() {
        return;
    }
    let (tx, rx) = mpsc::channel();
    state.review_scan = Some(rx);
    std::thread::spawn(move || {
        for (dir, title, cwd, path) in notes {
            if !path.is_file() || live.iter().any(|dir| review::has(dir, &path)) {
                continue;
            }
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
    /// The tabs' counts, once a worker has read them.
    pub(super) fn poll_review(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if poll_counts(&mut state) {
            self.ivars().needs_redraw.set(true);
        }
    }

    /// Keep or Undo from an agent's row in Source Control: the whole file.
    /// Undo is refused for a file with unsaved edits in crc, and by the
    /// store for one not as the agent left it; the status line says which.
    pub(super) fn agent_row_action(&self, undo: bool) {
        let said = (|| -> Result<Option<String>, String> {
            let Some(state) = self.state() else {
                return Ok(None);
            };
            let index = state.context_agent;
            let Some(agent) = index.and_then(|i| state.git.agents.get(i).cloned()) else {
                return Ok(None);
            };
            let name = agent.name();
            if undo {
                if dirty_in_crc(&state, &agent.file.path) {
                    return Err(unsaved_refusal(&agent.file));
                }
                review::undo_all(&agent.session, &agent.file)?;
                Ok(Some(format!("Put {name} back as it was")))
            } else {
                review::keep_all(&agent.session, &agent.file)?;
                Ok(Some(format!("Kept every change in {name}")))
            }
        })();
        if let Some(mut state) = self.state_mut() {
            refresh_review(&mut state);
            match said {
                Ok(Some(text)) => state.say(layout::Feedback::Success, text),
                Ok(None) => {}
                Err(text) => state.say(layout::Feedback::Failure, text),
            }
        }
        self.resume_display_link();
        // crc's own writes do not come back through the watcher: open
        // documents are checked now. A clean one reloads, and its old text
        // goes on its undo stack, so Cmd-Z there brings the agent's back.
        if undo {
            self.check_open_files();
        }
        self.request_redraw();
        self.pump();
    }
}
