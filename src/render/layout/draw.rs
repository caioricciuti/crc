//! The drawing primitives: rectangles, UI text and fields, clipping.

use super::*;

/// Cuts a quad to the rows between `top` and `bottom`, texture included,
/// so a line half scrolled out of the text does not draw over the chrome.
pub fn clip_vertical(quad: &mut GlyphInstance, top: f32, bottom: f32) {
    clip_axis(quad, 1, top, bottom);
}

pub fn clip_horizontal(quad: &mut GlyphInstance, left: f32, right: f32) {
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
/// It ends short of the strip's right edge by the inset it starts at: a
/// label too long for that is cut with an ellipsis.
pub fn strip_pill(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    strip: Viewport,
    label: &str,
    tone: [f32; 4],
) -> Viewport {
    const INSET: f32 = 16.0;
    const PAD: f32 = 10.0;
    let pill = Viewport {
        x: strip.x + INSET,
        y: strip.y + 6.0,
        width: (ui_text_width(atlas, label) + PAD * 2.0).min((strip.width - INSET * 2.0).max(0.0)),
        height: 28.0,
    };
    push_rounded_rect(out, pill, 6.0, [tone[0], tone[1], tone[2], 0.18]);
    // Centred in the pill less its padding, which is the pill's centre,
    // so a cut label keeps the padding a whole one has.
    push_ui_text_centered(
        out,
        atlas,
        Viewport {
            x: pill.x + PAD,
            width: (pill.width - PAD * 2.0).max(0.0),
            ..pill
        },
        label,
        tone,
    );
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
    push_ui_text_elided(
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
    if field.text.is_empty() {
        push_ui_text_elided(out, atlas, input, field.placeholder, theme.status_text);
    } else {
        push_ui_field_text(out, atlas, input, &shown, theme.text);
    }
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
///
/// A label wider than `rect` is cut at the rect's right edge, mid glyph and
/// without a mark. That is right only where the caller has already made
/// the text fit; a label that can outgrow its room is drawn with
/// [`push_ui_text_elided`], and text being typed with
/// [`push_ui_field_text`].
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
    let bounded: String = text.chars().take(UI_TEXT_CHARS).collect();
    let Some(line) = atlas.shape_ui(&bounded) else {
        return;
    };
    #[cfg(test)]
    note_cut(&bounded, line.caret_offset(usize::MAX), rect.width);
    push_shaped(out, atlas, rect, &line, color);
}

/// How many characters of a UI label are shaped, drawn and measured.
const UI_TEXT_CHARS: usize = 120;

/// A label drawn into a rectangle counts as fitting when it is at most
/// this much wider: a width handed back by a measurement and rounded on
/// the way comes out a rounding error short of the text it was taken from.
const ELIDE_SLACK: f32 = 0.5;

/// A label that ends in an ellipsis when `rect` is too narrow for it,
/// instead of being cut at the rect's edge; the width it drew at.
///
/// A label that fits, as nearly every one does, costs what
/// [`push_ui_text`] costs: its width is read off the line shaped to draw
/// it. Only one that does not fit is cut and shaped again, and both
/// shapings are cached from then on.
pub fn push_ui_text_elided(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    rect: Viewport,
    text: &str,
    color: [f32; 4],
) -> f32 {
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return 0.0;
    }
    let bounded: String = text.chars().take(UI_TEXT_CHARS).collect();
    let Some(mut line) = atlas.shape_ui(&bounded) else {
        return 0.0;
    };
    let mut width = line.caret_offset(usize::MAX);
    if width > rect.width + ELIDE_SLACK {
        let cut = elide_end(atlas, &bounded, rect.width);
        let Some(shorter) = atlas.shape_ui(&cut) else {
            return 0.0;
        };
        width = shorter.caret_offset(usize::MAX);
        line = shorter;
    }
    push_shaped(out, atlas, rect, &line, color);
    width.min(rect.width)
}

/// The text of a field being typed into: cut at the field's edge, where an
/// ellipsis would stand in for the very characters being typed.
pub fn push_ui_field_text(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    rect: Viewport,
    text: &str,
    color: [f32; 4],
) {
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return;
    }
    let bounded: String = text.chars().take(UI_TEXT_CHARS).collect();
    if let Some(line) = atlas.shape_ui(&bounded) {
        push_shaped(out, atlas, rect, &line, color);
    }
}

/// The quads of a shaped UI line in `rect`, cut to it.
fn push_shaped(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    rect: Viewport,
    line: &crate::render::font::ShapedLine,
    color: [f32; 4],
) {
    let m = atlas.metrics;
    let (cw, ch) = atlas.cell_size();
    for glyph in &line.glyphs {
        if glyph.x > rect.width {
            continue;
        }
        let Some(slot) = atlas.slot_for_shaped(line, glyph) else {
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

#[cfg(test)]
thread_local! {
    static CUT: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Records a label drawn `width` wide into `room` that is too narrow for
/// it: one the frame shows with its end sliced off.
#[cfg(test)]
pub(super) fn note_cut(text: &str, width: f32, room: f32) {
    if width > room + ELIDE_SLACK {
        CUT.with(|cut| {
            cut.borrow_mut()
                .push(format!("{text:?} is {width:.1} wide in {room:.1}"))
        });
    }
}

/// The labels this thread has drawn cut at the edge of their rectangle
/// since the last call: every [`push_ui_text`] and scaled label wider than
/// the room it was given. A panel's frame built in a test leaves this
/// empty, at any width, or a label in it is sliced rather than elided.
#[cfg(test)]
pub fn take_cut_labels() -> Vec<String> {
    CUT.with(|cut| std::mem::take(&mut *cut.borrow_mut()))
}

// ---- eliding ----------------------------------------------------------------
// One way to shorten a label, used by every panel: an ellipsis at the end
// for prose and names read from the left, in the middle for a name whose
// end matters as much as its start (a file's extension, a branch's
// counts).

/// `s`, cut with an ellipsis at its end to fit `width`; empty where not
/// even the ellipsis fits. Half a point of slack: a value right-aligned
/// into exactly its own width comes back a rounding error short of it.
pub fn elide_end(atlas: &mut Atlas, s: &str, width: f32) -> String {
    let width = width + ELIDE_SLACK;
    if ui_text_width(atlas, s) <= width {
        return s.to_owned();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    // A UI width counts 120 characters, so a longer cut measures as the
    // whole string did, and does not fit either.
    let top = (chars.len() - 1).min(UI_TEXT_CHARS - 1);
    let ellipsis = ui_text_width(atlas, "\u{2026}");
    let guess: Vec<f32> = match caret_xs(atlas, &chars[..=top]) {
        Some(x) => (0..=top).rev().map(|n| x[n].0 + ellipsis).collect(),
        None => vec![f32::NEG_INFINITY; top + 1],
    };
    first_fitting(atlas, width, &guess, |i| {
        chars[..top - i].iter().collect::<String>() + "\u{2026}"
    })
    .unwrap_or_default()
}

/// The caret x before each of `chars` and after the last, from the string
/// shaped once, if it can be. Inside a grapheme (an emoji sequence, a
/// base and its marks) a cut shapes as something else, often narrower, so
/// there it is a range: the x where the grapheme starts and where the next
/// one does.
///
/// Not for right-to-left text: a UI width is the caret x at the end,
/// which there is not the width at all, so every cut is measured.
fn caret_xs(atlas: &mut Atlas, chars: &[char]) -> Option<Vec<(f32, f32)>> {
    let rtl = |c: &char| {
        matches!(*c as u32,
            0x0590..=0x08ff | 0xfb1d..=0xfdff | 0xfe70..=0xfeff | 0x10800..=0x10fff
            | 0x1e800..=0x1efff | 0x200f | 0x202b | 0x202e | 0x2067)
    };
    if chars.iter().any(rtl) {
        return None;
    }
    let s: String = chars.iter().collect();
    let line = atlas.shape_ui(&s)?;
    // Printable ASCII, which most labels are: every character is its own
    // grapheme and its own UTF-16 unit, so there is nothing to ask.
    if chars.iter().all(|c| (' '..='~').contains(c)) {
        return Some(
            (0..=chars.len())
                .map(|n| {
                    let at = line.caret_offset(n);
                    (at, at)
                })
                .collect(),
        );
    }
    let composed = objc2_foundation::NSString::from_str(&s);
    let mut utf16 = 0;
    let mut x = Vec::with_capacity(chars.len() + 1);
    let mut start = 0.0;
    for c in chars {
        let at = line.caret_offset(utf16);
        if composed
            .rangeOfComposedCharacterSequenceAtIndex(utf16)
            .location
            == utf16
        {
            start = at;
            x.push((at, at));
        } else {
            x.push((start, f32::NAN));
        }
        utf16 += c.len_utf16();
    }
    let mut next = line.caret_offset(utf16);
    x.push((next, next));
    for edge in x.iter_mut().rev() {
        if edge.1.is_nan() {
            edge.1 = next;
        } else {
            next = edge.1;
        }
    }
    Some(x)
}

/// The first of the cuts `cut(0)`, `cut(1)`, ... that measures no wider
/// than `width`, as measuring each in turn finds it, measuring few: the
/// search starts at the first whose `guess`, its width from the whole
/// string's glyph advances, fits, and steps from there while measured
/// widths disagree. The guess is off only by kerning and shaping across
/// the cut, well under `SLACK`; a guess of minus infinity measures every
/// cut in turn.
fn first_fitting(
    atlas: &mut Atlas,
    width: f32,
    guess: &[f32],
    cut: impl Fn(usize) -> String,
) -> Option<String> {
    const SLACK: f32 = 2.0;
    let fits = |atlas: &mut Atlas, i: usize| {
        let c = cut(i);
        (ui_text_width(atlas, &c) <= width).then_some(c)
    };
    let mut i = guess
        .iter()
        .position(|&w| w <= width)
        .unwrap_or(guess.len().checked_sub(1)?);
    match fits(atlas, i) {
        Some(mut found) => {
            while i > 0 && guess[i - 1] <= width + SLACK {
                let Some(c) = fits(atlas, i - 1) else { break };
                found = c;
                i -= 1;
            }
            Some(found)
        }
        None => (i + 1..guess.len()).find_map(|j| fits(atlas, j)),
    }
}

/// `name` with an ellipsis in its middle to fit `width`: the beginning and
/// the end of a long name both stay, as much of each as there is room for.
pub fn elide_middle(atlas: &mut Atlas, name: &str, width: f32) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= UI_TEXT_CHARS && ui_text_width(atlas, name) <= width {
        return name.to_owned();
    }
    let mut low = 0;
    let mut high = chars.len().min(UI_TEXT_CHARS - 1);
    while low < high {
        let keep = (low + high).div_ceil(2);
        let head = keep.div_ceil(2);
        let tail = keep / 2;
        let candidate = middle_ellipsis(&chars, head, tail);
        if ui_text_width(atlas, &candidate) <= width {
            low = keep;
        } else {
            high = keep - 1;
        }
    }
    let cut = middle_ellipsis(&chars, low.div_ceil(2), low / 2);
    // Nothing kept and no room for the mark either: nothing, not a mark
    // cut in half by the rectangle it is drawn into.
    if low == 0 && ui_text_width(atlas, &cut) > width {
        return String::new();
    }
    cut
}

/// The first `head` and last `tail` characters joined by an ellipsis.
///
/// Neither cut lands inside a joined emoji: a zero-width joiner or a
/// variation selector stays with what it joins.
pub(super) fn middle_ellipsis(chars: &[char], head: usize, tail: usize) -> String {
    let joins = |c: char| c == '\u{200d}' || ('\u{fe00}'..='\u{fe0f}').contains(&c);
    let mut head = head.min(chars.len());
    while head > 0 && head < chars.len() && (joins(chars[head]) || joins(chars[head - 1])) {
        head -= 1;
    }
    let mut tail = tail.min(chars.len() - head);
    let mut from = chars.len() - tail;
    while tail > 0 && from > head && (joins(chars[from]) || joins(chars[from - 1])) {
        tail -= 1;
        from += 1;
    }
    chars[..head]
        .iter()
        .chain(std::iter::once(&'\u{2026}'))
        .chain(chars[chars.len() - tail..].iter())
        .collect()
}

/// Draws a line shaped with [`Atlas::shape_ui`] in `rect`, moved `scroll`
/// points left and clipped to it, each glyph coloured by `color` of the
/// source byte it came from: a field wider than its box, tinted in parts.
pub fn push_ui_line(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    rect: Viewport,
    line: &crate::render::font::ShapedLine,
    scroll: f32,
    color: impl Fn(usize) -> [f32; 4],
) {
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return;
    }
    let m = atlas.metrics;
    let (cw, ch) = atlas.cell_size();
    for glyph in &line.glyphs {
        let x = glyph.x - scroll;
        // Two cells of slack on the left for a glyph that starts out of
        // view and ends in it.
        if x > rect.width || x < -cw * 2.0 {
            continue;
        }
        let Some(slot) = atlas.slot_for_shaped(line, glyph) else {
            continue;
        };
        let byte = line
            .source_bytes
            .get(glyph.source_utf16)
            .copied()
            .unwrap_or(0);
        let mut quad = GlyphInstance {
            pos: [
                m.snap(rect.x + x + slot.dx),
                rect.y + m.glyph_dy(rect.height),
            ],
            size: [cw * slot.cells as f32, ch],
            uv: slot.uv,
            flags: slot.flags(),
            color: color(byte),
            ..Default::default()
        };
        clip_horizontal(&mut quad, rect.x, rect.x + rect.width);
        if quad.size[0] > 0.0 {
            out.push(quad);
        }
    }
}

/// The advance width of a UI label, in points.
pub fn ui_text_width(atlas: &mut Atlas, text: &str) -> f32 {
    let bounded: String = text.chars().take(UI_TEXT_CHARS).collect();
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
/// Text wider than the control starts at its left edge and ends in an
/// ellipsis, so a long label keeps its own start and says it was cut.
pub fn push_ui_text_centered(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    rect: Viewport,
    text: &str,
    color: [f32; 4],
) {
    let width = ui_text_width(atlas, text);
    if width > rect.width + ELIDE_SLACK {
        push_ui_text_elided(out, atlas, rect, text, color);
        return;
    }
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
///
/// Text wider than `rect` fills it from the left and ends in an ellipsis.
pub fn push_ui_text_right(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    rect: Viewport,
    text: &str,
    color: [f32; 4],
) {
    let width = ui_text_width(atlas, text);
    if width > rect.width + ELIDE_SLACK {
        push_ui_text_elided(out, atlas, rect, text, color);
        return;
    }
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

/// What a panel's frame got wrong, for a test that built `out` into
/// `panel`: the labels cut at the edge of their rectangle since the last
/// [`take_cut_labels`], and the quads that reach past the panel's left or
/// right edge. Empty for a frame that keeps everything it draws whole and
/// inside. Call [`take_cut_labels`] before building the frame.
#[cfg(test)]
pub fn frame_faults(out: &[GlyphInstance], panel: Viewport) -> Vec<String> {
    let mut faults = take_cut_labels();
    let (left, right) = (panel.x - 0.51, panel.x + panel.width + 0.51);
    for quad in out {
        if quad.size[0] > 0.0 && (quad.pos[0] < left || quad.pos[0] + quad.size[0] > right) {
            faults.push(format!(
                "a quad from {:.1} to {:.1} leaves the panel ({:.1} to {:.1})",
                quad.pos[0],
                quad.pos[0] + quad.size[0],
                panel.x,
                panel.x + panel.width
            ));
        }
    }
    faults
}

#[cfg(test)]
mod elide_tests {
    use super::*;

    fn atlas() -> Atlas {
        Atlas::build("SF Mono", 13.0, 2.0)
    }

    const ASCII: &str = "seedling-watering-schedule-for-the-north-bed.md";
    /// Kanji and kana, an accent as a combining mark, and a family joined
    /// by zero-width joiners.
    const WIDE: &str = "\u{82d7}\u{5e8a}\u{306e}\u{6c34}\u{306f}\u{3051} cafe\u{301} \
        \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467} tomato harvest log";

    #[test]
    fn a_label_that_fits_is_left_alone() {
        let mut atlas = atlas();
        for s in [ASCII, WIDE, "x", ""] {
            let w = ui_text_width(&mut atlas, s);
            assert_eq!(elide_end(&mut atlas, s, w), s);
            assert_eq!(elide_middle(&mut atlas, s, w), s);
        }
    }

    #[test]
    fn the_end_is_cut_to_the_width_with_an_ellipsis() {
        let mut atlas = atlas();
        for s in [ASCII, WIDE] {
            let whole = ui_text_width(&mut atlas, s);
            for share in [0.9, 0.6, 0.3, 0.12] {
                let width = whole * share;
                let cut = elide_end(&mut atlas, s, width);
                assert!(cut.ends_with('\u{2026}'), "{cut:?}");
                assert!(
                    ui_text_width(&mut atlas, &cut) <= width + 0.5,
                    "{cut:?} in {width}"
                );
                let kept = cut.trim_end_matches('\u{2026}');
                assert!(s.starts_with(kept), "{cut:?} is not a start of {s:?}");
                assert!(
                    !kept.ends_with('\u{200d}'),
                    "{cut:?} is cut inside a joined emoji"
                );
            }
        }
    }

    #[test]
    fn the_middle_is_cut_and_both_ends_stay() {
        let mut atlas = atlas();
        for s in [ASCII, WIDE] {
            let whole = ui_text_width(&mut atlas, s);
            for share in [0.9, 0.6, 0.3] {
                let width = whole * share;
                let cut = elide_middle(&mut atlas, s, width);
                let (head, tail) = cut.split_once('\u{2026}').expect("an ellipsis");
                assert!(ui_text_width(&mut atlas, &cut) <= width, "{cut:?}");
                assert!(s.starts_with(head) && s.ends_with(tail), "{cut:?}");
                assert!(!head.is_empty() && !tail.is_empty(), "{cut:?}");
            }
        }
    }

    #[test]
    fn a_width_under_the_ellipsis_draws_nothing() {
        let mut atlas = atlas();
        let ellipsis = ui_text_width(&mut atlas, "\u{2026}");
        for width in [0.0, ellipsis * 0.5, ellipsis - 1.0] {
            assert_eq!(elide_end(&mut atlas, ASCII, width), "", "at {width}");
            assert_eq!(elide_middle(&mut atlas, ASCII, width), "", "at {width}");
            assert_eq!(elide_end(&mut atlas, WIDE, width), "", "at {width}");
        }
        // Room for the mark alone: the mark, and none of the name.
        assert_eq!(elide_end(&mut atlas, ASCII, ellipsis + 0.5), "\u{2026}");
        assert_eq!(elide_middle(&mut atlas, ASCII, ellipsis + 0.5), "\u{2026}");
    }

    #[test]
    fn an_empty_label_stays_empty() {
        let mut atlas = atlas();
        for width in [-4.0, 0.0, 40.0] {
            assert_eq!(elide_end(&mut atlas, "", width), "");
            assert_eq!(elide_middle(&mut atlas, "", width), "");
        }
    }

    fn rect(width: f32) -> Viewport {
        Viewport {
            x: 20.0,
            y: 10.0,
            width,
            height: 24.0,
        }
    }

    /// The same quads, compared by where they are and what they show.
    fn places(quads: &[GlyphInstance]) -> Vec<([f32; 2], [f32; 2], [f32; 4])> {
        quads.iter().map(|q| (q.pos, q.size, q.uv)).collect()
    }

    #[test]
    fn the_eliding_draw_is_the_plain_draw_when_the_label_fits() {
        let mut atlas = atlas();
        let colour = Theme::default().text;
        let room = rect(ui_text_width(&mut atlas, ASCII));
        let (mut plain, mut elided) = (Vec::new(), Vec::new());
        push_ui_text(&mut plain, &mut atlas, room, ASCII, colour);
        let shaped = atlas.ui_shapes();
        let width = push_ui_text_elided(&mut elided, &mut atlas, room, ASCII, colour);
        assert_eq!(places(&plain), places(&elided));
        assert_eq!(width, room.width);
        assert_eq!(
            atlas.ui_shapes(),
            shaped,
            "a label that fits is not shaped or measured again"
        );
        assert!(take_cut_labels().is_empty());
    }

    #[test]
    fn the_eliding_draw_ends_a_long_label_in_an_ellipsis_inside_its_rectangle() {
        let mut atlas = atlas();
        let colour = Theme::default().text;
        for s in [ASCII, WIDE] {
            let room = rect(ui_text_width(&mut atlas, s) * 0.5);
            take_cut_labels();
            let mut elided = Vec::new();
            let width = push_ui_text_elided(&mut elided, &mut atlas, room, s, colour);
            assert!(width <= room.width && width > room.width * 0.7, "{width}");
            // What it drew is the cut label, whole.
            let cut = elide_end(&mut atlas, s, room.width);
            let mut expected = Vec::new();
            push_ui_text(&mut expected, &mut atlas, room, &cut, colour);
            assert_eq!(places(&elided), places(&expected));
            assert_eq!(frame_faults(&elided, room), Vec::<String>::new());
        }
        // No room for the mark: nothing, rather than half of it.
        let mut none = Vec::new();
        assert_eq!(
            push_ui_text_elided(&mut none, &mut atlas, rect(3.0), ASCII, colour),
            0.0
        );
        assert!(none.is_empty());
    }

    #[test]
    fn the_plain_draw_reports_a_label_it_cuts() {
        let mut atlas = atlas();
        let colour = Theme::default().text;
        let mut out = Vec::new();
        take_cut_labels();
        push_ui_text(&mut out, &mut atlas, rect(400.0), "EXPLORER", colour);
        assert!(take_cut_labels().is_empty());
        push_ui_text(&mut out, &mut atlas, rect(30.0), "EXPLORER", colour);
        let cut = take_cut_labels();
        assert_eq!(cut.len(), 1, "{cut:?}");
        assert!(cut[0].contains("EXPLORER"), "{cut:?}");
        // Typed text scrolls in its field; its cut is the design.
        push_ui_field_text(&mut out, &mut atlas, rect(30.0), "EXPLORER", colour);
        assert!(take_cut_labels().is_empty());
    }

    #[test]
    fn centred_and_right_aligned_labels_elide_too() {
        let mut atlas = atlas();
        let colour = Theme::default().text;
        let room = rect(60.0);
        let cut = elide_end(&mut atlas, ASCII, room.width);
        let mut expected = Vec::new();
        push_ui_text(&mut expected, &mut atlas, room, &cut, colour);
        take_cut_labels();
        for draw in [push_ui_text_centered, push_ui_text_right] {
            let mut out = Vec::new();
            draw(&mut out, &mut atlas, room, ASCII, colour);
            assert_eq!(places(&out), places(&expected));
        }
        assert!(take_cut_labels().is_empty());
    }
}

#[cfg(test)]
mod strip_tests {
    use super::*;

    #[test]
    fn the_strips_pill_and_facts_stay_in_the_strip() {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let theme = Theme::dark();
        let label = "502 Bad Gateway from the upstream proxy";
        let facts = "1.2 s \u{b7} 48 KB \u{b7} text/html; charset=utf-8";
        for width in [90.0, 180.0, 320.0, 900.0] {
            let strip = Viewport {
                x: 200.0,
                y: 60.0,
                width,
                height: 74.0,
            };
            let mut out = Vec::new();
            take_cut_labels();
            let pill = strip_pill(&mut out, &mut atlas, strip, label, theme.diff_removed);
            strip_facts(&mut out, &mut atlas, strip, pill, facts, &theme);
            assert_eq!(
                frame_faults(&out, strip),
                Vec::<String>::new(),
                "the strip at {width}"
            );
            assert!(pill.x + pill.width <= strip.x + strip.width - 16.0 + 0.01);
        }
        // Room for it: as wide as its label and padding, as before.
        let wide = Viewport {
            x: 0.0,
            y: 0.0,
            width: 900.0,
            height: 74.0,
        };
        let pill = strip_pill(&mut Vec::new(), &mut atlas, wide, "Table", theme.accent);
        assert_eq!(pill.width, ui_text_width(&mut atlas, "Table") + 20.0);
    }
}
