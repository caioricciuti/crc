//! The Settings page: Cmd-, gives it the editor column. Every setting is
//! listed with what it does, its value and its default, read from
//! `config.toml`, which stays the one place a setting is written: Edit
//! opens the file at that key, and saving the file updates the page.

use crate::platform::mcp_page::{button, link, text};
use crate::platform::settings::{DESCRIBED, Settings};
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

const INTRO: &str = "crc reads these from config.toml. Edit opens the file at the setting, and saving the file applies it. A value that differs from its default is shown with the default beside it.";

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
    /// `settings.close`, `settings.edit.KEY`.
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

    // The header: the title, then the actions from the right, the least
    // needed dropped first when the column is narrow.
    let title_w = layout::push_title(out, atlas, x, y - 6.0, 20.0, "Settings", theme.text, 200.0);
    let mut bx = right;
    for (label, action, primary) in [
        ("Close", Action::Close, false),
        ("Open config.toml", Action::Open, true),
        ("Reload", Action::Reload, false),
    ] {
        let w = layout::ui_text_width(atlas, label) + 28.0;
        if bx - w < x + title_w + 24.0 {
            break;
        }
        bx -= w;
        let r = button(out, atlas, theme, bx, y - 4.0, label, primary);
        hits.push((r, action));
        bx -= 8.0;
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
    text(out, atlas, x, y, right - x, &status, colour);
    y += 28.0;
    layout::push_rect(out, atlas, [x, y], [right - x, 1.0], theme.hairline);
    y += 12.0;

    // The body, scrolled.
    let body_top = y;
    let body_start = out.len();
    let bottom = rect.y + rect.height;
    page.rect = Some(Viewport {
        x: rect.x,
        y: body_top,
        width: rect.width,
        height: (bottom - body_top).max(0.0),
    });
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
        layout::push_title(out, atlas, x, y, 15.0, group, theme.text, measure);
        y += 28.0;
        // Each row: the key and its value, Edit at the trailing edge, then
        // what it does and, when changed, the default.
        let rows: Vec<_> = DESCRIBED
            .iter()
            .filter(|d| d.group == group)
            .map(|d| {
                let value = page.settings.value_of(d.key);
                let default = defaults.value_of(d.key);
                let differs = value != default;
                let mut lines = layout::wrap_words(atlas, d.what, measure - 32.0);
                if differs {
                    lines.push(format!("Default: {default}"));
                }
                (d.key, value, differs, lines)
            })
            .collect();
        let height: f32 = rows
            .iter()
            .map(|(_, _, _, lines)| row_height(lines.len()))
            .sum();
        let card = Viewport {
            x,
            y,
            width: measure,
            height,
        };
        layout::push_card(out, card, theme);
        let mut ry = y;
        for (n, (key, value, differs, lines)) in rows.iter().enumerate() {
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
            if layout::hovered(row) {
                layout::push_rounded_rect(out, row, layout::UI_RADIUS + 2.0, theme.row_hover);
            }
            let edit_w = layout::ui_text_width(atlas, "Edit") + 12.0;
            let edit = link(
                out,
                atlas,
                theme,
                x + measure - 16.0 - edit_w + 6.0,
                ry + 22.0 - layout::UI_CONTROL * 0.5,
                "Edit",
            );
            let value_w = (measure * 0.45).min(layout::ui_text_width(atlas, value) + 4.0);
            let value_x = edit.x - 12.0 - value_w;
            text(
                out,
                atlas,
                x + 16.0,
                ry + 12.0,
                value_x - x - 28.0,
                key,
                theme.text,
            );
            layout::push_ui_text_right(
                out,
                atlas,
                Viewport {
                    x: value_x,
                    y: ry + 12.0,
                    width: value_w,
                    height: 20.0,
                },
                value,
                if *differs { theme.accent } else { dim },
            );
            let mut ly = ry + 36.0;
            for line in lines {
                text(out, atlas, x + 16.0, ly, measure - 32.0, line, dim);
                ly += 20.0;
            }
            hits.push((edit, Action::Edit(key)));
            hits.push((row, Action::Edit(key)));
            ry += row.height;
        }
        y = card.y + card.height + 24.0;
    }
    page.content = y + page.scroll - body_top;
    page.hits = hits;
    (body_top, body_start)
}

fn row_height(lines: usize) -> f32 {
    36.0 + lines as f32 * 20.0 + 10.0
}
