//! The find bar and project search: matching in the document, replacing,
//! searching the project, and opening a result.

use super::*;

impl EditorView {
    /// Keys while the find bar has focus. Returns whether it consumed them.
    pub(super) fn handle_find_key(&self, event: &NSEvent) -> bool {
        self.poll_project_search();
        const ESCAPE: u16 = 53;
        let flags = event.modifierFlags();
        let command = flags.contains(NSEventModifierFlags::Command);
        let shift = flags.contains(NSEventModifierFlags::Shift);
        let code = match event.keyCode() {
            key::KEYPAD_ENTER => key::RETURN,
            code => code,
        };

        const TAB: u16 = 48;
        match code {
            ESCAPE => {
                self.close_find();
                return true;
            }
            TAB => {
                // Tab crosses between Find and Replace rather than inserting,
                // showing the replace row first; over the terminal there is
                // no Replace.
                let Some(mut state) = self.state_mut() else {
                    return false;
                };
                if let Some(find) = state.find.as_mut().filter(|f| f.terminal.is_none()) {
                    if !find.show_replace {
                        find.show_replace = true;
                        find.replacing = true;
                    } else {
                        find.replacing = !find.replacing;
                    }
                }
                drop(state);
                self.request_redraw();
                self.pump();
                return true;
            }
            key::RETURN => {
                let replacing = self
                    .ivars()
                    .state
                    .borrow()
                    .find
                    .as_ref()
                    .is_some_and(|f| f.replacing);
                if replacing {
                    self.replace_one();
                } else {
                    self.find_step(!shift);
                }
                return true;
            }
            _ => {}
        }

        // The find fields are real editors: the familiar shortcuts work in
        // them as in every field.
        if command {
            let letter = event
                .charactersIgnoringModifiers()
                .and_then(|s| s.to_string().chars().next())
                .map(|c| c.to_ascii_lowercase());
            let Some(mut state) = self.state_mut() else {
                return false;
            };
            let Some(bar) = &mut state.find else {
                return false;
            };
            let replacing = bar.replacing;
            let field = if replacing {
                &mut bar.replacement
            } else {
                &mut bar.query
            };
            let Some(changed) = super::search_view::field_shortcut(field, code, letter, shift)
            else {
                return false;
            };
            drop(state);
            if changed && !replacing {
                self.refresh_find();
            }
            self.request_redraw();
            self.pump();
            return true;
        }

        let motion = if shift { Motion::Extend } else { Motion::Move };
        {
            let mut as_text = false;
            let Some(mut state) = self.state_mut() else {
                return false;
            };
            let Some(bar) = &mut state.find else {
                return false;
            };
            let replacing = bar.replacing;
            let field = if replacing {
                &mut bar.replacement
            } else {
                &mut bar.query
            };
            match code {
                // One line: Up and Down go to its ends.
                key::UP => field.move_line_start(motion),
                key::DOWN => field.move_line_end(motion),
                _ => as_text = !field_key(field, code, flags),
            }
            if as_text {
                // The borrow has to end before the input system is asked,
                // because it answers by calling back into this view.
                drop(state);
                return self.interpret(event);
            }
            // Editing the replacement does not move the match.
            if replacing {
                drop(state);
                self.request_redraw();
                self.pump();
                return true;
            }
        }

        self.refresh_find();
        true
    }

    /// Re-runs the search from the top of the current match, so the selection
    /// tracks the query as it changes, however it changed.
    /// The find bar's matches in the active document, for an action: the
    /// needle, and the matches or `None` when there is nothing to do. Find
    /// reuses the matches drawing keeps; a replacement is expanded per
    /// match, so replacing searches afresh. No size limit here: an action
    /// runs once, drawing runs every frame. An invalid pattern says so.
    pub(super) fn action_matches(
        &self,
        replacement: Option<&str>,
    ) -> Option<(String, Vec<search::Match>)> {
        let mut state = self.state_mut()?;
        let state = &mut *state;
        // Searching the terminal: nothing in the document to act on.
        let bar = state.find.as_ref().filter(|bar| bar.terminal.is_none())?;
        let needle = bar.query.rope.to_string();
        if needle.is_empty() {
            return None;
        }
        let buffer = state.docs.active();
        let found = match replacement {
            None => fill_find_cache(&mut state.find_cache, bar, buffer).map(<[_]>::to_vec),
            Some(with) => search::find(&buffer.rope.to_string(), &needle, with, bar.options),
        };
        match found {
            Ok(matches) => Some((needle, matches)),
            Err(error) => {
                state.message = Some((format!("invalid regex: {error}"), Instant::now()));
                None
            }
        }
    }

    pub(super) fn refresh_find(&self) {
        if self.searching_terminal() {
            self.terminal_find_sync(true);
            self.terminal_find_reveal();
            return;
        }
        let at = {
            let Some(state) = self.state() else {
                return;
            };
            let active = state.docs.active();
            active.selection().map_or(active.cursor(), |r| r.start)
        };
        let Some((_, matches)) = self.action_matches(None) else {
            return;
        };
        if let Some(found) = matches
            .iter()
            .find(|m| m.range.start >= at)
            .or_else(|| matches.first())
        {
            let (rows, cols) = self.grid();
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let covered = find_covered_rows(&state);
            state
                .docs
                .active_mut()
                .select_range(found.range.start, found.range.end);
            state
                .docs
                .active_mut()
                .scroll_to_cursor_below(rows, cols, covered);
        }
    }

    /// Whether the find bar searches the terminal.
    pub(super) fn searching_terminal(&self) -> bool {
        self.state().is_some_and(|state| {
            state
                .find
                .as_ref()
                .is_some_and(|bar| bar.terminal.is_some())
        })
    }

    /// Cmd-F: the find bar over the terminal while it has the keyboard (or
    /// its bar has them back), the document's otherwise.
    pub(super) fn open_find(&self) {
        let terminal = self.terminal_has_keys()
            || self.state().is_some_and(|state| {
                state.terminal.has_keys()
                    && state
                        .find
                        .as_ref()
                        .is_some_and(|bar| bar.has_keys && bar.terminal.is_some())
            });
        if terminal {
            self.open_terminal_find();
        } else {
            self.open_document_find();
        }
    }

    /// Opens the find bar, seeding it from the selection when there is one.
    pub(super) fn open_document_find(&self) {
        let seed = {
            let Some(state) = self.state() else {
                return;
            };
            state
                .docs
                .active()
                .selected_text()
                .filter(|t| !t.contains('\n') && t.len() < 200)
        };
        let mut query = Buffer::new();
        if let Some(seed) = &seed {
            query.insert(seed);
            query.select_all();
        }
        if let Some(mut state) = self.state_mut() {
            close_fields(&mut state);
        }
        // Nothing selected: the last search, selected, so typing replaces
        // it and Return repeats it.
        let mut options = SearchOptions::default();
        if seed.is_none()
            && let Some((last, last_options)) = self.state().and_then(|s| s.last_find.clone())
        {
            query.insert(&last);
            query.select_all();
            options = last_options;
        }
        if let Some(mut state) = self.state_mut()
            && let Some(bar) = &mut state.find
        {
            // Already open: the keys come back to it, with its options, its
            // replacement and its results; a selection becomes the query.
            if seed.is_some() {
                bar.query = query;
            } else {
                bar.query.select_all();
            }
            bar.has_keys = true;
            bar.replacing = false;
            // Off the terminal, onto the document.
            bar.terminal = None;
            drop(state);
            self.refresh_find();
            self.request_redraw();
            self.pump();
            return;
        }
        if let Some(mut state) = self.state_mut() {
            state.find = Some(FindBar {
                query,
                replacement: Buffer::new(),
                replacing: false,
                has_keys: true,
                show_replace: false,
                options,
                terminal: None,
            });
        }
        if seed.is_none() {
            self.refresh_find();
        }
        self.request_redraw();
        self.pump();
    }

    pub(super) fn close_find(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some(bar) = state.find.take() else {
            return;
        };
        remember_find(&mut state, &bar);
        // The terminal it searched has the keyboard again.
        if bar.terminal.is_some() && state.terminal.open {
            state.terminal.focus = true;
        }
        drop(state);
        self.request_redraw();
        self.pump();
    }

    pub(super) fn find_click(&self, x: f32, y: f32, rect: Viewport) {
        // The same rectangles the bar was drawn from. This used to re-derive
        // them: `right - 180.0` here and `right - 180.0` there, option slots
        // 66pt wide in the handler and 62pt wide on screen, so a click beside
        // a chip still toggled it.
        let terminal = self.searching_terminal();
        // Aa, Word and .*; over the terminal, one row with no Replace.
        let g = layout::FindGeometry::panel(rect, 3, !terminal);

        for (slot, hit) in g.options.iter().enumerate() {
            if !hit.contains(x, y) {
                continue;
            }
            {
                let Some(mut state) = self.state_mut() else {
                    return;
                };
                if let Some(bar) = &mut state.find {
                    match slot {
                        0 => bar.options.case_sensitive = !bar.options.case_sensitive,
                        1 => bar.options.whole_word = !bar.options.whole_word,
                        _ => bar.options.regex = !bar.options.regex,
                    }
                }
            }
            self.refresh_find();
            self.request_redraw();
            self.pump();
            return;
        }

        if g.close.contains(x, y) {
            self.close_find();
        } else if g.toggle.contains(x, y) {
            // The chevron: the replace row shows or hides; hidden, the
            // keys are the find field's.
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if let Some(bar) = &mut state.find {
                bar.show_replace = !bar.show_replace;
                if !bar.show_replace {
                    bar.replacing = false;
                }
            }
        } else if g.previous.contains(x, y) || g.next.contains(x, y) {
            self.find_step(g.next.contains(x, y));
        } else if terminal && !g.find_field.contains(x, y) {
            // Nothing else of the bar is over the terminal.
        } else if g.replace_one.contains(x, y) {
            self.replace_one();
        } else if g.replace_all.contains(x, y) {
            self.replace_all();
        } else if g.find_field.contains(x, y) || g.replace_field.contains(x, y) {
            // Clicking a field focuses it and puts the caret where the
            // pointer is, measured through the same shaping that drew it.
            let replace = g.replace_field.contains(x, y);
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let State { find, renderer, .. } = &mut *state;
            if let Some(bar) = find {
                bar.replacing = replace;
                bar.has_keys = true;
                let box_rect = if replace {
                    g.replace_field
                } else {
                    g.find_field
                };
                let field = if replace {
                    &mut bar.replacement
                } else {
                    &mut bar.query
                };
                place_field_caret(&mut renderer.atlas, field, x - box_rect.x - FIND_FIELD_PAD);
            }
        }
        self.request_redraw();
        self.pump();
    }

    /// Replaces the current match, then advances to the next one.
    pub(super) fn replace_one(&self) {
        let (replacement, selected) = {
            let Some(state) = self.state() else {
                return;
            };
            let Some(bar) = &state.find else {
                return;
            };
            (
                bar.replacement.rope.to_string(),
                state.docs.active().selection(),
            )
        };
        let Some((_, matches)) = self.action_matches(Some(&replacement)) else {
            return;
        };
        let replaced = selected
            .and_then(|range| matches.iter().find(|m| m.range == range))
            .is_some_and(|found| {
                let mut state = self.ivars().state.borrow_mut();
                let buffer = state.docs.active_mut();
                // Only the current match: an insert with several cursors
                // would put the replacement at every one of them.
                buffer.collapse_cursors();
                buffer.insert(&found.replacement);
                true
            });

        // Whether or not this one matched, move on: pressing Return in the
        // replace field should always make progress through the file.
        self.find_step(true);
        if replaced {
            self.reparse();
        }
        self.request_redraw();
        self.pump();
    }

    /// Replaces every match in the active document.
    pub(super) fn replace_all(&self) {
        let replacement = {
            let Some(state) = self.state() else {
                return;
            };
            let Some(bar) = &state.find else { return };
            bar.replacement.rope.to_string()
        };
        let Some((needle, matches)) = self.action_matches(Some(&replacement)) else {
            return;
        };
        let edits: Vec<_> = matches
            .into_iter()
            .map(|m| (m.range, m.replacement))
            .collect();

        let count = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let n = state.docs.active_mut().replace_ranges(&edits);
            state.message = Some((
                match n {
                    0 => format!("no matches for {needle:?}"),
                    1 => "replaced 1 occurrence".to_string(),
                    n => format!("replaced {n} occurrences"),
                },
                Instant::now(),
            ));
            n
        };
        if count > 0 {
            self.reparse();
        }
        self.request_redraw();
        self.pump();
    }

    /// Moves the cursor to the next or previous match and selects it.
    pub(super) fn find_step(&self, forward: bool) {
        if self.searching_terminal() {
            self.terminal_find_step(forward);
            return;
        }
        let (from, selected) = {
            let Some(state) = self.state() else {
                return;
            };
            (
                state.docs.active().cursor(),
                state.docs.active().selection(),
            )
        };
        let Some((query, matches)) = self.action_matches(None) else {
            return;
        };
        let (rows, cols) = self.grid();
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let found = if forward {
            let start = selected.map_or(from, |r| r.start.saturating_add(1));
            matches
                .iter()
                .find(|m| m.range.start >= start)
                .or_else(|| matches.first())
        } else {
            let before = selected.map_or(from, |r| r.start);
            matches
                .iter()
                .rev()
                .find(|m| m.range.start < before)
                .or_else(|| matches.last())
        };

        let Some(found) = found else {
            state.message = Some((format!("no match for {query:?}"), Instant::now()));
            return;
        };

        let covered = find_covered_rows(&state);
        state
            .docs
            .active_mut()
            .select_range(found.range.start, found.range.end);
        state
            .docs
            .active_mut()
            .scroll_to_cursor_below(rows, cols, covered);
    }
}

/// The text rows the document's find panel covers, so a match is shown
/// below it rather than under it. None over the terminal, or closed.
pub(super) fn find_covered_rows(state: &State) -> usize {
    let rows = match state.find.as_ref() {
        Some(bar) if bar.terminal.is_none() => 1 + usize::from(bar.show_replace),
        _ => return 0,
    };
    layout::find_panel_rows(rows, state.renderer.atlas.metrics.line_height)
}

/// Keeps `bar`'s query and options for the next bar, when it has a query.
pub(super) fn remember_find(state: &mut State, bar: &FindBar) {
    let query = bar.query.rope.to_string();
    if !query.is_empty() {
        state.last_find = Some((query, bar.options));
    }
}
