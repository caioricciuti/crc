//! The MCP sidebar: the servers in `mcp.json`, each started and stopped by
//! a click, and under a running one its tools, resources and prompts. A
//! click on one of those opens its call document.

use crate::mcp_client::{Config, Server, Status};
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

    /// The header's buttons: Edit and Reload, right-aligned in the row the
    /// Explorer uses for its own.
    fn buttons(column: Viewport) -> [Viewport; 2] {
        let y = column.y + 38.0;
        let right = column.x + column.width - 10.0;
        [
            Viewport {
                x: right - 64.0 - 4.0 - 64.0,
                y,
                width: 64.0,
                height: 26.0,
            },
            Viewport {
                x: right - 64.0,
                y,
                width: 64.0,
                height: 26.0,
            },
        ]
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
        layout::push_ui_text(
            out,
            atlas,
            Viewport {
                x: x + 2.0,
                y: column.y + 11.0,
                width,
                height: 20.0,
            },
            "MCP SERVERS",
            theme.status_text,
        );
        let [edit, reload] = Panel::buttons(column);
        for (rect, label, action) in [
            (edit, "Edit", Action::Edit),
            (reload, "Reload", Action::Reload),
        ] {
            layout::push_rounded_rect(out, rect, 5.0, theme.tab_hover);
            layout::push_ui_text_centered(out, atlas, rect, label, theme.status_text);
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
            say(out, atlas, top + ROW, "Edit adds them to mcp.json.");
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
                    layout::push_ui_text_right(
                        out,
                        atlas,
                        Viewport {
                            x: x + width - 52.0,
                            y: y + 4.0,
                            width: 48.0,
                            height: 20.0,
                        },
                        verb,
                        theme.accent,
                    );
                    self.hits.push((row, Action::Toggle(*si)));
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

    pub fn hit(&self, x: f32, y: f32) -> Option<Action> {
        self.hits
            .iter()
            .find(|(rect, _)| rect.contains(x, y))
            .map(|(_, action)| action.clone())
    }

    /// A region for scripts: `mcp.edit`, `mcp.reload`, `mcp.server.NAME`,
    /// `mcp.tool.SERVER.TOOL`, `mcp.resource.SERVER.N`, `mcp.prompt.SERVER.NAME`.
    pub fn named(&self, name: &str) -> Option<Viewport> {
        self.hits
            .iter()
            .find(|(_, action)| self.name_of(action) == name)
            .map(|(rect, _)| *rect)
    }

    fn name_of(&self, action: &Action) -> String {
        let server = |si: usize| self.servers.get(si).map_or("", |s| s.name.as_str());
        match action {
            Action::Edit => "mcp.edit".into(),
            Action::Reload => "mcp.reload".into(),
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
}
