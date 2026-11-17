//! Text in the system's proportional font, for titles and labels that are
//! not code.

use super::*;

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
