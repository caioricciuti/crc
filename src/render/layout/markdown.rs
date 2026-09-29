//! Markdown drawn as prose rather than source.

use super::*;

/// Draws parsed Markdown, read only, from block `scroll`, appending to
/// `out`: an extension's README. What it adds is cut to `viewport`: a code line wider than the pane, or a block's
/// background, must not draw into the next pane or over the status bar.
#[allow(clippy::too_many_arguments)]
pub fn build_markdown_appending(
    blocks: &[SpannedBlock],
    scroll: usize,
    atlas: &mut Atlas,
    viewport: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) -> usize {
    let first = out.len();
    let rows = build_markdown_unclipped(blocks, scroll, atlas, viewport, theme, out);
    let (left, right) = (viewport.x, viewport.x + viewport.width);
    let (top, bottom) = (viewport.y, viewport.y + viewport.height);
    for quad in &mut out[first..] {
        clip_horizontal(quad, left, right);
        clip_vertical(quad, top, bottom);
    }
    rows
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_markdown_unclipped(
    blocks: &[SpannedBlock],
    scroll: usize,
    atlas: &mut Atlas,
    viewport: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) -> usize {
    atlas.finish_shaping_frame();
    let m = atlas.metrics;
    let (cell_w, cell_h) = atlas.cell_size();
    let solid = atlas.solid_uv();
    let glyph_dy = m.glyph_dy(m.line_height);
    let hairline = 1.0 / m.scale;

    // Generous margins: a preview is for reading, not for editing, so it does
    // not want the gutter's density.
    let left = viewport.x + MD_MARGIN;
    let usable = (viewport.width - MD_MARGIN * 2.0).max(m.advance);
    let columns = (usable / m.advance).floor().max(1.0) as usize;

    let mut y = viewport.y + MD_MARGIN;
    let bottom = viewport.y + viewport.height;
    let mut rows_drawn = 0usize;

    for spanned in blocks.iter().skip(scroll) {
        if y >= bottom {
            break;
        }
        let block = &spanned.block;
        match block {
            Block::Blank => {
                y += m.snap(m.line_height * 0.5);
            }

            Block::Rule => {
                out.push(GlyphInstance {
                    pos: [left, y + m.line_height * 0.5],
                    size: [usable, hairline],
                    uv: solid,
                    color: theme.md_rule,
                    ..Default::default()
                });
                y += m.line_height;
            }

            Block::Heading { level, runs } => {
                // Extra air above a heading, but not at the very top.
                if y > viewport.y + MD_MARGIN {
                    y += m.line_height * 0.6;
                }
                // Headings are shaped at their own size in the bold face,
                // not monospace bitmaps stretched by the quad size, which is
                // what made them soft.
                let size = MD_BODY_PT * md_heading_scale(*level);
                let (_, _, end) = draw_prose(
                    out,
                    atlas,
                    runs,
                    left,
                    y,
                    usable,
                    size,
                    Face::Bold,
                    theme,
                    Some(theme.md_heading),
                    bottom,
                );
                y = end;

                // A rule under the top two levels, as a document would have.
                if *level <= 2 {
                    out.push(GlyphInstance {
                        pos: [left, y],
                        size: [usable, hairline],
                        uv: solid,
                        color: theme.md_rule,
                        ..Default::default()
                    });
                    y += m.line_height * 0.35;
                }
                rows_drawn += 1;
            }

            Block::Paragraph { runs } => {
                let (_, _, end) = draw_prose(
                    out,
                    atlas,
                    runs,
                    left,
                    y,
                    usable,
                    MD_BODY_PT,
                    Face::Regular,
                    theme,
                    None,
                    bottom,
                );
                y = end;
                rows_drawn += 1;
            }

            Block::Quote { runs, depth } => {
                let start = y;
                // Each level of quoting steps in and gets its own bar, so a
                // nested quote reads as nested instead of showing a literal
                // ">" in the text.
                let indent = MD_QUOTE_INDENT * (*depth as f32 + 1.0);
                let (_, _, end) = draw_prose(
                    out,
                    atlas,
                    runs,
                    left + indent,
                    y,
                    (usable - indent).max(0.0),
                    MD_BODY_PT,
                    Face::Regular,
                    theme,
                    Some(theme.syn_comment),
                    bottom,
                );
                // One bar per level, each spanning however many rows the text
                // wrapped onto.
                for level in 0..=*depth {
                    out.push(GlyphInstance {
                        pos: [left + MD_QUOTE_INDENT * level as f32, start],
                        size: [2.0, (end - start).max(m.line_height)],
                        uv: solid,
                        color: theme.md_quote_bar,
                        ..Default::default()
                    });
                }
                y = end;
                rows_drawn += 1;
            }

            Block::Code { lang, lines } => {
                let height = m.line_height * (lines.len() + 1) as f32 + m.line_height * 0.4;
                out.push(GlyphInstance {
                    pos: [left, y],
                    size: [usable, height],
                    uv: solid,
                    color: theme.md_code_background,
                    ..Default::default()
                });
                y += m.line_height * 0.2;
                if !lang.is_empty() {
                    push_text(out, atlas, left + m.advance, y, lang, theme.gutter_text);
                }
                y += m.line_height;
                for line in lines {
                    if y >= bottom {
                        break;
                    }
                    push_text(out, atlas, left + m.advance, y, line, theme.syn_string);
                    y += m.line_height;
                }
                y += m.line_height * 0.2;
                rows_drawn += 1;
            }

            Block::ListItem {
                depth,
                number,
                task,
                runs,
            } => {
                let indent = left + (*depth as f32) * MD_LIST_INDENT;
                let marker = match (task, number) {
                    (Some(true), _) => "\u{2611} ".to_string(),
                    (Some(false), _) => "\u{2610} ".to_string(),
                    (None, Some(n)) => format!("{n}. "),
                    (None, None) => "\u{2022} ".to_string(),
                };
                push_text(out, atlas, indent, y, &marker, theme.accent);
                let text_x = indent + marker.chars().count() as f32 * m.advance;
                let (_, _, end) = draw_prose(
                    out,
                    atlas,
                    runs,
                    text_x,
                    y,
                    (usable - (text_x - left)).max(0.0),
                    MD_BODY_PT,
                    Face::Regular,
                    theme,
                    None,
                    bottom,
                );
                y = end;
                rows_drawn += 1;
            }

            Block::TableRow { cells, header } => {
                // Even columns. Measuring every row to fit content would need
                // a pass over the whole table, which the flat block list
                // deliberately does not give us.
                let per = (columns / cells.len().max(1)).max(4);
                let mut x = left;
                for cell in cells {
                    let color = if *header {
                        Some(theme.md_heading)
                    } else {
                        None
                    };
                    draw_runs(out, atlas, cell, x, y, per.saturating_sub(1), theme, color);
                    x += per as f32 * m.advance;
                }
                y += m.line_height;
                if *header {
                    out.push(GlyphInstance {
                        pos: [left, y],
                        size: [usable, hairline],
                        uv: solid,
                        color: theme.md_rule,
                        ..Default::default()
                    });
                    y += m.line_height * 0.25;
                }
                rows_drawn += 1;
            }
        }
        let _ = (cell_w, cell_h, glyph_dy);
    }

    rows_drawn
}

/// Draws runs on one line, truncating at `columns`.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_runs(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    runs: &[Run],
    x: f32,
    y: f32,
    columns: usize,
    theme: &Theme,
    override_color: Option<[f32; 4]>,
) {
    let advance = atlas.metrics.advance;
    let mut column = 0usize;
    for run in runs {
        let color = override_color.unwrap_or_else(|| style_color(run.style, theme));
        for ch in run.text.chars() {
            if column >= columns {
                return;
            }
            push_text(
                out,
                atlas,
                x + column as f32 * advance,
                y,
                &ch.to_string(),
                color,
            );
            column += display_width(ch);
        }
    }
}

/// Draws wrapped prose and reports where every character landed.
///
/// One pass produces both, deliberately. The drawing and the caret map used
/// to be two functions walking the same runs with the same arithmetic; the
/// moment prose stopped being a fixed-width grid they would have disagreed,
/// and the caret in the live-edited line would sit beside the text instead of
/// in it. This is the same lesson `layout::Chrome` exists for.
///
/// Returns the visible text, one position per character, and the y after the
/// last line.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_prose(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    runs: &[Run],
    x: f32,
    mut y: f32,
    width: f32,
    size_pt: f32,
    base: Face,
    theme: &Theme,
    override_color: Option<[f32; 4]>,
    bottom: f32,
) -> (String, Vec<[f32; 2]>, f32) {
    let m = atlas.metrics;
    let line_height = prose_line_height(m, size_pt);
    let mut text = String::new();
    let mut positions: Vec<[f32; 2]> = Vec::new();
    let mut cursor = 0.0f32;

    for run in runs {
        let color = override_color.unwrap_or_else(|| style_color(run.style, theme));
        let face = match style_face(run.style) {
            Face::Regular => base,
            Face::Bold => base.with_bold(true),
            Face::Italic => base.with_italic(true),
            Face::BoldItalic => base.with_bold(true).with_italic(true),
        };
        let code = style_is_code(run.style);
        // Wrap on whitespace, so words stay whole; a "word" longer than
        // shaping takes (a URL, a hash) is cut into pieces that each shape
        // and can wrap, where it used to have no width and vanish.
        for word in run.text.split_inclusive(' ').flat_map(prose_pieces) {
            let advance = if code {
                word.chars().map(display_width).sum::<usize>() as f32 * m.advance
            } else {
                prose_width(atlas, word, size_pt, face)
            };
            if cursor + advance > width && cursor > 0.0 {
                y += line_height;
                cursor = 0.0;
                if y >= bottom {
                    positions.push([x + cursor, y]);
                    return (text, positions, y);
                }
            }
            // Per-character positions come from the shaped word, so they are
            // the real glyph edges rather than a column count.
            let (shaped_pt, glyph_scale) = prose_fit(atlas, size_pt);
            let shaped = (!code)
                .then(|| atlas.shape_prose(word, shaped_pt, face))
                .flatten();
            let mut utf16 = 0usize;
            let mut column = 0usize;
            for ch in word.chars() {
                let dx = match &shaped {
                    Some(line) => line.caret_offset(utf16) * glyph_scale,
                    None => column as f32 * m.advance,
                };
                positions.push([x + cursor + dx, y]);
                text.push(ch);
                utf16 += ch.len_utf16();
                column += display_width(ch);
            }
            if code {
                let mut column = 0usize;
                for ch in word.chars() {
                    push_text(
                        out,
                        atlas,
                        x + cursor + column as f32 * m.advance,
                        y,
                        &ch.to_string(),
                        color,
                    );
                    column += display_width(ch);
                }
            } else {
                push_prose(
                    out,
                    atlas,
                    x + cursor,
                    y,
                    size_pt,
                    face,
                    word,
                    color,
                    (width - cursor).max(0.0),
                );
            }
            cursor += advance;
        }
    }
    positions.push([x + cursor, y]);
    (text, positions, y + line_height)
}

/// Row height for prose at `size_pt`, keeping the monospace line height as
/// the floor so body text still sits on the view's rhythm.
pub(super) fn prose_line_height(m: crate::render::font::Metrics, size_pt: f32) -> f32 {
    (m.line_height * size_pt / MD_BODY_PT).max(m.line_height)
}

/// Point size of body prose in the Markdown view.
pub const MD_BODY_PT: f32 = 13.0;

/// Size of a heading relative to body prose.
pub(super) fn md_heading_scale(level: u8) -> f32 {
    match level {
        1 => 1.9,
        2 => 1.55,
        3 => 1.3,
        4 => 1.15,
        _ => 1.05,
    }
}

pub(super) fn style_color(style: Style, theme: &Theme) -> [f32; 4] {
    match style {
        // Weight and slant carry emphasis now, so body, bold and italic share
        // the reading colour instead of being told apart by brightness.
        Style::Plain | Style::Strong | Style::Emphasis | Style::StrongEmphasis => theme.text,
        Style::Code => theme.syn_string,
        Style::Link => theme.accent,
        Style::Image => theme.syn_type,
        Style::Strike => theme.gutter_text,
    }
}

/// The proportional face a run is drawn in.
pub(super) fn style_face(style: Style) -> Face {
    match style {
        Style::Strong => Face::Bold,
        Style::Emphasis => Face::Italic,
        Style::StrongEmphasis => Face::BoldItalic,
        _ => Face::Regular,
    }
}

/// Code inside prose stays monospace; everything else is proportional.
pub(super) fn style_is_code(style: Style) -> bool {
    matches!(style, Style::Code)
}

/// Draws a proportional run and returns the width it took.
///
/// The Markdown view was drawn entirely in the code font, headings included,
/// so a document read as terminal output. Prose goes through the system font
/// at a real size and weight; only code keeps the monospace cell grid.
#[allow(clippy::too_many_arguments)]
pub(super) fn push_prose(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    x: f32,
    y: f32,
    size_pt: f32,
    face: Face,
    text: &str,
    color: [f32; 4],
    limit: f32,
) -> f32 {
    let (shaped_pt, scale) = prose_fit(atlas, size_pt);
    let Some(line) = atlas.shape_prose(text, shaped_pt, face) else {
        return 0.0;
    };
    let m = atlas.metrics;
    let (cw, ch) = atlas.cell_size();
    for glyph in &line.glyphs {
        if glyph.x * scale > limit {
            break;
        }
        let Some(slot) = atlas.slot_for_shaped(&line, glyph) else {
            continue;
        };
        let mut quad = GlyphInstance {
            pos: [m.snap(x + (glyph.x + slot.dx) * scale), y],
            size: [cw * slot.cells as f32 * scale, ch * scale],
            uv: slot.uv,
            flags: slot.flags(),
            color,
            ..Default::default()
        };
        clip_horizontal(&mut quad, x, x + limit);
        out.push(quad);
    }
    line.offsets.last().copied().unwrap_or(0.0) * scale
}

/// The size prose is actually rasterized at, and the factor the quads are
/// scaled by to reach the size that was asked for.
///
/// Glyphs live in cells sized for the monospace face, and a proportional face
/// at the same point size already fills one, so nothing larger than body text
/// can be rasterized without losing its ascenders. Body, bold and italic are
/// therefore crisp; a heading is shaped at the largest size that fits and its
/// quads are scaled the rest of the way, which is soft but keeps the document
/// hierarchy and the correct proportional metrics.
///
/// Lifting this means a second cell grid with taller cells, which changes the
/// allocator, the uv arithmetic and the eviction model. It is written up in
/// the handoff rather than smuggled into this change.
pub(super) fn prose_fit(atlas: &mut Atlas, size_pt: f32) -> (f32, f32) {
    let cap = atlas.max_prose_pt();
    if size_pt <= cap {
        (size_pt, 1.0)
    } else {
        (cap, size_pt / cap)
    }
}

/// Width of a prose run without drawing it, at the size that was asked for.
/// `word` in pieces short enough to shape, cut on character boundaries.
pub(super) fn prose_pieces(word: &str) -> impl Iterator<Item = &str> {
    const PIECE: usize = 128;
    let mut rest = word;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let mut cut = rest.len().min(PIECE);
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        let (piece, tail) = rest.split_at(cut);
        rest = tail;
        Some(piece)
    })
}

pub fn prose_width(atlas: &mut Atlas, text: &str, size_pt: f32, face: Face) -> f32 {
    let (shaped_pt, scale) = prose_fit(atlas, size_pt);
    atlas
        .shape_prose(text, shaped_pt, face)
        .and_then(|line| line.offsets.last().copied())
        .unwrap_or(0.0)
        * scale
}

/// Margins and indents for the preview, in points.
pub(super) const MD_MARGIN: f32 = 28.0;
pub(super) const MD_LIST_INDENT: f32 = 20.0;
pub(super) const MD_QUOTE_INDENT: f32 = 16.0;
