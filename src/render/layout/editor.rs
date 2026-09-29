//! The text itself: rows on screen, glyphs, carets, selections, gutter
//! marks, underlines, the completion ribbon and the signature panel.

use super::*;

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Quads emitted, cursor and highlights included.
    pub quads: usize,
    /// Lines actually laid out.
    pub lines: usize,
    /// Characters skipped because they are outside the ASCII fast path.
    pub unsupported: usize,
    /// Lines past the shaping limit that are not plain ASCII, so they are
    /// drawn a character at a time: accents and scripts may look wrong.
    pub unshaped: usize,
}

/// Lays out the visible region into `out`, **replacing its contents**.
///
/// Note the replacing. This clears `out` first, so it must be the *first*
/// thing drawn in a frame: anything appended before it is silently erased.
/// That is exactly how the tab bar came to be invisible for several commits
/// while producing quads perfectly happily every frame.
pub fn build(
    buffer: &Buffer,
    atlas: &mut Atlas,
    viewport: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) -> Stats {
    build_with_matches(buffer, atlas, viewport, theme, "", out)
}

/// As [`build`], additionally highlighting occurrences of `query`.
///
/// Matches are computed only for the visible byte range, so a search in a
/// 100MB file costs the same as one in a small file.
pub fn build_with_matches(
    buffer: &Buffer,
    atlas: &mut Atlas,
    viewport: Viewport,
    theme: &Theme,
    query: &str,
    out: &mut Vec<GlyphInstance>,
) -> Stats {
    build_full(buffer, atlas, viewport, theme, query, &[], out)
}

/// As [`build_with_matches`], additionally colouring by syntax `spans`.
///
/// Spans must be sorted by start, which `Highlighter::spans` guarantees.
/// Walking them alongside the text means colouring costs one cursor advance
/// per character rather than a lookup per character.
pub fn build_full(
    buffer: &Buffer,
    atlas: &mut Atlas,
    viewport: Viewport,
    theme: &Theme,
    query: &str,
    spans: &[Span],
    out: &mut Vec<GlyphInstance>,
) -> Stats {
    let rows = screen_rows(buffer, viewport, atlas.metrics.line_height);
    build_full_search(
        buffer,
        atlas,
        viewport,
        &rows,
        theme,
        query,
        None,
        spans,
        &Markdown::default(),
        true,
        out,
    )
}

/// Draws search highlights supplied as UTF-8 byte ranges. This is used for
/// regex, whole-word and case-insensitive matches; `None` uses literal find.
/// `carets` false leaves the carets out: the off half of a blink.
#[allow(clippy::too_many_arguments)]
pub fn build_full_search(
    buffer: &Buffer,
    atlas: &mut Atlas,
    viewport: Viewport,
    rows: &[ScreenRow],
    theme: &Theme,
    query: &str,
    search_ranges: Option<&[std::ops::Range<usize>]>,
    spans: &[Span],
    markdown: &Markdown,
    carets: bool,
    out: &mut Vec<GlyphInstance>,
) -> Stats {
    out.clear();
    atlas.begin_frame();
    build_text_appending(
        buffer,
        atlas,
        viewport,
        rows,
        theme,
        query,
        search_ranges,
        spans,
        markdown,
        true,
        carets,
        out,
    )
}

/// [`build_full_search`] without clearing the frame first: a second editor
/// pane drawn after the first. `focused` draws the carets and the current
/// line band; a pane without the keyboard shows neither. `carets` false
/// hides the carets of a focused pane for the off half of a blink.
#[allow(clippy::too_many_arguments)]
pub fn build_text_appending(
    buffer: &Buffer,
    atlas: &mut Atlas,
    viewport: Viewport,
    screen: &[ScreenRow],
    theme: &Theme,
    query: &str,
    search_ranges: Option<&[std::ops::Range<usize>]>,
    spans: &[Span],
    markdown: &Markdown,
    focused: bool,
    carets: bool,
    out: &mut Vec<GlyphInstance>,
) -> Stats {
    let m = atlas.metrics;
    let (cell_w, cell_h) = atlas.cell_size();
    let solid = atlas.solid_uv();
    // The row is taller than the glyph cell now, so glyphs are centred in it
    // while bands and highlights still span the whole row.
    let glyph_dy = m.glyph_dy(m.line_height);

    let rows = viewport.rows(m.line_height);
    if rows == 0 {
        return Stats::default();
    }

    let total_lines = buffer.rope.len_lines();
    let std::ops::Range {
        start: first,
        end: last,
    } = visible_lines(buffer, viewport, m.line_height);
    let offset = scroll_offset(buffer, m.line_height);
    let drawn_from = out.len();

    // Gutter is sized to the widest line number plus breathing room, so it
    // does not jitter as you scroll between 999 and 1000.
    let digits = digit_count(total_lines);
    let gutter_w = (digits as f32 + 2.0) * m.advance;
    let text_x = viewport.x + gutter_w;

    let (cursor_line, _) = buffer.cursor_position();
    // Everything in the text area is shifted left by the horizontal scroll.
    // The gutter deliberately is not: line numbers stay pinned.
    let scroll_x = buffer.scroll_column as f32 * m.advance;
    let mut stats = Stats::default();

    // Every selection, not just the primary: with multiple cursors each one
    // needs its own band.
    let selections: Vec<(usize, usize)> = buffer
        .selections()
        .into_iter()
        .filter(|(s, e)| e > s)
        .collect();
    let selection = buffer.selection();

    // Occurrences of the search query inside the visible range only.
    let visible_start = buffer.rope.line_to_byte(first);
    let visible_end = if last < total_lines {
        buffer.rope.line_to_byte(last)
    } else {
        buffer.rope.len_bytes()
    };
    let matches: Vec<std::ops::Range<usize>> = if let Some(ranges) = search_ranges {
        ranges
            .iter()
            .filter(|r| r.end > visible_start && r.start < visible_end)
            .cloned()
            .collect()
    } else if query.is_empty() {
        Vec::new()
    } else if !buffer.folds.is_empty() {
        // Line by line: the span from the first to the last row can hold
        // everything a fold hides.
        let mut lines: Vec<usize> = screen.iter().map(|r| r.line).collect();
        lines.dedup();
        lines
            .into_iter()
            .flat_map(|l| {
                let start = buffer.rope.line_to_byte(l);
                let end = buffer.rope.line_range(l).end;
                buffer.rope.find_in(query, start..end)
            })
            .map(|at| at..at + query.len())
            .collect()
    } else {
        buffer
            .rope
            .find_in(query, visible_start..visible_end)
            .into_iter()
            .map(|at| at..at + query.len())
            .collect()
    };
    let mut shaped_carets = Vec::new();

    // Indent guides: one hairline per indent level, and blank lines carry
    // the guides of the block they sit in. The level is read from the
    // lines on screen, so a two-space file gets guides at two.
    // Only lines with a row on screen: a fold can put a million hidden
    // lines between the first and the last.
    let rows_on_screen = screen;
    let mut shown_lines: Vec<usize> = rows_on_screen.iter().map(|r| r.line).collect();
    shown_lines.dedup();
    let indents: Vec<Option<usize>> = shown_lines
        .iter()
        .map(|&l| buffer.indent_columns(l))
        .collect();
    let unit = indent_unit(&indents);
    // Which visible lines could fold, from the indents already read: the
    // next non-blank line is deeper. Only a line with nothing non-blank
    // below it on screen asks the buffer, which reads further down.
    let foldable: Vec<bool> = (0..indents.len())
        .map(|i| match indents[i] {
            None => false,
            Some(base) => match indents[i + 1..].iter().flatten().next() {
                Some(next) => *next > base,
                None => buffer.can_fold(shown_lines[i]),
            },
        })
        .collect();
    let bracket = if focused { bracket_match(buffer) } else { None };
    let wrapping = buffer.wrap.is_some();
    let touched = touched_lines(buffer);
    let _ = (offset, first);

    for (row_index, row) in rows_on_screen.iter().enumerate() {
        let line = row.line;
        let y = row.y;

        let line_start = buffer.rope.line_to_byte(line);
        let line_end = buffer.rope.line_range(line).end;

        // Shape bounded non-ASCII lines. The cached CoreText offsets are
        // shared by text, selection, search bands, and the caret.
        let shaped_line =
            atlas.shape_editor_line((buffer.id(), line), &buffer.rope, line_start..line_end);
        // A shaped line is laid out whole, in visual order; a row cut out of
        // it is wrong once a right-to-left run crosses the break. Such a
        // line is drawn cell by cell while it wraps.
        let shaped_line = shaped_line
            .filter(|_| !(wrapping && !(row.first && row.last) && has_rtl(buffer, line)));
        if shaped_line.is_none()
            && line_end - line_start > crate::render::font::MAX_SHAPED_LINE_BYTES
            && buffer.rope.byte_to_char(line_end) - buffer.rope.byte_to_char(line_start)
                != line_end - line_start
        {
            stats.unshaped += 1;
        }
        // Markdown off the caret: syntax hidden, table cells padded.
        let map = if shaped_line.is_none() {
            markdown.line(buffer, &touched, line)
        } else {
            None
        };
        let offset_at = |byte: usize| -> f32 {
            if let Some(shaped) = &shaped_line {
                shaped.x_of_byte(byte.saturating_sub(line_start))
            } else if let Some(map) = &map {
                map.column(&buffer.rope, byte) * m.advance
            } else {
                buffer.rope.visual_column(line_start..byte) as f32 * m.advance
            }
        };
        // A wrapped row is drawn as if the line began at the row's start.
        // Without wrapping this is the horizontal scroll alone.
        let row_x0 = if wrapping && !row.first {
            offset_at(row.start)
        } else {
            0.0
        };
        let scroll_x = scroll_x + row_x0;
        // What of the line this row shows, for bands and highlights.
        let (row_start, row_end) = (row.start, row.end);

        // Current-line highlight, behind everything on this row. Suppressed
        // while there is a selection: two overlapping washes on the same row
        // read as a rendering bug rather than as two pieces of information.
        // Any cursor's selection counts, not only the primary's: an extra
        // cursor's range on this row would draw its band over the wash.
        let selected_here = selections
            .iter()
            .any(|&(a, b)| a < b && a <= row_end && b >= row_start);
        if focused && line == cursor_line && selection.is_none() && !selected_here {
            out.push(GlyphInstance {
                pos: [viewport.x, y],
                size: [viewport.width, m.line_height],
                uv: solid,
                color: theme.current_line,
                ..Default::default()
            });
        }

        let shown_at = shown_lines.binary_search(&line).unwrap_or(0);
        let indent = match indents[shown_at] {
            Some(columns) => columns,
            None => blank_line_indent(buffer, line, &shown_lines, &indents),
        };
        let indent = if row.first { indent } else { 0 };
        for level in (0..indent).step_by(unit) {
            let x = text_x + level as f32 * m.advance - scroll_x;
            if x < text_x || x > viewport.x + viewport.width {
                continue;
            }
            out.push(GlyphInstance {
                pos: [x, y],
                size: [1.0, m.line_height],
                uv: solid,
                color: theme.indent_guide,
                ..Default::default()
            });
        }

        if let Some((open, close)) = bracket {
            for at in [open, close] {
                if at < row_start || at >= row_end {
                    continue;
                }
                let x = text_x + offset_at(at) - scroll_x;
                if x < text_x || x > viewport.x + viewport.width {
                    continue;
                }
                out.push(GlyphInstance {
                    pos: [x, y],
                    size: [m.advance, m.line_height],
                    uv: solid,
                    color: theme.bracket_match,
                    ..Default::default()
                });
            }
        }

        // Markdown: a band behind a code block's rows, and a tint behind
        // inline code, under the selection like the current line's wash.
        if markdown
            .bands
            .iter()
            .any(|b| b.start < row_end.max(row_start + 1) && b.end >= row_start)
        {
            out.push(GlyphInstance {
                pos: [text_x, y],
                size: [
                    (viewport.x + viewport.width - text_x).max(0.0),
                    m.line_height,
                ],
                uv: solid,
                color: theme.md_code_background,
                ..Default::default()
            });
        }
        let first_span = spans.partition_point(|s| s.end <= row_start);
        for span in spans[first_span..]
            .iter()
            .take_while(|s| s.start < row_end)
            .filter(|s| s.kind == Kind::MdCode)
        {
            let x0 = text_x + offset_at(span.start.max(row_start)) - scroll_x - 2.0;
            let x1 = text_x + offset_at(span.end.min(row_end)) - scroll_x + 2.0;
            let x0 = x0.max(text_x);
            let x1 = x1.min(viewport.x + viewport.width);
            if x1 > x0 {
                push_rounded_rect(
                    out,
                    Viewport {
                        x: x0,
                        y: y + 2.0,
                        width: x1 - x0,
                        height: m.line_height - 4.0,
                    },
                    3.0,
                    theme.md_code_background,
                );
            }
        }

        // One band over `from..to` of this row, clipped to the text area:
        // `extend` more at its end (half a cell marks a selected newline).
        let right_edge = viewport.x + viewport.width;
        let band = |out: &mut Vec<GlyphInstance>, from: usize, to: usize, extend: f32, color| {
            let intervals = if let Some(shaped) = &shaped_line {
                shaped.selection_intervals(
                    from - line_start,
                    to - line_start,
                    scroll_x..scroll_x + right_edge - text_x,
                    extend,
                )
            } else {
                vec![(offset_at(from), offset_at(to) + extend)]
            };
            for (left, right) in intervals {
                let x0 = text_x + left.min(right) - scroll_x;
                let clipped = x0.max(text_x);
                let width = (right - left).abs() - (clipped - x0);
                if width > 0.0 && clipped < right_edge {
                    out.push(GlyphInstance {
                        pos: [clipped, y],
                        size: [width.min(right_edge - clipped), m.line_height],
                        uv: solid,
                        color,
                        ..Default::default()
                    });
                }
            }
        };

        // Selection bands for this row, also behind the text.
        // The last row of a line ends past its newline; the newline is the
        // byte before `line_end`, and the last line of the file has none.
        let newline = (row.last && line + 1 < total_lines).then(|| line_end - 1);
        for &(sel_start, sel_end) in &selections {
            // Half a cell after the text marks a selected newline, so a
            // selection that takes whole lines shows it does, down to one
            // ending at the start of the next line (Shift-Down from column 0).
            let newline_selected = newline.is_some_and(|nl| sel_start <= nl && sel_end > nl);
            let from = sel_start.max(row_start);
            let to = sel_end.min(newline.unwrap_or(row_end));
            // The newline alone: an empty line in a block, or a selection
            // that starts at a line's end.
            if from >= to && newline_selected {
                let x0 = text_x + offset_at(line_end) - scroll_x;
                if x0 >= text_x && x0 < viewport.x + viewport.width {
                    out.push(GlyphInstance {
                        pos: [x0, y],
                        size: [m.advance * 0.5, m.line_height],
                        uv: solid,
                        color: theme.selection,
                        ..Default::default()
                    });
                }
            }
            if from < to {
                let extend = if newline_selected {
                    m.advance * 0.5
                } else {
                    0.0
                };
                band(out, from, to, extend, theme.selection);
            }
        }

        // Search matches, behind the text alongside the selection.
        if !matches.is_empty() {
            for range in &matches {
                let (at, end) = (range.start, range.end);
                if end <= row_start || at >= row_end {
                    continue;
                }
                band(
                    out,
                    at.max(row_start),
                    end.min(row_end),
                    0.0,
                    theme.find_match,
                );
            }
        }

        // Line number, right-aligned against the gutter, on a line's first
        // row only.
        let number = line + 1;
        let label = if row.first {
            number.to_string()
        } else {
            String::new()
        };
        let label_x = viewport.x + gutter_w - m.advance * (label.len() as f32 + 1.0);
        for (i, ch) in label.chars().enumerate() {
            if let Some(slot) = atlas.slot_for(ch) {
                out.push(GlyphInstance {
                    pos: [label_x + i as f32 * m.advance, y + glyph_dy],
                    size: [cell_w, cell_h],
                    uv: slot.uv,
                    flags: slot.flags(),
                    color: if line == buffer.rope.byte_to_line(buffer.cursor()) {
                        theme.gutter_text_active
                    } else {
                        theme.gutter_text
                    },
                    ..Default::default()
                });
            }
        }

        // Folding: a chevron in the cell after the number, pointing right on
        // a folded line and down, faintly, on one that could fold. A folded
        // line ends in a marker standing for what it hides.
        if row.first {
            let folded = !buffer.folds.is_empty() && buffer.is_folded_at(line);
            if folded || foldable[shown_at] {
                let icon = if folded {
                    crate::project::icons::CHEVRON_RIGHT
                } else {
                    crate::project::icons::CHEVRON_DOWN
                };
                if let Some(slot) = atlas.slot_for(icon) {
                    let [r, g, b, a] = theme.gutter_text;
                    out.push(GlyphInstance {
                        pos: [
                            m.snap(viewport.x + gutter_w - m.advance * 0.95),
                            y + glyph_dy + cell_h * 0.15,
                        ],
                        size: [cell_w * slot.cells as f32 * 0.7, cell_h * 0.7],
                        uv: slot.uv,
                        flags: slot.flags(),
                        color: if folded {
                            theme.accent
                        } else {
                            [r, g, b, a * 0.5]
                        },
                        ..Default::default()
                    });
                }
            }
            if folded {
                let end = crate::text::wrap::line_end(&buffer.rope, line);
                let row_end_x = if row.last {
                    offset_at(end)
                } else {
                    offset_at(row_end)
                };
                let x = text_x + row_end_x - scroll_x + m.advance * 0.5;
                if x < viewport.x + viewport.width {
                    push_rounded_rect(
                        out,
                        Viewport {
                            x,
                            y: y + 3.0,
                            width: m.advance * 3.0,
                            height: m.line_height - 6.0,
                        },
                        4.0,
                        theme.tab_hover,
                    );
                    push_text(out, atlas, x + m.advance, y, "…", theme.status_text);
                }
            }
        }

        let source_quads = out.len();
        if let Some(shaped) = shaped_line {
            let utf16_bytes = &shaped.source_bytes;
            for glyph in shaped.visible_glyphs(
                scroll_x,
                scroll_x + viewport.x + viewport.width - text_x,
                cell_w * 2.0,
            ) {
                let byte = line_start + utf16_bytes[glyph.source_utf16.min(utf16_bytes.len() - 1)];
                if wrapping && (byte < row_start || byte >= row_end) {
                    continue;
                }
                let span_at = spans.partition_point(|s| s.end <= byte);
                let color = spans
                    .get(span_at)
                    .filter(|s| s.start <= byte && byte < s.end)
                    .map_or(theme.text, |s| theme.syntax(s.kind));
                let x = text_x + glyph.x - scroll_x;
                if x + cell_w * 2.0 <= text_x || x > viewport.x + viewport.width {
                    continue;
                }
                let Some(slot) = atlas.slot_for_shaped(&shaped, glyph) else {
                    stats.unsupported += 1;
                    continue;
                };
                out.push(GlyphInstance {
                    pos: [x + slot.dx, y + glyph_dy],
                    size: [cell_w * slot.cells as f32, cell_h],
                    uv: slot.uv,
                    flags: slot.flags(),
                    color,
                    ..Default::default()
                });
            }
            for quad in &mut out[source_quads..] {
                clip_horizontal(quad, text_x, viewport.x + viewport.width);
            }
            shaped_carets.push((row_index, shaped, row_x0));
            if row.first {
                stats.lines += 1;
            }
            continue;
        }

        // The line's text. Read straight from the rope's chunks so a long
        // line is not copied into a String first.
        // Include enough left overhang for the widest atlas cell.
        let margin = (cell_w * 2.0 / m.advance).ceil() as usize;
        // A wrapped row starts at its own first column; tabs still stop
        // where they would on the whole line.
        let base_column = if wrapping && !row.first {
            buffer.rope.visual_column(line_start..row_start)
        } else {
            0
        };
        let (mut byte, mut column) = if wrapping {
            (row_start, base_column)
        } else if map.is_some() {
            // Hidden syntax pulls later text left: seek from the start.
            (line_start, 0)
        } else {
            buffer.rope.visual_seek(
                line_start..line_end,
                buffer.scroll_column.saturating_sub(margin),
            )
        };
        // Columns the drawn text sits from where its raw column puts it.
        let mut shift = map.map_or(0.0, |map| {
            map.column(&buffer.rope, byte) - buffer.rope.visual_column(line_start..byte) as f32
        });
        let mut span_at = spans.partition_point(|s| s.end <= byte);
        'line: for chunk in buffer.rope.chunks_in(byte..row_end) {
            for ch in chunk.chars() {
                // Advance past spans that ended before this character.
                while span_at < spans.len() && spans[span_at].end <= byte {
                    span_at += 1;
                }
                let span = spans
                    .get(span_at)
                    .filter(|s| s.start <= byte && byte < s.end);
                let color = span.map_or(theme.text, |s| theme.syntax(s.kind));
                let face = span.map_or(Face::Regular, |s| {
                    Face::Regular
                        .with_bold(s.kind.bold())
                        .with_italic(s.kind.italic())
                });
                let advance_bytes = ch.len_utf8();
                match ch {
                    '\n' | '\r' => {
                        byte += advance_bytes;
                        continue;
                    }
                    '\t' => {
                        column = (column / TAB_WIDTH + 1) * TAB_WIDTH;
                        byte += advance_bytes;
                        continue;
                    }
                    _ => {}
                }
                if let Some(map) = &map {
                    shift += map.pad_at(byte) as f32;
                    if map.hides(byte) {
                        shift -= display_width(ch) as f32;
                        column += display_width(ch);
                        byte += advance_bytes;
                        continue;
                    }
                }

                // `scroll_x` holds the row's own start, `base_column` in raw
                // columns plus any shift before it.
                let x = text_x + (column as f32 + shift) * m.advance - scroll_x;
                if x > viewport.x + viewport.width {
                    break 'line;
                }
                // Scrolled off to the left: skip rather than draw under the
                // gutter. Still costs the walk, but not a quad. By the
                // character's own width: a wide one half in view is drawn,
                // and the clip below trims its left half.
                if x + cell_w * display_width(ch).max(1) as f32 <= text_x {
                    column += display_width(ch);
                    byte += advance_bytes;
                    continue;
                }

                let slot = if face == Face::Regular {
                    atlas.slot_for_fallback(ch)
                } else {
                    atlas.slot_for_face(ch, face)
                };
                match slot {
                    Some(slot) => out.push(GlyphInstance {
                        pos: [x, y + glyph_dy],
                        size: [cell_w * slot.cells as f32, cell_h],
                        uv: slot.uv,
                        flags: slot.flags(),
                        color,
                        ..Default::default()
                    }),
                    // Nothing on the system can draw it. Counted rather than
                    // silently dropped, so it is at least reportable.
                    None => stats.unsupported += 1,
                }
                column += display_width(ch);
                byte += advance_bytes;
            }
        }
        for quad in &mut out[source_quads..] {
            clip_horizontal(quad, text_x, viewport.x + viewport.width);
        }
        if row.first {
            stats.lines += 1;
        }
    }

    // Carets last, so they sit on top. One per cursor.
    for caret in buffer
        .caret_positions()
        .into_iter()
        .filter(|_| focused && carets)
    {
        let caret_line = buffer.rope.byte_to_line(caret);
        let Some(row_index) = rows_on_screen
            .iter()
            .position(|r| r.holds(caret_line, caret))
        else {
            continue;
        };
        let row = rows_on_screen[row_index];
        let line_start = buffer.rope.line_to_byte(caret_line);
        let x = if let Some((_, shaped, row_x0)) =
            shaped_carets.iter().find(|(index, ..)| *index == row_index)
        {
            text_x + shaped.x_of_byte(caret - line_start) - scroll_x - row_x0
        } else if let Some(map) = markdown.line(buffer, &touched, caret_line) {
            let column = map.column(&buffer.rope, caret) - map.column(&buffer.rope, row.start);
            text_x + column * m.advance - scroll_x
        } else {
            let column = crate::text::wrap::column_in_row(&buffer.rope, row.start, caret);
            text_x + column as f32 * m.advance - scroll_x
        };
        let y = row.y;
        if x <= viewport.x + viewport.width && x >= text_x {
            out.push(GlyphInstance {
                pos: [x, y],
                size: [(m.advance * 0.15).max(1.0), m.line_height],
                uv: solid,
                color: theme.cursor,
                ..Default::default()
            });
        }
    }

    // The first and last lines are usually cut by the edges.
    for quad in &mut out[drawn_from..] {
        clip_vertical(quad, viewport.y, viewport.y + viewport.height);
    }

    if let Some(thumb) = scrollbar_thumb(buffer, viewport, m.line_height) {
        let [r, g, b, _] = theme.status_text;
        push_rounded_rect(out, thumb, SCROLLBAR_WIDTH / 2.0, [r, g, b, 0.4]);
    }

    stats.quads = out.len();
    atlas.finish_shaping_frame();
    stats
}

/// Draws the Git marks down the left edge of the gutter: a bar beside an
/// added or modified line, a short stub at the top of a line that lost the
/// lines above it. Appended after the text, in the gutter's first cell,
/// which the right-aligned line numbers never reach.
pub fn push_gutter_marks(
    out: &mut Vec<GlyphInstance>,
    atlas: &Atlas,
    viewport: Viewport,
    rows: &[ScreenRow],
    theme: &Theme,
    marks: &[crate::project::git::Mark],
) {
    use crate::project::git::MarkKind;
    let m = atlas.metrics;
    let (Some(top), Some(bottom)) = (rows.first(), rows.last()) else {
        return;
    };
    let (first, last) = (top.line, bottom.line + 1);
    let drawn_from = out.len();
    let x = viewport.x + 2.0;
    for mark in marks {
        // A line's rows: its bar runs down all of them.
        let mut on = rows.iter().filter(|r| r.line == mark.line);
        let (y, height) = match (on.next(), on.next_back()) {
            (Some(a), Some(b)) => (a.y, b.y + m.line_height - a.y),
            (Some(a), None) => (a.y, m.line_height),
            _ => (bottom.y + m.line_height, m.line_height),
        };
        match mark.kind {
            MarkKind::Added | MarkKind::Modified => {
                if mark.line < first || mark.line >= last {
                    continue;
                }
                let color = if mark.kind == MarkKind::Added {
                    theme.diff_added
                } else {
                    theme.diff_modified
                };
                push_rect(out, atlas, [x, y], [3.0, height], color);
            }
            MarkKind::Removed => {
                // Between two lines, so it may sit on the boundary just
                // past the last visible row, or past the last line.
                if mark.line < first || mark.line > last {
                    continue;
                }
                push_rect(out, atlas, [x, y - 1.5], [8.0, 3.0], theme.diff_removed);
            }
        }
    }
    for quad in &mut out[drawn_from..] {
        clip_vertical(quad, viewport.y, viewport.y + viewport.height);
    }
}

/// The lightbulb on the caret's line, where its number was: the server has
/// code actions there. The number is the only one drawn in the active
/// gutter colour, so it is taken back out of `out`, which [`build_full`]
/// has just filled. Returns where the bulb is, for clicks.
pub fn push_bulb(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    buffer: &Buffer,
    viewport: Viewport,
    rows: &[ScreenRow],
    theme: &Theme,
) -> Option<Viewport> {
    let m = atlas.metrics;
    let line = buffer.rope.byte_to_line(buffer.cursor());
    let row = *rows.iter().find(|r| r.line == line && r.first)?;
    let slot = atlas.slot_for(crate::project::icons::LIGHTBULB)?;
    let numbers_end = viewport.x + gutter_width(buffer, atlas) - m.advance;
    let (top, bottom) = (row.y, row.y + m.line_height);
    out.retain(|g| {
        !(g.color == theme.gutter_text_active
            && g.pos[0] >= viewport.x
            && g.pos[0] < numbers_end
            && g.pos[1] >= top
            && g.pos[1] < bottom)
    });
    let (cell_w, cell_h) = atlas.cell_size();
    let size = [cell_w * slot.cells as f32 * 0.8, cell_h * 0.8];
    let x = m.snap(numbers_end - size[0]);
    let y = top + (m.line_height - size[1]) * 0.5;
    if y < viewport.y || y + size[1] > viewport.y + viewport.height {
        return None;
    }
    out.push(GlyphInstance {
        pos: [x, y],
        size,
        uv: slot.uv,
        flags: slot.flags(),
        color: theme.syn_constant,
        ..Default::default()
    });
    Some(Viewport {
        x,
        y: top,
        width: size[0],
        height: m.line_height,
    })
}

/// Whether a point in the text area's own coordinates (as
/// [`offset_at_point`] takes them) is on the caret line's bulb, where
/// [`push_bulb`] draws it.
pub fn bulb_at(buffer: &Buffer, atlas: &Atlas, x: f32, y: f32) -> bool {
    let m = atlas.metrics;
    let numbers_end = gutter_width(buffer, atlas) - m.advance;
    let (cell_w, _) = atlas.cell_size();
    if x < numbers_end - cell_w * 2.0 || x >= numbers_end {
        return false;
    }
    let line = buffer.rope.byte_to_line(buffer.cursor());
    let rows = rows_down_to(buffer, y, m.line_height);
    rows.iter()
        .any(|r| r.line == line && r.first && r.y <= y && y < r.y + m.line_height)
}

/// A line under `range` of `buffer`'s text, on every visible row it
/// touches: how a diagnostic is shown.
/// Squiggles under each of `marks`, a range and its colour: the visible
/// rows are worked out once for all of them, and a range off screen costs
/// two line lookups.
pub fn push_underlines(
    out: &mut Vec<GlyphInstance>,
    atlas: &Atlas,
    buffer: &Buffer,
    text: Viewport,
    rows: &[ScreenRow],
    marks: &[(std::ops::Range<usize>, [f32; 4])],
) {
    let (Some(first), Some(last)) = (rows.first(), rows.last()) else {
        return;
    };
    let (first, last) = (first.line, last.line);
    for (range, color) in marks {
        push_underline_on(
            out,
            atlas,
            buffer,
            text,
            rows,
            (first, last),
            range.clone(),
            *color,
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn push_underline_on(
    out: &mut Vec<GlyphInstance>,
    atlas: &Atlas,
    buffer: &Buffer,
    text: Viewport,
    rows: &[ScreenRow],
    (first, last): (usize, usize),
    range: std::ops::Range<usize>,
    color: [f32; 4],
) {
    let m = atlas.metrics;
    let text_x = text.x + gutter_width(buffer, atlas);
    let scroll_x = buffer.scroll_column as f32 * m.advance;
    let start_line = buffer
        .rope
        .byte_to_line(range.start.min(buffer.rope.len_bytes()));
    let end_line = buffer
        .rope
        .byte_to_line(range.end.min(buffer.rope.len_bytes()));
    if end_line < first || start_line > last {
        return;
    }
    for row in rows
        .iter()
        .filter(|r| r.line >= start_line && r.line <= end_line)
    {
        let line = row.line;
        let line_start = buffer.rope.line_to_byte(line);
        let content_end = crate::text::wrap::line_end(&buffer.rope, line);
        let row_end = if row.last { content_end } else { row.end };
        let from = if line == start_line {
            range.start.max(row.start)
        } else {
            row.start
        };
        let to = if line == end_line {
            range.end.min(row_end)
        } else {
            row_end
        };
        // Not on this row of a wrapped line.
        if from > row_end
            || to < row.start
            || (from == to && !(row.start..=row_end).contains(&from))
        {
            continue;
        }
        let lead = buffer.rope.visual_column(line_start..row.start);
        let c0 = (buffer.rope.visual_column(line_start..from.min(to)) - lead) as f32;
        let c1 = (buffer.rope.visual_column(line_start..to.max(from)) - lead) as f32;
        let x0 = (text_x + c0 * m.advance - scroll_x).max(text_x);
        // An empty range still gets a mark one cell wide, or it is invisible.
        let x1 = (text_x + c1.max(c0 + 1.0) * m.advance - scroll_x).min(text.x + text.width);
        if x1 <= x0 {
            continue;
        }
        let y = row.y + m.line_height - 2.0;
        if y < text.y || y + 1.5 > text.y + text.height {
            continue;
        }
        // A squiggle: short steps alternating up and down, which is what
        // every editor draws for a problem and what a straight line (a link,
        // a spelling underline) is not.
        const STEP: f32 = 2.0;
        let mut x = x0;
        let mut up = false;
        while x < x1 {
            let width = STEP.min(x1 - x);
            push_rect(
                out,
                atlas,
                [x, if up { y - 1.2 } else { y }],
                [width, 1.2],
                color,
            );
            x += STEP;
            up = !up;
        }
    }
}

/// Whether a row break of `line` falls in or against right-to-left text
/// (Hebrew, Arabic and the scripts after them): a wrapped row cut out of
/// the whole line's shaping is wrong there, and only there.
pub(super) fn has_rtl(buffer: &Buffer, line: usize) -> bool {
    let rtl = |c: char| {
        matches!(c as u32,
            0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF | 0x10800..=0x10FFF | 0x1E800..=0x1EFFF)
    };
    let starts = buffer.row_starts(line);
    starts.iter().skip(1).any(|&at| {
        // The letters either side of the break, past any spaces.
        let before = buffer.rope.slice_to_string(at.saturating_sub(16)..at);
        let after_end = (at + 16).min(buffer.rope.len_bytes());
        let after = buffer.rope.slice_to_string(at..after_end);
        before.trim_end().chars().next_back().is_some_and(rtl)
            || after.trim_start().chars().next().is_some_and(rtl)
    })
}

/// How many of `chars` fit in `cells`, without ending between the parts of
/// a joined emoji (a zero-width joiner or a variation selector and what it
/// joins stay together).
pub(super) fn fit_cells<'a>(chars: impl Iterator<Item = &'a char>, cells: usize) -> usize {
    let joins = |c: char| c == '\u{200d}' || ('\u{fe00}'..='\u{fe0f}').contains(&c);
    let chars: Vec<char> = chars.copied().collect();
    let (mut count, mut used) = (0, 0);
    while count < chars.len() {
        let width = display_width(chars[count]);
        if used + width > cells {
            break;
        }
        used += width;
        count += 1;
    }
    // Back off to a boundary that is not inside a joined sequence.
    while count > 0 && count < chars.len() && (joins(chars[count]) || joins(chars[count - 1])) {
        count -= 1;
    }
    count
}

/// One suggestion as the ribbon draws it.
pub struct Chip<'a> {
    pub label: &'a str,
    pub icon: char,
}

pub const RIBBON_HEIGHT: f32 = 30.0;
pub const RIBBON_WHY_HEIGHT: f32 = 22.0;
pub(super) const CHIP_PAD: f32 = 9.0;
pub(super) const CHIP_GAP: f32 = 2.0;
pub(super) const CHIP_ICON: f32 = 18.0;
pub(super) const RIBBON_MAX_CHIPS: usize = 7;

/// The signature of the call the caret is in, above the caret's line, with
/// the parameter being typed in the accent colour. Code, so monospace.
/// Returns the panel it drew.
pub fn build_signature(
    label: &str,
    active: Option<std::ops::Range<usize>>,
    caret: Viewport,
    text: Viewport,
    atlas: &mut Atlas,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) -> Viewport {
    let advance = atlas.metrics.advance;
    let line = atlas.metrics.line_height;
    let room = (((text.width - 32.0) / advance).max(8.0)) as usize;
    let shown: String = label.chars().take(room).collect();
    let width = shown.chars().count() as f32 * advance + 20.0;
    let height = line + 10.0;
    let x = (caret.x - 10.0).clamp(
        text.x + 8.0,
        (text.x + text.width - width - 8.0).max(text.x + 8.0),
    );
    let above = caret.y - height - 4.0;
    let y = if above >= text.y {
        above
    } else {
        caret.y + caret.height + 4.0
    };
    let panel = Viewport {
        x,
        y,
        width,
        height,
    };
    push_panel(out, panel, 6.0, theme);
    let active = active.filter(|r| r.start <= r.end && shown.get(r.clone()).is_some());
    let parts: [(&str, [f32; 4]); 3] = match &active {
        Some(r) => [
            (&shown[..r.start], theme.status_text),
            (&shown[r.clone()], theme.accent),
            (&shown[r.end..], theme.status_text),
        ],
        None => [
            (&shown[..], theme.status_text),
            ("", theme.text),
            ("", theme.text),
        ],
    };
    let mut cx = panel.x + 10.0;
    for (part, color) in parts {
        push_text(out, atlas, cx, panel.y + 5.0, part, color);
        cx += part.chars().count() as f32 * advance;
    }
    panel
}

/// Completion as a ribbon, not a list box: the best guess as ghost text at
/// the caret, the alternatives as one row of chips under the line (above it
/// near the bottom of the view), and under them why the picked one is
/// offered. Nothing covers the lines around the caret beyond that row.
///
/// `word_x` is where the word being completed starts, so the chips line up
/// with it. Returns each drawn chip's rectangle with its index in `chips`,
/// for clicks.
#[allow(clippy::too_many_arguments)]
pub fn build_completion_ribbon(
    chips: &[Chip<'_>],
    selected: usize,
    why: &str,
    ghost: Option<&str>,
    caret: Viewport,
    word_x: f32,
    text: Viewport,
    atlas: &mut Atlas,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) -> Vec<(Viewport, usize)> {
    // The ghost: what Tab would add, dimmed, on the caret's own line.
    if let Some(ghost) = ghost.filter(|g| !g.is_empty()) {
        let room = ((text.x + text.width - caret.x) / atlas.metrics.advance).max(0.0) as usize;
        let shown: String = ghost.chars().take(room).collect();
        let [r, g, b, _] = theme.text;
        push_text(out, atlas, caret.x, caret.y, &shown, [r, g, b, 0.38]);
    }
    if chips.is_empty() {
        return Vec::new();
    }

    // Which chips fit: a window around the selected one.
    let widths: Vec<f32> = chips
        .iter()
        .map(|c| CHIP_PAD * 2.0 + CHIP_ICON + ui_text_width(atlas, c.label).min(260.0))
        .collect();
    let max_width = (text.width - 16.0).max(80.0);
    let mut first = selected.saturating_sub(RIBBON_MAX_CHIPS - 1);
    let fits = |first: usize, widths: &[f32]| {
        let mut total = 8.0;
        let mut last = first;
        for (i, w) in widths.iter().enumerate().skip(first).take(RIBBON_MAX_CHIPS) {
            if total + w > max_width && i > first {
                break;
            }
            total += w + CHIP_GAP;
            last = i;
        }
        (last, total)
    };
    let (mut last, mut width) = fits(first, &widths);
    while last < selected && first < selected {
        first += 1;
        (last, width) = fits(first, &widths);
    }
    let why_height = if why.is_empty() {
        0.0
    } else {
        RIBBON_WHY_HEIGHT
    };
    let width = width.max(if why.is_empty() {
        0.0
    } else {
        ui_text_width(atlas, why).min(max_width) + 20.0
    });
    let height = RIBBON_HEIGHT + why_height;
    let x = (word_x - 12.0).clamp(
        text.x + 8.0,
        (text.x + text.width - width - 8.0).max(text.x + 8.0),
    );
    let below = caret.y + caret.height + 4.0;
    let y = if below + height <= text.y + text.height || caret.y - height - 4.0 < text.y {
        below
    } else {
        caret.y - height - 4.0
    };
    let panel = Viewport {
        x,
        y,
        width,
        height,
    };

    push_rounded_rect(
        out,
        Viewport {
            x: panel.x - 3.0,
            y: panel.y + 2.0,
            width: panel.width + 6.0,
            height: panel.height + 3.0,
        },
        10.0,
        [0.0, 0.0, 0.0, 0.16],
    );
    push_panel(out, panel, 8.0, theme);

    let (cell_w, cell_h) = atlas.cell_size();
    let glyph_dy = atlas.metrics.glyph_dy(RIBBON_HEIGHT - 6.0);
    let mut hits = Vec::new();
    let mut cx = panel.x + 4.0;
    for (index, chip) in chips.iter().enumerate().skip(first).take(last + 1 - first) {
        let rect = Viewport {
            x: cx,
            y: panel.y + 3.0,
            width: widths[index],
            height: RIBBON_HEIGHT - 6.0,
        };
        let picked = index == selected;
        if picked {
            push_rounded_rect(out, rect, 5.0, theme.palette_selected);
        }
        if let Some(slot) = atlas.slot_for(chip.icon) {
            out.push(GlyphInstance {
                pos: [
                    atlas.metrics.snap(rect.x + CHIP_PAD - 2.0),
                    rect.y + glyph_dy,
                ],
                size: [cell_w * slot.cells as f32 * 0.8, cell_h * 0.8],
                uv: slot.uv,
                flags: slot.flags(),
                color: if picked {
                    theme.accent
                } else {
                    theme.status_text
                },
                ..Default::default()
            });
        }
        push_ui_text(
            out,
            atlas,
            Viewport {
                x: rect.x + CHIP_PAD + CHIP_ICON,
                width: (rect.width - CHIP_PAD * 2.0 - CHIP_ICON).max(0.0),
                ..rect
            },
            chip.label,
            if picked {
                theme.text
            } else {
                theme.status_text
            },
        );
        hits.push((rect, index));
        cx += widths[index] + CHIP_GAP;
    }
    if !why.is_empty() {
        push_rect(
            out,
            atlas,
            [panel.x + 8.0, panel.y + RIBBON_HEIGHT],
            [panel.width - 16.0, 1.0],
            theme.palette_border,
        );
        push_ui_text(
            out,
            atlas,
            Viewport {
                x: panel.x + 12.0,
                y: panel.y + RIBBON_HEIGHT,
                width: panel.width - 20.0,
                height: RIBBON_WHY_HEIGHT,
            },
            why,
            theme.status_text,
        );
    }
    hits
}

/// Width of the line-number gutter, in logical points.
///
/// Public because mouse hit-testing needs the same number the renderer used.
/// Deriving it twice is how a click ends up one column off from where the
/// caret is drawn.
pub fn gutter_width(buffer: &Buffer, atlas: &Atlas) -> f32 {
    let digits = digit_count(buffer.rope.len_lines());
    (digits as f32 + 2.0) * atlas.metrics.advance
}

/// Byte offset of the character nearest a point in the view, for click and
/// drag positioning. `x` and `y` are logical points from the view's top-left.
pub fn offset_at_point(
    buffer: &Buffer,
    atlas: &Atlas,
    markdown: &Markdown,
    x: f32,
    y: f32,
) -> usize {
    let m = atlas.metrics;
    let total_lines = buffer.rope.len_lines();

    if buffer.row_mode() {
        // Horizontal scroll only exists without wrapping.
        let scrolled = buffer.scroll_column as f32 * m.advance;
        // Rows from the top of the text; y is measured from there too.
        let rows = rows_down_to(buffer, y, m.line_height);
        let Some(row) = rows
            .iter()
            .rev()
            .find(|r| r.y <= y)
            .or(rows.first())
            .copied()
        else {
            return buffer.rope.len_bytes();
        };
        let text_x = gutter_width(buffer, atlas);
        let line_start = buffer.rope.line_to_byte(row.line);
        let content_end = crate::text::wrap::line_end(&buffer.rope, row.line);
        let row_end = if row.last { content_end } else { row.end };
        let split_rtl = !(row.first && row.last) && has_rtl(buffer, row.line);
        if let Some(shaped) = atlas
            .cached_editor_line((buffer.id(), row.line), &buffer.rope)
            .filter(|_| !split_rtl)
        {
            let x0 = shaped.x_of_byte(row.start - line_start);
            let at = line_start + shaped.byte_at_x(x - text_x + x0 + scrolled);
            return at.clamp(
                row.start,
                if row.last {
                    row_end
                } else {
                    // The last char's start, not the byte before
                    // `row_end`: a CJK row breaks mid-word, and the byte
                    // before the break is inside a character.
                    buffer
                        .rope
                        .char_to_byte(buffer.rope.byte_to_char(row_end).saturating_sub(1))
                        .max(row.start)
                },
            );
        }
        let at = if let Some(map) = markdown.line(buffer, &touched_lines(buffer), row.line) {
            let column = (x - text_x + scrolled) / m.advance + map.column(&buffer.rope, row.start);
            map.byte_at(&buffer.rope, row.start, row_end, column)
        } else {
            let column = ((x - text_x + scrolled) / m.advance).max(0.0);
            crate::text::wrap::byte_at_fraction(&buffer.rope, row.start, row_end, column)
        };
        // The end of a continued row is the next row's start: a click past
        // the text stays on the row that was clicked.
        return if !row.last && at >= row_end {
            buffer
                .rope
                .char_to_byte(buffer.rope.byte_to_char(row_end).saturating_sub(1))
        } else {
            at
        };
    }

    let row = ((y + scroll_offset(buffer, m.line_height)) / m.line_height)
        .floor()
        .max(0.0) as usize;
    let line = (buffer.scroll_line + row).min(total_lines.saturating_sub(1));

    // Round rather than floor: clicking the right half of a character should
    // put the caret after it, which is what every editor does.
    let text_x = gutter_width(buffer, atlas);
    let start = buffer.rope.line_to_byte(line);
    if let Some(shaped) = atlas.cached_editor_line((buffer.id(), line), &buffer.rope) {
        let target_x = x - text_x + buffer.scroll_column as f32 * m.advance;
        return start + shaped.byte_at_x(target_x);
    }
    let column = ((x - text_x) / m.advance).max(0.0) + buffer.scroll_column as f32;
    if let Some(map) = markdown.line(buffer, &touched_lines(buffer), line) {
        let end = crate::text::wrap::line_end(&buffer.rope, line);
        return map.byte_at(&buffer.rope, start, end, column);
    }

    crate::text::wrap::byte_at_fraction(
        &buffer.rope,
        start,
        crate::text::wrap::line_end(&buffer.rope, line),
        column,
    )
}

/// The screen rows from the top of the text down to the one under `y`, for
/// hit tests measured from the top of the text rather than a viewport.
pub(super) fn rows_down_to(buffer: &Buffer, y: f32, line_height: f32) -> Vec<ScreenRow> {
    screen_rows(
        buffer,
        Viewport {
            x: 0.0,
            y: 0.0,
            width: f32::MAX,
            height: y.max(0.0) + line_height,
        },
        line_height,
    )
}

/// The line whose fold chevron is under a point in the text area's own
/// coordinates (as [`offset_at_point`] takes them), when there is one: the
/// cell after the line number, on a line's first row, of a line that is
/// folded or could fold.
pub fn fold_chevron_at(buffer: &Buffer, atlas: &Atlas, x: f32, y: f32) -> Option<usize> {
    let m = atlas.metrics;
    let gutter = gutter_width(buffer, atlas);
    if x < gutter - m.advance * 1.2 || x >= gutter {
        return None;
    }
    let rows = rows_down_to(buffer, y, m.line_height);
    let row = rows.iter().find(|r| r.y <= y && y < r.y + m.line_height)?;
    (row.first && (buffer.is_folded_at(row.line) || buffer.can_fold(row.line))).then_some(row.line)
}

/// Where the primary caret is, in the same coordinates as `text`: the top
/// left of its cell and the cell's size. `None` when it is scrolled out of
/// view. The inverse of [`offset_at_point`], and what the input method asks
/// for so it can put its candidate window next to what is being typed.
pub fn caret_rect(
    buffer: &Buffer,
    atlas: &Atlas,
    markdown: &Markdown,
    text: Viewport,
) -> Option<Viewport> {
    let rows = if buffer.row_mode() {
        screen_rows(buffer, text, atlas.metrics.line_height)
    } else {
        Vec::new()
    };
    caret_rect_on(buffer, atlas, markdown, text, &rows)
}

/// [`caret_rect`] with the frame's rows, which it reads while wrapping or
/// folding.
pub fn caret_rect_on(
    buffer: &Buffer,
    atlas: &Atlas,
    markdown: &Markdown,
    text: Viewport,
    rows: &[ScreenRow],
) -> Option<Viewport> {
    let m = atlas.metrics;
    let (line, _) = buffer.cursor_position();
    let map = markdown.line(buffer, &touched_lines(buffer), line);
    if buffer.row_mode() {
        let caret = buffer.cursor();
        let row = *rows.iter().find(|r| r.holds(line, caret))?;
        let line_start = buffer.rope.line_to_byte(line);
        let split_rtl = !(row.first && row.last) && has_rtl(buffer, line);
        let x = if let Some(shaped) = atlas
            .cached_editor_line((buffer.id(), line), &buffer.rope)
            .filter(|_| !split_rtl)
        {
            shaped.x_of_byte(caret - line_start) - shaped.x_of_byte(row.start - line_start)
        } else if let Some(map) = &map {
            (map.column(&buffer.rope, caret) - map.column(&buffer.rope, row.start)) * m.advance
        } else {
            crate::text::wrap::column_in_row(&buffer.rope, row.start, caret) as f32 * m.advance
        };
        let x = x - buffer.scroll_column as f32 * m.advance;
        if x < 0.0 {
            return None;
        }
        return Some(Viewport {
            x: text.x + gutter_width(buffer, atlas) + x,
            y: row.y,
            width: m.advance,
            height: m.line_height,
        });
    }
    let row = line.checked_sub(buffer.scroll_line)?;
    if row >= text.rows(m.line_height).max(1) {
        return None;
    }
    let line_start = buffer.rope.line_to_byte(line);
    if let Some(shaped) = atlas.cached_editor_line((buffer.id(), line), &buffer.rope) {
        let x =
            text.x + gutter_width(buffer, atlas) + shaped.x_of_byte(buffer.cursor() - line_start)
                - buffer.scroll_column as f32 * m.advance;
        if x < text.x + gutter_width(buffer, atlas) {
            return None;
        }
        return Some(Viewport {
            x,
            y: text.y + row as f32 * m.line_height - scroll_offset(buffer, m.line_height),
            width: m.advance,
            height: m.line_height,
        });
    }
    let column = match &map {
        Some(map) => map.column(&buffer.rope, buffer.cursor()),
        None => buffer.rope.visual_column(line_start..buffer.cursor()) as f32,
    } - buffer.scroll_column as f32;
    if column < 0.0 {
        return None;
    }
    Some(Viewport {
        x: text.x + gutter_width(buffer, atlas) + column * m.advance,
        y: text.y + row as f32 * m.line_height - scroll_offset(buffer, m.line_height),
        width: m.advance,
        height: m.line_height,
    })
}

/// Draws text an input method is still composing, over the caret: the
/// accent waiting for its letter, or the syllables waiting to become a word.
/// Underlined, which is how every Mac text field marks "not committed yet".
pub fn push_marked_text(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    at: Viewport,
    text: &str,
    caret: usize,
    theme: &Theme,
) {
    let m = atlas.metrics;
    let cells: usize = text.chars().map(display_width).sum();
    let caret_cells: usize = text.chars().take(caret).map(display_width).sum();
    let width = cells.max(1) as f32 * m.advance;
    // Its own background, since it is drawn over whatever follows the caret
    // rather than pushing it along.
    let opaque = [
        theme.background[0] as f32,
        theme.background[1] as f32,
        theme.background[2] as f32,
        1.0,
    ];
    push_rect(out, atlas, [at.x, at.y], [width, at.height], opaque);
    push_text(out, atlas, at.x, at.y, text, theme.text);
    let thickness = (2.0 / m.scale).max(1.0 / m.scale);
    push_rect(
        out,
        atlas,
        [at.x, m.snap(at.y + at.height - thickness * 2.0)],
        [width, thickness],
        theme.accent,
    );
    // The input method's own caret, where the next keystroke changes the
    // composition.
    push_rect(
        out,
        atlas,
        [at.x + caret_cells as f32 * m.advance, at.y],
        [(m.advance * 0.15).max(1.0), at.height],
        theme.cursor,
    );
}

/// Appends a run of text at an arbitrary position. Used for chrome such as
/// the status line, which is not part of the buffer.
pub fn push_text(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    x: f32,
    y: f32,
    text: &str,
    color: [f32; 4],
) {
    let (cell_w, cell_h) = atlas.cell_size();
    let m = atlas.metrics;
    let advance = m.advance;
    let glyph_dy = m.glyph_dy(m.line_height);
    let mut column = 0usize;
    for ch in text.chars() {
        if let Some(slot) = atlas.slot_for(ch) {
            out.push(GlyphInstance {
                pos: [x + column as f32 * advance, y + glyph_dy],
                size: [cell_w * slot.cells as f32, cell_h],
                uv: slot.uv,
                flags: slot.flags(),
                color,
                ..Default::default()
            });
        }
        column += display_width(ch);
    }
}

/// How far above its viewport the text starts, in points: the part of the
/// first line a trackpad has scrolled off. Everything that places a line by
/// its row subtracts this, or it lands a fraction of a line off the text.
pub fn scroll_offset(buffer: &Buffer, line_height: f32) -> f32 {
    buffer.scroll_fraction.clamp(0.0, 1.0) * line_height
}

/// The lines with any part in `viewport`: the top one, partly scrolled
/// off, down to the one the bottom edge cuts through.
pub fn visible_lines(
    buffer: &Buffer,
    viewport: Viewport,
    line_height: f32,
) -> std::ops::Range<usize> {
    let total = buffer.rope.len_lines();
    let first = buffer.scroll_line.min(total.saturating_sub(1));
    if line_height <= 0.0 || viewport.height <= 0.0 {
        return first..first;
    }
    if buffer.row_mode() {
        return lines_of(&screen_rows(buffer, viewport, line_height)).unwrap_or(first..first);
    }
    let reach = viewport.height + scroll_offset(buffer, line_height);
    let count = (reach / line_height).ceil() as usize;
    first..(first + count).min(total)
}

/// The lines `rows` touch, hidden ones inside a fold included.
pub fn lines_of(rows: &[ScreenRow]) -> Option<std::ops::Range<usize>> {
    Some(rows.first()?.line..rows.last()?.line + 1)
}

/// One row of text on screen: a whole line, or one row of a wrapped line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScreenRow {
    pub line: usize,
    /// The row's text. The last row of a line ends past its newline, where
    /// the next line starts, as `line_to_byte(line + 1)` does.
    pub start: usize,
    pub end: usize,
    /// The first and last rows of the line.
    pub first: bool,
    pub last: bool,
    /// Top of the row.
    pub y: f32,
}

impl ScreenRow {
    /// Whether a caret at `byte` on `line` is drawn on this row. A caret on
    /// a break belongs to the row the break starts.
    pub fn holds(&self, line: usize, byte: usize) -> bool {
        self.line == line && byte >= self.start && (byte < self.end || self.last)
    }
}

/// The rows with any part in `viewport`, top to bottom. Every place that
/// puts text on screen or finds text under the pointer reads these, so the
/// two cannot disagree about where a wrapped row is.
/// What a Markdown document draws beyond its colours: the band behind code
/// blocks, and on lines no caret or selection touches, its syntax hidden and
/// its table cells padded. Empty for any other document.
#[derive(Clone, Copy, Default)]
pub struct Markdown<'a> {
    pub bands: &'a [std::ops::Range<usize>],
    pub hidden: &'a [std::ops::Range<usize>],
    pub pads: &'a [(usize, usize)],
}

impl<'a> Markdown<'a> {
    pub fn of(styled: Option<&'a crate::markdown::source::Styled>) -> Self {
        styled.map_or_else(Self::default, |s| Markdown {
            bands: &s.bands,
            hidden: &s.hidden,
            pads: &s.pads,
        })
    }

    /// `line` drawn off the monospace grid, or `None` when it is drawn as
    /// written. A line in `touched` shows its syntax; its pads stay.
    fn line(
        &self,
        buffer: &Buffer,
        touched: &[(usize, usize)],
        line: usize,
    ) -> Option<LineMap<'a>> {
        if self.hidden.is_empty() && self.pads.is_empty() {
            return None;
        }
        let start = buffer.rope.line_to_byte(line);
        let end = crate::text::wrap::line_end(&buffer.rope, line);
        let hidden = if touched.iter().any(|&(a, b)| a <= line && line <= b) {
            &[][..]
        } else {
            let from = self.hidden.partition_point(|h| h.end <= start);
            let to = self.hidden.partition_point(|h| h.start < end);
            &self.hidden[from..to.max(from)]
        };
        let from = self.pads.partition_point(|p| p.0 < start);
        let to = self.pads.partition_point(|p| p.0 < end);
        let pads = &self.pads[from..to.max(from)];
        (!hidden.is_empty() || !pads.is_empty()).then_some(LineMap {
            start,
            end,
            hidden,
            pads,
        })
    }
}

/// The lines each cursor's selection runs over, a caret's own line included.
pub(super) fn touched_lines(buffer: &Buffer) -> Vec<(usize, usize)> {
    buffer
        .selections()
        .into_iter()
        .map(|(a, b)| (buffer.rope.byte_to_line(a), buffer.rope.byte_to_line(b)))
        .collect()
}

/// One Markdown line's hidden syntax and pads, for placing its text.
#[derive(Clone, Copy)]
pub(super) struct LineMap<'a> {
    start: usize,
    end: usize,
    hidden: &'a [std::ops::Range<usize>],
    pads: &'a [(usize, usize)],
}

impl LineMap<'_> {
    fn hides(&self, byte: usize) -> bool {
        let at = self.hidden.partition_point(|h| h.end <= byte);
        self.hidden.get(at).is_some_and(|h| h.start <= byte)
    }

    /// Blank columns drawn before the character at `byte`.
    fn pad_at(&self, byte: usize) -> usize {
        self.pads
            .binary_search_by_key(&byte, |p| p.0)
            .map_or(0, |at| self.pads[at].1)
    }

    /// Where the caret at `byte` is drawn, in columns from the line start:
    /// the pads before it counted, the hidden syntax not.
    fn column(&self, rope: &crate::text::rope::Rope, byte: usize) -> f32 {
        let byte = byte.clamp(self.start, self.end);
        let mut column = rope.visual_column(self.start..byte) as f32;
        for h in self.hidden.iter().take_while(|h| h.start < byte) {
            let (from, to) = (h.start.max(self.start), h.end.min(byte));
            if to > from {
                column -= (rope.byte_to_char(to) - rope.byte_to_char(from)) as f32;
            }
        }
        for p in self.pads.iter().take_while(|p| p.0 < byte) {
            column += p.1 as f32;
        }
        column.max(0.0)
    }

    /// The caret stop in `from..=to` drawn nearest `column`; the earliest of
    /// those drawn at the same place, which a hidden run makes several.
    fn byte_at(
        &self,
        rope: &crate::text::rope::Rope,
        from: usize,
        to: usize,
        column: f32,
    ) -> usize {
        let mut at = self.column(rope, from);
        let mut raw = rope.visual_column(self.start..from);
        let mut byte = from;
        let mut best = (from, (at - column).abs());
        for chunk in rope.chunks_in(from..to) {
            for ch in chunk.chars() {
                if ch == '\n' || ch == '\r' {
                    return best.0;
                }
                let width = if ch == '\t' {
                    TAB_WIDTH - raw % TAB_WIDTH
                } else {
                    display_width(ch)
                };
                raw += width;
                at += self.pad_at(byte) as f32;
                if !self.hides(byte) {
                    at += width as f32;
                }
                byte += ch.len_utf8();
                let distance = (at - column).abs();
                if distance < best.1 {
                    best = (byte, distance);
                }
            }
        }
        best.0
    }
}

pub fn screen_rows(buffer: &Buffer, viewport: Viewport, line_height: f32) -> Vec<ScreenRow> {
    let total = buffer.rope.len_lines();
    let mut rows = Vec::new();
    if line_height <= 0.0 || viewport.height <= 0.0 || total == 0 {
        return rows;
    }
    let bottom = viewport.y + viewport.height;
    let mut y = viewport.y - scroll_offset(buffer, line_height);
    let mut line = buffer.scroll_line.min(total - 1);
    let mut skip = if buffer.row_mode() {
        buffer.scroll_row
    } else {
        0
    };
    while line < total && y < bottom {
        let next = buffer.rope.line_range(line).end;
        let starts = buffer.row_starts(line);
        let count = starts.len();
        if count == 0 {
            // Folded away: jump past the whole fold.
            line = buffer.hidden_until(line).map_or(line + 1, |end| end + 1);
            skip = 0;
            continue;
        }
        for (index, &start) in starts.iter().enumerate().skip(skip.min(count - 1)) {
            if y >= bottom {
                break;
            }
            rows.push(ScreenRow {
                line,
                start,
                end: starts.get(index + 1).copied().unwrap_or(next),
                first: index == 0,
                last: index + 1 == count,
                y,
            });
            y += line_height;
        }
        skip = 0;
        line += 1;
    }
    rows
}

/// The indent step the guides are drawn at: the greatest common divisor of
/// the indents on screen, so two-space and four-space files both get one
/// guide per level. Nothing to go on means the tab width.
pub(super) fn indent_unit(indents: &[Option<usize>]) -> usize {
    fn gcd(a: usize, b: usize) -> usize {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    let unit = indents
        .iter()
        .flatten()
        .filter(|&&n| n > 0)
        .fold(0, |acc, &n| gcd(acc, n));
    if unit == 0 || unit > TAB_WIDTH {
        TAB_WIDTH
    } else {
        unit
    }
}

/// A blank line inside a block keeps the block's guides: the smaller of
/// the indents of the nearest non-blank lines above and below. Lines off
/// screen are read from the buffer, a bounded distance away.
pub(super) fn blank_line_indent(
    buffer: &Buffer,
    line: usize,
    lines: &[usize],
    indents: &[Option<usize>],
) -> usize {
    const REACH: usize = 200;
    let at = |l: usize| -> Option<usize> {
        match lines.binary_search(&l) {
            Ok(i) => indents[i],
            Err(_) => buffer.indent_columns(l),
        }
    };
    let above = (line.saturating_sub(REACH)..line).rev().find_map(at);
    let total = buffer.rope.len_lines();
    let below = (line + 1..(line + 1 + REACH).min(total)).find_map(at);
    match (above, below) {
        (Some(a), Some(b)) => a.min(b),
        _ => 0,
    }
}

/// The bracket touching the caret and its partner, as byte offsets in
/// order, or `None`. A closing bracket just before the caret is tried first,
/// then an opening one just after, which is how the caret usually lands
/// when typing or arrowing past a bracket. The search counts only the same
/// pair, ignores strings and comments, and gives up after a bounded run so
/// an unmatched bracket in a huge file costs the same as a matched one.
pub fn bracket_match(buffer: &Buffer) -> Option<(usize, usize)> {
    const LIMIT: usize = 128 * 1024;
    let rope = &buffer.rope;
    let caret = buffer.cursor();
    for at in [caret.checked_sub(1), Some(caret)].into_iter().flatten() {
        let (open, close, forward) = match rope.byte_at(at)? {
            b'(' => (b'(', b')', true),
            b'[' => (b'[', b']', true),
            b'{' => (b'{', b'}', true),
            b')' => (b'(', b')', false),
            b']' => (b'[', b']', false),
            b'}' => (b'{', b'}', false),
            _ => continue,
        };
        let found = if forward {
            let end = (at + 1).saturating_add(LIMIT).min(rope.len_bytes());
            let mut depth = 0usize;
            let mut pos = at + 1;
            let mut hit = None;
            'scan: for chunk in rope.bytes_in(at + 1..end) {
                for &byte in chunk {
                    if byte == open {
                        depth += 1;
                    } else if byte == close {
                        if depth == 0 {
                            hit = Some(pos);
                            break 'scan;
                        }
                        depth -= 1;
                    }
                    pos += 1;
                }
            }
            hit
        } else {
            let start = at.saturating_sub(LIMIT);
            let chunks: Vec<&[u8]> = rope.bytes_in(start..at).collect();
            let mut depth = 0usize;
            let mut pos = at;
            let mut hit = None;
            'scan: for chunk in chunks.iter().rev() {
                for &byte in chunk.iter().rev() {
                    pos -= 1;
                    if byte == close {
                        depth += 1;
                    } else if byte == open {
                        if depth == 0 {
                            hit = Some(pos);
                            break 'scan;
                        }
                        depth -= 1;
                    }
                }
            }
            hit
        };
        if let Some(other) = found {
            return Some((at.min(other), at.max(other)));
        }
    }
    None
}
