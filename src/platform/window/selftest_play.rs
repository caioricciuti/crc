//! The self-test player: each scripted step performed as a real event
//! through the window, and the report a `dump` writes.

use super::*;

/// How long a `wait` step sleeps between looks at the Agents reads.
const WAIT_SLICE: Duration = Duration::from_millis(50);

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
        // A left-button event, sent to the view as AppKit would send it.
        let press = |kind: NSEventType, x: f64, y: f64, count: isize, flags| {
            let Some(event) = mouse_with(kind, x, y, count, flags) else {
                return;
            };
            if kind == NSEventType::LeftMouseDown {
                let _: () = unsafe { msg_send![self, mouseDown: &*event] };
            } else if kind == NSEventType::LeftMouseUp {
                let _: () = unsafe { msg_send![self, mouseUp: &*event] };
            } else {
                let _: () = unsafe { msg_send![self, mouseDragged: &*event] };
            }
        };
        let send = |kind: NSEventType, x: f64, y: f64, count: isize| {
            press(kind, x, y, count, NSEventModifierFlags::empty())
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
                    region_named(&mut state, name)
                };
                let Some(rect) = target else {
                    eprintln!("selftest: no region named {name} in this frame");
                    return;
                };
                let (x, y) = (f64::from(rect.x) + dx, f64::from(rect.y) + dy);
                let flags = modifier_flags(mods);
                press(NSEventType::LeftMouseDown, x, y, *count, flags);
                press(NSEventType::LeftMouseUp, x, y, *count, flags);
            }
            Step::Key { code, mods, chars } => {
                let flags = modifier_flags(mods);
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
                send(NSEventType::LeftMouseDown, *x, *y, *count);
                send(NSEventType::LeftMouseUp, *x, *y, *count);
                if let Some(mut state) = self.state_mut() {
                    state.last_selftest_click_ms = started.elapsed().as_secs_f64() * 1000.0;
                }
            }
            Step::ClickNamed { name, count } => {
                let target = {
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    region_named(&mut state, name)
                };
                let Some(rect) = target else {
                    eprintln!("selftest: no region named {name} in this frame");
                    return;
                };
                let (x, y) = (
                    f64::from(rect.x + rect.width / 2.0),
                    f64::from(rect.y + rect.height / 2.0),
                );
                send(NSEventType::LeftMouseDown, x, y, *count);
                send(NSEventType::LeftMouseUp, x, y, *count);
            }
            Step::DownNamed { name } => {
                let target = {
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    region_named(&mut state, name)
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
                send(NSEventType::LeftMouseDown, x, y, 1);
            }
            Step::MoveIn { name, dx, dy, mods } => {
                let target = {
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    region_named(&mut state, name)
                };
                let Some(rect) = target else {
                    eprintln!("selftest: no region named {name} in this frame");
                    return;
                };
                let (x, y) = (f64::from(rect.x) + dx, f64::from(rect.y) + dy);
                if let Some(mut state) = self.state_mut() {
                    state.selftest_pointer = (x, y);
                }
                let event = mouse_with(NSEventType::MouseMoved, x, y, 0, modifier_flags(mods));
                if let Some(event) = event {
                    let _: () = unsafe { msg_send![self, mouseMoved: &*event] };
                }
            }
            Step::Flags(mods) => {
                let empty = NSString::from_str("");
                let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                    NSEventType::FlagsChanged,
                    NSPoint::new(0.0, 0.0),
                    modifier_flags(mods),
                    0.0,
                    window.windowNumber(),
                    None,
                    &empty,
                    &empty,
                    false,
                    // kVK_Command: the key that changed, whichever it was.
                    55,
                );
                if let Some(event) = event {
                    let _: () = unsafe { msg_send![self, flagsChanged: &*event] };
                }
            }
            Step::DragBy { dx, dy } => {
                let Some((x, y)) = self.state().map(|state| state.selftest_pointer) else {
                    return;
                };
                send(NSEventType::LeftMouseDragged, x + dx, y + dy, 1);
            }
            Step::UpBy { dx, dy } => {
                let Some((x, y)) = self.state().map(|state| state.selftest_pointer) else {
                    return;
                };
                send(NSEventType::LeftMouseUp, x + dx, y + dy, 1);
            }
            Step::At {
                mouse,
                name,
                fx,
                fy,
                mods,
            } => {
                let target = {
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    region_named(&mut state, name)
                };
                let Some(rect) = target else {
                    eprintln!("selftest: no region named {name} in this frame");
                    return;
                };
                // Just inside: a rectangle holds its left edge, not its right.
                let inside = |start: f32, size: f32, f: f64| {
                    let at = f64::from(start) + f64::from(size) * f.clamp(0.0, 1.0);
                    at.min(f64::from(start + size) - 0.5).max(f64::from(start))
                };
                let (x, y) = (
                    inside(rect.x, rect.width, *fx),
                    inside(rect.y, rect.height, *fy),
                );
                use crate::platform::selftest::Mouse;
                let flags = modifier_flags(mods);
                match mouse {
                    Mouse::Click => {
                        press(NSEventType::LeftMouseDown, x, y, 1, flags);
                        press(NSEventType::LeftMouseUp, x, y, 1, flags);
                    }
                    Mouse::Down => {
                        if let Some(mut state) = self.state_mut() {
                            state.selftest_pointer = (x, y);
                        }
                        press(NSEventType::LeftMouseDown, x, y, 1, flags);
                    }
                    Mouse::Drag => press(NSEventType::LeftMouseDragged, x, y, 1, flags),
                    Mouse::Up => press(NSEventType::LeftMouseUp, x, y, 1, flags),
                }
            }
            Step::Down { x, y, count } => {
                if let Some(mut state) = self.state_mut() {
                    state.selftest_pointer = (*x, *y);
                }
                send(NSEventType::LeftMouseDown, *x, *y, *count);
            }
            Step::Drag { x, y } => {
                send(NSEventType::LeftMouseDragged, *x, *y, 1);
            }
            Step::Up { x, y } => {
                send(NSEventType::LeftMouseUp, *x, *y, 1);
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
            Step::Trackpad { x, y, dy, dx } => {
                self.scroll_at(*x as f32, *y as f32, *dx, *dy, true);
            }
            // Over the terminal, the way `scrollWheel:` sends it there.
            Step::WheelAt { name, fx, fy, dy } => {
                let target = {
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    region_named(&mut state, name)
                };
                let Some(rect) = target else {
                    eprintln!("selftest: no region named {name} in this frame");
                    return;
                };
                let x = rect.x + rect.width * fx.clamp(0.0, 1.0) as f32;
                let y = rect.y + rect.height * fy.clamp(0.0, 1.0) as f32;
                let (x, y) = (
                    x.min(rect.x + rect.width - 0.5),
                    y.min(rect.y + rect.height - 0.5),
                );
                if self.chrome().terminal.is_some_and(|r| r.contains(x, y)) {
                    let lines = self.wheel_lines_by(*dy, true, WheelTarget::Terminal);
                    self.terminal_wheel(x, y, lines, NSEventModifierFlags::empty());
                } else {
                    self.scroll_at(x, y, 0.0, *dy, true);
                }
            }
            Step::Idle(_) => {}
            Step::Front(front) => {
                if let Some(mut state) = self.state_mut() {
                    state.agents.front = Some(*front);
                }
                if *front {
                    self.came_to_front();
                }
            }
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
                send(NSEventType::LeftMouseDragged, x + 40.0, y, 1);
                send(NSEventType::LeftMouseUp, x + 40.0, y, 1);
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
                // Slept in slices, the Agents reads looked at between them:
                // the display link that takes them does not fire while
                // this thread sleeps, and a folder's status is read only
                // once its list read has landed, so one look at the end
                // would leave the inspector sketched for the dump after.
                let mut left = Duration::from_millis(*ms);
                while !left.is_zero() {
                    let slice = left.min(WAIT_SLICE);
                    std::thread::sleep(slice);
                    left -= slice;
                    self.poll_agents();
                }
                self.check_open_files();
                self.poll_tree_children();
                self.poll_project_index();
                self.poll_project_search();
                self.poll_http();
                self.poll_update();
                self.poll_ignored();
                self.poll_completion();
                self.poll_claude();
                self.poll_syntax();
                // File History's replies too: the display link that takes
                // them does not fire while the display sleeps.
                self.poll_scrub();
                // And local history: an undo journal found for a file,
                // and what Recover Deleted File read.
                self.poll_history();
                self.poll_history_reply();
                if let Some(mut state) = self.state_mut() {
                    state.git.poll();
                }
            }
            Step::Fade(ms) => {
                if let Some(mut state) = self.state_mut() {
                    super::scrub::freeze_fade(&mut state, *ms);
                }
                self.request_redraw();
            }
            Step::Frame(path) => {
                // What is on screen, drawn again through the same renderer
                // into a texture: no screen capture, which a test instance
                // (and a shell without the permission) cannot rely on.
                self.render();
                let Some(mut state) = self.state_mut() else {
                    return;
                };
                let state = &mut *state;
                let scale = state.renderer.atlas.metrics.scale;
                let (w, h) = (state.viewport.width, state.viewport.height);
                let (pw, ph) = ((w * scale) as usize, (h * scale) as usize);
                let bgra = state.renderer.render_offscreen(
                    pw,
                    ph,
                    &state.glyphs,
                    (w, h),
                    state.theme.background,
                );
                let written = crate::render::bmp::write_bmp(path, pw, ph, |x, y| {
                    let at = (y * pw + x) * 4;
                    [bgra[at], bgra[at + 1], bgra[at + 2]]
                });
                if let Err(e) = written {
                    eprintln!("selftest: frame {path}: {e}");
                }
            }
            Step::Dump(path) => {
                if let Some(mut state) = self.state_mut() {
                    sync_conflicts(&mut state);
                    sync_tables(&mut state);
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
                    "window {}x{} sidebar {:?} text {},{} {}x{} preview {:?} font {} theme {}",
                    state.viewport.width,
                    state.viewport.height,
                    chrome.sidebar.map(|r| r.width),
                    chrome.text.x,
                    chrome.text.y,
                    chrome.text.width,
                    chrome.text.height,
                    chrome.preview.map(|r| r.width),
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
                    "git_open: {}\ngit_focus: {}\ngit_diff: {}\ngit_pending: {}\ngit_changes: {}\ngit_staged: {}\ngit_hunks_staged: {}\ngit_hunks_working: {}\ngit_message: {}\ngit_message_lines: {}\ngit_message_caret: {}\ngit_subject_len: {}\ngit_amend: {}\ngit_sync: {}\ngit_diff_selection: {}\nproject_menu_requested: {}\ncrumb_menu: {}\nlast_click_ms: {:.3}\nfinder_entries: {}\nsidebar_edit: {}\npalette_query: {}\npalette_first: {}\npalette_scroll: {}\npalette_loading: {}\nfind_searching: {}\npanes: {}\nfocused_pane: {}\nlsp: {}\ndiagnostics: {}\ncompletion: {}\nkey_handler_draws: {}\nwindow_title: {}\nshaping_pending: {}\ncaret_shaped: {}\nclaude: {}\nterminal: {}\n{}",
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
                    // One line in the dump, its breaks written out.
                    state.git.message.rope.to_string().replace('\n', "\\n"),
                    state.git.message.rope.len_lines(),
                    {
                        let (line, column) = state.git.message.cursor_position();
                        format!("{}:{}", line + 1, column + 1)
                    },
                    crate::platform::git_panel::subject_len(&state.git.message.rope.to_string()),
                    state.git.amend_state(),
                    state.git.sync_summary(),
                    {
                        let lines = state.git.diff_selection();
                        format!(
                            "{}-{} cursor {}",
                            lines.start(),
                            lines.end(),
                            state.git.diff_cursor
                        )
                    },
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
                    state.palette.as_ref().is_some_and(|(query, _)| {
                        let sources = palette_sources(&state);
                        let text = query.rope.to_string();
                        palette_words(&sources, &text, PaletteMode::of(&sources)).loading
                    }),
                    state.find.as_ref().is_some_and(|bar| bar.searching),
                    pane_count(&state),
                    state.focused_pane,
                    state
                        .lsp
                        .servers
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
                        "open={} focus={} height={} tabs={} selection={:?} screen={:?} scrollback={} back={} find={} link={} opened={}",
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
                            .unwrap_or_default(),
                        state.terminal.active_tab().map_or(0, |t| t
                            .session
                            .term
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .scrollback_len()),
                        state.terminal.back,
                        state.find.as_ref().map_or("none".to_string(), |bar| {
                            let query = bar.query.rope.to_string();
                            match &bar.terminal {
                                Some(find) => {
                                    let (at, total) = find.count();
                                    let at = if total == 0 { 0 } else { at + 1 };
                                    format!(
                                        "{query} {at} of {total} target=terminal keys={}",
                                        bar.has_keys
                                    )
                                }
                                None => format!("{query} target=document keys={}", bar.has_keys),
                            }
                        }),
                        state
                            .terminal
                            .link
                            .as_ref()
                            .map_or("none".to_string(), |(_, link)| {
                                let spans: Vec<String> = link
                                    .spans
                                    .iter()
                                    .map(|(_, from, to)| format!("{from}-{to}"))
                                    .collect();
                                format!("{}@{}", link.url, spans.join(","))
                            }),
                        state.terminal.opened.join("|"),
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
                    if state.mcp.open {
                        "mcp"
                    } else if state.extensions.is_some() {
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
                // Syntax spans the last frame drew the editor with.
                let spans = state.spans.len();
                let report = format!(
                    // First: the report ends with the document's text.
                    "message: {}\npreview: {}\nmd: {md}\nspans: {spans}\nactivity: {}\npointer_targets: {}\nextensions: {}\next_commands: {}\nbulb: {}\nactions: {}\nbranch: {}\nrepo: {}\ngit_note: {}\nhome: {}\nmcp: {}\nmcp_page: {}\nsettings_page: {}\nreview_page: {}\nterminals: {}\nagents: {}\ninspector: {}\nteams: {}\nagents_tree: {}\nagents_activity: {}\nagents_prs: {}\nconflicts: {}\ngit_conflicts: {}\ntable: {}\nblame: {}\nfind_results: {}\nsignature: {}\nrename: {}\nread_only: {}\nunshaped: {}\nignored_rows: {}\ncompletion_why: {}\npeek: {}\ngutter: {}\nissue_tab: {}\nscrub: {}\nscrub_strip: {}\nscrub_fade: {}\nscrub_filter: {}\nscrub_compare: {}\nblame_heat: {}\n{report}",
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
                        .ext.commands
                        .iter()
                        .map(|c| c.title.as_str())
                        .collect::<Vec<_>>()
                        .join("|"),
                    state.lsp.bulb.as_ref().map_or("none".to_string(), |b| format!(
                        "{} at {}",
                        b.actions.iter().filter(|a| a.disabled.is_none()).count(),
                        b.caret
                    )),
                    state
                        .palette
                        .as_ref()
                        .filter(|_| state.lsp.action_list.is_some())
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
                    state.git.repo.as_deref().unwrap_or(""),
                    state.git.note,
                    state.home.as_ref().map_or("none".to_string(), |h| format!(
                        "waiting={} repos={} commits={} notes={} calls={} hits={}",
                        h.waiting.join("|"),
                        h.repos
                            .iter()
                            .map(|r| format!("{}:{}:{}", r.label, r.status, r.changes))
                            .collect::<Vec<_>>()
                            .join(","),
                        h.repos.iter().map(|r| r.commits.len()).sum::<usize>(),
                        h.notes.len(),
                        h.calls.len(),
                        state.home_hits.len(),
                    )),
                    state.mcp.report(),
                    state.mcp.page_report(),
                    state.settings_page.report(),
                    state.review_page.report(),
                    state
                        .terminal
                        .tabs
                        .iter()
                        .map(|t| match t.review_files {
                            0 => t.state(),
                            n => format!("{} review={n}", t.state()),
                        })
                        .collect::<Vec<_>>()
                        .join("|"),
                    state.agents.report(
                        &state.terminal.tabs,
                        state.terminal.active,
                        state.terminal.shown_pair(),
                    ),
                    state.agents.inspector.report(
                        state
                            .agents
                            .selected_index()
                            .and_then(|i| state.agents.repos_of(&state.agents.tasks[i]))
                            .as_deref(),
                    ),
                    state.agents.report_teams(&state.terminal.tabs),
                    // With the folder each folder workspace's row shows,
                    // on a line of its own.
                    format_args!(
                        "{}\nagents_folders: {}",
                        state.agents.report_tree(),
                        state.agents.report_folders()
                    ),
                    state.agents.report_activity(),
                    state.agents.report_pull_requests(),
                    active_conflicts(&state).map_or("none".to_string(), |v| format!(
                        "{} side={} unmerged={} resolvable={} scroll={}",
                        v.conflicts.len(),
                        side_by_side(&state),
                        v.unmerged,
                        v.can_resolve(),
                        v.scroll
                    )),
                    state.git.conflict_count(),
                    crate::platform::table::dump(active_table(&state), state.cell_edit.is_some()),
                    state
                        .blame.shown
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
                        .lsp.signature
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
                    state.peek.as_ref().map_or("closed".to_string(), |p| format!(
                        "{} {}..{} drawn={}",
                        p.content.title,
                        p.change.new_start + 1,
                        p.change.new_end + 1,
                        p.hits.is_some()
                    )),
                    // The marks drawn for the active document, and whether
                    // Next Change has anything to act on.
                    state.gutter.docs.get(&buffer.id()).map_or("none".to_string(), |entry| {
                        let mut marks = entry
                            .marks
                            .iter()
                            .map(|m| {
                                let kind = match m.kind {
                                    crate::project::git::MarkKind::Added => 'A',
                                    crate::project::git::MarkKind::Modified => 'M',
                                    crate::project::git::MarkKind::Removed => 'R',
                                };
                                format!("{}{kind}", m.line + 1)
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        if !super::changes::active_has_changes(&state) {
                            marks.push_str(" inactive");
                        }
                        marks
                    }),
                    // The issue the active tab shows: number, repository,
                    // whether it takes typing, and its page.
                    super::issues::active_issue(&state).map_or("none".to_string(), |t| format!(
                        "#{} repo={}{} view_only={} url={}",
                        t.issue.number,
                        t.repo.0.as_deref().map(|h| format!("{h}:")).unwrap_or_default(),
                        t.repo.1.display(),
                        state.docs.active().is_view_only(),
                        t.issue.url
                    )),
                    super::scrub::dump(&state),
                    super::scrub::dump_strip(&state),
                    super::scrub::dump_fade(&state),
                    super::scrub::dump_filter(&state),
                    super::scrub::dump_compare(&state),
                    // The active document's strip: its lines, commits, the
                    // lines drawn, and the first line hidden as edited.
                    if !state.heat.on {
                        "off".to_string()
                    } else {
                        state.heat.docs.get(&buffer.id()).map_or("on none".to_string(), |doc| {
                            let strip = doc.strip();
                            let shown = strip
                                .lines
                                .iter()
                                .take(strip.hide_from)
                                .filter(|&&c| strip.alpha.get(c as usize).is_some_and(|&a| a > 0.0))
                                .count();
                            // The runs drawn last frame, by their tooltips;
                            // the first one's, its line break a slash.
                            let tips: Vec<&str> = state
                                .hotspots
                                .iter()
                                .filter_map(|h| h.tip.as_deref())
                                .filter(|t| strip.tips.iter().any(|tip| tip == t))
                                .collect();
                            // Commits known to be unsigned, drawn dashed.
                            let unsigned = strip.dashed.iter().filter(|&&d| d).count();
                            format!(
                                "on lines={} commits={} unsigned={unsigned} shown={shown} hidden_from={} spots={} tip={}",
                                strip.lines.len(),
                                doc.map.commits.len(),
                                match strip.hide_from {
                                    usize::MAX => "-".to_string(),
                                    line => (line + 1).to_string(),
                                },
                                tips.len(),
                                tips.first().map_or(String::new(), |t| t.replace('\n', " / ")),
                            )
                        })
                    },
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
                self.history_finish();
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

/// A script's modifier keys as AppKit's flags.
fn modifier_flags(mods: &crate::platform::selftest::Mods) -> NSEventModifierFlags {
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
    flags
}

/// A region by the name a script gives it: a page's or panel's own names
/// first, then the window frame's.
fn region_named(state: &mut State, name: &str) -> Option<Viewport> {
    match state.extensions.as_ref() {
        Some(page) if name.starts_with("extensions.") => page.named(name),
        _ if name.starts_with("mcp.") => state.mcp.named(name),
        _ if name.starts_with("settings.") => state.settings_page.named(name),
        _ if name.starts_with("review_page.") => state.review_page.named(name),
        _ if name.starts_with("agents.") => state.agents.named(name),
        _ if name.starts_with("peek.") => {
            let hits = state.peek.as_ref()?.hits?;
            match name {
                "peek.revert" => Some(hits.revert),
                "peek.stage" => Some(hits.stage),
                "peek.close" => Some(hits.close),
                _ => Some(hits.panel),
            }
        }
        // The mark beside a line, by its number: `change_bar.2`.
        _ if name.starts_with("change_bar.") => {
            let line = name["change_bar.".len()..]
                .parse::<usize>()
                .ok()?
                .checked_sub(1)?;
            let text = chrome_of(state).text;
            let buffer = state.docs.active();
            let m = state.renderer.atlas.metrics;
            let row = layout::screen_rows(buffer, text, m.line_height)
                .into_iter()
                .find(|r| r.line == line && r.first)?;
            Some(Viewport {
                x: text.x,
                y: row.y,
                width: m.advance,
                height: m.line_height,
            })
        }
        // A cell of the text, by line number and display column, scrolled
        // as drawn: `cell.2.4` is the fifth column of the second line.
        _ if name.starts_with("cell.") => {
            let (line, column) = name["cell.".len()..].split_once('.')?;
            let line = line.parse::<usize>().ok()?.checked_sub(1)?;
            let column = column.parse::<f32>().ok()?;
            let text = chrome_of(state).text;
            let buffer = state.docs.active();
            let m = state.renderer.atlas.metrics;
            let row = layout::screen_rows(buffer, text, m.line_height)
                .into_iter()
                .find(|r| r.line == line && r.first)?;
            let gutter = layout::gutter_width(buffer, &state.renderer.atlas);
            Some(Viewport {
                x: text.x + gutter + (column - buffer.scroll_x()) * m.advance,
                y: row.y,
                width: m.advance,
                height: m.line_height,
            })
        }
        // The blame heat strip beside a line, by its number: `heat.2`.
        _ if name.starts_with("heat.") => {
            let line = name["heat.".len()..]
                .parse::<usize>()
                .ok()?
                .checked_sub(1)?;
            let text = chrome_of(state).text;
            let buffer = state.docs.active();
            let m = state.renderer.atlas.metrics;
            let row = layout::screen_rows(buffer, text, m.line_height)
                .into_iter()
                .find(|r| r.line == line && r.first)?;
            let (dx, width) = layout::heat_strip(m.advance);
            Some(Viewport {
                x: m.snap(text.x + dx),
                y: row.y,
                width,
                height: m.line_height,
            })
        }
        // A row of the Source Control diff, by its line index: `git.diff.3`.
        _ if name.starts_with("git.diff.") => {
            let line = name["git.diff.".len()..].parse::<usize>().ok()?;
            let text = chrome_of(state).text;
            state.git.diff_line_rect(text, line)
        }
        _ if name.starts_with("inspector.") => state.agents.inspector.named(name),
        _ if name == "home.review" => state
            .home_hits
            .iter()
            .find(|h| h.action == layout::HomeAction::OpenReview)
            .map(|h| h.rect),
        _ if name == "home.update" => state
            .home_hits
            .iter()
            .find(|h| h.action == layout::HomeAction::RestartToUpdate)
            .map(|h| h.rect),
        _ => frame_of(state).named(name),
    }
}
