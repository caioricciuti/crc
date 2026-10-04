//! The MCP sidebar: the servers in `mcp.json`, each started and stopped by
//! a click, and under a running one its tools, resources and prompts. A
//! click on one of those opens its call document. The page in the editor
//! column (`mcp_page`) shows the same servers with room to explain them.

use crate::mcp_client::{Config, Server, Status};
use crate::project::icons;
use crate::render::{
    font::Atlas,
    layout::{self, Theme, Viewport},
    metal::GlyphInstance,
};

/// Height of one row of the list.
pub const ROW: f32 = 26.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Start a stopped or failed server, stop a running one.
    Toggle(usize),
    Tool(usize, usize),
    Resource(usize, usize),
    Prompt(usize, usize),
    /// Open `mcp.json`, written first when it does not exist.
    Edit,
    /// Read `mcp.json` again.
    Reload,
    /// Show a server's details on the page.
    Select(usize),
    /// Back to the page's home: every server.
    Home,
    /// Give the editor column back; the list stays in the sidebar.
    Close,
    /// Pick an installed program with the Open panel and add it.
    AddCommand,
    /// Type a Streamable HTTP server's URL in the palette and add it.
    AddUrl,
}

#[derive(Default)]
pub struct Panel {
    pub servers: Vec<Server>,
    /// Why `mcp.json` could not be read.
    pub error: Option<String>,
    /// Whether the sidebar shows this panel.
    pub open: bool,
    /// The first row shown.
    pub scroll: usize,
    /// Rows and buttons as last drawn, with what a click on each does.
    pub hits: Vec<(Viewport, Action)>,
    /// Whether `mcp.json` has been read since the app started.
    pub loaded: bool,
    /// Whether the page has the editor column. Escape, Close and a tab
    /// give the column back; the list stays in the sidebar.
    pub details: bool,
    /// The server whose details the page shows; the home otherwise.
    pub selected: Option<usize>,
    /// What the last action came to, on the page.
    pub note: Option<String>,
    /// The page's buttons and rows as last drawn.
    pub page_hits: Vec<(Viewport, Action)>,
    /// How far the page's body is scrolled, in points.
    pub page_scroll: f32,
    /// Where the page's body was drawn, for the wheel.
    pub page_rect: Option<Viewport>,
    /// How tall the body was last frame, which bounds the scroll.
    pub page_content: f32,
}

/// One line of the list.
enum Line {
    Server(usize),
    Note(String),
    Heading(String),
    Item(Action, char, String, String),
}

impl Panel {
    /// Reads `mcp.json` again. A server whose entry is unchanged keeps
    /// running; one whose entry changed or went is stopped.
    pub fn reload(&mut self) {
        let config = Config::load();
        self.error = config.error;
        let selected = self
            .selected
            .and_then(|i| self.servers.get(i))
            .map(|s| s.name.clone());
        let mut old = std::mem::take(&mut self.servers);
        for (name, entry) in config.servers {
            match old.iter().position(|s| s.name == name && s.config == entry) {
                Some(i) => self.servers.push(old.remove(i)),
                None => self.servers.push(Server::new(&name, entry)),
            }
        }
        // What is left in `old` is dropped, which stops it.
        self.loaded = true;
        self.scroll = 0;
        self.selected = selected.and_then(|name| self.find(&name));
    }

    /// Scrolls the page's body by `points`, within what was drawn.
    pub fn scroll_page(&mut self, points: f32) {
        let visible = self.page_rect.map_or(0.0, |r| r.height);
        let max = (self.page_content - visible).max(0.0);
        self.page_scroll = (self.page_scroll + points).clamp(0.0, max);
    }

    /// Whether a server waits on an answer.
    pub fn busy(&self) -> bool {
        self.servers.iter().any(Server::busy)
    }

    pub fn find(&self, name: &str) -> Option<usize> {
        self.servers.iter().position(|s| s.name == name)
    }

    fn lines(&self) -> Vec<Line> {
        let mut lines = Vec::new();
        for (si, server) in self.servers.iter().enumerate() {
            lines.push(Line::Server(si));
            match &server.status {
                Status::Failed(why) => lines.push(Line::Note(why.clone())),
                Status::Ready => {
                    if !server.info.is_empty() {
                        lines.push(Line::Note(server.info.clone()));
                    }
                    if !server.tools.is_empty() {
                        lines.push(Line::Heading(format!("Tools {}", server.tools.len())));
                    }
                    for (ti, tool) in server.tools.iter().enumerate() {
                        let detail = if tool.read_only {
                            "read-only"
                        } else if tool.destructive {
                            "changes things"
                        } else {
                            ""
                        };
                        lines.push(Line::Item(
                            Action::Tool(si, ti),
                            crate::project::icons::SYMBOL_METHOD,
                            tool.title.clone().unwrap_or_else(|| tool.name.clone()),
                            detail.into(),
                        ));
                    }
                    if !server.resources.is_empty() {
                        lines.push(Line::Heading(format!(
                            "Resources {}",
                            server.resources.len()
                        )));
                    }
                    for (ri, resource) in server.resources.iter().enumerate() {
                        let name = if resource.name.is_empty() {
                            resource.uri.clone()
                        } else {
                            resource.name.clone()
                        };
                        lines.push(Line::Item(
                            Action::Resource(si, ri),
                            crate::project::icons::FILE,
                            name,
                            resource.mime_type.clone().unwrap_or_default(),
                        ));
                    }
                    if !server.prompts.is_empty() {
                        lines.push(Line::Heading(format!("Prompts {}", server.prompts.len())));
                    }
                    for (pi, prompt) in server.prompts.iter().enumerate() {
                        lines.push(Line::Item(
                            Action::Prompt(si, pi),
                            crate::project::icons::SYMBOL_TEXT,
                            prompt.name.clone(),
                            String::new(),
                        ));
                    }
                }
                _ => {}
            }
        }
        lines
    }

    /// How many rows fit below the header.
    fn visible(column: Viewport) -> usize {
        ((column.height - layout::SIDEBAR_HEADER_HEIGHT).max(0.0) / ROW).floor() as usize
    }

    pub fn scroll_by(&mut self, lines: isize, column: Viewport) {
        let total = self.lines().len();
        let max = total.saturating_sub(Panel::visible(column));
        self.scroll = (self.scroll as isize + lines).clamp(0, max as isize) as usize;
    }

    /// The header's buttons: Edit and Reload, in the title row where the
    /// Explorer keeps its own.
    fn buttons(column: Viewport) -> [Viewport; 2] {
        layout::sidebar_header_buttons::<2>(column)
    }

    pub fn draw(
        &mut self,
        atlas: &mut Atlas,
        column: Viewport,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) {
        self.hits.clear();
        layout::push_rect(
            out,
            atlas,
            [column.x, column.y],
            [column.width, column.height],
            theme.sidebar_background,
        );
        let x = column.x + 10.0;
        let width = (column.width - 20.0).max(0.0);
        layout::push_sidebar_title(out, atlas, column, "MCP SERVERS", 2, theme);
        let [edit, reload] = Panel::buttons(column);
        for (rect, icon, tip, action) in [
            (edit, icons::EDIT, "Edit mcp.json", Action::Edit),
            (reload, icons::REFRESH, "Reload Servers", Action::Reload),
        ] {
            layout::Button::new(rect)
                .icon(icon)
                .tone(layout::Tone::Ghost)
                .tip(tip)
                .draw(out, atlas, theme);
            self.hits.push((rect, action));
        }
        let top = column.y + layout::SIDEBAR_HEADER_HEIGHT;
        let say = |out: &mut Vec<GlyphInstance>, atlas: &mut Atlas, y: f32, text: &str| {
            layout::push_ui_text(
                out,
                atlas,
                Viewport {
                    x: x + 2.0,
                    y: y + 4.0,
                    width,
                    height: 20.0,
                },
                text,
                theme.gutter_text,
            );
        };
        if let Some(error) = &self.error {
            say(out, atlas, top, error);
            return;
        }
        if self.servers.is_empty() {
            say(out, atlas, top, "No servers yet.");
            say(
                out,
                atlas,
                top + ROW,
                "Add them in mcp.json: the pencil above opens it.",
            );
            return;
        }
        let lines = self.lines();
        let rows = Panel::visible(column);
        self.scroll = self.scroll.min(lines.len().saturating_sub(rows));
        for (n, line) in lines.iter().skip(self.scroll).take(rows).enumerate() {
            let y = top + n as f32 * ROW;
            let row = Viewport {
                x: column.x + 4.0,
                y,
                width: (column.width - 8.0).max(0.0),
                height: ROW,
            };
            match line {
                Line::Server(si) => {
                    let server = &self.servers[*si];
                    if self.details && self.selected == Some(*si) {
                        layout::push_rect(
                            out,
                            atlas,
                            [row.x, row.y],
                            [row.width, row.height],
                            theme.sidebar_selected,
                        );
                    }
                    let dot = match server.status {
                        Status::Ready => theme.diff_added,
                        Status::Starting => theme.diff_modified,
                        Status::Failed(_) => theme.diff_removed,
                        Status::Stopped => theme.gutter_text,
                    };
                    layout::push_rounded_rect(
                        out,
                        Viewport {
                            x: x + 2.0,
                            y: y + 9.0,
                            width: 8.0,
                            height: 8.0,
                        },
                        4.0,
                        dot,
                    );
                    layout::push_ui_text(
                        out,
                        atlas,
                        Viewport {
                            x: x + 18.0,
                            y: y + 4.0,
                            width: (width - 18.0 - 52.0).max(0.0),
                            height: 20.0,
                        },
                        &server.name,
                        theme.text,
                    );
                    let verb = match server.status {
                        Status::Ready | Status::Starting => "Stop",
                        _ => "Start",
                    };
                    let verb_rect = Viewport {
                        x: x + width - 52.0,
                        y,
                        width: 52.0,
                        height: ROW,
                    };
                    layout::push_ui_text_right(
                        out,
                        atlas,
                        Viewport {
                            y: y + 4.0,
                            width: 48.0,
                            height: 20.0,
                            ..verb_rect
                        },
                        verb,
                        theme.accent,
                    );
                    // The verb starts or stops; the rest of the row shows
                    // the server on the page without starting anything.
                    self.hits.push((verb_rect, Action::Toggle(*si)));
                    self.hits.push((
                        Viewport {
                            width: (row.width - verb_rect.width).max(0.0),
                            ..row
                        },
                        Action::Select(*si),
                    ));
                }
                Line::Note(text) => {
                    layout::push_ui_text(
                        out,
                        atlas,
                        Viewport {
                            x: x + 18.0,
                            y: y + 4.0,
                            width: (width - 18.0).max(0.0),
                            height: 20.0,
                        },
                        text,
                        theme.gutter_text,
                    );
                }
                Line::Heading(text) => {
                    layout::push_ui_text(
                        out,
                        atlas,
                        Viewport {
                            x: x + 18.0,
                            y: y + 6.0,
                            width: (width - 18.0).max(0.0),
                            height: 20.0,
                        },
                        text,
                        theme.status_text,
                    );
                }
                Line::Item(action, glyph, name, detail) => {
                    layout::push_icon_centered(
                        out,
                        atlas,
                        Viewport {
                            x: x + 18.0,
                            y,
                            width: 18.0,
                            height: ROW,
                        },
                        *glyph,
                        theme.status_text,
                    );
                    let name_w = (layout::ui_text_width(atlas, name) + 4.0).min(width * 0.65);
                    layout::push_ui_text(
                        out,
                        atlas,
                        Viewport {
                            x: x + 42.0,
                            y: y + 4.0,
                            width: name_w,
                            height: 20.0,
                        },
                        name,
                        theme.text,
                    );
                    layout::push_ui_text(
                        out,
                        atlas,
                        Viewport {
                            x: x + 42.0 + name_w + 8.0,
                            y: y + 4.0,
                            width: (width - 42.0 - name_w - 8.0).max(0.0),
                            height: 20.0,
                        },
                        detail,
                        theme.gutter_text,
                    );
                    self.hits.push((row, action.clone()));
                }
            }
        }
    }

    /// What a click does: on the page while it has the column, or in the
    /// sidebar. The two never overlap, so the order is only a tie-break.
    pub fn hit(&self, x: f32, y: f32) -> Option<Action> {
        self.page_hits
            .iter()
            .filter(|_| self.details)
            .chain(self.hits.iter())
            .find(|(rect, _)| rect.contains(x, y))
            .map(|(_, action)| action.clone())
    }

    /// A region for scripts: `mcp.edit`, `mcp.reload`, `mcp.server.NAME`
    /// (its Start/Stop), `mcp.select.NAME`, `mcp.tool.SERVER.TOOL`,
    /// `mcp.resource.SERVER.N`, `mcp.prompt.SERVER.NAME`; on the page
    /// `mcp.page.add`, `mcp.page.add-url`, `mcp.page.close`,
    /// `mcp.page.home` and the same server, tool, resource and prompt
    /// names with a `page.` in front (`mcp.page.server.NAME`).
    pub fn named(&self, name: &str) -> Option<Viewport> {
        let page = name
            .strip_prefix("mcp.page.")
            .map(|rest| format!("mcp.{rest}"));
        match page {
            Some(name) => self
                .page_hits
                .iter()
                .filter(|_| self.details)
                .find(|(_, action)| self.name_of(action) == name)
                .map(|(rect, _)| *rect),
            None => self
                .hits
                .iter()
                .find(|(_, action)| self.name_of(action) == name)
                .map(|(rect, _)| *rect),
        }
    }

    pub(crate) fn name_of(&self, action: &Action) -> String {
        let server = |si: usize| self.servers.get(si).map_or("", |s| s.name.as_str());
        match action {
            Action::Edit => "mcp.edit".into(),
            Action::Reload => "mcp.reload".into(),
            Action::Home => "mcp.home".into(),
            Action::Close => "mcp.close".into(),
            Action::AddCommand => "mcp.add".into(),
            Action::AddUrl => "mcp.add-url".into(),
            Action::Select(si) => format!("mcp.select.{}", server(*si)),
            Action::Toggle(si) => format!("mcp.server.{}", server(*si)),
            Action::Tool(si, ti) => format!(
                "mcp.tool.{}.{}",
                server(*si),
                self.servers[*si]
                    .tools
                    .get(*ti)
                    .map_or("", |t| t.name.as_str())
            ),
            Action::Resource(si, ri) => format!("mcp.resource.{}.{ri}", server(*si)),
            Action::Prompt(si, pi) => format!(
                "mcp.prompt.{}.{}",
                server(*si),
                self.servers[*si]
                    .prompts
                    .get(*pi)
                    .map_or("", |p| p.name.as_str())
            ),
        }
    }

    /// One line per server for the self-test: `name:status:tools`.
    pub fn report(&self) -> String {
        self.servers
            .iter()
            .map(|s| format!("{}:{}:{}", s.name, s.status.label(), s.tools.len()))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// The page for the self-test: whether it has the column, which
    /// server it shows, its note and how many targets it drew.
    pub fn page_report(&self) -> String {
        format!(
            "{} selected={} note={} targets={}",
            if self.details { "open" } else { "closed" },
            self.selected
                .and_then(|i| self.servers.get(i))
                .map_or("-", |s| s.name.as_str()),
            self.note.as_deref().unwrap_or("-"),
            self.page_hits.len()
        )
    }
}
