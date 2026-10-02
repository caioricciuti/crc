//! The Extensions page: what is installed, what the signed registry offers,
//! and the details of one of them. It takes the editor column the way a
//! Source Control diff does, and is closed with Escape or a tab.
//!
//! Installing always goes through a confirmation that lists every
//! capability the extension asks for, and says so plainly when it is
//! unsigned. It is drawn in the page, not a modal, so it can be read
//! alongside the README and so the GUI self-test can drive it.

use std::collections::HashMap;

use crate::ext::manifest::Manifest;
use crate::ext::registry::Entry;
use crate::ext::store::{Installed, Package};
use crate::render::font::Atlas;
use crate::render::layout::{self, Theme, Viewport};
use crate::render::metal::GlyphInstance;

pub enum Registry {
    Loading,
    /// What it offers, and what this crc cannot use from it.
    Ready(Vec<Entry>, Vec<crate::ext::registry::Skipped>),
    Failed(String),
}

/// An install waiting for the person's yes.
pub enum Pending {
    Registry(Box<Entry>),
    Folder(Box<Package>),
}

impl Pending {
    pub fn manifest(&self) -> &Manifest {
        match self {
            Pending::Registry(e) => &e.manifest,
            Pending::Folder(p) => &p.manifest,
        }
    }
    pub fn signed(&self) -> bool {
        matches!(self, Pending::Registry(_))
    }
}

pub struct Page {
    pub installed: Vec<Installed>,
    pub registry: Registry,
    /// The extension whose details show, by id.
    pub selected: Option<String>,
    pub confirm: Option<Pending>,
    /// Work in progress, such as "Installing Sort Lines…".
    pub busy: Option<String>,
    /// What the last action came to.
    pub note: Option<String>,
    /// Recent log lines per extension, from its calls.
    pub logs: HashMap<String, Vec<String>>,
    pub hits: Vec<(Viewport, Action)>,
    /// The list's rows in the sidebar, as last drawn.
    pub list_hits: Vec<(Viewport, Action)>,
    /// Whether the details have the editor column. Escape and a tab give
    /// the column back; the list stays in the sidebar.
    pub details: bool,
    /// The README's first block shown: it scrolls under the wheel.
    pub readme_scroll: usize,
    /// Where the README was drawn, for the wheel.
    pub readme_rect: Option<Viewport>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Select(String),
    Install(String),
    Uninstall(String),
    Toggle(String),
    InstallFolder,
    Refresh,
    Confirm,
    Cancel,
    /// Gives the editor column back; the list stays in the sidebar.
    Close,
    /// Back to the page's home: what is installed and offered, and the
    /// actions that are about all of them.
    Home,
}

impl Action {
    /// A name for scripts: `extensions.install.crc.sort-lines`.
    pub fn name(&self) -> String {
        match self {
            Action::Select(id) => format!("extensions.select.{id}"),
            Action::Install(id) => format!("extensions.install.{id}"),
            Action::Uninstall(id) => format!("extensions.uninstall.{id}"),
            Action::Toggle(id) => format!("extensions.toggle.{id}"),
            Action::InstallFolder => "extensions.folder".into(),
            Action::Refresh => "extensions.refresh".into(),
            Action::Confirm => "extensions.confirm".into(),
            Action::Cancel => "extensions.cancel".into(),
            Action::Close => "extensions.close".into(),
            Action::Home => "extensions.home".into(),
        }
    }
}

impl Page {
    pub fn new(installed: Vec<Installed>) -> Page {
        // Opens on its home, not on whichever extension came first.
        Page {
            installed,
            registry: Registry::Loading,
            selected: None,
            confirm: None,
            busy: None,
            note: None,
            logs: HashMap::new(),
            hits: Vec::new(),
            list_hits: Vec::new(),
            details: true,
            readme_scroll: 0,
            readme_rect: None,
        }
    }

    /// Scrolls the README by `blocks`, as the wheel over it asks.
    pub fn scroll_readme(&mut self, blocks: isize) {
        self.readme_scroll = self.readme_scroll.saturating_add_signed(blocks);
    }

    pub fn installed(&self, id: &str) -> Option<&Installed> {
        self.installed.iter().find(|i| i.manifest.id == id)
    }

    pub fn available(&self, id: &str) -> Option<&Entry> {
        match &self.registry {
            Registry::Ready(entries, _) => entries.iter().find(|e| e.manifest.id == id),
            _ => None,
        }
    }

    /// Registry entries not installed, or newer than what is.
    fn offered(&self) -> Vec<&Entry> {
        match &self.registry {
            Registry::Ready(entries, _) => entries
                .iter()
                .filter(|e| {
                    self.installed(&e.manifest.id)
                        .is_none_or(|i| newer(&e.manifest.version, &i.manifest.version))
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<Action> {
        self.hits
            .iter()
            .filter(|_| self.details)
            .chain(self.list_hits.iter())
            .rev()
            .find(|(r, _)| r.contains(x, y))
            .map(|(_, a)| a.clone())
    }

    /// Where a named target is, for scripts.
    pub fn named(&self, name: &str) -> Option<Viewport> {
        self.hits
            .iter()
            .filter(|_| self.details)
            .chain(self.list_hits.iter())
            .find(|(_, a)| a.name() == name)
            .map(|(r, _)| *r)
    }
}

/// Whether version `a` is newer than `b`, by the same rule crc's own
/// updates use: numbers first, and a prerelease before its release.
pub fn newer(a: &str, b: &str) -> bool {
    crate::platform::update::compare(a, b) == std::cmp::Ordering::Greater
}

const PAD: f32 = 28.0;
const BUTTON_H: f32 = layout::UI_CONTROL;

fn button(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    theme: &Theme,
    x: f32,
    y: f32,
    label: &str,
    primary: bool,
) -> Viewport {
    let width = layout::ui_text_width(atlas, label) + 28.0;
    let rect = Viewport {
        x,
        y,
        width,
        height: BUTTON_H,
    };
    layout::Button::new(rect)
        .label(label)
        .tone(if primary {
            layout::Tone::Primary
        } else {
            layout::Tone::Secondary
        })
        .draw(out, atlas, theme);
    rect
}

fn text(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    x: f32,
    y: f32,
    width: f32,
    s: &str,
    color: [f32; 4],
) {
    layout::push_ui_text(
        out,
        atlas,
        Viewport {
            x,
            y,
            width,
            height: 20.0,
        },
        s,
        color,
    );
}

/// The editor column: the header with its two actions, then the selected
/// extension's details, or the install confirmation.
pub fn draw_details(
    page: &mut Page,
    atlas: &mut Atlas,
    rect: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    let start = out.len();
    draw_page(page, atlas, rect, theme, out);
    // A long description, many commands or a long log run past a short
    // window: what does not fit is cut at the page's edge, not drawn over
    // what is below it, and cannot be clicked there.
    let bottom = rect.y + rect.height;
    for quad in &mut out[start..] {
        layout::clip_vertical(quad, rect.y, bottom);
    }
    page.hits.retain(|(r, _)| r.y + r.height <= bottom);
}

fn draw_page(
    page: &mut Page,
    atlas: &mut Atlas,
    rect: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    let mut hits = Vec::new();
    layout::push_rect(
        out,
        atlas,
        [rect.x, rect.y],
        [rect.width, rect.height],
        theme.tab_active,
    );
    if rect.width < 320.0 || rect.height < 200.0 {
        page.hits = hits;
        return;
    }
    let dim = theme.status_text;
    let x = rect.x + PAD;
    let right = rect.x + rect.width - PAD;
    let bottom = rect.y + rect.height - 12.0;
    let mut y = rect.y + 22.0;
    page.readme_rect = None;
    let home = page.selected.is_none() && page.confirm.is_none();
    // Work under way gets a spinner before its words.
    let working =
        page.busy.is_some() || (page.note.is_none() && matches!(page.registry, Registry::Loading));
    let status = match (&page.busy, &page.note, &page.registry) {
        (Some(busy), _, _) => busy.clone(),
        (None, Some(note), _) => note.clone(),
        (None, None, Registry::Loading) => "Checking the registry\u{2026}".into(),
        (None, None, Registry::Failed(why)) => format!("Registry: {why}"),
        (None, None, Registry::Ready(entries, skipped)) => {
            let mut status = format!(
                "{} installed \u{b7} {} in the registry, signed",
                page.installed.len(),
                entries.len()
            );
            if let Some(first) = skipped.first() {
                status.push_str(&format!(
                    " \u{b7} {} is not shown: {}",
                    first.name, first.reason
                ));
                if skipped.len() > 1 {
                    status.push_str(&format!(", and {} more", skipped.len() - 1));
                }
            }
            status
        }
    };
    if home {
        // The header is the home page's: the title, the count, and the
        // actions that are about extensions in general.
        layout::push_title(
            out,
            atlas,
            x,
            y - 6.0,
            20.0,
            "Extensions",
            theme.text,
            260.0,
        );
        let mut bx = right;
        for (label, action) in [
            ("Close", Action::Close),
            ("Refresh", Action::Refresh),
            ("Install from Folder\u{2026}", Action::InstallFolder),
        ] {
            let w = layout::ui_text_width(atlas, label) + 28.0;
            bx -= w;
            let r = button(out, atlas, theme, bx, y - 4.0, label, false);
            hits.push((r, action));
            bx -= 8.0;
        }
        y += 26.0;
        let mut sx = x;
        if working {
            sx += layout::push_spinner(
                out,
                x + 1.0,
                Viewport {
                    x,
                    y,
                    width: 20.0,
                    height: 20.0,
                },
                theme.accent,
            ) + 8.0;
        }
        text(out, atlas, sx, y, right - sx, &status, dim);
        y += 28.0;
        layout::push_rect(out, atlas, [x, y], [right - x, 1.0], theme.hairline);
        y += 12.0;
    } else {
        // One extension: a way back to the home, and what just happened
        // (an install, an error) when something did.
        let r = link(out, atlas, theme, x, y - 4.0, "\u{2039} Extensions");
        hits.push((r, Action::Home));
        if page.busy.is_some() || page.note.is_some() {
            let mut sx = r.x + r.width + 16.0;
            if working {
                sx += layout::push_spinner(
                    out,
                    sx,
                    Viewport {
                        x: sx,
                        y,
                        width: 20.0,
                        height: 20.0,
                    },
                    theme.accent,
                ) + 8.0;
            }
            text(out, atlas, sx, y, right - sx, &status, dim);
        }
        y += 34.0;
    }

    // The details take the column; the list is in the sidebar.
    // Prose keeps a readable measure rather than the width of a wide window.
    let detail_x = x;
    let detail_w = (right - x).min(680.0);

    // Details of the selected one, or the install confirmation.
    let dx = detail_x;
    if let Some(pending) = &page.confirm {
        let m = pending.manifest();
        layout::push_title(
            out,
            atlas,
            dx,
            y - 6.0,
            20.0,
            &format!("Install {} {}?", m.name, m.version),
            theme.text,
            detail_w,
        );
        y += 34.0;
        if !pending.signed() {
            for line in layout::wrap_words(
                atlas,
                "Unsigned: this did not come from the signed registry, so nobody has checked it. Install it only if you trust where it came from.",
                detail_w,
            ) {
                text(out, atlas, dx, y, detail_w, &line, theme.diff_removed);
                y += 20.0;
            }
            y += 8.0;
        }
        text(out, atlas, dx, y, detail_w, "It will be able to:", dim);
        y += 24.0;
        for capability in &m.capabilities {
            text(
                out,
                atlas,
                dx + 12.0,
                y,
                detail_w - 12.0,
                &format!("\u{2022} {}", capability.describe()),
                theme.text,
            );
            y += 22.0;
        }
        text(
            out,
            atlas,
            dx + 12.0,
            y,
            detail_w - 12.0,
            "Nothing else: no files, no network, no other programs.",
            dim,
        );
        y += 34.0;
        let r = button(out, atlas, theme, dx, y, "Install", true);
        hits.push((r, Action::Confirm));
        let r = button(out, atlas, theme, r.x + r.width + 10.0, y, "Cancel", false);
        hits.push((r, Action::Cancel));
        page.hits = hits;
        return;
    }
    let Some(id) = page.selected.clone() else {
        for line in layout::wrap_words(
            atlas,
            "Extensions add commands to crc. Pick one in the list to see what it does and what it may touch. Installed ones run from the Extensions menu, from the editor's right-click menu, and from the palette.",
            detail_w,
        ) {
            text(out, atlas, dx, y, detail_w, &line, dim);
            y += 20.0;
        }
        page.hits = hits;
        return;
    };
    let installed = page.installed(&id).cloned();
    let entry = page.available(&id).cloned();
    let Some(manifest) = installed
        .as_ref()
        .map(|i| i.manifest.clone())
        .or_else(|| entry.as_ref().map(|e| e.manifest.clone()))
    else {
        page.hits = hits;
        return;
    };
    // A tile with its icon, the name large beside it, and the facts.
    let tile = Viewport {
        x: dx,
        y,
        width: 52.0,
        height: 52.0,
    };
    layout::push_rounded_rect(out, tile, 12.0, theme.tab_hover);
    layout::push_icon_scaled(
        out,
        atlas,
        tile,
        crate::project::icons::extension_icon(&manifest.icon),
        theme.accent,
        1.9,
    );
    let nx = dx + tile.width + 14.0;
    layout::push_title(
        out,
        atlas,
        nx,
        y + 2.0,
        20.0,
        &manifest.name,
        theme.text,
        detail_w - 66.0,
    );
    let header_y = y;
    y += 30.0;
    let mut facts = vec![manifest.version.clone(), manifest.license.clone()];
    if !manifest.authors.is_empty() {
        facts.push(format!("by {}", manifest.authors.join(", ")));
    }
    facts.push(match &installed {
        Some(i) if i.tampered => "changed since it was installed: reinstall it".into(),
        Some(i) if i.signed => "signed".into(),
        Some(_) => "unsigned".into(),
        None => "signed".into(),
    });
    text(
        out,
        atlas,
        nx,
        y,
        detail_w - 66.0,
        &facts.join(" \u{b7} "),
        dim,
    );
    y = header_y + tile.height + 18.0;

    // What can be done with it here.
    let mut bx = dx;
    let mut add = |out: &mut Vec<GlyphInstance>,
                   atlas: &mut Atlas,
                   label: &str,
                   action: Action,
                   primary: bool| {
        let r = button(out, atlas, theme, bx, y, label, primary);
        bx += r.width + 10.0;
        hits.push((r, action));
    };
    match (&installed, &entry) {
        (None, Some(_)) => add(out, atlas, "Install", Action::Install(id.clone()), true),
        (Some(i), e) => {
            if e.as_ref()
                .is_some_and(|e| newer(&e.manifest.version, &i.manifest.version))
            {
                add(out, atlas, "Update", Action::Install(id.clone()), true);
            }
            add(
                out,
                atlas,
                if i.enabled { "Disable" } else { "Enable" },
                Action::Toggle(id.clone()),
                false,
            );
            add(
                out,
                atlas,
                "Uninstall",
                Action::Uninstall(id.clone()),
                false,
            );
        }
        (None, None) => {}
    }
    y += BUTTON_H + 20.0;

    for line in layout::wrap_words(atlas, &manifest.description, detail_w) {
        text(out, atlas, dx, y, detail_w, &line, theme.text);
        y += 20.0;
    }
    y += 14.0;
    // What it may touch and what it adds, each on a card with a heading,
    // one line per item behind an icon.
    let card = |out: &mut Vec<GlyphInstance>,
                atlas: &mut Atlas,
                y: f32,
                heading: &str,
                glyph: char,
                items: &[String]|
     -> f32 {
        let height = 34.0 + items.len().max(1) as f32 * 22.0 + 8.0;
        layout::push_card(
            out,
            Viewport {
                x: dx,
                y,
                width: detail_w,
                height,
            },
            theme,
        );
        text(
            out,
            atlas,
            dx + 14.0,
            y + 10.0,
            detail_w - 28.0,
            &heading.to_uppercase(),
            dim,
        );
        let mut ry = y + 34.0;
        if items.is_empty() {
            text(out, atlas, dx + 14.0, ry, detail_w - 28.0, "Nothing", dim);
        }
        for item in items {
            let w = layout::icon_width(atlas, glyph);
            layout::push_icon_centered(
                out,
                atlas,
                Viewport {
                    x: dx + 14.0,
                    y: ry,
                    width: w,
                    height: 20.0,
                },
                glyph,
                theme.accent,
            );
            text(
                out,
                atlas,
                dx + 14.0 + w + 8.0,
                ry,
                detail_w - 36.0 - w,
                item,
                theme.text,
            );
            ry += 22.0;
        }
        y + height
    };
    let may: Vec<String> = manifest
        .capabilities
        .iter()
        .map(|c| c.describe().to_owned())
        .collect();
    y = card(out, atlas, y, "It may", crate::project::icons::CHECK, &may) + 12.0;
    let commands: Vec<String> = manifest.commands.iter().map(|c| c.title.clone()).collect();
    y = card(
        out,
        atlas,
        y,
        "Commands: Extensions menu, right-click, palette",
        crate::project::icons::CHEVRON_RIGHT,
        &commands,
    );
    if let Some(log) = page.logs.get(&id).filter(|l| !l.is_empty()) {
        y += 12.0;
        text(out, atlas, dx, y, detail_w, "Log:", dim);
        y += 22.0;
        for line in log.iter().rev().take(6).rev() {
            text(out, atlas, dx + 12.0, y, detail_w - 12.0, line, dim);
            y += 20.0;
        }
    }
    let readme = installed
        .as_ref()
        .map(|i| i.readme.clone())
        .or_else(|| entry.as_ref().map(|e| e.readme.clone()))
        .unwrap_or_default();
    if !readme.trim().is_empty() {
        y += 16.0;
        layout::push_rect(out, atlas, [dx, y], [detail_w, 1.0], theme.hairline);
        y += 4.0;
        // The README as the Markdown preview draws a document: headings,
        // lists, code, tables. It scrolls under the wheel.
        if bottom > y + 40.0 {
            let blocks = crate::markdown::parse_spanned(&readme);
            page.readme_scroll = page.readme_scroll.min(blocks.len().saturating_sub(1));
            let view = Viewport {
                x: dx - 28.0,
                y,
                width: detail_w + 56.0,
                height: bottom - y,
            };
            layout::build_markdown_appending(&blocks, page.readme_scroll, atlas, view, theme, out);
            page.readme_rect = Some(view);
        }
    }
    page.hits = hits;
}

/// A borderless text button, for the way back.
fn link(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    theme: &Theme,
    x: f32,
    y: f32,
    label: &str,
) -> Viewport {
    let width = layout::ui_text_width(atlas, label) + 12.0;
    let r = Viewport {
        x: x - 6.0,
        y,
        width,
        height: BUTTON_H,
    };
    layout::push_ui_text(
        out,
        atlas,
        Viewport {
            x,
            width: width - 6.0,
            ..r
        },
        label,
        theme.accent,
    );
    r
}

/// The sidebar: installed extensions, then what the registry offers that is
/// not installed or is newer. A row selects it and shows its details.
pub fn draw_list(
    page: &mut Page,
    atlas: &mut Atlas,
    rect: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    let mut hits = Vec::new();
    let dim = theme.status_text;
    // The title row: the panel's name, then Install from Folder and a new
    // look at the registry.
    layout::push_sidebar_title(out, atlas, rect, "EXTENSIONS", 2, theme);
    let [folder, refresh] = layout::sidebar_header_buttons::<2>(rect);
    let loading = matches!(page.registry, Registry::Loading);
    for (r, icon, tip, action, enabled) in [
        (
            folder,
            crate::project::icons::NEW_FOLDER,
            "Install from a Folder\u{2026}",
            Action::InstallFolder,
            true,
        ),
        (
            refresh,
            crate::project::icons::REFRESH,
            "Check the Registry Again",
            Action::Refresh,
            !loading,
        ),
    ] {
        layout::Button::new(r)
            .icon(icon)
            .tone(layout::Tone::Ghost)
            .enabled(enabled)
            .tip(tip)
            .draw(out, atlas, theme);
        if enabled {
            hits.push((r, action));
        }
    }
    if loading {
        layout::push_progress(
            out,
            atlas,
            Viewport {
                x: rect.x,
                y: rect.y + layout::SIDEBAR_HEADER_HEIGHT - 2.0,
                width: rect.width - 1.0,
                height: 2.0,
            },
            theme,
        );
    }
    let x = rect.x + layout::UI_INSET;
    let width = rect.width - layout::UI_INSET * 2.0;
    let bottom = rect.y + rect.height - 8.0;
    let mut y = rect.y + layout::SIDEBAR_HEADER_HEIGHT + 4.0;

    // A section heading, as Source Control draws its own: small capitals
    // and a count.
    let heading = |out: &mut Vec<GlyphInstance>,
                   atlas: &mut Atlas,
                   y: f32,
                   title: &str,
                   count: Option<usize>| {
        let row = Viewport {
            x,
            y,
            width,
            height: 24.0,
        };
        let upper = title.to_uppercase();
        layout::push_ui_text(out, atlas, row, &upper, dim);
        let w = layout::ui_text_width(atlas, &upper);
        if let Some(count) = count {
            layout::push_count(out, atlas, x + w + 8.0, row, count, theme);
        }
        w
    };

    let mut rows: Vec<(&'static str, String, String, String, char)> = Vec::new();
    for i in &page.installed {
        let mut state = i.manifest.version.clone();
        if !i.enabled {
            state.push_str(" \u{b7} off");
        }
        if i.tampered {
            state.push_str(" \u{b7} changed, reinstall");
        } else if !i.signed {
            state.push_str(" \u{b7} unsigned");
        }
        rows.push((
            "Installed",
            i.manifest.id.clone(),
            i.manifest.name.clone(),
            state,
            crate::project::icons::extension_icon(&i.manifest.icon),
        ));
    }
    let offered = page.offered();
    for e in &offered {
        let state = if page.installed(&e.manifest.id).is_some() {
            format!("update to {}", e.manifest.version)
        } else {
            e.manifest.version.clone()
        };
        rows.push((
            "Available",
            e.manifest.id.clone(),
            e.manifest.name.clone(),
            state,
            crate::project::icons::extension_icon(&e.manifest.icon),
        ));
    }
    let count_of = |section: &str| rows.iter().filter(|r| r.0 == section).count();
    const ROW_H: f32 = 44.0;
    let mut last = "";
    for (section, id, name, state, icon) in &rows {
        if *section != last {
            if !last.is_empty() {
                y += 8.0;
            }
            if y + 24.0 > bottom {
                break;
            }
            heading(out, atlas, y, section, Some(count_of(section)));
            y += 26.0;
            last = section;
        }
        if y + ROW_H > bottom {
            break;
        }
        let row = Viewport {
            x: rect.x,
            y,
            width: rect.width,
            height: ROW_H,
        };
        if page.selected.as_deref() == Some(id.as_str()) && page.details {
            layout::push_rounded_rect(
                out,
                Viewport {
                    x: row.x + 6.0,
                    y: row.y + 1.0,
                    width: row.width - 12.0,
                    height: row.height - 2.0,
                },
                layout::UI_RADIUS,
                theme.sidebar_selected,
            );
            layout::hotspot(row, layout::Cursor::Pointing, None);
        } else {
            layout::push_row_hover(out, row, theme);
        }
        // The icon on a tile, so a row of extensions reads as a row of
        // things, not a column of glyphs.
        let tile = Viewport {
            x,
            y: y + 6.0,
            width: 32.0,
            height: 32.0,
        };
        layout::push_rounded_rect(out, tile, layout::UI_RADIUS + 1.0, theme.tab_hover);
        layout::push_icon_scaled(out, atlas, tile, *icon, theme.accent, 1.25);
        let tx = x + 32.0 + 10.0;
        let tw = (width - 42.0).max(0.0);
        text(out, atlas, tx, y + 4.0, tw, name, theme.text);
        let state_colour = if state.contains("update to") {
            theme.accent
        } else if state.contains("changed") {
            theme.diff_removed
        } else {
            theme.gutter_text
        };
        text(out, atlas, tx, y + 21.0, tw, state, state_colour);
        hits.push((row, Action::Select(id.clone())));
        y += ROW_H;
    }
    // Until the registry answers, Available is there with rows sketched in
    // it, so the list has its shape at once and does not jump when the
    // answer lands.
    match &page.registry {
        Registry::Loading => {
            if last != "Available" {
                if !last.is_empty() {
                    y += 8.0;
                }
                if y + 24.0 <= bottom {
                    let w = heading(out, atlas, y, "Available", None);
                    layout::push_spinner(
                        out,
                        x + w + 10.0,
                        Viewport {
                            x,
                            y,
                            width,
                            height: 24.0,
                        },
                        theme.accent,
                    );
                    layout::hotspot(
                        Viewport {
                            x,
                            y,
                            width,
                            height: 24.0,
                        },
                        layout::Cursor::Arrow,
                        Some("Checking the registry\u{2026}"),
                    );
                    y += 26.0;
                }
            }
            for (n, widths) in [(0.55, 0.3), (0.7, 0.25), (0.45, 0.35)]
                .into_iter()
                .enumerate()
            {
                if y + ROW_H > bottom {
                    break;
                }
                let _ = n;
                layout::push_skeleton_row(
                    out,
                    Viewport {
                        x,
                        y,
                        width,
                        height: ROW_H,
                    },
                    32.0,
                    widths,
                    true,
                    theme,
                );
                y += ROW_H;
            }
        }
        Registry::Failed(why) => {
            if last != "Available" {
                if !last.is_empty() {
                    y += 8.0;
                }
                if y + 24.0 <= bottom {
                    heading(out, atlas, y, "Available", None);
                    y += 26.0;
                }
            }
            let icon_w = layout::icon_width(atlas, crate::project::icons::ERROR);
            if y + 22.0 <= bottom {
                layout::push_icon_centered(
                    out,
                    atlas,
                    Viewport {
                        x,
                        y,
                        width: icon_w,
                        height: 22.0,
                    },
                    crate::project::icons::ERROR,
                    theme.diff_removed,
                );
                text(
                    out,
                    atlas,
                    x + icon_w + 6.0,
                    y + 1.0,
                    width - icon_w - 6.0,
                    "The registry did not answer",
                    theme.text,
                );
                y += 24.0;
            }
            for line in layout::wrap_words(atlas, why, width).into_iter().take(4) {
                if y + 20.0 > bottom {
                    break;
                }
                text(out, atlas, x, y, width, &line, dim);
                y += 20.0;
            }
            let retry = Viewport {
                x,
                y: y + 6.0,
                width: layout::ui_text_width(atlas, "Try Again") + 28.0,
                height: layout::UI_CONTROL,
            };
            if retry.y + retry.height <= bottom {
                layout::Button::new(retry)
                    .label("Try Again")
                    .draw(out, atlas, theme);
                hits.push((retry, Action::Refresh));
            }
        }
        Registry::Ready(..) => {
            if rows.is_empty() {
                text(out, atlas, x, y, width, "Nothing installed yet.", dim);
            }
        }
    }
    page.list_hits = hits;
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_newer_version_by_the_update_rule() {
        assert!(newer("0.2.0", "0.1.9"));
        assert!(newer("0.10.0", "0.9.0"), "numbers, not text");
        assert!(
            newer("0.2.0", "0.2.0-rc.1"),
            "a release after its prerelease"
        );
        assert!(!newer("0.2.0-rc.1", "0.2.0"));
        assert!(!newer("1.0.0", "1.0.0"));
    }

    use super::newer;

    #[test]
    fn versions_compare_by_number() {
        assert!(newer("0.2.0", "0.1.9"));
        assert!(newer("0.10.0", "0.9.0"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.2.0"));
    }
}
