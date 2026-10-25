//! The Search view in the sidebar: a query with its options, a
//! replacement, files to include and exclude, and what the project
//! search found, listed by file and line. The window drives it (see
//! `window/search.rs`); this is its state, its geometry and its drawing,
//! so a click lands where the eye says it should.
//!
//! Project search used to list its results under the find bar, in the
//! editor column, a dozen rows at most with the file pushed down under
//! them. A view of its own keeps the results while you read and edit,
//! the way Source Control keeps its changes.

use crate::platform::search::{Options, ProjectHit};
use crate::project::icons;
use crate::render::{
    font::Atlas,
    layout::{self, Button, Theme, Tone, Viewport},
    metal::GlyphInstance,
};
use crate::text::buffer::Buffer;
use std::{
    collections::HashSet,
    ops::Range,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// A pause in typing this long starts the search.
pub const DEBOUNCE: Duration = Duration::from_millis(250);
/// A result row's height.
pub const ROW: f32 = layout::SIDEBAR_ROW_HEIGHT;
/// The "N results in M files" line.
const NOTE_HEIGHT: f32 = 24.0;
/// The chevron that shows the replace row.
const TOGGLE: f32 = 18.0;
/// An option chip inside the query field: Aa, Word, .*.
const CHIP: f32 = 22.0;
/// Air inside a field, before its text.
const FIELD_PAD: f32 = 8.0;
/// Where a line row's text starts, in from the file row's name.
const LINE_INDENT: f32 = 28.0;

/// The field with the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Query,
    Replace,
    Include,
    Exclude,
}

/// A row of the list: a file, or one line of it with every match on that
/// line. The hits used to be listed one per row, so three matches on a
/// line were three rows saying the same thing, with nothing to say which
/// part of the line had matched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// The file of the hits from `first`, and how many of them.
    File { first: usize, count: usize },
    /// The hits `hits` of the results, on one line, with one snippet.
    Line { hits: Range<usize> },
}

impl Row {
    /// The hit the row opens: a file's first, a line's first.
    pub fn hit(&self) -> usize {
        match self {
            Row::File { first, .. } => *first,
            Row::Line { hits } => hits.start,
        }
    }
}

/// The rows for `hits`, which the search lists by file and line: a file
/// row, then a row per line of it with the same snippet.
pub fn group_hits(hits: &[ProjectHit]) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut i = 0;
    while i < hits.len() {
        let file_from = i;
        while i < hits.len() && hits[i].path == hits[file_from].path {
            i += 1;
        }
        rows.push(Row::File {
            first: file_from,
            count: i - file_from,
        });
        let mut j = file_from;
        while j < i {
            let line_from = j;
            while j < i
                && hits[j].line == hits[line_from].line
                && hits[j].snippet == hits[line_from].snippet
            {
                j += 1;
            }
            rows.push(Row::Line { hits: line_from..j });
        }
    }
    rows
}

/// Every rectangle of the view, for drawing and for hit testing.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    /// Collapse All, Clear Results, Filters, at the title row's right.
    pub actions: [Viewport; 3],
    /// The chevron that shows and hides the replace row.
    pub toggle: Viewport,
    pub query: Viewport,
    /// Aa, Word and .*, inside the query field at its right.
    pub chips: [Viewport; 3],
    /// No height while the replace row is hidden.
    pub replace: Viewport,
    pub replace_all: Viewport,
    /// No height while the filters are hidden.
    pub include: Viewport,
    pub exclude: Viewport,
    pub note: Viewport,
    pub list: Viewport,
}

impl Geometry {
    pub fn new(column: Viewport, show_replace: bool, show_filters: bool) -> Self {
        let actions = layout::sidebar_header_buttons::<3>(column);
        let x = column.x + layout::UI_INSET;
        let width = (column.width - layout::UI_INSET * 2.0).max(0.0);
        let mut y = column.y + layout::SIDEBAR_HEADER_HEIGHT;
        let row = |y: f32, shown: bool| Viewport {
            x,
            y,
            width,
            height: if shown { layout::UI_CONTROL } else { 0.0 },
        };
        let step = |y: &mut f32, shown: bool| {
            if shown {
                *y += layout::UI_CONTROL + layout::UI_GAP;
            }
        };

        let first = row(y, true);
        let toggle = Viewport {
            width: TOGGLE,
            ..first
        };
        let query = Viewport {
            x: first.x + TOGGLE + 2.0,
            width: (first.width - TOGGLE - 2.0).max(0.0),
            ..first
        };
        let chips = std::array::from_fn(|i| Viewport {
            x: query.x + query.width - 3.0 - CHIP * (3 - i) as f32 - 2.0 * (2 - i) as f32,
            y: query.y + (query.height - CHIP) * 0.5,
            width: CHIP,
            height: CHIP,
        });
        step(&mut y, true);

        let second = row(y, show_replace);
        let replace_all = Viewport {
            x: second.x + second.width - layout::UI_CONTROL,
            width: if show_replace {
                layout::UI_CONTROL
            } else {
                0.0
            },
            ..second
        };
        let replace = Viewport {
            x: query.x,
            width: (replace_all.x - layout::UI_GAP - query.x).max(0.0),
            ..second
        };
        step(&mut y, show_replace);

        let include = Viewport {
            x: query.x,
            width: query.width,
            ..row(y, show_filters)
        };
        step(&mut y, show_filters);
        let exclude = Viewport {
            x: query.x,
            width: query.width,
            ..row(y, show_filters)
        };
        step(&mut y, show_filters);

        let note = Viewport {
            x,
            y,
            width,
            height: NOTE_HEIGHT,
        };
        y += NOTE_HEIGHT;
        let list = Viewport {
            x: column.x,
            y,
            width: column.width,
            height: (column.y + column.height - y).max(0.0),
        };
        Geometry {
            actions,
            toggle,
            query,
            chips,
            replace,
            replace_all,
            include,
            exclude,
            note,
            list,
        }
    }

    /// The field at a point.
    pub fn field_at(&self, x: f32, y: f32) -> Option<Field> {
        [
            (Field::Query, self.query),
            (Field::Replace, self.replace),
            (Field::Include, self.include),
            (Field::Exclude, self.exclude),
        ]
        .into_iter()
        .find(|(_, rect)| rect.height > 0.0 && rect.contains(x, y))
        .map(|(field, _)| field)
    }

    /// The field's rectangle.
    pub fn field(&self, field: Field) -> Viewport {
        match field {
            Field::Query => self.query,
            Field::Replace => self.replace,
            Field::Include => self.include,
            Field::Exclude => self.exclude,
        }
    }

    /// Rows the list has room for.
    pub fn visible_rows(&self) -> usize {
        (self.list.height / ROW).floor() as usize
    }
}

/// The view's state: what is asked, and what was found.
#[derive(Default)]
pub struct Panel {
    pub query: Buffer,
    pub replacement: Buffer,
    pub include: Buffer,
    pub exclude: Buffer,
    pub options: Options,
    pub show_replace: bool,
    pub show_filters: bool,
    pub results: Vec<ProjectHit>,
    rows: Vec<Row>,
    /// Files whose line rows are folded away.
    pub collapsed: HashSet<PathBuf>,
    /// The selected row, in `rows`.
    pub selected: usize,
    /// The first listed row shown.
    pub scroll: usize,
    /// A search runs on a worker.
    pub searching: bool,
    /// When the search for what was typed starts, once typing pauses.
    pub due: Option<Instant>,
    /// The results are Find References for the query, not a text search.
    pub references: bool,
    /// Whether a search has answered since the query changed: "No
    /// results" is said only then.
    pub answered: bool,
}

impl Panel {
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// The geometry for the view in `column`, as it is now.
    pub fn geometry(&self, column: Viewport) -> Geometry {
        Geometry::new(column, self.show_replace, self.show_filters)
    }

    /// Takes a search's results, listed by file and line, on the first
    /// line row, scrolled to the top.
    pub fn take_results(&mut self, results: Vec<ProjectHit>) {
        self.rows = group_hits(&results);
        self.results = results;
        self.collapsed.clear();
        self.selected = self
            .rows
            .iter()
            .position(|row| matches!(row, Row::Line { .. }))
            .unwrap_or(0);
        self.scroll = 0;
        self.searching = false;
        self.answered = true;
    }

    /// No results, nothing selected, the top.
    pub fn clear(&mut self) {
        self.results.clear();
        self.rows.clear();
        self.collapsed.clear();
        self.selected = 0;
        self.scroll = 0;
        self.answered = false;
    }

    /// The file the results from `first` are in.
    fn file_of(&self, first: usize) -> Option<&Path> {
        self.results.get(first).map(|hit| hit.path.as_path())
    }

    /// The indices of the rows listed: every file row, and the line rows
    /// of the files not collapsed.
    pub fn listed(&self) -> Vec<usize> {
        let mut out = Vec::with_capacity(self.rows.len());
        let mut hidden = false;
        for (i, row) in self.rows.iter().enumerate() {
            match row {
                Row::File { first, .. } => {
                    hidden = self
                        .file_of(*first)
                        .is_some_and(|path| self.collapsed.contains(path));
                    out.push(i);
                }
                Row::Line { .. } if !hidden => out.push(i),
                Row::Line { .. } => {}
            }
        }
        out
    }

    /// The rows on screen: each one's index in `rows` and its rectangle.
    pub fn visible(&self, g: Geometry) -> Vec<(usize, Viewport)> {
        self.listed()
            .into_iter()
            .skip(self.scroll)
            .take(g.visible_rows())
            .enumerate()
            .map(|(on_screen, index)| {
                (
                    index,
                    Viewport {
                        x: g.list.x,
                        y: g.list.y + on_screen as f32 * ROW,
                        width: g.list.width,
                        height: ROW,
                    },
                )
            })
            .collect()
    }

    /// The row at `y` of the list, if one is drawn there.
    pub fn row_at(&self, g: Geometry, y: f32) -> Option<usize> {
        self.visible(g)
            .into_iter()
            .find(|(_, rect)| y >= rect.y && y < rect.y + rect.height)
            .map(|(index, _)| index)
    }

    /// Folds or unfolds the file of row `index`.
    pub fn toggle_file(&mut self, index: usize) {
        let Some(Row::File { first, .. }) = self.rows.get(index) else {
            return;
        };
        let Some(path) = self.file_of(*first).map(Path::to_path_buf) else {
            return;
        };
        if !self.collapsed.remove(&path) {
            self.collapsed.insert(path);
        }
        self.selected = index;
    }

    /// Folds every file, or unfolds them all when every one is folded.
    pub fn collapse_all(&mut self) {
        let files: Vec<PathBuf> = self
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::File { first, .. } => self.file_of(*first).map(Path::to_path_buf),
                Row::Line { .. } => None,
            })
            .collect();
        if files.iter().all(|f| self.collapsed.contains(f)) {
            self.collapsed.clear();
        } else {
            self.collapsed.extend(files);
        }
        self.scroll = 0;
    }

    /// The next or previous listed line row from the selected one, which
    /// the arrows step through; file rows are headings. The list follows.
    pub fn select_step(&mut self, forward: bool, visible: usize) {
        let listed = self.listed();
        let lines = || {
            listed
                .iter()
                .copied()
                .filter(|&i| matches!(self.rows[i], Row::Line { .. }))
        };
        let next = if forward {
            lines().find(|&i| i > self.selected)
        } else {
            lines().rev().find(|&i| i < self.selected)
        };
        if let Some(next) = next {
            self.selected = next;
            if let Some(at) = listed.iter().position(|&i| i == next) {
                if at < self.scroll {
                    self.scroll = at;
                }
                if visible > 0 && at >= self.scroll + visible {
                    self.scroll = at + 1 - visible;
                }
            }
        }
    }

    /// The wheel over the list.
    pub fn scroll_by(&mut self, delta: isize, visible: usize) {
        self.scroll =
            layout::scroll_clamped(self.scroll, delta, self.listed().len(), visible.max(1));
    }

    /// How many files the results are in.
    pub fn files(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| matches!(row, Row::File { .. }))
            .count()
    }

    /// What the note under the fields says.
    pub fn note(&self) -> Option<String> {
        if self.searching && self.results.is_empty() {
            return Some("Searching\u{2026}".into());
        }
        if self.query.rope.len_bytes() == 0 && !self.references {
            return None;
        }
        if !self.answered {
            return None;
        }
        let count = self.results.len();
        if count == 0 {
            return Some("No results".into());
        }
        let what = if self.references {
            "reference"
        } else {
            "result"
        };
        let files = self.files();
        Some(format!(
            "{count}{} {what}{} in {files} file{}",
            if count >= crate::platform::search::PROJECT_HITS {
                "+"
            } else {
                ""
            },
            if count == 1 { "" } else { "s" },
            if files == 1 { "" } else { "s" }
        ))
    }

    /// Draws the view in `column`; `focus` is the field with the
    /// keyboard, `root` the folder paths are shown relative to.
    pub fn draw(
        &self,
        atlas: &mut Atlas,
        column: Viewport,
        focus: Option<Field>,
        root: Option<&Path>,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) {
        let g = self.geometry(column);
        layout::push_sidebar_title(
            out,
            atlas,
            column,
            if self.references {
                "REFERENCES"
            } else {
                "SEARCH"
            },
            3,
            theme,
        );
        Button::new(g.actions[0])
            .icon(icons::COLLAPSE_ALL)
            .tone(Tone::Ghost)
            .enabled(!self.rows.is_empty())
            .tip("Collapse All")
            .draw(out, atlas, theme);
        Button::new(g.actions[1])
            .icon(icons::CLOSE)
            .tone(Tone::Ghost)
            .enabled(!self.rows.is_empty() || self.query.rope.len_bytes() > 0)
            .tip("Clear Search")
            .draw(out, atlas, theme);
        Button::new(g.actions[2])
            .icon(icons::ELLIPSIS)
            .tone(Tone::Ghost)
            .on(self.show_filters)
            .tip("Files to Include and Exclude")
            .draw(out, atlas, theme);

        Button::new(g.toggle)
            .icon(if self.show_replace {
                icons::CHEVRON_DOWN
            } else {
                icons::CHEVRON_RIGHT
            })
            .tone(Tone::Ghost)
            .tip(if self.show_replace {
                "Hide Replace"
            } else {
                "Replace"
            })
            .draw(out, atlas, theme);
        field(
            out,
            atlas,
            g.query,
            &self.query,
            "Search",
            focus == Some(Field::Query),
            CHIP * 3.0 + 10.0,
            theme,
        );
        for (rect, (label, on, tip)) in g.chips.into_iter().zip([
            ("Aa", self.options.case_sensitive, "Match Case"),
            ("ab", self.options.whole_word, "Match Whole Word"),
            (".*", self.options.regex, "Use Regular Expression"),
        ]) {
            Button::new(rect)
                .label(label)
                .tone(Tone::Ghost)
                .on(on)
                .tip(tip)
                .draw(out, atlas, theme);
        }
        if self.show_replace {
            field(
                out,
                atlas,
                g.replace,
                &self.replacement,
                "Replace",
                focus == Some(Field::Replace),
                0.0,
                theme,
            );
            Button::new(g.replace_all)
                .icon(icons::REPLACE_ALL)
                .tone(Tone::Danger)
                .enabled(!self.results.is_empty())
                .tip("Replace All")
                .draw(out, atlas, theme);
        }
        if self.show_filters {
            field(
                out,
                atlas,
                g.include,
                &self.include,
                "files to include",
                focus == Some(Field::Include),
                0.0,
                theme,
            );
            field(
                out,
                atlas,
                g.exclude,
                &self.exclude,
                "files to exclude",
                focus == Some(Field::Exclude),
                0.0,
                theme,
            );
        }

        if let Some(note) = self.note() {
            let mut x = g.note.x;
            if self.searching {
                x += layout::push_spinner(out, g.note.x, g.note, theme.accent) + 6.0;
            }
            layout::push_ui_text(
                out,
                atlas,
                Viewport {
                    x,
                    width: (g.note.width - (x - g.note.x)).max(0.0),
                    ..g.note
                },
                &note,
                if !self.searching && self.answered && self.results.is_empty() {
                    theme.diff_removed
                } else {
                    theme.status_text
                },
            );
        }

        let from = layout::hotspots_len();
        for (index, rect) in self.visible(g) {
            layout::hotspot(rect, layout::Cursor::Pointing, None);
            if index == self.selected {
                layout::push_rect(
                    out,
                    atlas,
                    [rect.x, rect.y],
                    [rect.width, rect.height],
                    theme.sidebar_selected,
                );
            } else if layout::hovered(rect) {
                layout::push_rect(
                    out,
                    atlas,
                    [rect.x, rect.y],
                    [rect.width, rect.height],
                    theme.row_hover,
                );
            }
            match &self.rows[index] {
                Row::File { first, count } => {
                    let hit = &self.results[*first];
                    let path = root
                        .and_then(|root| hit.path.strip_prefix(root).ok())
                        .unwrap_or(&hit.path);
                    let folded = self.collapsed.contains(&hit.path);
                    layout::push_icon_centered(
                        out,
                        atlas,
                        Viewport {
                            x: rect.x + layout::UI_INSET - 4.0,
                            width: 16.0,
                            ..rect
                        },
                        if folded {
                            icons::CHEVRON_RIGHT
                        } else {
                            icons::CHEVRON_DOWN
                        },
                        theme.status_text,
                    );
                    let count = count.to_string();
                    let count_width = layout::ui_text_width(atlas, &count);
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let dir = path
                        .parent()
                        .map(|p| p.to_string_lossy().into_owned())
                        .filter(|p| !p.is_empty())
                        .unwrap_or_default();
                    let text_x = rect.x + layout::UI_INSET + 16.0;
                    let room =
                        (rect.x + rect.width - layout::UI_INSET - count_width - 8.0 - text_x)
                            .max(0.0);
                    let name_width = layout::ui_text_width(atlas, &name).min(room);
                    layout::push_ui_text(
                        out,
                        atlas,
                        Viewport {
                            x: text_x,
                            width: room,
                            ..rect
                        },
                        &name,
                        theme.text,
                    );
                    if !dir.is_empty() {
                        layout::push_ui_text(
                            out,
                            atlas,
                            Viewport {
                                x: text_x + name_width + 6.0,
                                width: (room - name_width - 6.0).max(0.0),
                                ..rect
                            },
                            &dir,
                            theme.status_text,
                        );
                    }
                    layout::push_ui_text_right(
                        out,
                        atlas,
                        Viewport {
                            x: rect.x + layout::UI_INSET,
                            width: (rect.width - layout::UI_INSET * 2.0).max(0.0),
                            ..rect
                        },
                        &count,
                        theme.status_text,
                    );
                }
                Row::Line { hits } => {
                    let hit = &self.results[hits.start];
                    let text_rect = Viewport {
                        x: rect.x + layout::UI_INSET + LINE_INDENT,
                        width: (rect.width - layout::UI_INSET * 2.0 - LINE_INDENT).max(0.0),
                        ..rect
                    };
                    for h in &self.results[hits.clone()] {
                        let before = layout::ui_text_width(atlas, &hit.snippet[..h.at.start]);
                        let span = layout::ui_text_width(atlas, &hit.snippet[h.at.clone()]);
                        if span <= 0.0 || before >= text_rect.width {
                            continue;
                        }
                        layout::push_rounded_rect(
                            out,
                            Viewport {
                                x: text_rect.x + before - 1.0,
                                y: rect.y + 5.0,
                                width: (span + 2.0).min(text_rect.width - before),
                                height: rect.height - 10.0,
                            },
                            3.0,
                            theme.find_match,
                        );
                    }
                    layout::push_ui_text(out, atlas, text_rect, &hit.snippet, theme.sidebar_text);
                }
            }
        }
        layout::clip_hotspots(from, g.list);
    }
}

/// A field that looks like a field: a box, a ring while it has the keys,
/// its text or placeholder, with `trailing` points at its end kept clear.
#[allow(clippy::too_many_arguments)]
fn field(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    rect: Viewport,
    buffer: &Buffer,
    placeholder: &str,
    focused: bool,
    trailing: f32,
    theme: &Theme,
) {
    if rect.height <= 0.0 {
        return;
    }
    if focused {
        layout::push_focus_ring(out, rect, layout::UI_RADIUS, 1.5, theme);
    }
    layout::push_rounded_rect(out, rect, layout::UI_RADIUS, theme.tab_active);
    layout::hotspot(rect, layout::Cursor::Text, None);
    let text = buffer.rope.to_string();
    layout::push_ui_field(
        out,
        atlas,
        Viewport {
            x: rect.x + FIELD_PAD,
            width: (rect.width - FIELD_PAD * 2.0 - trailing).max(0.0),
            ..rect
        },
        (5.0, rect.height - 10.0),
        &layout::UiField {
            text: &text,
            cursor: buffer.cursor(),
            selection: buffer.selection(),
            placeholder,
            focused,
        },
        theme,
    );
}

/// The byte of `field`'s text under `x`, measured from the field's left
/// edge through the same shaping that drew it.
pub fn byte_at(atlas: &mut Atlas, field: &Buffer, x: f32) -> usize {
    let text = field.rope.to_string();
    let (shown, start) = layout::ui_input_window(&text, field.cursor());
    atlas
        .shape_ui(&shown)
        .map_or(text.len(), |line| start + line.byte_at_x(x - FIELD_PAD))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(path: &str, line: usize, snippet: &str) -> ProjectHit {
        ProjectHit {
            path: path.into(),
            range: 0..1,
            line,
            column: 0,
            snippet: snippet.to_string(),
            at: 0..1,
        }
    }

    fn hits() -> Vec<ProjectHit> {
        vec![
            hit("a.rs", 4, "let plant = plants[0];"),
            hit("a.rs", 4, "let plant = plants[0];"),
            hit("a.rs", 9, "plant"),
            hit("b.rs", 0, "plant"),
        ]
    }

    #[test]
    fn hits_are_listed_by_file_and_line() {
        let rows = group_hits(&hits());
        assert_eq!(
            rows,
            vec![
                Row::File { first: 0, count: 3 },
                Row::Line { hits: 0..2 },
                Row::Line { hits: 2..3 },
                Row::File { first: 3, count: 1 },
                Row::Line { hits: 3..4 },
            ]
        );
        assert_eq!(
            rows.iter().map(Row::hit).collect::<Vec<_>>(),
            [0, 0, 2, 3, 3]
        );
        assert!(group_hits(&[]).is_empty());
    }

    #[test]
    fn folding_a_file_hides_its_lines_and_the_arrows_skip_headings() {
        let mut panel = Panel::default();
        panel.take_results(hits());
        assert_eq!(panel.selected, 1, "the first line row");
        assert_eq!(panel.listed(), [0, 1, 2, 3, 4]);
        assert_eq!(panel.files(), 2);
        panel.toggle_file(0);
        assert_eq!(panel.listed(), [0, 3, 4]);
        panel.select_step(true, 10);
        assert_eq!(panel.selected, 4, "over the folded lines and the heading");
        panel.select_step(false, 10);
        assert_eq!(panel.selected, 4, "nothing listed before it");
        panel.toggle_file(0);
        assert_eq!(panel.listed(), [0, 1, 2, 3, 4]);
        panel.collapse_all();
        assert_eq!(panel.listed(), [0, 3]);
        panel.collapse_all();
        assert_eq!(panel.listed().len(), 5, "all folded: unfolds them all");
        panel.clear();
        assert!(panel.rows().is_empty() && panel.note().is_none());
    }

    #[test]
    fn the_list_follows_the_selection_and_the_wheel_stays_in_range() {
        let mut panel = Panel::default();
        panel.take_results(hits());
        panel.select_step(true, 2);
        assert_eq!((panel.selected, panel.scroll), (2, 1));
        panel.select_step(true, 2);
        assert_eq!((panel.selected, panel.scroll), (4, 3));
        panel.scroll_by(-10, 2);
        assert_eq!(panel.scroll, 0);
        panel.scroll_by(10, 2);
        assert_eq!(panel.scroll, 3);
    }

    #[test]
    fn the_note_counts_results_and_files() {
        let mut panel = Panel::default();
        assert_eq!(panel.note(), None);
        panel.query.insert("plant");
        panel.searching = true;
        assert_eq!(panel.note().as_deref(), Some("Searching\u{2026}"));
        panel.take_results(hits());
        assert_eq!(panel.note().as_deref(), Some("4 results in 2 files"));
        panel.take_results(vec![hit("a.rs", 1, "x")]);
        assert_eq!(panel.note().as_deref(), Some("1 result in 1 file"));
        panel.take_results(Vec::new());
        assert_eq!(panel.note().as_deref(), Some("No results"));
    }

    #[test]
    fn the_geometry_stacks_the_rows_it_shows() {
        let column = Viewport {
            x: 50.0,
            y: 48.0,
            width: 300.0,
            height: 700.0,
        };
        let plain = Geometry::new(column, false, false);
        assert_eq!(plain.replace.height, 0.0);
        assert_eq!(plain.include.height, 0.0);
        assert!(plain.list.y > plain.query.y + plain.query.height);
        assert_eq!(
            plain.field_at(plain.query.x + 1.0, plain.query.y + 1.0),
            Some(Field::Query)
        );
        assert_eq!(
            plain.field_at(plain.query.x + 1.0, plain.list.y + 1.0),
            None
        );
        assert!(plain.chips[2].x + plain.chips[2].width <= plain.query.x + plain.query.width);
        let full = Geometry::new(column, true, true);
        assert!(full.replace.y > full.query.y);
        assert!(full.include.y > full.replace.y);
        assert!(full.exclude.y > full.include.y);
        assert!(full.list.y > full.exclude.y);
        assert_eq!(
            full.field_at(full.exclude.x + 1.0, full.exclude.y + 1.0),
            Some(Field::Exclude)
        );
        assert!(full.visible_rows() < plain.visible_rows());
    }
}
