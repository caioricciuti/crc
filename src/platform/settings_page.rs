//! The Settings page: Cmd-, gives it the editor column. Every setting is
//! listed by name with what it does, read from `config.toml`, which stays
//! the one place a setting is kept: a switch, a stepper or a choice
//! rewrites its one line of the file and applies it at once; a setting
//! that takes free text opens the file at its line.

use crate::platform::mcp_page::{link, text};
use crate::platform::settings::{Change, Control, DESCRIBED, Settings, control_of};
use crate::project::icons;
use crate::render::{
    font::Atlas,
    layout::{self, Theme, Viewport},
    metal::GlyphInstance,
};

const PAD: f32 = 28.0;
/// Prose and cards keep a readable measure on a wide window.
const MEASURE: f32 = 720.0;
/// One wheel line, in points.
pub const ROW: f32 = 26.0;

const OPEN: &str = "Open config.toml";

const INTRO: &str = "A change here is written to config.toml and applies at once. Settings that take text open the file at their line.";

/// What a click on the page does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Open config.toml in a tab.
    Open,
    /// Read config.toml again.
    Reload,
    /// Give the column back.
    Close,
    /// Open config.toml at this key.
    Edit(&'static str),
    /// Flip this switch.
    Toggle(&'static str),
    /// Step this number up (`true`) or down.
    Step(&'static str, bool),
    /// Pick this word for this key.
    Choose(&'static str, &'static str),
}

impl Action {
    /// The key a control changes and what it asks for, for the window to
    /// write; none for the header's actions and the rows that open the file.
    pub fn change(self) -> Option<(&'static str, Change<'static>)> {
        match self {
            Action::Toggle(key) => Some((key, Change::Flip)),
            Action::Step(key, up) => Some((key, Change::Step(up))),
            Action::Choose(key, word) => Some((key, Change::Choose(word))),
            _ => None,
        }
    }
}

#[derive(Default)]
pub struct Page {
    /// Whether the page has the editor column. Escape, Close and a tab
    /// give it back.
    pub open: bool,
    /// The settings as last read, and the lines that could not be used.
    pub settings: Settings,
    pub problems: Vec<String>,
    /// The page's buttons and rows as last drawn.
    pub hits: Vec<(Viewport, Action)>,
    /// How far the body is scrolled, in points.
    pub scroll: f32,
    /// Where the body was drawn, for the wheel.
    pub rect: Option<Viewport>,
    /// How tall the body was last frame, which bounds the scroll.
    pub content: f32,
}

impl Page {
    /// Reads config.toml again.
    pub fn reload(&mut self) {
        let (settings, problems) = Settings::load_checked();
        self.settings = settings;
        self.problems = problems;
    }

    pub fn scroll_by(&mut self, points: f32) {
        let visible = self.rect.map_or(0.0, |r| r.height);
        let max = (self.content - visible).max(0.0);
        self.scroll = (self.scroll + points).clamp(0.0, max);
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<Action> {
        self.hits
            .iter()
            .filter(|_| self.open)
            .find(|(rect, _)| rect.contains(x, y))
            .map(|(_, action)| *action)
    }

    /// A region for scripts: `settings.open`, `settings.reload`,
    /// `settings.close`, `settings.edit.KEY`, `settings.toggle.KEY`,
    /// `settings.less.KEY`, `settings.more.KEY`, `settings.choose.KEY.WORD`.
    pub fn named(&self, name: &str) -> Option<Viewport> {
        self.hits
            .iter()
            .filter(|_| self.open)
            .find(|(_, action)| name_of(*action) == name)
            .map(|(rect, _)| *rect)
    }

    /// For the self-test: whether it has the column, how many settings
    /// differ from the default, how many lines could not be used, and how
    /// many targets it drew.
    pub fn report(&self) -> String {
        format!(
            "{} changed={} problems={} targets={}",
            if self.open { "open" } else { "closed" },
            changed(&self.settings),
            self.problems.len(),
            self.hits.len()
        )
    }
}

fn name_of(action: Action) -> String {
    match action {
        Action::Open => "settings.open".into(),
        Action::Reload => "settings.reload".into(),
        Action::Close => "settings.close".into(),
        Action::Edit(key) => format!("settings.edit.{key}"),
        Action::Toggle(key) => format!("settings.toggle.{key}"),
        Action::Step(key, up) => format!("settings.{}.{key}", if up { "more" } else { "less" }),
        Action::Choose(key, word) => format!("settings.choose.{key}.{word}"),
    }
}

fn changed(settings: &Settings) -> usize {
    let defaults = Settings::default();
    DESCRIBED
        .iter()
        .filter(|d| settings.value_of(d.key) != defaults.value_of(d.key))
        .count()
}

/// The editor column: the header with its actions, then the settings by
/// group, scrolled by the wheel and cut at the page's edges.
pub fn draw(
    page: &mut Page,
    atlas: &mut Atlas,
    rect: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    let start = out.len();
    let (top, body_start) = draw_page(page, atlas, rect, theme, out);
    let bottom = rect.y + rect.height;
    for quad in &mut out[start..] {
        layout::clip_vertical(quad, rect.y, bottom);
    }
    for quad in &mut out[body_start..] {
        layout::clip_vertical(quad, top, bottom);
    }
    // A row running past the bottom keeps the part that shows; one cut by
    // the header, or wholly below, cannot be clicked.
    for (r, _) in &mut page.hits {
        if r.y < bottom && r.y + r.height > bottom {
            r.height = bottom - r.y;
        }
    }
    page.hits
        .retain(|(r, _)| r.y + r.height <= bottom && (r.y + r.height <= top || r.y >= top));
}

fn draw_page(
    page: &mut Page,
    atlas: &mut Atlas,
    rect: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) -> (f32, usize) {
    let mut hits = Vec::new();
    layout::push_rect(
        out,
        atlas,
        [rect.x, rect.y],
        [rect.width, rect.height],
        theme.tab_active,
    );
    page.rect = None;
    if rect.width < 320.0 || rect.height < 200.0 {
        page.hits = hits;
        return (rect.y, out.len());
    }
    let dim = theme.status_text;
    let x = rect.x + PAD;
    let right = rect.x + rect.width - PAD;
    let measure = (right - x).min(MEASURE);
    let mut y = rect.y + 22.0;

    // The header: the title, then the icon buttons from the right: close,
    // as every panel closes, and read the file again.
    layout::push_page_title(out, atlas, x, y - 4.0, "Settings", theme.text, 200.0);
    let mut bx = right + 6.0;
    for (glyph, action, tip) in [
        (icons::CLOSE, Action::Close, "Close Settings  \u{238b}"),
        (icons::REFRESH, Action::Reload, "Read config.toml Again"),
    ] {
        bx -= layout::UI_CONTROL;
        let r = Viewport {
            x: bx,
            y: y - 4.0,
            width: layout::UI_CONTROL,
            height: layout::UI_CONTROL,
        };
        layout::Button::new(r)
            .icon(glyph)
            .tone(layout::Tone::Ghost)
            .tip(tip)
            .draw(out, atlas, theme);
        hits.push((r, action));
        bx -= 4.0;
    }
    y += 26.0;

    // What the file came to: the lines it could not use, or the count.
    let (status, colour) = match Settings::describe_problems(&page.problems) {
        Some(said) => (said, theme.diff_removed),
        None => (
            match changed(&page.settings) {
                0 => format!("{} settings \u{b7} all at their defaults", DESCRIBED.len()),
                n => format!(
                    "{} settings \u{b7} {n} changed from the default",
                    DESCRIBED.len()
                ),
            },
            dim,
        ),
    };
    // Everything the page does not show is in the file: the link to it
    // sits at the status line's end.
    let open_w = layout::ui_text_width(atlas, OPEN) + 12.0;
    let open = link(
        out,
        atlas,
        theme,
        right - open_w + 6.0,
        y + 10.0 - layout::UI_CONTROL * 0.5,
        OPEN,
    );
    layout::hotspot(
        open,
        layout::Cursor::Pointing,
        Some("Open config.toml in a Tab"),
    );
    hits.push((open, Action::Open));
    text(out, atlas, x, y, open.x - x - 12.0, &status, colour);
    y += 28.0;
    layout::push_rect(out, atlas, [x, y], [right - x, 1.0], theme.hairline);
    y += 12.0;

    // The body, scrolled.
    let body_top = y;
    let body_start = out.len();
    let body_hot = layout::hotspots_len();
    let bottom = rect.y + rect.height;
    let body = Viewport {
        x: rect.x,
        y: body_top,
        width: rect.width,
        height: (bottom - body_top).max(0.0),
    };
    page.rect = Some(body);
    let max_scroll = (page.content - (bottom - body_top)).max(0.0);
    page.scroll = page.scroll.clamp(0.0, max_scroll);
    let mut y = body_top - page.scroll;

    for line in layout::wrap_words(atlas, INTRO, measure) {
        text(out, atlas, x, y, measure, &line, dim);
        y += 20.0;
    }
    y += 16.0;

    let defaults = Settings::default();
    let mut groups: Vec<&str> = Vec::new();
    for d in &DESCRIBED {
        if !groups.contains(&d.group) {
            groups.push(d.group);
        }
    }
    for group in groups {
        layout::push_heading(out, atlas, x, y, group, theme.text, measure);
        y += 28.0;
        // Each row: the setting's name, its control at the trailing edge,
        // then what it does and, when changed, the default.
        let rows: Vec<_> = DESCRIBED
            .iter()
            .filter(|d| d.group == group)
            .map(|d| {
                let control = control_of(d.key);
                let room = measure - 32.0 - control_width(atlas, control) - 16.0;
                let mut lines = layout::wrap_words(atlas, d.what, measure - 32.0);
                let differs = page.settings.value_of(d.key) != defaults.value_of(d.key);
                if differs {
                    lines.push(format!("Default: {}", defaults.display_of(d.key)));
                }
                (d, control, room.max(0.0), differs, lines)
            })
            .collect();
        let height: f32 = rows
            .iter()
            .map(|(_, _, _, _, lines)| row_height(lines.len()))
            .sum();
        let card = Viewport {
            x,
            y,
            width: measure,
            height,
        };
        layout::push_card(out, card, theme);
        let mut ry = y;
        for (n, (d, control, room, differs, lines)) in rows.iter().enumerate() {
            let key = d.key;
            let row = Viewport {
                x,
                y: ry,
                width: measure,
                height: row_height(lines.len()),
            };
            if n > 0 {
                layout::push_rect(
                    out,
                    atlas,
                    [x + 16.0, ry],
                    [measure - 32.0, 1.0],
                    theme.hairline,
                );
            }
            let control_right = x + measure - 16.0;
            let control_y = ry + 22.0 - layout::UI_CONTROL_SM * 0.5;
            let name_w = match control {
                // Free text: the whole row opens the file at the key, with
                // the value and a chevron at its end.
                Control::Text => {
                    layout::hotspot(row, layout::Cursor::Pointing, None);
                    if layout::hovered(row) {
                        layout::push_rounded_rect(out, row, layout::UI_RADIUS_LG, theme.row_hover);
                    }
                    let chevron = layout::icon_width(atlas, icons::CHEVRON_RIGHT);
                    layout::push_icon_centered(
                        out,
                        atlas,
                        Viewport {
                            x: control_right - chevron,
                            y: control_y,
                            width: chevron,
                            height: layout::UI_CONTROL_SM,
                        },
                        icons::CHEVRON_RIGHT,
                        dim,
                    );
                    let value = page.settings.display_of(key);
                    let value_right = control_right - chevron - 6.0;
                    let value_w = (measure * 0.45).min(layout::ui_text_width(atlas, &value) + 4.0);
                    layout::push_ui_text_right(
                        out,
                        atlas,
                        Viewport {
                            x: value_right - value_w,
                            y: ry + 12.0,
                            width: value_w,
                            height: 20.0,
                        },
                        &value,
                        if *differs { theme.text } else { dim },
                    );
                    hits.push((row, Action::Edit(key)));
                    value_right - value_w - 16.0 - (x + 16.0)
                }
                Control::Toggle => {
                    let rect = layout::toggle_rect(control_right, control_y);
                    layout::push_toggle(out, rect, page.settings.flag_of(key), theme, None);
                    hits.push((rect, Action::Toggle(key)));
                    *room
                }
                Control::Stepper { min, max } => {
                    let value = page.settings.number_of(key);
                    let rects =
                        layout::stepper_rects(control_right, control_y, layout::UI_CONTROL_SM);
                    let can = [value > *min + 0.001, value < *max - 0.001];
                    layout::push_stepper(
                        out,
                        atlas,
                        rects,
                        &page.settings.display_of(key),
                        can,
                        theme,
                    );
                    if can[0] {
                        hits.push((rects[0], Action::Step(key, false)));
                    }
                    if can[1] {
                        hits.push((rects[2], Action::Step(key, true)));
                    }
                    *room
                }
                Control::Choice(choices) => {
                    let word = page.settings.word_of(key);
                    let mut cx = control_right - control_width(atlas, *control);
                    let segments: Vec<layout::Segment> = choices
                        .iter()
                        .map(|(label, w)| {
                            let width = segment_width(atlas, label);
                            let rect = Viewport {
                                x: cx,
                                y: control_y,
                                width,
                                height: layout::UI_CONTROL_SM,
                            };
                            cx += width;
                            hits.push((rect, Action::Choose(key, w)));
                            layout::Segment::new(rect, label).on(*w == word)
                        })
                        .collect();
                    layout::push_segmented(out, atlas, &segments, theme);
                    *room
                }
            };
            text(
                out,
                atlas,
                x + 16.0,
                ry + 12.0,
                name_w.max(0.0),
                d.label,
                theme.text,
            );
            let mut ly = ry + 36.0;
            for line in lines {
                text(out, atlas, x + 16.0, ly, measure - 32.0, line, dim);
                ly += 20.0;
            }
            ry += row.height;
        }
        y = card.y + card.height + 24.0;
    }
    page.content = y + page.scroll - body_top;
    page.hits = hits;
    // A row scrolled under the header or past the bottom is cut away, so
    // it must not keep the pointer either.
    layout::clip_hotspots(body_hot, body);
    (body_top, body_start)
}

/// The width a control takes at a row's end; a text row's value is
/// laid out as it is drawn.
fn control_width(atlas: &mut Atlas, control: Control) -> f32 {
    match control {
        Control::Toggle => layout::UI_TOGGLE[0],
        Control::Stepper { .. } => layout::UI_STEP * 2.0 + layout::UI_STEP_VALUE,
        Control::Choice(choices) => choices
            .iter()
            .map(|(label, _)| segment_width(atlas, label))
            .sum(),
        Control::Text => 0.0,
    }
}

/// One choice's cell in a segmented control: its label with room round it.
fn segment_width(atlas: &mut Atlas, label: &str) -> f32 {
    (layout::ui_text_width(atlas, label) + 20.0).ceil()
}

fn row_height(lines: usize) -> f32 {
    36.0 + lines as f32 * 20.0 + 10.0
}
