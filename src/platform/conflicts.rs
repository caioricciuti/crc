//! Merge conflicts in an open document, two ways of looking at them.
//!
//! Inline, the conflict stays in the file: its sections are washed in the
//! side's colour behind the text, and the `<<<<<<<` line carries the buttons
//! that resolve it. Everything else about the file is the editor as usual,
//! so a conflict can also be fixed by hand, undone, searched and completed.
//!
//! Side by side, the editor column shows the file as columns, current,
//! base when the markers carry one, and incoming, with each conflict's
//! sections next to each other and padded to the same height, so the text
//! after a conflict lines up again. The columns are drawn from the
//! document; they are a view of it, not a copy, and a click on one goes
//! back to the file at that line.
//!
//! Which documents have conflicts is found from their text
//! ([`crate::project::conflict`]), and which are unmerged from Git's status.
//! The strip above the text says how many are left and holds the mode
//! switch, Previous and Next, and Mark Resolved.
use crate::project::conflict::{self, Conflict, Row, Take};
use crate::render::{
    font::Atlas,
    layout::{self, Hit, Theme, Viewport},
    metal::GlyphInstance,
};
use crate::text::{buffer::Buffer, rope::Rope};
use std::collections::HashMap;

/// Bigger than this and a document is not scanned for markers: a merge
/// conflict is in source, and a hundred-megabyte log is not.
pub const MAX_BYTES: usize = 16 * 1024 * 1024;

/// What was found in one document, and against which text and which Git
/// status.
pub struct Scan {
    rope: Rope,
    /// Whether the text was unsaved when looked at: a save changes that
    /// and not the text.
    dirty: bool,
    generation: u64,
    pub view: Option<View>,
}

/// A document with conflicts, or one Git still lists as unmerged.
pub struct View {
    pub conflicts: Vec<Conflict>,
    /// Git lists the file as unmerged.
    pub unmerged: bool,
    /// The most conflicts seen at once, for "2 of 5 left".
    pub most: usize,
    /// Side-by-side rows, built when first drawn after a change.
    rows: Option<Vec<Row>>,
    /// The first side-by-side row on screen.
    pub scroll: usize,
}

impl View {
    /// Whether Mark Resolved can stage the file: Git has it unmerged and no
    /// markers are left in it.
    pub fn can_resolve(&self) -> bool {
        self.unmerged && self.conflicts.is_empty()
    }

    /// Whether any conflict carries its base, which adds the middle column.
    pub fn has_base(&self) -> bool {
        self.conflicts.iter().any(|c| c.base.is_some())
    }

    pub fn rows(&mut self, total_lines: usize) -> &[Row] {
        let conflicts = &self.conflicts;
        self.rows
            .get_or_insert_with(|| conflict::rows(conflicts, total_lines))
    }
}

/// Brings `scans` up to date for `buffer`.
///
/// A document with no conflicts is scanned when first seen, when Git's
/// status changes, and when its text changes while it is clean, which is a
/// save or a reload: typing into an ordinary file costs nothing. A document
/// with conflicts is scanned whenever its text changes.
pub fn sync(
    scans: &mut HashMap<u64, Scan>,
    buffer: &Buffer,
    generation: u64,
    unmerged: impl FnOnce(&std::path::Path) -> bool,
) {
    let id = buffer.id();
    let Some(path) = buffer.path.as_deref() else {
        scans.remove(&id);
        return;
    };
    if buffer.rope.len_bytes() > MAX_BYTES || buffer.is_preview_file() {
        scans.remove(&id);
        return;
    }
    let scan = scans.get(&id);
    let text_changed =
        scan.is_none_or(|s| !s.rope.same_as(&buffer.rope) || (s.dirty && !buffer.is_dirty()));
    let git_changed = scan.is_none_or(|s| s.generation != generation);
    if !text_changed && !git_changed {
        return;
    }
    let tracked = scan.is_some_and(|s| s.view.is_some());
    let rescan = scan.is_none() || git_changed || tracked || !buffer.is_dirty();
    if !rescan {
        // Typing in a file with no conflicts: remember the text, so the
        // next frame does not ask again.
        if let Some(scan) = scans.get_mut(&id) {
            scan.rope = buffer.rope.clone();
            scan.dirty = true;
        }
        return;
    }
    let conflicts = if text_changed || !tracked {
        conflict::parse(&buffer.rope)
    } else {
        scan.and_then(|s| s.view.as_ref())
            .map(|v| v.conflicts.clone())
            .unwrap_or_default()
    };
    let unmerged = if git_changed || scan.is_none() {
        unmerged(path)
    } else {
        scan.and_then(|s| s.view.as_ref())
            .is_some_and(|v| v.unmerged)
    };
    let previous = scans.remove(&id).and_then(|s| s.view);
    let view = (!conflicts.is_empty() || unmerged).then(|| {
        let (most, scroll) = previous.as_ref().map_or((0, 0), |v| (v.most, v.scroll));
        View {
            most: most.max(conflicts.len()),
            conflicts,
            unmerged,
            rows: None,
            scroll,
        }
    });
    scans.insert(
        id,
        Scan {
            rope: buffer.rope.clone(),
            dirty: buffer.is_dirty(),
            generation,
            view,
        },
    );
}

/// Drops what is kept for documents no longer open.
pub fn prune(scans: &mut HashMap<u64, Scan>, open: &[u64]) {
    if scans.len() > open.len() + 16 {
        scans.retain(|id, _| open.contains(id));
    }
}

// ---- the strip ------------------------------------------------------------

const MODES: [&str; 2] = ["Inline", "Side by side"];

/// Every control in the strip, where it is: mode segments, Previous, Next,
/// and Mark Resolved when Git has the file unmerged.
pub fn strip_hits(view: &View, atlas: &mut Atlas, strip: Viewport) -> Vec<(Hit, Viewport)> {
    let y = strip.y + 38.0;
    let mut x = strip.x + 16.0;
    let mut hits = Vec::new();
    for (index, label) in MODES.iter().enumerate() {
        let width = layout::ui_text_width(atlas, label) + 24.0;
        hits.push((
            Hit::ConflictMode(index == 1),
            Viewport {
                x,
                y,
                width,
                height: 26.0,
            },
        ));
        x += width;
    }
    x += 14.0;
    if !view.conflicts.is_empty() {
        for (forward, label) in [(false, "Previous"), (true, "Next")] {
            let width = layout::ui_text_width(atlas, label) + 24.0;
            hits.push((
                Hit::ConflictStep(forward),
                Viewport {
                    x,
                    y,
                    width,
                    height: 26.0,
                },
            ));
            x += width + 6.0;
        }
    }
    if view.unmerged {
        let label = "Mark Resolved";
        let width = layout::ui_text_width(atlas, label) + 28.0;
        let right = Viewport {
            x: (strip.x + strip.width - 16.0 - width).max(x),
            y,
            width,
            height: 26.0,
        };
        hits.push((Hit::ConflictResolve, right));
    }
    hits
}

/// What is left to do, the switch and the buttons.
#[allow(clippy::too_many_arguments)]
pub fn draw_strip(
    view: &View,
    side: bool,
    file: &str,
    dirty: bool,
    atlas: &mut Atlas,
    strip: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    let tone = if view.conflicts.is_empty() {
        theme.diff_added
    } else {
        theme.diff_removed
    };
    let pill_label = if view.conflicts.is_empty() {
        "Resolved"
    } else {
        "Conflicts"
    };
    let pill = Viewport {
        x: strip.x + 16.0,
        y: strip.y + 6.0,
        width: layout::ui_text_width(atlas, pill_label) + 20.0,
        height: 26.0,
    };
    layout::push_rounded_rect(out, pill, 6.0, [tone[0], tone[1], tone[2], 0.18]);
    layout::push_ui_text_centered(out, atlas, pill, pill_label, tone);
    let left = view.conflicts.len();
    let facts = match (left, view.unmerged) {
        (0, true) => format!(
            "No markers left in {file}. Mark Resolved {} it for the commit.",
            if dirty { "saves and stages" } else { "stages" }
        ),
        (0, false) => format!("No markers left in {file}."),
        (n, _) => {
            let mut facts = if n == 1 {
                format!("1 conflict in {file}")
            } else {
                format!("{n} conflicts in {file}")
            };
            if view.most > n {
                facts.push_str(&format!(", {} resolved", view.most - n));
            }
            if let Some(first) = view.conflicts.first() {
                facts.push_str(&format!(
                    "   Current: {}   Incoming: {}",
                    or_unnamed(&first.current_label),
                    or_unnamed(&first.incoming_label)
                ));
            }
            facts
        }
    };
    layout::push_ui_text(
        out,
        atlas,
        Viewport {
            x: pill.x + pill.width + 12.0,
            y: pill.y,
            width: (strip.x + strip.width - pill.x - pill.width - 28.0).max(0.0),
            height: pill.height,
        },
        &facts,
        theme.status_text,
    );
    let hits = strip_hits(view, atlas, strip);
    // The two modes read as one control: a track, and the chosen half lit.
    let modes: Vec<Viewport> = hits
        .iter()
        .filter(|(h, _)| matches!(h, Hit::ConflictMode(_)))
        .map(|(_, r)| *r)
        .collect();
    if let (Some(first), Some(last)) = (modes.first(), modes.last()) {
        layout::push_rounded_rect(
            out,
            Viewport {
                width: last.x + last.width - first.x,
                ..*first
            },
            6.0,
            theme.tab_hover,
        );
    }
    for (hit, rect) in hits {
        match hit {
            Hit::ConflictMode(is_side) => {
                let chosen = is_side == side;
                if chosen {
                    layout::push_rounded_rect(
                        out,
                        Viewport {
                            x: rect.x + 2.0,
                            y: rect.y + 2.0,
                            width: rect.width - 4.0,
                            height: rect.height - 4.0,
                        },
                        5.0,
                        theme.palette_selected,
                    );
                }
                layout::push_ui_text_centered(
                    out,
                    atlas,
                    rect,
                    MODES[usize::from(is_side)],
                    if chosen {
                        theme.text
                    } else {
                        theme.status_text
                    },
                );
            }
            Hit::ConflictStep(forward) => {
                layout::push_rounded_rect(out, rect, 6.0, theme.tab_hover);
                layout::push_ui_text_centered(
                    out,
                    atlas,
                    rect,
                    if forward { "Next" } else { "Previous" },
                    theme.status_text,
                );
            }
            Hit::ConflictResolve => {
                let can = view.can_resolve();
                let accent = theme.accent;
                layout::push_rounded_rect(
                    out,
                    rect,
                    6.0,
                    if can {
                        [accent[0], accent[1], accent[2], 0.22]
                    } else {
                        theme.tab_hover
                    },
                );
                layout::push_ui_text_centered(
                    out,
                    atlas,
                    rect,
                    "Mark Resolved",
                    if can { theme.text } else { theme.gutter_text },
                );
            }
            _ => {}
        }
    }
    layout::push_rect(
        out,
        atlas,
        [strip.x, strip.y + strip.height - 1.0],
        [strip.width, 1.0],
        theme.hairline,
    );
}

fn or_unnamed(label: &str) -> &str {
    if label.is_empty() { "unnamed" } else { label }
}

// ---- inline ---------------------------------------------------------------

/// The colour a side is drawn in.
fn side_colour(theme: &Theme, take: Take) -> [f32; 4] {
    match take {
        Take::Current => theme.diff_added,
        Take::Incoming => theme.conflict_incoming,
        Take::Both => theme.accent,
        Take::Base => theme.gutter_text,
    }
}

fn wash(colour: [f32; 4], alpha: f32) -> [f32; 4] {
    [colour[0], colour[1], colour[2], alpha]
}

/// Where the band on `line` comes from, if it is in a conflict: the side,
/// and whether the line is a marker.
fn band_of(conflict: &Conflict, line: usize) -> Option<(Take, bool)> {
    if line == conflict.start_line {
        Some((Take::Current, true))
    } else if line == conflict.end_line {
        Some((Take::Incoming, true))
    } else if Some(line) == conflict.base_line || line == conflict.separator_line {
        Some((Take::Base, true))
    } else if conflict.current_lines().contains(&line) {
        Some((Take::Current, false))
    } else if conflict.incoming_lines().contains(&line) {
        Some((Take::Incoming, false))
    } else if conflict.base_lines().is_some_and(|r| r.contains(&line)) {
        Some((Take::Base, false))
    } else {
        None
    }
}

/// Washes behind each conflict's rows, to go under the text: markers
/// stronger than the sections they bound.
pub fn bands(
    view: &View,
    buffer: &Buffer,
    atlas: &Atlas,
    text: Viewport,
    theme: &Theme,
) -> Vec<GlyphInstance> {
    let m = atlas.metrics;
    let mut out = Vec::new();
    if view.conflicts.is_empty() {
        return out;
    }
    for row in layout::screen_rows(buffer, text, m.line_height) {
        let Some(index) = conflict::at_line(&view.conflicts, row.line) else {
            continue;
        };
        let Some((take, marker)) = band_of(&view.conflicts[index], row.line) else {
            continue;
        };
        let alpha = match (take, marker) {
            (Take::Base, true) => 0.16,
            (Take::Base, false) => 0.08,
            (_, true) => 0.30,
            _ => 0.13,
        };
        layout::push_rect(
            &mut out,
            atlas,
            [text.x, row.y],
            [text.width, m.line_height],
            wash(side_colour(theme, take), alpha),
        );
        // A bar in the gutter's first cell, so the extent of a section
        // shows even where its text is short.
        layout::push_rect(
            &mut out,
            atlas,
            [text.x + 2.0, row.y],
            [3.0, m.line_height],
            wash(side_colour(theme, take), if marker { 0.9 } else { 0.55 }),
        );
    }
    for quad in &mut out {
        layout::clip_vertical(quad, text.y, text.y + text.height);
    }
    out
}

/// Short labels on the marker line, where the words "Accept" would repeat.
fn inline_label(take: Take) -> &'static str {
    match take {
        Take::Current => "Accept Current",
        Take::Incoming => "Accept Incoming",
        Take::Both => "Accept Both",
        Take::Base => "Accept Base",
    }
}

/// The resolve buttons on each visible `<<<<<<<` line, after its text.
pub fn inline_hits(
    view: &View,
    buffer: &Buffer,
    atlas: &mut Atlas,
    text: Viewport,
) -> Vec<(Hit, Viewport)> {
    let m = atlas.metrics;
    let mut hits = Vec::new();
    if view.conflicts.is_empty() {
        return hits;
    }
    let text_x = text.x + layout::gutter_width(buffer, atlas);
    let right = text.x + text.width - 8.0;
    for row in layout::screen_rows(buffer, text, m.line_height) {
        if !row.first || row.y < text.y || row.y + m.line_height > text.y + text.height {
            continue;
        }
        let Ok(index) = view
            .conflicts
            .binary_search_by_key(&row.line, |c| c.start_line)
        else {
            continue;
        };
        let conflict = &view.conflicts[index];
        let marker_chars = buffer.rope.line(row.line).trim_end().chars().count();
        let mut x = (text_x
            + (marker_chars as f32 + 3.0 - buffer.scroll_column as f32) * m.advance)
            .max(text_x + 4.0);
        for take in Take::ALL {
            if !conflict.offers(take) {
                continue;
            }
            let width = layout::ui_text_width(atlas, inline_label(take)) + 16.0;
            if x + width > right {
                break;
            }
            hits.push((
                Hit::ConflictTake(index, take),
                Viewport {
                    x,
                    y: row.y + 1.0,
                    width,
                    height: m.line_height - 2.0,
                },
            ));
            x += width + 4.0;
        }
    }
    hits
}

/// The buttons from [`inline_hits`], drawn over the text.
pub fn draw_inline_buttons(
    hits: &[(Hit, Viewport)],
    atlas: &mut Atlas,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    for (hit, rect) in hits {
        if let Hit::ConflictTake(_, take) = hit {
            let colour = side_colour(theme, *take);
            layout::push_rounded_rect(out, *rect, 4.0, theme.tab_active);
            layout::push_rounded_rect(out, *rect, 4.0, wash(colour, 0.20));
            layout::push_ui_text_centered(
                out,
                atlas,
                *rect,
                inline_label(*take),
                if *take == Take::Base {
                    theme.status_text
                } else {
                    colour
                },
            );
        }
    }
}

// ---- side by side -----------------------------------------------------------

/// Row height of the columns, the diff view's.
pub const SIDE_LINE: f32 = crate::platform::git_panel::DIFF_LINE;
/// The row naming each column, above the scrolling rows.
const SIDE_HEADER: f32 = 28.0;
/// Line numbers, per column.
const SIDE_GUTTER: f32 = 48.0;

/// The columns, left to right, and the side each shows.
fn columns(view: &View, rect: Viewport) -> Vec<(Take, Viewport)> {
    let sides: &[Take] = if view.has_base() {
        &[Take::Current, Take::Base, Take::Incoming]
    } else {
        &[Take::Current, Take::Incoming]
    };
    let width = (rect.width / sides.len() as f32).floor();
    sides
        .iter()
        .enumerate()
        .map(|(i, take)| {
            (
                *take,
                Viewport {
                    x: rect.x + i as f32 * width,
                    width: if i + 1 == sides.len() {
                        rect.width - i as f32 * width
                    } else {
                        width
                    },
                    ..rect
                },
            )
        })
        .collect()
}

/// Where the rows go, under the column names.
fn side_body(rect: Viewport) -> Viewport {
    Viewport {
        y: rect.y + SIDE_HEADER,
        height: (rect.height - SIDE_HEADER).max(0.0),
        ..rect
    }
}

/// How many rows fit.
pub fn side_rows_visible(rect: Viewport) -> usize {
    (side_body(rect).height / SIDE_LINE).max(1.0) as usize
}

/// Keeps the scroll inside the rows.
pub fn clamp_side_scroll(view: &mut View, total_lines: usize, rect: Viewport) {
    let count = view.rows(total_lines).len();
    view.scroll = view
        .scroll
        .min(count.saturating_sub(side_rows_visible(rect)));
}

/// Puts conflict `index` near the top of the columns.
pub fn show_in_side(view: &mut View, total_lines: usize, index: usize, rect: Viewport) {
    if let Some(at) = conflict::header_row(view.rows(total_lines), index) {
        view.scroll = at.saturating_sub(2);
        clamp_side_scroll(view, total_lines, rect);
    }
}

/// Each conflict's buttons in its header row: one per column, and Accept
/// Both at the end of the last.
pub fn side_hits(
    view: &mut View,
    total_lines: usize,
    atlas: &mut Atlas,
    rect: Viewport,
) -> Vec<(Hit, Viewport)> {
    let body = side_body(rect);
    let visible = side_rows_visible(rect);
    let cols = columns(view, rect);
    let scroll = view.scroll;
    let mut hits = Vec::new();
    let headers: Vec<(usize, usize)> = view
        .rows(total_lines)
        .iter()
        .enumerate()
        .skip(scroll)
        .take(visible)
        .filter_map(|(at, row)| match row {
            Row::Header { index } => Some((at - scroll, *index)),
            _ => None,
        })
        .collect();
    for (visible_row, index) in headers {
        let y = body.y + visible_row as f32 * SIDE_LINE + 1.0;
        for (take, column) in &cols {
            let width = layout::ui_text_width(atlas, take.label()) + 16.0;
            let x = column.x + SIDE_GUTTER;
            if x + width <= column.x + column.width - 4.0 {
                hits.push((
                    Hit::ConflictTake(index, *take),
                    Viewport {
                        x,
                        y,
                        width,
                        height: SIDE_LINE - 2.0,
                    },
                ));
            }
        }
        if let Some((_, last)) = cols.last() {
            let width = layout::ui_text_width(atlas, Take::Both.label()) + 16.0;
            let x = last.x + last.width - width - 8.0;
            let taken = hits
                .iter()
                .rev()
                .find(|(h, _)| matches!(h, Hit::ConflictTake(i, _) if *i == index))
                .map_or(last.x, |(_, r)| r.x + r.width);
            if x > taken + 8.0 {
                hits.push((
                    Hit::ConflictTake(index, Take::Both),
                    Viewport {
                        x,
                        y,
                        width,
                        height: SIDE_LINE - 2.0,
                    },
                ));
            }
        }
    }
    hits
}

/// The document line under a point in the columns: the line a click there
/// goes back to in the file.
pub fn side_line_at(
    view: &mut View,
    total_lines: usize,
    rect: Viewport,
    x: f32,
    y: f32,
) -> Option<usize> {
    let body = side_body(rect);
    if !body.contains(x, y) {
        return None;
    }
    let cols = columns(view, rect);
    let take = cols
        .iter()
        .find(|(_, c)| c.contains(x, y))
        .map(|(t, _)| *t)?;
    let at = view.scroll + ((y - body.y) / SIDE_LINE) as usize;
    let row = *view.rows(total_lines).get(at)?;
    let conflicts_start = |index: usize| view.conflicts.get(index).map(|c| c.start_line);
    match row {
        Row::Common { current, .. } => Some(current.line),
        Row::Header { index } => conflicts_start(index),
        Row::Side {
            index,
            current,
            base,
            incoming,
        } => match take {
            Take::Current => current.map(|c| c.line),
            Take::Base => base.map(|c| c.line),
            _ => incoming.map(|c| c.line),
        }
        .or_else(|| conflicts_start(index)),
    }
}

/// The document as columns.
pub fn draw_side(
    view: &mut View,
    rope: &Rope,
    atlas: &mut Atlas,
    rect: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    let total_lines = rope.len_lines();
    clamp_side_scroll(view, total_lines, rect);
    let cols = columns(view, rect);
    let body = side_body(rect);
    let visible = side_rows_visible(rect);
    let labels: Vec<String> = cols
        .iter()
        .map(|(take, _)| {
            let first = view.conflicts.first();
            let name = match take {
                Take::Current => first.map(|c| c.current_label.clone()),
                Take::Incoming => first.map(|c| c.incoming_label.clone()),
                _ => first.and_then(|c| c.base_label.clone()),
            }
            .unwrap_or_default();
            let side = match take {
                Take::Current => "Current",
                Take::Incoming => "Incoming",
                _ => "Base",
            };
            if name.is_empty() {
                side.to_owned()
            } else {
                format!("{side}  {name}")
            }
        })
        .collect();
    // Column names.
    for ((take, column), label) in cols.iter().zip(&labels) {
        let colour = side_colour(theme, *take);
        layout::push_rect(
            out,
            atlas,
            [column.x, rect.y],
            [column.width, SIDE_HEADER - 1.0],
            wash(colour, 0.10),
        );
        layout::push_ui_text(
            out,
            atlas,
            Viewport {
                x: column.x + 12.0,
                y: rect.y + 1.0,
                width: (column.width - 24.0).max(0.0),
                height: SIDE_HEADER - 2.0,
            },
            label,
            if *take == Take::Base {
                theme.status_text
            } else {
                colour
            },
        );
    }
    layout::push_rect(
        out,
        atlas,
        [rect.x, rect.y + SIDE_HEADER - 1.0],
        [rect.width, 1.0],
        theme.hairline,
    );
    if view.conflicts.is_empty() {
        layout::push_ui_text(
            out,
            atlas,
            Viewport {
                x: body.x + 16.0,
                y: body.y + 8.0,
                width: (body.width - 32.0).max(0.0),
                height: SIDE_LINE,
            },
            "No conflicts left in this file",
            theme.status_text,
        );
        return;
    }
    let (cell_w, _) = atlas.cell_size();
    let scroll = view.scroll;
    let rows: Vec<Row> = view
        .rows(total_lines)
        .iter()
        .skip(scroll)
        .take(visible)
        .copied()
        .collect();
    for (n, row) in rows.iter().enumerate() {
        let y = body.y + n as f32 * SIDE_LINE;
        for (take, column) in &cols {
            let cell = match *row {
                Row::Common { current, incoming } => Some(match take {
                    Take::Incoming => incoming,
                    _ => current,
                }),
                Row::Header { index } => {
                    // A rule across the column, and the conflict's number
                    // where the buttons leave room for it.
                    layout::push_rect(
                        out,
                        atlas,
                        [column.x, y],
                        [column.width, SIDE_LINE],
                        wash(side_colour(theme, *take), 0.22),
                    );
                    if *take == Take::Current {
                        layout::push_ui_text_right(
                            out,
                            atlas,
                            Viewport {
                                x: column.x,
                                y,
                                width: SIDE_GUTTER - 8.0,
                                height: SIDE_LINE,
                            },
                            &format!("#{}", index + 1),
                            theme.gutter_text,
                        );
                    }
                    continue;
                }
                Row::Side {
                    current,
                    base,
                    incoming,
                    ..
                } => {
                    let cell = match take {
                        Take::Current => current,
                        Take::Base => base,
                        _ => incoming,
                    };
                    layout::push_rect(
                        out,
                        atlas,
                        [column.x, y],
                        [column.width, SIDE_LINE],
                        match cell {
                            Some(_) => wash(side_colour(theme, *take), 0.13),
                            // Padding: this side has fewer lines here.
                            None => wash(theme.gutter_text, 0.05),
                        },
                    );
                    cell
                }
            };
            let Some(cell) = cell else {
                continue;
            };
            if !matches!(row, Row::Common { .. }) || *take != Take::Base {
                layout::push_ui_text_right(
                    out,
                    atlas,
                    Viewport {
                        x: column.x,
                        y,
                        width: SIDE_GUTTER - 10.0,
                        height: SIDE_LINE,
                    },
                    &cell.number.to_string(),
                    theme.gutter_text,
                );
            }
            let columns_fit = ((column.width - SIDE_GUTTER - 8.0).max(0.0) / cell_w) as usize;
            let mut width = 0;
            let text: String = rope
                .line(cell.line)
                .trim_end_matches(['\n', '\r'])
                .chars()
                .flat_map(|ch| if ch == '\t' { vec![' '; 4] } else { vec![ch] })
                .take_while(|ch| {
                    width += crate::render::font::display_width(*ch);
                    width <= columns_fit
                })
                .collect();
            layout::push_text(
                out,
                atlas,
                column.x + SIDE_GUTTER,
                y,
                &text,
                if matches!(row, Row::Common { .. }) {
                    theme.sidebar_text
                } else {
                    theme.text
                },
            );
        }
    }
    // Column rules, over the bands.
    for (_, column) in cols.iter().skip(1) {
        layout::push_rect(
            out,
            atlas,
            [column.x, rect.y],
            [1.0, rect.height],
            theme.divider,
        );
    }
    let hits = side_hits(view, total_lines, atlas, rect);
    for (hit, button) in hits {
        if let Hit::ConflictTake(_, take) = hit {
            let colour = side_colour(theme, take);
            layout::push_rounded_rect(out, button, 4.0, theme.tab_active);
            layout::push_rounded_rect(out, button, 4.0, wash(colour, 0.24));
            layout::push_ui_text_centered(
                out,
                atlas,
                button,
                take.label(),
                if take == Take::Base {
                    theme.status_text
                } else {
                    colour
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(text: &str) -> Buffer {
        let dir = std::env::temp_dir().join(format!("crc-conflicts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("f{}.txt", text.len()));
        std::fs::write(&path, text).unwrap();
        Buffer::open(path).unwrap()
    }

    const TEXT: &str = "a\n<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> b\nz\n";

    #[test]
    fn sync_finds_conflicts_and_follows_git() {
        let b = buffer(TEXT);
        let mut scans = HashMap::new();
        sync(&mut scans, &b, 1, |_| true);
        let view = scans[&b.id()].view.as_ref().unwrap();
        assert_eq!(view.conflicts.len(), 1);
        assert!(view.unmerged && !view.can_resolve());
        // Nothing changed: the Git check is not asked again.
        sync(&mut scans, &b, 1, |_| panic!("asked again"));
        // Git's answer changed: asked.
        sync(&mut scans, &b, 2, |_| false);
        assert!(!scans[&b.id()].view.as_ref().unwrap().unmerged);
    }

    #[test]
    fn a_file_without_conflicts_is_not_scanned_while_typing() {
        let mut b = buffer("plain\n");
        let mut scans = HashMap::new();
        sync(&mut scans, &b, 1, |_| false);
        assert!(scans[&b.id()].view.is_none());
        b.insert("<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> b\n");
        sync(&mut scans, &b, 1, |_| false);
        assert!(scans[&b.id()].view.is_none(), "dirty text is not scanned");
        b.save(None).unwrap();
        sync(&mut scans, &b, 1, |_| false);
        assert!(scans[&b.id()].view.is_some(), "a save is");
    }

    #[test]
    fn resolving_every_conflict_leaves_the_view_until_git_agrees() {
        let mut b = buffer(TEXT);
        let mut scans = HashMap::new();
        sync(&mut scans, &b, 1, |_| true);
        let c = scans[&b.id()].view.as_ref().unwrap().conflicts[0].clone();
        let text = c.resolution(&b.rope, Take::Incoming);
        b.replace_ranges(&[(c.range.clone(), text)]);
        sync(&mut scans, &b, 1, |_| true);
        let view = scans[&b.id()].view.as_ref().unwrap();
        assert!(view.conflicts.is_empty() && view.can_resolve());
        assert_eq!(view.most, 1);
        assert_eq!(b.rope.to_string(), "a\ny\nz\n");
        sync(&mut scans, &b, 2, |_| false);
        assert!(scans[&b.id()].view.is_none(), "staged: nothing to show");
    }
}
