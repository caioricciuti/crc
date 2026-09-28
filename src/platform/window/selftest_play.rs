//! The self-test player: each scripted step performed as a real event
//! through the window, and the report a `dump` writes.

use super::*;

impl EditorView {
    /// Performs one self-test step as a real event through the window.
    pub(super) fn play(&self, step: &Step) {
        let mtm = MainThreadMarker::from(self);
        let Some(window) = self.window() else {
            return;
        };
        let mouse_with = |kind: NSEventType,
                          x: f64,
                          y: f64,
                          count: isize,
                          flags: NSEventModifierFlags| {
            let at = self.convertPoint_toView(NSPoint::new(x, y), None);
            NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                kind,
                at,
                flags,
                0.0,
                window.windowNumber(),
                None,
                0,
                count,
                1.0,
            )
        };
        let mouse = |kind: NSEventType, x: f64, y: f64, count: isize| {
            mouse_with(kind, x, y, count, NSEventModifierFlags::empty())
        };
        match step {
            Step::ClickIn {
                name,
                dx,
                dy,
                count,
                mods,
            } => {
                let target = {
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    frame_of(&mut state).named(name)
                };
                let Some(rect) = target else {
                    eprintln!("selftest: no region named {name} in this frame");
                    return;
                };
                let (x, y) = (f64::from(rect.x) + dx, f64::from(rect.y) + dy);
                let mut flags = NSEventModifierFlags::empty();
                for (on, flag) in [
                    (mods.command, NSEventModifierFlags::Command),
                    (mods.shift, NSEventModifierFlags::Shift),
                    (mods.option, NSEventModifierFlags::Option),
                    (mods.control, NSEventModifierFlags::Control),
                ] {
                    if on {
                        flags |= flag;
                    }
                }
                if let Some(event) = mouse_with(NSEventType::LeftMouseDown, x, y, *count, flags) {
                    let _: () = unsafe { msg_send![self, mouseDown: &*event] };
                }
                if let Some(event) = mouse_with(NSEventType::LeftMouseUp, x, y, *count, flags) {
                    let _: () = unsafe { msg_send![self, mouseUp: &*event] };
                }
            }
            Step::Key { code, mods, chars } => {
                let mut flags = NSEventModifierFlags::empty();
                for (on, flag) in [
                    (mods.command, NSEventModifierFlags::Command),
                    (mods.shift, NSEventModifierFlags::Shift),
                    (mods.option, NSEventModifierFlags::Option),
                    (mods.control, NSEventModifierFlags::Control),
                ] {
                    if on {
                        flags |= flag;
                    }
                }
                let characters = NSString::from_str(chars);
                let plain = NSString::from_str(&chars.to_lowercase());
                let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                    NSEventType::KeyDown,
                    NSPoint::new(0.0, 0.0),
                    flags,
                    0.0,
                    window.windowNumber(),
                    None,
                    &characters,
                    &plain,
                    false,
                    *code,
                );
                let Some(event) = event else {
                    return;
                };
                // What NSApplication does with a key: the menu bar gets the
                // first look, for its key equivalents, then the window.
                //
                // The item is found and its action sent here, rather than
                // through performKeyEquivalent:, which delivers to the key
                // window's responder chain. A test instance launched behind
                // whatever the person is using is often not key, and then the
                // shortcut went nowhere and every click after it was aimed at
                // a layout that had not changed.
                let app = NSApplication::sharedApplication(mtm);
                let action = app
                    .mainMenu()
                    .filter(|_| mods.command)
                    .and_then(|menu| menu_action_for(&menu, chars, flags));
                match action {
                    Some(action) => {
                        let target: &AnyObject = self;
                        unsafe { app.sendAction_to_from(action, Some(target), None) };
                    }
                    None => window.sendEvent(&event),
                }
            }
            // Mouse events go to the view's own handlers, not through the
            // window. A window in an app that is not frontmost keeps a single
            // click for itself, to come forward with, and hands on only the
            // double and triple ones, so half a script would vanish depending
            // on what else was on screen. The handlers are what is under
            // test; which window is in front is not.
            Step::Click { x, y, count } => {
                let started = Instant::now();
                if let Some(event) = mouse(NSEventType::LeftMouseDown, *x, *y, *count) {
                    let _: () = unsafe { msg_send![self, mouseDown: &*event] };
                }
                if let Some(event) = mouse(NSEventType::LeftMouseUp, *x, *y, *count) {
                    let _: () = unsafe { msg_send![self, mouseUp: &*event] };
                }
                if let Some(mut state) = self.state_mut() {
                    state.last_selftest_click_ms = started.elapsed().as_secs_f64() * 1000.0;
                }
            }
            Step::ClickNamed { name, count } => {
                let target = {
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    match state.extensions.as_ref() {
                        Some(page) if name.starts_with("extensions.") => page.named(name),
                        _ => frame_of(&mut state).named(name),
                    }
                };
                let Some(rect) = target else {
                    eprintln!("selftest: no region named {name} in this frame");
                    return;
                };
                let (x, y) = (
                    f64::from(rect.x + rect.width / 2.0),
                    f64::from(rect.y + rect.height / 2.0),
                );
                if let Some(event) = mouse(NSEventType::LeftMouseDown, x, y, *count) {
                    let _: () = unsafe { msg_send![self, mouseDown: &*event] };
                }
                if let Some(event) = mouse(NSEventType::LeftMouseUp, x, y, *count) {
                    let _: () = unsafe { msg_send![self, mouseUp: &*event] };
                }
            }
            Step::DownNamed { name } => {
                let target = {
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    frame_of(&mut state).named(name)
                };
                let Some(rect) = target else {
                    eprintln!("selftest: no region named {name} in this frame");
                    return;
                };
                let (x, y) = (
                    f64::from(rect.x + rect.width / 2.0),
                    f64::from(rect.y + rect.height / 2.0),
                );
                if let Some(mut state) = self.state_mut() {
                    state.selftest_pointer = (x, y);
                }
                if let Some(event) = mouse(NSEventType::LeftMouseDown, x, y, 1) {
                    let _: () = unsafe { msg_send![self, mouseDown: &*event] };
                }
            }
            Step::DragBy { dx, dy } => {
                let Some((x, y)) = self.state().map(|state| state.selftest_pointer) else {
                    return;
                };
                if let Some(event) = mouse(NSEventType::LeftMouseDragged, x + dx, y + dy, 1) {
                    let _: () = unsafe { msg_send![self, mouseDragged: &*event] };
                }
            }
            Step::UpBy { dx, dy } => {
                let Some((x, y)) = self.state().map(|state| state.selftest_pointer) else {
                    return;
                };
                if let Some(event) = mouse(NSEventType::LeftMouseUp, x + dx, y + dy, 1) {
                    let _: () = unsafe { msg_send![self, mouseUp: &*event] };
                }
            }
            Step::Down { x, y, count } => {
                if let Some(mut state) = self.state_mut() {
                    state.selftest_pointer = (*x, *y);
                }
                if let Some(event) = mouse(NSEventType::LeftMouseDown, *x, *y, *count) {
                    let _: () = unsafe { msg_send![self, mouseDown: &*event] };
                }
            }
            Step::Drag { x, y } => {
                if let Some(event) = mouse(NSEventType::LeftMouseDragged, *x, *y, 1) {
                    let _: () = unsafe { msg_send![self, mouseDragged: &*event] };
                }
            }
            Step::Up { x, y } => {
                if let Some(event) = mouse(NSEventType::LeftMouseUp, *x, *y, 1) {
                    let _: () = unsafe { msg_send![self, mouseUp: &*event] };
                }
            }
            Step::Resize { width, height } => {
                window.setContentSize(NSSize::new(*width, *height));
                self.request_redraw();
                self.pump();
            }
            Step::Touch(path) => {
                if let Err(error) = std::fs::write(path, "personal sample\n") {
                    eprintln!("crc: selftest could not create {path}: {error}");
                }
            }
            Step::Write(path, text) => {
                let result = std::fs::write(path, format!("{text}\n")).and_then(|()| {
                    std::fs::File::options().write(true).open(path)?.set_times(
                        std::fs::FileTimes::new()
                            .set_modified(std::time::SystemTime::now() + Duration::from_secs(2)),
                    )
                });
                if let Err(error) = result {
                    eprintln!("crc: selftest could not write {path}: {error}");
                }
            }
            Step::Wheel(dy) => {
                if self.state().is_some_and(|state| state.palette.is_some()) {
                    self.palette_wheel_by(*dy, false);
                } else {
                    eprintln!("selftest: wheel only drives the palette");
                }
            }
            Step::Trackpad { x, y, dy } => {
                self.scroll_at(*x as f32, *y as f32, 0.0, *dy, true);
            }
            Step::Idle(_) => {}
            Step::Reenter => {
                let held = self.ivars().state.borrow_mut();
                let x = NSString::from_str("x");
                if let Some(event) = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                    NSEventType::KeyDown,
                    NSPoint::new(0.0, 0.0),
                    NSEventModifierFlags::empty(),
                    0.0,
                    window.windowNumber(),
                    None,
                    &x,
                    &x,
                    false,
                    7,
                ) {
                    let _: () = unsafe { msg_send![self, keyDown: &*event] };
                }
                let (x, y) = (80.0, 125.0);
                if let Some(event) = mouse(NSEventType::LeftMouseDown, x, y, 1) {
                    let _: () = unsafe { msg_send![self, mouseDown: &*event] };
                    let _: Option<Retained<NSMenu>> =
                        unsafe { msg_send![self, menuForEvent: &*event] };
                }
                if let Some(event) = mouse(NSEventType::LeftMouseDragged, x + 40.0, y, 1) {
                    let _: () = unsafe { msg_send![self, mouseDragged: &*event] };
                }
                if let Some(event) = mouse(NSEventType::LeftMouseUp, x + 40.0, y, 1) {
                    let _: () = unsafe { msg_send![self, mouseUp: &*event] };
                }
                let _: () = unsafe { msg_send![self, resetCursorRects] };
                let app = NSApplication::sharedApplication(mtm);
                let target: &AnyObject = self;
                for action in [
                    sel!(undo:),
                    sel!(selectAll:),
                    sel!(copy:),
                    sel!(findInProject:),
                ] {
                    unsafe { app.sendAction_to_from(action, Some(target), None) };
                }
                let range: NSRange = unsafe { msg_send![self, selectedRange] };
                let drawn = self.render().is_some();
                let close: bool = unsafe { msg_send![self, windowShouldClose: &*window] };
                let quit = app.delegate().map(|delegate| {
                    let reply: NSApplicationTerminateReply =
                        unsafe { msg_send![&*delegate, applicationShouldTerminate: &*app] };
                    reply == NSApplicationTerminateReply::TerminateCancel
                });
                drop(held);
                if let Some(mut state) = self.state_mut() {
                    state.message = Some((
                        format!(
                            "reentered: drawn={drawn} range={} close={close} quit_cancelled={}",
                            range.location == NSNotFound as usize,
                            quit.unwrap_or(false),
                        ),
                        Instant::now(),
                    ));
                }
            }
            Step::WebJs(expression) => {
                let Some(state) = self.state() else {
                    return;
                };
                match state.html_preview.as_ref().and_then(|p| p.web.as_ref()) {
                    Some(web) if expression == "probe" => web.probe(PREVIEW_PROBE),
                    Some(web) => web.probe(expression),
                    None => eprintln!("selftest: webjs with no preview page"),
                }
            }
            Step::Wait(ms) => {
                std::thread::sleep(Duration::from_millis(*ms));
                self.check_open_files();
                self.poll_tree_children();
                self.poll_project_index();
                self.poll_project_search();
                self.poll_http();
                self.poll_update();
                self.poll_ignored();
                self.poll_completion();
                self.poll_claude();
                if let Some(mut state) = self.state_mut() {
                    state.git.poll();
                }
            }
            Step::Dump(path) => {
                if let Some(mut state) = self.state_mut() {
                    sync_conflicts(&mut state);
                }
                let Some(state) = self.state() else {
                    return;
                };
                let titles: Vec<String> =
                    (0..state.docs.len()).map(|i| state.docs.title(i)).collect();
                // Where things are, so a script whose clicks miss can be told
                // from one whose clicks were ignored.
                let chrome = chrome_of(&state);
                let layout = format!(
                    "window {}x{} sidebar {:?} text {},{} {}x{} font {} theme {}",
                    state.viewport.width,
                    state.viewport.height,
                    chrome.sidebar.map(|r| r.width),
                    chrome.text.x,
                    chrome.text.y,
                    chrome.text.width,
                    chrome.text.height,
                    state.font_size,
                    if state.theme.is_dark() {
                        "dark"
                    } else {
                        "light"
                    },
                );
                let buffer = state.docs.active();
                let caret_shaped = state
                    .renderer
                    .atlas
                    .cached_editor_line(
                        (buffer.id(), buffer.rope.byte_to_line(buffer.cursor())),
                        &buffer.rope,
                    )
                    .is_some();
                let report = format!(
                    "git_open: {}\ngit_focus: {}\ngit_diff: {}\ngit_pending: {}\ngit_changes: {}\ngit_staged: {}\ngit_hunks_staged: {}\ngit_hunks_working: {}\ngit_message: {}\nproject_menu_requested: {}\ncrumb_menu: {}\nlast_click_ms: {:.3}\nfinder_entries: {}\nsidebar_edit: {}\npalette_query: {}\npalette_first: {}\npalette_scroll: {}\npanes: {}\nfocused_pane: {}\nlsp: {}\ndiagnostics: {}\ncompletion: {}\nkey_handler_draws: {}\nwindow_title: {}\nshaping_pending: {}\ncaret_shaped: {}\nclaude: {}\nterminal: {}\n{}",
                    state.git_open,
                    state.git_focus,
                    diffing(&state),
                    state.git.busy(),
                    state.git.snapshot.as_ref().map_or(0, |s| s.changes.len()),
                    state.git.snapshot.as_ref().map_or(0, |s| s
                        .changes
                        .iter()
                        .filter(|c| c.staged())
                        .count()),
                    state.git.hunk_counts().0,
                    state.git.hunk_counts().1,
                    state.git.message.rope,
                    state.project_menu_requested,
                    state.crumb_menu_requested.as_deref().unwrap_or("none"),
                    state.last_selftest_click_ms,
                    state.finder.len(),
                    state
                        .sidebar_edit
                        .as_ref()
                        .map(|e| e.field.rope.to_string())
                        .unwrap_or_default(),
                    state
                        .palette
                        .as_ref()
                        .map(|(query, _)| query.rope.to_string())
                        .unwrap_or_default(),
                    state
                        .palette
                        .as_ref()
                        .and_then(|(query, _)| {
                            palette_rows(&palette_sources(&state), &query.rope.to_string())
                                .into_iter()
                                .next()
                        })
                        .map(|(row, _)| row.title)
                        .unwrap_or_default(),
                    state.palette_scroll,
                    pane_count(&state),
                    state.focused_pane,
                    state
                        .lsp
                        .values()
                        .map(|s| format!("{} {:?}", s.name, s.phase))
                        .collect::<Vec<_>>()
                        .join(", "),
                    lsp_server_for(&state, state.docs.active())
                        .and_then(|s| state
                            .docs
                            .active()
                            .path
                            .as_ref()
                            .and_then(|p| s.diagnostics.get(p)))
                        .map_or(0, Vec::len),
                    state
                        .completion
                        .as_ref()
                        .map(|c| c
                            .shown
                            .iter()
                            .map(|i| i.label.as_str())
                            .collect::<Vec<_>>()
                            .join("|"))
                        .unwrap_or_default(),
                    self.ivars().draws_during_key_handler.get(),
                    window.title(),
                    state.renderer.atlas.has_pending_shaping(),
                    caret_shaped,
                    state.claude.as_ref().map_or("off".to_string(), |c| format!(
                        "connected={} reviews={} pending={}",
                        c.is_connected(),
                        c.reviews.len(),
                        c.reviews.values().filter(|r| r.decided.is_none()).count()
                    )),
                    format_args!(
                        "open={} focus={} height={} tabs={} selection={:?} screen={:?}",
                        state.terminal.open,
                        state.terminal.focus,
                        state.terminal.height,
                        state
                            .terminal
                            .tabs
                            .iter()
                            .map(|t| t.title.as_str())
                            .collect::<Vec<_>>()
                            .join("|"),
                        state.terminal.selection.and_then(|(a, b)| {
                            state.terminal.active_tab().map(|tab| {
                                tab.session
                                    .term
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .text_between(a, b)
                            })
                        }),
                        state
                            .terminal
                            .active_tab()
                            .map(|t| t
                                .session
                                .term
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .screen_text())
                            .unwrap_or_default()
                    ),
                    selftest::report(
                        state.docs.active(),
                        &titles,
                        state.docs.active_index(),
                        state.marked.as_deref(),
                        &layout,
                        state.native_preview.is_some(),
                    )
                );
                let activity = format!(
                    "{} sidebar={}",
                    if state.extensions.is_some() {
                        "extensions"
                    } else if state.git_open {
                        "source-control"
                    } else {
                        "explorer"
                    },
                    if state.sidebar { "on" } else { "off" }
                );
                // Markdown styled in the source: the kinds on the first
                // line, and whether any code block has a band.
                let md = {
                    let buffer = state.docs.active();
                    let first_line = buffer.rope.line_to_byte(1.min(buffer.rope.len_lines()));
                    let kinds: Vec<String> = state
                        .syntax
                        .spans_with(buffer.id(), 0..first_line, |r| {
                            buffer.rope.slice_to_string(r)
                        })
                        .iter()
                        .map(|s| format!("{:?}", s.kind))
                        .collect();
                    format!(
                        "{} bands={}",
                        kinds.join(","),
                        state
                            .syntax
                            .markdown(buffer.id())
                            .map_or(0, |m| m.bands.len())
                    )
                };
                let report = format!(
                    // First: the report ends with the document's text.
                    "message: {}\npreview: {}\nmd: {md}\nactivity: {}\npointer_targets: {}\nextensions: {}\next_commands: {}\nbulb: {}\nactions: {}\nbranch: {}\nconflicts: {}\ngit_conflicts: {}\nblame: {}\nfind_results: {}\nsignature: {}\nrename: {}\nread_only: {}\nunshaped: {}\nignored_rows: {}\ncompletion_why: {}\n{report}",
                    state.message.as_ref().map_or("", |(text, _)| text.as_str()),
                    state.html_preview.as_ref().map_or("closed".to_string(), |p| format!(
                        "open ext={} view={} probe={}",
                        p.command.installed.manifest.id,
                        match &p.web {
                            None => "none",
                            Some(web) if web.is_veiled() => "veiled",
                            Some(web) if web.is_shown() => "shown",
                            Some(_) => "loading",
                        },
                        crate::platform::webview::probe_answer().unwrap_or_default()
                    )),
                    activity,
                    state.pointer_targets.len(),
                    state.extensions.as_ref().map_or("closed".to_string(), |page| {
                        use crate::platform::extensions::Registry;
                        format!(
                            "{} selected={} installed={} registry={} confirm={} busy={} note={}",
                            if page.details { "open" } else { "list" },
                            page.selected.as_deref().unwrap_or("-"),
                            page.installed
                                .iter()
                                .map(|i| format!(
                                    "{}:{}:{}:{}",
                                    i.manifest.id,
                                    i.manifest.version,
                                    if i.enabled { "on" } else { "off" },
                                    if i.signed { "signed" } else { "unsigned" }
                                ))
                                .collect::<Vec<_>>()
                                .join(","),
                            match &page.registry {
                                Registry::Loading => "loading".to_string(),
                                Registry::Ready(e, _) => format!(
                                    "ready:{}",
                                    e.iter().map(|e| e.manifest.id.as_str()).collect::<Vec<_>>().join(",")
                                ),
                                Registry::Failed(why) => format!("failed:{why}"),
                            },
                            page.confirm.as_ref().map_or("-", |c| c.manifest().id.as_str()),
                            page.busy.as_deref().unwrap_or("-"),
                            page.note.as_deref().unwrap_or("-"),
                        )
                    }),
                    state
                        .ext_commands
                        .iter()
                        .map(|c| c.title.as_str())
                        .collect::<Vec<_>>()
                        .join("|"),
                    state.bulb.as_ref().map_or("none".to_string(), |b| format!(
                        "{} at {}",
                        b.actions.iter().filter(|a| a.disabled.is_none()).count(),
                        b.caret
                    )),
                    state
                        .palette
                        .as_ref()
                        .filter(|_| state.action_list.is_some())
                        .map(|(query, _)| palette_rows(
                            &palette_sources(&state),
                            &query.rope.to_string()
                        )
                        .into_iter()
                        .map(|(row, _)| row.title)
                        .collect::<Vec<_>>()
                        .join("|"))
                        .unwrap_or_default(),
                    state.git.branch_status(),
                    active_conflicts(&state).map_or("none".to_string(), |v| format!(
                        "{} side={} unmerged={} resolvable={} scroll={}",
                        v.conflicts.len(),
                        side_by_side(&state),
                        v.unmerged,
                        v.can_resolve(),
                        v.scroll
                    )),
                    state.git.conflict_count(),
                    state
                        .blame
                        .as_ref()
                        .map_or("", |(_, _, text)| text.as_str()),
                    state
                        .find
                        .as_ref()
                        .map(|bar| {
                            bar.results
                                .iter()
                                .map(|h| {
                                    format!(
                                        "{}:{}",
                                        h.path
                                            .file_name()
                                            .map(|n| n.to_string_lossy().into_owned())
                                            .unwrap_or_default(),
                                        h.line + 1
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("|")
                        })
                        .unwrap_or_default(),
                    state
                        .signature
                        .as_ref()
                        .map(|t| {
                            let label = &t.signature.label;
                            match &t.signature.active {
                                Some(r) => format!("{} [{}]", label, label.get(r.clone()).unwrap_or_default()),
                                None => label.clone(),
                            }
                        })
                        .unwrap_or_default(),
                    state
                        .rename
                        .as_ref()
                        .map(|r| r.field.rope.to_string())
                        .unwrap_or_default(),
                    state.docs.active().is_read_only(),
                    state.unshaped_on_screen,
                    state
                        .tree
                        .rows()
                        .iter()
                        .map(|e| state.tree.ignored(&e.path))
                        .map(|i| match i {
                            crate::project::tree::Ignored::No => "-",
                            crate::project::tree::Ignored::Here => "H",
                            crate::project::tree::Ignored::Inside => "I",
                            crate::project::tree::Ignored::Preview => "P",
                        })
                        .collect::<String>(),
                    state
                        .completion
                        .as_ref()
                        .and_then(|c| c.shown.get(c.selected))
                        .map_or("", |c| c.why.as_str()),
                );
                if let Err(e) = std::fs::write(path, report) {
                    eprintln!("crc: selftest could not write {path}: {e}");
                }
            }
            // Straight out: no unsaved-changes prompt to sit waiting for a
            // person, and no session written over the real one.
            Step::Panic => {
                panic!("selftest: forced panic");
            }
            Step::Quit => {
                self.claude_shutdown();
                std::process::exit(0)
            }
        }
    }
}

/// The action of the menu item whose key equivalent is `chars` with `flags`,
/// searching submenus. What AppKit's own lookup does, minus the dependence on
/// which window is key.
fn menu_action_for(menu: &NSMenu, chars: &str, flags: NSEventModifierFlags) -> Option<Sel> {
    let relevant = NSEventModifierFlags::Command
        | NSEventModifierFlags::Shift
        | NSEventModifierFlags::Option
        | NSEventModifierFlags::Control;
    for item in menu.itemArray().iter() {
        if let Some(submenu) = item.submenu()
            && let Some(action) = menu_action_for(&submenu, chars, flags)
        {
            return Some(action);
        }
        let key = item.keyEquivalent().to_string();
        if key.is_empty() || !key.eq_ignore_ascii_case(chars) {
            continue;
        }
        // An upper-case key equivalent is AppKit's way of writing Shift.
        let mut wanted = item.keyEquivalentModifierMask() & relevant;
        if key.chars().any(char::is_uppercase) {
            wanted |= NSEventModifierFlags::Shift;
        }
        if wanted == flags & relevant {
            return item.action();
        }
    }
    None
}
