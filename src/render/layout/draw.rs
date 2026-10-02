//! The drawing primitives: rectangles, UI text and fields, clipping.

use super::*;

/// Cuts a quad to the rows between `top` and `bottom`, texture included,
/// so a line half scrolled out of the text does not draw over the chrome.
pub fn clip_vertical(quad: &mut GlyphInstance, top: f32, bottom: f32) {
    clip_axis(quad, 1, top, bottom);
}

pub(super) fn clip_horizontal(quad: &mut GlyphInstance, left: f32, right: f32) {
    clip_axis(quad, 0, left, right);
}

/// Crops textured geometry to `lo..hi` on `axis` (0 is x, 1 is y), UVs
/// included, rather than painting over it.
pub(super) fn clip_axis(quad: &mut GlyphInstance, axis: usize, lo: f32, hi: f32) {
    let start = quad.pos[axis];
    let size = quad.size[axis];
    if start >= lo && start + size <= hi {
        return;
    }
    let from = start.max(lo).min(hi);
    let to = (start + size).min(hi).max(from);
    if size > 0.0 {
        let uv = quad.uv[axis + 2] - quad.uv[axis];
        quad.uv[axis] += uv * (from - start) / size;
        quad.uv[axis + 2] = quad.uv[axis] + uv * (to - from) / size;
    }
    quad.pos[axis] = from;
    quad.size[axis] = to - from;
}

/// Short proportional UI labels, shaped and cached independently from code.
pub fn ui_input_window(text: &str, cursor: usize) -> (String, usize) {
    let cursor = cursor.min(text.len());
    let start = text[..cursor]
        .char_indices()
        .rev()
        .nth(79)
        .map_or(0, |(at, _)| at);
    let end = text[cursor..]
        .char_indices()
        .nth(20)
        .map_or(text.len(), |(at, _)| cursor + at);
    (text[start..end].to_owned(), start)
}

/// A one-line field in the UI font, as every such field draws it.
pub struct UiField<'a> {
    pub text: &'a str,
    pub cursor: usize,
    pub selection: Option<std::ops::Range<usize>>,
    /// Shown dimmed while the field is empty.
    pub placeholder: &'a str,
    /// The caret is drawn only in the field that has the keyboard.
    pub focused: bool,
}

// ---- status strips --------------------------------------------------------
// The strip above a merge conflict and above a Claude review: a tinted pill
// naming the state, the facts beside it, a row of buttons below, and a
// hairline under it all.

/// Draws the pill at the strip's top left, tinted `tone`; its rectangle.
pub fn strip_pill(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    strip: Viewport,
    label: &str,
    tone: [f32; 4],
) -> Viewport {
    let pill = Viewport {
        x: strip.x + 16.0,
        y: strip.y + 6.0,
        width: ui_text_width(atlas, label) + 20.0,
        height: 28.0,
    };
    push_rounded_rect(out, pill, 6.0, [tone[0], tone[1], tone[2], 0.18]);
    push_ui_text_centered(out, atlas, pill, label, tone);
    pill
}

/// The strip's facts, right of its `pill`.
pub fn strip_facts(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    strip: Viewport,
    pill: Viewport,
    facts: &str,
    theme: &Theme,
) {
    push_ui_text(
        out,
        atlas,
        Viewport {
            x: pill.x + pill.width + 12.0,
            y: pill.y,
            width: (strip.x + strip.width - pill.x - pill.width - 28.0).max(0.0),
            height: pill.height,
        },
        facts,
        theme.status_text,
    );
}

/// A button on the strip's second row, starting at `x`, `pad` wider than
/// its label.
pub fn strip_button(atlas: &mut Atlas, strip: Viewport, x: f32, label: &str, pad: f32) -> Viewport {
    Viewport {
        x,
        y: strip.y + 39.0,
        width: ui_text_width(atlas, label) + pad,
        height: 28.0,
    }
}

/// The hairline along the strip's bottom edge.
pub fn strip_hairline(out: &mut Vec<GlyphInstance>, atlas: &Atlas, strip: Viewport, theme: &Theme) {
    push_rect(
        out,
        atlas,
        [strip.x, strip.y + strip.height - 1.0],
        [strip.width, 1.0],
        theme.hairline,
    );
}

/// The palette query's text inset from the palette's left edge.
pub const PALETTE_INPUT_PAD: f32 = 42.0;

/// Draws `field` in `input`: the text scrolled to keep the caret in view,
/// the selection band behind it and the caret, both `band` = (top offset,
/// height) within `input`.
pub fn push_ui_field(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    input: Viewport,
    band: (f32, f32),
    field: &UiField,
    theme: &Theme,
) {
    let (shown, start) = ui_input_window(field.text, field.cursor);
    // Behind the glyphs, so the text stays readable on top of the band.
    if let Some(range) = &field.selection {
        let from = range.start.max(start).min(start + shown.len());
        let to = range.end.max(start).min(start + shown.len());
        if from < to {
            let x0 = ui_caret_x(atlas, &shown, from - start).min(input.width);
            let x1 = ui_caret_x(atlas, &shown, to - start).min(input.width);
            push_rect(
                out,
                atlas,
                [input.x + x0, input.y + band.0],
                [(x1 - x0).max(0.0), band.1],
                theme.selection,
            );
        }
    }
    let empty = field.text.is_empty();
    push_ui_text(
        out,
        atlas,
        input,
        if empty { field.placeholder } else { &shown },
        if empty { theme.status_text } else { theme.text },
    );
    if field.focused {
        let caret = ui_caret_x(atlas, &shown, field.cursor.saturating_sub(start)).min(input.width);
        push_rect(
            out,
            atlas,
            [input.x + caret, input.y + band.0],
            [1.0, band.1],
            theme.cursor,
        );
    }
}

pub fn ui_caret_x(atlas: &mut Atlas, text: &str, cursor: usize) -> f32 {
    let utf16 = text[..cursor.min(text.len())].encode_utf16().count();
    atlas
        .shape_ui(text)
        .map_or(0.0, |line| line.caret_offset(utf16))
}

/// Short proportional UI labels, shaped and cached independently from code.
pub fn push_ui_text(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    rect: Viewport,
    text: &str,
    color: [f32; 4],
) {
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return;
    }
    // Keep even an adversarial file name bounded on the main thread.
    let bounded: String = text.chars().take(120).collect();
    let Some(line) = atlas.shape_ui(&bounded) else {
        return;
    };
    let m = atlas.metrics;
    let (cw, ch) = atlas.cell_size();
    for glyph in &line.glyphs {
        if glyph.x > rect.width {
            continue;
        }
        let Some(slot) = atlas.slot_for_shaped(&line, glyph) else {
            continue;
        };
        let mut quad = GlyphInstance {
            pos: [
                m.snap(rect.x + glyph.x + slot.dx),
                rect.y + m.glyph_dy(rect.height),
            ],
            size: [cw * slot.cells as f32, ch],
            uv: slot.uv,
            flags: slot.flags(),
            color,
            ..Default::default()
        };
        clip_horizontal(&mut quad, rect.x, rect.x + rect.width);
        out.push(quad);
    }
}

/// The advance width of a UI label, in points.
pub fn ui_text_width(atlas: &mut Atlas, text: &str) -> f32 {
    let bounded: String = text.chars().take(120).collect();
    ui_caret_x(atlas, &bounded, bounded.len())
}

/// `text` broken into lines that fit `width`, at spaces.
pub fn wrap_words(atlas: &mut Atlas, text: &str, width: f32) -> Vec<String> {
    // Measured a word at a time: the UI width of a whole line stops
    // counting at 120 characters, so a long one never seemed to overflow.
    let space = ui_text_width(atlas, " ");
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        let mut line_w = 0.0;
        for word in paragraph.split_whitespace() {
            let word_w = ui_text_width(atlas, word);
            if !line.is_empty() && line_w + space + word_w > width {
                lines.push(std::mem::take(&mut line));
                line_w = 0.0;
            }
            if !line.is_empty() {
                line.push(' ');
                line_w += space;
            }
            line.push_str(word);
            line_w += word_w;
        }
        lines.push(line);
    }
    lines
}

/// A UI label centred horizontally in `rect`.
///
/// Controls used to centre their text by hand, with a fixed left inset chosen
/// for one particular string: `rect.x + 9.0` puts "Refresh" 9pt from the left
/// of an 82pt pill and 23pt from the right, and the narrower "×" 9pt from the
/// left of a 32pt one and 15pt from the right. Every label of a new length was
/// off by a new amount, which is how cramped toolbar labels came back after
/// being "fixed" by widening their bounds. Measure, then centre.
///
/// Text wider than the control falls back to the left inset it would have had,
/// so a long label clips at the right edge instead of losing its own start.
pub fn push_ui_text_centered(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    rect: Viewport,
    text: &str,
    color: [f32; 4],
) {
    let width = ui_text_width(atlas, text);
    let inset = ((rect.width - width) * 0.5).max(0.0);
    push_ui_text(
        out,
        atlas,
        Viewport {
            x: rect.x + inset,
            width: (rect.width - inset).max(0.0),
            ..rect
        },
        text,
        color,
    );
}

/// A focus ring: drawn *behind* the control's own fill, which then covers
/// all but `width` of it. Drawing a transparent rectangle on top would not
/// erase anything, since it is blended rather than punched out.
pub fn push_focus_ring(
    out: &mut Vec<GlyphInstance>,
    rect: Viewport,
    radius: f32,
    width: f32,
    theme: &Theme,
) {
    push_rounded_rect(
        out,
        Viewport {
            x: rect.x - width,
            y: rect.y - width,
            width: rect.width + width * 2.0,
            height: rect.height + width * 2.0,
        },
        radius + width,
        faded(theme.accent, 0.55),
    );
}

/// An icon-font glyph centred in `rect`, both axes.
///
/// Icons occupy two cells, so centring them means measuring those cells
/// rather than assuming a single advance the way text placement does.
pub fn push_icon_centered(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    rect: Viewport,
    glyph: char,
    color: [f32; 4],
) {
    let Some(slot) = atlas.slot_for(glyph) else {
        return;
    };
    let (cell_w, cell_h) = atlas.cell_size();
    let width = cell_w * slot.cells as f32;
    out.push(GlyphInstance {
        pos: [
            atlas.metrics.snap(rect.x + (rect.width - width) * 0.5),
            atlas.metrics.snap(rect.y + (rect.height - cell_h) * 0.5),
        ],
        size: [width, cell_h],
        uv: slot.uv,
        flags: slot.flags(),
        color,
        ..Default::default()
    });
}

/// An icon-font glyph centred in `rect` at `scale` times its usual size,
/// for places where an icon stands alone and a text-sized one looks lost.
pub fn push_icon_scaled(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    rect: Viewport,
    glyph: char,
    color: [f32; 4],
    scale: f32,
) {
    let Some(slot) = atlas.slot_for(glyph) else {
        return;
    };
    let (cell_w, cell_h) = atlas.cell_size();
    let (width, height) = (cell_w * slot.cells as f32 * scale, cell_h * scale);
    out.push(GlyphInstance {
        pos: [
            atlas.metrics.snap(rect.x + (rect.width - width) * 0.5),
            atlas.metrics.snap(rect.y + (rect.height - height) * 0.5),
        ],
        size: [width, height],
        uv: slot.uv,
        flags: slot.flags(),
        color,
        ..Default::default()
    });
}

/// A UI label aligned to the right edge of `rect`.
///
/// For trailing hints like a keyboard shortcut, which were previously placed
/// by guessing a width and subtracting it from the right edge. The guess and
/// the real advance width disagreed, so the hint floated.
pub fn push_ui_text_right(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    rect: Viewport,
    text: &str,
    color: [f32; 4],
) {
    let width = ui_text_width(atlas, text);
    let inset = (rect.width - width).max(0.0);
    push_ui_text(
        out,
        atlas,
        Viewport {
            x: rect.x + inset,
            width: (rect.width - inset).max(0.0),
            ..rect
        },
        text,
        color,
    );
}

/// A floating panel: a hairline in the palette's border colour around its
/// background, both rounded by `radius`.
pub fn push_panel(out: &mut Vec<GlyphInstance>, rect: Viewport, radius: f32, theme: &Theme) {
    push_rounded_rect(out, rect, radius, theme.palette_border);
    push_rounded_rect(
        out,
        Viewport {
            x: rect.x + 0.5,
            y: rect.y + 0.5,
            width: (rect.width - 1.0).max(0.0),
            height: (rect.height - 1.0).max(0.0),
        },
        radius - 0.5,
        theme.palette_background,
    );
}

/// A single analytically antialiased quad; no extra textures or draw calls.
pub fn push_rounded_rect(
    out: &mut Vec<GlyphInstance>,
    rect: Viewport,
    radius: f32,
    color: [f32; 4],
) {
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return;
    }
    out.push(GlyphInstance {
        pos: [rect.x, rect.y],
        size: [rect.width, rect.height],
        color,
        flags: crate::render::metal::ROUNDED,
        _pad: [
            radius
                .min(rect.width * 0.5)
                .min(rect.height * 0.5)
                .to_bits(),
            0,
            0,
        ],
        ..Default::default()
    });
}

/// Appends a solid rectangle.
pub fn push_rect(
    out: &mut Vec<GlyphInstance>,
    atlas: &Atlas,
    pos: [f32; 2],
    size: [f32; 2],
    color: [f32; 4],
) {
    out.push(GlyphInstance {
        pos,
        size,
        uv: atlas.solid_uv(),
        color,
        ..Default::default()
    });
}
