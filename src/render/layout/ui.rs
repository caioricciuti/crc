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
