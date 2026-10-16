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
    let scroll_x = buffer.scroll_x() * m.advance;
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
        // Markdown off the caret: syntax hidden, table cells padded, on a
        // shaped line through its CoreText positions.
        let map = markdown.line(buffer, &touched, line);
        // A quote's `>` is drawn as a bar, except on a line being edited.
        let quote_bars = !touched.iter().any(|&(a, b)| a <= line && line <= b);
        let quote_bar = |out: &mut Vec<GlyphInstance>, x: f32, color| {
            out.push(GlyphInstance {
                pos: [x + (m.advance - 2.0) * 0.5, y],
                size: [2.0, m.line_height],
                uv: solid,
                color,
                ..Default::default()
            });
        };
        let placed = shaped_line
            .as_deref()
            .map(|shaped| Placed::new(shaped, buffer, line_start, map, m.advance));
        let offset_at = |byte: usize| -> f32 {
            if let Some(placed) = &placed {
                placed.x(byte)
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
            let intervals = if let Some(shaped) = shaped_line
                .as_ref()
                .filter(|_| !placed.as_ref().is_some_and(Placed::remapped))
            {
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
        if let Some(shaped) = &shaped_line {
            let placed = placed.as_ref().expect("a shaped line is placed");
            // Pads push glyphs right and hidden runs pull them left, so the
            // glyphs to look at reach that much further either way.
            let (pads, hidden) = placed.spread();
            let utf16_bytes = &shaped.source_bytes;
            for glyph in shaped.visible_glyphs(
                scroll_x - pads,
                scroll_x + viewport.x + viewport.width - text_x + hidden,
                cell_w * 2.0,
            ) {
                let byte = line_start + utf16_bytes[glyph.source_utf16.min(utf16_bytes.len() - 1)];
                if wrapping && (byte < row_start || byte >= row_end) {
                    continue;
                }
                if placed.hides(byte) {
                    continue;
                }
                let span_at = spans.partition_point(|s| s.end <= byte);
                let span = spans
                    .get(span_at)
                    .filter(|s| s.start <= byte && byte < s.end);
                let color = span.map_or(theme.text, |s| theme.syntax(s.kind));
                let x = text_x + glyph.x + placed.shift(byte, true) - scroll_x;
                if x + cell_w * 2.0 <= text_x || x > viewport.x + viewport.width {
                    continue;
                }
                if quote_bars
                    && span.is_some_and(|s| s.kind == Kind::MdMarker)
                    && quote_mark(buffer, line_start, byte)
                {
                    quote_bar(out, x, color);
                    continue;
                }
                let Some(slot) = atlas.slot_for_shaped(shaped, glyph) else {
                    stats.unsupported += 1;
                    continue;
                };
                let glyph = GlyphInstance {
                    pos: [x + slot.dx, y + glyph_dy],
                    size: [cell_w * slot.cells as f32, cell_h],
                    uv: slot.uv,
                    flags: slot.flags(),
                    color,
                    ..Default::default()
                };
                out.push(glyph);
                // A shaped line is set in one face: Markdown's bold is the
                // same glyph again one device pixel to the right.
                if span.is_some_and(|s| s.kind.bold()) {
                    out.push(GlyphInstance {
                        pos: [glyph.pos[0] + 1.0 / atlas.metrics.scale, glyph.pos[1]],
                        ..glyph
                    });
                }
            }
            for quad in &mut out[source_quads..] {
                clip_horizontal(quad, text_x, viewport.x + viewport.width);
            }
            shaped_carets.push((row_index, shaped.clone(), row_x0, map));
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
        // Only pipes, dashes, colons and spaces: a table's delimiter row.
        let delimiter_row = map.is_some_and(|map| !map.pads.is_empty())
            && buffer.rope.chunks_in(line_start..line_end).all(|c| {
                c.chars()
                    .all(|ch| matches!(ch, '|' | '-' | ':' | ' ' | '\t' | '\r' | '\n'))
            });
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
                    let pad = map.pad_at(byte);
                    // A delimiter row's cell runs on in dashes to its pipe.
                    if pad > 0
                        && delimiter_row
                        && byte > line_start
                        && buffer.rope.byte_at(byte - 1) == Some(b'-')
                        && let Some(slot) = atlas.slot_for_fallback('-')
                    {
                        let color = theme.syntax(Kind::MdMarker);
                        for k in 0..pad {
                            let x =
                                text_x + (column as f32 + shift + k as f32) * m.advance - scroll_x;
                            if x >= text_x && x <= viewport.x + viewport.width {
                                out.push(GlyphInstance {
                                    pos: [x, y + glyph_dy],
                                    size: [cell_w * slot.cells as f32, cell_h],
                                    uv: slot.uv,
                                    flags: slot.flags(),
                                    color,
                                    ..Default::default()
                                });
                            }
                        }
                    }
                    shift += pad as f32;
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

                if ch == '>'
                    && quote_bars
                    && span.is_some_and(|s| s.kind == Kind::MdMarker)
                    && quote_mark(buffer, line_start, byte)
                {
                    quote_bar(out, x, color);
                    column += 1;
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
        let x = if let Some((_, shaped, row_x0, map)) =
            shaped_carets.iter().find(|(index, ..)| *index == row_index)
        {
            let placed = Placed::new(shaped, buffer, line_start, *map, m.advance);
            text_x + placed.x(caret) - scroll_x - row_x0
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
/// which the right-aligned line numbers never reach. The cell is where a
/// click on a mark lands ([`change_bar_at`]), so it points there.
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
                hotspot(
                    Viewport {
                        x: viewport.x,
                        y,
                        width: m.advance,
                        height,
                    },
                    Cursor::Pointing,
                    None,
                );
            }
            MarkKind::Removed => {
                // Between two lines, so it may sit on the boundary just
                // past the last visible row, or past the last line.
                if mark.line < first || mark.line > last {
                    continue;
                }
                push_rect(out, atlas, [x, y - 1.5], [8.0, 3.0], theme.diff_removed);
                hotspot(
                    Viewport {
                        x: viewport.x,
                        y: y - m.line_height * 0.25,
                        width: m.advance,
                        height: m.line_height * 0.5,
                    },
                    Cursor::Pointing,
                    None,
                );
            }
        }
    }
    for quad in &mut out[drawn_from..] {
        clip_vertical(quad, viewport.y, viewport.y + viewport.height);
    }
}

/// How strongly the blame heat strip shows a line last changed
/// `age_seconds` ago: full for a day, fading over a year, then not at all.
pub fn heat(age_seconds: i64) -> f32 {
    const DAY: i64 = 86_400;
    match age_seconds {
        a if a < DAY => 1.0,
        a if a < 7 * DAY => 0.8,
        a if a < 30 * DAY => 0.55,
        a if a < 90 * DAY => 0.35,
        a if a < 365 * DAY => 0.18,
        _ => 0.0,
    }
}

/// Where the blame heat strip sits, from the gutter's left edge, and how
/// wide it is, for a cell `advance` wide. Right of the change bar (`2..5`)
/// when the first cell has room before the line numbers start; at small
/// sizes it has none, and the strip takes the gap left of the bar.
pub fn heat_strip(advance: f32) -> (f32, f32) {
    const FROM: f32 = 6.0;
    let room = advance - 0.5 - FROM;
    if room >= 1.0 {
        (FROM, room.min(2.0))
    } else {
        (0.0, 1.5)
    }
}

/// One document's blame, as the heat strip draws it.
pub struct BlameHeat<'a> {
    /// Per line, an index into `alpha` and `tips`, or
    /// [`crate::project::git::NOT_COMMITTED`].
    pub lines: &'a [u32],
    /// Per commit, the strip's opacity: [`heat`] of its age.
    pub alpha: &'a [f32],
    /// Per commit, what the strip's tooltip says.
    pub tips: &'a [String],
    /// Per commit, whether it is known to be unsigned: its strip is
    /// dashed. A commit missing here is drawn solid.
    pub dashed: &'a [bool],
    /// Lines from here down were edited since the blame and are not drawn.
    pub hide_from: usize,
}

/// The dashes of an unsigned commit's strip: this long, this far apart.
pub const HEAT_DASH: f32 = 3.0;
pub const HEAT_GAP: f32 = 2.0;

/// The pieces a strip from `top` to `bottom` is drawn as: one, or dashes
/// from the top down, the last cut at the bottom.
pub fn heat_pieces(top: f32, bottom: f32, dashed: bool) -> Vec<(f32, f32)> {
    if !dashed {
        return vec![(top, bottom)];
    }
    let mut out = Vec::new();
    let mut y = top;
    while y < bottom {
        out.push((y, (y + HEAT_DASH).min(bottom)));
        y += HEAT_DASH + HEAT_GAP;
    }
    out
}

/// Draws the blame heat strip down the gutter's first cell: per line, the
/// heat colour as strong as its commit is recent, nothing for a line not
/// committed yet or older than a year; dashed for an unsigned commit. A
/// run of rows blamed to one commit is one strip, and one hotspot whose
/// tooltip names the commit.
pub fn push_blame_heat(
    out: &mut Vec<GlyphInstance>,
    atlas: &Atlas,
    viewport: Viewport,
    rows: &[ScreenRow],
    theme: &Theme,
    heat: &BlameHeat,
) {
    let m = atlas.metrics;
    let (dx, width) = heat_strip(m.advance);
    let x = m.snap(viewport.x + dx);
    let drawn_from = out.len();
    let commit_of = |row: &ScreenRow| {
        (row.line < heat.hide_from)
            .then(|| heat.lines.get(row.line).copied())
            .flatten()
            .filter(|&c| heat.alpha.get(c as usize).is_some_and(|&a| a > 0.0))
    };
    let flush = |out: &mut Vec<GlyphInstance>, (commit, top, bottom): (u32, f32, f32)| {
        let color = faded(theme.blame_heat, heat.alpha[commit as usize]);
        let dashed = heat.dashed.get(commit as usize).copied().unwrap_or(false);
        for (from, to) in heat_pieces(top, bottom, dashed) {
            push_rect(out, atlas, [x, from], [width, to - from], color);
        }
        hotspot(
            Viewport {
                x: viewport.x,
                y: top,
                width: m.advance,
                height: bottom - top,
            },
            Cursor::Pointing,
            heat.tips.get(commit as usize).map(String::as_str),
        );
    };
    // The commit, top and bottom of the rows being gathered.
    let mut run: Option<(u32, f32, f32)> = None;
    for row in rows {
        let commit = commit_of(row);
        match (run, commit) {
            (Some((c, top, bottom)), Some(n)) if c == n && (row.y - bottom).abs() < 0.5 => {
                run = Some((c, top, row.y + m.line_height));
            }
            _ => {
                if let Some(done) = run {
                    flush(out, done);
                }
                run = commit.map(|c| (c, row.y, row.y + m.line_height));
            }
        }
    }
    if let Some(done) = run {
        flush(out, done);
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

/// The line whose change mark is under a point in the text area's own
/// coordinates (as [`offset_at_point`] takes them): the gutter's first
/// cell, where [`push_gutter_marks`] draws. A deletion's stub sits on the
/// boundary between two lines, so it is hit from a quarter of a line either
/// side of it, and answers the line below, which it marks.
pub fn change_bar_at(
    buffer: &Buffer,
    atlas: &Atlas,
    marks: &[crate::project::git::Mark],
    x: f32,
    y: f32,
) -> Option<usize> {
    use crate::project::git::MarkKind;
    let m = atlas.metrics;
    if !(0.0..m.advance).contains(&x) || marks.is_empty() {
        return None;
    }
    let rows = rows_down_to(buffer, y, m.line_height);
    let row = rows.iter().find(|r| r.y <= y && y < r.y + m.line_height)?;
    let kind = |line: usize| {
        let i = marks.partition_point(|mark| mark.line < line);
        marks
            .get(i)
            .filter(|mark| mark.line == line)
            .map(|mark| mark.kind)
    };
    match kind(row.line) {
        Some(MarkKind::Added | MarkKind::Modified) => return Some(row.line),
        Some(MarkKind::Removed) if row.first && y < row.y + m.line_height * 0.25 => {
            return Some(row.line);
        }
        _ => {}
    }
    (row.last && y >= row.y + m.line_height * 0.75 && kind(row.line + 1) == Some(MarkKind::Removed))
        .then_some(row.line + 1)
}

/// The line whose blame heat a click at `(x, y)` in the text area is on:
/// anywhere in the gutter's first cell, where the strip and its pointer
/// are, beside a line the blame still holds (not one edited since). The
/// line may not be committed yet, or too old to draw; the caller says so.
pub fn heat_strip_at(
    buffer: &Buffer,
    atlas: &Atlas,
    heat: &BlameHeat,
    x: f32,
    y: f32,
) -> Option<usize> {
    let m = atlas.metrics;
    if !(0.0..m.advance).contains(&x) {
        return None;
    }
    let rows = rows_down_to(buffer, y, m.line_height);
    let row = rows.iter().find(|r| r.y <= y && y < r.y + m.line_height)?;
    (row.line < heat.hide_from && row.line < heat.lines.len()).then_some(row.line)
}

/// What the peek at one change says: a title, and the lines HEAD had where
/// the change is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangePeek {
    pub kind: crate::project::git::MarkKind,
    pub title: String,
    /// HEAD's lines the change replaced, without line breaks, tabs
    /// expanded. Empty for an addition.
    pub head: Vec<String>,
}

/// The peek's content for `change`, from `head`, the file as of HEAD.
pub fn change_peek(head: &str, change: &crate::ide::diff::Change) -> ChangePeek {
    use crate::project::git::MarkKind;
    let lines = |n: usize| if n == 1 { "line" } else { "lines" };
    let added = change.new_end - change.new_start;
    let removed = change.old_end.saturating_sub(change.old_start);
    let title = match change.kind {
        MarkKind::Added => format!("Added {added} {}", lines(added)),
        MarkKind::Removed => format!("Removed {removed} {}", lines(removed)),
        MarkKind::Modified => format!("Changed {added} {}, {removed} in HEAD", lines(added)),
    };
    let head = head
        .split_inclusive('\n')
        .skip(change.old_start)
        .take(removed)
        .map(expanded_line)
        .collect();
    ChangePeek {
        kind: change.kind,
        title,
        head,
    }
}

/// `line` without its line break, its tabs expanded to the columns they
/// reach: a line of another text, drawn cell by cell.
pub fn expanded_line(line: &str) -> String {
    let mut out = String::new();
    let mut column = 0;
    for ch in line.trim_end_matches(['\n', '\r']).chars() {
        if ch == '\t' {
            let next = (column / TAB_WIDTH + 1) * TAB_WIDTH;
            out.extend(std::iter::repeat_n(' ', next - column));
            column = next;
        } else {
            out.push(ch);
            column += display_width(ch);
        }
    }
    out
}

/// Where the peek's controls were drawn, for clicks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PeekHits {
    pub panel: Viewport,
    pub revert: Viewport,
    pub stage: Viewport,
    pub close: Viewport,
}

/// Lines of HEAD a peek shows before it says how many more there are.
const PEEK_MAX_LINES: usize = 12;
/// The peek's title row.
const PEEK_HEADER: f32 = UI_CONTROL_SM + 14.0;
/// Space around the peek's body inside the card.
const PEEK_PAD: f32 = 8.0;
/// Where the code starts in the body, after the `−` marker.
const PEEK_CODE_X: f32 = 24.0;

/// The peek at one change: a card under the change's last line (over its
/// first when there is no room below), with a title, Revert, Stage and
/// close, and what HEAD had there drawn as Source Control draws removed
/// lines. `None` when the change is not on screen.
#[allow(clippy::too_many_arguments)]
pub fn build_change_peek(
    peek: &ChangePeek,
    change: &crate::ide::diff::Change,
    buffer: &Buffer,
    text: Viewport,
    rows: &[ScreenRow],
    atlas: &mut Atlas,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) -> Option<PeekHits> {
    use crate::project::git::MarkKind;
    let m = atlas.metrics;
    let lh = m.line_height;
    // The change's extent on screen: from the top of its first row to the
    // bottom of its last. A deletion holds the line under its boundary,
    // where the caret goes, so the peek does not cover that line.
    let (top, bottom) = if change.kind == MarkKind::Removed {
        rows.iter()
            .find(|r| r.line == change.new_start && r.first)
            .map(|r| (r.y, r.y + lh))
            .or_else(|| {
                rows.last()
                    .filter(|r| r.line + 1 == change.new_start)
                    .map(|r| (r.y + lh, r.y + lh))
            })?
    } else {
        let mut on = rows
            .iter()
            .filter(|r| (change.new_start..change.new_end).contains(&r.line));
        let first = on.next()?;
        let last = on.next_back().unwrap_or(first);
        (first.y, last.y + lh)
    };

    let gutter = gutter_width(buffer, atlas);
    let x = (text.x + gutter - m.advance * 0.5).max(text.x + 4.0);
    let room_w = (text.x + text.width - x - 12.0).max(0.0);

    // The body: HEAD's lines, as many as fit, or a note for an addition.
    let room_below = text.y + text.height - bottom - 4.0;
    let room_above = top - text.y - 4.0;
    let fits = ((room_below.max(room_above) - PEEK_HEADER - PEEK_PAD * 2.0) / lh).floor();
    let budget = (fits.max(1.0) as usize).min(PEEK_MAX_LINES);
    let (shown, more) = if peek.head.len() > budget {
        let shown = budget.saturating_sub(1).max(1);
        (shown, peek.head.len() - shown)
    } else {
        (peek.head.len(), 0)
    };
    let body_rows = if peek.head.is_empty() {
        1
    } else {
        shown + usize::from(more > 0)
    };
    let height = PEEK_HEADER + body_rows as f32 * lh + PEEK_PAD * 2.0;

    // Wide enough for the longest line and the header, at most the text.
    let note = "Added lines, nothing in HEAD";
    let close_w = UI_CONTROL_SM;
    let revert = Button::new(Viewport::new(0.0, UI_CONTROL_SM))
        .label("Revert")
        .icon(icons::DISCARD)
        .tip("Put back what HEAD has here");
    let stage = Button::new(Viewport::new(0.0, UI_CONTROL_SM))
        .label("Stage")
        .icon(icons::ADD)
        .tip("Stage this change");
    let revert_w = revert.content_width(atlas, 8.0);
    let stage_w = stage.content_width(atlas, 8.0);
    let title_w = ui_text_width(atlas, &peek.title);
    let header_w =
        12.0 + 10.0 + title_w + 24.0 + revert_w + UI_GAP + stage_w + UI_GAP + close_w + 8.0;
    let longest = peek
        .head
        .iter()
        .take(shown)
        .map(|l| l.chars().map(display_width).sum::<usize>())
        .max()
        .unwrap_or(0);
    let body_w = if peek.head.is_empty() {
        ui_text_width(atlas, note) + PEEK_PAD * 4.0
    } else {
        PEEK_CODE_X + longest as f32 * m.advance + PEEK_PAD * 3.0
    };
    let width = header_w.max(body_w).max(360.0).min(room_w);
    if width <= 0.0 {
        return None;
    }

    let y = if height <= room_below || room_below >= room_above {
        bottom + 2.0
    } else {
        top - 2.0 - height
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

    // The header: what kind of change, its title, and the controls.
    let header = Viewport {
        height: PEEK_HEADER,
        ..panel
    };
    let kind_colour = match peek.kind {
        MarkKind::Added => theme.diff_added,
        MarkKind::Modified => theme.diff_modified,
        MarkKind::Removed => theme.diff_removed,
    };
    push_rounded_rect(
        out,
        Viewport {
            x: header.x + 12.0,
            y: header.y + (PEEK_HEADER - 14.0) * 0.5,
            width: 3.0,
            height: 14.0,
        },
        1.5,
        kind_colour,
    );
    let button_y = header.y + (PEEK_HEADER - UI_CONTROL_SM) * 0.5;
    let close = Viewport {
        x: panel.x + panel.width - 8.0 - close_w,
        y: button_y,
        width: close_w,
        height: UI_CONTROL_SM,
    };
    let stage_rect = Viewport {
        x: close.x - UI_GAP - stage_w,
        width: stage_w,
        ..close
    };
    let revert_rect = Viewport {
        x: stage_rect.x - UI_GAP - revert_w,
        width: revert_w,
        ..close
    };
    let title_x = header.x + 22.0;
    push_ui_text(
        out,
        atlas,
        Viewport {
            x: title_x,
            width: (revert_rect.x - 12.0 - title_x).max(0.0),
            ..header
        },
        &peek.title,
        theme.text,
    );
    Button {
        rect: revert_rect,
        ..revert
    }
    .draw(out, atlas, theme);
    Button {
        rect: stage_rect,
        ..stage
    }
    .draw(out, atlas, theme);
    Button::new(close)
        .icon(icons::CLOSE)
        .tone(Tone::Ghost)
        .tip("Close (Esc)")
        .draw(out, atlas, theme);
    push_rect(
        out,
        atlas,
        [panel.x + 1.0, header.y + PEEK_HEADER - 1.0],
        [panel.width - 2.0, 1.0 / m.scale],
        theme.palette_border,
    );

    // The body: what HEAD had, as Source Control draws a removed line.
    let body = Viewport {
        x: panel.x + PEEK_PAD,
        y: header.y + PEEK_HEADER + PEEK_PAD,
        width: panel.width - PEEK_PAD * 2.0,
        height: body_rows as f32 * lh,
    };
    if peek.head.is_empty() {
        push_ui_text(
            out,
            atlas,
            Viewport {
                x: body.x + 6.0,
                width: body.width - 6.0,
                ..body
            },
            note,
            theme.status_text,
        );
    } else {
        push_rounded_rect(
            out,
            Viewport {
                height: shown as f32 * lh,
                ..body
            },
            UI_RADIUS_SM,
            theme.diff_removed_band,
        );
        let columns = ((body.width - PEEK_CODE_X - PEEK_PAD) / m.advance).max(0.0) as usize;
        for (i, line) in peek.head.iter().take(shown).enumerate() {
            let row = Viewport {
                y: body.y + i as f32 * lh,
                height: lh,
                ..body
            };
            push_ui_text(
                out,
                atlas,
                Viewport {
                    x: row.x + 8.0,
                    width: 10.0,
                    ..row
                },
                "\u{2212}",
                theme.diff_removed,
            );
            let shown = crate::text::columns::fit_columns(line.chars(), columns);
            push_text(
                out,
                atlas,
                row.x + PEEK_CODE_X,
                row.y,
                &shown,
                theme.diff_removed,
            );
        }
        if more > 0 {
            push_ui_text(
                out,
                atlas,
                Viewport {
                    x: body.x + PEEK_CODE_X,
                    y: body.y + shown as f32 * lh,
                    width: body.width - PEEK_CODE_X,
                    height: lh,
                },
                &format!(
                    "\u{2026} {more} more {}",
                    if more == 1 { "line" } else { "lines" }
                ),
                theme.status_text,
            );
        }
    }
    Some(PeekHits {
        panel,
        revert: revert_rect,
        stage: stage_rect,
        close,
    })
}

/// Washes behind the rows of each of `tints`, a range of lines and its
/// colour, across the text: what a step through a file's history changed.
/// They go under the text. `rows` are the frame's screen rows in `text`.
pub fn line_tints(
    rows: &[ScreenRow],
    atlas: &Atlas,
    text: Viewport,
    tints: &[(std::ops::Range<usize>, [f32; 4])],
) -> Vec<GlyphInstance> {
    let mut out = Vec::new();
    if tints.is_empty() {
        return out;
    }
    for row in rows {
        if let Some((_, colour)) = tints.iter().find(|(lines, _)| lines.contains(&row.line)) {
            push_rect(
                &mut out,
                atlas,
                [text.x, row.y],
                [text.width, atlas.metrics.line_height],
                *colour,
            );
        }
    }
    for quad in &mut out {
        clip_vertical(quad, text.y, text.y + text.height);
    }
    out
}

/// Lines a step through a file's history removed, drawn for a moment where
/// they were.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ghost {
    /// The line they were above in the text shown: the line count when
    /// they were at its end.
    pub at: usize,
    /// The first few, without line breaks, tabs expanded.
    pub lines: Vec<String>,
    /// How many more there were.
    pub more: usize,
}

/// Of a ghost's `alpha`, the part below which its text is gone and its
/// cover starts to go.
const GHOST_HANDOFF: f32 = 0.25;

/// `ghost` over the text from its boundary down, as the peek draws removed
/// lines: `diff_removed` on `diff_removed_band`, over the editor's own
/// background. An overlay only: the rows under it stay where they are, for
/// the pointer as for the eye. Nothing when the boundary is off screen.
///
/// Two texts in one place cannot fade into each other legibly, so as
/// `alpha` goes from 1 to 0 the ghost's lines fade first, with the cover
/// still whole, and only once they are gone does the cover fade off the
/// rows under it.
#[allow(clippy::too_many_arguments)]
pub fn push_ghost(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    buffer: &Buffer,
    text: Viewport,
    rows: &[ScreenRow],
    ghost: &Ghost,
    alpha: f32,
    theme: &Theme,
) {
    let m = atlas.metrics;
    let lh = m.line_height;
    let top = rows
        .iter()
        .find(|r| r.line == ghost.at && r.first)
        .map(|r| r.y)
        .or_else(|| {
            rows.last()
                .filter(|r| r.last && r.line + 1 == ghost.at)
                .map(|r| r.y + lh)
        });
    let Some(top) = top else {
        return;
    };
    let count = ghost.lines.len() + usize::from(ghost.more > 0);
    let code_x = text.x + gutter_width(buffer, atlas);
    let x = code_x - m.advance * 0.5;
    let width = (text.x + text.width - x).max(0.0);
    let height = count as f32 * lh;
    let mut quads = Vec::new();
    let cover = (alpha / GHOST_HANDOFF).clamp(0.0, 1.0);
    let words = ((alpha - GHOST_HANDOFF) / (1.0 - GHOST_HANDOFF)).clamp(0.0, 1.0);
    let ground = theme.background.map(|c| c as f32);
    push_rect(
        &mut quads,
        atlas,
        [x, top],
        [width, height],
        faded(ground, cover),
    );
    push_rect(
        &mut quads,
        atlas,
        [x, top],
        [width, height],
        faded(theme.diff_removed_band, cover),
    );
    push_rect(
        &mut quads,
        atlas,
        [x, top],
        [2.0, height],
        faded(theme.diff_removed, cover),
    );
    if words <= 0.0 {
        for quad in &mut quads {
            clip_vertical(quad, text.y, text.y + text.height);
        }
        out.extend(quads);
        return;
    }
    let columns = ((text.x + text.width - code_x) / m.advance).max(0.0) as usize;
    let ink = faded(theme.diff_removed, words);
    // A column a trackpad has partly scrolled past is drawn and cut at the
    // gutter, as the text under the ghost is.
    let shift = buffer.scroll_x_fraction * m.advance;
    let lines_from = quads.len();
    for (i, line) in ghost.lines.iter().enumerate() {
        let shown =
            crate::text::columns::fit_columns(line.chars().skip(buffer.scroll_column), columns + 1);
        push_text(
            &mut quads,
            atlas,
            code_x - shift,
            top + i as f32 * lh,
            &shown,
            ink,
        );
    }
    for quad in &mut quads[lines_from..] {
        clip_horizontal(quad, code_x, text.x + text.width);
    }
    if ghost.more > 0 {
        push_ui_text(
            &mut quads,
            atlas,
            Viewport {
                x: code_x,
                y: top + ghost.lines.len() as f32 * lh,
                width: (text.x + text.width - code_x).max(0.0),
                height: lh,
            },
            &format!(
                "\u{2026} {} more {}",
                ghost.more,
                if ghost.more == 1 { "line" } else { "lines" }
            ),
            faded(theme.status_text, words),
        );
    }
    for quad in &mut quads {
        clip_vertical(quad, text.y, text.y + text.height);
    }
    out.extend(quads);
}

/// Squiggles under each of `marks`, a range of `buffer`'s text and its
/// colour, on every visible row the range touches: how a diagnostic is
/// shown. The visible rows are worked out once for all of them, and a range
/// off screen costs two line lookups.
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
    let scroll_x = buffer.scroll_x() * m.advance;
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
        // The wave runs under the glyph cell, in the row's own leading, so
        // its crests clear an underscore (which sits lower than the font's
        // underline); it never leaves the row.
        let cell_bottom = row.y + m.glyph_dy(m.line_height) + m.ascent + m.descent;
        let middle = (cell_bottom + 0.75)
            .min(row.y + m.line_height - SQUIGGLE_AMPLITUDE - SQUIGGLE_THICKNESS * 0.5);
        if middle - SQUIGGLE_AMPLITUDE - SQUIGGLE_THICKNESS < text.y
            || middle + SQUIGGLE_AMPLITUDE + SQUIGGLE_THICKNESS > text.y + text.height
        {
            continue;
        }
        push_squiggle(out, x0, x1, text_x - scroll_x, middle, color, m.scale);
    }
}

/// How far a squiggle swings either side of its middle, its wavelength and
/// its stroke, in points.
const SQUIGGLE_AMPLITUDE: f32 = 0.85;
const SQUIGGLE_WAVELENGTH: f32 = 4.0;
const SQUIGGLE_THICKNESS: f32 = 1.0;

/// A smooth wave from `x0` to `x1` around `middle`: what every editor draws
/// for a problem, where a straight line is a link or an underline. Drawn
/// as round-ended dabs a device pixel apart, which overlap into one
/// anti-aliased stroke. Its phase counts from `origin` (the text's left
/// edge), so the wave stays put as the line around it changes.
pub(super) fn push_squiggle(
    out: &mut Vec<GlyphInstance>,
    x0: f32,
    x1: f32,
    origin: f32,
    middle: f32,
    color: [f32; 4],
    scale: f32,
) {
    let step = 1.0 / scale.max(1.0);
    let half = SQUIGGLE_THICKNESS * 0.5;
    let mut x = x0;
    while x < x1 {
        let phase = (x - origin) / SQUIGGLE_WAVELENGTH * std::f32::consts::TAU;
        let y = middle - SQUIGGLE_AMPLITUDE * phase.sin();
        push_rounded_rect(
            out,
            Viewport {
                x: x - half,
                y: y - half,
                width: (step + SQUIGGLE_THICKNESS).min(x1 - x + SQUIGGLE_THICKNESS),
                height: SQUIGGLE_THICKNESS,
            },
            half,
            color,
        );
        x += step;
    }
}

/// Whether a row break of `line` falls in or against right-to-left text
/// (Hebrew, Arabic and the scripts after them): a wrapped row cut out of
/// the whole line's shaping is wrong there, and only there.
pub(super) fn has_rtl(buffer: &Buffer, line: usize) -> bool {
    let rtl = is_rtl;
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

/// Whether the `>` at `byte` opens a quote: nothing but spaces and other
/// `>` before it on its line.
fn quote_mark(buffer: &Buffer, line_start: usize, byte: usize) -> bool {
    buffer.rope.byte_at(byte) == Some(b'>')
        && (line_start..byte).all(|b| matches!(buffer.rope.byte_at(b), Some(b' ' | b'>')))
}

/// Hebrew, Arabic and the right-to-left scripts after them.
fn is_rtl(c: char) -> bool {
    matches!(c as u32,
        0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF | 0x10800..=0x10FFF | 0x1E800..=0x1EFFF)
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
        let scrolled = buffer.scroll_x() * m.advance;
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
            let map = markdown.line(buffer, &touched_lines(buffer), row.line);
            let placed = Placed::new(shaped, buffer, line_start, map, m.advance);
            let x0 = placed.x(row.start);
            let at = placed.byte_at(x - text_x + x0 + scrolled);
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
        let target_x = x - text_x + buffer.scroll_x() * m.advance;
        let map = markdown.line(buffer, &touched_lines(buffer), line);
        return Placed::new(shaped, buffer, start, map, m.advance).byte_at(target_x);
    }
    let column = ((x - text_x) / m.advance).max(0.0) + buffer.scroll_x();
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
            let placed = Placed::new(shaped, buffer, line_start, map, m.advance);
            placed.x(caret) - placed.x(row.start)
        } else if let Some(map) = &map {
            (map.column(&buffer.rope, caret) - map.column(&buffer.rope, row.start)) * m.advance
        } else {
            crate::text::wrap::column_in_row(&buffer.rope, row.start, caret) as f32 * m.advance
        };
        let x = x - buffer.scroll_x() * m.advance;
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
        let placed = Placed::new(shaped, buffer, line_start, map, m.advance);
        let x = text.x + gutter_width(buffer, atlas) + placed.x(buffer.cursor())
            - buffer.scroll_x() * m.advance;
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
    } - buffer.scroll_x();
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
    pub(super) fn line(
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

/// A shaped line where it is drawn: CoreText's positions, less the width
/// of the Markdown syntax hidden before a byte, plus the table pads. A line
/// holding right-to-left text is drawn as shaped, syntax and all: a hidden
/// run could cut through its visual order.
pub(super) struct Placed<'a> {
    shaped: &'a crate::render::font::ShapedLine,
    start: usize,
    map: Option<LineMap<'a>>,
    advance: f32,
}

impl<'a> Placed<'a> {
    pub(super) fn new(
        shaped: &'a crate::render::font::ShapedLine,
        buffer: &Buffer,
        start: usize,
        map: Option<LineMap<'a>>,
        advance: f32,
    ) -> Self {
        let map = map.filter(|map| {
            !buffer
                .rope
                .chunks_in(map.start..map.end)
                .any(|chunk| chunk.chars().any(is_rtl))
        });
        Placed {
            shaped,
            start,
            map,
            advance,
        }
    }

    /// Whether Markdown moves anything on this line.
    pub(super) fn remapped(&self) -> bool {
        self.map.is_some()
    }

    fn raw(&self, byte: usize) -> f32 {
        self.shaped.x_of_byte(byte.saturating_sub(self.start))
    }

    /// Whether the syntax at `byte` is hidden.
    pub(super) fn hides(&self, byte: usize) -> bool {
        self.map.is_some_and(|map| map.hides(byte))
    }

    /// How far what is at `byte` moves from where CoreText put it: left by
    /// the hidden runs before it, right by the pads before it, and by its
    /// own pad too when `own_pad` (a glyph is drawn after its pad, a caret
    /// before it).
    pub(super) fn shift(&self, byte: usize, own_pad: bool) -> f32 {
        let Some(map) = &self.map else {
            return 0.0;
        };
        let mut shift = 0.0;
        for h in map.hidden.iter().take_while(|h| h.start < byte) {
            let (from, to) = (h.start.max(map.start), h.end.min(byte));
            if to > from {
                shift -= self.raw(to) - self.raw(from);
            }
        }
        for p in map
            .pads
            .iter()
            .take_while(|p| p.0 < byte || (own_pad && p.0 == byte))
        {
            shift += p.1 as f32 * self.advance;
        }
        shift
    }

    /// Where the caret at `byte` is drawn, in points from the line start.
    pub(super) fn x(&self, byte: usize) -> f32 {
        let byte = self.map.map_or(byte, |map| byte.clamp(map.start, map.end));
        self.raw(byte) + self.shift(byte, false)
    }

    /// The pads' total width and the hidden runs' total width.
    pub(super) fn spread(&self) -> (f32, f32) {
        let Some(map) = &self.map else {
            return (0.0, 0.0);
        };
        let pads = map.pads.iter().map(|p| p.1 as f32).sum::<f32>() * self.advance;
        (pads, pads - self.shift(map.end, false))
    }

    /// The caret stop drawn nearest `x`, in points from the line start; the
    /// earliest of those drawn at the same place, which a hidden run makes
    /// several. Each stretch between hidden runs and pads moves as a whole,
    /// so it is CoreText's own answer inside the stretch, shifted.
    pub(super) fn byte_at(&self, x: f32) -> usize {
        let Some(map) = &self.map else {
            return self.start + self.shaped.byte_at_x(x);
        };
        let mut cuts = vec![map.start, map.end];
        for h in map.hidden {
            cuts.push(h.start.clamp(map.start, map.end));
            cuts.push(h.end.clamp(map.start, map.end));
        }
        cuts.extend(map.pads.iter().map(|p| p.0.clamp(map.start, map.end)));
        cuts.sort_unstable();
        cuts.dedup();
        let mut best = (map.start, f32::INFINITY);
        for pair in cuts.windows(2) {
            let (from, to) = (pair[0], pair[1]);
            if map.hides(from) {
                continue;
            }
            let shift = self.x(from) - self.raw(from);
            let at = (self.start + self.shaped.byte_at_x(x - shift)).clamp(from, to);
            let distance = (self.x(at) - x).abs();
            if distance < best.1 {
                best = (at, distance);
            }
        }
        if best.1.is_infinite() {
            return map.start;
        }
        // A hidden run is drawn where it ends: its start is the earliest
        // stop there, as on a line drawn by cells.
        let mut at = best.0;
        while let Some(h) = map.hidden.iter().find(|h| h.end == at && h.start < at) {
            at = h.start.max(map.start);
        }
        at
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_squiggle_is_one_unbroken_wave_inside_its_bounds() {
        let mut out = Vec::new();
        push_squiggle(&mut out, 10.0, 50.0, 10.0, 100.0, [1.0; 4], 2.0);
        // A dab every device pixel, each overlapping the next.
        assert_eq!(out.len(), 80);
        for pair in out.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            assert!(b.pos[0] < a.pos[0] + a.size[0], "a gap in the stroke");
            assert!((b.pos[1] - a.pos[1]).abs() < SQUIGGLE_THICKNESS, "a step");
        }
        let top = out.iter().map(|g| g.pos[1]).fold(f32::MAX, f32::min);
        let bottom = out
            .iter()
            .map(|g| g.pos[1] + g.size[1])
            .fold(f32::MIN, f32::max);
        let reach = SQUIGGLE_AMPLITUDE + SQUIGGLE_THICKNESS * 0.5;
        assert!(top >= 100.0 - reach - 0.01 && bottom <= 100.0 + reach + 0.01);
        // It swings: not a flat line.
        assert!(bottom - top > SQUIGGLE_AMPLITUDE);
        // Nothing past its end.
        assert!(
            out.iter()
                .all(|g| g.pos[0] + g.size[0] <= 50.0 + SQUIGGLE_THICKNESS)
        );
    }
}
