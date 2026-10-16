//! Local history in the window: the timer for an idle snapshot that falls
//! due with no frame to notice it, what the history thread says, and the
//! versions only the window sees go: a tab closed unsaved, quitting.

use super::*;

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
}
