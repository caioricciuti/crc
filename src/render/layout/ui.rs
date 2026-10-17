//! The shared controls: one button, its states, and the register of what
//! the pointer can land on in the frame being drawn.
//!
//! Every panel used to draw its own buttons: a rounded rectangle, then a
//! label centred by hand, at its own height, radius and inset. Twenty-odd
//! call sites settled on six control heights, seven radii and no hover or
//! pressed state anywhere but the editor tabs. A control drawn through
//! [`Button`] looks like every other one, reacts to the pointer, and leaves
//! a [`Hotspot`] behind, from which the window builds its cursor rectangles
//! and tooltips. What is drawn and what is clickable cannot drift apart,
//! because they are the same rectangle.

use super::*;
use std::cell::{Cell, RefCell};

// ---- sizes ----------------------------------------------------------------
// One scale for the whole window. A control is 28 tall, or 22 where it
// sits inside a row; panels inset their content by 12; things in a row are
// 6 apart.

/// A button, field or menu box.
pub const UI_CONTROL: f32 = 28.0;
/// A control inside a list row or a dense strip.
pub const UI_CONTROL_SM: f32 = 22.0;
/// The corner of a control.
pub const UI_RADIUS: f32 = 6.0;
/// The corner of a small control or a row highlight.
pub const UI_RADIUS_SM: f32 = 4.0;
/// The corner of a card or a tile: a surface things sit on, not a control.
pub const UI_RADIUS_LG: f32 = 8.0;
/// A page's title, in points: a step above a heading, two above the
/// sidebar's section names, and no display face.
pub const UI_TITLE: f32 = 17.0;
/// A heading inside a page: a group of settings, a section of facts.
pub const UI_HEADING: f32 = 14.0;
/// How far a segment sits inside its track.
pub const UI_SEGMENT_INSET: f32 = 2.0;
/// A panel's content inset from its edges.
pub const UI_INSET: f32 = 12.0;
/// The space between neighbouring controls.
pub const UI_GAP: f32 = 6.0;
/// The space between an icon and its label inside one control.
pub const UI_ICON_GAP: f32 = 6.0;

// ---- the pointer ------------------------------------------------------------

/// What a hotspot does to the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cursor {
    /// Clickable: the pointing hand.
    Pointing,
    /// Text to type into.
    Text,
    /// Nothing to click, but something to say: a tooltip alone.
    Arrow,
}

/// A rectangle of the last frame the pointer can do something with.
#[derive(Clone, Debug, PartialEq)]
pub struct Hotspot {
    pub rect: Viewport,
    pub cursor: Cursor,
    /// Shown after the pointer rests on it. Icon-only buttons always have
    /// one; a button that says what it does in words does not need it.
    pub tip: Option<String>,
}

#[derive(Clone, Copy, Default)]
struct Pointer {
    at: Option<(f32, f32)>,
    /// Where the button went down, while it is down.
    down: Option<(f32, f32)>,
}

thread_local! {
    static POINTER: Cell<Pointer> = Cell::new(Pointer::default());
    static HOTSPOTS: RefCell<Vec<Hotspot>> = const { RefCell::new(Vec::new()) };
}

/// Where the pointer is, in view points; `None` once it leaves the window.
pub fn set_pointer(at: Option<(f32, f32)>) {
    POINTER.with(|p| {
        let mut v = p.get();
        v.at = at.filter(|(x, y)| x.is_finite() && y.is_finite());
        p.set(v);
    });
}

/// Where the mouse button went down, or `None` once it is up again.
pub fn set_pressed(at: Option<(f32, f32)>) {
    POINTER.with(|p| {
        let mut v = p.get();
        v.down = at;
        p.set(v);
    });
}

/// Where the pointer is, while it is in the window: for a control whose
/// parts are finer than its hotspots, such as a timeline's slots.
pub fn pointer() -> Option<(f32, f32)> {
    POINTER.with(|p| p.get().at)
}

/// Whether the pointer is over `rect`.
pub fn hovered(rect: Viewport) -> bool {
    POINTER.with(|p| p.get().at.is_some_and(|(x, y)| rect.contains(x, y)))
}

/// Whether `rect` is being held down: the press started in it and the
/// pointer is still over it.
pub fn pressed(rect: Viewport) -> bool {
    POINTER.with(|p| {
        let v = p.get();
        v.down.is_some_and(|(x, y)| rect.contains(x, y))
            && v.at.is_none_or(|(x, y)| rect.contains(x, y))
    })
}

/// Starts a frame's register of hotspots.
pub fn begin_frame() {
    HOTSPOTS.with(|h| h.borrow_mut().clear());
}

/// Adds a hotspot to the frame being drawn.
pub fn hotspot(rect: Viewport, cursor: Cursor, tip: Option<&str>) {
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return;
    }
    HOTSPOTS.with(|h| {
        h.borrow_mut().push(Hotspot {
            rect,
            cursor,
            tip: tip.map(str::to_owned),
        })
    });
}

/// The hotspots of the frame just drawn, in drawing order.
pub fn hotspots() -> Vec<Hotspot> {
    HOTSPOTS.with(|h| h.borrow().clone())
}

/// Which of `spots` the point is over: the last drawn wins, as it is on
/// top.
pub fn hotspot_at(spots: &[Hotspot], x: f32, y: f32) -> Option<usize> {
    spots.iter().rposition(|s| s.rect.contains(x, y))
}

// ---- colour -----------------------------------------------------------------

/// `colour` with its alpha multiplied by `alpha`.
pub fn faded([r, g, b, a]: [f32; 4], alpha: f32) -> [f32; 4] {
    [r, g, b, a * alpha]
}

/// `colour` moved `amount` of the way toward `toward`.
pub fn mixed(colour: [f32; 4], toward: [f32; 4], amount: f32) -> [f32; 4] {
    std::array::from_fn(|i| {
        if i == 3 {
            colour[3]
        } else {
            colour[i] + (toward[i] - colour[i]) * amount
        }
    })
}

// ---- the button -------------------------------------------------------------

/// How much a button stands out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// The one action a panel exists for: Commit, Install.
    Primary,
    /// A filled control: most buttons.
    Secondary,
    /// No fill until the pointer is over it: icon buttons in a header or a
    /// row, which would otherwise read as a row of boxes.
    Ghost,
    /// Something that throws work away.
    Danger,
}

/// One control. Build it, then [`Button::draw`] it.
#[derive(Clone, Copy, Debug)]
pub struct Button<'a> {
    pub rect: Viewport,
    pub label: &'a str,
    pub icon: Option<char>,
    pub tone: Tone,
    pub enabled: bool,
    /// A toggle that is on, or the menu whose list is open.
    pub on: bool,
    pub tip: Option<&'a str>,
    /// A trailing hint in the label's dimmer colour: a shortcut, a count.
    pub hint: Option<&'a str>,
    /// Lays the content out from the left edge instead of the centre, the
    /// way a menu box or a field reads.
    pub leading: bool,
    /// A chevron at the trailing edge: the button opens a menu.
    pub menu: bool,
    pub radius: f32,
}

impl<'a> Button<'a> {
    pub fn new(rect: Viewport) -> Self {
        Self {
            rect,
            label: "",
            icon: None,
            tone: Tone::Secondary,
            enabled: true,
            on: false,
            tip: None,
            hint: None,
            leading: false,
            menu: false,
            radius: if rect.height <= UI_CONTROL_SM {
                UI_RADIUS_SM
            } else {
                UI_RADIUS
            },
        }
    }
    pub fn label(mut self, label: &'a str) -> Self {
        self.label = label;
        self
    }
    pub fn icon(mut self, icon: char) -> Self {
        self.icon = Some(icon);
        self
    }
    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
    pub fn on(mut self, on: bool) -> Self {
        self.on = on;
        self
    }
    pub fn tip(mut self, tip: &'a str) -> Self {
        self.tip = Some(tip);
        self
    }
    pub fn hint(mut self, hint: &'a str) -> Self {
        self.hint = Some(hint);
        self
    }
    pub fn leading(mut self) -> Self {
        self.leading = true;
        self
    }
    pub fn menu(mut self) -> Self {
        self.menu = true;
        self.leading = true;
        self
    }
    pub fn radius(mut self, radius: f32) -> Self {
        self.radius = radius;
        self
    }

    /// The width this button needs for its content, `pad` on each side.
    pub fn content_width(&self, atlas: &mut Atlas, pad: f32) -> f32 {
        let icon = self.icon.map_or(0.0, |g| icon_width(atlas, g));
        let label = if self.label.is_empty() {
            0.0
        } else {
            ui_text_width(atlas, self.label)
        };
        let gap = if icon > 0.0 && label > 0.0 {
            UI_ICON_GAP
        } else {
            0.0
        };
        let hint = self
            .hint
            .map_or(0.0, |h| ui_text_width(atlas, h) + UI_ICON_GAP);
        icon + gap + label + hint + pad * 2.0
    }

    /// Fill and ink for the state the pointer leaves it in.
    fn colours(&self, theme: &Theme) -> ([f32; 4], [f32; 4]) {
        let hover = self.enabled && hovered(self.rect);
        let down = self.enabled && pressed(self.rect);
        let clear = [0.0, 0.0, 0.0, 0.0];
        if !self.enabled {
            // A primary that cannot act yet is still the primary: the
            // accent, washed out, not a lesser secondary.
            return match self.tone {
                Tone::Ghost => (clear, faded(theme.gutter_text, 0.8)),
                Tone::Primary => (faded(theme.accent, 0.22), faded(theme.accent, 0.7)),
                _ => (theme.tab_hover, faded(theme.gutter_text, 0.8)),
            };
        }
        if self.on {
            let fill = if down {
                faded(theme.accent, 0.30)
            } else if hover {
                faded(theme.accent, 0.22)
            } else {
                theme.palette_selected
            };
            return (fill, theme.accent);
        }
        match self.tone {
            Tone::Primary => {
                let lift = if theme.is_dark() {
                    [1.0, 1.0, 1.0, 1.0]
                } else {
                    [0.0, 0.0, 0.0, 1.0]
                };
                let fill = if down {
                    mixed(theme.accent, [0.0, 0.0, 0.0, 1.0], 0.18)
                } else if hover {
                    mixed(theme.accent, lift, 0.12)
                } else {
                    theme.accent
                };
                let ink = if theme.is_dark() {
                    [0.063, 0.086, 0.078, 1.0]
                } else {
                    [1.0, 1.0, 1.0, 1.0]
                };
                (fill, ink)
            }
            Tone::Secondary => {
                let fill = if down {
                    theme.control_pressed
                } else if hover {
                    theme.control_hover
                } else {
                    theme.tab_hover
                };
                (fill, theme.text)
            }
            Tone::Ghost => {
                let fill = if down {
                    theme.control_pressed
                } else if hover {
                    theme.control_hover
                } else {
                    clear
                };
                (fill, if hover { theme.text } else { theme.status_text })
            }
            Tone::Danger => {
                let fill = if down {
                    faded(theme.diff_removed, 0.30)
                } else if hover {
                    faded(theme.diff_removed, 0.20)
                } else {
                    theme.tab_hover
                };
                (fill, theme.diff_removed)
            }
        }
    }

    /// Draws the button and registers it with the frame.
    pub fn draw(self, out: &mut Vec<GlyphInstance>, atlas: &mut Atlas, theme: &Theme) {
        let rect = self.rect;
        if rect.width <= 0.0 || rect.height <= 0.0 {
            return;
        }
        let (fill, ink) = self.colours(theme);
        if fill[3] > 0.0 {
            push_rounded_rect(out, rect, self.radius, fill);
        }
        let pad = if self.leading { 8.0 } else { 0.0 };
        let chevron = if self.menu {
            icon_width(atlas, icons::CHEVRON_DOWN)
        } else {
            0.0
        };
        let inner = Viewport {
            x: rect.x + pad,
            width: (rect.width - pad * 2.0 - if chevron > 0.0 { chevron + 2.0 } else { 0.0 })
                .max(0.0),
            ..rect
        };
        let content = Button {
            menu: false,
            ..self
        }
        .content_width(atlas, 0.0);
        let mut x = if self.leading {
            inner.x
        } else {
            inner.x + ((inner.width - content) * 0.5).max(0.0)
        };
        let right = inner.x + inner.width;
        if let Some(glyph) = self.icon {
            let w = icon_width(atlas, glyph);
            push_icon_centered(
                out,
                atlas,
                Viewport {
                    x,
                    width: w,
                    ..rect
                },
                glyph,
                ink,
            );
            x += w + if self.label.is_empty() {
                0.0
            } else {
                UI_ICON_GAP
            };
        }
        if !self.label.is_empty() {
            let hint_w = self
                .hint
                .map_or(0.0, |h| ui_text_width(atlas, h) + UI_ICON_GAP);
            push_ui_text(
                out,
                atlas,
                Viewport {
                    x,
                    width: (right - x - if self.leading { hint_w } else { 0.0 }).max(0.0),
                    ..rect
                },
                self.label,
                ink,
            );
            if let Some(hint) = self.hint {
                let w = ui_text_width(atlas, hint);
                let at = if self.leading {
                    right - w
                } else {
                    x + ui_text_width(atlas, self.label) + UI_ICON_GAP
                };
                push_ui_text(
                    out,
                    atlas,
                    Viewport {
                        x: at,
                        width: w + 1.0,
                        ..rect
                    },
                    hint,
                    faded(ink, 0.6),
                );
            }
        }
        if self.menu {
            push_icon_centered(
                out,
                atlas,
                Viewport {
                    x: rect.x + rect.width - pad - chevron,
                    width: chevron,
                    ..rect
                },
                icons::CHEVRON_DOWN,
                faded(ink, 0.7),
            );
        }
        hotspot(
            rect,
            if self.enabled {
                Cursor::Pointing
            } else {
                Cursor::Arrow
            },
            self.tip,
        );
    }
}

/// The width an icon-font glyph draws at.
pub fn icon_width(atlas: &mut Atlas, glyph: char) -> f32 {
    let (cell_w, _) = atlas.cell_size();
    atlas
        .slot_for(glyph)
        .map_or(cell_w * 2.0, |slot| cell_w * slot.cells as f32)
}

/// A row the pointer is over gets a faint wash, so a list reads as a list
/// of things to click. Registers the row as clickable.
pub fn push_row_hover(out: &mut Vec<GlyphInstance>, row: Viewport, theme: &Theme) {
    let inset = Viewport {
        x: row.x + 6.0,
        y: row.y + 1.0,
        width: (row.width - 12.0).max(0.0),
        height: (row.height - 2.0).max(0.0),
    };
    if hovered(row) {
        push_rounded_rect(out, inset, UI_RADIUS_SM, theme.row_hover);
    }
    hotspot(row, Cursor::Pointing, None);
}

// ---- the segmented control ----------------------------------------------
// A track with the choices side by side and the chosen one lit: Grid or
// Text over a table, Editor or Agents in the toolbar, the steps and the
// filter over a file's history. One shape for every row of related
// choices, so a row of them never reads as a row of unrelated buttons.

/// One choice of a segmented control. Its `rect` is the cell in the
/// track; the segment is drawn [`UI_SEGMENT_INSET`] inside it.
#[derive(Clone, Copy, Debug)]
pub struct Segment<'a> {
    pub rect: Viewport,
    pub label: &'a str,
    pub icon: Option<char>,
    /// The chosen one, lit with the accent.
    pub on: bool,
    pub enabled: bool,
    pub tip: Option<&'a str>,
}

impl<'a> Segment<'a> {
    pub fn new(rect: Viewport, label: &'a str) -> Self {
        Self {
            rect,
            label,
            icon: None,
            on: false,
            enabled: true,
            tip: None,
        }
    }
    pub fn icon(mut self, icon: char) -> Self {
        self.icon = Some(icon);
        self
    }
    pub fn on(mut self, on: bool) -> Self {
        self.on = on;
        self
    }
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
    pub fn tip(mut self, tip: &'a str) -> Self {
        self.tip = Some(tip);
        self
    }
}

/// The track under a row of segments: from the first cell to the last.
pub fn segmented_track(segments: &[Segment<'_>]) -> Option<Viewport> {
    let (first, last) = (segments.first()?.rect, segments.last()?.rect);
    Some(Viewport {
        width: (last.x + last.width - first.x).max(0.0),
        ..first
    })
}

/// Draws a segmented control: the track in the control's resting fill,
/// each segment a ghost button inside its cell, the chosen one on.
pub fn push_segmented(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    segments: &[Segment<'_>],
    theme: &Theme,
) {
    let Some(track) = segmented_track(segments) else {
        return;
    };
    let radius = if track.height <= UI_CONTROL_SM {
        UI_RADIUS_SM
    } else {
        UI_RADIUS
    };
    push_rounded_rect(out, track, radius, theme.tab_hover);
    for segment in segments {
        let inner = Viewport {
            x: segment.rect.x + UI_SEGMENT_INSET,
            y: segment.rect.y + UI_SEGMENT_INSET,
            width: (segment.rect.width - UI_SEGMENT_INSET * 2.0).max(0.0),
            height: (segment.rect.height - UI_SEGMENT_INSET * 2.0).max(0.0),
        };
        let mut button = Button::new(inner)
            .label(segment.label)
            .tone(Tone::Ghost)
            .radius(UI_RADIUS_SM)
            .on(segment.on)
            .enabled(segment.enabled);
        if let Some(icon) = segment.icon {
            button = button.icon(icon);
        }
        if let Some(tip) = segment.tip {
            button = button.tip(tip);
        }
        button.draw(out, atlas, theme);
    }
}

/// A small rounded count beside a heading: `Changes (3)` without the
/// brackets.
pub fn push_count(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    x: f32,
    row: Viewport,
    count: usize,
    theme: &Theme,
) -> f32 {
    let label = if count > 999 {
        "999+".to_owned()
    } else {
        count.to_string()
    };
    let w = (ui_text_width(atlas, &label) + 10.0).max(18.0);
    let pill = Viewport {
        x,
        y: row.y + ((row.height - 16.0) * 0.5).round(),
        width: w,
        height: 16.0,
    };
    push_rounded_rect(out, pill, 8.0, theme.tab_hover);
    push_ui_text_centered(out, atlas, pill, &label, theme.status_text);
    w
}

/// A section heading in a sidebar list: chevron, small-caps-like title and
/// its count, on the same baseline as the rows under it.
#[allow(clippy::too_many_arguments)]
pub fn push_section_heading(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    row: Viewport,
    title: &str,
    count: Option<usize>,
    expanded: bool,
    colour: [f32; 4],
    theme: &Theme,
) {
    let chevron = if expanded {
        icons::CHEVRON_DOWN
    } else {
        icons::CHEVRON_RIGHT
    };
    let cw = icon_width(atlas, chevron);
    push_icon_centered(
        out,
        atlas,
        Viewport {
            x: row.x + 8.0,
            width: cw,
            ..row
        },
        chevron,
        theme.gutter_text,
    );
    let x = row.x + 8.0 + cw + 2.0;
    let upper = title.to_uppercase();
    let w = ui_text_width(atlas, &upper);
    push_ui_text(
        out,
        atlas,
        Viewport {
            x,
            width: (row.x + row.width - x).max(0.0),
            ..row
        },
        &upper,
        colour,
    );
    if let Some(count) = count {
        push_count(out, atlas, x + w + 8.0, row, count, theme);
    }
}

/// A UI label at `scale` times the UI size, centred in `rect`: badges and
/// counts, where the 13pt UI face crowds a 16pt pill. Drawn from the same
/// atlas glyphs, scaled, so it costs nothing new.
pub fn push_ui_text_scaled(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    rect: Viewport,
    text: &str,
    color: [f32; 4],
    scale: f32,
) {
    let Some(line) = atlas.shape_ui(text) else {
        return;
    };
    let m = atlas.metrics;
    let (cw, ch) = atlas.cell_size();
    let width = ui_text_width(atlas, text) * scale;
    let x0 = rect.x + (rect.width - width) * 0.5;
    let y0 = m.snap(rect.y + (rect.height - ch * scale) * 0.5);
    for glyph in &line.glyphs {
        let Some(slot) = atlas.slot_for_shaped(&line, glyph) else {
            continue;
        };
        out.push(GlyphInstance {
            pos: [x0 + (glyph.x + slot.dx) * scale, y0],
            size: [cw * slot.cells as f32 * scale, ch * scale],
            uv: slot.uv,
            flags: slot.flags(),
            color,
            ..Default::default()
        });
    }
}

/// A count on an icon: a small accent pill at its top right, cut out of
/// the icon by a ring in `ground`, the surface it sits on.
pub fn push_badge(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    icon: Viewport,
    count: usize,
    ground: [f32; 4],
    theme: &Theme,
) {
    if count == 0 {
        return;
    }
    let label = if count > 99 {
        "99+".to_owned()
    } else {
        count.to_string()
    };
    const SCALE: f32 = 0.78;
    const H: f32 = 15.0;
    let w = (ui_text_width(atlas, &label) * SCALE + 8.0).max(H);
    let pill = Viewport {
        x: (icon.x + icon.width * 0.5 + 2.0).round(),
        y: (icon.y + icon.height * 0.5 - 15.0).round(),
        width: w.round(),
        height: H,
    };
    push_rounded_rect(
        out,
        Viewport {
            x: pill.x - 2.0,
            y: pill.y - 2.0,
            width: pill.width + 4.0,
            height: pill.height + 4.0,
        },
        (H + 4.0) * 0.5,
        ground,
    );
    push_rounded_rect(out, pill, H * 0.5, theme.accent);
    let ink = if theme.is_dark() {
        [0.063, 0.086, 0.078, 1.0]
    } else {
        [1.0, 1.0, 1.0, 1.0]
    };
    push_ui_text_scaled(out, atlas, pill, &label, ink, SCALE);
}

/// A placeholder for a list row that is on its way: an icon square and
/// two bars, breathing, so a list being fetched has a shape before it has
/// content. `two_lines` for rows with a title and a detail.
pub fn push_skeleton_row(
    out: &mut Vec<GlyphInstance>,
    row: Viewport,
    icon: f32,
    widths: (f32, f32),
    two_lines: bool,
    theme: &Theme,
) {
    // A whole sine, so the breath eases in and out at both ends; folding
    // half of it with abs() put a kink at every turn.
    let t = phase(1.6);
    let breathe = 0.55 + 0.45 * (0.5 - 0.5 * (t * std::f32::consts::TAU).cos());
    let fill = faded(theme.control_hover, breathe);
    let mut x = row.x;
    if icon > 0.0 {
        push_rounded_rect(
            out,
            Viewport {
                x,
                y: row.y + ((row.height - icon) * 0.5).round(),
                width: icon,
                height: icon,
            },
            UI_RADIUS_SM,
            fill,
        );
        x += icon + 10.0;
    }
    let room = (row.x + row.width - x).max(0.0);
    let bar = |out: &mut Vec<GlyphInstance>, y: f32, w: f32, h: f32| {
        push_rounded_rect(
            out,
            Viewport {
                x,
                y,
                width: (room * w).max(12.0).min(room),
                height: h,
            },
            h * 0.5,
            fill,
        )
    };
    if two_lines {
        bar(out, row.y + row.height * 0.5 - 10.0, widths.0, 8.0);
        bar(out, row.y + row.height * 0.5 + 3.0, widths.1, 7.0);
    } else {
        bar(
            out,
            row.y + ((row.height - 8.0) * 0.5).round(),
            widths.0,
            8.0,
        );
    }
    ANIMATING.with(|a| a.set(true));
}

/// A heading in bold prose at `size` points, top-left at `(x, y)`; its
/// width. For page titles, which the 13pt UI face is too small for.
#[allow(clippy::too_many_arguments)]
pub fn push_title(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    x: f32,
    y: f32,
    size: f32,
    text: &str,
    color: [f32; 4],
    limit: f32,
) -> f32 {
    super::markdown::push_prose(out, atlas, x, y, size, Face::Bold, text, color, limit)
}

/// A page's title at [`UI_TITLE`]: the one size and weight every page
/// heads with. `y` is the top of the header row the title shares with
/// its buttons; the title is set on their centre.
pub fn push_page_title(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    x: f32,
    y: f32,
    text: &str,
    color: [f32; 4],
    limit: f32,
) -> f32 {
    // Prose above the atlas's cap is drawn scaled up from the cap's cell.
    let (_, ch) = atlas.cell_size();
    let drawn = ch * (UI_TITLE / atlas.max_prose_pt()).max(1.0);
    let top = y + ((UI_CONTROL - drawn) * 0.5).floor();
    push_title(out, atlas, x, top, UI_TITLE, text, color, limit)
}

/// A heading inside a page at [`UI_HEADING`], top-left at `(x, y)`.
pub fn push_heading(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    x: f32,
    y: f32,
    text: &str,
    color: [f32; 4],
    limit: f32,
) -> f32 {
    push_title(out, atlas, x, y, UI_HEADING, text, color, limit)
}

/// A panel of grouped facts on a page: a faint rounded card.
pub fn push_card(out: &mut Vec<GlyphInstance>, rect: Viewport, theme: &Theme) {
    push_rounded_rect(out, rect, UI_RADIUS_LG, theme.md_code_background);
}

/// What a status-line message is: news, a success, or a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feedback {
    Info,
    Success,
    Failure,
}

/// Reads a status-line message for what kind it is. Messages are plain
/// sentences from all over the app; their wording is consistent enough to
/// tell a failure from a success, and a failure must not look like news.
pub fn feedback_of(text: &str) -> Feedback {
    let lower = text.to_lowercase();
    const FAILURE: [&str; 15] = [
        "fail",
        "could not",
        "couldn't",
        "error",
        "refused",
        "not committed",
        "cannot",
        "can't",
        "invalid",
        "no upstream",
        "not a git",
        "rejected",
        "fatal:",
        "did not",
        "not saved",
    ];
    const SUCCESS: [&str; 17] = [
        "saved",
        "committed",
        "pushed",
        "pulled",
        "fetched",
        "staged",
        "unstaged",
        "switched",
        "created",
        "deleted",
        "renamed",
        "copied",
        "installed",
        "removed",
        "moved",
        "replaced",
        "marked",
    ];
    if FAILURE.iter().any(|w| lower.contains(w)) {
        Feedback::Failure
    } else if SUCCESS.iter().any(|w| lower.starts_with(w)) {
        Feedback::Success
    } else {
        Feedback::Info
    }
}

/// How long a status-line message stays: a failure twice as long, since
/// it has to be read, not just noticed.
pub fn message_lasts(text: &str) -> std::time::Duration {
    message_lasts_for(feedback_of(text))
}

/// How long a message of `feedback` stays: a failure long enough to read.
pub fn message_lasts_for(feedback: Feedback) -> std::time::Duration {
    std::time::Duration::from_secs(match feedback {
        Feedback::Failure => 8,
        _ => 4,
    })
}

/// What a message is: as said by whoever set it (`kind`, when it is about
/// this very text), else as its wording reads.
pub fn feedback_for(text: &str, kind: Option<(&str, Feedback)>) -> Feedback {
    kind.filter(|(said, _)| *said == text)
        .map_or_else(|| feedback_of(text), |(_, kind)| kind)
}

/// How long a message that carries a button stays: long enough to reach
/// for it.
pub const MESSAGE_ACTION_LASTS: std::time::Duration = std::time::Duration::from_secs(10);

/// A message's last moment: it thins out over this long before it goes.
pub const MESSAGE_FADE: std::time::Duration = std::time::Duration::from_millis(300);

/// How opaque a message `elapsed` into the `lasts` it is given for is:
/// whole until its last [`MESSAGE_FADE`], then down to nothing.
pub fn message_alpha(elapsed: std::time::Duration, lasts: std::time::Duration) -> f32 {
    let left = lasts.saturating_sub(elapsed);
    if left >= MESSAGE_FADE {
        1.0
    } else {
        (left.as_secs_f32() / MESSAGE_FADE.as_secs_f32()).clamp(0.0, 1.0)
    }
}

/// Something that happens around a message and can make it stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dismissal {
    /// Editor to Agents or back: the message was about the other mode.
    ModeSwitch,
    /// A document closed: what was said about it is over.
    TabClosed,
}

/// Whether a message of `feedback` goes on `dismissal`. News and a
/// success are about the moment and the place, so a change of either
/// clears them; a failure stays until it has been read, whatever moves
/// around it.
pub fn message_clears_on(feedback: Feedback, dismissal: Dismissal) -> bool {
    match (feedback, dismissal) {
        (Feedback::Failure, _) => false,
        (Feedback::Info | Feedback::Success, Dismissal::ModeSwitch | Dismissal::TabClosed) => true,
    }
}

/// The time-based phase of anything animated, 0..1 over `period` seconds.
pub fn phase(period: f32) -> f32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() % 1_000_000) as f32;
    (ms / 1000.0 / period).fract()
}

/// A thin bar sliding along the top of `rect`: work under way whose length
/// is unknown. The window keeps redrawing while one is on screen.
pub fn push_progress(out: &mut Vec<GlyphInstance>, atlas: &Atlas, rect: Viewport, theme: &Theme) {
    let t = phase(1.4);
    let seg = (rect.width * 0.32).max(24.0);
    let x = rect.x - seg + (rect.width + seg) * t;
    let from = x.max(rect.x);
    let to = (x + seg).min(rect.x + rect.width);
    if to > from {
        push_rect(out, atlas, [from, rect.y], [to - from, 2.0], theme.accent);
    }
    ANIMATING.with(|a| a.set(true));
}

/// Three dots pulsing in turn: a spinner for a status line or a button.
/// Returns the width taken.
pub fn push_spinner(out: &mut Vec<GlyphInstance>, x: f32, row: Viewport, colour: [f32; 4]) -> f32 {
    let t = phase(1.0);
    let size = 4.0;
    let cy = row.y + ((row.height - size) * 0.5).round();
    for i in 0..3 {
        let p = (t * 3.0 - i as f32).rem_euclid(3.0);
        let alpha = if p < 1.0 { 1.0 - p * 0.65 } else { 0.35 };
        push_rounded_rect(
            out,
            Viewport {
                x: x + i as f32 * (size + 3.0),
                y: cy,
                width: size,
                height: size,
            },
            size * 0.5,
            faded(colour, alpha),
        );
    }
    ANIMATING.with(|a| a.set(true));
    3.0 * size + 6.0
}

// ---- the empty state --------------------------------------------------------
// What a surface says when it has nothing to show. Every panel used to say
// it in its own way, most as one grey line at the top left, which reads as
// a bug. This is the one way: an icon, a title, a line of hint and at most
// one button, centred in the room the panel has.

/// The row an empty state's icon sits in.
pub const UI_EMPTY_ICON: f32 = 40.0;

/// How much larger than a text glyph the icon of an empty state draws.
const EMPTY_ICON_SCALE: f32 = 1.8;

/// An empty state. Build it, then [`push_empty_state`] it.
#[derive(Clone, Copy, Debug)]
pub struct EmptyState<'a> {
    /// The glyph over the title. `None` is the small variant, for a block
    /// under a heading inside a list, where an icon would outweigh the
    /// rows around it.
    pub icon: Option<char>,
    pub title: &'a str,
    /// One line, wrapped to two at most where the column is narrow.
    pub hint: &'a str,
    /// The one action: its label and, when it has one, its icon.
    pub button: Option<(&'a str, Option<char>)>,
}

/// Where the parts of an [`EmptyState`] land.
#[derive(Clone, Debug, PartialEq)]
pub struct EmptyLayout {
    pub icon: Option<Viewport>,
    pub title: Viewport,
    /// The hint's lines, each with its row.
    pub hint: Vec<(String, Viewport)>,
    pub button: Option<Viewport>,
    /// The block itself: as tall as its parts, centred in the rect given.
    pub block: Viewport,
}

impl<'a> EmptyState<'a> {
    pub fn new(title: &'a str, hint: &'a str) -> Self {
        Self {
            icon: None,
            title,
            hint,
            button: None,
        }
    }
    pub fn icon(mut self, icon: char) -> Self {
        self.icon = Some(icon);
        self
    }
    pub fn button(mut self, label: &'a str, icon: Option<char>) -> Self {
        self.button = Some((label, icon));
        self
    }

    /// The height the block takes in a column `width` wide: for a panel
    /// that lays it out in the flow of a list rather than centred in a
    /// rect.
    pub fn height(&self, atlas: &mut Atlas, width: f32) -> f32 {
        self.layout(atlas, Viewport::new(width, 0.0)).block.height
    }

    /// Lays the parts out in `rect`: centred in it when it is taller than
    /// the block, from its top when it is not.
    pub fn layout(&self, atlas: &mut Atlas, rect: Viewport) -> EmptyLayout {
        let x = rect.x + UI_INSET;
        let width = (rect.width - UI_INSET * 2.0).max(0.0);
        let lines: Vec<String> = if self.hint.is_empty() {
            Vec::new()
        } else {
            wrap_words(atlas, self.hint, width)
                .into_iter()
                .take(2)
                .collect()
        };
        let mut height = UI_CONTROL_SM;
        if self.icon.is_some() {
            height += UI_EMPTY_ICON + UI_GAP;
        }
        height += lines.len() as f32 * UI_CONTROL_SM;
        if self.button.is_some() {
            height += UI_INSET + UI_CONTROL;
        }
        let top = rect.y + ((rect.height - height) * 0.5).max(0.0).floor();
        let mut y = top;
        let row = |y: f32, height: f32| Viewport {
            x,
            y,
            width,
            height,
        };
        let icon = self.icon.map(|_| {
            let r = row(y, UI_EMPTY_ICON);
            y += UI_EMPTY_ICON + UI_GAP;
            r
        });
        let title = row(y, UI_CONTROL_SM);
        y += UI_CONTROL_SM;
        let hint = lines
            .into_iter()
            .map(|line| {
                let r = row(y, UI_CONTROL_SM);
                y += UI_CONTROL_SM;
                (line, r)
            })
            .collect();
        let button = self.button.map(|(label, icon)| {
            y += UI_INSET;
            let mut b = Button::new(row(y, UI_CONTROL)).label(label);
            if let Some(icon) = icon {
                b = b.icon(icon);
            }
            let w = b.content_width(atlas, UI_INSET).min(width).round();
            Viewport {
                x: (x + (width - w) * 0.5).floor(),
                y,
                width: w,
                height: UI_CONTROL,
            }
        });
        EmptyLayout {
            icon,
            title,
            hint,
            button,
            block: row(top, height),
        }
    }
}

/// Draws an empty state in `rect`. The button, when there is one, is
/// drawn as a [`Button`] and its rectangle returned for the caller to
/// act on.
pub fn push_empty_state(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    rect: Viewport,
    state: EmptyState,
    theme: &Theme,
) -> Option<Viewport> {
    if rect.width <= UI_INSET * 2.0 || rect.height <= 0.0 {
        return None;
    }
    let laid = state.layout(atlas, rect);
    if let (Some(glyph), Some(r)) = (state.icon, laid.icon) {
        push_icon_scaled(out, atlas, r, glyph, theme.gutter_text, EMPTY_ICON_SCALE);
    }
    push_ui_text_centered(out, atlas, laid.title, state.title, theme.text);
    for (line, r) in &laid.hint {
        push_ui_text_centered(out, atlas, *r, line, theme.gutter_text);
    }
    if let (Some((label, icon)), Some(r)) = (state.button, laid.button) {
        let mut b = Button::new(r).label(label);
        if let Some(icon) = icon {
            b = b.icon(icon);
        }
        b.draw(out, atlas, theme);
    }
    laid.button
}

thread_local! {
    static ANIMATING: Cell<bool> = const { Cell::new(false) };
    /// Frames drawn so far, and when the overlay last seen was first seen.
    static FRAME: Cell<u64> = const { Cell::new(0) };
    static OVERLAY: Cell<Option<(u64, u64, std::time::Instant)>> = const { Cell::new(None) };
}

/// How long an overlay takes to come in.
pub const OVERLAY_FADE: std::time::Duration = std::time::Duration::from_millis(120);

/// Marks the frame being drawn as one more; the overlay fade counts them.
pub fn next_frame() {
    FRAME.with(|f| f.set(f.get() + 1));
}

/// How far in an overlay is, 0..1: the palette, a popover. `id` names
/// it; its fade starts the first frame it is drawn after a frame without
/// it, and the window keeps redrawing only until it is whole. Eased so
/// it settles rather than stops.
pub fn overlay_alpha(id: u64) -> f32 {
    let now = std::time::Instant::now();
    let frame = FRAME.with(Cell::get);
    let since = OVERLAY.with(|o| {
        let since = match o.get() {
            Some((seen, last, since)) if seen == id && last + 1 >= frame => since,
            _ => now,
        };
        o.set(Some((id, frame, since)));
        since
    });
    let t = now.duration_since(since).as_secs_f32() / OVERLAY_FADE.as_secs_f32();
    if t >= 1.0 {
        return 1.0;
    }
    ANIMATING.with(|a| a.set(true));
    let t = t.clamp(0.0, 1.0);
    t * (2.0 - t)
}

/// Thins everything drawn since `from` by `alpha`: the way a fade is
/// applied to an overlay already built.
pub fn fade_from(out: &mut [GlyphInstance], from: usize, alpha: f32) {
    if alpha >= 1.0 {
        return;
    }
    for glyph in out.iter_mut().skip(from) {
        glyph.color[3] *= alpha;
    }
}

/// Something drawn in this frame is still moving: the next one is wanted.
pub fn keep_animating() {
    ANIMATING.with(|a| a.set(true));
}

/// Whether the frame just drawn has something moving in it, and clears the
/// flag for the next one.
pub fn take_animating() -> bool {
    ANIMATING.with(|a| a.replace(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hover_and_press_follow_the_pointer() {
        let r = Viewport {
            x: 10.0,
            y: 10.0,
            width: 20.0,
            height: 20.0,
        };
        set_pointer(Some((15.0, 15.0)));
        assert!(hovered(r));
        assert!(!pressed(r));
        set_pressed(Some((15.0, 15.0)));
        assert!(pressed(r));
        // Dragged off the button: no longer pressed, as AppKit's own.
        set_pointer(Some((50.0, 50.0)));
        assert!(!pressed(r));
        assert!(!hovered(r));
        set_pressed(None);
        set_pointer(None);
        assert!(!hovered(r));
    }

    #[test]
    fn messages_are_told_apart_by_their_wording() {
        assert_eq!(
            feedback_of("could not start zsh: denied"),
            Feedback::Failure
        );
        assert_eq!(feedback_of("Not committed: leak.txt:1"), Feedback::Failure);
        assert_eq!(
            feedback_of("this branch has no upstream to pull from"),
            Feedback::Failure
        );
        assert_eq!(feedback_of("pushed: main -> main"), Feedback::Success);
        assert_eq!(feedback_of("staged 3 files"), Feedback::Success);
        assert_eq!(feedback_of("Folder opened"), Feedback::Info);
        assert_eq!(feedback_of("Fetching…"), Feedback::Info);
        assert!(message_lasts("push failed") > message_lasts("saved"));
    }

    #[test]
    fn news_clears_when_the_scene_changes_and_a_failure_stays() {
        use std::time::Duration;
        for dismissal in [Dismissal::ModeSwitch, Dismissal::TabClosed] {
            assert!(message_clears_on(Feedback::Info, dismissal));
            assert!(message_clears_on(Feedback::Success, dismissal));
            assert!(!message_clears_on(Feedback::Failure, dismissal));
        }
        // Whoever set the message outranks its wording.
        assert_eq!(
            feedback_for(
                "saved notes.md",
                Some(("saved notes.md", Feedback::Failure))
            ),
            Feedback::Failure
        );
        assert_eq!(
            feedback_for(
                "saved notes.md",
                Some(("something else", Feedback::Failure))
            ),
            Feedback::Success
        );
        // Whole until the last moment, then thinning out, then gone.
        let lasts = Duration::from_secs(4);
        assert_eq!(message_alpha(Duration::ZERO, lasts), 1.0);
        assert_eq!(message_alpha(lasts - MESSAGE_FADE, lasts), 1.0);
        let half = message_alpha(lasts - MESSAGE_FADE / 2, lasts);
        assert!((half - 0.5).abs() < 0.01, "{half}");
        assert_eq!(message_alpha(lasts, lasts), 0.0);
        assert_eq!(message_alpha(lasts * 2, lasts), 0.0);
    }

    #[test]
    fn an_empty_state_centres_its_block_and_stacks_the_button_last() {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let rect = Viewport {
            x: 40.0,
            y: 100.0,
            width: 600.0,
            height: 400.0,
        };
        let state = EmptyState::new(
            "No servers yet",
            "Add servers to mcp.json to list them here.",
        )
        .icon('\u{eb50}')
        .button("Open mcp.json", Some('\u{ea73}'));
        let laid = state.layout(&mut atlas, rect);
        // Centred: as much room above the block as below it.
        let above = laid.block.y - rect.y;
        let below = rect.y + rect.height - (laid.block.y + laid.block.height);
        assert!((above - below).abs() <= 1.0, "{above} above, {below} below");
        assert_eq!(laid.block.height, state.height(&mut atlas, rect.width));
        // Icon, title, hint, button, each under the one before.
        let icon = laid.icon.expect("an icon row");
        assert_eq!(icon.y, laid.block.y);
        assert!(laid.title.y >= icon.y + icon.height);
        let (_, hint) = laid.hint.last().expect("a hint line");
        assert_eq!(laid.hint.len(), 1);
        assert!(hint.y >= laid.title.y + laid.title.height);
        let button = laid.button.expect("a button");
        assert!(button.y >= hint.y + hint.height);
        assert_eq!(button.y + button.height, laid.block.y + laid.block.height);
        assert_eq!(button.height, UI_CONTROL);
        // Sized to its label and centred on the column.
        let centre = rect.x + rect.width * 0.5;
        assert!((button.x + button.width * 0.5 - centre).abs() <= 1.0);
        assert!(button.width < rect.width * 0.5);
        // The small variant has no icon row and is shorter by it.
        let small = EmptyState::new("No recent folders", "Folders you open will be listed here.");
        let small_h = small.height(&mut atlas, rect.width);
        let full_h = EmptyState::new("No recent folders", "Folders you open will be listed here.")
            .icon('\u{ea83}')
            .height(&mut atlas, rect.width);
        assert_eq!(full_h - small_h, UI_EMPTY_ICON + UI_GAP);
        assert!(small.layout(&mut atlas, rect).icon.is_none());
        // A narrow column wraps the hint rather than clipping it.
        let narrow = state.layout(&mut atlas, Viewport::new(200.0, 400.0));
        assert_eq!(narrow.hint.len(), 2);
        // In the flow of a list: a rect of its own height starts at its top.
        let flow = Viewport {
            height: full_h,
            ..rect
        };
        assert_eq!(
            EmptyState::new("No recent folders", "Folders you open will be listed here.")
                .icon('\u{ea83}')
                .layout(&mut atlas, flow)
                .block
                .y,
            rect.y
        );
    }

    #[test]
    fn a_segmented_control_runs_one_track_under_its_cells() {
        let cell = |x: f32| Viewport {
            x,
            y: 10.0,
            width: 50.0,
            height: UI_CONTROL,
        };
        let segments = [
            Segment::new(cell(100.0), "Older"),
            Segment::new(cell(150.0), "Newer"),
            Segment::new(cell(200.0), "All").on(true),
        ];
        let track = segmented_track(&segments).expect("a track");
        assert_eq!((track.x, track.width), (100.0, 150.0));
        assert_eq!((track.y, track.height), (10.0, UI_CONTROL));
        assert!(segmented_track(&[]).is_none());
        // Drawn: the track, then a hotspot per segment inside its cell.
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let mut out = Vec::new();
        begin_frame();
        push_segmented(&mut out, &mut atlas, &segments, &Theme::dark());
        let spots = hotspots();
        assert_eq!(spots.len(), 3);
        assert_eq!(spots[0].rect.x, 100.0 + UI_SEGMENT_INSET);
        assert_eq!(spots[0].rect.height, UI_CONTROL - UI_SEGMENT_INSET * 2.0);
    }

    #[test]
    fn a_disabled_primary_keeps_the_accent_and_an_overlay_fades_in_once() {
        let theme = Theme::dark();
        let rect = Viewport::new(80.0, UI_CONTROL);
        set_pointer(None);
        let primary = Button::new(rect).tone(Tone::Primary).enabled(false);
        let secondary = Button::new(rect).tone(Tone::Secondary).enabled(false);
        assert_ne!(primary.colours(&theme), secondary.colours(&theme));
        assert_eq!(primary.colours(&theme).0[..3], theme.accent[..3]);
        // An overlay: thin on its first frame, whole after the fade, and
        // asking for frames only until then.
        take_animating();
        next_frame();
        let first = overlay_alpha(7);
        assert!(first < 1.0, "{first}");
        assert!(take_animating());
        std::thread::sleep(OVERLAY_FADE + std::time::Duration::from_millis(20));
        next_frame();
        assert_eq!(overlay_alpha(7), 1.0);
        assert!(!take_animating());
        // Gone for a frame, then back: it fades in again.
        next_frame();
        next_frame();
        assert!(overlay_alpha(7) < 1.0);
        take_animating();
        let mut out = vec![GlyphInstance::default(); 2];
        out[0].color = [1.0, 1.0, 1.0, 1.0];
        out[1].color = [1.0, 1.0, 1.0, 0.5];
        fade_from(&mut out, 1, 0.5);
        assert_eq!((out[0].color[3], out[1].color[3]), (1.0, 0.25));
    }

    #[test]
    fn the_last_hotspot_drawn_is_the_one_on_top() {
        begin_frame();
        let big = Viewport::new(100.0, 100.0);
        let small = Viewport {
            x: 10.0,
            y: 10.0,
            width: 10.0,
            height: 10.0,
        };
        hotspot(big, Cursor::Arrow, None);
        hotspot(small, Cursor::Pointing, Some("Refresh"));
        let spots = hotspots();
        assert_eq!(hotspot_at(&spots, 12.0, 12.0), Some(1));
        assert_eq!(hotspot_at(&spots, 50.0, 50.0), Some(0));
        assert_eq!(hotspot_at(&spots, 150.0, 50.0), None);
        begin_frame();
        assert!(hotspots().is_empty());
    }
}
