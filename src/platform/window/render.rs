//! A frame: laying out and drawing everything the window shows, from the
//! chrome to the text, the overlays and the other panes.

use super::*;
use crate::project::icons;

/// The palette, to the overlay fade.
const PALETTE_OVERLAY: u64 = 1;

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
        let key = self.key_window();
        let mut state = self.state_mut()?;
        layout::begin_frame();
        layout::next_frame();
        super::terminal::report_focus(&mut state, key);
        sync_conflicts(&mut state);
        sync_tables(&mut state);
        super::agents::forget_emptied(&mut state);
        let column = column_of(&state);
        let chrome = chrome_of(&state);
        let terminal_find_rect = super::terminal::find_rect(&state, &chrome);
        let terminal_filter_rect = super::terminal::filter_rect(&state, &chrome);
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
            search,
            search_open,
            search_focus,
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
            deleted_list,
            mcp_url_prompt,
            task_prompt,
            commit_prompt,
            term_search,
            extensions,
            settings_page,
            update_ready,
            replaced,
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
            said,
            status_action,
            marked,
            marked_caret,
            responses,
            sidebar_edit,
            agents_edit,
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
            peek,
            claude,
            terminal: terminal_panel,
            agents,
            gutter: Gutter { docs: gutter, .. },
            heat,
            history,
            home_hits,
            home_view,
            recent_projects,
            home: home_summary,
            status_detail_x,
            unshaped_on_screen,
            completion_chips,
            conflict_scans,
            conflict_side,
            tables,
            cell_edit,
            query: query_field,
            query_job,
            queries,
            issue_tabs,
            scrub,
            watch,
            ..
        } = &mut *state;
        *unshaped_on_screen = false;
        // Drawn again below when the document is; until then, nothing to click.
        if let Some(open) = peek.as_mut() {
            open.hits = None;
        }

        // First, before anything in the frame can go wrong: what the panic
        // hook would write is the text as of this frame.
        recovery::publish(docs.iter().chain(panes.iter().flat_map(|p| p.docs.iter())));
        // Beside it, and as cheap: which versions local history keeps.
        history.observe(
            docs.iter().chain(panes.iter().flat_map(|p| p.docs.iter())),
            Instant::now(),
        );

        crate::platform::jank::lap("draw.publish");
        let viewport = *viewport;
        if viewport.width <= 0.0 || viewport.height <= 0.0 {
            return None;
        }
        // The atlas's frame, once, whatever the window shows. Left to the
        // editor's build, Agents mode and the full-column pages never
        // advanced it, and every cached label looked used this frame.
        renderer.atlas.begin_frame();

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
            inspector: inspector_rect,
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
        if !agents.on {
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

        if agents.on {
            // The terminal covers the column; drawn with the sidebar below.
            glyphs.clear();
        } else if column == Column::Settings {
            glyphs.clear();
            crate::platform::settings_page::draw(
                settings_page,
                &mut renderer.atlas,
                ext_details_rect,
                theme,
                glyphs,
            );
        } else if let Some(page) = extensions.as_mut().filter(|_| column == Column::Extensions) {
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
        } else if column == Column::Table
            && let Some(view) = tables.get_mut(&buffer.id())
        {
            glyphs.clear();
            crate::platform::table::draw(
                view,
                &buffer.rope,
                cell_edit.as_ref(),
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
            layout::build_home_view(
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
                update_ready.as_ref().map(|r| r.version.as_str()),
                *replaced,
                home_view,
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
            // build_full_search less its begin_frame: the frame began above.
            glyphs.clear();
            let stats = layout::build_text_appending(
                buffer,
                &mut renderer.atlas,
                editor_rect,
                &rows,
                theme,
                &query,
                find.as_ref().map(|_| ranges.as_slice()),
                spans,
                &markdown,
                true,
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
            if heat.on
                && let Some(doc) = heat.follow(buffer.id(), &buffer.rope)
            {
                layout::push_blame_heat(
                    glyphs,
                    &renderer.atlas,
                    editor_rect,
                    &rows,
                    theme,
                    &doc.strip(),
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
            // A step through File History: what it changed, fading out.
            super::scrub::draw_fade(
                scrub,
                buffer,
                &mut renderer.atlas,
                editor_rect,
                &rows,
                theme,
                glyphs,
            );

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

            // The peek at a Git change, over everything else on the text.
            super::changes::check_peek(peek, buffer);
            if let Some(open) = peek.as_mut()
                && palette.is_none()
            {
                open.hits = layout::build_change_peek(
                    &open.content,
                    &open.change,
                    buffer,
                    editor_rect,
                    &rows,
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
                store, rects, tree, syntax, responses, gutter, heat, renderer, theme, glyphs,
                *word_wrap,
            );
        }
        layout::build_toolbar(
            tree,
            agents.on,
            agents.on.then_some(!agents.inspector.hidden),
            &mut renderer.atlas,
            toolbar_rect,
            theme,
            glyphs,
        );

        // Under the Extensions details the tabs are still laid out, so
        // their hit list stays true, but drawn into nothing.
        let mut hidden = Vec::new();
        let details =
            agents.on || matches!(column, Column::Extensions | Column::Mcp | Column::Settings);
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

        if details {
            // The Extensions, MCP and Settings pages cover this row; a document's
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
            // A table's strip, when no conflict, review or history has it.
            if let Some(strip) = response_rect
                && !responses.contains_key(&buffer.id())
                && claude
                    .as_ref()
                    .and_then(|c| c.reviews.get(&buffer.id()))
                    .is_none()
                && conflict_scans
                    .get(&buffer.id())
                    .and_then(|s| s.view.as_ref())
                    .is_none()
                && scrub.as_ref().is_none_or(|s| !s.strip_over(buffer.id()))
                && let Some(view) = tables.get(&buffer.id())
            {
                let query =
                    super::query::query_strip(buffer.id(), tables, query_field, query_job, queries);
                crate::platform::table::draw_strip(
                    view,
                    &buffer.display_name(),
                    &query,
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
            if let Some(strip) = response_rect
                && let Some(tab) = issue_tabs.get(&buffer.id())
            {
                super::issues::draw_strip(tab, &mut renderer.atlas, strip, theme, glyphs);
            }
            if let Some(strip) = response_rect
                && let Some(history) = scrub.as_ref().filter(|s| s.strip_over(buffer.id()))
            {
                super::scrub::draw_strip(history, &mut renderer.atlas, strip, theme, glyphs);
            }
        }

        if let Some(rect) = find_rect
            && let Some(bar) = find.as_ref()
        {
            // From the selection's start: a found match is selected with
            // the caret at its end.
            let from = buffer.selection().map_or(buffer.cursor(), |r| r.start);
            let count = search_matches.map(|found| {
                let at = found.iter().position(|m| m.range.start >= from);
                (at.unwrap_or(0), found.len())
            });
            draw_find_bar(glyphs, renderer, theme, rect, bar, count);
        }

        if activity_rect.width > 0.0 {
            layout::push_activity(
                glyphs,
                &mut renderer.atlas,
                activity_rect,
                theme,
                sidebar_rect.filter(|_| !agents.on).map(|_| {
                    if mcp.open {
                        4
                    } else if extensions.is_some() {
                        3
                    } else if *git_open {
                        2
                    } else if *search_open {
                        1
                    } else {
                        0
                    }
                }),
                git.snapshot.as_ref().map_or(0, |s| s.changes.len()),
            );
        }
        terminal_panel.shown = agents.on.then(|| {
            let tasks = &agents.tasks;
            let selected = agents.selected_index();
            (0..terminal_panel.tabs.len())
                .filter(|i| {
                    tasks.is_empty()
                        || crate::platform::agents::task_of(tasks, &terminal_panel.tabs[*i])
                            == selected
                })
                .collect()
        });
        if let Some(rect) = sidebar_rect
            && agents.on
        {
            let active = terminal_panel.active_tab().map(|_| terminal_panel.active);
            agents.inline = agents_edit.as_ref().map(|(id, field)| {
                let text = field.rope.to_string();
                let caret = text
                    .get(..field.cursor().min(text.len()))
                    .map_or(0, |t| t.chars().count());
                (id.clone(), text, caret)
            });
            agents.draw(
                &terminal_panel.tabs,
                active,
                &mut renderer.atlas,
                rect,
                theme,
                glyphs,
            );
            if let Some(rect) = inspector_rect {
                let selected = agents.selected_index().map(|i| agents.tasks[i].clone());
                // A pull request is a worktree branch's, once its row is read;
                // a host's when its origin is on GitHub.
                let pull_request = selected
                    .as_ref()
                    .filter(|w| !w.main && !w.folder && w.branch.is_some())
                    .filter(|w| w.host.is_none() || w.github.is_some())
                    .and_then(|w| agents.info.get(&w.id()))
                    .map(|info| info.pull_request.clone());
                let github = agents.github.unwrap_or(false);
                let repos = selected.as_ref().and_then(|w| agents.repos_of(w));
                agents.inspector.unreachable = selected
                    .as_ref()
                    .is_some_and(|w| agents.unreachable.contains(&w.id()));
                agents.inspector.scanning = agents.scanning();
                // A folder workspace's repositories' worktrees, a strip each.
                let worktrees: Vec<_> = match (&selected, &repos) {
                    (Some(w), Some(repos)) if w.folder => agents
                        .worktrees_of(w.host.as_deref(), repos)
                        .into_iter()
                        .map(|t| {
                            let title = agents.title(&t);
                            let pace = agents.pace(&t);
                            let merged = agents.is_merged(&t);
                            (t, title, pace, merged)
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                // The terminal showing, when it is this workspace's and in ssh.
                // A remote workspace's is ssh by design (a ProxyCommand's ssh
                // under it would read as one): its own card and Git show.
                let remote = terminal_panel
                    .active_tab()
                    .filter(|_| selected.as_ref().is_none_or(|w| w.host.is_none()))
                    .filter(|t| {
                        crate::platform::agents::task_of(&agents.tasks, t)
                            == agents.selected_index()
                    })
                    .and_then(|t| t.remote.clone());
                let issues = crate::platform::inspector::Issues {
                    read: &agents.issues,
                    asked: agents.github_asked(),
                    loaded: &agents.arrangement.issues,
                    reading: agents.reading_issues(),
                    now: crate::platform::unix_seconds(),
                };
                agents.inspector.folder_label = selected.as_ref().map_or_else(String::new, |w| {
                    crate::platform::agents::folder_name(w, &agents.tasks)
                });
                agents.inspector.draw(
                    selected.as_ref(),
                    repos.as_deref(),
                    &worktrees,
                    remote.as_ref(),
                    pull_request.as_ref().map(Option::as_ref),
                    github,
                    issues,
                    &agents.arrangement.folded_repos,
                    &mut renderer.atlas,
                    rect,
                    theme,
                    glyphs,
                );
            }
        } else if let Some(rect) = sidebar_rect
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
        } else if let Some(rect) = sidebar_rect
            && *search_open
        {
            layout::push_rect(
                glyphs,
                &renderer.atlas,
                [rect.x, rect.y],
                [rect.width, rect.height],
                theme.sidebar_background,
            );
            search.draw(
                &mut renderer.atlas,
                rect,
                *search_focus,
                tree.root(),
                theme,
                glyphs,
            );
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
            let searching = find.as_ref().and_then(|bar| bar.terminal.as_ref());
            crate::platform::terminal::draw(
                terminal_panel,
                searching,
                &mut renderer.atlas,
                rect,
                theme,
                glyphs,
            );
        }
        // The terminal's find bar, over the top of its screen, with a
        // hairline under it as the document's has the text's edge.
        if let Some(rect) = terminal_find_rect
            && let Some(bar) = find.as_ref()
            && let Some(searching) = bar.terminal.as_ref()
        {
            let count = searching.error.is_none().then(|| searching.count());
            draw_find_bar(glyphs, renderer, theme, rect, bar, count);
            // Filter mode's list, under the bar and over the screen.
            if let Some(list) = terminal_filter_rect {
                crate::platform::terminal::draw_filter(
                    searching,
                    &mut renderer.atlas,
                    list,
                    theme,
                    glyphs,
                );
            }
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
        // Whole until its last moment, when it thins out (not with a
        // button beside it: a control does not fade); gone once it is
        // over.
        let (note, note_alpha) = match message {
            Some((text, at)) if at.elapsed() < message_lasts_of(text, said.as_ref()) => {
                let alpha = if said
                    .as_ref()
                    .and_then(|s| s.about(text))
                    .is_some_and(|s| s.action.is_some())
                {
                    1.0
                } else {
                    layout::message_alpha(at.elapsed(), message_lasts_of(text, said.as_ref()))
                };
                (Some(text.clone()), alpha)
            }
            _ => {
                *message = None;
                (None, 1.0)
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
        let agents_note = note.clone();
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
            table_cell: (column == Column::Table)
                .then(|| tables.get(&buffer.id()).and_then(|view| view.selected))
                .flatten(),
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
        // Agents mode: where the terminal showing is and its branch; the
        // document's name, position and branch are the editor's.
        let (status, detail) = if agents.on {
            let cwd = terminal_panel.active_tab().map(|t| t.cwd.clone());
            let remote = terminal_panel.active_tab().and_then(|t| t.remote.clone());
            let left = agents_note.unwrap_or_else(|| match &remote {
                Some(r) => format!("ssh {}   {}", r.login(), r.connected_for()),
                None => cwd.as_deref().map(short_path).unwrap_or_default(),
            });
            let branch = cwd.as_deref().and_then(|cwd| {
                agents
                    .inspector
                    .status
                    .iter()
                    .filter(|(repo, _)| cwd.starts_with(repo))
                    .max_by_key(|(repo, _)| repo.components().count())
                    .and_then(|(_, s)| s.as_ref().ok()?.branch.clone())
            });
            // This Mac's branch says nothing about the server's.
            let branch = branch.filter(|_| remote.is_none());
            let claude = if claude.as_ref().is_some_and(|c| c.is_connected()) {
                "\u{273b} Claude     "
            } else {
                ""
            };
            (
                left,
                format!(
                    "{claude}{}",
                    branch.map(|b| format!("{b}     ")).unwrap_or_default()
                ),
            )
        } else {
            (status, detail)
        };
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
        } else if let Some(work) = git.working() {
            Some(work.to_owned())
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
                layout::feedback_for(text, Said::kind_of(said.as_ref()))
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
                    layout::faded(tint, note_alpha),
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
        // A message that asks for something has its button right after
        // the text, which yields the room; a window too narrow for both
        // keeps the text.
        let said_now = shown_note.and_then(|text| said.as_ref().and_then(|s| s.about(text)));
        *status_action = None;
        let mut text_rect = left;
        let mut button = None;
        if let Some(action) = said_now.and_then(|s| s.action.as_ref()) {
            let height = layout::UI_CONTROL_SM;
            let row = Viewport {
                x: left.x,
                y: status_rect.y + ((status_height - height) * 0.5).round(),
                width: 0.0,
                height,
            };
            let label = action.label();
            let w = layout::Button::new(row)
                .label(label)
                .content_width(&mut renderer.atlas, layout::UI_INSET)
                .round();
            let room = left.width - w - layout::UI_INSET;
            if room > 0.0 {
                let text_w = layout::ui_text_width(&mut renderer.atlas, text).ceil();
                text_rect.width = text_w.min(room);
                let rect = Viewport {
                    x: left.x + text_rect.width + layout::UI_INSET,
                    width: w,
                    ..row
                };
                button = Some(layout::Button::new(rect).label(label));
            }
        }
        layout::push_ui_text(
            glyphs,
            &mut renderer.atlas,
            text_rect,
            text,
            layout::faded(colour, note_alpha),
        );
        if let Some(note) = shown_note {
            // A long message is cut by the window, and a path is cut to
            // its name; the whole of it is a pointer's rest away.
            let tip = said_now.and_then(|s| s.detail.as_deref()).unwrap_or(note);
            layout::hotspot(text_rect, layout::Cursor::Arrow, Some(tip));
        }
        if let Some(button) = button {
            button.draw(glyphs, &mut renderer.atlas, theme);
            layout::hotspot(button.rect, layout::Cursor::Pointing, None);
            *status_action = Some(button.rect);
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
                indexing: watch.index_rx.is_some(),
                branches: branch_list.as_ref(),
                actions: action_list.as_ref(),
                repos: repo_list.as_deref(),
                deleted: deleted_list.as_deref(),
                mcp_url: *mcp_url_prompt,
                task: *task_prompt,
                task_folder: agents.new_folder(tree.root()),
                task_repo: agents
                    .task_repo()
                    .map(|(host, repo)| shown_repo(host, &repo)),
                task_rename: agents.renaming_name(),
                commit: commit_prompt.as_deref(),
                switch: switch_sources(agents, terminal_panel),
                terminals: term_search.as_ref(),
            };
            let mode = PaletteMode::of(&sources);
            let switching = sources.switch.as_ref().map(|_| switcher::query(&text));
            let intent = sources.branches.map(|pick| &pick.intent);
            let rows: Vec<layout::PaletteRow> = palette_rows(&sources, &text)
                .into_iter()
                .map(|(row, _)| row)
                .collect();
            let words = palette_words(&sources, &text, mode);
            *palette_count = rows.len();
            let rect = layout::palette_rect(
                viewport,
                layout::palette_rows_shown(rows.len(), words.loading),
            );
            // The scrim and the palette come in together over a short
            // fade; once whole, nothing here asks for another frame.
            let overlay_from = glyphs.len();
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
                    heading: words.heading,
                    empty: words.empty,
                    loading: words.loading,
                    placeholder: match mode {
                        PaletteMode::Branch => "Branch name",
                        PaletteMode::Action => "Filter actions",
                        PaletteMode::Repo => "Repository name",
                        PaletteMode::Deleted => "File name",
                        PaletteMode::McpUrl => "https://host/mcp",
                        PaletteMode::TaskName => "Workspace name",
                        PaletteMode::CommitMessage => "Commit message",
                        PaletteMode::Terminals => "Text in every terminal",
                        PaletteMode::Open if switching.is_some() => {
                            "Switch to\u{2026} (> commands, / files)"
                        }
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
                    } else if mode == PaletteMode::TaskName {
                        "Start"
                    } else if mode == PaletteMode::CommitMessage {
                        "Commit"
                    } else if mode == PaletteMode::Deleted {
                        "Recover"
                    } else if mode == PaletteMode::Terminals {
                        "Show"
                    } else if mode == PaletteMode::Action || commands::query(&text).is_some() {
                        "Run"
                    } else if matches!(switching, Some(switcher::Query::Switch(_))) {
                        "Switch"
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
            layout::fade_from(glyphs, overlay_from, layout::overlay_alpha(PALETTE_OVERLAY));
        }

        let background = theme.background;
        crate::platform::jank::lap("draw.build");
        let timing = renderer.draw(layer, glyphs, (viewport.width, viewport.height), background);
        crate::platform::jank::lap("draw.gpu");
        if timing.is_some() {
            *drew_once = true;
        }
        timing
    }
}

/// The find bar: its two fields, the buttons and options beside them and,
/// for a project search, the results under them. `count` is the current
/// match's place from 0 and how many there are, in the document or the
/// terminal. Over the terminal it is one row: no Replace, no Project, and
/// a Filter chip for the list of the lines with a match.
fn draw_find_bar(
    glyphs: &mut Vec<GlyphInstance>,
    renderer: &mut Renderer,
    theme: &Theme,
    rect: Viewport,
    bar: &FindBar,
    count: Option<(usize, usize)>,
) {
    let terminal = bar.terminal.is_some();
    // A panel floating over the text or the screen, with the palette's
    // edge, so it reads as laid on it rather than cut into it.
    layout::push_panel(glyphs, rect, layout::UI_RADIUS_LG, theme);
    let g = layout::FindGeometry::panel(rect, find_chips(terminal), !terminal);
    if !terminal {
        Button::new(g.toggle)
            .icon(if bar.show_replace {
                icons::CHEVRON_DOWN
            } else {
                icons::CHEVRON_RIGHT
            })
            .tone(Tone::Ghost)
            .tip(if bar.show_replace {
                "Hide Replace"
            } else {
                "Replace  \u{21e5}"
            })
            .draw(glyphs, &mut renderer.atlas, theme);
    }

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
                 trailing: Option<(&str, [f32; 4])>| {
        if focused {
            layout::push_focus_ring(glyphs, box_rect, layout::UI_RADIUS, 1.5, theme);
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
        if let Some((trailing, colour)) = trailing {
            layout::push_ui_text_right(glyphs, atlas, inner, trailing, colour);
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
    // nothing from one that found everything. Nothing found is said in
    // the removed-lines red, the one colour here that means "no".
    // A terminal search for a new query that is taking a while says so
    // where its count will be.
    let searching = bar
        .terminal
        .as_ref()
        .is_some_and(|find| find.searching_shown());
    let said = count.map(|(at, total)| match total {
        _ if searching => ("Searching\u{2026}".to_string(), theme.status_text),
        0 => ("No matches".to_string(), theme.diff_removed),
        // The terminal's search stops at a limit; past it, more.
        n if terminal && n >= crate::platform::terminal::FIND_LIMIT => {
            (format!("{} of {n}+", at + 1), theme.status_text)
        }
        n => (format!("{} of {n}", at + 1), theme.status_text),
    });
    field(
        glyphs,
        &mut renderer.atlas,
        g.find_field,
        "Find",
        &bar.query,
        bar.has_keys && !bar.replacing,
        said.as_ref().map(|(text, colour)| (text.as_str(), *colour)),
    );
    let replace_row = !terminal && bar.show_replace;
    if replace_row {
        field(
            glyphs,
            &mut renderer.atlas,
            g.replace_field,
            "Replace with",
            &bar.replacement,
            bar.has_keys && bar.replacing,
            None,
        );
    }

    // The shared button: the same height, corners and hover as every
    // other control, with arrows and a cross from the icon font rather
    // than guillemets and a multiplication sign.
    let has_matches = count.is_some_and(|(_, total)| total > 0);
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
    // One row over a terminal, or until the replace row is asked for.
    if replace_row {
        Button::new(g.replace_one)
            .label("Replace")
            .enabled(has_matches)
            .draw(glyphs, atlas, theme);
        Button::new(g.replace_all)
            .label("All")
            .enabled(has_matches)
            .tip("Replace All")
            .draw(glyphs, atlas, theme);
    }

    // Toggles that show their state: filled and accented when on,
    // quiet when off.
    for (slot, (label, on, tip)) in [
        ("Aa", bar.options.case_sensitive, "Match Case"),
        ("Word", bar.options.whole_word, "Match Whole Word"),
        (".*", bar.options.regex, "Use Regular Expression"),
    ]
    .into_iter()
    .enumerate()
    {
        Button::new(g.options[slot])
            .label(label)
            .on(on)
            .tip(tip)
            .draw(glyphs, atlas, theme);
    }
    if let Some(find) = bar.terminal.as_ref() {
        Button::new(g.options[3])
            .label("Filter")
            .on(find.filter.is_some())
            .tip("Show Matching Lines  \u{2325}\u{21b5}")
            .draw(glyphs, atlas, theme);
    }
}

/// The find bar's option chips: Aa, Word and .*, and over the terminal
/// Filter too.
pub(super) fn find_chips(terminal: bool) -> usize {
    if terminal { 4 } else { 3 }
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
    /// The grid's selected cell, as record and column from 0, while a CSV
    /// or TSV document shows as a grid: it stands in for the caret.
    table_cell: Option<(usize, usize)>,
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
        } else if buffer.is_frozen() {
            "Read-only".to_string()
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
    // In the grid the selected cell is the position, numbered as the
    // gutter numbers the records; the same width as the caret's.
    let place = match parts.table_cell {
        Some((row, column)) => format!("Row {}, Col {}", row + 1, column + 1),
        None => format!("Ln {}, Col {}", line + 1, column + 1),
    };
    let position = if buffer.is_view_only() {
        String::new()
    } else {
        format!(
            "{place}     {}     {}",
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
            table_cell: None,
        }
    }

    #[test]
    fn the_grids_cell_stands_in_for_the_caret() {
        let buffer = Buffer::new();
        let grid = StatusParts {
            table_cell: Some((2, 1)),
            ..parts(&buffer)
        };
        let (_, right) = status_texts(&grid);
        assert!(right.starts_with("Row 3, Col 2     "), "{right}");
        assert!(!right.contains("Ln "), "{right}");
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
