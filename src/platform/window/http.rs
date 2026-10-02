//! HTTP requests from a .http file: sending one on a worker and showing
//! the reply in its response tab.

use super::*;

impl EditorView {
    /// Sends the request under the caret of the active `.http` document,
    /// or re-sends the request behind the active response tab.
    ///
    /// The response tab opens at once, marked as sending, and is filled in
    /// when curl answers. Re-sending reuses the tab, so a request edited and
    /// sent ten times leaves one tab, not ten.
    pub(super) fn send_request(&self) {
        // Cmd-Return in an MCP call document runs the call.
        if self.state().is_some_and(|state| is_mcp_call(&state)) {
            self.run_mcp_call();
            return;
        }
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if state.http.is_some() {
            state.message = Some(("a request is still in flight".into(), Instant::now()));
            drop(state);
            self.request_redraw();
            return;
        }
        let prepared = if let Some(view) = state.responses.get(&state.docs.active().id()) {
            Ok(view.request.clone())
        } else {
            let buffer = state.docs.active();
            if !buffer
                .path
                .as_deref()
                .is_some_and(crate::http::is_request_file)
            {
                state.message = Some(("Send Request works in a .http file".into(), Instant::now()));
                drop(state);
                self.request_redraw();
                return;
            }
            crate::http::prepare(
                &buffer.rope.to_string(),
                buffer.cursor(),
                buffer.path.as_deref(),
            )
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                state.message = Some((error, Instant::now()));
                drop(state);
                self.request_redraw();
                return;
            }
        };
        let title = prepared.title();
        let id = show_response(
            &mut state,
            &title,
            crate::http::view::View::pending(prepared.clone()),
        );
        state.http = Some((id, crate::http::curl::spawn(prepared)));
        state.message = Some((format!("sending {title}"), Instant::now()));
        drop(state);
        self.resume_display_link();
        self.sync_title();
        self.reparse();
        self.request_redraw();
        self.pump();
    }

    pub(super) fn poll_http(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some((_, rx)) = &state.http else {
            return;
        };
        let outcome = match rx.try_recv() {
            Ok(outcome) => outcome,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("the request worker stopped".to_owned()),
        };
        let Some((id, _)) = state.http.take() else {
            return;
        };
        // The tab may have been closed while the request was out.
        let State {
            docs,
            panes,
            responses,
            message,
            ..
        } = &mut *state;
        let Some(view) = responses.get_mut(&id) else {
            return;
        };
        view.set_outcome(outcome);
        let note = format!("{}: {}", view.request.title(), view.status());
        let (text, ext) = view.text(view.segment);
        // In whichever pane the tab is: the focus may have moved to another
        // since the request went out.
        let buffer = buffer_by_id_mut(docs, panes, id);
        if let Some(buffer) = buffer {
            buffer.regenerate(&text);
            buffer.display_ext = ext;
        }
        *message = Some((note, Instant::now()));
        drop(state);
        self.sync_title();
        self.reparse();
        self.request_redraw();
    }

    /// Shows segment `index` of the active response tab.
    pub(super) fn response_select(&self, index: usize) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let id = state.docs.active().id();
        let State {
            docs, responses, ..
        } = &mut *state;
        let Some(view) = responses.get_mut(&id) else {
            return;
        };
        let Some(hit) = Some(index).filter(|i| *i < crate::http::view::Segment::ALL.len()) else {
            return;
        };
        let segment = crate::http::view::Segment::ALL[hit];
        if segment == view.segment {
            return;
        }
        view.segment = segment;
        let (text, ext) = view.text(segment);
        let buffer = docs.active_mut();
        buffer.regenerate(&text);
        buffer.display_ext = ext;
        drop(state);
        self.reparse();
        self.request_redraw();
        self.pump();
    }
}
