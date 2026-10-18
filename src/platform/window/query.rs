//! SQL over the active table through DuckDB: the Query field in the
//! table's strip, the statement run on a worker, and the result tab. The
//! field and the strip are `crate::platform::table`; the tool is
//! `crate::project::duckdb`.

use super::*;
use crate::platform::table::{QueryField, QueryStrip, QueryView, TableView};
use crate::project::duckdb;
use std::path::PathBuf;

/// A query out on the worker: the document it ran over, enough to title
/// and describe the result tab, and where the outcome arrives.
pub(super) struct QueryJob {
    pub(super) buffer: u64,
    pub(super) source: PathBuf,
    pub(super) sql: String,
    pub(super) rx: mpsc::Receiver<Result<duckdb::Outcome, String>>,
}

/// Closes the Query field, keeping its text with the document's table for
/// the next time it opens.
pub(super) fn close_query_field(state: &mut State) {
    let Some(field) = state.query.take() else {
        return;
    };
    if let Some(view) = state.tables.get_mut(&field.buffer) {
        view.sql = field.field.rope.to_string();
    }
}

/// The query side of document `id`'s strip, from the parts of the state
/// that hold it; for the frames and hit lists that have them borrowed
/// apart.
pub(super) fn query_strip<'a>(
    id: u64,
    tables: &'a HashMap<u64, TableView>,
    query: &'a Option<QueryField>,
    job: &'a Option<QueryJob>,
    queries: &'a HashMap<u64, QueryView>,
) -> QueryStrip<'a> {
    QueryStrip {
        field: query
            .as_ref()
            .filter(|field| field.buffer == id)
            .map(|field| &field.field),
        running: job.as_ref().is_some_and(|job| job.buffer == id),
        error: tables.get(&id).and_then(|view| view.query_error.as_deref()),
        result: queries.get(&id),
    }
}

/// The query side of the active document's strip.
pub(super) fn active_query_strip(state: &State) -> QueryStrip<'_> {
    query_strip(
        state.docs.active().id(),
        &state.tables,
        &state.query,
        &state.query_job,
        &state.queries,
    )
}

/// Whether the active document can have the Query field: a table, and not
/// itself the result of a query.
pub(super) fn can_query(state: &State) -> bool {
    active_table(state).is_some() && !state.queries.contains_key(&state.docs.active().id())
}

impl EditorView {
    /// Opens the Query field over the active table with the statement it
    /// last held, or closes it: the Query segment and its menu command.
    pub(super) fn toggle_query_field(&self) {
        let open = self.state().is_some_and(|state| state.query.is_some());
        if open {
            if let Some(mut state) = self.state_mut() {
                close_query_field(&mut state);
            }
            self.after_query_change();
        } else {
            self.open_query_field();
        }
    }

    /// Opens the Query field over the active table, or leaves it open.
    /// Whether it is open after.
    pub(super) fn open_query_field(&self) -> bool {
        {
            let Some(mut state) = self.state_mut() else {
                return false;
            };
            sync_tables(&mut state);
            if state.query.is_some() {
                return true;
            }
            if !can_query(&state) {
                return false;
            }
            let id = state.docs.active().id();
            let sql = state
                .tables
                .get(&id)
                .map(|view| view.sql.clone())
                .unwrap_or_default();
            close_fields(&mut state);
            state.query = Some(QueryField::new(id, &sql));
        }
        self.after_query_change();
        true
    }

    /// The field's box, pointer and keys moved with it.
    fn after_query_change(&self) {
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
        self.request_redraw();
        self.pump();
    }

    /// Puts the field's caret where a press at `x` landed in it.
    pub(super) fn query_field_press(&self, x: f32) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(rect) = frame_of(&mut state).rect(&Hit::TableQueryField) else {
                return;
            };
            let State {
                renderer, query, ..
            } = &mut *state;
            if let Some(field) = query.as_mut() {
                place_field_caret(
                    &mut renderer.atlas,
                    &mut field.field,
                    x - rect.x - crate::platform::table::QUERY_FIELD_PAD,
                );
            }
        }
        self.request_redraw();
        self.pump();
    }

    /// Runs the field's statement over the active document's file: Return
    /// in the field or the Run button. The file is read from disk, so an
    /// unsaved edit is not in the result; the strip says it is running
    /// until the worker answers ([`Self::poll_query`]).
    pub(super) fn run_query(&self) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(field) = state.query.as_ref() else {
                return;
            };
            let id = field.buffer;
            let sql = field.field.rope.to_string();
            if let Some(view) = state.tables.get_mut(&id) {
                view.sql = sql.clone();
            }
            if sql.trim().is_empty() {
                state.say(
                    layout::Feedback::Info,
                    "Type a statement to run; the file is the table called here",
                );
                drop(state);
                self.request_redraw();
                return;
            }
            if state.query_job.is_some() {
                state.say(layout::Feedback::Info, "a query is still running");
                drop(state);
                self.request_redraw();
                return;
            }
            let buffer = state.docs.active();
            if buffer.id() != id {
                return;
            }
            let Some(path) = buffer.path.clone() else {
                state.say(
                    layout::Feedback::Failure,
                    "Save the file first: DuckDB reads it from disk",
                );
                drop(state);
                self.request_redraw();
                return;
            };
            let Some(reader) = duckdb::Reader::for_path(&path) else {
                state.say(
                    layout::Feedback::Failure,
                    "DuckDB has no reader for this kind of file",
                );
                drop(state);
                self.request_redraw();
                return;
            };
            let Some(exe) = duckdb::duckdb() else {
                state.say(layout::Feedback::Failure, duckdb::missing());
                drop(state);
                self.request_redraw();
                return;
            };
            let dirty = buffer.is_dirty();
            if let Some(view) = state.tables.get_mut(&id) {
                view.query_error = None;
            }
            let query = duckdb::Query {
                source: path.clone(),
                reader,
                table: None,
                sql: sql.clone(),
            };
            let rx = duckdb::spawn(query, &exe);
            state.query_job = Some(QueryJob {
                buffer: id,
                source: path,
                sql,
                rx,
            });
            if dirty {
                state.say(
                    layout::Feedback::Info,
                    "querying the file as it is saved on disk",
                );
            }
        }
        // The spinner in the strip animates on the display link.
        self.resume_display_link();
        self.request_redraw();
        self.pump();
    }

    /// Takes a finished query in: its CSV into the result tab titled for
    /// the file, reused by a second run, read-only and drawn as a grid;
    /// a failure into the strip of the document it ran over, whose field
    /// keeps its text.
    pub(super) fn poll_query(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let Some(job) = &state.query_job else {
            return;
        };
        let outcome = match job.rx.try_recv() {
            Ok(outcome) => outcome,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("the query worker stopped".to_owned()),
        };
        let Some(job) = state.query_job.take() else {
            return;
        };
        match outcome {
            Ok(outcome) => {
                let file = job
                    .source
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let title = crate::platform::table::result_title(&file);
                match state.docs.index_of_label(&title) {
                    Some(index) => {
                        state.docs.switch(index);
                        state.docs.active_mut().regenerate(&outcome.csv);
                    }
                    None => state
                        .docs
                        .push(Buffer::generated(&title, "csv", &outcome.csv)),
                }
                let buffer = state.docs.active_mut();
                // Read and never edited: the grid refuses a cell edit on it.
                buffer.set_frozen(true);
                let id = buffer.id();
                let view = QueryView {
                    source: job.source,
                    sql: job.sql,
                    rows: outcome.rows,
                    elapsed: outcome.elapsed,
                    truncated: outcome.truncated,
                };
                let note = format!(
                    "{title}: {} in {:.1} s",
                    crate::platform::table::rows_text(view.rows, view.truncated),
                    view.elapsed.as_secs_f64()
                );
                state.queries.insert(id, view);
                uncover_column(&mut state);
                reveal_active_tab(&mut state);
                state.say(layout::Feedback::Success, note);
            }
            Err(error) => {
                if let Some(view) = state.tables.get_mut(&job.buffer) {
                    view.query_error = Some(error);
                }
            }
        }
        drop(state);
        self.sync_title();
        self.reparse();
        self.request_redraw();
    }

    /// Back to the file a result tab came from, its Query field open with
    /// the statement.
    pub(super) fn query_back(&self) {
        let index = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let id = state.docs.active().id();
            let Some(view) = state.queries.get(&id) else {
                return;
            };
            let source = view.source.clone();
            match state.docs.index_of(&source) {
                Some(index) => index,
                None => {
                    let name = source
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    state.say(layout::Feedback::Info, format!("{name} is not open"));
                    drop(state);
                    self.request_redraw();
                    return;
                }
            }
        };
        self.activate_tab(index);
        self.open_query_field();
    }

    /// Keys while the Query field has them. Return runs the statement,
    /// with or without Command; Escape closes the field, its text kept;
    /// the editing keys edit the field and the rest is typed into it.
    /// Other Command keys go to the menu.
    pub(super) fn handle_query_key(&self, event: &NSEvent) -> bool {
        const ESCAPE: u16 = 53;
        let flags = event.modifierFlags();
        let command = flags.contains(NSEventModifierFlags::Command);
        let code = match event.keyCode() {
            key::KEYPAD_ENTER => key::RETURN,
            code => code,
        };
        match code {
            key::RETURN => {
                self.run_query();
                return true;
            }
            _ if command => return false,
            ESCAPE => {
                if let Some(mut state) = self.state_mut() {
                    close_query_field(&mut state);
                }
                self.after_query_change();
                return true;
            }
            key::DELETE | key::FORWARD_DELETE | key::LEFT | key::RIGHT | key::HOME | key::END => {
                let Some(mut state) = self.state_mut() else {
                    return false;
                };
                if let Some(field) = &mut state.query {
                    field_key(&mut field.field, code, flags);
                }
            }
            _ => return self.interpret(event),
        }
        self.request_redraw();
        self.pump();
        true
    }
}
