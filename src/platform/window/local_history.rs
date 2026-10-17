//! Local history in the window: the timer for an idle snapshot that falls
//! due with no frame to notice it, what the history thread says, and the
//! versions only the window sees go: a tab closed unsaved, quitting. And
//! its commands: Recover Deleted File, and clearing it for one file or all.

use super::*;

/// Recover Deleted File lists this many files at most, the most recently
/// kept first.
const DELETED_LISTED: usize = 200;

/// A file that is gone from disk with versions in local history, as the
/// palette lists it.
pub(super) struct DeletedRow {
    path: std::path::PathBuf,
    name: String,
    folder: String,
}

/// What a worker read of local history for its commands.
pub(super) enum Reply {
    /// The files Recover Deleted File offers, or why there are none.
    Deleted(Result<Vec<DeletedRow>, String>),
    /// A deleted file's latest version that reads back, as a document, or
    /// why it could not be had.
    Recovered(std::path::PathBuf, Result<Box<Buffer>, String>),
}

/// Recover Deleted File's rows: the files whose names match.
pub(super) fn deleted_rows(rows: &[DeletedRow], query: &str) -> Vec<(layout::PaletteRow, Pick)> {
    crate::project::finder::ranked(rows, query, |r| &r.name)
        .into_iter()
        .map(|i| {
            let row = &rows[i];
            (
                layout::PaletteRow {
                    icon: Some(crate::project::icons::HISTORY),
                    title: row.name.clone(),
                    detail: row.folder.clone(),
                    shortcut: String::new(),
                },
                Pick::Recover(row.path.clone()),
            )
        })
        .collect()
}

/// The files in the history at `db` that are gone from disk. On a worker:
/// it stats every file the history has.
fn read_deleted(db: Option<&Path>) -> Result<Vec<DeletedRow>, String> {
    let Some(store) = db
        .map(crate::history::store::Store::open_to_read)
        .transpose()
        .map_err(|e| e.to_string())?
        .flatten()
    else {
        return Ok(Vec::new());
    };
    let paths = store
        .deleted_paths(|path| path.exists())
        .map_err(|e| e.to_string())?;
    Ok(paths
        .into_iter()
        .take(DELETED_LISTED)
        .map(|path| DeletedRow {
            name: path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            ),
            folder: path.parent().map(shown_path).unwrap_or_default(),
            path,
        })
        .collect())
}

/// `path`'s latest version that reads back, opened as unsaved changes to
/// it. On a worker.
fn read_latest(db: Option<&Path>, path: &Path) -> Result<Box<Buffer>, String> {
    let gone = || "it is no longer in local history".to_string();
    let store = db
        .map(crate::history::store::Store::open_to_read)
        .transpose()
        .map_err(|e| e.to_string())?
        .flatten()
        .ok_or_else(gone)?;
    for entry in store.list(path).map_err(|e| e.to_string())? {
        if let Some(text) = store.read(entry.id).map_err(|e| e.to_string())? {
            return Ok(Box::new(Buffer::recovered(
                Some(path.to_path_buf()),
                &text,
                None,
            )));
        }
    }
    Err(gone())
}

/// The name a status line or prompt gives `path`.
fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// The recorder's timing. A self-test sets `CRC_HISTORY_IDLE_MS` rather
/// than wait 30 seconds for an idle snapshot.
pub(super) fn timing() -> crate::history::recorder::Timing {
    let mut timing = crate::history::recorder::Timing::default();
    if std::env::var_os("CRC_SELFTEST").is_some()
        && let Some(ms) = std::env::var("CRC_HISTORY_IDLE_MS")
            .ok()
            .and_then(|ms| ms.parse().ok())
    {
        timing.idle = Duration::from_millis(ms);
    }
    timing
}

impl EditorView {
    /// The history thread's wake-up: a main-thread poll.
    pub(super) fn history_wake(&self) -> crate::platform::dispatch::Wake {
        self.wake(EditorView::poll_history)
    }

    /// After every frame: a timer for the next idle snapshot, when it falls
    /// due before the one already set. A comparison when nothing is due.
    pub(super) fn history_after_frame(&self) {
        let due = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(due) = state.history.due() else {
                return;
            };
            if state.history.armed.is_some_and(|armed| armed <= due) {
                return;
            }
            state.history.armed = Some(due);
            due
        };
        unsafe {
            let _: () = msg_send![
                objc2::class!(NSObject),
                cancelPreviousPerformRequestsWithTarget: self,
                selector: sel!(historyTick:),
                object: None::<&AnyObject>
            ];
        }
        self.arm_history_tick(due.saturating_duration_since(Instant::now()));
    }

    fn arm_history_tick(&self, wait: Duration) {
        let _: () = unsafe {
            msg_send![self, performSelector: sel!(historyTick:),
                withObject: None::<&AnyObject>, afterDelay: wait.as_secs_f64()]
        };
    }

    /// The timer fired: the recorder looks again, as a frame would.
    pub(super) fn history_tick(&self) {
        let adopted = {
            let Some(mut state) = self.state_mut() else {
                // Busy: a moment later.
                self.arm_history_tick(Duration::from_millis(250));
                return;
            };
            state.history.armed = None;
            let State {
                docs,
                panes,
                history,
                ..
            } = &mut *state;
            history.observe(
                docs.iter().chain(panes.iter().flat_map(|p| p.docs.iter())),
                Instant::now(),
            );
            history.adopt(
                docs.iter_mut()
                    .chain(panes.iter_mut().flat_map(|p| p.docs.iter_mut())),
            )
        };
        if adopted {
            self.request_redraw();
        }
        self.history_after_frame();
    }

    /// What the history thread said: a full disk, a damaged file set
    /// aside, a failure, each once in the status line; and the undo
    /// journals it found, taken by the documents still untouched.
    pub(super) fn poll_history(&self) {
        let Some(mut state) = self.state_mut() else {
            // Busy: a moment later, so a journal found does not wait for
            // the next frame.
            self.arm_history_tick(Duration::from_millis(250));
            return;
        };
        let adopted = {
            let State {
                docs,
                panes,
                history,
                ..
            } = &mut *state;
            history.adopt(
                docs.iter_mut()
                    .chain(panes.iter_mut().flat_map(|p| p.docs.iter_mut())),
            )
        };
        let mut said = None;
        while let Some(sentence) = state.history.poll() {
            said = Some(sentence);
        }
        if !adopted && said.is_none() {
            return;
        }
        if let Some(said) = said {
            state.say(layout::Feedback::Failure, said);
        }
        drop(state);
        self.request_redraw();
        self.pump();
    }

    /// The active document is closing with its changes not saved.
    pub(super) fn history_closed_unsaved(&self) {
        if let Some(mut state) = self.state_mut() {
            let State { docs, history, .. } = &mut *state;
            history.closed_unsaved(docs.active());
        }
    }

    /// Quitting: what is still unsaved is kept, and what is queued is
    /// written, waiting a moment at most.
    pub(super) fn history_finish(&self) {
        if let Some(mut state) = self.state_mut() {
            let State {
                docs,
                panes,
                history,
                ..
            } = &mut *state;
            history.finish(
                docs.iter().chain(panes.iter().flat_map(|p| p.docs.iter())),
                Duration::from_secs(2),
            );
        }
    }

    /// Runs `read` on a worker; its reply is taken by
    /// [`EditorView::poll_history_reply`], woken for it.
    fn read_history(&self, read: impl FnOnce() -> Reply + Send + 'static) {
        let (tx, rx) = mpsc::channel();
        let wake = self.wake(EditorView::poll_history_reply);
        std::thread::spawn(move || {
            let _ = tx.send(read());
            wake();
        });
        if let Some(mut state) = self.state_mut() {
            state.history_rx = Some(rx);
        }
        self.resume_display_link();
    }

    /// Git > Recover Deleted File: the files local history has versions of
    /// that are gone from disk, read on a worker, then picked in the
    /// palette.
    pub(super) fn recover_deleted_file(&self) {
        let db = crate::history::store::default_path();
        self.read_history(move || Reply::Deleted(read_deleted(db.as_deref())));
    }

    /// A deleted file picked: its latest version, read on a worker, opens
    /// as unsaved changes at its old path, which Save writes back.
    pub(super) fn recover_file(&self, path: std::path::PathBuf) {
        let db = crate::history::store::default_path();
        if let Some(mut state) = self.state_mut() {
            let said = format!("reading {} from local history", file_name(&path));
            state.message = Some((said, Instant::now()));
        }
        self.read_history(move || {
            let read = read_latest(db.as_deref(), &path);
            Reply::Recovered(path, read)
        });
        self.request_redraw();
    }

    /// Takes what a worker read for Recover Deleted File.
    pub(super) fn poll_history_reply(&self) {
        let reply = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(rx) = &state.history_rx else {
                return;
            };
            let reply = match rx.try_recv() {
                Ok(reply) => reply,
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Reply::Deleted(Err("could not read local history".into()))
                }
            };
            state.history_rx = None;
            reply
        };
        match reply {
            Reply::Deleted(Ok(rows)) if !rows.is_empty() => {
                self.open_palette_with("");
                if let Some(mut state) = self.state_mut() {
                    state.deleted_list = Some(rows);
                }
            }
            Reply::Deleted(Ok(_)) => self.say_history("no deleted files in local history".into()),
            Reply::Deleted(Err(why)) => {
                self.say_history(format!("could not read local history: {why}"));
            }
            Reply::Recovered(path, Ok(buffer)) => self.open_recovered(&path, *buffer),
            Reply::Recovered(path, Err(why)) => {
                self.say_history(format!("could not recover {}: {why}", file_name(&path)));
            }
        }
        self.request_redraw();
        self.pump();
    }

    fn say_history(&self, said: String) {
        if let Some(mut state) = self.state_mut() {
            state.message = Some((said, Instant::now()));
        }
    }

    /// Opens a recovered file's text in a tab of its own; one already open
    /// at its path, in any pane, comes to the front instead.
    fn open_recovered(&self, path: &Path, buffer: Buffer) {
        if self.state().is_some_and(|state| state.agents.on) {
            self.set_agents_mode(false);
        }
        let elsewhere = self.state().and_then(|state| pane_holding(&state, path));
        let name = file_name(path);
        if let Some((pane, tab)) = elsewhere {
            self.focus_pane(pane);
            self.activate_tab(tab);
            self.say_history(format!("{name} is open already"));
            return;
        }
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let open = state.docs.index_of(path).is_some();
            state.docs.add(buffer);
            reveal_active_tab(&mut state);
            let said = if open {
                format!("{name} is open already")
            } else {
                format!("recovered {name} from local history, not saved: Save writes it back")
            };
            state.message = Some((said, Instant::now()));
            state.title_sync_pending = true;
        }
        self.reparse();
        self.lsp_sync_open();
    }

    /// Git > Clear History for This File, once asked: every version and
    /// the undo kept for the active tab's file. Its File History closes.
    pub(super) fn clear_file_history(&self) {
        let Some(path) = self.state().and_then(|state| scrub::history_path(&state)) else {
            return;
        };
        let name = file_name(&path);
        let detail = "Every version crc kept of it, and its saved undo, are deleted. \
                      The file itself is not changed.";
        if !self.confirm_clear(
            &format!("Clear the local history of \u{201c}{name}\u{201d}?"),
            detail,
        ) {
            return;
        }
        if let Some(mut state) = self.state_mut() {
            state.history.forget(&path);
            scrub::close_cleared(&mut state, Some(&path));
            let said = format!("cleared the local history of {name}");
            state.message = Some((said, Instant::now()));
        }
        self.after_clear();
    }

    /// Git > Clear All Local History, once asked: every file's.
    pub(super) fn clear_all_history(&self) {
        let detail = "Every version crc kept of every file, and all saved undo, are \
                      deleted. No file is changed.";
        if !self.confirm_clear("Clear all local history?", detail) {
            return;
        }
        if let Some(mut state) = self.state_mut() {
            state.history.forget_all();
            scrub::close_cleared(&mut state, None);
            state.message = Some(("cleared all local history".into(), Instant::now()));
        }
        self.after_clear();
    }

    /// Asks before a clear. A test instance cannot answer a modal: it says
    /// what it would have asked, and takes the answer from the environment.
    fn confirm_clear(&self, message: &str, detail: &str) -> bool {
        if self.ivars().testing {
            eprintln!("crc: clear history prompt: {message} {detail}");
            return std::env::var("CRC_CLEAR_HISTORY_ANSWER").is_ok_and(|a| a == "clear");
        }
        ask(
            MainThreadMarker::from(self),
            message,
            detail,
            &["Clear", "Cancel"],
        ) == 0
    }

    fn after_clear(&self) {
        self.sync_title();
        self.reparse();
        self.request_redraw();
        self.pump();
    }
}
