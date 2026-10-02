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
            let fill = match self.tone {
                Tone::Ghost => clear,
                _ => theme.tab_hover,
            };
            return (fill, faded(theme.gutter_text, 0.8));
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
        push_rounded_rect(out, inset, UI_RADIUS_SM + 1.0, theme.row_hover);
    }
    hotspot(row, Cursor::Pointing, None);
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
    let t = phase(1.6);
    let breathe = 0.55 + 0.45 * (t * std::f32::consts::TAU).sin().abs();
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
            UI_RADIUS_SM + 1.0,
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

/// A panel of grouped facts on a page: a faint rounded card.
pub fn push_card(out: &mut Vec<GlyphInstance>, rect: Viewport, theme: &Theme) {
    push_rounded_rect(out, rect, UI_RADIUS + 2.0, theme.md_code_background);
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
    std::time::Duration::from_secs(match feedback_of(text) {
        Feedback::Failure => 8,
        _ => 4,
    })
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

thread_local! {
    static ANIMATING: Cell<bool> = const { Cell::new(false) };
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
