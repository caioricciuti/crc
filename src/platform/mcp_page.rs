//! The MCP page: the editor column while the MCP sidebar is open, the way
//! the Extensions page takes it. It says what a server is, lists each one
//! as a card with its Start/Stop, and shows a server's tools, resources
//! and prompts with their descriptions. A new server is added from here:
//! an installed program through the Open panel, or a URL through the
//! palette. `mcp.json` stays the source of truth; the page only writes
//! entries into it.

use crate::mcp_client::{Server, Status};
use crate::platform::mcp_panel::{Action, Panel};
use crate::project::icons;
use crate::render::{
    font::Atlas,
    layout::{self, Theme, Viewport},
    metal::GlyphInstance,
};

const PAD: f32 = 28.0;
/// One row of a tools, resources or prompts list.
const ROW: f32 = 26.0;
/// A server's card on the home.
const CARD: f32 = 78.0;
/// Prose keeps a readable measure rather than the width of a wide window.
const MEASURE: f32 = 680.0;
/// The plug, as the activity strip draws it.
const PLUG: char = '\u{eb2d}';

/// What the page says a server is, on the home above the cards.
const WHAT: &str = "An MCP server is a program crc starts for you, or a web address it connects to. Each one offers tools, resources and prompts: click a tool to fill in a call and run it. Nothing starts until you click Start, and a launcher that downloads code at start (npx, uvx) is refused.";

const EXAMPLE: [&str; 9] = [
    "{",
    "  \"mcpServers\": {",
    "    \"notes\": {",
    "      \"command\": \"/usr/local/bin/notes-mcp\",",
    "      \"args\": [\"--root\", \"~/notes\"]",
    "    },",
    "    \"remote\": { \"url\": \"https://example.com/mcp\" }",
    "  }",
    "}",
];

/// The editor column: the header with its actions, then the home or a
/// server's details, scrolled by the wheel and cut at the page's edges.
pub fn draw(
    panel: &mut Panel,
    atlas: &mut Atlas,
    rect: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    let start = out.len();
    let body_top = draw_page(panel, atlas, rect, theme, out);
    let bottom = rect.y + rect.height;
    for quad in &mut out[start..] {
        layout::clip_vertical(quad, rect.y, bottom);
    }
    // The body scrolls under the header: what runs into it is cut there,
    // and a row cut at either edge cannot be clicked.
    for quad in &mut out[body_top.1..] {
        layout::clip_vertical(quad, body_top.0, bottom);
    }
    let top = body_top.0;
    panel
        .page_hits
        .retain(|(r, _)| r.y + r.height <= bottom && (r.y + r.height <= top || r.y >= top));
}

/// Draws everything; answers where the body starts, in points and as the
/// index of its first quad.
fn draw_page(
    panel: &mut Panel,
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
    panel.page_rect = None;
    if rect.width < 320.0 || rect.height < 200.0 {
        panel.page_hits = hits;
        return (rect.y, out.len());
    }
    let dim = theme.status_text;
    let x = rect.x + PAD;
    let right = rect.x + rect.width - PAD;
    let measure = (right - x).min(MEASURE);
    let mut y = rect.y + 22.0;

    // The header: the title, and the actions that are about servers in
    // general, from the right, the least needed dropped first when the
    // column is narrow.
    let title_w = layout::push_title(
        out,
        atlas,
        x,
        y - 6.0,
        20.0,
        "MCP Servers",
        theme.text,
        260.0,
    );
    let mut bx = right;
    for (label, action, primary) in [
        ("Close", Action::Close, false),
        ("Add Server\u{2026}", Action::AddCommand, true),
        ("Add by URL\u{2026}", Action::AddUrl, false),
        ("Open mcp.json", Action::Edit, false),
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

    // What is going on: an error reading the file, the last action's
    // outcome, or the count.
    let running = panel
        .servers
        .iter()
        .filter(|s| matches!(s.status, Status::Ready | Status::Starting))
        .count();
    let starting = panel.servers.iter().any(|s| s.status == Status::Starting);
    let (status, colour) = match (&panel.error, &panel.note) {
        (Some(error), _) => (error.clone(), theme.diff_removed),
        (None, Some(note)) => (note.clone(), dim),
        (None, None) => (
            match (panel.servers.len(), running) {
                (0, _) => "No servers yet: add one, or open mcp.json.".to_owned(),
                (n, 0) => format!("{n} in mcp.json \u{b7} none running"),
                (n, r) => format!("{n} in mcp.json \u{b7} {r} running"),
            },
            dim,
        ),
    };
    let mut sx = x;
    if starting {
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
    text(out, atlas, sx, y, right - sx, &status, colour);
    y += 28.0;
    layout::push_rect(out, atlas, [x, y], [right - x, 1.0], theme.hairline);
    y += 12.0;

    // The body, scrolled.
    let body_top = y;
    let body_start = out.len();
    let bottom = rect.y + rect.height;
    panel.page_rect = Some(Viewport {
        x: rect.x,
        y: body_top,
        width: rect.width,
        height: (bottom - body_top).max(0.0),
    });
    let max_scroll = (panel.page_content - (bottom - body_top)).max(0.0);
    panel.page_scroll = panel.page_scroll.clamp(0.0, max_scroll);
    let mut y = body_top - panel.page_scroll;

    let selected = panel.selected.filter(|&i| i < panel.servers.len());
    match selected {
        None => y = draw_home(panel, atlas, theme, out, &mut hits, x, y, measure),
        Some(si) => y = draw_server(panel, si, atlas, theme, out, &mut hits, x, y, measure),
    }
    panel.page_content = y + panel.page_scroll - body_top + 12.0;
    panel.page_hits = hits;
    (body_top, body_start)
}

/// The home: what a server is, then a card per server, or how to get the
/// first one when there is none.
#[allow(clippy::too_many_arguments)]
fn draw_home(
    panel: &Panel,
    atlas: &mut Atlas,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
    hits: &mut Vec<(Viewport, Action)>,
    x: f32,
    mut y: f32,
    measure: f32,
) -> f32 {
    let dim = theme.status_text;
    for line in layout::wrap_words(atlas, WHAT, measure) {
        text(out, atlas, x, y, measure, &line, dim);
        y += 20.0;
    }
    y += 16.0;

    if panel.servers.is_empty() {
        // Getting the first one: the three ways in, then the file's shape
        // so the one to type is known before the file opens.
        let steps = [
            "Add Server\u{2026} picks an installed program: its path goes in, nothing is downloaded.",
            "Add by URL\u{2026} connects to a Streamable HTTP server at an address you type.",
            "Open mcp.json adds arguments, environment variables or a working folder to an entry.",
        ];
        let mut lines = Vec::new();
        for (n, step) in steps.iter().enumerate() {
            for (i, line) in layout::wrap_words(atlas, step, measure - 52.0)
                .into_iter()
                .enumerate()
            {
                lines.push((if i == 0 { Some(n + 1) } else { None }, line));
            }
        }
        let card = Viewport {
            x,
            y,
            width: measure,
            height: 16.0 + lines.len() as f32 * 20.0 + 16.0,
        };
        layout::push_card(out, card, theme);
        let mut ly = y + 16.0;
        for (number, line) in &lines {
            if let Some(n) = number {
                text(
                    out,
                    atlas,
                    x + 16.0,
                    ly,
                    24.0,
                    &format!("{n}."),
                    theme.accent,
                );
            }
            text(out, atlas, x + 40.0, ly, measure - 56.0, line, theme.text);
            ly += 20.0;
        }
        y = card.y + card.height + 20.0;

        text(
            out,
            atlas,
            x,
            y,
            measure,
            "The file has the shape Claude Code's .mcp.json uses, so an entry can be pasted across:",
            dim,
        );
        y += 26.0;
        let code = Viewport {
            x,
            y,
            width: measure,
            height: 12.0 + EXAMPLE.len() as f32 * 20.0 + 12.0,
        };
        layout::push_card(out, code, theme);
        let mut ly = y + 12.0;
        for line in EXAMPLE {
            text(out, atlas, x + 16.0, ly, measure - 32.0, line, theme.text);
            ly += 20.0;
        }
        return code.y + code.height;
    }

    for (si, server) in panel.servers.iter().enumerate() {
        let card = Viewport {
            x,
            y,
            width: measure,
            height: CARD,
        };
        let open = panel.selected == Some(si);
        layout::push_card(out, card, theme);
        if open || layout::hovered(card) {
            layout::push_rounded_rect(out, card, layout::UI_RADIUS + 2.0, theme.row_hover);
        }
        let dot = Viewport {
            x: x + 18.0,
            y: y + 20.0,
            width: 10.0,
            height: 10.0,
        };
        layout::push_rounded_rect(out, dot, 5.0, status_colour(&server.status, theme));
        let nx = x + 40.0;
        let verb = verb_of(server);
        let verb_w = layout::ui_text_width(atlas, verb) + 28.0;
        let name_w = measure - 40.0 - verb_w - 28.0;
        layout::push_title(
            out,
            atlas,
            nx,
            y + 10.0,
            15.0,
            &server.name,
            theme.text,
            name_w,
        );
        text(out, atlas, nx, y + 32.0, name_w, &where_of(server), dim);
        let (line, colour) = summary_of(server, theme);
        text(out, atlas, nx, y + 52.0, name_w, &line, colour);
        // Start or Stop at the trailing edge; the rest of the card opens
        // the details.
        let r = button(
            out,
            atlas,
            theme,
            card.x + card.width - verb_w - 14.0,
            y + (CARD - layout::UI_CONTROL) * 0.5,
            verb,
            !server.is_running(),
        );
        hits.push((r, Action::Toggle(si)));
        hits.push((
            Viewport {
                width: card.width - verb_w - 14.0,
                ..card
            },
            Action::Select(si),
        ));
        y += CARD + 10.0;
    }
    y += 8.0;
    text(
        out,
        atlas,
        x,
        y,
        measure,
        "Arguments, environment variables and the working folder are edited in mcp.json.",
        dim,
    );
    y + 20.0
}

/// One server: its facts and actions, then everything it offers.
#[allow(clippy::too_many_arguments)]
fn draw_server(
    panel: &Panel,
    si: usize,
    atlas: &mut Atlas,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
    hits: &mut Vec<(Viewport, Action)>,
    x: f32,
    mut y: f32,
    measure: f32,
) -> f32 {
    let dim = theme.status_text;
    let server = &panel.servers[si];
    let r = link(out, atlas, theme, x, y, "\u{2039} MCP Servers");
    hits.push((r, Action::Home));
    y += 34.0;

    // A tile with the plug, the name large beside it, and the facts.
    let tile = Viewport {
        x,
        y,
        width: 52.0,
        height: 52.0,
    };
    layout::push_rounded_rect(out, tile, 12.0, theme.tab_hover);
    layout::push_icon_scaled(out, atlas, tile, PLUG, theme.accent, 1.9);
    let dot = Viewport {
        x: tile.x + tile.width - 14.0,
        y: tile.y + tile.height - 14.0,
        width: 10.0,
        height: 10.0,
    };
    layout::push_rounded_rect(out, dot, 5.0, status_colour(&server.status, theme));
    let nx = x + tile.width + 14.0;
    layout::push_title(
        out,
        atlas,
        nx,
        y + 2.0,
        20.0,
        &server.name,
        theme.text,
        measure - 66.0,
    );
    text(
        out,
        atlas,
        nx,
        y + 30.0,
        measure - 66.0,
        &where_of(server),
        dim,
    );
    y += tile.height + 18.0;

    let verb = verb_of(server);
    let r = button(out, atlas, theme, x, y, verb, !server.is_running());
    hits.push((r, Action::Toggle(si)));
    let r = button(
        out,
        atlas,
        theme,
        r.x + r.width + 10.0,
        y,
        "Edit in mcp.json",
        false,
    );
    hits.push((r, Action::Edit));
    y += layout::UI_CONTROL + 18.0;

    match &server.status {
        Status::Failed(why) => {
            for line in layout::wrap_words(atlas, &format!("Failed: {why}"), measure) {
                text(out, atlas, x, y, measure, &line, theme.diff_removed);
                y += 20.0;
            }
            return y;
        }
        Status::Stopped => {
            text(
                out,
                atlas,
                x,
                y,
                measure,
                "Stopped. Start it to see what it offers.",
                dim,
            );
            return y + 20.0;
        }
        Status::Starting => {
            let w = layout::push_spinner(
                out,
                x + 1.0,
                Viewport {
                    x,
                    y,
                    width: 20.0,
                    height: 20.0,
                },
                theme.accent,
            );
            text(out, atlas, x + w + 8.0, y, measure, "Starting\u{2026}", dim);
            return y + 20.0;
        }
        Status::Ready => {}
    }
    if !server.info.is_empty() {
        for line in layout::wrap_words(atlas, &server.info, measure) {
            text(out, atlas, x, y, measure, &line, dim);
            y += 20.0;
        }
        y += 8.0;
    }
    if server.tools.is_empty() && server.resources.is_empty() && server.prompts.is_empty() {
        text(
            out,
            atlas,
            x,
            y,
            measure,
            "This server offers nothing to call.",
            dim,
        );
        return y + 20.0;
    }

    // Each list: a heading with the count, then a row per item with its
    // description under the name. A click on a row does what the sidebar's
    // does: a call document, a read, a prompt.
    let heading = |out: &mut Vec<GlyphInstance>, atlas: &mut Atlas, y: f32, title: &str| {
        layout::push_title(out, atlas, x, y, 13.0, title, theme.text, measure);
    };
    if !server.tools.is_empty() {
        heading(out, atlas, y, &format!("Tools  {}", server.tools.len()));
        y += 28.0;
        for (ti, tool) in server.tools.iter().enumerate() {
            let flag = if tool.read_only {
                "read-only"
            } else if tool.destructive {
                "changes things: asks first"
            } else {
                ""
            };
            y = item_row(
                out,
                atlas,
                theme,
                hits,
                x,
                y,
                measure,
                icons::SYMBOL_METHOD,
                tool.title.as_deref().unwrap_or(&tool.name),
                flag,
                &tool.description,
                Action::Tool(si, ti),
            );
        }
        y += 10.0;
    }
    if !server.resources.is_empty() {
        heading(
            out,
            atlas,
            y,
            &format!("Resources  {}", server.resources.len()),
        );
        y += 28.0;
        for (ri, resource) in server.resources.iter().enumerate() {
            let name = if resource.name.is_empty() {
                resource.uri.as_str()
            } else {
                resource.name.as_str()
            };
            let detail = if resource.name.is_empty() {
                String::new()
            } else {
                resource.uri.clone()
            };
            y = item_row(
                out,
                atlas,
                theme,
                hits,
                x,
                y,
                measure,
                icons::FILE,
                name,
                resource.mime_type.as_deref().unwrap_or(""),
                &detail,
                Action::Resource(si, ri),
            );
        }
        y += 10.0;
    }
    if !server.prompts.is_empty() {
        heading(out, atlas, y, &format!("Prompts  {}", server.prompts.len()));
        y += 28.0;
        for (pi, prompt) in server.prompts.iter().enumerate() {
            y = item_row(
                out,
                atlas,
                theme,
                hits,
                x,
                y,
                measure,
                icons::SYMBOL_TEXT,
                &prompt.name,
                "",
                &prompt.description,
                Action::Prompt(si, pi),
            );
        }
    }
    y
}

/// A clickable row: icon, name, a flag after it, and a description line
/// under it when there is one. Answers where the next row starts.
#[allow(clippy::too_many_arguments)]
fn item_row(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    theme: &Theme,
    hits: &mut Vec<(Viewport, Action)>,
    x: f32,
    y: f32,
    measure: f32,
    glyph: char,
    name: &str,
    flag: &str,
    description: &str,
    action: Action,
) -> f32 {
    let description = description.lines().next().unwrap_or("").trim();
    let height = if description.is_empty() {
        ROW
    } else {
        ROW + 18.0
    };
    let row = Viewport {
        x: x - 8.0,
        y,
        width: measure + 16.0,
        height,
    };
    if layout::hovered(row) {
        layout::push_rounded_rect(out, row, layout::UI_RADIUS, theme.row_hover);
    }
    layout::push_icon_centered(
        out,
        atlas,
        Viewport {
            x,
            y,
            width: 20.0,
            height: ROW,
        },
        glyph,
        theme.status_text,
    );
    let nx = x + 28.0;
    let name_w = (layout::ui_text_width(atlas, name) + 4.0).min(measure * 0.6);
    text(out, atlas, nx, y + 3.0, name_w, name, theme.text);
    if !flag.is_empty() {
        text(
            out,
            atlas,
            nx + name_w + 10.0,
            y + 3.0,
            measure - 28.0 - name_w - 10.0,
            flag,
            theme.status_text,
        );
    }
    if !description.is_empty() {
        text(
            out,
            atlas,
            nx,
            y + ROW,
            measure - 28.0,
            description,
            theme.gutter_text,
        );
    }
    hits.push((row, action));
    y + height + 2.0
}

fn status_colour(status: &Status, theme: &Theme) -> [f32; 4] {
    match status {
        Status::Ready => theme.diff_added,
        Status::Starting => theme.diff_modified,
        Status::Failed(_) => theme.diff_removed,
        Status::Stopped => theme.gutter_text,
    }
}

fn verb_of(server: &Server) -> &'static str {
    if server.is_running() { "Stop" } else { "Start" }
}

/// The command line or the URL, as `mcp.json` has it.
fn where_of(server: &Server) -> String {
    let config = &server.config;
    match (&config.url, config.command.is_empty()) {
        (Some(url), true) => url.clone(),
        _ => {
            let mut line = config.command.clone();
            for arg in &config.args {
                line.push(' ');
                line.push_str(arg);
            }
            if line.is_empty() {
                "no command or url in its entry".into()
            } else {
                line
            }
        }
    }
}

/// The card's third line: the status, and what the server offers once it
/// is up, or why it is not.
fn summary_of(server: &Server, theme: &Theme) -> (String, [f32; 4]) {
    match &server.status {
        Status::Failed(why) => (format!("Failed: {why}"), theme.diff_removed),
        Status::Starting => ("Starting\u{2026}".into(), theme.status_text),
        Status::Stopped => ("Stopped".into(), theme.status_text),
        Status::Ready => {
            let mut parts = vec!["Running".to_owned()];
            for (n, what) in [
                (server.tools.len(), "tool"),
                (server.resources.len(), "resource"),
                (server.prompts.len(), "prompt"),
            ] {
                if n > 0 {
                    parts.push(format!("{n} {what}{}", if n == 1 { "" } else { "s" }));
                }
            }
            if parts.len() == 1 {
                parts.push("nothing to call".into());
            }
            (parts.join(" \u{b7} "), theme.status_text)
        }
    }
}

pub(crate) fn button(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    theme: &Theme,
    x: f32,
    y: f32,
    label: &str,
    primary: bool,
) -> Viewport {
    let rect = Viewport {
        x,
        y,
        width: layout::ui_text_width(atlas, label) + 28.0,
        height: layout::UI_CONTROL,
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

pub(crate) fn link(
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
        height: layout::UI_CONTROL,
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

pub(crate) fn text(
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
