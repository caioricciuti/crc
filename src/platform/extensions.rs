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
    Ready(Vec<Entry>),
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
        }
    }
}

impl Page {
    pub fn new(installed: Vec<Installed>) -> Page {
        let selected = installed.first().map(|i| i.manifest.id.clone());
        Page {
            installed,
            registry: Registry::Loading,
            selected,
            confirm: None,
            busy: None,
            note: None,
            logs: HashMap::new(),
            hits: Vec::new(),
            list_hits: Vec::new(),
            details: true,
        }
    }

    pub fn installed(&self, id: &str) -> Option<&Installed> {
        self.installed.iter().find(|i| i.manifest.id == id)
    }

    pub fn available(&self, id: &str) -> Option<&Entry> {
        match &self.registry {
            Registry::Ready(entries) => entries.iter().find(|e| e.manifest.id == id),
            _ => None,
        }
    }

    /// Registry entries not installed, or newer than what is.
    fn offered(&self) -> Vec<&Entry> {
        match &self.registry {
            Registry::Ready(entries) => entries
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

/// Whether version `a` is newer than `b`, both x.y.z.
pub fn newer(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> { v.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    parse(a) > parse(b)
}

const PAD: f32 = 28.0;
const BUTTON_H: f32 = 26.0;

fn button(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    theme: &Theme,
    x: f32,
    y: f32,
    label: &str,
    primary: bool,
) -> Viewport {
    let width = layout::ui_text_width(atlas, label) + 24.0;
    let rect = Viewport {
        x,
        y,
        width,
        height: BUTTON_H,
    };
    if primary {
        layout::push_rounded_rect(out, rect, 5.0, theme.accent);
        layout::push_ui_text_centered(out, atlas, rect, label, theme.tab_active);
    } else {
        layout::push_rounded_rect(out, rect, 5.0, theme.palette_selected);
        layout::push_ui_text_centered(out, atlas, rect, label, theme.text);
    }
    rect
}

/// `text` broken into lines that fit `width`, at spaces.
fn wrap(atlas: &mut Atlas, text: &str, width: f32) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_owned()
            } else {
                format!("{line} {word}")
            };
            if !line.is_empty() && layout::ui_text_width(atlas, &candidate) > width {
                lines.push(std::mem::take(&mut line));
                line = word.to_owned();
            } else {
                line = candidate;
            }
        }
        lines.push(line);
    }
    lines
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

    // Header: the title, and the two actions that are always there.
    text(out, atlas, x, y, 200.0, "Extensions", theme.text);
    let mut bx = right;
    for (label, action) in [
        ("Close", Action::Close),
        ("Refresh", Action::Refresh),
        ("Install from Folder\u{2026}", Action::InstallFolder),
    ] {
        let w = layout::ui_text_width(atlas, label) + 24.0;
        bx -= w;
        let r = button(out, atlas, theme, bx, y - 4.0, label, false);
        hits.push((r, action));
        bx -= 8.0;
    }
    y += 26.0;
    let status = match (&page.busy, &page.note, &page.registry) {
        (Some(busy), _, _) => busy.clone(),
        (None, Some(note), _) => note.clone(),
        (None, None, Registry::Loading) => "Checking the registry\u{2026}".into(),
        (None, None, Registry::Failed(why)) => format!("Registry: {why}"),
        (None, None, Registry::Ready(entries)) => format!(
            "{} installed \u{b7} {} in the registry, signed",
            page.installed.len(),
            entries.len()
        ),
    };
    text(out, atlas, x, y, right - x, &status, dim);
    y += 28.0;
    layout::push_rect(out, atlas, [x, y], [right - x, 1.0], theme.hairline);
    y += 12.0;

    // The details take the column; the list is in the sidebar.
    // Prose keeps a readable measure rather than the width of a wide window.
    let detail_x = x;
    let detail_w = (right - x).min(680.0);
    let top = y;
    let _ = bottom;

    // Details of the selected one, or the install confirmation.
    let mut y = top;
    let dx = detail_x;
    if let Some(pending) = &page.confirm {
        let m = pending.manifest();
        text(
            out,
            atlas,
            dx,
            y,
            detail_w,
            &format!("Install {} {}?", m.name, m.version),
            theme.text,
        );
        y += 30.0;
        if !pending.signed() {
            for line in wrap(
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
        text(
            out,
            atlas,
            dx,
            y,
            detail_w,
            "Pick an extension to see what it does and what it may touch.",
            dim,
        );
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
    text(out, atlas, dx, y, detail_w, &manifest.name, theme.text);
    y += 24.0;
    let mut facts = vec![manifest.version.clone(), manifest.license.clone()];
    if !manifest.authors.is_empty() {
        facts.push(format!("by {}", manifest.authors.join(", ")));
    }
    facts.push(match &installed {
        Some(i) if i.signed => "signed".into(),
        Some(_) => "unsigned".into(),
        None => "signed".into(),
    });
    text(out, atlas, dx, y, detail_w, &facts.join(" \u{b7} "), dim);
    y += 30.0;

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

    for line in wrap(atlas, &manifest.description, detail_w) {
        text(out, atlas, dx, y, detail_w, &line, theme.text);
        y += 20.0;
    }
    y += 12.0;
    text(out, atlas, dx, y, detail_w, "May:", dim);
    y += 22.0;
    for capability in &manifest.capabilities {
        text(
            out,
            atlas,
            dx + 12.0,
            y,
            detail_w - 12.0,
            &format!("\u{2022} {}", capability.describe()),
            theme.text,
        );
        y += 20.0;
    }
    y += 12.0;
    text(
        out,
        atlas,
        dx,
        y,
        detail_w,
        "Commands, in the palette and the Extensions menu:",
        dim,
    );
    y += 22.0;
    for command in &manifest.commands {
        text(
            out,
            atlas,
            dx + 12.0,
            y,
            detail_w - 12.0,
            &format!("\u{2022} {}", command.title),
            theme.text,
        );
        y += 20.0;
    }
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
        y += 12.0;
        for raw in readme.lines() {
            // Plain text: headings brighter, `- ` items as bullets, bold
            // markers dropped.
            let heading = raw.starts_with('#');
            let line = raw.trim_start_matches('#').trim().replace("**", "");
            let line = match line.strip_prefix("- ") {
                Some(item) => format!("\u{2022} {item}"),
                None => line,
            };
            for part in wrap(atlas, &line, detail_w) {
                if y + 20.0 > bottom {
                    break;
                }
                text(
                    out,
                    atlas,
                    dx,
                    y,
                    detail_w,
                    &part,
                    if heading { theme.text } else { dim },
                );
                y += 20.0;
            }
        }
    }
    page.hits = hits;
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
    let (title, _) = layout::sidebar_switcher(rect);
    text(
        out,
        atlas,
        title.x + 2.0,
        title.y + 3.0,
        title.width,
        "EXTENSIONS",
        dim,
    );
    let x = rect.x + 12.0;
    let width = rect.width - 24.0;
    let bottom = rect.y + rect.height - 8.0;
    let mut y = title.y + title.height + 14.0;

    let mut rows: Vec<(&'static str, String, String, String, char)> = Vec::new();
    for i in &page.installed {
        let mut state = i.manifest.version.clone();
        if !i.enabled {
            state.push_str(" \u{b7} off");
        }
        if !i.signed {
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
    for e in page.offered() {
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
    let mut last = "";
    for (section, id, name, state, icon) in &rows {
        if *section != last {
            if y + 22.0 > bottom {
                break;
            }
            text(out, atlas, x, y, width, section, dim);
            y += 24.0;
            last = section;
        }
        const ROW_H: f32 = 44.0;
        if y + ROW_H > bottom {
            break;
        }
        let row = Viewport {
            x: rect.x + 4.0,
            y,
            width: rect.width - 8.0,
            height: ROW_H - 4.0,
        };
        if page.selected.as_deref() == Some(id.as_str()) && page.details {
            layout::push_rounded_rect(out, row, 6.0, theme.sidebar_selected);
        }
        layout::push_icon_scaled(
            out,
            atlas,
            Viewport {
                x,
                y: y + 3.0,
                width: 26.0,
                height: 34.0,
            },
            *icon,
            theme.accent,
            1.3,
        );
        text(
            out,
            atlas,
            x + 36.0,
            y + 2.0,
            width - 36.0,
            name,
            theme.sidebar_text,
        );
        text(out, atlas, x + 36.0, y + 20.0, width - 36.0, state, dim);
        hits.push((row, Action::Select(id.clone())));
        y += ROW_H;
    }
    if rows.is_empty() {
        let note = match page.registry {
            Registry::Loading => "Checking the registry\u{2026}",
            _ => "Nothing installed yet.",
        };
        text(out, atlas, x, y, width, note, dim);
    }
    page.list_hits = hits;
}

#[cfg(test)]
mod tests {
    use super::newer;

    #[test]
    fn versions_compare_by_number() {
        assert!(newer("0.2.0", "0.1.9"));
        assert!(newer("0.10.0", "0.9.0"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.2.0"));
    }
}
