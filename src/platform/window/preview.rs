//! The Markdown preview pane beside the editor (Cmd-E): the page is made
//! here from the document's text and shown in a web view.

use super::*;

impl EditorView {
    /// The button beside the status-line message: what it asks for.
    pub(super) fn status_action(&self) {
        let action = self.state().and_then(|state| {
            let (text, _) = state.message.as_ref()?;
            state.said.as_ref()?.about(text)?.action.clone()
        });
        // The message stays for a cancelled quit: it is still true.
        if let Some(MessageAction::Restart) = action {
            self.restart(false);
        }
    }

    /// Opens the preview pane on the active document.
    pub(super) fn open_preview(&self) {
        self.close_preview();
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let buffer = state.docs.active();
            let preview = HtmlPreview {
                buffer: buffer.id(),
                folder: buffer
                    .path
                    .as_deref()
                    .and_then(Path::parent)
                    .map(Path::to_path_buf),
                web: None,
                page: None,
                observed: buffer.rope.clone(),
                changed_at: None,
                only: false,
            };
            state.html_preview = Some(preview);
        }
        self.run_preview();
        self.resume_display_link();
        self.request_redraw();
    }

    pub(super) fn close_preview(&self) {
        let Some(preview) = self.state_mut().map(|mut state| state.html_preview.take()) else {
            return;
        };
        // Out of the state first: the view leaving its superview can call
        // back into this one.
        if let Some(web) = preview.and_then(|p| p.web) {
            web.close();
        }
        self.request_redraw();
    }

    /// Makes a page of the document as it is. The parser is linear, so
    /// this runs right here rather than on a thread.
    pub(super) fn run_preview(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let buffer = state.docs.active();
        if state
            .html_preview
            .as_ref()
            .is_none_or(|p| p.buffer != buffer.id())
        {
            return;
        }
        let observed = buffer.rope.clone();
        let page = crate::markdown::html::page(&observed.slice_to_string(0..observed.len_bytes()));
        if let Some(preview) = state.html_preview.as_mut() {
            preview.page = Some(page);
            preview.observed = observed;
            preview.changed_at = None;
        }
    }

    /// Keeps the preview pane in step: closes it when its document is no
    /// longer the active one, makes a new page after edits, makes the web
    /// view once WebKit's rules are ready, and hands it each page.
    pub(super) fn sync_html_preview(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some(preview) = &state.html_preview else {
            return;
        };
        let active = state.docs.active();
        if active.id() != preview.buffer {
            drop(state);
            self.close_preview();
            return;
        }
        let chrome = chrome_of(&state);
        let frame = chrome
            .preview
            .map(|p| {
                if chrome.text.width > 0.0 {
                    Viewport {
                        x: p.x + layout::PREVIEW_GRAB,
                        width: (p.width - layout::PREVIEW_GRAB).max(0.0),
                        ..p
                    }
                } else {
                    p
                }
            })
            .map(ns_rect);
        // Crc's own overlays are drawn under any native view; the page
        // steps aside while one is up.
        let veiled = frame.is_none()
            || state.palette.is_some()
            || state.goto.is_some()
            || state.branch_list.is_some()
            || state.repo_list.is_some()
            || state.deleted_list.is_some()
            || state.lsp.action_list.is_some();
        let rope = active.rope.clone();
        let dark = state.theme.is_dark();
        let Some(preview) = state.html_preview.as_mut() else {
            return;
        };
        if !rope.same_as(&preview.observed) {
            preview.observed = rope;
            preview.changed_at = Some(Instant::now());
        }
        let due = preview
            .changed_at
            .is_some_and(|t| t.elapsed() >= PREVIEW_DEBOUNCE);
        drop(state);
        if due {
            self.run_preview();
        }
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some(preview) = state.html_preview.as_mut() else {
            return;
        };
        let folder = preview.folder.clone();
        let page_file = crate::platform::webview::page_path(preview.buffer);
        // Out of the state while AppKit is called: a view can call back.
        let mut web = preview.web.take();
        let page = preview.page.take();
        drop(state);
        if web.is_none()
            && let Some(frame) = frame
            && let Some(page_file) = page_file
        {
            match crate::platform::webview::rules(folder.as_deref(), &page_file) {
                // Compiling: next frame.
                None => {}
                Some(Ok(rules)) => {
                    web = crate::platform::webview::WebPreview::new(
                        frame,
                        &rules,
                        folder.as_deref(),
                        &page_file,
                    );
                    match &web {
                        Some(view) => unsafe {
                            let _: () = msg_send![self, addSubview: view.view()];
                        },
                        None => self.preview_failed("the preview needs WebKit, which did not load"),
                    }
                }
                Some(Err(())) => {
                    self.preview_failed("the preview could not be set up for this folder");
                }
            }
        }
        let mut page = page;
        if let Some(view) = &mut web {
            view.set_dark(dark);
            if let Some(frame) = frame {
                view.set_frame(frame);
            }
            if let Some(next) = page.take() {
                match view.show(&next) {
                    Ok(true) => {}
                    Ok(false) => page = Some(next),
                    Err(why) => {
                        if let Some(mut state) = self.state_mut() {
                            state.message = Some((why, Instant::now()));
                        }
                    }
                }
            }
            view.reveal_when_loaded(veiled);
        }
        let Some(mut state) = self.state_mut() else {
            return;
        };
        match state.html_preview.as_mut() {
            Some(preview) => {
                preview.web = web;
                if preview.page.is_none() {
                    preview.page = page;
                }
            }
            None => {
                drop(state);
                if let Some(view) = web {
                    view.close();
                }
            }
        }
    }

    pub(super) fn preview_failed(&self, why: &str) {
        if let Some(mut state) = self.state_mut() {
            state.message = Some((why.to_string(), Instant::now()));
        }
        self.close_preview();
    }
}

/// Something else has the editor column, and the preview gives way.
pub(super) fn preview_displaced(state: &State) -> bool {
    !matches!(column_of(state), Column::Home | Column::Text) || state.native_preview.is_some()
}

/// Cmd-E on a document that is not Markdown: what the pane is for.
pub(super) fn say_not_markdown(state: &mut State) {
    state.tell(Said::new(
        layout::Feedback::Info,
        "Markdown Preview works on Markdown files",
    ));
}
