//! Extensions and their previews in the window: the Extensions page, the
//! registry and installs, running a command on the worker and applying
//! its answer, and the HTML preview pane beside the editor.

use super::*;

impl EditorView {
    /// crc > Extensions…: the page, with what is installed now and the
    /// registry being fetched.
    pub(super) fn open_extensions(&self) {
        let installed = crate::ext::store::list();
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let mut page = crate::platform::extensions::Page::new(installed);
            page.logs = state.ext_logs.clone();
            state.extensions = Some(page);
            state.palette = None;
            state.completion = None;
            state.git_open = false;
            state.sidebar = true;
        }
        self.refresh_registry();
        self.request_redraw();
        self.pump();
    }

    pub(super) fn refresh_registry(&self) {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::ext::registry::fetch_index());
        });
        let Some(mut state) = self.state_mut() else {
            return;
        };
        state.ext_registry_rx = Some(rx);
        if let Some(page) = &mut state.extensions {
            page.registry = crate::platform::extensions::Registry::Loading;
        }
        drop(state);
        self.resume_display_link();
    }

    /// After anything that changes what is installed: the page's list, the
    /// Extensions menu, and the extension thread's loaded instances.
    pub(super) fn extensions_changed(&self) {
        let installed = crate::ext::store::list();
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.ext_generation += 1;
            if let Some(page) = &mut state.extensions {
                if page.selected.as_ref().is_some_and(|id| {
                    !installed.iter().any(|i| &i.manifest.id == id) && page.available(id).is_none()
                }) {
                    page.selected = None;
                }
                page.installed = installed;
            }
        }
        self.rebuild_extension_menu();
    }

    pub(super) fn extensions_action(&self, action: crate::platform::extensions::Action) {
        use crate::platform::extensions::{Action, Pending};
        match action {
            Action::Select(id) => {
                let Some(mut state) = self.state_mut() else {
                    return;
                };
                if let Some(page) = &mut state.extensions {
                    if page.selected.as_ref() != Some(&id) {
                        page.readme_scroll = 0;
                    }
                    page.selected = Some(id);
                    page.confirm = None;
                    page.details = true;
                }
            }
            Action::Home => {
                let Some(mut state) = self.state_mut() else {
                    return;
                };
                if let Some(page) = &mut state.extensions {
                    page.selected = None;
                    page.confirm = None;
                    page.details = true;
                }
            }
            Action::Install(id) => {
                let Some(mut state) = self.state_mut() else {
                    return;
                };
                if let Some(page) = &mut state.extensions
                    && let Some(entry) = page.available(&id).cloned()
                {
                    page.selected = Some(id);
                    page.confirm = Some(Pending::Registry(Box::new(entry)));
                }
            }
            Action::Cancel => {
                let Some(mut state) = self.state_mut() else {
                    return;
                };
                if let Some(page) = &mut state.extensions {
                    page.confirm = None;
                }
            }
            Action::Confirm => {
                let pending = {
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    state.extensions.as_mut().and_then(|p| p.confirm.take())
                };
                match pending {
                    Some(Pending::Registry(entry)) => {
                        let name = entry.manifest.name.clone();
                        let (tx, rx) = mpsc::channel();
                        std::thread::spawn(move || {
                            let _ = tx.send(crate::ext::registry::download(&entry));
                        });
                        let Some(mut state) = self.state_mut() else {
                            return;
                        };
                        state.ext_install_rx = Some(rx);
                        if let Some(page) = &mut state.extensions {
                            page.busy = Some(format!("Installing {name}\u{2026}"));
                        }
                        drop(state);
                        self.resume_display_link();
                    }
                    Some(Pending::Folder(package)) => self.finish_install(Ok(*package)),
                    None => {}
                }
            }
            Action::Uninstall(id) => {
                let installed = self
                    .ivars()
                    .state
                    .borrow()
                    .extensions
                    .as_ref()
                    .and_then(|p| p.installed(&id).cloned());
                if let Some(installed) = installed {
                    let note = match crate::ext::store::uninstall(&installed) {
                        Ok(()) => format!("Removed {}", installed.manifest.name),
                        Err(e) => e,
                    };
                    self.extensions_changed();
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    if let Some(page) = &mut state.extensions {
                        page.note = Some(note);
                    }
                }
            }
            Action::Toggle(id) => {
                let installed = self
                    .ivars()
                    .state
                    .borrow()
                    .extensions
                    .as_ref()
                    .and_then(|p| p.installed(&id).cloned());
                if let Some(installed) = installed {
                    let on = !installed.enabled;
                    let note = match crate::ext::store::set_enabled(&installed, on) {
                        Ok(()) => format!(
                            "{} is {}",
                            installed.manifest.name,
                            if on { "on" } else { "off" }
                        ),
                        Err(e) => e,
                    };
                    self.extensions_changed();
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    if let Some(page) = &mut state.extensions {
                        page.note = Some(note);
                    }
                }
            }
            Action::InstallFolder => {
                let folder = self.choose_extension_folder();
                if let Some(folder) = folder {
                    let result = crate::ext::store::Package::from_folder(&folder);
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    if let Some(page) = &mut state.extensions {
                        match result {
                            Ok(package) => {
                                page.selected = Some(package.manifest.id.clone());
                                page.confirm = Some(Pending::Folder(Box::new(package)));
                            }
                            Err(e) => page.note = Some(format!("Not installed: {e}")),
                        }
                    }
                }
            }
            Action::Refresh => self.refresh_registry(),
            Action::Close => {
                let Some(mut state) = self.state_mut() else {
                    return;
                };
                if let Some(page) = &mut state.extensions {
                    page.details = false;
                    page.confirm = None;
                }
            }
        }
        self.request_redraw();
        self.pump();
    }

    /// The folder to install from. A test instance never shows the panel:
    /// `CRC_EXT_FOLDER` names it instead.
    pub(super) fn choose_extension_folder(&self) -> Option<std::path::PathBuf> {
        if self.ivars().testing {
            return std::env::var_os("CRC_EXT_FOLDER").map(Into::into);
        }
        choose_path(
            MainThreadMarker::from(self),
            true,
            Some("Choose a folder with manifest.json, README.md and the extension's .wasm"),
        )
    }

    pub(super) fn finish_install(&self, result: Result<crate::ext::store::Package, String>) {
        let note = match result.and_then(|p| crate::ext::store::install(&p)) {
            Ok(installed) => {
                let id = installed.manifest.id.clone();
                let note = format!(
                    "Installed {} {}{}",
                    installed.manifest.name,
                    installed.manifest.version,
                    if installed.signed { "" } else { ", unsigned" }
                );
                self.extensions_changed();
                let Some(mut state) = self.state_mut() else {
                    return;
                };
                if let Some(page) = &mut state.extensions {
                    page.selected = Some(id);
                }
                drop(state);
                note
            }
            Err(e) => format!("Not installed: {e}"),
        };
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if let Some(page) = &mut state.extensions {
            page.busy = None;
            page.note = Some(note.clone());
        } else {
            state.message = Some((note, Instant::now()));
        }
    }

    /// The Extensions menu: its first two items, then every enabled
    /// extension's commands. The palette reads them from here.
    pub(super) fn rebuild_extension_menu(&self) {
        let installed = crate::ext::store::list();
        let mut commands = Vec::new();
        for i in installed.iter().filter(|i| i.enabled && !i.tampered) {
            for c in &i.manifest.commands {
                commands.push(ExtCommand {
                    installed: i.clone(),
                    command: c.id.clone(),
                    title: c.title.clone(),
                });
            }
        }
        let mtm = MainThreadMarker::from(self);
        // Top-level holders have no title of their own; the submenu does.
        let menu = NSApplication::sharedApplication(mtm)
            .mainMenu()
            .and_then(|bar| {
                (0..bar.numberOfItems())
                    .filter_map(|i| bar.itemAtIndex(i).and_then(|item| item.submenu()))
                    .find(|menu| menu.title().to_string() == "Extensions")
            });
        if let Some(menu) = menu {
            while menu.numberOfItems() > 2 {
                menu.removeItemAtIndex(2);
            }
            for item in extension_items(mtm, &commands) {
                menu.addItem(&item);
            }
        }
        if let Some(mut state) = self.state_mut() {
            state.ext_commands = commands;
        }
    }

    /// Runs extension command `tag` on the selection, or the whole document
    /// when nothing is selected, on the extension thread.
    pub(super) fn run_extension(&self, tag: isize) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some(command) = usize::try_from(tag)
            .ok()
            .and_then(|t| state.ext_commands.get(t))
            .cloned()
        else {
            return;
        };
        let buffer = state.docs.active();
        let selection = buffer.selection();
        let range = selection.clone().unwrap_or(0..buffer.rope.len_bytes());
        let manifest = &command.installed.manifest;
        if !manifest.may_read(selection.is_some()) {
            state.message = Some((
                if selection.is_none() {
                    format!("{}: select some text first", command.title)
                } else {
                    format!("{} does not work on a selection", manifest.name)
                },
                Instant::now(),
            ));
            drop(state);
            self.request_redraw();
            return;
        }
        // A preview's command opens the pane rather than answering once.
        if manifest.may_preview() && command.command == crate::ext::manifest::PREVIEW_COMMAND {
            drop(state);
            self.open_preview(command);
            return;
        }
        // Refused here, on the length alone: nothing is copied to find out.
        if range.len() > crate::ext::run::MAX_REQUEST {
            state.message = Some((
                format!(
                    "{}: the text is over {} MB, more than an extension is given",
                    command.title,
                    crate::ext::run::MAX_REQUEST >> 20
                ),
                Instant::now(),
            ));
            drop(state);
            self.request_redraw();
            return;
        }
        let request = crate::ext::run::Request {
            command: command.command.clone(),
            text: crate::ext::run::Text::of(&buffer.rope, range.clone()),
            selection: selection.is_some(),
            language: ext_language(buffer),
        };
        let call = ExtCall {
            buffer: buffer.id(),
            range,
            snapshot: buffer.rope.clone(),
            title: command.title.clone(),
            preview: false,
        };
        send_ext_job(&mut state, &command, request, call);
        drop(state);
        self.resume_display_link();
        self.request_redraw();
    }

    /// Opens the preview pane on the active document, made by `command`.
    pub(super) fn open_preview(&self, command: ExtCommand) {
        self.close_preview();
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let buffer = state.docs.active();
            let preview = HtmlPreview {
                buffer: buffer.id(),
                command,
                folder: buffer
                    .path
                    .as_deref()
                    .and_then(Path::parent)
                    .map(Path::to_path_buf),
                web: None,
                page: None,
                observed: buffer.rope.clone(),
                changed_at: None,
                running: false,
                answered: false,
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

    /// Asks the preview's extension for a page of the document as it is.
    pub(super) fn run_preview(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some(command) = state.html_preview.as_ref().map(|p| p.command.clone()) else {
            return;
        };
        let buffer = state.docs.active();
        if state
            .html_preview
            .as_ref()
            .is_some_and(|p| p.buffer != buffer.id())
        {
            return;
        }
        let request = crate::ext::run::Request {
            command: command.command.clone(),
            text: crate::ext::run::Text::of(&buffer.rope, 0..buffer.rope.len_bytes()),
            selection: false,
            language: ext_language(buffer),
        };
        let call = ExtCall {
            buffer: buffer.id(),
            range: 0..buffer.rope.len_bytes(),
            snapshot: buffer.rope.clone(),
            title: command.title.clone(),
            preview: true,
        };
        let observed = buffer.rope.clone();
        let sent = send_ext_job(&mut state, &command, request, call);
        if let Some(preview) = state.html_preview.as_mut() {
            preview.observed = observed;
            preview.changed_at = None;
            preview.running = sent;
        }
    }

    /// Keeps the preview pane in step: closes it when its document is no
    /// longer the active one, asks for a new page after edits, makes the
    /// web view once WebKit's rules are ready, and hands it each page.
    pub(super) fn sync_html_preview(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some(preview) = &state.html_preview else {
            return;
        };
        let active = state.docs.active();
        let gone = active.id() != preview.buffer
            || !state.ext_commands.iter().any(|c| {
                c.installed.manifest.id == preview.command.installed.manifest.id
                    && c.command == preview.command.command
            });
        if gone {
            drop(state);
            self.close_preview();
            return;
        }
        let frame = chrome_of(&state).preview.map(ns_rect);
        // Crc's own overlays are drawn under any native view; the page
        // steps aside while one is up.
        let veiled = frame.is_none()
            || state.palette.is_some()
            || state.goto.is_some()
            || state.branch_list.is_some()
            || state.action_list.is_some();
        let rope = active.rope.clone();
        let dark = state.theme.is_dark();
        let Some(preview) = state.html_preview.as_mut() else {
            return;
        };
        if !rope.same_as(&preview.observed) {
            preview.observed = rope;
            preview.changed_at = Some(Instant::now());
        }
        let due = !preview.running
            && preview
                .changed_at
                .is_some_and(|t| t.elapsed() >= PREVIEW_DEBOUNCE);
        let folder = preview.folder.clone();
        let page_file = crate::platform::webview::page_path(preview.buffer);
        // Out of the state while AppKit is called: a view can call back.
        let mut web = preview.web.take();
        let page = preview.page.take();
        drop(state);
        if due {
            self.run_preview();
        }
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

    /// Everything extensions sent back: the registry, a download, command
    /// answers. Runs from the display link.
    pub(super) fn ext_poll(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if let Some(rx) = &state.ext_registry_rx
            && let Ok(result) = rx.try_recv()
        {
            state.ext_registry_rx = None;
            if let Some(page) = &mut state.extensions {
                page.registry = match result {
                    Ok((entries, skipped)) => {
                        crate::platform::extensions::Registry::Ready(entries, skipped)
                    }
                    Err(e) => crate::platform::extensions::Registry::Failed(e),
                };
            }
            self.ivars().needs_redraw.set(true);
        }
        let download = state
            .ext_install_rx
            .as_ref()
            .and_then(|rx| rx.try_recv().ok());
        let mut done = Vec::new();
        for (_, rx) in state.ext_worker.iter().chain(&state.ext_preview_worker) {
            while let Ok(answer) = rx.try_recv() {
                done.push(answer);
            }
        }
        drop(state);
        if let Some(result) = download {
            if let Some(mut state) = self.state_mut() {
                state.ext_install_rx = None;
            }
            self.finish_install(result);
            self.ivars().needs_redraw.set(true);
        }
        for answer in done {
            self.ext_answer(answer);
        }
    }

    /// Applies an extension's answer, as one undo step, if its document has
    /// not changed since the call.
    pub(super) fn ext_answer(&self, done: crate::ext::run::Done) {
        let (rows, cols) = self.grid();
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if !done.log.is_empty() {
            state.ext_logs.insert(done.id.clone(), done.log.clone());
            if let Some(page) = &mut state.extensions {
                page.logs.insert(done.id.clone(), done.log);
            }
        }
        let Some(call) = state.ext_pending.remove(&done.tag) else {
            return;
        };
        // Three failures in a row, not counting time or instruction limits,
        // turn an extension off until someone turns it back on.
        let broken = match &done.result {
            Err(_) if !done.over_budget => {
                let count = state.ext_failures.entry(done.id.clone()).or_insert(0);
                *count += 1;
                *count >= EXT_FAILURES_TO_DISABLE
            }
            Err(_) => false,
            Ok(_) => {
                state.ext_failures.remove(&done.id);
                false
            }
        };
        if broken {
            state.ext_failures.remove(&done.id);
            let installed = state
                .ext_commands
                .iter()
                .find(|c| c.installed.manifest.id == done.id)
                .map(|c| c.installed.clone());
            drop(state);
            if let Some(installed) = installed {
                let note = match crate::ext::store::set_enabled(&installed, false) {
                    Ok(()) => format!(
                        "{} failed three times in a row and was turned off; turn it back on in Extensions",
                        installed.manifest.name
                    ),
                    Err(e) => format!("{} keeps failing: {e}", installed.manifest.name),
                };
                self.extensions_changed();
                if let Some(mut state) = self.state_mut() {
                    state.message = Some((note, Instant::now()));
                }
            }
            self.ivars().needs_redraw.set(true);
            return;
        }
        if call.preview {
            drop(state);
            self.preview_answer(&call, done.result, done.over_budget);
            return;
        }
        let response = match done.result {
            Ok(r) => r,
            Err(e) => {
                state.message = Some((e, Instant::now()));
                self.ivars().needs_redraw.set(true);
                return;
            }
        };
        let mut changed = false;
        let mut note = response.message.clone();
        if let Some(text) = response.replace {
            let buffer = all_docs_mut(&mut state)
                .into_iter()
                .flat_map(|d| d.iter_mut())
                .find(|b| b.id() == call.buffer);
            match buffer {
                Some(b) if b.rope.same_text(&call.snapshot) => {
                    if b.rope.slice_to_string(call.range.clone()) != text {
                        changed = b.replace_ranges(&[(call.range.clone(), text)]) > 0;
                        b.scroll_to_cursor(rows, cols);
                    } else if note.is_none() {
                        note = Some(format!("{}: nothing to change", call.title));
                    }
                }
                Some(_) => note = Some(format!("{}: skipped, the text changed", call.title)),
                None => {}
            }
        }
        if changed {
            state.lsp_dirty.insert(call.buffer, Instant::now());
            state.gutter_dirty.insert(call.buffer, Instant::now());
        }
        state.message = Some((note.unwrap_or_else(|| call.title.clone()), Instant::now()));
        let active = state.docs.active().id() == call.buffer;
        drop(state);
        if changed && active {
            self.reparse();
            self.sync_title();
        }
        self.ivars().needs_redraw.set(true);
    }

    /// An answer for the preview pane. It says nothing in the status line
    /// unless the extension did, or something failed; a run that fails
    /// before any page arrived closes the pane.
    pub(super) fn preview_answer(
        &self,
        call: &ExtCall,
        result: Result<crate::ext::run::Response, String>,
        over_budget: bool,
    ) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some(preview) = state
            .html_preview
            .as_mut()
            .filter(|p| p.buffer == call.buffer)
        else {
            return;
        };
        preview.running = false;
        let (note, close) = match result {
            Ok(crate::ext::run::Response {
                html: Some(page),
                message,
                ..
            }) => {
                preview.page = Some(page);
                preview.answered = true;
                (message, false)
            }
            Ok(response) => (
                Some(
                    response
                        .message
                        .unwrap_or_else(|| format!("{} made no page", call.title)),
                ),
                !preview.answered,
            ),
            // Out of time on this document: every edit would be the same,
            // so the pane closes rather than trying again and again.
            Err(_) if over_budget => (
                Some(format!(
                    "{}: the document is too large to preview",
                    call.title
                )),
                true,
            ),
            Err(error) => (Some(error), !preview.answered),
        };
        if let Some(note) = note {
            state.message = Some((note, Instant::now()));
        }
        drop(state);
        if close {
            self.close_preview();
        }
        self.ivars().needs_redraw.set(true);
        self.resume_display_link();
    }
}

/// crc's name for `buffer`'s language, as extensions are told it.
pub(super) fn ext_language(buffer: &Buffer) -> String {
    if is_markdown(buffer) {
        return "markdown".into();
    }
    buffer
        .extension()
        .and_then(|e| Language::from_extension(&e))
        .map(|l| format!("{l:?}").to_lowercase())
        .unwrap_or_else(|| "text".into())
}

/// Sends `request` to an extension thread, starting it if needed, and
/// files `call` to meet the answer: a preview's thread for a preview, the
/// commands' thread otherwise. False when the thread is gone.
pub(super) fn send_ext_job(
    state: &mut State,
    command: &ExtCommand,
    request: crate::ext::run::Request,
    call: ExtCall,
) -> bool {
    let preview = call.preview;
    let slot = if preview {
        &mut state.ext_preview_worker
    } else {
        &mut state.ext_worker
    };
    if slot.is_none() {
        *slot = Some(crate::ext::run::spawn(Box::new(|| {})));
    }
    let job = state.ext_next_job;
    state.ext_next_job += 1;
    let worker = if preview {
        &state.ext_preview_worker
    } else {
        &state.ext_worker
    };
    let sent = worker.as_ref().is_some_and(|(tx, _)| {
        tx.send(crate::ext::run::Job {
            tag: job,
            manifest: command.installed.manifest.clone(),
            wasm: command.installed.wasm(),
            sha256: command.installed.sha256.clone(),
            generation: state.ext_generation,
            request,
        })
        .is_ok()
    });
    if sent {
        state.ext_pending.insert(job, call);
    } else {
        if preview {
            state.ext_preview_worker = None;
        } else {
            state.ext_worker = None;
        }
        state.message = Some(("the extension thread stopped".into(), Instant::now()));
    }
    sent
}

/// Whether the Extensions details have the editor column.
pub(super) fn ext_details(state: &State) -> bool {
    state.extensions.as_ref().is_some_and(|p| p.details)
}

/// Something else has the editor column, and the preview gives way.
pub(super) fn preview_displaced(state: &State) -> bool {
    ext_details(state)
        || diffing(state)
        || active_review(state).is_some()
        || side_by_side(state)
        || state.native_preview.is_some()
}

/// The command Cmd-E runs: the first installed one that may show a
/// preview.
pub(super) fn preview_command(state: &State) -> Option<ExtCommand> {
    state
        .ext_commands
        .iter()
        .find(|c| {
            c.installed.manifest.may_preview() && c.command == crate::ext::manifest::PREVIEW_COMMAND
        })
        .cloned()
}
