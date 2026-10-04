//! A frame: laying out and drawing everything the window shows, from the
//! chrome to the text, the overlays and the other panes.

use super::*;
use crate::project::icons;

impl EditorView {
    pub(super) fn resize(&self, size: NSSize) {
        let scale = self.window().map_or(2.0, |w| w.backingScaleFactor());
        let Some(mut state) = self.state_mut() else {
            self.ivars().deferred_size.set(Some(size));
            self.request_redraw();
            return;
        };
        state.viewport = Viewport::new(size.width as f32, size.height as f32);
        if (state.renderer.atlas.metrics.scale - scale as f32).abs() > 0.01 {
            // Moved to a display with a different scale: the glyphs were
            // rasterised for the old one and would be resampled, which is
            // the blur the whole atlas exists to avoid.
            let atlas = Atlas::build_with_ui(
                &state.font,
                state.font_size,
                crate::platform::settings::DEFAULT_FONT_SIZE,
                scale as f32,
            );
            state.renderer.replace_atlas(atlas);
        }
        state.layer.setContentsScale(scale);
        state.layer.setDrawableSize(objc2_core_foundation::CGSize {
            width: size.width * scale,
            height: size.height * scale,
        });
        drop(state);
        self.request_redraw();
        self.pump();
    }

    pub(super) fn render(&self) -> Option<FrameTiming> {
        // try_borrow_mut, not borrow_mut. AppKit re-enters this view at times
        // we do not choose: the display link fires on any run-loop iteration
        // including inside the nested loop of a modal alert or panel, and
        // resetCursorRects is called during tracking. If a borrow is already
        // live, the honest answer is to skip this frame and draw on the next
        // one, not to abort the process.
        let mut state = self.state_mut()?;
        layout::begin_frame();
        sync_conflicts(&mut state);
        let column = column_of(&state);
        let chrome = chrome_of(&state);
        let carets_on = !self.caret_blinks(&state) || caret_phase(state.caret_since.elapsed()).0;
        let State {
            docs,
            tree,
            git,
            git_open,
            git_focus,
            mcp,
            find,
            find_cache,
            tab_hits,
            tab_scroll,
            hovered_tab,
            drag,
            syntax,
            spans,
            finder,
            palette,
            commands: command_list,
            symbols: symbol_list,
            palette_scroll,
            palette_count,
            goto,
            rename,
            word_wrap,
            branch_list,
            repo_list,
            mcp_url_prompt,
            extensions,
            blame: Blame { shown: blame, .. },
            renderer,
            glyphs,
            theme,
            latency,
            layer,
            viewport,
            drew_once,
            worst,
            message,
            message_kind,
            marked,
            marked_caret,
            responses,
            sidebar_edit,
            panes,
            focused_pane,
            lsp:
                Lsp {
                    servers: lsp,
                    signature,
                    action_list,
                    bulb,
                    bulb_rect,
                    ..
                },
            completion,
            claude,
            terminal: terminal_panel,
            gutter: Gutter { docs: gutter, .. },
            home_hits,
            recent_projects,
            home: home_summary,
            status_detail_x,
            unshaped_on_screen,
            completion_chips,
            conflict_scans,
            conflict_side,
            ..
        } = &mut *state;
        *unshaped_on_screen = false;

        // First, before anything in the frame can go wrong: what the panic
        // hook would write is the text as of this frame.
        recovery::publish(docs.iter().chain(panes.iter().flat_map(|p| p.docs.iter())));

        let viewport = *viewport;
        if viewport.width <= 0.0 || viewport.height <= 0.0 {
            return None;
        }

        let query = find
            .as_ref()
            .map(|f| f.query.rope.to_string())
            .unwrap_or_default();

        // The one layout. `chrome_of` needs the whole state, so it is asked
        // before the state is taken apart field by field below.
        let ext_details_rect = details_rect(&chrome);
        let Chrome {
            toolbar: toolbar_rect,
            activity: activity_rect,
            sidebar: sidebar_rect,
            tabs: tab_rect,
            breadcrumbs: breadcrumb_rect,
            find: find_rect,
            response: response_rect,
            terminal: terminal_rect,
            text: editor_rect,
            status: status_rect,
            preview: _,
            panes: _,
            others: other_panes,
        } = chrome;

        // Scroll positions are only ever clamped when something scrolls, and
        // what they are clamped against changes without any scrolling: the
        // window grows, text is deleted, a folder collapses. Left alone that
        // is blank rows under the last line, which reads as having scrolled
        // past the end. Re-clamped here, every frame, against the rows this
        // frame really has.
        {
            let m = renderer.atlas.metrics;
            let gutter = layout::gutter_width(docs.active(), &renderer.atlas);
            let (rows, cols) = (
                editor_rect.rows(m.line_height),
                editor_rect.columns(m.advance, gutter),
            );
            apply_wrap(docs.active_mut(), *word_wrap, cols);
            docs.active_mut().clamp_scroll(rows, cols);
            if let Some(rect) = sidebar_rect {
                tree.scroll_by(0, layout::sidebar_rows(rect));
            }
        }
        let buffer = docs.active();
        let search_matches = find
            .as_ref()
            .and_then(|bar| find_matches(find_cache, bar, buffer));

        // Nothing open: the home screen, drawn by the same renderer. There is
        // no home "mode" to get stuck in. Typing lands in the untouched buffer
        // underneath, which stops being untouched, and the editor is back.
        let home = column == Column::Home;

        // A change picked in Source Control has a tab of its own, and takes
        // the editor column while that tab is active, the way a diff editor
        // does. The list highlights the change only then.
        let diffing = column == Column::GitDiff;
        git.showing_diff = diffing;
        let reviewing = claude
            .as_ref()
            .and_then(|c| c.reviews.get(&buffer.id()))
            .filter(|_| column == Column::Review);

        if let Some(page) = extensions.as_mut().filter(|_| column == Column::Extensions) {
            glyphs.clear();
            crate::platform::extensions::draw_details(
                page,
                &mut renderer.atlas,
                ext_details_rect,
                theme,
                glyphs,
            );
        } else if column == Column::Mcp {
            glyphs.clear();
            crate::platform::mcp_page::draw(
                mcp,
                &mut renderer.atlas,
                ext_details_rect,
                theme,
                glyphs,
            );
        } else if diffing {
            glyphs.clear();
            layout::push_rect(
                glyphs,
                &renderer.atlas,
                [editor_rect.x, editor_rect.y],
                [editor_rect.width, editor_rect.height],
                theme.tab_active,
            );
            git.draw_diff(&mut renderer.atlas, editor_rect, theme, glyphs);
        } else if column == Column::ConflictColumns
            && let Some(view) = conflict_scans
                .get_mut(&buffer.id())
                .and_then(|s| s.view.as_mut())
        {
            glyphs.clear();
            layout::push_rect(
                glyphs,
                &renderer.atlas,
                [editor_rect.x, editor_rect.y],
                [editor_rect.width, editor_rect.height],
                theme.tab_active,
            );
            crate::platform::conflicts::draw_side(
                view,
                &buffer.rope,
                &mut renderer.atlas,
                editor_rect,
                theme,
                glyphs,
            );
        } else if let Some(review) = reviewing {
            glyphs.clear();
            layout::push_rect(
                glyphs,
                &renderer.atlas,
                [editor_rect.x, editor_rect.y],
                [editor_rect.width, editor_rect.height],
                theme.tab_active,
            );
            crate::platform::claude::draw_review(
                review,
                &mut renderer.atlas,
                editor_rect,
                theme,
                glyphs,
            );
        } else if home {
            glyphs.clear();
            layout::push_rect(
                glyphs,
                &renderer.atlas,
                [editor_rect.x, editor_rect.y],
                [editor_rect.width, editor_rect.height],
                theme.tab_active,
            );
            layout::build_home(
                glyphs,
                &mut renderer.atlas,
                editor_rect,
                theme,
                tree.root(),
                recent_projects,
                home_summary.as_ref(),
                &terminal_panel
                    .tabs
                    .iter()
                    .map(|t| (t.name(), t.state()))
                    .collect::<Vec<_>>(),
                home_hits,
            );
        } else {
            // Highlight only what is on screen. Querying a whole file to draw
            // sixty lines of it would cost more than everything else in the
            // frame put together.
            spans.clear();
            // The rows on screen, worked out once for everything this frame
            // draws on them.
            let rows = layout::screen_rows(buffer, editor_rect, renderer.atlas.metrics.line_height);
            if syntax.has(buffer.id())
                && let Some(std::ops::Range {
                    start: first,
                    end: last,
                }) = layout::lines_of(&rows)
            {
                let total = buffer.rope.len_lines();
                let from = buffer.rope.line_to_byte(first);
                let to = if last < total {
                    buffer.rope.line_to_byte(last)
                } else {
                    buffer.rope.len_bytes()
                };
                // spans_with, not spans: predicates need the captured text, and
                // without it a `#match?` pattern matches everything.
                spans.extend(
                    syntax.spans_with(buffer.id(), from..to, |r| buffer.rope.slice_to_string(r)),
                );
            }

            let markdown = layout::Markdown::of(syntax.markdown(buffer.id()));
            let ranges: Vec<_> = search_matches
                .as_ref()
                .map(|found| found.iter().map(|m| m.range.clone()).collect())
                .unwrap_or_default();
            let stats = layout::build_full_search(
                buffer,
                &mut renderer.atlas,
                editor_rect,
                &rows,
                theme,
                &query,
                find.as_ref().map(|_| ranges.as_slice()),
                spans,
                &markdown,
                carets_on,
                glyphs,
            );
            *unshaped_on_screen = stats.unshaped > 0;
            if let Some(marks) = gutter.get(&buffer.id()) {
                layout::push_gutter_marks(
                    glyphs,
                    &renderer.atlas,
                    editor_rect,
                    &rows,
                    theme,
                    &marks.marks,
                );
            }
            if palette.is_none()
                && bulb.as_ref().is_some_and(|b| {
                    (b.buffer, b.caret) == (buffer.id(), buffer.cursor()) && b.shows()
                })
            {
                *bulb_rect = layout::push_bulb(
                    glyphs,
                    &mut renderer.atlas,
                    buffer,
                    editor_rect,
                    &rows,
                    theme,
                );
            } else {
                *bulb_rect = None;
            }
            // Conflicts: washes under the text, which was drawn first into
            // a cleared list, so they go in at the front; the buttons on
            // each opening marker go on top.
            if let Some(view) = conflict_scans
                .get(&buffer.id())
                .and_then(|s| s.view.as_ref())
            {
                let bands = crate::platform::conflicts::bands(
                    view,
                    &rows,
                    &renderer.atlas,
                    editor_rect,
                    theme,
                );
                glyphs.splice(0..0, bands);
                let hits = crate::platform::conflicts::inline_hits(
                    view,
                    buffer,
                    &rows,
                    &mut renderer.atlas,
                    editor_rect,
                );
                crate::platform::conflicts::draw_inline_buttons(
                    &hits,
                    &mut renderer.atlas,
                    theme,
                    glyphs,
                );
            }

            // A composition in progress, drawn at the caret it will land at.
            if let Some(text) = marked.as_deref()
                && !find.as_ref().is_some_and(|bar| bar.has_keys)
                && palette.is_none()
                && goto.is_none()
                && let Some(at) =
                    layout::caret_rect_on(buffer, &renderer.atlas, &markdown, editor_rect, &rows)
            {
                layout::push_marked_text(
                    glyphs,
                    &mut renderer.atlas,
                    at,
                    text,
                    *marked_caret,
                    theme,
                );
            }

            // What the language server thinks of the visible lines.
            if let (Some(path), Some(language)) = (&buffer.path, lsp_language(buffer))
                && let Some(server) = lsp.get(&crate::lsp::servers::server_key(language))
                && let Some(list) = server.diagnostics.get(path)
            {
                use crate::lsp::Severity;
                let marks: Vec<_> = list
                    .iter()
                    .map(|diagnostic| {
                        let start = crate::lsp::offset_of(&buffer.rope, diagnostic.start);
                        let end = crate::lsp::offset_of(&buffer.rope, diagnostic.end);
                        let color = match diagnostic.severity {
                            Severity::Error => theme.diff_removed,
                            Severity::Warning => theme.syn_constant,
                            Severity::Information | Severity::Hint => theme.status_text,
                        };
                        (start..end, color)
                    })
                    .collect();
                layout::push_underlines(
                    glyphs,
                    &renderer.atlas,
                    buffer,
                    editor_rect,
                    &rows,
                    &marks,
                );
            }

            // Completion: ghost text at the caret, chips under the line.
            completion_chips.clear();
            if let Some(popup) = completion
                .as_ref()
                .filter(|p| p.buffer == buffer.id() && !p.shown.is_empty())
                && let Some(caret) =
                    layout::caret_rect_on(buffer, &renderer.atlas, &markdown, editor_rect, &rows)
            {
                let cursor = buffer.cursor();
                let prefix = buffer
                    .rope
                    .slice_to_string(popup.anchor.min(cursor)..cursor);
                let line = buffer.rope.byte_to_line(cursor);
                let line_end = buffer.rope.line_range(line).end;
                let rest = buffer.rope.slice_to_string(cursor..line_end);
                let picked = &popup.shown[popup.selected.min(popup.shown.len() - 1)];
                // Only at the end of a line: drawn over text that follows the
                // caret, it would read as if it were there.
                let ghost = (rest.trim().is_empty() && picked.insert.starts_with(prefix.as_str()))
                    .then(|| &picked.insert[prefix.len()..]);
                let chips: Vec<layout::Chip> = popup
                    .shown
                    .iter()
                    .map(|c| layout::Chip {
                        label: c.label.as_str(),
                        icon: completion_icon(c, &popup.items),
                    })
                    .collect();
                let word_x =
                    caret.x - prefix.chars().count() as f32 * renderer.atlas.metrics.advance;
                *completion_chips = layout::build_completion_ribbon(
                    &chips,
                    popup.selected,
                    &picked.why,
                    ghost,
                    caret,
                    word_x,
                    editor_rect,
                    &mut renderer.atlas,
                    theme,
                    glyphs,
                );
            }

            // Signature help, above the caret's line.
            if let Some(tip) = signature.as_ref().filter(|t| t.buffer == buffer.id())
                && let Some(caret) =
                    layout::caret_rect_on(buffer, &renderer.atlas, &markdown, editor_rect, &rows)
            {
                layout::build_signature(
                    &tip.signature.label,
                    tip.signature.active.clone(),
                    caret,
                    editor_rect,
                    &mut renderer.atlas,
                    theme,
                    glyphs,
                );
            }
        }

        // After build_full, not before: that call clears the glyph buffer it
        // is handed, so anything drawn earlier in the frame is silently
        // erased. The sidebar and status line already append after it for
        // the same reason.
        for (index, rects) in &other_panes {
            let Some(store) = panes.get_mut(index - usize::from(*index > *focused_pane)) else {
                continue;
            };
            draw_other_pane(
                store, rects, tree, syntax, responses, gutter, renderer, theme, glyphs, *word_wrap,
            );
        }
        layout::build_toolbar(tree, &mut renderer.atlas, toolbar_rect, theme, glyphs);

        // Under the Extensions details the tabs are still laid out, so
        // their hit list stays true, but drawn into nothing.
        let mut hidden = Vec::new();
        let details = extensions.as_ref().is_some_and(|p| p.details) || column == Column::Mcp;
        layout::build_tab_bar(
            docs,
            *tab_scroll,
            *hovered_tab,
            &mut renderer.atlas,
            tab_rect,
            theme,
            if details { &mut hidden } else { glyphs },
            tab_hits,
        );

        if matches!(column, Column::Extensions | Column::Mcp) {
            // The Extensions and MCP pages cover this row; a document's
            // path here would label them as something they are not.
        } else if diffing {
            // The breadcrumb row says which change is on screen, so the
            // editor column is never an unlabelled wall of diff.
            layout::push_rect(
                glyphs,
                &renderer.atlas,
                [breadcrumb_rect.x, breadcrumb_rect.y],
                [breadcrumb_rect.width, breadcrumb_rect.height],
                theme.tab_active,
            );
            layout::push_ui_text(
                glyphs,
                &mut renderer.atlas,
                Viewport {
                    x: breadcrumb_rect.x + 12.0,
                    width: (breadcrumb_rect.width - 120.0).max(0.0),
                    ..breadcrumb_rect
                },
                &git.diff_title(),
                theme.text,
            );
            layout::push_ui_text_right(
                glyphs,
                &mut renderer.atlas,
                Viewport {
                    width: (breadcrumb_rect.width - 12.0).max(0.0),
                    ..breadcrumb_rect
                },
                &git.diff_summary(),
                theme.status_text,
            );
        } else {
            layout::build_breadcrumbs(
                buffer,
                tree,
                home,
                &mut renderer.atlas,
                breadcrumb_rect,
                theme,
                glyphs,
            );
            if let Some(strip) = response_rect
                && let Some(view) = responses.get(&buffer.id())
            {
                layout::build_response_strip(view, &mut renderer.atlas, strip, theme, glyphs);
            }
            if let Some(strip) = response_rect
                && !responses.contains_key(&buffer.id())
                && claude
                    .as_ref()
                    .and_then(|c| c.reviews.get(&buffer.id()))
                    .is_none()
                && let Some(view) = conflict_scans
                    .get(&buffer.id())
                    .and_then(|s| s.view.as_ref())
            {
                let file = buffer
                    .path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                crate::platform::conflicts::draw_strip(
                    view,
                    *conflict_side,
                    &file,
                    buffer.is_dirty(),
                    &mut renderer.atlas,
                    strip,
                    theme,
                    glyphs,
                );
            }
            if let Some(strip) = response_rect
                && let Some(review) = claude.as_ref().and_then(|c| c.reviews.get(&buffer.id()))
            {
                crate::platform::claude::draw_review_strip(
                    review,
                    tree.root(),
                    &mut renderer.atlas,
                    strip,
                    theme,
                    glyphs,
                );
            }
        }

        if let Some(rect) = find_rect
            && let Some(bar) = find.as_ref()
        {
            // From the selection's start: a found match is selected with
            // the caret at its end.
            let from = buffer.selection().map_or(buffer.cursor(), |r| r.start);
            let found = search_matches.map(|found| (found, from));
            draw_find_bar(glyphs, renderer, theme, rect, bar, found, tree);
        }

        layout::push_activity(
            glyphs,
            &mut renderer.atlas,
            activity_rect,
            theme,
            sidebar_rect.map(|_| {
                if mcp.open {
                    3
                } else if extensions.is_some() {
                    2
                } else if *git_open {
                    1
                } else {
                    0
                }
            }),
            git.snapshot.as_ref().map_or(0, |s| s.changes.len()),
        );
        if let Some(rect) = sidebar_rect
            && let Some(page) = extensions.as_mut()
        {
            layout::push_rect(
                glyphs,
                &renderer.atlas,
                [rect.x, rect.y],
                [rect.width, rect.height],
                theme.sidebar_background,
            );
            crate::platform::extensions::draw_list(page, &mut renderer.atlas, rect, theme, glyphs);
        } else if let Some(rect) = sidebar_rect
            && mcp.open
        {
            mcp.draw(&mut renderer.atlas, rect, theme, glyphs);
        } else if let Some(rect) = sidebar_rect {
            let edit_text = sidebar_edit.as_ref().map(|e| e.field.rope.to_string());
            let edit = sidebar_edit
                .as_ref()
                .zip(edit_text.as_deref())
                .map(|(e, text)| layout::SidebarEdit {
                    row: e.row,
                    depth: e.depth,
                    replaces: e.replaces(),
                    is_dir: matches!(e.kind, SidebarEditKind::NewFolder)
                        || matches!(&e.kind, SidebarEditKind::Rename(path) if path.is_dir()),
                    text,
                    cursor: e.field.cursor(),
                    selection: e.field.selection(),
                });
            tree.set_preview(ignore_preview(docs.active(), completion.as_ref(), tree));
            layout::build_sidebar_with_edit(
                tree,
                *git_open,
                edit,
                &mut renderer.atlas,
                rect,
                theme,
                glyphs,
            );
            if *git_open {
                git.draw_sidebar(&mut renderer.atlas, rect, *git_focus, theme, glyphs);
            } else if let Some(Drag::Tree(drag)) = drag
                && drag.active
                && drag.valid
            {
                // Where it would land. A drag with no target drawn is a drag
                // you have to guess at.
                let band = match drag.over {
                    Some(index) => Viewport {
                        x: rect.x + 4.0,
                        width: (rect.width - 8.0).max(0.0),
                        ..layout::sidebar_row_rect(rect, index.saturating_sub(tree.scroll))
                    },
                    // The project root: the whole column below the tree.
                    None => Viewport {
                        x: rect.x + 4.0,
                        y: rect.y + layout::SIDEBAR_HEADER_HEIGHT,
                        width: (rect.width - 8.0).max(0.0),
                        height: (rect.height - layout::SIDEBAR_HEADER_HEIGHT).max(0.0),
                    },
                };
                layout::push_focus_ring(glyphs, band, 5.0, 1.5, theme);
            }
        }

        if let Some(rect) = terminal_rect {
            crate::platform::terminal::draw(
                terminal_panel,
                &mut renderer.atlas,
                rect,
                theme,
                glyphs,
            );
        }

        // Status line along the bottom, spanning the full width.
        let (y, status_height) = (status_rect.y, status_rect.height);
        layout::push_rect(
            glyphs,
            &renderer.atlas,
            [status_rect.x, y],
            [status_rect.width, status_height],
            theme.status_background,
        );
        // A transient note (save result, open error) takes over the status
        // line briefly, then yields back to the steady-state readout.
        let note = match message {
            Some((text, at))
                if at.elapsed()
                    < layout::message_lasts_for(layout::feedback_for(
                        text,
                        message_kind.as_ref(),
                    )) =>
            {
                Some(text.clone())
            }
            _ => {
                *message = None;
                None
            }
        };
        let diagnostics =
            buffer
                .path
                .as_ref()
                .zip(lsp_language(buffer))
                .and_then(|(path, language)| {
                    lsp.get(&crate::lsp::servers::server_key(language))
                        .and_then(|s| s.diagnostics.get(path))
                });
        let (line, _) = buffer.cursor_position();
        let (status, detail) = status_texts(&StatusParts {
            buffer,
            note,
            diagnostics: diagnostics.map(Vec::as_slice),
            blame: blame
                .as_ref()
                .filter(|(id, l, _)| *id == buffer.id() && *l == line)
                .map(|(_, _, text)| text.as_str()),
            unshaped: *unshaped_on_screen,
            branch: git.branch_status(),
            claude: claude.as_ref().is_some_and(|c| c.is_connected()),
            lsp: lsp_language(buffer)
                .and_then(|language| lsp.get(&crate::lsp::servers::server_key(language)))
                .and_then(|server| match &server.phase {
                    crate::lsp::client::Phase::Ready => Some(server.name.clone()),
                    crate::lsp::client::Phase::Failed(_) => {
                        Some(format!("{} stopped", server.name))
                    }
                    crate::lsp::client::Phase::Starting => None,
                }),
        });
        let detail = if std::env::var_os("CRC_SHOW_LATENCY").is_some() {
            format!("{}  {:?}", latency.summary(), worst)
        } else {
            detail
        };
        let right = status_detail_rect(status_rect);
        // Work that outlasts a message: Git talking to a remote, a
        // language server starting. A spinner and what it is, until done.
        let shown_note = message.as_ref().map(|(text, _)| text.as_str());
        let working = if shown_note.is_some() {
            None
        } else if git.busy() && git.note.ends_with('\u{2026}') && git.note != "Working\u{2026}" {
            Some(git.note.clone())
        } else {
            lsp_language(buffer)
                .and_then(|language| lsp.get(&crate::lsp::servers::server_key(language)))
                .filter(|server| !server.is_ready())
                .map(|server| format!("{} starting\u{2026}", server.name))
        };
        let mut left = Viewport {
            x: status_rect.x + layout::UI_INSET,
            width: (right.x - status_rect.x - layout::UI_INSET * 2.0).max(0.0),
            ..status_rect
        };
        let lead = |left: &mut Viewport, w: f32| {
            left.x += w;
            left.width = (left.width - w).max(0.0);
        };
        let (text, colour) = if let Some(work) = &working {
            let w = layout::push_spinner(glyphs, left.x, left, theme.accent);
            lead(&mut left, w + 8.0);
            (work.as_str(), theme.status_text)
        } else {
            let feedback = shown_note.map_or(layout::Feedback::Info, |text| {
                layout::feedback_for(text, message_kind.as_ref())
            });
            let icon = match feedback {
                layout::Feedback::Failure => Some((icons::ERROR, theme.diff_removed)),
                layout::Feedback::Success => Some((icons::PASS, theme.diff_added)),
                layout::Feedback::Info => None,
            };
            if let Some((glyph, tint)) = icon {
                let w = layout::icon_width(&mut renderer.atlas, glyph);
                layout::push_icon_centered(
                    glyphs,
                    &mut renderer.atlas,
                    Viewport { width: w, ..left },
                    glyph,
                    tint,
                );
                lead(&mut left, w + 6.0);
            }
            (
                status.as_str(),
                if feedback == layout::Feedback::Failure {
                    theme.diff_removed
                } else {
                    theme.status_text
                },
            )
        };
        layout::push_ui_text(glyphs, &mut renderer.atlas, left, text, colour);
        if let Some(note) = shown_note {
            // A long message is cut by the window; the whole of it is a
            // pointer's rest away.
            layout::hotspot(left, layout::Cursor::Arrow, Some(note));
        }
        // Right-aligned, so the readout ends at the window's edge however
        // long the branch name is, instead of leaving a ragged gap.
        let detail_w = layout::ui_text_width(&mut renderer.atlas, &detail).min(right.width);
        *status_detail_x = Some(right.x + right.width - detail_w);
        layout::push_ui_text_right(
            glyphs,
            &mut renderer.atlas,
            right,
            &detail,
            theme.status_text,
        );
        // Go-to-line takes over the status line: it is a line number, and
        // that is where line numbers already live.
        let prompt = goto
            .as_ref()
            .map(|field| ("Go to line: ", field))
            .or_else(|| rename.as_ref().map(|r| ("Rename to: ", &r.field)));
        if let Some((label, field)) = prompt {
            let row = Viewport {
                x: 0.0,
                width: viewport.width,
                ..status_rect
            };
            draw_prompt(glyphs, &mut renderer.atlas, theme, row, label, field);
        }

        // The palette floats over everything, so it is drawn last.
        if let Some((query, selected)) = palette {
            let text = query.rope.to_string();
            let sources = PaletteSources {
                finder,
                commands: command_list,
                symbols: symbol_list,
                root: tree.root(),
                branches: branch_list.as_ref(),
                actions: action_list.as_ref(),
                repos: repo_list.as_deref(),
                mcp_url: *mcp_url_prompt,
            };
            let mode = PaletteMode::of(&sources);
            let intent = sources.branches.map(|pick| &pick.intent);
            let rows: Vec<layout::PaletteRow> = palette_rows(&sources, &text)
                .into_iter()
                .map(|(row, _)| row)
                .collect();
            *palette_count = rows.len();
            let rect = layout::palette_rect(viewport, rows.len());
            layout::push_rect(
                glyphs,
                &renderer.atlas,
                [viewport.x, viewport.y],
                [viewport.width, viewport.height],
                theme.scrim,
            );
            layout::build_palette(
                layout::PaletteView {
                    rows: &rows,
                    heading: palette_heading(&text, mode, intent).0,
                    empty: palette_heading(&text, mode, intent).1,
                    placeholder: match mode {
                        PaletteMode::Branch => "Branch name",
                        PaletteMode::Action => "Filter actions",
                        PaletteMode::Repo => "Repository name",
                        PaletteMode::McpUrl => "https://host/mcp",
                        PaletteMode::Open => {
                            "Find a file  ·  > commands  ·  @ symbols  ·  # in project"
                        }
                    },
                    action: if mode == PaletteMode::Branch {
                        match intent {
                            Some(BranchIntent::Rename | BranchIntent::RenameTo(_)) => "Rename",
                            Some(BranchIntent::Delete) => "Delete",
                            _ => "Switch",
                        }
                    } else if mode == PaletteMode::McpUrl {
                        "Add"
                    } else if mode == PaletteMode::Action || commands::query(&text).is_some() {
                        "Run"
                    } else {
                        "Open"
                    },
                    query: &text,
                    selected: *selected,
                    scroll: *palette_scroll,
                    cursor: query.cursor(),
                    selection: query.selection(),
                },
                &mut renderer.atlas,
                rect,
                theme,
                glyphs,
            );
        }

        let background = theme.background;
        let timing = renderer.draw(layer, glyphs, (viewport.width, viewport.height), background);
        if timing.is_some() {
            *drew_once = true;
        }
        timing
    }
}

/// The find bar: its two fields, the buttons and options beside them and,
/// for a project search, the results under them. `found` is the active
/// document's matches and the offset the "n of m" count starts from.
fn draw_find_bar(
    glyphs: &mut Vec<GlyphInstance>,
    renderer: &mut Renderer,
    theme: &Theme,
    rect: Viewport,
    bar: &FindBar,
    found: Option<(&[search::Match], usize)>,
    tree: &Tree,
) {
    layout::push_rect(
        glyphs,
        &renderer.atlas,
        [rect.x, rect.y],
        [rect.width, rect.height],
        theme.find_background,
    );
    let g = layout::FindGeometry::new(rect);

    // A field that looks like a field. There was no box at all: a
    // "Find" label sat at the far left and the text began eleven
    // monospace columns later, with nothing to say where you could
    // type or how far the field reached.
    let field = |glyphs: &mut Vec<GlyphInstance>,
                 atlas: &mut Atlas,
                 box_rect: Viewport,
                 placeholder: &str,
                 buffer: &Buffer,
                 focused: bool,
                 trailing: Option<&str>| {
        if focused {
            layout::push_focus_ring(glyphs, box_rect, 6.0, 1.5, theme);
        }
        layout::push_rounded_rect(glyphs, box_rect, layout::UI_RADIUS, theme.tab_active);
        layout::hotspot(box_rect, layout::Cursor::Text, None);
        let text = buffer.rope.to_string();
        let inner = Viewport {
            x: box_rect.x + FIND_FIELD_PAD,
            width: (box_rect.width - FIND_FIELD_PAD * 2.0).max(0.0),
            ..box_rect
        };
        // The match count first, so the text knows how much room is
        // left and never runs underneath it.
        let mut room = inner.width;
        if let Some(trailing) = trailing {
            layout::push_ui_text_right(glyphs, atlas, inner, trailing, theme.status_text);
            room = (room - layout::ui_text_width(atlas, trailing) - 12.0).max(0.0);
        }
        layout::push_ui_field(
            glyphs,
            atlas,
            Viewport {
                width: room,
                ..inner
            },
            (5.0, box_rect.height - 10.0),
            &layout::UiField {
                text: &text,
                cursor: buffer.cursor(),
                selection: buffer.selection(),
                placeholder,
                focused,
            },
            theme,
        );
    };

    // "3 of 17" in the field it belongs to, which the bar never
    // reported at all: there was no way to tell a search that found
    // nothing from one that found everything.
    let count = if bar.project {
        (!bar.results.is_empty()).then(|| format!("{} found", bar.results.len()))
    } else {
        found.map(|(found, from)| {
            if found.is_empty() {
                "No matches".to_string()
            } else {
                let at = found
                    .iter()
                    .position(|m| m.range.start >= from)
                    .unwrap_or(0);
                format!("{} of {}", at + 1, found.len())
            }
        })
    };
    field(
        glyphs,
        &mut renderer.atlas,
        g.find_field,
        if bar.project {
            "Search the project"
        } else {
            "Find"
        },
        &bar.query,
        bar.has_keys && !bar.replacing,
        count.as_deref(),
    );
    field(
        glyphs,
        &mut renderer.atlas,
        g.replace_field,
        if bar.project {
            "Replace in every file listed"
        } else {
            "Replace with"
        },
        &bar.replacement,
        bar.has_keys && bar.replacing,
        None,
    );

    // The shared button: the same height, corners and hover as every
    // other control, with arrows and a cross from the icon font rather
    // than guillemets and a multiplication sign.
    let has_matches = if bar.project {
        !bar.results.is_empty()
    } else {
        found.is_some_and(|(f, _)| !f.is_empty())
    };
    let atlas = &mut renderer.atlas;
    Button::new(g.previous)
        .icon(icons::ARROW_UP)
        .tone(Tone::Ghost)
        .enabled(has_matches)
        .tip("Previous Match  \u{21e7}\u{2318}G")
        .draw(glyphs, atlas, theme);
    Button::new(g.next)
        .icon(icons::ARROW_DOWN)
        .tone(Tone::Ghost)
        .enabled(has_matches)
        .tip("Next Match  \u{2318}G")
        .draw(glyphs, atlas, theme);
    Button::new(g.close)
        .icon(icons::CLOSE)
        .tone(Tone::Ghost)
        .tip("Close  \u{238b}")
        .draw(glyphs, atlas, theme);
    if !bar.project {
        Button::new(g.replace_one)
            .label("Replace")
            .enabled(has_matches)
            .draw(glyphs, atlas, theme);
        Button::new(g.replace_all)
            .label("All")
            .enabled(has_matches)
            .tip("Replace All")
            .draw(glyphs, atlas, theme);
    } else {
        Button::new(g.replace_one)
            .label("Search")
            .draw(glyphs, atlas, theme);
        // Rows open with a click or Return; this button replaces
        // in every file the results list.
        Button::new(g.replace_all)
            .label("Replace All")
            .tone(Tone::Danger)
            .enabled(has_matches)
            .draw(glyphs, atlas, theme);
    }

    // Toggles that show their state: filled and accented when on,
    // quiet when off.
    for (slot, (label, on, tip)) in [
        ("Aa", bar.options.case_sensitive, "Match Case"),
        ("Word", bar.options.whole_word, "Match Whole Word"),
        (".*", bar.options.regex, "Use Regular Expression"),
        (
            "Project",
            bar.project,
            "Search the Whole Project  \u{21e7}\u{2318}F",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        Button::new(g.options[slot])
            .label(label)
            .tone(Tone::Ghost)
            .on(on)
            .tip(tip)
            .draw(glyphs, atlas, theme);
    }
    if bar.project {
        for (row, hit) in bar
            .results
            .iter()
            .skip(bar.result_scroll)
            .take(FIND_RESULT_ROWS)
            .enumerate()
        {
            let y = g.results.y + layout::FIND_ROW_HEIGHT * row as f32;
            let row_rect = Viewport {
                x: g.results.x + 8.0,
                y,
                width: (g.results.width - 16.0).max(0.0),
                height: layout::FIND_ROW_HEIGHT,
            };
            if bar.selected == bar.result_scroll + row {
                layout::push_rounded_rect(
                    glyphs,
                    Viewport {
                        y: y + 1.0,
                        height: layout::FIND_ROW_HEIGHT - 2.0,
                        ..row_rect
                    },
                    5.0,
                    theme.palette_selected,
                );
            }
            let path = tree
                .root()
                .and_then(|root| hit.path.strip_prefix(root).ok())
                .unwrap_or(&hit.path);
            // Where it is, then what it says, told apart by colour
            // rather than run together into one monospace string.
            let where_it_is = format!("{}:{}", path.display(), hit.line + 1);
            let width = layout::ui_text_width(&mut renderer.atlas, &where_it_is);
            layout::push_ui_text(
                glyphs,
                &mut renderer.atlas,
                Viewport {
                    x: row_rect.x + 8.0,
                    ..row_rect
                },
                &where_it_is,
                theme.accent,
            );
            layout::push_ui_text(
                glyphs,
                &mut renderer.atlas,
                Viewport {
                    x: row_rect.x + 20.0 + width,
                    width: (row_rect.width - 28.0 - width).max(0.0),
                    ..row_rect
                },
                hit.snippet.trim(),
                theme.sidebar_text,
            );
        }
        if bar.searching {
            layout::push_ui_text(
                glyphs,
                &mut renderer.atlas,
                Viewport {
                    x: g.results.x + 16.0,
                    ..g.results
                },
                "Searching project\u{2026}",
                theme.status_text,
            );
        }
    }
}

/// A one-line prompt over the status row: its label, then the field with
/// its own caret and selection.
fn draw_prompt(
    glyphs: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    theme: &Theme,
    row: Viewport,
    label: &str,
    field: &Buffer,
) {
    let (y, status_height) = (row.y, row.height);
    let advance = atlas.metrics.advance;
    let text = field.rope.to_string();
    layout::push_rect(
        glyphs,
        atlas,
        [row.x, y],
        [row.width, status_height],
        theme.find_background,
    );
    layout::push_text(glyphs, atlas, advance, y, label, theme.gutter_text);
    let x = advance * (label.len() as f32 + 1.0);
    // The field's own caret and selection: Left, Right and Shift
    // move them. By the cells the text takes, as push_text lays it
    // out: a wide character is two.
    let column = |at: usize| {
        let cells: usize = text[..at.min(text.len())]
            .chars()
            .map(crate::text::columns::display_width)
            .sum();
        cells as f32 * advance
    };
    if let Some(range) = field.selection() {
        layout::push_rect(
            glyphs,
            atlas,
            [x + column(range.start), y],
            [column(range.end) - column(range.start), status_height],
            theme.selection,
        );
    }
    layout::push_text(glyphs, atlas, x, y, &text, theme.text);
    layout::push_rect(
        glyphs,
        atlas,
        [x + column(field.cursor()), y],
        [(advance * 0.15).max(1.0), status_height],
        theme.cursor,
    );
}

/// What the status line says, gathered by `render`.
struct StatusParts<'a> {
    buffer: &'a Buffer,
    /// A transient note: a save result, an open error.
    note: Option<String>,
    /// The document's diagnostics, from its language server.
    diagnostics: Option<&'a [crate::lsp::Diagnostic]>,
    /// Who last changed the caret's line.
    blame: Option<&'a str>,
    /// The frame drew a line too long to shape.
    unshaped: bool,
    branch: String,
    /// Claude Code is connected.
    claude: bool,
    /// The document's language server once it answers: its name, or that
    /// it stopped. Nothing while it starts; the spinner says that.
    lsp: Option<String>,
}

/// The status line's left and right text. The left is a note, else the
/// diagnostic under the caret, else the file's name and save state; the
/// right is Claude, the branch, the diagnostic counts and the position.
fn status_texts(parts: &StatusParts<'_>) -> (String, String) {
    let buffer = parts.buffer;
    let (line, column) = buffer.cursor_position();
    let (here, counts) = match parts.diagnostics {
        Some(list) if !list.is_empty() => {
            let here = list
                .iter()
                .find(|d| (d.start.line as usize..=d.end.line as usize).contains(&line))
                .map(|d| d.message.lines().next().unwrap_or("").to_owned());
            let errors = list
                .iter()
                .filter(|d| d.severity == crate::lsp::Severity::Error)
                .count();
            let warnings = list.len() - errors;
            (here, format!("✕ {errors}  ⚠ {warnings}     "))
        }
        _ => (None, String::new()),
    };
    let name = buffer.display_name();
    let left = parts.note.clone().or(here).unwrap_or_else(|| {
        if buffer.is_view_only() {
            return name;
        }
        let saved = if buffer.is_read_only() {
            format!(
                "Read-only: over {}",
                crate::text::buffer::human_size(crate::text::buffer::read_only_limit())
            )
        } else if buffer.is_dirty() {
            "Unsaved changes".to_string()
        } else {
            "All changes saved".to_string()
        };
        // Known only from drawing: finding such a line up front would mean
        // scanning the whole file.
        let unshaped = if parts.unshaped {
            "   Long lines drawn without shaping"
        } else {
            ""
        };
        let blamed = parts
            .blame
            .filter(|text| !text.is_empty())
            .map(|text| format!("   ·   {text}"))
            .unwrap_or_default();
        format!("{name}   {saved}{unshaped}{blamed}")
    });
    let position = if buffer.is_view_only() {
        String::new()
    } else {
        format!(
            "Ln {}, Col {}     {}     {}",
            line + 1,
            column + 1,
            buffer.disk_format().label(),
            buffer.disk_format().line_ending_label()
        )
    };
    let right = format!(
        "{}{}{}{counts}{position}",
        if parts.claude { "✻ Claude     " } else { "" },
        parts
            .lsp
            .as_deref()
            .map(|lsp| format!("● {lsp}     "))
            .unwrap_or_default(),
        if parts.branch.is_empty() {
            String::new()
        } else {
            format!("{}     ", parts.branch)
        },
    );
    (left, right)
}

#[cfg(test)]
mod status_tests {
    use super::{StatusParts, status_texts};
    use crate::lsp::{Diagnostic, Position, Severity};
    use crate::text::buffer::Buffer;

    fn parts(buffer: &Buffer) -> StatusParts<'_> {
        StatusParts {
            buffer,
            note: None,
            diagnostics: None,
            blame: None,
            unshaped: false,
            branch: String::new(),
            claude: false,
            lsp: None,
        }
    }

    #[test]
    fn the_language_server_shows_at_the_right_once_it_answers() {
        let buffer = Buffer::new();
        let mut p = parts(&buffer);
        assert!(!status_texts(&p).1.contains('\u{25cf}'));
        p.lsp = Some("rust-analyzer".into());
        assert!(
            status_texts(&p)
                .1
                .starts_with("\u{25cf} rust-analyzer     ")
        );
        p.lsp = Some("rust-analyzer stopped".into());
        assert!(status_texts(&p).1.contains("rust-analyzer stopped"));
    }

    fn diagnostic(line: u32, severity: Severity, message: &str) -> Diagnostic {
        Diagnostic {
            start: Position { line, character: 0 },
            end: Position { line, character: 1 },
            severity,
            message: message.to_owned(),
            source: None,
            raw: String::new(),
        }
    }

    #[test]
    fn the_left_side_is_the_note_then_the_diagnostic_here_then_the_file() {
        let buffer = Buffer::new();
        let (left, right) = status_texts(&parts(&buffer));
        assert_eq!(left, "Untitled   All changes saved");
        assert!(right.starts_with("Ln 1, Col 1"), "{right}");

        let list = [
            diagnostic(0, Severity::Error, "expected `;`\nmore"),
            diagnostic(4, Severity::Warning, "unused"),
        ];
        let with_list = StatusParts {
            diagnostics: Some(&list),
            blame: Some("Ada, 2 days ago"),
            ..parts(&buffer)
        };
        let (left, right) = status_texts(&with_list);
        assert_eq!(
            left, "expected `;`",
            "the first line of the one under the caret"
        );
        assert!(right.starts_with("✕ 1  ⚠ 1"), "{right}");

        let noted = StatusParts {
            note: Some("Saved".into()),
            ..with_list
        };
        assert_eq!(status_texts(&noted).0, "Saved");
    }

    #[test]
    fn blame_long_lines_claude_and_the_branch_are_added_when_known() {
        let buffer = Buffer::new();
        let all = StatusParts {
            blame: Some("Ada, 2 days ago"),
            unshaped: true,
            branch: "main".into(),
            claude: true,
            ..parts(&buffer)
        };
        let (left, right) = status_texts(&all);
        assert_eq!(
            left,
            "Untitled   All changes saved   Long lines drawn without shaping   ·   Ada, 2 days ago"
        );
        assert!(right.starts_with("✻ Claude     main     Ln 1"), "{right}");
        let empty_blame = StatusParts {
            blame: Some(""),
            ..parts(&buffer)
        };
        assert_eq!(status_texts(&empty_blame).0, "Untitled   All changes saved");
    }
}
