//! The window's frame around the text: toolbar, tabs, sidebar, find bar
//! geometry, palette, home screen, scrollbar, breadcrumbs, response strip,
//! and the hit targets they leave behind.

use super::*;

/// Chrome heights in points. Deliberately fixed rather than multiples of
/// `line_height`: a control's size is a property of the platform, not of the
/// font the user picked. Tie them to the font and a 16pt setting grows the
/// tab bar by 23%, which no Mac app does.
/// The strip is as tall as a tab needs and no taller. At 40pt it cost more
/// vertical space than the breadcrumb and status rows put together.
pub const TAB_BAR_HEIGHT: f32 = 30.0;
pub const TOOLBAR_HEIGHT: f32 = 48.0;
pub const STATUS_HEIGHT: f32 = 28.0;
pub const BREADCRUMB_HEIGHT: f32 = 30.0;
/// The panel's title, with its actions at the right of the same row. Every
/// sidebar panel starts its content this far down.
pub const SIDEBAR_HEADER_HEIGHT: f32 = 40.0;
pub const SIDEBAR_ROW_HEIGHT: f32 = 26.0;
pub const FIND_ROW_HEIGHT: f32 = 34.0;

/// Every rectangle in the find bar, for drawing and for hit testing.
///
/// These were two sets of hand-written numbers: the drawing put a button at
/// `right - 180.0` and the click handler decided what had been pressed with
/// its own `right - 180.0`, its own `66.0`-point option slots and its own row
/// arithmetic. They were already only approximately the same, and the option
/// chips could be toggled by clicking beside them. One description, used by
/// both, is the rule in this codebase for a reason.
#[derive(Clone, Copy)]
pub struct FindGeometry {
    pub find_field: Viewport,
    pub replace_field: Viewport,
    /// Aa, Word, .* and Project, in that order.
    pub options: [Viewport; 4],
    pub previous: Viewport,
    pub next: Viewport,
    pub close: Viewport,
    /// "Replace"/"Search" and "All"/"Open", which is what the second row's
    /// buttons do in document and project mode respectively.
    pub replace_one: Viewport,
    pub replace_all: Viewport,
    pub results: Viewport,
}

impl FindGeometry {
    pub fn new(rect: Viewport) -> Self {
        Self::with_chips(rect, 4)
    }

    /// The bar with only the first `chips` option chips: the terminal's
    /// has no Project. The others are left with no width, so nothing hits
    /// them, and the field takes their room.
    pub fn with_chips(rect: Viewport, chips: usize) -> Self {
        const PAD: f32 = 12.0;
        const GAP: f32 = 6.0;
        const CHIP: f32 = 46.0;
        const NAV: f32 = 30.0;
        let row = |index: f32| Viewport {
            x: rect.x,
            y: rect.y + index * FIND_ROW_HEIGHT,
            width: rect.width,
            height: FIND_ROW_HEIGHT,
        };
        let inset = |r: Viewport, height: f32| Viewport {
            y: r.y + (FIND_ROW_HEIGHT - height) * 0.5,
            height,
            ..r
        };
        let control = 26.0;
        let right = rect.x + rect.width - PAD;
        // Right to left: close, the two nav buttons, then the option chips.
        let close = inset(
            Viewport {
                x: right - NAV,
                width: NAV,
                ..row(0.0)
            },
            control,
        );
        let next = inset(
            Viewport {
                x: close.x - GAP - NAV,
                width: NAV,
                ..row(0.0)
            },
            control,
        );
        let previous = inset(
            Viewport {
                x: next.x - GAP - NAV,
                width: NAV,
                ..row(0.0)
            },
            control,
        );
        let chips_right = previous.x - PAD;
        let shown = chips.min(4) as f32;
        let mut options = [close; 4];
        for (index, slot) in options.iter_mut().enumerate() {
            let at = index as f32;
            *slot = if at < shown {
                let x = chips_right - CHIP * (shown - at) - GAP * (shown - 1.0 - at);
                inset(
                    Viewport {
                        x,
                        width: CHIP,
                        ..row(0.0)
                    },
                    control,
                )
            } else {
                Viewport {
                    x: chips_right,
                    width: 0.0,
                    ..inset(row(0.0), control)
                }
            };
        }
        let field_width = (options[0].x - PAD - (rect.x + PAD)).max(0.0);
        let find_field = inset(
            Viewport {
                x: rect.x + PAD,
                width: field_width,
                ..row(0.0)
            },
            control,
        );
        // The second row's buttons sit under the chips, so the two fields keep
        // the same width and the eye has one left edge to follow.
        let replace_all = inset(
            Viewport {
                x: options[2].x,
                width: options[3].x + options[3].width - options[2].x,
                ..row(1.0)
            },
            control,
        );
        let replace_one = inset(
            Viewport {
                x: options[0].x,
                width: options[1].x + options[1].width - options[0].x,
                ..row(1.0)
            },
            control,
        );
        Self {
            find_field,
            replace_field: inset(
                Viewport {
                    x: find_field.x,
                    width: field_width,
                    ..row(1.0)
                },
                control,
            ),
            options,
            previous,
            next,
            close,
            replace_one,
            replace_all,
            results: Viewport {
                x: rect.x,
                y: rect.y + FIND_ROW_HEIGHT * 2.0,
                width: rect.width,
                height: (rect.height - FIND_ROW_HEIGHT * 2.0).max(0.0),
            },
        }
    }

    /// Which result row `y` falls on, if any.
    pub fn result_row(&self, y: f32) -> Option<usize> {
        (y >= self.results.y && y < self.results.y + self.results.height)
            .then(|| ((y - self.results.y) / FIND_ROW_HEIGHT) as usize)
    }
}
/// Air above the first line, so text is not glued to whatever is above it.
pub const TEXT_TOP_PAD: f32 = 8.0;
/// The response strip: one row of status and facts, one of segment buttons.
pub const RESPONSE_STRIP_HEIGHT: f32 = 72.0;
/// Narrowest the editor column may get before the sidebar gives way.
pub const EDITOR_MIN_WIDTH: f32 = 140.0;

/// Where everything in the window is.
///
/// There is exactly one of these per frame and everything reads it: the
/// renderer to draw, the mouse handlers to hit-test, the key handlers to know
/// how many rows fit. It exists because those used to work it out separately
/// and disagree. The renderer took the tab bar, the padding and the find bar
/// off the text area; the row count behind scrolling took off only the status
/// line, so the caret walked below the last drawn row and the end of a file
/// could not be scrolled into view; and clicks subtracted nothing at all, so
/// every one landed two rows low and a sidebar's width to the right.
#[derive(Clone, Debug)]
pub struct Chrome {
    pub toolbar: Viewport,
    /// The icon strip at the far left: Explorer, Source Control,
    /// Extensions. There whether or not the sidebar is.
    pub activity: Viewport,
    pub sidebar: Option<Viewport>,
    /// Across the top of the editor column.
    pub tabs: Viewport,
    pub breadcrumbs: Viewport,
    /// Under the tabs, one row for Find and two with Replace.
    pub find: Option<Viewport>,
    /// Above the text of a response tab: status, facts and the segment
    /// switch. `None` for every other document.
    pub response: Option<Viewport>,
    /// The terminal panel under the editor column, spanning every pane.
    pub terminal: Option<Viewport>,
    /// Agents mode's workspace inspector, at the right of the terminal.
    pub inspector: Option<Viewport>,
    /// The text itself: line numbers and lines, nothing else.
    pub text: Viewport,
    /// Across the bottom of the whole window.
    pub status: Viewport,
    /// The preview pane: the right half of the focused pane's text, when
    /// an extension's page is showing beside it.
    pub preview: Option<Viewport>,
    /// How many editor panes share the column.
    pub panes: usize,
    /// The panes that do not have the keyboard, by pane index, each with
    /// its own tab bar, breadcrumbs and text. The focused pane is the
    /// `tabs`, `breadcrumbs`, `find`, `response` and `text` above.
    pub others: Vec<(usize, PaneChrome)>,
}

/// The rectangles of an unfocused pane.
#[derive(Clone, Copy, Debug)]
pub struct PaneChrome {
    pub tabs: Viewport,
    pub breadcrumbs: Viewport,
    pub text: Viewport,
}

impl PaneChrome {
    /// Tabs through text, as one target.
    pub fn whole(&self) -> Viewport {
        Viewport {
            x: self.tabs.x,
            y: self.tabs.y,
            width: self.tabs.width,
            height: self.text.y + self.text.height - self.tabs.y,
        }
    }
}

/// Between two panes.
pub const PANE_GAP: f32 = 1.0;
/// The narrowest the text or the page beside it is dragged to.
pub const PREVIEW_MIN: f32 = 160.0;
/// Points on the page's side of the split that take the divider's drag
/// rather than the page: a native view over the text would take the press.
pub const PREVIEW_GRAB: f32 = 6.0;

impl Chrome {
    /// `sidebar` is its width when showing. `find_rows` is 0, 1 or 2.
    pub fn new(window: Viewport, sidebar: Option<f32>, find_rows: usize) -> Chrome {
        Chrome::with_response(window, sidebar, find_rows, false)
    }

    /// As [`Chrome::new`], with the response strip when the active document
    /// is a response tab.
    pub fn with_response(
        window: Viewport,
        sidebar: Option<f32>,
        find_rows: usize,
        response: bool,
    ) -> Chrome {
        Chrome::with_panes(window, sidebar, find_rows, response, 1, 0, None)
    }

    /// As [`Chrome::with_response`], with the editor column shared equally
    /// by `panes` panes, of which `focused` has the keyboard.
    pub fn with_panes(
        window: Viewport,
        sidebar: Option<f32>,
        find_rows: usize,
        response: bool,
        panes: usize,
        focused: usize,
        terminal: Option<f32>,
    ) -> Chrome {
        let status_height = STATUS_HEIGHT.min(window.height);
        let status = Viewport {
            x: window.x,
            y: window.y + window.height - status_height,
            width: window.width,
            height: status_height,
        };
        let body = Viewport {
            height: window.height - status_height,
            ..window
        };
        let (toolbar, body) = body.split_top(TOOLBAR_HEIGHT);
        let activity = body.take_left(ACTIVITY_WIDTH.min(body.width));
        let body = body.inset_left(activity.width);

        // A sidebar that would squeeze the editor to nothing is not shown.
        let sidebar = sidebar
            .filter(|width| body.width > width + EDITOR_MIN_WIDTH)
            .map(|width| body.take_left(width));
        let full_column = match sidebar {
            Some(rect) => body.inset_left(rect.width),
            None => body,
        };
        // The terminal takes the bottom of the column, leaving the editor
        // at least a tab bar, breadcrumbs and a few lines.
        let (full_column, terminal) = match terminal {
            Some(height) => {
                let keep = TAB_BAR_HEIGHT + BREADCRUMB_HEIGHT + 80.0;
                let height = height.min(full_column.height - keep).max(0.0);
                let (top, bottom) = full_column.split_top(full_column.height - height);
                (top, (height > 0.0).then_some(bottom))
            }
            None => (full_column, None),
        };
        let panes = panes.max(1);
        let focused = focused.min(panes - 1);
        let pane_width =
            ((full_column.width - PANE_GAP * (panes - 1) as f32) / panes as f32).max(0.0);
        let pane_column = |index: usize| Viewport {
            x: full_column.x + index as f32 * (pane_width + PANE_GAP),
            width: pane_width,
            ..full_column
        };
        let mut others = Vec::new();
        for index in (0..panes).filter(|i| *i != focused) {
            let (tabs, rest) = pane_column(index).split_top(TAB_BAR_HEIGHT);
            let (breadcrumbs, rest) = rest.split_top(BREADCRUMB_HEIGHT);
            let (_, text) = rest.split_top(TEXT_TOP_PAD);
            others.push((
                index,
                PaneChrome {
                    tabs,
                    breadcrumbs,
                    text,
                },
            ));
        }
        let column = pane_column(focused);

        let (tabs, rest) = column.split_top(TAB_BAR_HEIGHT);
        let (breadcrumbs, rest) = rest.split_top(BREADCRUMB_HEIGHT);
        let (find, rest) = match find_rows {
            0 => (None, rest),
            rows => {
                let (find, rest) = rest.split_top(FIND_ROW_HEIGHT * rows as f32);
                (Some(find), rest)
            }
        };
        let (response, rest) = if response {
            let (strip, rest) = rest.split_top(RESPONSE_STRIP_HEIGHT);
            (Some(strip), rest)
        } else {
            (None, rest)
        };
        let (_, text) = rest.split_top(TEXT_TOP_PAD);

        Chrome {
            toolbar,
            activity,
            sidebar,
            tabs,
            breadcrumbs,
            find,
            response,
            terminal,
            inspector: None,
            text,
            status,
            preview: None,
            panes,
            others,
        }
    }

    /// Gives the right half of the focused pane's text to the preview. The
    /// text keeps the left half, on a whole point.
    /// Gives the text `share` of the editor column's room and the page
    /// beside it the rest, each at least [`PREVIEW_MIN`] wide while the
    /// column has room for both. `only` gives the page the whole column.
    pub fn split_preview(&mut self, share: f32, only: bool) {
        if only {
            self.preview = Some(self.text);
            self.text.width = 0.0;
            return;
        }
        let room = (self.text.width - PANE_GAP).max(0.0);
        let least = PREVIEW_MIN.min(room / 2.0);
        let left = (room * share.clamp(0.0, 1.0))
            .floor()
            .clamp(least, (room - least).max(least));
        let preview = Viewport {
            x: self.text.x + left + PANE_GAP,
            width: (self.text.width - left - PANE_GAP).max(0.0),
            ..self.text
        };
        self.text.width = left;
        self.preview = Some(preview);
    }

    /// Agents mode: the terminal takes the editor column whole, and the
    /// activity strip goes, so the sidebar starts at the window's edge.
    /// The editor's rectangles are left with no height at the column's
    /// top, so nothing draws or answers a click there. `inspector` is the
    /// workspace inspector's width, taken from the column's right when the
    /// terminal keeps `terminal_min` beside it.
    pub fn terminal_only(mut self, inspector: Option<f32>, terminal_min: f32) -> Chrome {
        let strip = self.activity.width;
        self.activity.width = 0.0;
        if let Some(sidebar) = &mut self.sidebar {
            sidebar.x -= strip;
        }
        self.tabs.x -= strip;
        self.tabs.width += strip;
        let column = Viewport {
            height: self.status.y - self.tabs.y,
            ..self.tabs
        };
        let empty = Viewport {
            height: 0.0,
            ..self.tabs
        };
        self.tabs = empty;
        self.breadcrumbs = empty;
        self.text = empty;
        self.find = None;
        self.response = None;
        self.preview = None;
        self.others.clear();
        self.panes = 1;
        let mut column = column;
        if let Some(width) = inspector.filter(|w| column.width - w >= terminal_min) {
            column.width -= width;
            self.inspector = Some(Viewport {
                x: column.x + column.width,
                width,
                ..column
            });
        }
        self.terminal = Some(column);
        self
    }

    /// A point in the window, as a point relative to the text area, which is
    /// what [`offset_at_point`] takes.
    pub fn to_text(&self, x: f32, y: f32) -> (f32, f32) {
        (x - self.text.x, y - self.text.y)
    }
}

/// What a click on the home screen does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HomeAction {
    OpenFolder,
    NewFile,
    FindFile,
    OpenProject(std::path::PathBuf),
    /// Open these files, the last one shown.
    OpenFiles(Vec<std::path::PathBuf>),
    /// Show this repository in Source Control.
    ShowRepo(std::path::PathBuf),
    /// Open this saved MCP call and run it.
    RunCall(std::path::PathBuf),
    /// Show this terminal session, with the keyboard.
    ShowTerminal(usize),
    /// Install the downloaded update: Help > Restart to Update.
    RestartToUpdate,
    /// Open the app on disk again: Help > Restart crc.
    Restart,
}

/// A clickable row on the home screen.
#[derive(Clone, Debug)]
pub struct HomeHit {
    pub rect: Viewport,
    pub action: HomeAction,
}

/// The height of a Home row, and what one wheel line scrolls.
pub const HOME_ROW: f32 = 30.0;

/// How many recent folders Home lists: as many as the session keeps.
/// The column scrolls, so the pane does not bound the list.
pub const HOME_RECENT: usize = 8;

/// How far Home is scrolled, kept per window like the other pages'.
/// Home lays out everything it has and cuts to the pane; what is below
/// the pane is reached with the wheel, Page Up and Down, Home and End.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HomeView {
    /// How far the column is scrolled, in points.
    pub scroll: f32,
    /// Where it was drawn, for the wheel and the page keys.
    pub rect: Option<Viewport>,
    /// How tall the content was last frame, which bounds the scroll.
    pub content: f32,
}

impl HomeView {
    /// The most the column can scroll: what does not fit the pane.
    pub fn max_scroll(&self) -> f32 {
        let visible = self.rect.map_or(0.0, |r| r.height);
        (self.content - visible).max(0.0)
    }

    pub fn scroll_by(&mut self, points: f32) {
        self.scroll = (self.scroll + points).clamp(0.0, self.max_scroll());
    }

    /// Page Down (`pages` 1.0) or Page Up (-1.0): a pane's height less a
    /// row, so the last row seen is the first row then.
    pub fn page_by(&mut self, pages: f32) {
        let visible = self.rect.map_or(0.0, |r| r.height);
        self.scroll_by(pages * (visible - HOME_ROW).max(HOME_ROW));
    }

    pub fn to_top(&mut self) {
        self.scroll = 0.0;
    }

    pub fn to_end(&mut self) {
        self.scroll = self.max_scroll();
    }

    /// Whether the point is on the column as last drawn.
    pub fn over(&self, x: f32, y: f32) -> bool {
        self.rect.is_some_and(|r| r.contains(x, y))
    }

    /// For the self-test: how far it is scrolled, of how far it can be.
    pub fn report(&self) -> String {
        format!("{:.0} of {:.0}", self.scroll, self.max_scroll())
    }
}

/// The widest Home's column gets. Wider, and a row's detail drifts away
/// from its label and the shortcut on the right from both.
pub const HOME_MEASURE: f32 = 680.0;

/// Home's column in `rect`: its left edge and its width. Centred once the
/// pane is wider than the column; on a narrow pane it keeps a margin.
pub fn home_column(rect: Viewport) -> (f32, f32) {
    let width = (rect.width - 112.0)
        .clamp(240.0, HOME_MEASURE)
        .min((rect.width - 56.0).max(0.0));
    let x = rect.x + ((rect.width - width) / 2.0).max(28.0);
    (x, width)
}

/// A folder's path cut to fit: the home folder as `~`, then, when the
/// whole does not fit, the middle elided and the last two components
/// kept, since where a folder sits says more than where its disk starts.
/// `fits` says whether a candidate fits the room.
pub fn elide_path(path: &str, mut fits: impl FnMut(&str) -> bool) -> String {
    let path = shorten_home(path);
    if fits(&path) {
        return path;
    }
    let parts: Vec<&str> = path.split('/').collect();
    let n = parts.len();
    let tail = |keep: usize| parts[n - keep.min(n)..].join("/");
    let mut candidates = Vec::new();
    // The head is the root's first folder or `~`, kept when something
    // lies between it and the last two parts.
    let head = if parts[0].is_empty() { 2 } else { 1 };
    if n > head + 2 {
        candidates.push(format!("{}/\u{2026}/{}", parts[..head].join("/"), tail(2)));
    }
    if n > 2 {
        candidates.push(format!("\u{2026}/{}", tail(2)));
    }
    if n > 1 {
        candidates.push(format!("\u{2026}/{}", tail(1)));
    }
    candidates
        .into_iter()
        .find(|c| fits(c))
        .unwrap_or_else(|| tail(1))
}

/// The home screen: what the editor column shows with nothing open.
///
/// Drawn natively rather than as a Markdown document, so it can have rows
/// you click, the project's recent folders, and the UI font instead of a
/// monospace table. Appends to `out`; the caller clears and paints the
/// background first, as for every other panel. `update` is a downloaded
/// release's version; `replaced` says the app on disk was replaced under
/// this one, which a restart puts right. `hits` receives the rows.
///
/// Unscrolled: for a pane that keeps no [`HomeView`], such as the other
/// half of a split. The focused pane draws with [`build_home_view`].
#[allow(clippy::too_many_arguments)]
pub fn build_home(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    rect: Viewport,
    theme: &Theme,
    project: Option<&std::path::Path>,
    recent: &[std::path::PathBuf],
    summary: Option<&crate::project::workspace::Summary>,
    terminals: &[(String, String)],
    update: Option<&str>,
    replaced: bool,
    hits: &mut Vec<HomeHit>,
) {
    let mut view = HomeView::default();
    build_home_view(
        out, atlas, rect, theme, project, recent, summary, terminals, update, replaced, &mut view,
        hits,
    );
}

/// [`build_home`] scrolled by `view`, which learns where the column was
/// drawn and how tall its content is. Everything is laid out; what lies
/// outside `rect` is cut away, from the glyphs, the hits and the hotspots.
#[allow(clippy::too_many_arguments)]
pub fn build_home_view(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    rect: Viewport,
    theme: &Theme,
    project: Option<&std::path::Path>,
    recent: &[std::path::PathBuf],
    summary: Option<&crate::project::workspace::Summary>,
    terminals: &[(String, String)],
    update: Option<&str>,
    replaced: bool,
    view: &mut HomeView,
    hits: &mut Vec<HomeHit>,
) {
    const ROW: f32 = HOME_ROW;
    const GAP: f32 = 18.0;
    hits.clear();
    view.rect = None;
    if rect.width < 200.0 || rect.height < 120.0 {
        view.content = 0.0;
        view.scroll = 0.0;
        return;
    }
    view.rect = Some(rect);
    // Last frame's content bounds this frame's scroll, as on the pages.
    view.scroll_by(0.0);
    let start = out.len();
    let spots = super::ui::hotspots_len();
    let (x, width) = home_column(rect);
    let right = x + width;
    let top_pad = (rect.height * 0.06).clamp(24.0, 48.0);
    let mut y = rect.y + top_pad - view.scroll;
    // Nothing is left out for want of height: the column scrolls.
    let bottom = f32::INFINITY;
    let dim = theme.status_text;

    // Wordmark and the one-line situation report.
    push_prose(out, atlas, x, y, 26.0, Face::Bold, "crc", theme.text, right);
    y += 40.0;
    let line = |y: f32| Viewport {
        x,
        y,
        width,
        height: 20.0,
    };
    let situation = match project.and_then(|p| p.file_name()) {
        Some(name) => format!("Nothing is open in {}.", name.to_string_lossy()),
        None => "No project is open.".to_owned(),
    };
    push_ui_text(out, atlas, line(y), &situation, dim);
    y += 20.0 + GAP;

    let section = |out: &mut Vec<GlyphInstance>, atlas: &mut Atlas, y: &mut f32, title: &str| {
        push_ui_text(out, atlas, line(*y), title, dim);
        *y += 22.0;
        push_rect(out, atlas, [x, *y - 4.0], [width, 1.0], theme.hairline);
    };

    // A downloaded update waits for one click.
    if let Some(version) = update
        && y + 22.0 + ROW <= bottom
    {
        section(out, atlas, &mut y, "Update");
        let rect = home_row(
            out,
            atlas,
            theme,
            (x, width),
            y,
            icons::REFRESH,
            &format!("crc {version} is ready"),
            "Restart to Update",
        );
        hits.push(HomeHit {
            rect,
            action: HomeAction::RestartToUpdate,
        });
        y += ROW + GAP;
    }

    // Terminals first: a session waiting on you matters more than what
    // to open next, and the panel under Home leaves little height.
    // Waiting ones lead.
    if !terminals.is_empty() && y + 22.0 + ROW <= bottom {
        section(out, atlas, &mut y, "Terminals");
        let mut order: Vec<usize> = (0..terminals.len()).collect();
        order.sort_by_key(|&i| !terminals[i].1.starts_with("waiting"));
        for i in order {
            if y + ROW > bottom {
                break;
            }
            let (name, doing) = &terminals[i];
            let rect = home_row(
                out,
                atlas,
                theme,
                (x, width),
                y,
                icons::TERMINAL,
                name,
                doing,
            );
            if doing.starts_with("waiting") {
                push_rect(out, atlas, [x - 6.0, y + 12.0], [5.0, 5.0], theme.accent);
            }
            hits.push(HomeHit {
                rect,
                action: HomeAction::ShowTerminal(i),
            });
            y += ROW;
        }
        y += GAP;
    }

    // Start: the three things to do from here, each with its shortcut,
    // and the restart an install over the running app asks for.
    section(out, atlas, &mut y, "Start");
    let actions = [
        (
            icons::FOLDER,
            "Open Folder\u{2026}",
            "\u{2318}\u{21e7}O",
            HomeAction::OpenFolder,
        ),
        (
            icons::NEW_FILE,
            "New File",
            "\u{2318}N",
            HomeAction::NewFile,
        ),
        (
            icons::SEARCH,
            "Go to File",
            "\u{2318}P",
            HomeAction::FindFile,
        ),
    ];
    let restart = replaced.then_some((icons::REFRESH, "Restart crc", "", HomeAction::Restart));
    for (glyph, label, keys, action) in actions.into_iter().chain(restart) {
        if y + ROW > bottom {
            return;
        }
        let row = Viewport {
            x: x - 8.0,
            y,
            width: width + 16.0,
            height: ROW,
        };
        push_home_hover(out, row, theme);
        push_icon_centered(
            out,
            atlas,
            Viewport {
                x,
                y,
                width: 24.0,
                height: ROW,
            },
            glyph,
            theme.accent,
        );
        push_ui_text(
            out,
            atlas,
            Viewport {
                x: x + 32.0,
                y: y + 5.0,
                width: width - 120.0,
                height: 20.0,
            },
            label,
            theme.text,
        );
        push_ui_text_right(
            out,
            atlas,
            Viewport {
                x: right - 88.0,
                y: y + 5.0,
                width: 88.0,
                height: 20.0,
            },
            keys,
            dim,
        );
        hits.push(HomeHit { rect: row, action });
        y += ROW;
    }
    y += GAP;

    if let Some(summary) = summary {
        y = home_workspace(out, atlas, theme, (x, width), y, bottom, summary, hits);
    }

    // Recent: folders opened before, the name as the label and where it
    // is, cut to fit, dimmed after it. The whole path is the tooltip.
    let recent: Vec<&std::path::PathBuf> = recent
        .iter()
        .filter(|p| Some(p.as_path()) != project)
        .take(HOME_RECENT)
        .collect();
    if y + 22.0 + ROW <= bottom {
        section(out, atlas, &mut y, "Recent");
        if recent.is_empty() {
            // The small variant: under a heading, an icon would outweigh
            // the rows of the sections around it.
            let state =
                EmptyState::new("No recent folders", "Folders you open will be listed here.");
            let column = Viewport {
                x: x - UI_INSET,
                y,
                width: width + UI_INSET * 2.0,
                height: state.height(atlas, width + UI_INSET * 2.0),
            };
            push_empty_state(out, atlas, column, state, theme);
            y += column.height;
        }
        for path in recent {
            if y + ROW > bottom {
                break;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let full = path.display().to_string();
            let room = home_detail_width(atlas, width, &name);
            let parent = path
                .parent()
                .map(|p| {
                    elide_path(&p.display().to_string(), |s| {
                        ui_text_width(atlas, s) <= room
                    })
                })
                .unwrap_or_default();
            let rect = home_row_tipped(
                out,
                atlas,
                theme,
                (x, width),
                y,
                icons::FOLDER,
                &name,
                &parent,
                Some(&full),
            );
            hits.push(HomeHit {
                rect,
                action: HomeAction::OpenProject(path.clone()),
            });
            y += ROW;
        }
        y += GAP;
    }

    // Keys: the rest of the shortcuts, two columns, each key in a cap of
    // one width so they read as keys and the actions line up.
    let keys = [
        ("\u{2318}B", "Show or hide the sidebar"),
        ("\u{2318}\u{2325}G", "Source Control"),
        ("\u{2318}\u{21e7}F", "Find in project"),
        ("\u{2318}\u{21e7}C", "Claude Code"),
        ("\u{2318}O", "Open a file"),
        ("\u{2318}=", "Zoom in"),
    ];
    const KEY_ROW: f32 = 28.0;
    let rows_needed = keys.len().div_ceil(2) as f32 * KEY_ROW;
    if y + 22.0 + rows_needed + 30.0 <= bottom {
        section(out, atlas, &mut y, "Keys");
        let column = width / 2.0;
        let cap_w = keys
            .iter()
            .map(|(combo, _)| key_cap_width(atlas, combo))
            .fold(0.0, f32::max);
        for (i, (combo, what)) in keys.iter().enumerate() {
            let cx = x + (i % 2) as f32 * column;
            let cy = y + (i / 2) as f32 * KEY_ROW;
            let cap = Viewport {
                x: cx,
                y: cy + ((KEY_ROW - UI_CONTROL_SM) * 0.5).floor(),
                width: cap_w,
                height: UI_CONTROL_SM,
            };
            push_key_cap(out, atlas, cap, combo, theme.tab_active, theme);
            push_ui_text(
                out,
                atlas,
                Viewport {
                    x: cx + cap_w + UI_INSET,
                    y: cy + 4.0,
                    width: (column - cap_w - UI_INSET * 2.0).max(0.0),
                    height: 20.0,
                },
                what,
                theme.text,
            );
        }
        y += rows_needed + GAP;
    }

    if y + 20.0 <= bottom {
        push_ui_text(
            out,
            atlas,
            line(y),
            "Or start typing: this becomes an untitled document.",
            dim,
        );
        y += 20.0;
    }
    view.content = y + 16.0 - (rect.y - view.scroll);

    // Cut to the pane: a row scrolled out of it is neither seen, clicked
    // nor pointed at.
    let (top, bottom) = (rect.y, rect.y + rect.height);
    for quad in &mut out[start..] {
        clip_vertical(quad, top, bottom);
    }
    hits.retain_mut(|hit| {
        let y0 = hit.rect.y.max(top);
        let y1 = (hit.rect.y + hit.rect.height).min(bottom);
        hit.rect.y = y0;
        hit.rect.height = y1 - y0;
        y1 > y0
    });
    super::ui::clip_hotspots(spots, rect);
}

/// One clickable Home row: an icon, a label, and a dimmed detail after it.
/// Returns the row's rectangle.
/// A Home row under the pointer gets the list wash.
fn push_home_hover(out: &mut Vec<GlyphInstance>, row: Viewport, theme: &Theme) {
    if super::ui::hovered(row) {
        push_rounded_rect(out, row, UI_RADIUS, theme.row_hover);
    }
    hotspot(row, Cursor::Pointing, None);
}

/// The width a Home row's label takes: its text, capped so the detail
/// after it keeps some room.
fn home_label_width(atlas: &mut Atlas, width: f32, label: &str) -> f32 {
    (ui_text_width(atlas, label) + 4.0).min(width * 0.6)
}

/// The room a Home row's detail has after its label.
fn home_detail_width(atlas: &mut Atlas, width: f32, label: &str) -> f32 {
    (width - 32.0 - home_label_width(atlas, width, label) - 10.0).max(0.0)
}

#[allow(clippy::too_many_arguments)]
fn home_row(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    theme: &Theme,
    (x, width): (f32, f32),
    y: f32,
    glyph: char,
    label: &str,
    detail: &str,
) -> Viewport {
    home_row_tipped(out, atlas, theme, (x, width), y, glyph, label, detail, None)
}

/// [`home_row`] with a tooltip: the whole of what the detail cut short.
#[allow(clippy::too_many_arguments)]
fn home_row_tipped(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    theme: &Theme,
    (x, width): (f32, f32),
    y: f32,
    glyph: char,
    label: &str,
    detail: &str,
    tip: Option<&str>,
) -> Viewport {
    const ROW: f32 = 30.0;
    let whole = Viewport {
        x: x - 8.0,
        y,
        width: width + 16.0,
        height: ROW,
    };
    let over = super::ui::hovered(whole);
    if over {
        push_rounded_rect(out, whole, UI_RADIUS, theme.row_hover);
    }
    hotspot(whole, Cursor::Pointing, tip);
    push_icon_centered(
        out,
        atlas,
        Viewport {
            x,
            y,
            width: 24.0,
            height: ROW,
        },
        glyph,
        if over {
            theme.accent
        } else {
            theme.status_text
        },
    );
    let label_w = home_label_width(atlas, width, label);
    let detail_w = home_detail_width(atlas, width, label);
    push_ui_text(
        out,
        atlas,
        Viewport {
            x: x + 32.0,
            y: y + 5.0,
            width: label_w,
            height: 20.0,
        },
        label,
        theme.text,
    );
    push_ui_text(
        out,
        atlas,
        Viewport {
            x: x + 32.0 + label_w + 10.0,
            y: y + 5.0,
            width: detail_w,
            height: 20.0,
        },
        detail,
        theme.status_text,
    );
    Viewport {
        x: x - 8.0,
        y,
        width: width + 16.0,
        height: ROW,
    }
}

/// Home's workspace sections: what waits on you, the repositories, and
/// what changed since the last visit. Each is left out when empty or when
/// it no longer fits. Returns where the next section starts.
#[allow(clippy::too_many_arguments)]
fn home_workspace(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    theme: &Theme,
    (x, width): (f32, f32),
    mut y: f32,
    bottom: f32,
    summary: &crate::project::workspace::Summary,
    hits: &mut Vec<HomeHit>,
) -> f32 {
    const ROW: f32 = 30.0;
    const GAP: f32 = 18.0;
    let dim = theme.status_text;
    let section = |out: &mut Vec<GlyphInstance>, atlas: &mut Atlas, y: &mut f32, title: &str| {
        push_ui_text(
            out,
            atlas,
            Viewport {
                x,
                y: *y,
                width,
                height: 20.0,
            },
            title,
            dim,
        );
        *y += 22.0;
        push_rect(out, atlas, [x, *y - 4.0], [width, 1.0], theme.hairline);
    };
    let settings = &summary.settings;

    // The session: the state doc to start from, the log and state to end with.
    let mut session = Vec::new();
    if let Some(state) = &settings.state {
        session.push((
            icons::FILE,
            "Start Session",
            "read where things stand",
            HomeAction::OpenFiles(vec![state.clone()]),
        ));
        let mut end = Vec::new();
        end.extend(settings.log.clone());
        end.push(state.clone());
        session.push((
            icons::HISTORY,
            "End Session",
            if settings.log.is_some() {
                "update the log and the state"
            } else {
                "update the state"
            },
            HomeAction::OpenFiles(end),
        ));
    }
    if !session.is_empty() && y + 22.0 + ROW <= bottom {
        section(out, atlas, &mut y, "Session");
        for (glyph, label, detail, action) in session {
            if y + ROW > bottom {
                break;
            }
            let rect = home_row(out, atlas, theme, (x, width), y, glyph, label, detail);
            hits.push(HomeHit { rect, action });
            y += ROW;
        }
        y += GAP;
    }

    if !summary.waiting.is_empty()
        && let Some(state) = &settings.state
        && y + 22.0 + ROW <= bottom
    {
        section(out, atlas, &mut y, "Waiting on you");
        for item in &summary.waiting {
            if y + ROW > bottom {
                break;
            }
            push_rect(out, atlas, [x + 9.0, y + 13.0], [5.0, 5.0], theme.accent);
            push_ui_text(
                out,
                atlas,
                Viewport {
                    x: x + 32.0,
                    y: y + 5.0,
                    width: width - 32.0,
                    height: 20.0,
                },
                item,
                theme.text,
            );
            hits.push(HomeHit {
                rect: Viewport {
                    x: x - 8.0,
                    y,
                    width: width + 16.0,
                    height: ROW,
                },
                action: HomeAction::OpenFiles(vec![state.clone()]),
            });
            y += ROW;
        }
        y += GAP;
    }

    if !summary.repos.is_empty() && y + 22.0 + ROW <= bottom {
        section(out, atlas, &mut y, "Repositories");
        for repo in &summary.repos {
            if y + ROW > bottom {
                break;
            }
            let detail = match repo.changes {
                0 => repo.status.clone(),
                1 => format!("{}  ·  1 change", repo.status),
                n => format!("{}  ·  {n} changes", repo.status),
            };
            let full = repo.path.display().to_string();
            let rect = home_row_tipped(
                out,
                atlas,
                theme,
                (x, width),
                y,
                icons::FOLDER,
                &repo.label,
                &detail,
                Some(&full),
            );
            hits.push(HomeHit {
                rect,
                action: HomeAction::ShowRepo(repo.path.clone()),
            });
            y += ROW;
        }
        y += GAP;
    }

    if !summary.calls.is_empty() && y + 22.0 + ROW <= bottom {
        section(out, atlas, &mut y, "Saved calls");
        for saved in &summary.calls {
            if y + ROW > bottom {
                break;
            }
            let rect = home_row(
                out,
                atlas,
                theme,
                (x, width),
                y,
                icons::SYMBOL_METHOD,
                &saved.name,
                &saved.target,
            );
            hits.push(HomeHit {
                rect,
                action: HomeAction::RunCall(saved.path.clone()),
            });
            y += ROW;
        }
        y += GAP;
    }

    let committed: Vec<_> = summary
        .repos
        .iter()
        .filter(|r| !r.commits.is_empty())
        .collect();
    if summary.since.is_some()
        && (!committed.is_empty() || !summary.notes.is_empty())
        && y + 22.0 + ROW <= bottom
    {
        section(out, atlas, &mut y, "Since you were last here");
        for repo in committed {
            if y + ROW > bottom {
                break;
            }
            let detail = match repo.commits.len() {
                1 => format!("1 commit: {}", repo.commits[0]),
                n => format!("{n} commits, latest: {}", repo.commits[0]),
            };
            let rect = home_row(
                out,
                atlas,
                theme,
                (x, width),
                y,
                icons::HISTORY,
                &repo.label,
                &detail,
            );
            hits.push(HomeHit {
                rect,
                action: HomeAction::ShowRepo(repo.path.clone()),
            });
            y += ROW;
        }
        for note in &summary.notes {
            if y + ROW > bottom {
                break;
            }
            let label = note
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            let full = note.display().to_string();
            let room = home_detail_width(atlas, width, &label);
            let detail = note
                .parent()
                .map(|p| {
                    elide_path(&p.display().to_string(), |s| {
                        ui_text_width(atlas, s) <= room
                    })
                })
                .unwrap_or_default();
            let rect = home_row_tipped(
                out,
                atlas,
                theme,
                (x, width),
                y,
                icons::FILE,
                &label,
                &detail,
                Some(&full),
            );
            hits.push(HomeHit {
                rect,
                action: HomeAction::OpenFiles(vec![note.clone()]),
            });
            y += ROW;
        }
        y += GAP;
    }
    y
}

/// `/Users/name/...` as `~/...`, the way people read their own paths.
pub(super) fn shorten_home(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_owned(),
    }
}

/// Width of the editor's overlay scrollbar thumb, in points.
pub const SCROLLBAR_WIDTH: f32 = 5.0;
/// Air between the thumb and the pane's edges.
pub(super) const SCROLLBAR_INSET: f32 = 4.0;
/// A thumb never shrinks below this, or a long file has nothing to grab.
pub(super) const SCROLLBAR_MIN_THUMB: f32 = 24.0;

/// The vertical run the thumb moves along: the pane's height less an inset
/// at each end.
pub fn scrollbar_track(viewport: Viewport) -> Viewport {
    Viewport {
        x: viewport.x + viewport.width - SCROLLBAR_WIDTH - SCROLLBAR_INSET,
        y: viewport.y + SCROLLBAR_INSET,
        width: SCROLLBAR_WIDTH,
        height: (viewport.height - 2.0 * SCROLLBAR_INSET).max(0.0),
    }
}

/// Where the overlay scrollbar's thumb is for a document in `viewport`, or
/// `None` when the whole document fits and there is nothing to scroll.
///
/// One function for drawing and for hit testing, so the thumb you grab is
/// the thumb you see. The thumb's share of the track is the view's share
/// of the lines, and its position is the scroll's share of the way to the
/// last position, which is what makes the bottom of the thumb meet the
/// bottom of the track exactly when the last line is in view.
/// Files up to this many lines have their wrapped rows counted for the
/// scrollbar; past it, lines outnumber the view anyway and count as one.
pub(super) const COUNT_ROWS_UP_TO: usize = 4000;

/// What the scrollbar measures in, and where the view is in it: screen rows
/// for a small wrapped file (a line may be several), shown lines otherwise
/// (a folded line is none). With rows counted, the line each row is in, to
/// map a drag back to a line.
pub(super) fn scroll_extent(buffer: &Buffer) -> (usize, f32, Vec<usize>) {
    let total = buffer.rope.len_lines();
    if !buffer.row_mode() {
        let at = buffer.scroll_line as f32 + buffer.scroll_fraction;
        return (total, at, Vec::new());
    }
    if buffer.wrap.is_some() && total <= COUNT_ROWS_UP_TO {
        let mut units = Vec::new();
        let mut at = 0.0;
        for line in 0..total {
            if line == buffer.scroll_line {
                at = (units.len() + buffer.scroll_row) as f32 + buffer.scroll_fraction;
            }
            let rows = buffer.row_starts(line).len();
            units.extend(std::iter::repeat_n(line, rows));
        }
        return (units.len(), at, units);
    }
    // Lines, less the ones folds hide, from the fold list alone.
    let hidden_before = |line: usize| -> usize {
        buffer
            .folds
            .iter()
            .map(|&(a, b)| (b + 1).min(line).saturating_sub(a))
            .sum()
    };
    let shown = total - hidden_before(total);
    let at =
        (buffer.scroll_line - hidden_before(buffer.scroll_line)) as f32 + buffer.scroll_fraction;
    (shown, at, Vec::new())
}

/// The line at shown position `unit`: the inverse of [`scroll_extent`].
pub(super) fn line_at_unit(buffer: &Buffer, unit: usize, units: &[usize]) -> usize {
    if !units.is_empty() {
        return units
            .get(unit)
            .copied()
            .unwrap_or(buffer.rope.len_lines().saturating_sub(1));
    }
    let mut line = unit;
    for &(a, b) in &buffer.folds {
        if a <= line {
            line += b + 1 - a;
        }
    }
    line
}

pub fn scrollbar_thumb(buffer: &Buffer, viewport: Viewport, line_height: f32) -> Option<Viewport> {
    let rows = viewport.rows(line_height);
    let (total, at, _) = scroll_extent(buffer);
    if rows == 0 || total <= rows {
        return None;
    }
    let track = scrollbar_track(viewport);
    if track.height <= SCROLLBAR_MIN_THUMB {
        return None;
    }
    let height = (track.height * rows as f32 / total as f32).max(SCROLLBAR_MIN_THUMB);
    let max_scroll = (total - rows) as f32;
    let at = (at / max_scroll).clamp(0.0, 1.0);
    Some(Viewport {
        x: track.x,
        y: track.y + (track.height - height) * at,
        width: track.width,
        height,
    })
}

/// A match tick's height on the scrollbar's track.
pub const SCROLLBAR_TICK: f32 = 2.0;

/// Where on the track line `line` is, as the thumb measures it: the top
/// of a tick for a find match there, or `None` when the document fits
/// the view and there is no thumb to read it against.
pub fn scrollbar_tick_y(
    buffer: &Buffer,
    viewport: Viewport,
    line_height: f32,
    line: usize,
) -> Option<f32> {
    scrollbar_thumb(buffer, viewport, line_height)?;
    let (total, _, units) = scroll_extent(buffer);
    let unit = if !units.is_empty() {
        units.partition_point(|&l| l < line)
    } else {
        let hidden: usize = buffer
            .folds
            .iter()
            .map(|&(a, b)| (b + 1).min(line).saturating_sub(a))
            .sum();
        line - hidden
    };
    let track = scrollbar_track(viewport);
    let run = (track.height - SCROLLBAR_TICK).max(0.0);
    Some(track.y + run * (unit as f32 / total.max(1) as f32).clamp(0.0, 1.0))
}

/// The scroll line that puts the thumb's top at `y` on the track, for a
/// drag: the inverse of [`scrollbar_thumb`].
pub fn scrollbar_line_at(buffer: &Buffer, viewport: Viewport, line_height: f32, y: f32) -> usize {
    let rows = viewport.rows(line_height);
    let (total, _, units) = scroll_extent(buffer);
    let Some(thumb) = scrollbar_thumb(buffer, viewport, line_height) else {
        return 0;
    };
    let track = scrollbar_track(viewport);
    let run = track.height - thumb.height;
    if run <= 0.0 {
        return 0;
    }
    let at = ((y - track.y) / run).clamp(0.0, 1.0);
    let unit = ((total - rows) as f32 * at).round() as usize;
    line_at_unit(buffer, unit, &units)
}

// ---- timeline -------------------------------------------------------------
// A horizontal track with one slot per version, oldest at the left: the
// File History strip's. One set of functions for drawing and for hit
// testing, as with the scrollbar.

/// Points between neighbouring slots below which they get no tick each: a
/// 1pt tick needs air either side to read as one.
pub const TIMELINE_TICK_MIN: f32 = 3.0;
/// Air at each end of the track, so the thumb on the first or the last
/// slot is drawn whole inside it.
pub const TIMELINE_INSET: f32 = 8.0;

/// Where the slots are spread: the track less its insets, as its left
/// edge and its length.
fn timeline_run(track: Viewport) -> (f32, f32) {
    (
        track.x + TIMELINE_INSET,
        (track.width - 2.0 * TIMELINE_INSET).max(0.0),
    )
}

/// Where slot `index` of `slots` is on `track`: the first at the left end
/// of the run, the last at the right, one alone in the middle.
pub fn timeline_x(track: Viewport, slots: usize, index: usize) -> f32 {
    let (x0, run) = timeline_run(track);
    if slots < 2 {
        return x0 + run * 0.5;
    }
    x0 + run * index.min(slots - 1) as f32 / (slots - 1) as f32
}

/// The slot nearest `x`, the ends past the run included: the inverse of
/// [`timeline_x`].
pub fn timeline_slot_at(track: Viewport, slots: usize, x: f32) -> usize {
    let (x0, run) = timeline_run(track);
    if slots < 2 || run <= 0.0 {
        return 0;
    }
    let at = ((x - x0) / run).clamp(0.0, 1.0);
    (at * (slots - 1) as f32).round() as usize
}

/// Whether `slots` are too close on `track` for a tick each: then the
/// track is drawn as one band, with the thumb alone on it.
pub fn timeline_dense(track: Viewport, slots: usize) -> bool {
    let (_, run) = timeline_run(track);
    slots > 1 && run / ((slots - 1) as f32) < TIMELINE_TICK_MIN
}

/// The part of `track` that is slot `index`'s: halfway to each neighbour,
/// and out to the track's ends for the first and the last.
pub fn timeline_column(track: Viewport, slots: usize, index: usize) -> Viewport {
    let index = index.min(slots.saturating_sub(1));
    let at = timeline_x(track, slots, index);
    let left = if index == 0 {
        track.x
    } else {
        (timeline_x(track, slots, index - 1) + at) * 0.5
    };
    let right = if index + 1 >= slots {
        track.x + track.width
    } else {
        (at + timeline_x(track, slots, index + 1)) * 0.5
    };
    Viewport {
        x: left,
        width: (right - left).max(0.0),
        ..track
    }
}

/// Results the Cmd-P palette lists; more than fit, so the list scrolls.
pub const PALETTE_RESULTS: usize = 100;
pub const PALETTE_HEADER: f32 = 72.0;
pub const PALETTE_ROW: f32 = 44.0;
pub const PALETTE_FOOTER: f32 = 32.0;

/// Rows sketched while a palette list is on its way.
pub const PALETTE_SKELETON_ROWS: usize = 3;

/// How many rows the palette makes room for: its `count`, or the
/// sketched ones while the list is `loading` and still empty.
pub fn palette_rows_shown(count: usize, loading: bool) -> usize {
    if loading && count == 0 {
        PALETTE_SKELETON_ROWS
    } else {
        count
    }
}

pub fn palette_rect(window: Viewport, count: usize) -> Viewport {
    let width = 570.0f32.min((window.width - 32.0).max(0.0));
    let height = (PALETTE_HEADER + count.clamp(1, 8) as f32 * PALETTE_ROW + PALETTE_FOOTER)
        .min((window.height - 100.0).max(0.0));
    Viewport {
        x: window.x + (window.width - width) * 0.5,
        y: window.y + 72.0,
        width,
        height,
    }
}

pub fn palette_row_at(rect: Viewport, x: f32, y: f32) -> Option<usize> {
    if !rect.contains(x, y)
        || y < rect.y + PALETTE_HEADER
        || y >= rect.y + rect.height - PALETTE_FOOTER
    {
        return None;
    }
    Some(((y - rect.y - PALETTE_HEADER) / PALETTE_ROW) as usize)
}

pub fn palette_visible_rows(rect: Viewport) -> usize {
    ((rect.height - PALETTE_HEADER - PALETTE_FOOTER).max(0.0) / PALETTE_ROW) as usize
}

/// `scroll` moved by `delta` rows, kept where the last of `len` rows can
/// still fill `visible` of them.
pub fn scroll_clamped(scroll: usize, delta: isize, len: usize, visible: usize) -> usize {
    scroll
        .saturating_add_signed(delta)
        .min(len.saturating_sub(visible))
}

/// The last first row that still fills the list.
pub fn palette_max_scroll(count: usize, visible: usize) -> usize {
    count.saturating_sub(visible)
}

/// The first row to show so `selected` is in view, moving `scroll` as
/// little as possible: the list follows the keyboard only when it has to.
pub fn palette_follow(scroll: usize, selected: usize, count: usize, visible: usize) -> usize {
    let scroll = if selected < scroll {
        selected
    } else if visible > 0 && selected >= scroll + visible {
        selected + 1 - visible
    } else {
        scroll
    };
    scroll.min(palette_max_scroll(count, visible))
}

pub fn toolbar_sidebar(rect: Viewport) -> Viewport {
    Viewport {
        x: rect.x + 88.0,
        y: toolbar_control_y(rect),
        width: UI_CONTROL,
        height: UI_CONTROL,
    }
}

/// The top of every toolbar control: centred in the bar, above its
/// hairline.
fn toolbar_control_y(rect: Viewport) -> f32 {
    rect.y + ((rect.height - UI_CONTROL) * 0.5).floor()
}

pub(super) fn project_name(tree: &Tree) -> String {
    tree.root()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "crc".into())
}

/// Keep the beginning and end of a long folder name in a bounded toolbar.
pub(super) fn project_label(atlas: &mut Atlas, name: &str, width: f32) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= 120 && ui_text_width(atlas, name) <= width {
        return name.to_owned();
    }
    let mut low = 0;
    let mut high = chars.len().min(119);
    while low < high {
        let keep = (low + high).div_ceil(2);
        let head = keep.div_ceil(2);
        let tail = keep / 2;
        let candidate = middle_ellipsis(&chars, head, tail);
        if ui_text_width(atlas, &candidate) <= width {
            low = keep;
        } else {
            high = keep - 1;
        }
    }
    middle_ellipsis(&chars, low.div_ceil(2), low / 2)
}

/// The first `head` and last `tail` characters joined by an ellipsis.
///
/// Neither cut lands inside a joined emoji: a zero-width joiner or a
/// variation selector stays with what it joins.
pub(super) fn middle_ellipsis(chars: &[char], head: usize, tail: usize) -> String {
    let joins = |c: char| c == '\u{200d}' || ('\u{fe00}'..='\u{fe0f}').contains(&c);
    let mut head = head.min(chars.len());
    while head > 0 && head < chars.len() && (joins(chars[head]) || joins(chars[head - 1])) {
        head -= 1;
    }
    let mut tail = tail.min(chars.len() - head);
    let mut from = chars.len() - tail;
    while tail > 0 && from > head && (joins(chars[from]) || joins(chars[from - 1])) {
        tail -= 1;
        from += 1;
    }
    chars[..head]
        .iter()
        .chain(std::iter::once(&'\u{2026}'))
        .chain(chars[chars.len() - tail..].iter())
        .collect()
}

/// The project title and disclosure share this rectangle for drawing, clicks
/// and cursor shape. Its width follows the measured system-font label.
pub fn toolbar_project(tree: &Tree, atlas: &mut Atlas, rect: Viewport) -> Viewport {
    let x = toolbar_sidebar(rect).x + UI_CONTROL + UI_GAP;
    let right = toolbar_mode(rect).x;
    let available = (right - x - 12.0).max(0.0);
    let width = if available >= 42.0 {
        (ui_text_width(atlas, &project_name(tree)) + PROJECT_CHROME)
            .min(260.0)
            .min(available)
    } else {
        0.0
    };
    Viewport {
        x,
        y: toolbar_control_y(rect),
        width,
        height: UI_CONTROL,
    }
}

/// Folder icon, gaps and chevron around the project name.
pub(super) const PROJECT_CHROME: f32 = 62.0;

/// The Terminal button, left of the finder. Narrow windows give it up
/// before the project switcher; the menu and the shortcut remain.
pub fn toolbar_terminal(rect: Viewport) -> Viewport {
    let search = toolbar_search(rect);
    let width = if rect.width >= 520.0 { UI_CONTROL } else { 0.0 };
    Viewport {
        x: search.x - width - if width > 0.0 { UI_GAP } else { 0.0 },
        y: toolbar_control_y(rect),
        width,
        height: UI_CONTROL,
    }
}

/// The Editor / Agents switch, left of the Terminal button: icons and
/// labels when the toolbar has room, icons alone when it narrows, gone
/// before the project name is (Cmd-Shift-A stays).
pub fn toolbar_mode(rect: Viewport) -> Viewport {
    let terminal = toolbar_terminal(rect);
    let width = if rect.width >= 900.0 {
        MODE_WIDE
    } else if rect.width >= 600.0 {
        2.0 * UI_CONTROL + 4.0
    } else {
        0.0
    };
    Viewport {
        x: terminal.x - width - if width > 0.0 { UI_GAP } else { 0.0 },
        y: toolbar_control_y(rect),
        width,
        height: UI_CONTROL,
    }
}

const MODE_WIDE: f32 = 172.0;

/// The switch's two cells: Editor, then Agents. Each is half the track;
/// the segment drawn in it sits [`UI_SEGMENT_INSET`] inside.
pub fn toolbar_mode_segments(rect: Viewport) -> [Viewport; 2] {
    let switch = toolbar_mode(rect);
    let half = (switch.width / 2.0).floor().max(0.0);
    let cell = |i: f32| Viewport {
        x: switch.x + i * half,
        width: half,
        ..switch
    };
    [cell(0.0), cell(1.0)]
}

pub fn toolbar_search(rect: Viewport) -> Viewport {
    // Keep the project switcher reachable when the window narrows. The full
    // finder label and shortcut need room; a compact Find button does not.
    let preferred: f32 = if rect.width >= 760.0 {
        264.0
    } else if rect.width >= 620.0 {
        180.0
    } else {
        86.0
    };
    let width = preferred.min((rect.width - 156.0).max(0.0));
    Viewport {
        x: rect.x + rect.width - width - UI_INSET,
        y: toolbar_control_y(rect),
        width,
        height: UI_CONTROL,
    }
}

/// `inspector`: in Agents mode, whether the workspace inspector shows
/// (its button takes the Terminal button's place); `None` in the editor.
pub fn build_toolbar(
    tree: &Tree,
    agents: bool,
    inspector: Option<bool>,
    atlas: &mut Atlas,
    rect: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    push_rect(
        out,
        atlas,
        [rect.x, rect.y],
        [rect.width, rect.height],
        theme.sidebar_background,
    );
    // The icon buttons sit on the same resting fill, at the same height
    // and corner, as the mode switch's track: one shape across the bar.
    Button::new(toolbar_sidebar(rect))
        .icon(icons::SIDEBAR_LEFT)
        .tip("Show or Hide the Sidebar  \u{2318}B")
        .draw(out, atlas, theme);
    let search = toolbar_search(rect);
    let project = project_name(tree);
    let project_rect = toolbar_project(tree, atlas, rect);
    // The editor's project is no business of Agents mode.
    if project_rect.width > 0.0 && !agents {
        // The folder, its name and a chevron on one control, so it reads
        // as the menu it is.
        Button::new(project_rect)
            .tone(Tone::Ghost)
            .tip("Project: open, reveal or add files")
            .draw(out, atlas, theme);
        push_icon_centered(
            out,
            atlas,
            Viewport {
                x: project_rect.x + 6.0,
                width: 22.0,
                ..project_rect
            },
            icons::for_directory(&project, false).glyph,
            theme.accent,
        );
        let label_width = (project_rect.width - PROJECT_CHROME + 8.0).max(0.0);
        let label = project_label(atlas, &project, label_width);
        push_ui_text(
            out,
            atlas,
            Viewport {
                x: project_rect.x + 30.0,
                width: label_width,
                ..project_rect
            },
            &label,
            theme.text,
        );
        push_icon_centered(
            out,
            atlas,
            Viewport {
                x: project_rect.x + project_rect.width - 24.0,
                width: 20.0,
                ..project_rect
            },
            icons::CHEVRON_DOWN,
            theme.status_text,
        );
    }
    let switch = toolbar_mode(rect);
    if switch.width > 0.0 {
        let wide = switch.width >= MODE_WIDE;
        let [editor, agents_rect] = toolbar_mode_segments(rect);
        let segments = [
            Segment::new(editor, if wide { "Editor" } else { "" })
                .icon(icons::CODE)
                .on(!agents)
                .tip("Editor  \u{21e7}\u{2318}A"),
            Segment::new(agents_rect, if wide { "Agents" } else { "" })
                .icon(icons::AGENT)
                .on(agents)
                .tip("Agents  \u{21e7}\u{2318}A"),
        ];
        push_segmented(out, atlas, &segments, theme);
    }
    let terminal = toolbar_terminal(rect);
    if terminal.width > 0.0 && !agents {
        Button::new(terminal)
            .icon(icons::TERMINAL)
            .tip("Terminal  \u{2303}`")
            .draw(out, atlas, theme);
    }
    if terminal.width > 0.0
        && let Some(shown) = inspector.filter(|_| agents)
    {
        Button::new(terminal)
            .icon(icons::SIDEBAR_RIGHT)
            .on(shown)
            .tip("Show or Hide the Workspace Inspector  \u{2325}\u{2318}I")
            .draw(out, atlas, theme);
    }
    // The finder reads as a field: a search glyph, the prompt, the key.
    let over = hovered(search);
    push_rounded_rect(
        out,
        search,
        UI_RADIUS,
        if over {
            theme.control_hover
        } else {
            theme.tab_active
        },
    );
    hotspot(search, Cursor::Pointing, None);
    if search.width >= 220.0 {
        push_icon_centered(
            out,
            atlas,
            Viewport {
                x: search.x + 8.0,
                width: 20.0,
                ..search
            },
            icons::SEARCH,
            theme.gutter_text,
        );
        push_ui_text(
            out,
            atlas,
            Viewport {
                x: search.x + 32.0,
                width: (search.width - 44.0).max(0.0),
                ..search
            },
            "Search files and commands\u{2026}",
            theme.status_text,
        );
        push_ui_text_right(
            out,
            atlas,
            Viewport {
                width: (search.width - 12.0).max(0.0),
                ..search
            },
            "\u{2318}P",
            theme.gutter_text,
        );
    } else {
        Button::new(search)
            .icon(icons::SEARCH)
            .label(if search.width >= 80.0 { "Find" } else { "" })
            .tone(Tone::Ghost)
            .tip("Search files and commands  \u{2318}P")
            .draw(out, atlas, theme);
    }
    push_rect(
        out,
        atlas,
        [rect.x, rect.y + rect.height - 0.5],
        [rect.width, 0.5],
        theme.hairline,
    );
}

/// One palette result, ready to draw: a file or a command.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PaletteRow {
    /// The file-type glyph. Commands have none.
    pub icon: Option<char>,
    pub title: String,
    /// The parent folder of a file, the menu of a command.
    pub detail: String,
    /// A command's key equivalent, drawn at the trailing edge.
    pub shortcut: String,
}

/// Everything the palette needs to draw itself.
///
/// Grouped rather than passed as six parameters: they are one thing, and a
/// long positional argument list is where a caller eventually swaps two of
/// the same type without the compiler noticing.
pub struct PaletteView<'a> {
    pub rows: &'a [PaletteRow],
    /// The heading over the rows: "Project files", "Commands"...
    pub heading: &'a str,
    /// What an empty list says.
    pub empty: &'a str,
    /// The list is empty only because what fills it is still being
    /// worked out: rows are sketched in its place, not `empty`.
    pub loading: bool,
    /// The field's hint while it is empty.
    pub placeholder: &'a str,
    /// What Return does, for the footer: "Open", "Run", "Switch".
    pub action: &'a str,
    pub query: &'a str,
    pub selected: usize,
    /// The first row shown. Scrolling moves it without moving `selected`.
    pub scroll: usize,
    pub cursor: usize,
    /// Selected byte range in the query. The field carried only a caret, so
    /// Select All changed the buffer and drew nothing: the text looked
    /// untouched and the command looked broken.
    pub selection: Option<std::ops::Range<usize>>,
}

/// A file as a palette row: its name over the folder it is in.
pub fn palette_file_row(finder: &Finder, hit: &Match) -> Option<PaletteRow> {
    let entry = finder.entry(hit.index)?;
    let title = entry
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| entry.relative.clone());
    let detail = std::path::Path::new(&entry.relative)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Project root".into());
    Some(PaletteRow {
        icon: Some(icons::for_file(&entry.path).glyph),
        title,
        detail,
        shortcut: String::new(),
    })
}

/// Draws the fuzzy-open palette: a query line above a list of results.
///
/// Characters that the query matched are tinted, which is what makes a fuzzy
/// list readable: without it, a subsequence match looks arbitrary.
/// The palette's query field, right of its search glyph. Drawing, clicks
/// and the I-beam all use this.
pub fn palette_input(rect: Viewport) -> Viewport {
    Viewport {
        x: rect.x + PALETTE_INPUT_PAD,
        y: rect.y + 8.0,
        width: (rect.width - PALETTE_INPUT_PAD - palette_escape_width() - 30.0).max(0.0),
        height: 38.0,
    }
}

fn palette_escape_width() -> f32 {
    34.0
}

/// The palette's "esc" key cap, which closes it.
pub fn palette_escape(rect: Viewport) -> Viewport {
    Viewport {
        x: rect.x + rect.width - 16.0 - palette_escape_width(),
        y: rect.y + 8.0 + ((38.0 - UI_CONTROL_SM) * 0.5).floor(),
        width: palette_escape_width(),
        height: UI_CONTROL_SM,
    }
}

pub fn build_palette(
    palette: PaletteView<'_>,
    atlas: &mut Atlas,
    viewport: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    let PaletteView {
        rows,
        heading,
        empty,
        loading,
        placeholder,
        action,
        query,
        selected,
        scroll,
        cursor,
        selection,
    } = palette;
    for (spread, alpha) in [(10.0, 0.07), (6.0, 0.10), (2.0, 0.14)] {
        push_rounded_rect(
            out,
            Viewport {
                x: viewport.x - spread,
                y: viewport.y + 4.0,
                width: viewport.width + spread * 2.0,
                height: viewport.height + spread,
            },
            14.0,
            [0.0, 0.0, 0.0, alpha],
        );
    }
    push_panel(out, viewport, 12.0, theme);
    let input = palette_input(viewport);
    push_icon_scaled(
        out,
        atlas,
        Viewport {
            x: viewport.x + 12.0,
            width: PALETTE_INPUT_PAD - 16.0,
            ..input
        },
        icons::SEARCH,
        theme.gutter_text,
        1.15,
    );
    hotspot(input, Cursor::Text, None);
    push_ui_field(
        out,
        atlas,
        input,
        (9.0, 20.0),
        &UiField {
            text: query,
            cursor,
            selection,
            placeholder,
            focused: true,
        },
        theme,
    );
    // The key cap that closes the palette, lit when the pointer is on it.
    let escape = palette_escape(viewport);
    push_key_cap(
        out,
        atlas,
        escape,
        "esc",
        if super::ui::hovered(escape) {
            theme.control_hover
        } else {
            theme.palette_background
        },
        theme,
    );
    hotspot(escape, Cursor::Pointing, Some("Close"));
    push_rect(
        out,
        atlas,
        [viewport.x + 1.0, viewport.y + 51.0],
        [(viewport.width - 2.0).max(0.0), 0.5],
        theme.hairline,
    );
    push_ui_text(
        out,
        atlas,
        Viewport {
            x: viewport.x + 18.0,
            y: viewport.y + 51.0,
            width: viewport.width - 36.0,
            height: 21.0,
        },
        heading,
        theme.status_text,
    );
    if rows.is_empty() && loading {
        // On its way: the shape of the rows to come, an icon and two
        // lines each, as a file or symbol row is laid out.
        for n in 0..PALETTE_SKELETON_ROWS {
            push_skeleton_row(
                out,
                Viewport {
                    x: viewport.x + 14.0,
                    y: viewport.y + PALETTE_HEADER + n as f32 * PALETTE_ROW,
                    width: viewport.width - 32.0,
                    height: PALETTE_ROW,
                },
                20.0,
                (0.38 + 0.12 * (n % 2) as f32, 0.24),
                true,
                theme,
            );
        }
    } else if rows.is_empty() {
        push_ui_text(
            out,
            atlas,
            Viewport {
                x: viewport.x + 18.0,
                y: viewport.y + PALETTE_HEADER + 12.0,
                width: viewport.width - 36.0,
                height: 26.0,
            },
            empty,
            theme.status_text,
        );
    }
    let visible = palette_visible_rows(viewport);
    let first = scroll.min(palette_max_scroll(rows.len(), visible));
    if rows.len() > visible && visible > 0 {
        // An overlay scrollbar: a thumb in the list's trailing gutter,
        // sized by the share of rows in view.
        let track_y = viewport.y + PALETTE_HEADER + 4.0;
        let track = visible as f32 * PALETTE_ROW - 8.0;
        let thumb = (track * visible as f32 / rows.len() as f32).max(24.0);
        let at = first as f32 / palette_max_scroll(rows.len(), visible) as f32;
        let [r, g, b, _] = theme.status_text;
        push_rounded_rect(
            out,
            Viewport {
                x: viewport.x + viewport.width - 7.0,
                y: track_y + (track - thumb) * at,
                width: 4.0,
                height: thumb,
            },
            2.0,
            [r, g, b, 0.45],
        );
    }
    for (row, (index, item)) in rows
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .enumerate()
    {
        let y = viewport.y + PALETTE_HEADER + row as f32 * PALETTE_ROW;
        let rect = Viewport {
            x: viewport.x + 8.0,
            y,
            width: viewport.width - 16.0,
            height: PALETTE_ROW,
        };
        if index == selected {
            push_rounded_rect(out, rect, UI_RADIUS, theme.palette_selected);
        } else if super::ui::hovered(rect) {
            push_rounded_rect(out, rect, UI_RADIUS, theme.row_hover);
        }
        hotspot(rect, Cursor::Pointing, None);
        if let Some(icon) = item.icon {
            // Centred on the row, beside both of its lines.
            push_icon_centered(
                out,
                atlas,
                Viewport {
                    x: rect.x + 6.0,
                    width: 24.0,
                    ..rect
                },
                icon,
                if index == selected {
                    theme.accent
                } else {
                    theme.status_text
                },
            );
        }
        let text_x = if item.icon.is_some() { 38.0 } else { 12.0 };
        let shortcut_width = if item.shortcut.is_empty() {
            0.0
        } else {
            ui_text_width(atlas, &item.shortcut) + 16.0
        };
        let text_width = (rect.width - text_x - 18.0 - shortcut_width).max(0.0);
        push_ui_text(
            out,
            atlas,
            Viewport {
                x: rect.x + text_x,
                y: y + 1.0,
                width: text_width,
                height: 23.0,
            },
            &item.title,
            if index == selected {
                theme.accent
            } else {
                theme.text
            },
        );
        push_ui_text(
            out,
            atlas,
            Viewport {
                x: rect.x + text_x,
                y: y + 22.0,
                width: text_width,
                height: 19.0,
            },
            &item.detail,
            theme.status_text,
        );
        if !item.shortcut.is_empty() {
            push_ui_text_right(
                out,
                atlas,
                Viewport {
                    x: rect.x + rect.width - 12.0 - shortcut_width,
                    y,
                    width: shortcut_width,
                    height: PALETTE_ROW,
                },
                &item.shortcut,
                theme.status_text,
            );
        }
    }
    let footer = viewport.y + viewport.height - PALETTE_FOOTER;
    push_rect(
        out,
        atlas,
        [viewport.x + 1.0, footer],
        [viewport.width - 2.0, 0.5],
        theme.hairline,
    );
    push_ui_text(
        out,
        atlas,
        Viewport {
            x: viewport.x + 18.0,
            y: footer,
            width: viewport.width - 36.0,
            height: PALETTE_FOOTER,
        },
        // Escape is the key cap at the top right; said once.
        &format!("↑ ↓  Navigate       ↵  {action}"),
        theme.status_text,
    );
}

/// Tab metrics in points, deliberately not multiples of the font cell: a
/// control's size is a property of the platform, not of the user's font.
pub const TAB_PADDING_X: f32 = 12.0;
pub const TAB_CLOSE_SLOT: f32 = 22.0;
pub(super) const TAB_MIN_WIDTH: f32 = 120.0;
/// Side of the square hit box around the close glyph. Bigger than the glyph
/// on purpose: the target is what you aim at, not what you see.
pub const TAB_CLOSE_HIT: f32 = 18.0;
pub(super) const TAB_MAX_WIDTH: f32 = 240.0;

pub fn tab_width(docs: &Documents, index: usize, advance: f32) -> f32 {
    let icon_cells = if docs
        .iter()
        .nth(index)
        .and_then(|b| b.path.as_deref())
        .is_some()
        || docs.is_home()
    {
        3
    } else {
        0
    };
    let label_cells: usize = docs.title(index).chars().map(display_width).sum();
    (((label_cells + icon_cells + 2) as f32) * advance + TAB_PADDING_X * 2.0 + TAB_CLOSE_SLOT)
        .clamp(TAB_MIN_WIDTH, TAB_MAX_WIDTH)
}

/// Where each tab sits, so a click can be resolved back to a document.
#[derive(Clone, Copy, Debug)]
pub struct TabHit {
    pub index: usize,
    /// Logical-point span of the whole tab.
    pub x0: f32,
    pub x1: f32,
    /// Span of the close button, which is a narrower target inside it.
    pub close_x0: f32,
    pub close_x1: f32,
}

/// Lays out the tab bar and returns where each tab landed.
///
/// Returning the geometry rather than recomputing it for hit-testing is the
/// same rule the sidebar and gutter follow: one place decides where a thing
/// is drawn, and clicks consult that, so the two cannot drift apart.
#[allow(clippy::too_many_arguments)]
pub fn build_tab_bar(
    docs: &Documents,
    first: usize,
    hovered: Option<usize>,
    atlas: &mut Atlas,
    viewport: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
    hits: &mut Vec<TabHit>,
) {
    build_tab_bar_in(
        docs, first, hovered, true, atlas, viewport, theme, out, hits,
    );
}

/// The first tab the strip shows: `requested`, unless the active tab would
/// not fit from there (or is that tab), in which case as many tabs before
/// the active one as fit in `width`.
pub fn tab_strip_start(docs: &Documents, requested: usize, width: f32, advance: f32) -> usize {
    tab_strip_start_by(docs, requested, width, |index| {
        tab_width(docs, index, advance)
    })
}

/// The icon a tab leads with, if any.
fn tab_icon(docs: &Documents, index: usize) -> Option<icons::Icon> {
    match docs.iter().nth(index).and_then(|b| b.path.as_deref()) {
        Some(path) => Some(icons::for_file(path)),
        None if docs.is_home() => Some(icons::Icon { glyph: icons::HOME }),
        None => None,
    }
}

/// A tab's width with its title measured in the UI font it is drawn in.
/// [`tab_width`] counts monospace cells, which a proportional title does
/// not fill: every tab had a different amount of air after its name.
pub fn tab_width_ui(docs: &Documents, index: usize, atlas: &mut Atlas) -> f32 {
    let icon = tab_icon(docs, index).map_or(0.0, |i| icon_width(atlas, i.glyph) + 6.0);
    let label = ui_text_width(atlas, &docs.title(index));
    (icon + label + TAB_PADDING_X * 2.0 + TAB_CLOSE_SLOT + 4.0).clamp(TAB_MIN_WIDTH, TAB_MAX_WIDTH)
}

/// [`tab_strip_start`] with the widths given by `width_of`.
pub fn tab_strip_start_by(
    docs: &Documents,
    requested: usize,
    width: f32,
    mut width_of: impl FnMut(usize) -> f32,
) -> usize {
    let requested = requested.min(docs.len().saturating_sub(1));
    let active = docs.active_index();
    let mut fits_from = |start: usize, target: usize| {
        let used: f32 = (start..=target).map(&mut width_of).sum();
        used <= width
    };
    if active >= requested && fits_from(requested, active) && requested != active {
        return requested;
    }
    let mut start = active;
    let mut used = width_of(active);
    while start > 0 {
        let previous = width_of(start - 1);
        if used + previous > width {
            break;
        }
        start -= 1;
        used += previous;
    }
    start
}

/// [`build_tab_bar`] for a pane that may not have the keyboard: without it,
/// the active tab shows no close cross, since Cmd-W would not close it.
#[allow(clippy::too_many_arguments)]
pub fn build_tab_bar_in(
    docs: &Documents,
    first: usize,
    hovered: Option<usize>,
    focused: bool,
    atlas: &mut Atlas,
    viewport: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
    hits: &mut Vec<TabHit>,
) {
    hits.clear();
    let m = atlas.metrics;
    let solid = atlas.solid_uv();
    let hairline = 1.0 / m.scale;

    // The strip is DARKER than the editor and the active tab is exactly the
    // editor colour. That inversion is the whole fix: the active tab stops
    // being a highlight on a bar and becomes a continuation of the page,
    // which is the Safari and Xcode model.
    out.push(GlyphInstance {
        pos: [viewport.x, viewport.y],
        size: [viewport.width, viewport.height],
        uv: solid,
        color: theme.tab_bar,
        ..Default::default()
    });

    let widths: Vec<f32> = (0..docs.len())
        .map(|index| tab_width_ui(docs, index, atlas))
        .collect();
    let start = tab_strip_start_by(docs, first, viewport.width, |index| widths[index]);

    let mut x = viewport.x;
    let mut active_span: Option<(f32, f32)> = None;

    for (index, &natural_width) in widths.iter().enumerate().skip(start.min(docs.len())) {
        let active = index == docs.active_index();
        let dirty = docs.iter().nth(index).is_some_and(|b| b.is_dirty());
        let title = docs.title(index);

        // The file's icon leads the title, then a little air.
        let icon = tab_icon(docs, index);
        let remaining = viewport.x + viewport.width - x;
        if remaining <= 0.0 {
            break;
        }
        if natural_width > remaining && !hits.is_empty() {
            break;
        }
        // A very narrow window still gets one usable tab instead of an empty
        // strip. The label and close slot already clip to the same rectangle.
        let width = natural_width.min(remaining);

        if active {
            out.push(GlyphInstance {
                pos: [x, viewport.y],
                size: [width, viewport.height],
                uv: solid,
                color: theme.tab_active,
                ..Default::default()
            });
            // A 2pt accent rule along the top. The tab already merges
            // downward into the page, so its identity marker belongs above.
            out.push(GlyphInstance {
                pos: [x + TAB_PADDING_X, viewport.y + viewport.height - 2.0],
                size: [width - TAB_PADDING_X * 2.0, 2.0],
                uv: solid,
                color: theme.accent,
                ..Default::default()
            });
            active_span = Some((x, x + width));
        } else if hovered == Some(index) {
            // The tab under the pointer lifts toward the page, so it reads
            // as something to click before it is clicked.
            out.push(GlyphInstance {
                pos: [x, viewport.y],
                size: [width, viewport.height - hairline],
                uv: solid,
                color: theme.row_hover,
                ..Default::default()
            });
        } else if index > 0 {
            // Separator between inactive tabs, suppressed either side of the
            // active one so it reads as one shape with the page.
            let previous_active = index == docs.active_index() + 1;
            if !previous_active {
                out.push(GlyphInstance {
                    pos: [x, viewport.y + 6.0],
                    size: [hairline, viewport.height - 12.0],
                    uv: solid,
                    color: theme.hairline,
                    ..Default::default()
                });
            }
        }

        let color = if active {
            theme.tab_text
        } else {
            theme.tab_text_inactive
        };
        let mut label_x = x + TAB_PADDING_X;
        if let Some(icon) = icon {
            let w = icon_width(atlas, icon.glyph);
            push_icon_centered(
                out,
                atlas,
                Viewport {
                    x: label_x,
                    width: w,
                    ..viewport
                },
                icon.glyph,
                // Dimmed with the rest of an inactive tab, so the active one
                // is still the one that stands out.
                if active {
                    theme.accent
                } else {
                    theme.tab_text_inactive
                },
            );
            label_x += w + 6.0;
        }
        // Middle ellipsis keeps the extension visible, which is the half
        // people actually scan for; measured in the font it is drawn in.
        let room = (x + width - TAB_PADDING_X - TAB_CLOSE_SLOT - label_x).max(0.0);
        let shown = project_label(atlas, &title, room);
        push_ui_text(
            out,
            atlas,
            Viewport {
                x: label_x,
                y: viewport.y,
                width: (x + width - TAB_PADDING_X - TAB_CLOSE_SLOT - label_x).max(0.0),
                height: viewport.height,
            },
            &shown,
            color,
        );

        // The close slot. A dot for unsaved work, a cross otherwise, both in
        // the same column so a tab does not resize as you type into it.
        //
        // The cross appears on the active tab and on whichever tab the
        // pointer is over. Drawing one on every tab is a Windows convention
        // that turns the bar into a wall of targets you can hit by accident;
        // drawing it only on the active tab left no way to close the others
        // without selecting them first. Hover is the middle answer. A dirty
        // tab shows the cross on hover too, so the dot never blocks closing.
        let close_x = x + width - TAB_PADDING_X - m.advance;
        let hovered_here = hovered == Some(index);
        // A generous hit box, TAB_CLOSE_HIT points square and centred on the
        // glyph. A one-character target is the reason closing a tab felt bad:
        // the glyph is about 8pt wide and the finger is not.
        let centre_x = close_x + m.advance * 0.5;
        let close_box = Viewport {
            x: centre_x - TAB_CLOSE_HIT * 0.5,
            y: viewport.y + ((viewport.height - TAB_CLOSE_HIT) * 0.5).floor(),
            width: TAB_CLOSE_HIT,
            height: TAB_CLOSE_HIT,
        };
        if hovered_here || (active && focused && !dirty) {
            // The same close glyph as every other close button, on a
            // square that lights up under the pointer.
            if super::ui::hovered(close_box) {
                push_rounded_rect(out, close_box, UI_RADIUS_SM, theme.control_hover);
            }
            push_icon_centered(
                out,
                atlas,
                close_box,
                icons::CLOSE,
                if hovered_here {
                    theme.tab_text
                } else {
                    theme.tab_text_inactive
                },
            );
            hotspot(close_box, Cursor::Pointing, Some("Close Tab  \u{2318}W"));
        } else if dirty {
            // Unsaved: a dot where the cross would be.
            push_rounded_rect(
                out,
                Viewport {
                    x: centre_x - 4.0,
                    y: close_box.y + TAB_CLOSE_HIT * 0.5 - 4.0,
                    width: 8.0,
                    height: 8.0,
                },
                4.0,
                theme.tab_dirty,
            );
        }
        hits.push(TabHit {
            index,
            x0: x,
            x1: x + width,
            close_x0: centre_x - TAB_CLOSE_HIT * 0.5,
            close_x1: centre_x + TAB_CLOSE_HIT * 0.5,
        });
        x += width;
    }

    // Rule under the whole strip, broken where the active tab meets the page.
    let base_y = viewport.y + viewport.height - hairline;
    let (gap0, gap1) = active_span.unwrap_or((0.0, 0.0));
    for (from, to) in [
        (viewport.x, gap0.max(viewport.x)),
        (gap1, viewport.x + viewport.width),
    ] {
        if to > from {
            out.push(GlyphInstance {
                pos: [from, base_y],
                size: [to - from, hairline],
                uv: solid,
                color: theme.divider,
                ..Default::default()
            });
        }
    }
}

/// Visible tree rows exclude the project header and use UI control spacing.
pub fn sidebar_rows(viewport: Viewport) -> usize {
    ((viewport.height - SIDEBAR_HEADER_HEIGHT).max(0.0) / SIDEBAR_ROW_HEIGHT).floor() as usize
}

/// The two halves of the Explorer / Source Control switcher.
///
/// Source control lives in the sidebar rather than in a window-filling modal,
/// so it needs somewhere to be switched to.
/// Width of the icon strip at the window's left edge.
pub const ACTIVITY_WIDTH: f32 = 44.0;

/// The panels the icon strip switches between, top to bottom.
pub const ACTIVITY_ICONS: [(char, &str); 4] = [
    ('\u{eaf0}', "explorer"),       // cod-files
    ('\u{ea68}', "source-control"), // cod-source_control
    ('\u{eae6}', "extensions"),     // cod-extensions
    ('\u{eb2d}', "mcp"),            // cod-plug
];

/// What each icon in the strip is, said when the pointer rests on it.
pub const ACTIVITY_TIPS: [&str; 4] = [
    "Explorer",
    "Source Control  \u{2325}\u{2318}G",
    "Extensions",
    "MCP Servers",
];

/// Each icon's square in the strip.
pub fn activity_items(strip: Viewport) -> [Viewport; 4] {
    let size = ACTIVITY_WIDTH;
    std::array::from_fn(|i| Viewport {
        x: strip.x,
        y: strip.y + 6.0 + i as f32 * size,
        width: size,
        height: size,
    })
}

/// The icon strip. `active` is the panel the sidebar shows, when it shows;
/// `badge` counts Source Control's changes.
pub fn push_activity(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    strip: Viewport,
    theme: &Theme,
    active: Option<usize>,
    badge: usize,
) {
    push_rect(
        out,
        atlas,
        [strip.x, strip.y],
        [strip.width, strip.height],
        theme.sidebar_background,
    );
    push_rect(
        out,
        atlas,
        [strip.x + strip.width - 1.0, strip.y],
        [1.0, strip.height],
        theme.hairline,
    );
    for (i, rect) in activity_items(strip).into_iter().enumerate() {
        if rect.y + rect.height > strip.y + strip.height {
            break;
        }
        let on = active == Some(i);
        if on {
            push_rect(
                out,
                atlas,
                [rect.x, rect.y + 8.0],
                [2.0, rect.height - 16.0],
                theme.accent,
            );
        }
        let over = hovered(rect);
        if over && !on {
            push_rounded_rect(
                out,
                Viewport {
                    x: rect.x + 6.0,
                    y: rect.y + 4.0,
                    width: rect.width - 12.0,
                    height: rect.height - 8.0,
                },
                UI_RADIUS,
                theme.control_hover,
            );
        }
        hotspot(rect, Cursor::Pointing, Some(ACTIVITY_TIPS[i]));
        let [r, g, b, a] = theme.sidebar_text;
        push_icon_scaled(
            out,
            atlas,
            rect,
            ACTIVITY_ICONS[i].0,
            if on || over {
                theme.text
            } else {
                [r, g, b, a * 0.55]
            },
            1.45,
        );
        if i == 1 {
            push_badge(out, atlas, rect, badge, theme.sidebar_background, theme);
        }
    }
}

/// The sidebar's title row: the panel's name at the left, its actions at
/// the right, one control tall, centred in the header.
pub fn sidebar_title_row(viewport: Viewport) -> Viewport {
    Viewport {
        x: viewport.x + UI_INSET,
        y: viewport.y + ((SIDEBAR_HEADER_HEIGHT - UI_CONTROL) * 0.5).floor(),
        width: (viewport.width - UI_INSET * 2.0).max(0.0),
        height: UI_CONTROL,
    }
}

/// The title row in two halves, for anything that lines up with the old
/// Explorer / Source Control switcher.
pub fn sidebar_switcher(viewport: Viewport) -> (Viewport, Viewport) {
    let track = sidebar_title_row(viewport);
    let half = track.width * 0.5;
    (
        Viewport {
            width: half,
            ..track
        },
        Viewport {
            x: track.x + half,
            width: track.width - half,
            ..track
        },
    )
}

/// Side of each header action. Square, so a row of them is a row and not a
/// ragged line of differently sized pills.
pub const SIDEBAR_ACTION: f32 = 24.0;

/// `N` square header actions, right-aligned in the title row, left to right.
pub fn sidebar_header_buttons<const N: usize>(viewport: Viewport) -> [Viewport; N] {
    const GAP: f32 = 2.0;
    let row = sidebar_title_row(viewport);
    // The icons' own side bearing puts the last one a few points in from
    // the edge; moving the row out by as much lines the glyph up with the
    // content below.
    let right = row.x + row.width + 4.0;
    let y = row.y + ((row.height - SIDEBAR_ACTION) * 0.5).floor();
    std::array::from_fn(|index| {
        let from_right = (N - index) as f32;
        Viewport {
            x: right - from_right * SIDEBAR_ACTION - (from_right - 1.0) * GAP,
            y,
            width: SIDEBAR_ACTION,
            height: SIDEBAR_ACTION,
        }
    })
}

/// A sidebar panel's name in its title row, clipped short of `actions`
/// buttons. Returns the rectangle it took.
pub fn push_sidebar_title(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    viewport: Viewport,
    title: &str,
    actions: usize,
    theme: &Theme,
) -> Viewport {
    let row = sidebar_title_row(viewport);
    let used = actions as f32 * (SIDEBAR_ACTION + 2.0);
    let rect = Viewport {
        width: (row.width - used - UI_GAP).max(0.0),
        ..row
    };
    push_ui_text(out, atlas, rect, title, theme.status_text);
    rect
}

/// The Explorer's action buttons and the label beside them.
///
/// Returns the label rectangle and the buttons in drawing order: new file,
/// new folder, collapse all, refresh, on the title row's right.
pub fn sidebar_actions(viewport: Viewport) -> (Viewport, [Viewport; 4]) {
    let buttons = sidebar_header_buttons::<4>(viewport);
    let row = sidebar_title_row(viewport);
    let label = Viewport {
        width: (buttons[0].x - row.x - UI_GAP).max(0.0),
        ..row
    };
    (label, buttons)
}

/// Lays out the file-tree sidebar into `out`, appending to whatever is there.
///
/// Returns the number of rows drawn. Rows are exactly one line tall, which
/// keeps hit-testing a division rather than a search. With `scm` set, the
/// header and switcher are drawn and the body is left to the source-control
/// panel, which fills the same column.
pub fn build_sidebar(
    tree: &Tree,
    scm: bool,
    atlas: &mut Atlas,
    viewport: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) -> usize {
    build_sidebar_with_edit(tree, scm, None, atlas, viewport, theme, out)
}

/// A name being typed into the tree: a new file or folder about to exist,
/// or an item being renamed in place.
#[derive(Clone, Debug)]
pub struct SidebarEdit<'a> {
    /// The visible row the field occupies.
    pub row: usize,
    pub depth: usize,
    /// Renaming replaces the item's own row; creating inserts a row.
    pub replaces: bool,
    pub is_dir: bool,
    pub text: &'a str,
    pub cursor: usize,
    pub selection: Option<std::ops::Range<usize>>,
}

/// What the Explorer says with no folder open.
pub fn sidebar_empty_state() -> EmptyState<'static> {
    EmptyState::new("No folder open", "Open one to browse its files")
        .icon(icons::FOLDER)
        .button("Open Folder", None)
}

/// The room under the Explorer's title, where its empty state is centred.
fn sidebar_empty_body(viewport: Viewport) -> Viewport {
    Viewport {
        y: viewport.y + SIDEBAR_HEADER_HEIGHT,
        height: (viewport.height - SIDEBAR_HEADER_HEIGHT).max(0.0),
        ..viewport
    }
}

/// Where the empty Explorer's Open Folder button lands, for a click on it.
pub fn sidebar_empty_button(atlas: &mut Atlas, viewport: Viewport) -> Option<Viewport> {
    sidebar_empty_state()
        .layout(atlas, sidebar_empty_body(viewport))
        .button
}

/// [`build_sidebar`] with an inline name field at `edit`.
pub fn build_sidebar_with_edit(
    tree: &Tree,
    scm: bool,
    edit: Option<SidebarEdit<'_>>,
    atlas: &mut Atlas,
    viewport: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) -> usize {
    let m = atlas.metrics;
    let (cell_w, cell_h) = atlas.cell_size();
    let solid = atlas.solid_uv();
    let glyph_dy = m.glyph_dy(SIDEBAR_ROW_HEIGHT);

    // Panel background and the hairline separating it from the editor.
    out.push(GlyphInstance {
        pos: [viewport.x, viewport.y],
        size: [viewport.width, viewport.height],
        uv: solid,
        color: theme.sidebar_background,
        ..Default::default()
    });
    // One device pixel, not one logical point. A 1.0pt line is 2px on a
    // Retina display, and AppKit never draws one that thick. This is the
    // single most legible "not a Mac app" tell.
    let hairline = 1.0 / m.scale;
    out.push(GlyphInstance {
        pos: [viewport.x + viewport.width - hairline, viewport.y],
        size: [hairline, viewport.height],
        uv: solid,
        color: theme.divider,
        ..Default::default()
    });

    let rows = sidebar_rows(viewport);
    if rows == 0 {
        return 0;
    }

    // No folder: the one empty state, with the one thing to do. Home,
    // beside it, lists the rest.
    if tree.root().is_none() {
        push_sidebar_title(out, atlas, viewport, "EXPLORER", 0, theme);
        push_empty_state(
            out,
            atlas,
            sidebar_empty_body(viewport),
            sidebar_empty_state(),
            theme,
        );
        return 0;
    }

    // The panel's name and its actions, on one row.
    let title = if scm { "SOURCE CONTROL" } else { "EXPLORER" };
    if scm {
        // The panel draws its own actions and the rest of the column.
        return 0;
    }
    let (_, actions) = sidebar_actions(viewport);
    push_sidebar_title(out, atlas, viewport, title, actions.len(), theme);
    // The commands act on the selected item's folder; the tooltip says
    // which, so there is no guessing before committing.
    let where_to = tree
        .target_dir()
        .and_then(|dir| {
            tree.root()
                .and_then(|root| dir.strip_prefix(root).ok())
                .map(|rel| rel.to_string_lossy().into_owned())
        })
        .filter(|rel| !rel.is_empty());
    let in_folder = |what: &str| match &where_to {
        Some(dir) => format!("{what} in {dir}"),
        None => format!("{what} at the Project Root"),
    };
    let tips = [
        in_folder("New File"),
        in_folder("New Folder"),
        "Collapse Folders".to_owned(),
        "Refresh Explorer".to_owned(),
    ];
    for ((rect, glyph), tip) in actions
        .iter()
        .zip([
            icons::NEW_FILE,
            icons::NEW_FOLDER,
            icons::COLLAPSE_ALL,
            icons::REFRESH,
        ])
        .zip(&tips)
    {
        Button::new(*rect)
            .icon(glyph)
            .tone(Tone::Ghost)
            .tip(tip)
            .draw(out, atlas, theme);
    }

    // An inserted field is one more row; a rename takes its item's row.
    let total = tree.len() + usize::from(edit.as_ref().is_some_and(|e| !e.replaces));
    let first = tree.scroll.min(total.saturating_sub(1));
    let last = (first + rows).min(total);
    let mut drawn = 0;

    for visible in first..last {
        let y = sidebar_row_rect(viewport, visible - first).y;
        if let Some(edit) = edit.as_ref().filter(|e| e.row == visible) {
            push_sidebar_edit_row(edit, atlas, viewport, y, theme, out);
            drawn += 1;
            continue;
        }
        let i = match &edit {
            Some(e) if !e.replaces && visible > e.row => visible - 1,
            _ => visible,
        };
        let Some(entry) = tree.rows().get(i) else {
            break;
        };

        let row_rect = Viewport {
            y,
            height: SIDEBAR_ROW_HEIGHT,
            ..viewport
        };
        if tree.selected == Some(i) {
            push_rounded_rect(
                out,
                Viewport {
                    x: viewport.x + 6.0,
                    y: y + 1.0,
                    width: (viewport.width - 12.0).max(0.0),
                    height: SIDEBAR_ROW_HEIGHT - 2.0,
                },
                UI_RADIUS_SM + 1.0,
                theme.sidebar_selected,
            );
        } else {
            push_row_hover(out, row_rect, theme);
        }

        // Chevron, icon, name. Each icon is two cells wide, and a file gets
        // the chevron's two blank cells so names line up down a folder.
        let indent = UI_INSET + entry.depth as f32 * 16.0;
        let chevron_w = (cell_w * 2.0).max(16.0);
        let icon_w = (cell_w * 2.0).max(18.0) + 6.0;
        let mut column = indent;
        let mut draw_icon = |glyph: char, color: [f32; 4], column: f32| {
            if let Some(slot) = atlas.slot_for(glyph) {
                out.push(GlyphInstance {
                    pos: [m.snap(viewport.x + column), y + glyph_dy],
                    size: [cell_w * slot.cells as f32, cell_h],
                    uv: slot.uv,
                    flags: slot.flags(),
                    color,
                    ..Default::default()
                });
            }
        };
        let icon = if entry.is_dir {
            let chevron = if entry.expanded {
                icons::CHEVRON_DOWN
            } else {
                icons::CHEVRON_RIGHT
            };
            draw_icon(chevron, theme.gutter_text, column);
            icons::for_directory(&entry.name, entry.expanded)
        } else {
            icons::for_file(&entry.path)
        };
        // Ignored by Git: icon and name at half strength, so what the
        // repository does not track reads as a step back from what it does.
        let ignored = tree.ignored(&entry.path);
        let dim = |[r, g, b, a]: [f32; 4]| {
            if ignored == crate::project::tree::Ignored::No {
                [r, g, b, a]
            } else {
                [r, g, b, a * 0.45]
            }
        };
        column += chevron_w;
        draw_icon(
            icon.glyph,
            if tree.selected == Some(i) {
                theme.accent
            } else {
                dim(theme.status_text)
            },
            column,
        );
        column += icon_w;

        let color = if tree.selected == Some(i) {
            theme.accent
        } else if ignored == crate::project::tree::Ignored::Preview {
            // Would be ignored once the .gitignore being typed is saved.
            let [r, g, b, _] = theme.accent;
            [r, g, b, 0.8]
        } else if entry.is_dir {
            dim(theme.sidebar_directory)
        } else {
            dim(theme.sidebar_text)
        };
        push_ui_text(
            out,
            atlas,
            Viewport {
                x: viewport.x + column,
                y,
                width: (viewport.width - column - 14.0).max(0.0),
                height: SIDEBAR_ROW_HEIGHT,
            },
            &entry.name,
            color,
        );
        drawn += 1;
    }

    drawn
}

/// The name field in the tree, drawn where the item will appear: the same
/// indent, an icon that follows the extension as it is typed, and a caret.
pub(super) fn push_sidebar_edit_row(
    edit: &SidebarEdit<'_>,
    atlas: &mut Atlas,
    viewport: Viewport,
    y: f32,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    let m = atlas.metrics;
    let (cell_w, cell_h) = atlas.cell_size();
    let glyph_dy = m.glyph_dy(SIDEBAR_ROW_HEIGHT);
    let indent = 12.0 + edit.depth as f32 * 16.0 + 16.0;
    let icon = if edit.is_dir {
        icons::for_directory(edit.text, false)
    } else {
        icons::for_file(std::path::Path::new(edit.text))
    };
    if let Some(slot) = atlas.slot_for(icon.glyph) {
        out.push(GlyphInstance {
            pos: [m.snap(viewport.x + indent), y + glyph_dy],
            size: [cell_w * slot.cells as f32, cell_h],
            uv: slot.uv,
            flags: slot.flags(),
            color: theme.status_text,
            ..Default::default()
        });
    }
    let field = Viewport {
        x: viewport.x + indent + 22.0,
        y: y + 2.0,
        width: (viewport.width - indent - 22.0 - 12.0).max(0.0),
        height: SIDEBAR_ROW_HEIGHT - 4.0,
    };
    push_rounded_rect(out, field, 4.0, theme.accent);
    push_rounded_rect(
        out,
        Viewport {
            x: field.x + 1.0,
            y: field.y + 1.0,
            width: (field.width - 2.0).max(0.0),
            height: (field.height - 2.0).max(0.0),
        },
        3.0,
        theme.tab_active,
    );
    let input = Viewport {
        x: field.x + 6.0,
        y: field.y,
        width: (field.width - 12.0).max(0.0),
        height: field.height,
    };
    push_ui_field(
        out,
        atlas,
        input,
        (3.0, input.height - 6.0),
        &UiField {
            text: edit.text,
            cursor: edit.cursor,
            selection: edit.selection.clone(),
            placeholder: "",
            focused: true,
        },
        theme,
    );
}

/// Something the pointer can land on, named so input handling, cursor
/// shapes and scripted tests all address the same rectangle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hit {
    ToolbarSidebar,
    ToolbarProject,
    ToolbarSearch,
    /// Shows or hides the terminal panel.
    ToolbarTerminal,
    /// Shows or hides the workspace inspector, in Agents mode.
    ToolbarInspector,
    /// The Editor (`false`) or Agents (`true`) half of the mode switch.
    ToolbarMode(bool),
    /// The grab band on the terminal panel's top edge.
    TerminalDivider,
    /// A session tab in the terminal panel's header, its close button, and
    /// the button that starts a new shell.
    TerminalTab(usize),
    TerminalClose(usize),
    TerminalNew,
    /// Hides the panel; the sessions keep running.
    TerminalHide,
    /// The terminal's screen.
    Terminal,
    /// An icon in the strip: 0 Explorer, 1 Source Control, 2 Extensions.
    Activity(usize),
    /// New file, new folder, collapse all, refresh.
    SidebarAction(usize),
    SidebarRow(usize),
    /// The grab band on the divider between sidebar and editor.
    SidebarDivider,
    /// The grab band between the text and the page beside it.
    PreviewDivider,
    Tab(usize),
    TabClose(usize),
    /// Empty tab strip, right of the last tab.
    TabStrip,
    ResponseSegment(usize),
    /// Accept and Reject above a change proposed by Claude.
    ReviewAccept,
    ReviewReject,
    /// Start Task and Open on GitHub above an issue's tab.
    IssueStartTask,
    IssueOpenGitHub,
    /// Above a version from a file's history: Older, Newer, the close
    /// button, and the timeline of the versions.
    ScrubOlder,
    ScrubNewer,
    /// The filter: All, Commits or Local versions.
    ScrubFilter,
    /// What the gutter marks against: the version before, or the working
    /// tree; and Restore This Version.
    ScrubCompare,
    ScrubRestore,
    ScrubClose,
    ScrubTrack,
    /// Above a document with merge conflicts: the inline or side-by-side
    /// switch (`true` for side by side), Previous and Next (`true`), and
    /// Mark Resolved.
    ConflictMode(bool),
    ConflictStep(bool),
    ConflictResolve,
    /// Resolves conflict `n` of the active document one way, from its
    /// marker line or its header in the columns.
    ConflictTake(usize, crate::project::conflict::Take),
    /// Above a CSV or TSV document: the Grid (`true`) or Text half of the
    /// mode switch, and the grid itself.
    TableMode(bool),
    TableBody,
    /// The Query segment beside the modes: opens or closes the SQL field.
    TableQuery,
    /// The SQL field itself, while it is open.
    TableQueryField,
    /// Runs the field's statement through DuckDB.
    TableRun,
    /// On a result tab: back to the file the query ran over.
    TableBack,
    TableExport,
    /// A part of the breadcrumb path, left to right.
    Breadcrumb(usize),
    Find,
    Text,
    Status,
    /// The branch named in the status bar: it opens the branch list.
    StatusBranch,
    /// The button beside a status-line message that asks for something.
    StatusAction,
    /// The branch at the top of Source Control: it opens the branch list.
    GitBranch,
    /// The repository menu in Source Control's header, in a workspace of
    /// several: it opens the repository list.
    GitRepo,
    /// Source Control's other controls, named for scripts: `refresh`,
    /// `more`, `pull`, `push`, `message`, `commit`. Clicks on them go
    /// through the panel's own hit testing, with the same geometry.
    Git(&'static str),
    /// The `n`th row of Source Control's list on screen, and its staging
    /// control.
    GitRow(usize),
    GitToggle(usize),
    /// An unfocused editor pane, tabs through text. A click gives it the
    /// keyboard and is then handled as a click in the focused pane.
    Pane(usize),
}

impl Hit {
    /// The name a script uses: `toolbar.sidebar`, `sidebar.action.0`,
    /// `tab.close.1`, `response.segment.2`.
    pub fn name(&self) -> String {
        match self {
            Hit::ToolbarSidebar => "toolbar.sidebar".into(),
            Hit::ToolbarProject => "toolbar.project".into(),
            Hit::ToolbarSearch => "toolbar.search".into(),
            Hit::ToolbarTerminal => "toolbar.terminal".into(),
            Hit::ToolbarInspector => "toolbar.inspector".into(),
            Hit::ToolbarMode(agents) => if *agents {
                "toolbar.agents"
            } else {
                "toolbar.editor"
            }
            .into(),
            Hit::TerminalDivider => "terminal.divider".into(),
            Hit::TerminalTab(i) => format!("terminal.tab.{i}"),
            Hit::TerminalClose(i) => format!("terminal.close.{i}"),
            Hit::TerminalNew => "terminal.new".into(),
            Hit::TerminalHide => "terminal.hide".into(),
            Hit::Terminal => "terminal".into(),
            Hit::Activity(i) => format!("activity.{}", ACTIVITY_ICONS[*i].1),
            Hit::SidebarAction(i) => format!("sidebar.action.{i}"),
            Hit::SidebarRow(i) => format!("sidebar.row.{i}"),
            Hit::SidebarDivider => "sidebar.divider".into(),
            Hit::PreviewDivider => "preview.divider".into(),
            Hit::Tab(i) => format!("tab.{i}"),
            Hit::TabClose(i) => format!("tab.close.{i}"),
            Hit::TabStrip => "tab.strip".into(),
            Hit::ResponseSegment(i) => format!("response.segment.{i}"),
            Hit::ReviewAccept => "review.accept".into(),
            Hit::ReviewReject => "review.reject".into(),
            Hit::IssueStartTask => "issue.start".into(),
            Hit::IssueOpenGitHub => "issue.github".into(),
            Hit::ScrubOlder => "scrub.older".into(),
            Hit::ScrubNewer => "scrub.newer".into(),
            Hit::ScrubClose => "scrub.close".into(),
            Hit::ScrubFilter => "scrub.filter".into(),
            Hit::ScrubCompare => "scrub.compare".into(),
            Hit::ScrubRestore => "scrub.restore".into(),
            Hit::ScrubTrack => "scrub.track".into(),
            Hit::ConflictMode(side) => if *side {
                "conflict.side"
            } else {
                "conflict.inline"
            }
            .into(),
            Hit::ConflictStep(forward) => if *forward {
                "conflict.next"
            } else {
                "conflict.previous"
            }
            .into(),
            Hit::ConflictResolve => "conflict.resolve".into(),
            Hit::ConflictTake(i, take) => format!("conflict.{}.{i}", take.name()),
            Hit::TableMode(grid) => if *grid {
                "table.mode.grid"
            } else {
                "table.mode.text"
            }
            .into(),
            Hit::TableBody => "table.body".into(),
            Hit::TableQuery => "table.query".into(),
            Hit::TableQueryField => "table.query.field".into(),
            Hit::TableRun => "table.run".into(),
            Hit::TableBack => "table.back".into(),
            Hit::TableExport => "table.export".into(),
            Hit::Breadcrumb(i) => format!("breadcrumb.{i}"),
            Hit::Find => "find".into(),
            Hit::Text => "text".into(),
            Hit::Status => "status".into(),
            Hit::StatusBranch => "status.branch".into(),
            Hit::StatusAction => "status.action".into(),
            Hit::GitBranch => "git.branch".into(),
            Hit::GitRepo => "git.repo".into(),
            Hit::Git(name) => format!("git.{name}"),
            Hit::GitRow(i) => format!("git.row.{i}"),
            Hit::GitToggle(i) => format!("git.toggle.{i}"),
            Hit::Pane(i) => format!("pane.{i}"),
        }
    }
}

/// The rectangles of one frame, in hit-testing order: the first region
/// containing a point wins, so specific targets are listed before the
/// areas that contain them.
#[derive(Clone, Debug, Default)]
pub struct Frame {
    pub regions: Vec<(Hit, Viewport)>,
}

impl Frame {
    pub fn push(&mut self, hit: Hit, rect: Viewport) {
        if rect.width > 0.0 && rect.height > 0.0 {
            self.regions.push((hit, rect));
        }
    }

    /// What is under `(x, y)`.
    pub fn hit(&self, x: f32, y: f32) -> Option<&Hit> {
        self.regions
            .iter()
            .find(|(_, rect)| rect.contains(x, y))
            .map(|(hit, _)| hit)
    }

    /// Where `hit` is, if it is on screen.
    pub fn rect(&self, hit: &Hit) -> Option<Viewport> {
        self.regions
            .iter()
            .find(|(h, _)| h == hit)
            .map(|(_, rect)| *rect)
    }

    /// Where the region called `name` is, for scripts.
    pub fn named(&self, name: &str) -> Option<Viewport> {
        self.regions
            .iter()
            .find(|(h, _)| h.name() == name)
            .map(|(_, rect)| *rect)
    }
}

/// Which sidebar row is at `y`, or `None` past the last row.
/// The rectangle of the sidebar row `on_screen` rows below the first one
/// shown. [`sidebar_row_at`] is the inverse.
pub fn sidebar_row_rect(viewport: Viewport, on_screen: usize) -> Viewport {
    Viewport {
        y: viewport.y + SIDEBAR_HEADER_HEIGHT + on_screen as f32 * SIDEBAR_ROW_HEIGHT,
        height: SIDEBAR_ROW_HEIGHT,
        ..viewport
    }
}

pub fn sidebar_row_at(
    tree: &Tree,
    field: Option<SidebarField>,
    viewport: Viewport,
    y: f32,
) -> Option<usize> {
    if y < viewport.y + SIDEBAR_HEADER_HEIGHT || y >= viewport.y + viewport.height {
        return None;
    }
    let row = ((y - viewport.y - SIDEBAR_HEADER_HEIGHT) / SIDEBAR_ROW_HEIGHT).floor() as usize;
    let total = tree.len() + usize::from(field.is_some_and(|f| f.inserted));
    let visible = tree.scroll.min(total.saturating_sub(1)) + row;
    if visible >= total {
        return None;
    }
    tree_row_at(visible, field)
}

/// A name field open in the sidebar: the visible row it takes, and whether
/// it is inserted (New File, New Folder) rather than over a row (Rename).
#[derive(Clone, Copy, Debug)]
pub struct SidebarField {
    pub row: usize,
    pub inserted: bool,
}

/// The tree row drawn at visible row `visible`: none where the field is, and
/// one up below an inserted field, which pushes the rows under it down.
pub fn tree_row_at(visible: usize, field: Option<SidebarField>) -> Option<usize> {
    match field {
        Some(f) if f.row == visible => None,
        Some(f) if f.inserted && visible > f.row => Some(visible - 1),
        _ => Some(visible),
    }
}

/// File location uses the same geometry in the app and the offline frame.
/// The segment buttons of a response strip, in [`Segment::ALL`] order.
pub fn response_segments(atlas: &mut Atlas, strip: Viewport, labels: [&str; 3]) -> [Viewport; 3] {
    let mut x = strip.x + 16.0;
    let y = strip.y + 39.0;
    let mut rects = [strip; 3];
    for (rect, label) in rects.iter_mut().zip(labels) {
        let width = ui_text_width(atlas, label) + 24.0;
        *rect = Viewport {
            x,
            y,
            width,
            height: UI_CONTROL,
        };
        x += width + 4.0;
    }
    rects
}

/// Status, facts and the segment switch above a response body.
pub fn build_response_strip(
    view: &crate::http::view::View,
    atlas: &mut Atlas,
    strip: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    use crate::http::view::{Segment, Verdict};
    let tone = match view.verdict() {
        Verdict::Pending => theme.status_text,
        Verdict::Success => theme.diff_added,
        Verdict::Redirect => theme.syn_constant,
        Verdict::ClientError | Verdict::ServerError | Verdict::Failed => theme.diff_removed,
    };
    // Row one: the status pill, then the facts.
    let status = view.status();
    let pill = Viewport {
        x: strip.x + 16.0,
        y: strip.y + 6.0,
        width: ui_text_width(atlas, &status) + 20.0,
        height: 28.0,
    };
    push_rounded_rect(out, pill, 6.0, [tone[0], tone[1], tone[2], 0.18]);
    push_ui_text_centered(out, atlas, pill, &status, tone);
    push_ui_text(
        out,
        atlas,
        Viewport {
            x: pill.x + pill.width + 12.0,
            y: pill.y,
            width: (strip.x + strip.width - pill.x - pill.width - 28.0).max(0.0),
            height: pill.height,
        },
        &view.facts(),
        theme.status_text,
    );
    // Row two: the segments.
    let headers = format!("Headers {}", view.header_count());
    let labels = [
        Segment::Body.label(),
        headers.as_str(),
        Segment::Request.label(),
    ];
    for ((rect, label), segment) in response_segments(atlas, strip, labels)
        .into_iter()
        .zip(labels)
        .zip(Segment::ALL)
    {
        Button::new(rect)
            .label(label)
            .tone(Tone::Ghost)
            .on(segment == view.segment)
            .draw(out, atlas, theme);
    }
    push_rect(
        out,
        atlas,
        [strip.x, strip.y + strip.height - 1.0],
        [strip.width, 1.0],
        theme.hairline,
    );
}

/// One part of the breadcrumb path: the folder or file it names, and
/// where its label is, so a click can offer what is in it.
#[derive(Clone, Debug, PartialEq)]
pub struct Crumb {
    pub path: std::path::PathBuf,
    pub label: String,
    pub rect: Viewport,
}

pub(super) const CRUMB_SEPARATOR: &str = "  \u{203a}  ";

/// The breadcrumb row's parts, left to right, as drawn: relative to the
/// project root when the file is in it. Parts past the row's right edge
/// are left out.
pub fn breadcrumb_segments(
    buffer: &Buffer,
    tree: &Tree,
    atlas: &mut Atlas,
    rect: Viewport,
) -> Vec<Crumb> {
    let Some(path) = buffer.path.as_deref() else {
        return Vec::new();
    };
    let within = |root: &std::path::Path, path: &std::path::Path| {
        path.strip_prefix(root)
            .ok()
            .map(|rel| (root.to_path_buf(), rel.to_path_buf()))
    };
    // The root and the file may name the same folder differently (a
    // symlinked /var and /private/var); only then is it worth asking the
    // file system.
    let found = tree.root().and_then(|root| {
        within(root, path).or_else(|| {
            let root = std::fs::canonicalize(root).ok()?;
            let path = std::fs::canonicalize(path).ok()?;
            within(&root, &path)
        })
    });
    let (mut base, relative) =
        found.unwrap_or_else(|| (std::path::PathBuf::new(), path.to_path_buf()));
    let separator = ui_text_width(atlas, CRUMB_SEPARATOR);
    let right = rect.x + rect.width - 20.0;
    let mut x = rect.x + 20.0;
    let mut crumbs = Vec::new();
    for part in relative.components() {
        base.push(part);
        let label = part.as_os_str().to_string_lossy().into_owned();
        let width = ui_text_width(atlas, &label);
        if x >= right {
            break;
        }
        crumbs.push(Crumb {
            path: base.clone(),
            label,
            rect: Viewport {
                x: x - 4.0,
                width: (width + 8.0).min(right - x + 4.0),
                ..rect
            },
        });
        x += width + separator;
    }
    crumbs
}

/// The row under the tabs naming the open file, each part a target. Home
/// has no file, so the row is empty there.
pub fn build_breadcrumbs(
    buffer: &Buffer,
    tree: &Tree,
    home: bool,
    atlas: &mut Atlas,
    rect: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    let crumbs = breadcrumb_segments(buffer, tree, atlas, rect);
    if crumbs.is_empty() && !home {
        let label = buffer.display_name();
        push_ui_text(
            out,
            atlas,
            Viewport {
                x: rect.x + 20.0,
                width: (rect.width - 40.0).max(0.0),
                ..rect
            },
            &label,
            theme.status_text,
        );
    }
    let right = rect.x + rect.width - 20.0;
    let last = crumbs.len().saturating_sub(1);
    for (index, crumb) in crumbs.iter().enumerate() {
        // Each part opens its folder's menu: it lights up like a button.
        let pad = Viewport {
            y: crumb.rect.y + ((crumb.rect.height - UI_CONTROL_SM) * 0.5).floor(),
            height: UI_CONTROL_SM,
            ..crumb.rect
        };
        if super::ui::hovered(crumb.rect) {
            push_rounded_rect(out, pad, UI_RADIUS_SM, theme.control_hover);
        }
        hotspot(crumb.rect, Cursor::Pointing, None);
        let x = crumb.rect.x + 4.0;
        // The file itself reads brighter than the folders leading to it.
        let color = if index == last {
            theme.text
        } else {
            theme.status_text
        };
        push_ui_text(
            out,
            atlas,
            Viewport {
                x,
                width: (right - x).max(0.0),
                ..rect
            },
            &crumb.label,
            color,
        );
        if index < last {
            let sx = x + ui_text_width(atlas, &crumb.label);
            push_ui_text(
                out,
                atlas,
                Viewport {
                    x: sx,
                    width: (right - sx).max(0.0),
                    ..rect
                },
                CRUMB_SEPARATOR,
                theme.gutter_text,
            );
        }
    }
    // Home has no path to show: the band is part of the page, with no
    // rule cutting an empty strip off the top of it.
    if home && crumbs.is_empty() {
        return;
    }
    push_rect(
        out,
        atlas,
        [rect.x, rect.y + rect.height - 1.0 / atlas.metrics.scale],
        [rect.width, 1.0 / atlas.metrics.scale],
        theme.hairline,
    );
}

#[cfg(test)]
mod home_tests {
    use super::*;

    #[test]
    fn every_action_row_is_a_hit_and_recents_skip_the_open_project() {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let mut out = Vec::new();
        let mut hits = Vec::new();
        let rect = Viewport {
            x: 240.0,
            y: 116.0,
            width: 860.0,
            height: 616.0,
        };
        let here = std::path::PathBuf::from("/tmp/personal-notes");
        let recent = vec![
            here.clone(),
            std::path::PathBuf::from("/tmp/recipes"),
            std::path::PathBuf::from("/tmp/garden-log"),
        ];
        build_home(
            &mut out,
            &mut atlas,
            rect,
            &Theme::default(),
            Some(&here),
            &recent,
            None,
            &[],
            None,
            false,
            &mut hits,
        );
        let actions: Vec<&HomeAction> = hits.iter().map(|h| &h.action).collect();
        assert_eq!(
            actions[..3],
            [
                &HomeAction::OpenFolder,
                &HomeAction::NewFile,
                &HomeAction::FindFile
            ]
        );
        assert_eq!(
            actions[3..],
            [
                &HomeAction::OpenProject("/tmp/recipes".into()),
                &HomeAction::OpenProject("/tmp/garden-log".into())
            ],
            "the open project is not offered as recent"
        );
        for hit in &hits {
            assert!(hit.rect.x >= rect.x && hit.rect.x + hit.rect.width <= rect.x + rect.width);
            assert!(hit.rect.y >= rect.y && hit.rect.y + hit.rect.height <= rect.y + rect.height);
        }
        assert!(!out.is_empty());
    }

    #[test]
    fn a_workspace_adds_its_session_waiting_repositories_and_changes() {
        use crate::project::workspace::{RepoSummary, Settings, Summary};
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let mut out = Vec::new();
        let mut hits = Vec::new();
        let rect = Viewport {
            x: 0.0,
            y: 0.0,
            width: 860.0,
            height: 1400.0,
        };
        let root = std::path::PathBuf::from("/tmp/garden-log");
        let state = root.join("docs/state.md");
        let log = root.join("docs/log.md");
        let repo = |name: &str, commits: &[&str]| RepoSummary {
            path: root.join(name),
            label: name.into(),
            status: "main".into(),
            changes: 1,
            commits: commits.iter().map(|c| c.to_string()).collect(),
        };
        let summary = Summary {
            settings: Settings {
                state: Some(state.clone()),
                log: Some(log.clone()),
                waiting_heading: None,
                private_markers: Vec::new(),
            },
            waiting: vec!["Water the tomatoes".into()],
            repos: vec![
                repo("app", &["fix the watering schedule"]),
                repo("site", &[]),
            ],
            notes: vec![root.join("docs/plants.md")],
            since: Some(1),
            calls: Vec::new(),
        };
        build_home(
            &mut out,
            &mut atlas,
            rect,
            &Theme::default(),
            Some(&root),
            &[],
            Some(&summary),
            &[("claude".into(), "waiting: Approve?".into())],
            None,
            false,
            &mut hits,
        );
        let actions: Vec<HomeAction> = hits.iter().map(|h| h.action.clone()).collect();
        assert_eq!(actions[0], HomeAction::ShowTerminal(0));
        assert_eq!(
            actions[4..],
            [
                HomeAction::OpenFiles(vec![state.clone()]),
                HomeAction::OpenFiles(vec![log, state.clone()]),
                HomeAction::OpenFiles(vec![state]),
                HomeAction::ShowRepo(root.join("app")),
                HomeAction::ShowRepo(root.join("site")),
                HomeAction::ShowRepo(root.join("app")),
                HomeAction::OpenFiles(vec![root.join("docs/plants.md")]),
            ]
        );
        for pair in hits.windows(2) {
            assert!(pair[0].rect.y + pair[0].rect.height <= pair[1].rect.y + 0.01);
        }
    }

    #[test]
    fn the_column_is_centred_in_a_wide_pane_and_keeps_a_margin_in_a_narrow_one() {
        let pane = |width: f32| Viewport {
            x: 240.0,
            y: 0.0,
            width,
            height: 800.0,
        };
        // Wide: the column stops at its measure and sits in the middle.
        let (x, width) = home_column(pane(1200.0));
        assert_eq!(width, HOME_MEASURE);
        let left = x - 240.0;
        let right = 240.0 + 1200.0 - (x + width);
        assert!((left - right).abs() < 0.01, "left {left}, right {right}");
        // Narrow (a 900 pt window beside the sidebar): the column shrinks
        // and keeps the same margin on both sides.
        let (x, width) = home_column(pane(660.0));
        assert!((240.0..HOME_MEASURE).contains(&width));
        assert!((x - 240.0 - (240.0 + 660.0 - x - width)).abs() < 0.01);
        assert!(x - 240.0 >= 28.0);
        // Narrower than the column's floor: it still fits the pane.
        let (x, width) = home_column(pane(260.0));
        assert!(x + width <= 240.0 + 260.0);
        assert!(x - 240.0 >= 28.0);
    }

    #[test]
    fn a_recent_path_is_cut_from_the_left_and_keeps_its_last_two_parts() {
        let within = |n: usize| move |s: &str| s.chars().count() <= n;
        let deep = "/private/var/folders/g1/abc/T/tmp.x";
        assert_eq!(elide_path(deep, within(40)), deep, "fits: shown whole");
        assert_eq!(
            elide_path(deep, within(24)),
            "/private/\u{2026}/T/tmp.x",
            "the disk's first folder, the middle gone, the last two kept"
        );
        assert_eq!(elide_path(deep, within(12)), "\u{2026}/T/tmp.x");
        assert_eq!(elide_path(deep, within(8)), "\u{2026}/tmp.x");
        assert_eq!(
            elide_path(deep, within(3)),
            "tmp.x",
            "nothing fits: the name"
        );
        assert_eq!(
            elide_path("~/Desktop/notes/garden", within(16)),
            "~/\u{2026}/notes/garden"
        );
        assert_eq!(
            elide_path("~/a/notes/garden", within(15)),
            "\u{2026}/notes/garden",
            "the head goes when keeping it does not fit"
        );
    }

    #[test]
    fn a_recent_row_tells_the_whole_path_and_hits_in_the_column() {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let mut out = Vec::new();
        let mut hits = Vec::new();
        let rect = Viewport {
            x: 0.0,
            y: 0.0,
            width: 660.0,
            height: 800.0,
        };
        let deep = std::path::PathBuf::from(
            "/private/var/folders/a-long-folder-name/another-long-folder-name/and-one-more/garden",
        );
        super::super::ui::begin_frame();
        build_home(
            &mut out,
            &mut atlas,
            rect,
            &Theme::default(),
            None,
            std::slice::from_ref(&deep),
            None,
            &[],
            None,
            false,
            &mut hits,
        );
        let row = hits
            .iter()
            .find(|h| h.action == HomeAction::OpenProject(deep.clone()))
            .expect("the recent folder is a row");
        let (x, width) = home_column(rect);
        assert!(row.rect.x + row.rect.width <= x + width + 8.0 + 0.01);
        let spots = super::super::ui::hotspots();
        let tip = spots
            .iter()
            .find(|s| s.rect == row.rect)
            .and_then(|s| s.tip.as_deref());
        assert_eq!(
            tip,
            Some(deep.to_str().unwrap()),
            "the tooltip is the whole path"
        );
    }

    #[test]
    fn home_offers_a_restart_once_the_app_on_disk_was_replaced() {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let mut out = Vec::new();
        let mut hits = Vec::new();
        let rect = Viewport {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 616.0,
        };
        for replaced in [false, true] {
            build_home(
                &mut out,
                &mut atlas,
                rect,
                &Theme::default(),
                None,
                &[],
                None,
                &[],
                None,
                replaced,
                &mut hits,
            );
            assert_eq!(
                hits.iter().any(|h| h.action == HomeAction::Restart),
                replaced
            );
        }
        // Last in Start, under the three ways to begin.
        let start: Vec<&HomeAction> = hits.iter().take(4).map(|h| &h.action).collect();
        assert_eq!(
            start,
            [
                &HomeAction::OpenFolder,
                &HomeAction::NewFile,
                &HomeAction::FindFile,
                &HomeAction::Restart
            ]
        );
    }

    #[test]
    fn a_tall_home_scrolls_to_its_last_row_and_no_further() {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let mut hits = Vec::new();
        // An 1100 by 600 window: the pane beside the sidebar and under the
        // toolbar and tabs.
        let rect = Viewport {
            x: 240.0,
            y: 116.0,
            width: 860.0,
            height: 440.0,
        };
        let recent: Vec<std::path::PathBuf> = (1..=HOME_RECENT)
            .map(|n| std::path::PathBuf::from(format!("/tmp/garden-{n}")))
            .collect();
        let mut view = HomeView::default();
        let mut draw = |view: &mut HomeView, hits: &mut Vec<HomeHit>| {
            let mut out = Vec::new();
            super::super::ui::begin_frame();
            build_home_view(
                &mut out,
                &mut atlas,
                rect,
                &Theme::default(),
                None,
                &recent,
                None,
                &[],
                None,
                false,
                view,
                hits,
            );
            out
        };
        let out = draw(&mut view, &mut hits);
        assert!(
            view.content > rect.height,
            "eight folders overflow the pane"
        );
        assert_eq!(view.scroll, 0.0);
        let bottom = rect.y + rect.height;
        assert!(
            out.iter()
                .all(|q| q.pos[1] >= rect.y && q.pos[1] + q.size[1] <= bottom + 0.01),
            "every glyph is cut to the pane"
        );
        let last = HomeAction::OpenProject(recent[HOME_RECENT - 1].clone());
        assert!(
            !hits.iter().any(|h| h.action == last),
            "the last recent row is below the pane and not a hit"
        );
        for spot in super::super::ui::hotspots() {
            assert!(spot.rect.y >= rect.y && spot.rect.y + spot.rect.height <= bottom + 0.01);
        }

        // Scrolling further than there is content stops at the end, where
        // the last row is on screen and the first has gone.
        view.scroll_by(10_000.0);
        let max = view.max_scroll();
        assert!(max > 0.0 && view.scroll == max);
        let _ = draw(&mut view, &mut hits);
        assert_eq!(view.scroll, max, "the content height is stable");
        let row = hits
            .iter()
            .find(|h| h.action == last)
            .expect("the last recent row is reachable");
        assert!(row.rect.y >= rect.y && row.rect.y + row.rect.height <= bottom + 0.01);
        assert!(!hits.iter().any(|h| h.action == HomeAction::OpenFolder));

        // A page back, then Home and End.
        view.page_by(-1.0);
        assert_eq!(view.scroll, (max - (rect.height - HOME_ROW)).max(0.0));
        view.to_top();
        assert_eq!(view.scroll, 0.0);
        view.to_end();
        assert_eq!(view.scroll, max);
        assert_eq!(view.report(), format!("{max:.0} of {max:.0}"));
    }

    #[test]
    fn a_tiny_pane_draws_nothing_rather_than_overflowing() {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let mut out = Vec::new();
        let mut hits = Vec::new();
        let rect = Viewport {
            x: 0.0,
            y: 0.0,
            width: 150.0,
            height: 100.0,
        };
        build_home(
            &mut out,
            &mut atlas,
            rect,
            &Theme::default(),
            None,
            &[],
            None,
            &[],
            None,
            false,
            &mut hits,
        );
        assert!(hits.is_empty());
        assert!(out.is_empty());
    }
}

#[cfg(test)]
mod palette_loading_tests {
    use super::*;

    fn built(rows: &[PaletteRow], loading: bool) -> Vec<GlyphInstance> {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let mut out = Vec::new();
        let window = Viewport {
            x: 0.0,
            y: 0.0,
            width: 1200.0,
            height: 800.0,
        };
        let rect = palette_rect(window, palette_rows_shown(rows.len(), loading));
        take_animating();
        build_palette(
            PaletteView {
                rows,
                heading: "Project files",
                empty: "No matching files",
                loading,
                placeholder: "",
                action: "Open",
                query: "",
                selected: 0,
                scroll: 0,
                cursor: 0,
                selection: None,
            },
            &mut atlas,
            rect,
            &Theme::dark(),
            &mut out,
        );
        out
    }

    #[test]
    fn an_empty_list_on_its_way_is_sketched_not_declared_empty() {
        // Still loading: rows breathe in the list, which has room for them.
        let while_loading = built(&[], true);
        assert!(take_animating(), "the sketched rows breathe");
        assert_eq!(palette_rows_shown(0, true), PALETTE_SKELETON_ROWS);
        // Loaded and empty: the words, and nothing moves.
        let empty = built(&[], false);
        assert!(!take_animating(), "an empty list is still");
        assert_eq!(palette_rows_shown(0, false), 0);
        assert_ne!(while_loading.len(), empty.len());
        // Rows in hand: the flag changes nothing, nothing is sketched.
        let row = PaletteRow {
            title: "main.rs".into(),
            ..Default::default()
        };
        let _ = built(std::slice::from_ref(&row), true);
        assert!(!take_animating());
        assert_eq!(palette_rows_shown(1, true), 1);
    }
}
