//! Turns a buffer and a viewport into a flat array of glyph quads.
//!
//! This runs every frame. The caller reuses the output `Vec`, and the ASCII
//! path reads rope chunks without collecting lines. Short Unicode lines use
//! cached CoreText shaping. Only visible lines are touched, which keeps a
//! 100MB file costing roughly the same as a small one.

use crate::markdown::{Block, Run, SpannedBlock, Style};
use crate::project::finder::{Finder, Match};
use crate::project::icons;
use crate::project::tree::Tree;
use crate::render::font::{Atlas, Face, display_width};
#[cfg(test)]
use crate::render::font::{ShapedLine, shape_input};
use crate::render::metal::GlyphInstance;
use crate::syntax::{Kind, Span};
use crate::text::buffer::Buffer;
use crate::text::documents::Documents;
#[cfg(test)]
use objc2_foundation::NSString;

use crate::text::columns::{TAB_WIDTH, advance};

/// A rectangle of the window, in logical points, that something draws into.
///
/// Panels need an origin now that the sidebar exists; the editor no longer
/// starts at x = 0.
#[derive(Clone, Copy, Debug)]
pub struct Viewport {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Viewport {
    pub fn new(width: f32, height: f32) -> Self {
        Viewport {
            x: 0.0,
            y: 0.0,
            width,
            height,
        }
    }

    /// Whether a point is inside, edges on the top and left included.
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    /// Cuts `height` off the top, clamped to what there is. Returns the
    /// piece cut off and what is left under it.
    pub fn split_top(&self, height: f32) -> (Viewport, Viewport) {
        let height = height.clamp(0.0, self.height);
        (
            Viewport { height, ..*self },
            Viewport {
                y: self.y + height,
                height: self.height - height,
                ..*self
            },
        )
    }

    /// The same rect moved and shrunk from the left, for splitting off a panel.
    pub fn inset_left(&self, amount: f32) -> Self {
        Viewport {
            x: self.x + amount,
            y: self.y,
            width: (self.width - amount).max(0.0),
            height: self.height,
        }
    }

    /// A panel of `width` taken off the left edge.
    pub fn take_left(&self, width: f32) -> Self {
        Viewport {
            x: self.x,
            y: self.y,
            width: width.min(self.width),
            height: self.height,
        }
    }
}

impl Viewport {
    /// How many whole lines fit.
    pub fn rows(&self, line_height: f32) -> usize {
        if line_height <= 0.0 {
            return 0;
        }
        (self.height / line_height).floor().max(0.0) as usize
    }

    /// How many whole characters fit beside the gutter.
    pub fn columns(&self, advance: f32, gutter: f32) -> usize {
        if advance <= 0.0 {
            return 0;
        }
        ((self.width - gutter) / advance).floor().max(0.0) as usize
    }
}

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
/// Project card, the Explorer / Source Control switcher, then the row of
/// file actions.
pub const SIDEBAR_HEADER_HEIGHT: f32 = 70.0;
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
        let mut options = [close; 4];
        for (index, slot) in options.iter_mut().enumerate() {
            let x = chips_right - CHIP * (4.0 - index as f32) - GAP * (3.0 - index as f32);
            *slot = inset(
                Viewport {
                    x,
                    width: CHIP,
                    ..row(0.0)
                },
                control,
            );
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
    /// The text itself: line numbers and lines, nothing else.
    pub text: Viewport,
    /// Across the bottom of the whole window.
    pub status: Viewport,
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
            sidebar,
            tabs,
            breadcrumbs,
            find,
            response,
            terminal,
            text,
            status,
            panes,
            others,
        }
    }

    /// A point in the window, as a point relative to the text area, which is
    /// what [`offset_at_point`] takes.
    pub fn to_text(&self, x: f32, y: f32) -> (f32, f32) {
        (x - self.text.x, y - self.text.y)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub background: [f64; 4],
    pub text: [f32; 4],
    pub gutter_text: [f32; 4],
    pub cursor: [f32; 4],
    pub current_line: [f32; 4],
    pub selection: [f32; 4],
    /// The hairline at each indent level in a line's leading whitespace.
    pub indent_guide: [f32; 4],
    /// The wash behind a bracket at the caret and its partner.
    pub bracket_match: [f32; 4],
    pub sidebar_background: [f32; 4],
    pub sidebar_text: [f32; 4],
    pub sidebar_directory: [f32; 4],
    pub sidebar_selected: [f32; 4],
    pub divider: [f32; 4],
    pub find_background: [f32; 4],
    pub find_match: [f32; 4],
    pub palette_background: [f32; 4],
    pub palette_selected: [f32; 4],
    pub palette_hit: [f32; 4],
    pub palette_border: [f32; 4],
    /// Dims the editor behind a floating panel. Without it the palette reads
    /// as a rectangle that fell on the screen rather than as a modal.
    pub scrim: [f32; 4],
    pub status_background: [f32; 4],
    pub status_text: [f32; 4],
    /// The current line's own number, which is the one you actually read.
    pub gutter_text_active: [f32; 4],
    pub tab_hover: [f32; 4],
    pub tab_dirty: [f32; 4],
    /// One device pixel. AppKit never draws a two-pixel hairline.
    pub hairline: [f32; 4],
    /// The user's macOS accent colour, read at startup.
    pub accent: [f32; 4],

    // Source control. A diff was previously coloured with `syn_string` for
    // additions and `syn_constant` for removals; the latter is the tan used
    // for constants, which in this palette reads as another shade of green.
    // Added and removed have to be told apart at a glance, so they get their
    // own pair, plus the bands behind them that carry the shape of the change
    // even where the text is short.
    pub diff_added: [f32; 4],
    pub diff_removed: [f32; 4],
    /// Gutter mark for a line that differs from HEAD without being new.
    pub diff_modified: [f32; 4],
    pub diff_added_band: [f32; 4],
    pub diff_removed_band: [f32; 4],
    pub tab_bar: [f32; 4],
    pub tab_active: [f32; 4],
    pub tab_text: [f32; 4],
    pub tab_text_inactive: [f32; 4],

    // Syntax. One colour per highlight kind, so adding a kind is a compile
    // error here rather than a silently black token.
    pub syn_keyword: [f32; 4],
    pub syn_function: [f32; 4],
    pub syn_type: [f32; 4],
    pub syn_string: [f32; 4],
    pub syn_number: [f32; 4],
    pub syn_comment: [f32; 4],
    pub syn_constant: [f32; 4],
    pub syn_attribute: [f32; 4],
    pub syn_operator: [f32; 4],
    pub syn_punctuation: [f32; 4],
    pub syn_variable: [f32; 4],
    pub syn_property: [f32; 4],

    // Markdown preview
    pub md_heading: [f32; 4],
    pub md_code_background: [f32; 4],
    pub md_quote_bar: [f32; 4],
    pub md_rule: [f32; 4],
}

impl Theme {
    /// Colour for a highlight kind.
    pub fn syntax(&self, kind: Kind) -> [f32; 4] {
        match kind {
            Kind::Keyword => self.syn_keyword,
            Kind::Function => self.syn_function,
            Kind::Type => self.syn_type,
            Kind::String => self.syn_string,
            Kind::Number => self.syn_number,
            Kind::Comment => self.syn_comment,
            Kind::Constant => self.syn_constant,
            Kind::Attribute => self.syn_attribute,
            Kind::Operator => self.syn_operator,
            Kind::Punctuation => self.syn_punctuation,
            Kind::Variable => self.syn_variable,
            Kind::Property => self.syn_property,
        }
    }
}

impl Theme {
    /// The dark appearance, which is also the default.
    pub fn dark() -> Theme {
        Theme::default()
    }

    /// Whether this is the dark table, for anything that has to say so.
    pub fn is_dark(&self) -> bool {
        self.background[0] < 0.5
    }

    /// Graphite in daylight: the same surfaces one step apart, the same
    /// mint accent darkened until it reads on white, and the syntax hues
    /// brought down to the contrast the dark ones have on graphite.
    pub fn light() -> Theme {
        let accent = [0.122, 0.541, 0.388, 1.0];
        let text = [0.118, 0.137, 0.137, 1.0];
        let dim = [0.400, 0.447, 0.435, 1.0];
        Theme {
            background: [0.969, 0.973, 0.973, 1.0],
            sidebar_background: [0.925, 0.933, 0.933, 1.0],
            find_background: [0.925, 0.933, 0.933, 1.0],
            status_background: [0.925, 0.933, 0.933, 1.0],
            palette_background: [1.000, 1.000, 1.000, 1.0],
            tab_bar: [0.925, 0.933, 0.933, 1.0],
            tab_active: [0.969, 0.973, 0.973, 1.0],
            tab_hover: [0.886, 0.898, 0.898, 1.0],
            divider: [0.835, 0.851, 0.851, 1.0],
            hairline: [0.867, 0.882, 0.882, 1.0],
            palette_border: [0.725, 0.788, 0.761, 1.0],

            accent,
            cursor: accent,
            diff_added: [0.184, 0.561, 0.306, 1.0],
            diff_removed: [0.784, 0.271, 0.231, 1.0],
            diff_modified: [0.722, 0.525, 0.043, 1.0],
            diff_added_band: [0.184, 0.561, 0.306, 0.14],
            diff_removed_band: [0.784, 0.271, 0.231, 0.12],

            text,
            gutter_text: [0.541, 0.592, 0.576, 1.0],
            gutter_text_active: accent,
            sidebar_text: [0.294, 0.341, 0.329, 1.0],
            sidebar_directory: text,
            status_text: dim,
            tab_text: text,
            tab_text_inactive: dim,
            tab_dirty: [0.761, 0.490, 0.055, 1.0],
            palette_hit: accent,

            current_line: [0.000, 0.000, 0.000, 0.045],
            selection: [0.122, 0.541, 0.388, 0.18],
            indent_guide: [0.000, 0.000, 0.000, 0.09],
            bracket_match: [0.122, 0.541, 0.388, 0.22],
            find_match: [0.886, 0.643, 0.227, 0.30],
            sidebar_selected: [0.122, 0.541, 0.388, 0.12],
            palette_selected: [0.122, 0.541, 0.388, 0.12],
            scrim: [0.000, 0.000, 0.000, 0.18],

            syn_keyword: [0.486, 0.227, 0.929, 1.0],
            syn_function: [0.114, 0.306, 0.847, 1.0],
            syn_type: [0.059, 0.463, 0.431, 1.0],
            syn_property: [0.055, 0.455, 0.565, 1.0],
            syn_string: [0.247, 0.490, 0.165, 1.0],
            syn_number: [0.761, 0.255, 0.047, 1.0],
            syn_constant: [0.631, 0.384, 0.027, 1.0],
            syn_attribute: [0.745, 0.094, 0.365, 1.0],
            syn_variable: text,
            syn_operator: [0.310, 0.357, 0.345, 1.0],
            syn_punctuation: [0.420, 0.467, 0.451, 1.0],
            syn_comment: [0.490, 0.537, 0.522, 1.0],

            md_heading: text,
            md_code_background: [0.000, 0.000, 0.000, 0.05],
            md_quote_bar: [0.725, 0.788, 0.761, 1.0],
            md_rule: [0.835, 0.851, 0.851, 1.0],
        }
    }
}

#[cfg(test)]
mod theme_tests {
    use super::*;

    fn luminance(c: [f32; 4]) -> f32 {
        0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
    }

    /// Every opaque foreground has to stand off its surface in both tables:
    /// a colour picked for graphite that vanishes on white is the kind of
    /// slip a screenshot finds after the release.
    #[test]
    fn both_tables_keep_text_and_syntax_off_the_background() {
        for theme in [Theme::dark(), Theme::light()] {
            let bg = luminance([
                theme.background[0] as f32,
                theme.background[1] as f32,
                theme.background[2] as f32,
                1.0,
            ]);
            let foregrounds = [
                theme.text,
                theme.gutter_text,
                theme.status_text,
                theme.sidebar_text,
                theme.accent,
                theme.syn_keyword,
                theme.syn_function,
                theme.syn_type,
                theme.syn_property,
                theme.syn_string,
                theme.syn_number,
                theme.syn_constant,
                theme.syn_attribute,
                theme.syn_operator,
                theme.syn_punctuation,
                theme.syn_comment,
                theme.diff_added,
                theme.diff_removed,
                theme.diff_modified,
            ];
            for fg in foregrounds {
                assert!(
                    (luminance(fg) - bg).abs() > 0.3,
                    "{fg:?} is too close to a background of luminance {bg}"
                );
            }
        }
        assert!(Theme::dark().is_dark());
        assert!(!Theme::light().is_dark());
    }
}

impl Default for Theme {
    /// Graphite: neutral surfaces, restrained syntax and a mint focus accent.
    fn default() -> Self {
        Theme {
            // Surfaces
            background: [0.098, 0.110, 0.114, 1.0],
            sidebar_background: [0.125, 0.141, 0.145, 1.0],
            find_background: [0.125, 0.141, 0.145, 1.0],
            status_background: [0.125, 0.141, 0.145, 1.0],
            palette_background: [0.161, 0.180, 0.184, 1.0],
            tab_bar: [0.125, 0.141, 0.145, 1.0],
            tab_active: [0.098, 0.110, 0.114, 1.0],
            tab_hover: [0.161, 0.180, 0.184, 1.0],
            divider: [0.188, 0.212, 0.216, 1.0],
            hairline: [0.157, 0.180, 0.180, 1.0],
            palette_border: [0.325, 0.388, 0.357, 1.0],

            // Focus accent shared by selection, tabs and the caret.
            accent: [0.651, 0.867, 0.761, 1.0],
            diff_added: [0.596, 0.812, 0.624, 1.0],
            diff_removed: [0.886, 0.612, 0.588, 1.0],
            diff_modified: [0.886, 0.643, 0.227, 1.0],
            diff_added_band: [0.353, 0.702, 0.451, 0.13],
            diff_removed_band: [0.867, 0.396, 0.365, 0.13],
            cursor: [0.651, 0.867, 0.761, 1.0],

            // Text
            text: [0.863, 0.886, 0.875, 1.0],
            gutter_text: [0.447, 0.498, 0.471, 1.0],
            gutter_text_active: [0.651, 0.867, 0.761, 1.0],
            sidebar_text: [0.651, 0.694, 0.671, 1.0],
            sidebar_directory: [0.863, 0.886, 0.875, 1.0],
            status_text: [0.569, 0.612, 0.592, 1.0],
            tab_text: [0.863, 0.886, 0.875, 1.0],
            tab_text_inactive: [0.569, 0.612, 0.592, 1.0],
            tab_dirty: [0.886, 0.643, 0.227, 1.0],
            palette_hit: [0.651, 0.867, 0.761, 1.0],

            // Washes
            current_line: [1.000, 1.000, 1.000, 0.042],
            selection: [0.651, 0.867, 0.761, 0.18],
            indent_guide: [1.000, 1.000, 1.000, 0.10],
            bracket_match: [0.651, 0.867, 0.761, 0.26],
            find_match: [0.886, 0.643, 0.227, 0.16],
            sidebar_selected: [0.651, 0.867, 0.761, 0.12],
            palette_selected: [0.651, 0.867, 0.761, 0.12],
            scrim: [0.000, 0.000, 0.000, 0.28],

            // Syntax. Eight hue families that stay apart at the code size on
            // the graphite background: names are cool (violet, blue, mint,
            // cyan), literals are warm (green, orange, amber, pink), and the
            // machinery is neutral, ranked by how often you read it. The
            // earlier palette kept every hue at the same low saturation and
            // lightness, and a `use` line and a `pub struct` line read as
            // one grey in a screenshot.
            syn_keyword: [0.780, 0.573, 0.918, 1.0],
            syn_function: [0.510, 0.667, 1.000, 1.0],
            syn_type: [0.498, 0.820, 0.761, 1.0],
            syn_property: [0.537, 0.867, 1.000, 1.0],
            syn_string: [0.647, 0.839, 0.541, 1.0],
            syn_number: [0.965, 0.639, 0.357, 1.0],
            syn_constant: [0.914, 0.769, 0.416, 1.0],
            syn_attribute: [0.941, 0.639, 0.788, 1.0],
            syn_variable: [0.863, 0.886, 0.875, 1.0],
            syn_operator: [0.639, 0.710, 0.686, 1.0],
            syn_punctuation: [0.541, 0.608, 0.584, 1.0],
            syn_comment: [0.494, 0.561, 0.529, 1.0],

            md_heading: [0.863, 0.886, 0.875, 1.0],
            md_code_background: [1.000, 1.000, 1.000, 0.045],
            md_quote_bar: [0.325, 0.388, 0.357, 1.0],
            md_rule: [0.188, 0.212, 0.216, 1.0],
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Quads emitted, cursor and highlights included.
    pub quads: usize,
    /// Lines actually laid out.
    pub lines: usize,
    /// Characters skipped because they are outside the ASCII fast path.
    pub unsupported: usize,
    /// Lines past the shaping limit that are not plain ASCII, so they are
    /// drawn a character at a time: accents and scripts may look wrong.
    pub unshaped: usize,
}

/// Lays out the visible region into `out`, **replacing its contents**.
///
/// Note the replacing. This clears `out` first, so it must be the *first*
/// thing drawn in a frame: anything appended before it is silently erased.
/// That is exactly how the tab bar came to be invisible for several commits
/// while producing quads perfectly happily every frame.
pub fn build(
    buffer: &Buffer,
    atlas: &mut Atlas,
    viewport: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) -> Stats {
    build_with_matches(buffer, atlas, viewport, theme, "", out)
}

/// As [`build`], additionally highlighting occurrences of `query`.
///
/// Matches are computed only for the visible byte range, so a search in a
/// 100MB file costs the same as one in a small file.
pub fn build_with_matches(
    buffer: &Buffer,
    atlas: &mut Atlas,
    viewport: Viewport,
    theme: &Theme,
    query: &str,
    out: &mut Vec<GlyphInstance>,
) -> Stats {
    build_full(buffer, atlas, viewport, theme, query, &[], out)
}

/// As [`build_with_matches`], additionally colouring by syntax `spans`.
///
/// Spans must be sorted by start, which `Highlighter::spans` guarantees.
/// Walking them alongside the text means colouring costs one cursor advance
/// per character rather than a lookup per character.
pub fn build_full(
    buffer: &Buffer,
    atlas: &mut Atlas,
    viewport: Viewport,
    theme: &Theme,
    query: &str,
    spans: &[Span],
    out: &mut Vec<GlyphInstance>,
) -> Stats {
    build_full_search(
        buffer, atlas, viewport, theme, query, None, spans, true, out,
    )
}

/// Draws search highlights supplied as UTF-8 byte ranges. This is used for
/// regex, whole-word and case-insensitive matches; `None` uses literal find.
/// `carets` false leaves the carets out: the off half of a blink.
#[allow(clippy::too_many_arguments)]
pub fn build_full_search(
    buffer: &Buffer,
    atlas: &mut Atlas,
    viewport: Viewport,
    theme: &Theme,
    query: &str,
    search_ranges: Option<&[std::ops::Range<usize>]>,
    spans: &[Span],
    carets: bool,
    out: &mut Vec<GlyphInstance>,
) -> Stats {
    out.clear();
    atlas.begin_frame();
    build_text_appending(
        buffer,
        atlas,
        viewport,
        theme,
        query,
        search_ranges,
        spans,
        true,
        carets,
        out,
    )
}

/// [`build_full_search`] without clearing the frame first: a second editor
/// pane drawn after the first. `focused` draws the carets and the current
/// line band; a pane without the keyboard shows neither. `carets` false
/// hides the carets of a focused pane for the off half of a blink.
#[allow(clippy::too_many_arguments)]
pub fn build_text_appending(
    buffer: &Buffer,
    atlas: &mut Atlas,
    viewport: Viewport,
    theme: &Theme,
    query: &str,
    search_ranges: Option<&[std::ops::Range<usize>]>,
    spans: &[Span],
    focused: bool,
    carets: bool,
    out: &mut Vec<GlyphInstance>,
) -> Stats {
    let m = atlas.metrics;
    let (cell_w, cell_h) = atlas.cell_size();
    let solid = atlas.solid_uv();
    // The row is taller than the glyph cell now, so glyphs are centred in it
    // while bands and highlights still span the whole row.
    let glyph_dy = m.glyph_dy(m.line_height);

    let rows = viewport.rows(m.line_height);
    if rows == 0 {
        return Stats::default();
    }

    let total_lines = buffer.rope.len_lines();
    let std::ops::Range {
        start: first,
        end: last,
    } = visible_lines(buffer, viewport, m.line_height);
    let offset = scroll_offset(buffer, m.line_height);
    let drawn_from = out.len();

    // Gutter is sized to the widest line number plus breathing room, so it
    // does not jitter as you scroll between 999 and 1000.
    let digits = digit_count(total_lines);
    let gutter_w = (digits as f32 + 2.0) * m.advance;
    let text_x = viewport.x + gutter_w;

    let (cursor_line, _) = buffer.cursor_position();
    // Everything in the text area is shifted left by the horizontal scroll.
    // The gutter deliberately is not: line numbers stay pinned.
    let scroll_x = buffer.scroll_column as f32 * m.advance;
    let mut stats = Stats::default();

    // Every selection, not just the primary: with multiple cursors each one
    // needs its own band.
    let selections: Vec<(usize, usize)> = buffer
        .selections()
        .into_iter()
        .filter(|(s, e)| e > s)
        .collect();
    let selection = buffer.selection();

    // Occurrences of the search query inside the visible range only.
    let visible_start = buffer.rope.line_to_byte(first);
    let visible_end = if last < total_lines {
        buffer.rope.line_to_byte(last)
    } else {
        buffer.rope.len_bytes()
    };
    let matches: Vec<std::ops::Range<usize>> = if let Some(ranges) = search_ranges {
        ranges
            .iter()
            .filter(|r| r.end > visible_start && r.start < visible_end)
            .cloned()
            .collect()
    } else if query.is_empty() {
        Vec::new()
    } else if !buffer.folds.is_empty() {
        // Line by line: the span from the first to the last row can hold
        // everything a fold hides.
        let mut lines: Vec<usize> = screen_rows(buffer, viewport, m.line_height)
            .iter()
            .map(|r| r.line)
            .collect();
        lines.dedup();
        lines
            .into_iter()
            .flat_map(|l| {
                let start = buffer.rope.line_to_byte(l);
                let end = if l + 1 < total_lines {
                    buffer.rope.line_to_byte(l + 1)
                } else {
                    buffer.rope.len_bytes()
                };
                buffer.rope.find_in(query, start..end)
            })
            .map(|at| at..at + query.len())
            .collect()
    } else {
        buffer
            .rope
            .find_in(query, visible_start..visible_end)
            .into_iter()
            .map(|at| at..at + query.len())
            .collect()
    };
    let mut shaped_carets = Vec::new();

    // Indent guides: one hairline per indent level, and blank lines carry
    // the guides of the block they sit in. The level is read from the
    // lines on screen, so a two-space file gets guides at two.
    // Only lines with a row on screen: a fold can put a million hidden
    // lines between the first and the last.
    let rows_on_screen = screen_rows(buffer, viewport, m.line_height);
    let mut shown_lines: Vec<usize> = rows_on_screen.iter().map(|r| r.line).collect();
    shown_lines.dedup();
    let indents: Vec<Option<usize>> = shown_lines
        .iter()
        .map(|&l| indent_columns(buffer, l))
        .collect();
    let unit = indent_unit(&indents);
    // Which visible lines could fold, from the indents already read: the
    // next non-blank line is deeper. Only a line with nothing non-blank
    // below it on screen asks the buffer, which reads further down.
    let foldable: Vec<bool> = (0..indents.len())
        .map(|i| match indents[i] {
            None => false,
            Some(base) => match indents[i + 1..].iter().flatten().next() {
                Some(next) => *next > base,
                None => buffer.can_fold(shown_lines[i]),
            },
        })
        .collect();
    let bracket = if focused { bracket_match(buffer) } else { None };
    let wrapping = buffer.wrap.is_some();
    let _ = (offset, first);

    for (row_index, row) in rows_on_screen.iter().enumerate() {
        let line = row.line;
        let y = row.y;

        let line_start = buffer.rope.line_to_byte(line);
        let line_end = if line + 1 < total_lines {
            buffer.rope.line_to_byte(line + 1)
        } else {
            buffer.rope.len_bytes()
        };

        // Shape bounded non-ASCII lines. The cached CoreText offsets are
        // shared by text, selection, search bands, and the caret.
        let shaped_line =
            atlas.shape_editor_line((buffer.id(), line), &buffer.rope, line_start..line_end);
        if shaped_line.is_none()
            && line_end - line_start > crate::render::font::MAX_SHAPED_LINE_BYTES
            && buffer.rope.byte_to_char(line_end) - buffer.rope.byte_to_char(line_start)
                != line_end - line_start
        {
            stats.unshaped += 1;
        }
        let offset_at = |byte: usize| -> f32 {
            if let Some(shaped) = &shaped_line {
                let bytes = &shaped.source_bytes;
                let index = bytes.partition_point(|&b| b < byte.saturating_sub(line_start));
                shaped.caret_offset(index.min(shaped.offsets.len() - 1))
            } else {
                visual_column_between(buffer, line_start, byte) as f32 * m.advance
            }
        };
        // A wrapped row is drawn as if the line began at the row's start.
        // Without wrapping this is the horizontal scroll alone.
        let row_x0 = if wrapping && !row.first {
            offset_at(row.start)
        } else {
            0.0
        };
        let scroll_x = scroll_x + row_x0;
        // What of the line this row shows, for bands and highlights.
        let (row_start, row_end) = (row.start, row.end);

        // Current-line highlight, behind everything on this row. Suppressed
        // while there is a selection: two overlapping washes on the same row
        // read as a rendering bug rather than as two pieces of information.
        if focused && line == cursor_line && selection.is_none() {
            out.push(GlyphInstance {
                pos: [viewport.x, y],
                size: [viewport.width, m.line_height],
                uv: solid,
                color: theme.current_line,
                ..Default::default()
            });
        }

        let shown_at = shown_lines.binary_search(&line).unwrap_or(0);
        let indent = match indents[shown_at] {
            Some(columns) => columns,
            None => blank_line_indent(buffer, line, &shown_lines, &indents),
        };
        let indent = if row.first { indent } else { 0 };
        for level in (0..indent).step_by(unit) {
            let x = text_x + level as f32 * m.advance - scroll_x;
            if x < text_x || x > viewport.x + viewport.width {
                continue;
            }
            out.push(GlyphInstance {
                pos: [x, y],
                size: [1.0, m.line_height],
                uv: solid,
                color: theme.indent_guide,
                ..Default::default()
            });
        }

        if let Some((open, close)) = bracket {
            for at in [open, close] {
                if at < row_start || at >= row_end {
                    continue;
                }
                let x = text_x + offset_at(at) - scroll_x;
                if x < text_x || x > viewport.x + viewport.width {
                    continue;
                }
                out.push(GlyphInstance {
                    pos: [x, y],
                    size: [m.advance, m.line_height],
                    uv: solid,
                    color: theme.bracket_match,
                    ..Default::default()
                });
            }
        }

        // Selection bands for this row, also behind the text.
        for &(sel_start, sel_end) in &selections {
            let from = sel_start.max(row_start);
            let to = sel_end.min(row_end);
            if from < to {
                let intervals = if let Some(shaped) = &shaped_line {
                    let extend = if row.last && sel_end > line_end && line + 1 < total_lines {
                        m.advance * 0.5
                    } else {
                        0.0
                    };
                    shaped.selection_intervals(
                        from - line_start,
                        to - line_start,
                        scroll_x..scroll_x + viewport.x + viewport.width - text_x,
                        extend,
                    )
                } else {
                    vec![(offset_at(from), offset_at(to))]
                };
                let interval_count = intervals.len();
                for (index, (left, right)) in intervals.into_iter().enumerate() {
                    let x0 = text_x + left.min(right) - scroll_x;
                    let mut width = (right - left).abs();
                    if shaped_line.is_none()
                        && row.last
                        && index + 1 == interval_count
                        && sel_end > line_end
                        && line + 1 < total_lines
                    {
                        width += m.advance * 0.5;
                    }
                    let clipped = x0.max(text_x);
                    let width = width - (clipped - x0);
                    if width > 0.0 && clipped < viewport.x + viewport.width {
                        out.push(GlyphInstance {
                            pos: [clipped, y],
                            size: [
                                width.min(viewport.x + viewport.width - clipped),
                                m.line_height,
                            ],
                            uv: solid,
                            color: theme.selection,
                            ..Default::default()
                        });
                    }
                }
            }
        }

        // Search matches, behind the text alongside the selection.
        if !matches.is_empty() {
            for range in &matches {
                let (at, end) = (range.start, range.end);
                if end <= row_start || at >= row_end {
                    continue;
                }
                let from = at.max(row_start);
                let to = end.min(row_end);
                let intervals = if let Some(shaped) = &shaped_line {
                    shaped.selection_intervals(
                        from - line_start,
                        to - line_start,
                        scroll_x..scroll_x + viewport.x + viewport.width - text_x,
                        0.0,
                    )
                } else {
                    vec![(offset_at(from), offset_at(to))]
                };
                for (left, right) in intervals {
                    let x0 = text_x + left.min(right) - scroll_x;
                    let width = (right - left).abs();
                    let clipped = x0.max(text_x);
                    let width = width - (clipped - x0);
                    if width > 0.0 && clipped < viewport.x + viewport.width {
                        out.push(GlyphInstance {
                            pos: [clipped, y],
                            size: [
                                width.min(viewport.x + viewport.width - clipped),
                                m.line_height,
                            ],
                            uv: solid,
                            color: theme.find_match,
                            ..Default::default()
                        });
                    }
                }
            }
        }

        // Line number, right-aligned against the gutter, on a line's first
        // row only.
        let number = line + 1;
        let label = if row.first {
            number.to_string()
        } else {
            String::new()
        };
        let label_x = viewport.x + gutter_w - m.advance * (label.len() as f32 + 1.0);
        for (i, ch) in label.chars().enumerate() {
            if let Some(slot) = atlas.slot_for(ch) {
                out.push(GlyphInstance {
                    pos: [label_x + i as f32 * m.advance, y + glyph_dy],
                    size: [cell_w, cell_h],
                    uv: slot.uv,
                    flags: slot.flags(),
                    color: if line == buffer.rope.byte_to_line(buffer.cursor()) {
                        theme.gutter_text_active
                    } else {
                        theme.gutter_text
                    },
                    ..Default::default()
                });
            }
        }

        // Folding: a chevron in the cell after the number, pointing right on
        // a folded line and down, faintly, on one that could fold. A folded
        // line ends in a marker standing for what it hides.
        if row.first {
            let folded = !buffer.folds.is_empty() && buffer.is_folded_at(line);
            if folded || foldable[shown_at] {
                let icon = if folded {
                    crate::project::icons::CHEVRON_RIGHT
                } else {
                    crate::project::icons::CHEVRON_DOWN
                };
                if let Some(slot) = atlas.slot_for(icon) {
                    let [r, g, b, a] = theme.gutter_text;
                    out.push(GlyphInstance {
                        pos: [
                            m.snap(viewport.x + gutter_w - m.advance * 0.95),
                            y + glyph_dy + cell_h * 0.15,
                        ],
                        size: [cell_w * slot.cells as f32 * 0.7, cell_h * 0.7],
                        uv: slot.uv,
                        flags: slot.flags(),
                        color: if folded {
                            theme.accent
                        } else {
                            [r, g, b, a * 0.5]
                        },
                        ..Default::default()
                    });
                }
            }
            if folded {
                let end = crate::text::wrap::line_end(&buffer.rope, line);
                let row_end_x = if row.last {
                    offset_at(end)
                } else {
                    offset_at(row_end)
                };
                let x = text_x + row_end_x - scroll_x + m.advance * 0.5;
                if x < viewport.x + viewport.width {
                    push_rounded_rect(
                        out,
                        Viewport {
                            x,
                            y: y + 3.0,
                            width: m.advance * 3.0,
                            height: m.line_height - 6.0,
                        },
                        4.0,
                        theme.tab_hover,
                    );
                    push_text(out, atlas, x + m.advance, y, "…", theme.status_text);
                }
            }
        }

        let source_quads = out.len();
        if let Some(shaped) = shaped_line {
            let utf16_bytes = &shaped.source_bytes;
            for glyph in shaped.visible_glyphs(
                scroll_x,
                scroll_x + viewport.x + viewport.width - text_x,
                cell_w * 2.0,
            ) {
                let byte = line_start + utf16_bytes[glyph.source_utf16.min(utf16_bytes.len() - 1)];
                if wrapping && (byte < row_start || byte >= row_end) {
                    continue;
                }
                let span_at = spans.partition_point(|s| s.end <= byte);
                let color = spans
                    .get(span_at)
                    .filter(|s| s.start <= byte && byte < s.end)
                    .map_or(theme.text, |s| theme.syntax(s.kind));
                let x = text_x + glyph.x - scroll_x;
                if x + cell_w * 2.0 <= text_x || x > viewport.x + viewport.width {
                    continue;
                }
                let Some(slot) = atlas.slot_for_shaped(&shaped, glyph) else {
                    stats.unsupported += 1;
                    continue;
                };
                out.push(GlyphInstance {
                    pos: [x, y + glyph_dy],
                    size: [cell_w * slot.cells as f32, cell_h],
                    uv: slot.uv,
                    flags: slot.flags(),
                    color,
                    ..Default::default()
                });
            }
            for quad in &mut out[source_quads..] {
                clip_horizontal(quad, text_x, viewport.x + viewport.width);
            }
            shaped_carets.push((row_index, shaped, row_x0));
            if row.first {
                stats.lines += 1;
            }
            continue;
        }

        // The line's text. Read straight from the rope's chunks so a long
        // line is not copied into a String first.
        // Include enough left overhang for the widest atlas cell.
        let margin = (cell_w * 2.0 / m.advance).ceil() as usize;
        // A wrapped row starts at its own first column; tabs still stop
        // where they would on the whole line.
        let base_column = if wrapping && !row.first {
            buffer.rope.visual_column(line_start..row_start)
        } else {
            0
        };
        let (mut byte, mut column) = if wrapping {
            (row_start, base_column)
        } else {
            buffer.rope.visual_seek(
                line_start..line_end,
                buffer.scroll_column.saturating_sub(margin),
            )
        };
        let mut span_at = spans.partition_point(|s| s.end <= byte);
        'line: for chunk in buffer.rope.chunks_in(byte..row_end) {
            for ch in chunk.chars() {
                // Advance past spans that ended before this character.
                while span_at < spans.len() && spans[span_at].end <= byte {
                    span_at += 1;
                }
                let color = spans
                    .get(span_at)
                    .filter(|s| s.start <= byte && byte < s.end)
                    .map_or(theme.text, |s| theme.syntax(s.kind));
                let advance_bytes = ch.len_utf8();
                match ch {
                    '\n' | '\r' => {
                        byte += advance_bytes;
                        continue;
                    }
                    '\t' => {
                        column = (column / TAB_WIDTH + 1) * TAB_WIDTH;
                        byte += advance_bytes;
                        continue;
                    }
                    _ => {}
                }

                let x = text_x + (column - base_column) as f32 * m.advance - (scroll_x - row_x0);
                if x > viewport.x + viewport.width {
                    break 'line;
                }
                // Scrolled off to the left: skip rather than draw under the
                // gutter. Still costs the walk, but not a quad.
                if x + cell_w <= text_x {
                    column += display_width(ch);
                    byte += advance_bytes;
                    continue;
                }

                match atlas.slot_for_fallback(ch) {
                    Some(slot) => out.push(GlyphInstance {
                        pos: [x, y + glyph_dy],
                        size: [cell_w * slot.cells as f32, cell_h],
                        uv: slot.uv,
                        flags: slot.flags(),
                        color,
                        ..Default::default()
                    }),
                    // Nothing on the system can draw it. Counted rather than
                    // silently dropped, so it is at least reportable.
                    None => stats.unsupported += 1,
                }
                column += display_width(ch);
                byte += advance_bytes;
            }
        }
        for quad in &mut out[source_quads..] {
            clip_horizontal(quad, text_x, viewport.x + viewport.width);
        }
        if row.first {
            stats.lines += 1;
        }
    }

    // Carets last, so they sit on top. One per cursor.
    for caret in buffer
        .caret_positions()
        .into_iter()
        .filter(|_| focused && carets)
    {
        let caret_line = buffer.rope.byte_to_line(caret);
        let Some(row_index) = rows_on_screen
            .iter()
            .position(|r| r.holds(caret_line, caret))
        else {
            continue;
        };
        let row = rows_on_screen[row_index];
        let line_start = buffer.rope.line_to_byte(caret_line);
        let x = if let Some((_, shaped, row_x0)) =
            shaped_carets.iter().find(|(index, ..)| *index == row_index)
        {
            let index = shaped
                .source_bytes
                .partition_point(|&byte| byte < caret - line_start);
            text_x + shaped.caret_offset(index.min(shaped.offsets.len() - 1)) - scroll_x - row_x0
        } else {
            let column = visual_column_between(buffer, line_start, caret)
                - visual_column_between(buffer, line_start, row.start);
            text_x + column as f32 * m.advance - scroll_x
        };
        let y = row.y;
        if x <= viewport.x + viewport.width && x >= text_x {
            out.push(GlyphInstance {
                pos: [x, y],
                size: [(m.advance * 0.15).max(1.0), m.line_height],
                uv: solid,
                color: theme.cursor,
                ..Default::default()
            });
        }
    }

    // The first and last lines are usually cut by the edges.
    for quad in &mut out[drawn_from..] {
        clip_vertical(quad, viewport.y, viewport.y + viewport.height);
    }

    if let Some(thumb) = scrollbar_thumb(buffer, viewport, m.line_height) {
        let [r, g, b, _] = theme.status_text;
        push_rounded_rect(out, thumb, SCROLLBAR_WIDTH / 2.0, [r, g, b, 0.4]);
    }

    stats.quads = out.len();
    atlas.finish_shaping_frame();
    stats
}

/// Draws the Git marks down the left edge of the gutter: a bar beside an
/// added or modified line, a short stub at the top of a line that lost the
/// lines above it. Appended after the text, in the gutter's first cell,
/// which the right-aligned line numbers never reach.
pub fn push_gutter_marks(
    out: &mut Vec<GlyphInstance>,
    atlas: &Atlas,
    buffer: &Buffer,
    viewport: Viewport,
    theme: &Theme,
    marks: &[crate::project::git::Mark],
) {
    use crate::project::git::MarkKind;
    let m = atlas.metrics;
    let rows = screen_rows(buffer, viewport, m.line_height);
    let (Some(top), Some(bottom)) = (rows.first(), rows.last()) else {
        return;
    };
    let (first, last) = (top.line, bottom.line + 1);
    let drawn_from = out.len();
    let x = viewport.x + 2.0;
    for mark in marks {
        // A line's rows: its bar runs down all of them.
        let mut on = rows.iter().filter(|r| r.line == mark.line);
        let (y, height) = match (on.next(), on.next_back()) {
            (Some(a), Some(b)) => (a.y, b.y + m.line_height - a.y),
            (Some(a), None) => (a.y, m.line_height),
            _ => (bottom.y + m.line_height, m.line_height),
        };
        match mark.kind {
            MarkKind::Added | MarkKind::Modified => {
                if mark.line < first || mark.line >= last {
                    continue;
                }
                let color = if mark.kind == MarkKind::Added {
                    theme.diff_added
                } else {
                    theme.diff_modified
                };
                push_rect(out, atlas, [x, y], [3.0, height], color);
            }
            MarkKind::Removed => {
                // Between two lines, so it may sit on the boundary just
                // past the last visible row, or past the last line.
                if mark.line < first || mark.line > last {
                    continue;
                }
                push_rect(out, atlas, [x, y - 1.5], [8.0, 3.0], theme.diff_removed);
            }
        }
    }
    for quad in &mut out[drawn_from..] {
        clip_vertical(quad, viewport.y, viewport.y + viewport.height);
    }
}

/// What a click on the home screen does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HomeAction {
    OpenFolder,
    NewFile,
    FindFile,
    OpenProject(std::path::PathBuf),
}

/// A clickable row on the home screen.
#[derive(Clone, Debug)]
pub struct HomeHit {
    pub rect: Viewport,
    pub action: HomeAction,
}

/// The home screen: what the editor column shows with nothing open.
///
/// Drawn natively rather than as a Markdown document, so it can have rows
/// you click, the project's recent folders, and the UI font instead of a
/// monospace table. Appends to `out`; the caller clears and paints the
/// background first, as for every other panel. `hits` receives the rows.
pub fn build_home(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    rect: Viewport,
    theme: &Theme,
    project: Option<&std::path::Path>,
    recent: &[std::path::PathBuf],
    hits: &mut Vec<HomeHit>,
) {
    const ROW: f32 = 30.0;
    const GAP: f32 = 18.0;
    hits.clear();
    if rect.width < 200.0 || rect.height < 120.0 {
        return;
    }
    let width = (rect.width - 112.0).clamp(240.0, 560.0);
    let x = rect.x + ((rect.width - width) / 2.0).clamp(28.0, 72.0);
    let right = x + width;
    let mut y = rect.y + (rect.height * 0.06).clamp(24.0, 48.0);
    let bottom = rect.y + rect.height - 16.0;
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

    // Start: the three things to do from here, each with its shortcut.
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
    for (glyph, label, keys, action) in actions {
        if y + ROW > bottom {
            return;
        }
        let row = Viewport {
            x: x - 8.0,
            y,
            width: width + 16.0,
            height: ROW,
        };
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

    // Recent: folders opened before, name first, path dimmed after it.
    let recent: Vec<&std::path::PathBuf> = recent
        .iter()
        .filter(|p| Some(p.as_path()) != project)
        .take(6)
        .collect();
    if y + 22.0 + ROW <= bottom {
        section(out, atlas, &mut y, "Recent");
        if recent.is_empty() {
            push_ui_text(
                out,
                atlas,
                line(y + 5.0),
                "Folders you open will be listed here.",
                dim,
            );
            y += ROW;
        }
        for path in recent {
            if y + ROW > bottom {
                break;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let parent = path
                .parent()
                .map(|p| shorten_home(&p.display().to_string()))
                .unwrap_or_default();
            let row = Viewport {
                x: x - 8.0,
                y,
                width: width + 16.0,
                height: ROW,
            };
            push_icon_centered(
                out,
                atlas,
                Viewport {
                    x,
                    y,
                    width: 24.0,
                    height: ROW,
                },
                icons::FOLDER,
                dim,
            );
            let name_w = (ui_text_width(atlas, &name) + 4.0).min(width * 0.5);
            push_ui_text(
                out,
                atlas,
                Viewport {
                    x: x + 32.0,
                    y: y + 5.0,
                    width: name_w,
                    height: 20.0,
                },
                &name,
                theme.text,
            );
            push_ui_text(
                out,
                atlas,
                Viewport {
                    x: x + 32.0 + name_w + 10.0,
                    y: y + 5.0,
                    width: (width - 32.0 - name_w - 10.0).max(0.0),
                    height: 20.0,
                },
                &parent,
                dim,
            );
            hits.push(HomeHit {
                rect: row,
                action: HomeAction::OpenProject(path.clone()),
            });
            y += ROW;
        }
        y += GAP;
    }

    // Keys: the rest of the shortcuts, two columns.
    let keys = [
        ("\u{2318}B", "Show or hide the sidebar"),
        ("\u{2318}\u{2325}G", "Source Control"),
        ("\u{2318}\u{21e7}F", "Find in project"),
        ("\u{2318}\u{21e7}C", "Claude Code"),
        ("\u{2318}O", "Open a file"),
        ("\u{2318}=", "Zoom in"),
    ];
    let rows_needed = keys.len().div_ceil(2) as f32 * 24.0;
    if y + 22.0 + rows_needed + 30.0 <= bottom {
        section(out, atlas, &mut y, "Keys");
        let column = width / 2.0;
        for (i, (combo, what)) in keys.iter().enumerate() {
            let cx = x + (i % 2) as f32 * column;
            let cy = y + (i / 2) as f32 * 24.0 + 2.0;
            push_ui_text(
                out,
                atlas,
                Viewport {
                    x: cx,
                    y: cy,
                    width: 64.0,
                    height: 20.0,
                },
                combo,
                dim,
            );
            push_ui_text(
                out,
                atlas,
                Viewport {
                    x: cx + 68.0,
                    y: cy,
                    width: column - 76.0,
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
    }
}

/// `/Users/name/...` as `~/...`, the way people read their own paths.
fn shorten_home(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_owned(),
    }
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
            &mut hits,
        );
        assert!(hits.is_empty());
        assert!(out.is_empty());
    }
}

/// Width of the editor's overlay scrollbar thumb, in points.
pub const SCROLLBAR_WIDTH: f32 = 5.0;
/// Air between the thumb and the pane's edges.
const SCROLLBAR_INSET: f32 = 4.0;
/// A thumb never shrinks below this, or a long file has nothing to grab.
const SCROLLBAR_MIN_THUMB: f32 = 24.0;

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
pub fn scrollbar_thumb(buffer: &Buffer, viewport: Viewport, line_height: f32) -> Option<Viewport> {
    let rows = viewport.rows(line_height);
    let total = buffer.rope.len_lines();
    if rows == 0 || total <= rows {
        return None;
    }
    let track = scrollbar_track(viewport);
    if track.height <= SCROLLBAR_MIN_THUMB {
        return None;
    }
    let height = (track.height * rows as f32 / total as f32).max(SCROLLBAR_MIN_THUMB);
    let max_scroll = (total - rows) as f32;
    let at = ((buffer.scroll_line as f32 + buffer.scroll_fraction) / max_scroll).clamp(0.0, 1.0);
    Some(Viewport {
        x: track.x,
        y: track.y + (track.height - height) * at,
        width: track.width,
        height,
    })
}

/// The scroll line that puts the thumb's top at `y` on the track, for a
/// drag: the inverse of [`scrollbar_thumb`].
pub fn scrollbar_line_at(buffer: &Buffer, viewport: Viewport, line_height: f32, y: f32) -> usize {
    let rows = viewport.rows(line_height);
    let total = buffer.rope.len_lines();
    let Some(thumb) = scrollbar_thumb(buffer, viewport, line_height) else {
        return 0;
    };
    let track = scrollbar_track(viewport);
    let run = track.height - thumb.height;
    if run <= 0.0 {
        return 0;
    }
    let at = ((y - track.y) / run).clamp(0.0, 1.0);
    ((total - rows) as f32 * at).round() as usize
}

/// Results the Cmd-P palette lists; more than fit, so the list scrolls.
pub const PALETTE_RESULTS: usize = 100;
pub const PALETTE_HEADER: f32 = 72.0;
pub const PALETTE_ROW: f32 = 44.0;
pub const PALETTE_FOOTER: f32 = 32.0;

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
        y: rect.y + 9.0,
        width: 30.0,
        height: 30.0,
    }
}

fn project_name(tree: &Tree) -> String {
    tree.root()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "crc".into())
}

/// Keep the beginning and end of a long folder name in a bounded toolbar.
fn project_label(atlas: &mut Atlas, name: &str, width: f32) -> String {
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
        let candidate: String = chars[..head]
            .iter()
            .chain(std::iter::once(&'…'))
            .chain(chars[chars.len() - tail..].iter())
            .collect();
        if ui_text_width(atlas, &candidate) <= width {
            low = keep;
        } else {
            high = keep - 1;
        }
    }
    let head = low.div_ceil(2);
    let tail = low / 2;
    chars[..head]
        .iter()
        .chain(std::iter::once(&'…'))
        .chain(chars[chars.len() - tail..].iter())
        .collect()
}

/// The project title and disclosure share this rectangle for drawing, clicks
/// and cursor shape. Its width follows the measured system-font label.
pub fn toolbar_project(tree: &Tree, atlas: &mut Atlas, rect: Viewport) -> Viewport {
    let x = toolbar_sidebar(rect).x + 38.0;
    let right = toolbar_terminal(rect).x;
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
        y: rect.y + 9.0,
        width,
        height: 30.0,
    }
}

/// Folder icon, gaps and chevron around the project name.
const PROJECT_CHROME: f32 = 62.0;

/// The Terminal button, left of the finder. Narrow windows give it up
/// before the project switcher; the menu and the shortcut remain.
pub fn toolbar_terminal(rect: Viewport) -> Viewport {
    let search = toolbar_search(rect);
    let width = if rect.width >= 620.0 { 92.0 } else { 0.0 };
    Viewport {
        x: search.x - width - if width > 0.0 { 8.0 } else { 0.0 },
        y: rect.y + 9.0,
        width,
        height: 30.0,
    }
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
        x: rect.x + rect.width - width - 18.0,
        y: rect.y + 9.0,
        width,
        height: 30.0,
    }
}

pub fn build_toolbar(
    tree: &Tree,
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
    let toggle = toolbar_sidebar(rect);
    push_rounded_rect(out, toggle, 6.0, theme.tab_hover);
    push_rounded_rect(
        out,
        Viewport {
            x: toggle.x + 8.0,
            y: toggle.y + 8.0,
            width: 14.0,
            height: 14.0,
        },
        2.0,
        theme.status_text,
    );
    push_rounded_rect(
        out,
        Viewport {
            x: toggle.x + 9.0,
            y: toggle.y + 9.0,
            width: 12.0,
            height: 12.0,
        },
        1.0,
        theme.tab_hover,
    );
    push_rect(
        out,
        atlas,
        [toggle.x + 13.0, toggle.y + 9.0],
        [1.0, 12.0],
        theme.status_text,
    );
    let search = toolbar_search(rect);
    let project = project_name(tree);
    let project_rect = toolbar_project(tree, atlas, rect);
    if project_rect.width > 0.0 {
        // A control like the sidebar toggle beside it: folder, name, chevron
        // on one pill, so it reads as the menu it is.
        push_rounded_rect(out, project_rect, 6.0, theme.tab_hover);
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
    let terminal = toolbar_terminal(rect);
    if terminal.width > 0.0 {
        push_rounded_rect(out, terminal, 6.0, theme.tab_hover);
        push_ui_text_centered(out, atlas, terminal, "Terminal", theme.text);
    }
    push_rounded_rect(out, search, 6.0, theme.tab_active);
    if search.width >= 220.0 {
        push_ui_text(
            out,
            atlas,
            Viewport {
                x: search.x + 12.0,
                width: (search.width - 24.0).max(0.0),
                ..search
            },
            "Find a file…",
            theme.status_text,
        );
        push_ui_text_right(
            out,
            atlas,
            Viewport {
                width: (search.width - 12.0).max(0.0),
                ..search
            },
            "⌘ P",
            theme.gutter_text,
        );
    } else {
        push_ui_text_centered(out, atlas, search, "Find", theme.status_text);
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
    push_rounded_rect(out, viewport, 12.0, theme.palette_border);
    push_rounded_rect(
        out,
        Viewport {
            x: viewport.x + 0.5,
            y: viewport.y + 0.5,
            width: (viewport.width - 1.0).max(0.0),
            height: (viewport.height - 1.0).max(0.0),
        },
        11.5,
        theme.palette_background,
    );
    let input = Viewport {
        x: viewport.x + 18.0,
        y: viewport.y + 8.0,
        width: (viewport.width - 74.0).max(0.0),
        height: 38.0,
    };
    let (shown, start) = ui_input_window(query, cursor);
    // Behind the glyphs, so the text stays readable on top of the band.
    if let Some(range) = &selection {
        let from = range.start.max(start).min(start + shown.len());
        let to = range.end.max(start).min(start + shown.len());
        if from < to {
            let x0 = ui_caret_x(atlas, &shown, from - start).min(input.width);
            let x1 = ui_caret_x(atlas, &shown, to - start).min(input.width);
            push_rect(
                out,
                atlas,
                [input.x + x0, input.y + 9.0],
                [(x1 - x0).max(0.0), 20.0],
                theme.selection,
            );
        }
    }
    push_ui_text(
        out,
        atlas,
        input,
        if query.is_empty() {
            placeholder
        } else {
            &shown
        },
        if query.is_empty() {
            theme.status_text
        } else {
            theme.text
        },
    );
    let caret_x = ui_caret_x(atlas, &shown, cursor.saturating_sub(start)).min(input.width);
    push_rect(
        out,
        atlas,
        [input.x + caret_x, input.y + 9.0],
        [1.0, 20.0],
        theme.cursor,
    );
    let escape = Viewport {
        x: viewport.x + viewport.width - 48.0,
        y: viewport.y + 16.0,
        width: 30.0,
        height: 22.0,
    };
    push_rounded_rect(out, escape, 4.0, theme.tab_hover);
    push_ui_text(
        out,
        atlas,
        Viewport {
            x: escape.x + 5.0,
            ..escape
        },
        "esc",
        theme.status_text,
    );
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
    if rows.is_empty() {
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
            push_rounded_rect(out, rect, 6.0, theme.palette_selected);
        }
        if let Some(icon) = item.icon {
            push_text(
                out,
                atlas,
                rect.x + 10.0,
                y + 12.0,
                &icon.to_string(),
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
        &format!("↑ ↓  Navigate       ↵  {action}       esc  Dismiss"),
        theme.status_text,
    );
}

/// Tab metrics in points, deliberately not multiples of the font cell: a
/// control's size is a property of the platform, not of the user's font.
const TAB_PADDING_X: f32 = 12.0;
const TAB_CLOSE_SLOT: f32 = 22.0;
const TAB_MIN_WIDTH: f32 = 120.0;
/// Side of the square hit box around the close glyph. Bigger than the glyph
/// on purpose: the target is what you aim at, not what you see.
const TAB_CLOSE_HIT: f32 = 18.0;
const TAB_MAX_WIDTH: f32 = 240.0;

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
    let (cell_w, cell_h) = atlas.cell_size();
    let solid = atlas.solid_uv();
    let hairline = 1.0 / m.scale;
    let glyph_dy = m.glyph_dy(viewport.height);

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

    let requested = first.min(docs.len().saturating_sub(1));
    let active = docs.active_index();
    let fits_from = |start: usize, target: usize| {
        let width: f32 = (start..=target)
            .map(|index| tab_width(docs, index, m.advance))
            .sum();
        width <= viewport.width
    };
    let mut start = requested;
    if active < start || !fits_from(start, active) || start == active {
        start = active;
        let mut used = tab_width(docs, active, m.advance);
        while start > 0 {
            let previous = tab_width(docs, start - 1, m.advance);
            if used + previous > viewport.width {
                break;
            }
            start -= 1;
            used += previous;
        }
    }

    let mut x = viewport.x;
    let mut active_span: Option<(f32, f32)> = None;

    for index in start.min(docs.len())..docs.len() {
        let active = index == docs.active_index();
        let dirty = docs.iter().nth(index).is_some_and(|b| b.is_dirty());
        let title = docs.title(index);

        // The file's icon leads the title: two cells and one of air.
        const ICON_CELLS: usize = 3;
        let icon = match docs.iter().nth(index).and_then(|b| b.path.as_deref()) {
            Some(path) => Some(icons::for_file(path)),
            None if docs.is_home() => Some(icons::Icon { glyph: icons::HOME }),
            None => None,
        };
        let lead = if icon.is_some() { ICON_CELLS } else { 0 };

        let label_cells: usize = title.chars().map(display_width).sum();
        let remaining = viewport.x + viewport.width - x;
        if remaining <= 0.0 {
            break;
        }
        let natural_width = tab_width(docs, index, m.advance);
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
        if let Some(icon) = icon
            && let Some(slot) = atlas.slot_for(icon.glyph)
        {
            out.push(GlyphInstance {
                pos: [label_x, viewport.y + glyph_dy],
                size: [cell_w * slot.cells as f32, cell_h],
                uv: slot.uv,
                flags: slot.flags(),
                // Dimmed with the rest of an inactive tab, so the active one
                // is still the one that stands out.
                color: if active {
                    theme.accent
                } else {
                    theme.tab_text_inactive
                },
                ..Default::default()
            });
        }
        label_x += lead as f32 * m.advance;
        let room = (((width - TAB_PADDING_X * 2.0 - TAB_CLOSE_SLOT) / m.advance).floor() as usize)
            .saturating_sub(lead);
        let shown: String = if label_cells > room && room > 1 {
            // Middle ellipsis keeps the extension visible, which is the half
            // people actually scan for.
            let keep = room - 1;
            let head = keep / 2;
            let tail = keep - head;
            let chars: Vec<char> = title.chars().collect();
            chars[..head]
                .iter()
                .chain(std::iter::once(&'\u{2026}'))
                .chain(chars[chars.len() - tail..].iter())
                .collect()
        } else {
            title
        };
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
        let marker = if hovered_here || (active && focused && !dirty) {
            Some((
                '\u{2715}',
                if hovered_here {
                    theme.tab_text
                } else {
                    theme.tab_text_inactive
                },
            ))
        } else if dirty {
            Some(('\u{2022}', theme.tab_dirty))
        } else {
            None
        };
        if let Some((glyph, color)) = marker
            && let Some(slot) = atlas.slot_for(glyph)
        {
            out.push(GlyphInstance {
                pos: [close_x, viewport.y + glyph_dy],
                size: [cell_w, cell_h],
                uv: slot.uv,
                flags: slot.flags(),
                color,
                ..Default::default()
            });
        }

        // A generous hit box, TAB_CLOSE_HIT points square and centred on the
        // glyph. A one-character target is the reason closing a tab felt bad:
        // the glyph is about 8pt wide and the finger is not.
        let centre_x = close_x + m.advance * 0.5;
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
pub fn sidebar_switcher(viewport: Viewport) -> (Viewport, Viewport) {
    let track = Viewport {
        x: viewport.x + 10.0,
        y: viewport.y + 8.0,
        width: (viewport.width - 20.0).max(0.0),
        height: 26.0,
    };
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

/// Side of each action button. Square, so a row of them is a row and not a
/// ragged line of differently sized pills.
pub const SIDEBAR_ACTION: f32 = 26.0;

/// The Explorer's action buttons, right to left, and the label beside them.
///
/// Returns the label rectangle and the buttons in drawing order: new file,
/// new folder, collapse all, refresh. They are identical squares on a single
/// pitch and the row ends flush with the switcher above it.
pub fn sidebar_actions(viewport: Viewport) -> (Viewport, [Viewport; 4]) {
    const GAP: f32 = 4.0;
    let row = Viewport {
        x: viewport.x + 10.0,
        y: viewport.y + 38.0,
        width: (viewport.width - 20.0).max(0.0),
        height: SIDEBAR_ACTION,
    };
    let right = row.x + row.width;
    let mut buttons = [row; 4];
    for (index, slot) in buttons.iter_mut().enumerate() {
        // Index 0 is the leftmost of the four, so it sits four pitches back.
        let from_right = 4.0 - index as f32;
        *slot = Viewport {
            x: right - from_right * SIDEBAR_ACTION - (from_right - 1.0) * GAP,
            width: SIDEBAR_ACTION,
            ..row
        };
    }
    let label = Viewport {
        width: (buttons[0].x - row.x - GAP * 2.0).max(0.0),
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

    // An empty panel with no explanation reads as a bug. Say what to do.
    if tree.root().is_none() {
        let lines = [
            "No folder open",
            "",
            "Cmd-Shift-O   open a folder",
            "Cmd-O         open a file",
        ];
        for (i, line) in lines.iter().enumerate() {
            push_text(
                out,
                atlas,
                viewport.x + m.advance,
                viewport.y + (i as f32 + 1.0) * m.line_height,
                line,
                if i == 0 {
                    theme.sidebar_directory
                } else {
                    theme.gutter_text
                },
            );
        }
        return 0;
    }

    let (explorer, source) = sidebar_switcher(viewport);
    push_rounded_rect(
        out,
        Viewport {
            width: explorer.width + source.width,
            ..explorer
        },
        6.0,
        theme.tab_active,
    );
    let active = if scm { source } else { explorer };
    push_rounded_rect(
        out,
        Viewport {
            x: active.x + 2.0,
            y: active.y + 2.0,
            width: (active.width - 4.0).max(0.0),
            height: active.height - 4.0,
        },
        5.0,
        theme.tab_hover,
    );
    push_ui_text_centered(
        out,
        atlas,
        explorer,
        "Explorer",
        if scm { theme.status_text } else { theme.text },
    );
    push_ui_text_centered(
        out,
        atlas,
        source,
        "Source Control",
        if scm { theme.text } else { theme.status_text },
    );
    if scm {
        // The panel draws the rest of the column.
        return 0;
    }

    // Where a new file would go, then the buttons that make one. The label
    // matters: the commands act on the selected item's directory, and without
    // it there is no way to know which that is before committing.
    let (label_rect, actions) = sidebar_actions(viewport);
    let where_to = tree
        .target_dir()
        .and_then(|dir| {
            tree.root()
                .and_then(|root| dir.strip_prefix(root).ok())
                .map(|rel| rel.to_string_lossy().into_owned())
        })
        .filter(|rel| !rel.is_empty())
        .unwrap_or_else(|| "Project root".into());
    push_ui_text(out, atlas, label_rect, &where_to, theme.gutter_text);
    for (rect, glyph) in actions.iter().zip([
        icons::NEW_FILE,
        icons::NEW_FOLDER,
        icons::COLLAPSE_ALL,
        icons::REFRESH,
    ]) {
        push_rounded_rect(out, *rect, 5.0, theme.tab_hover);
        push_icon_centered(out, atlas, *rect, glyph, theme.status_text);
    }

    // An inserted field is one more row; a rename takes its item's row.
    let total = tree.len() + usize::from(edit.as_ref().is_some_and(|e| !e.replaces));
    let first = tree.scroll.min(total.saturating_sub(1));
    let last = (first + rows).min(total);
    let mut drawn = 0;

    for visible in first..last {
        let y = viewport.y + SIDEBAR_HEADER_HEIGHT + (visible - first) as f32 * SIDEBAR_ROW_HEIGHT;
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

        if tree.selected == Some(i) {
            push_rounded_rect(
                out,
                Viewport {
                    x: viewport.x + 8.0,
                    y: y + 1.0,
                    width: (viewport.width - 16.0).max(0.0),
                    height: SIDEBAR_ROW_HEIGHT - 2.0,
                },
                5.0,
                theme.sidebar_selected,
            );
        }

        // Chevron, icon, name. Each icon is two cells wide, and a file gets
        // the chevron's two blank cells so names line up down a folder.
        let indent = 12.0 + entry.depth as f32 * 16.0;
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
        column += 16.0;
        draw_icon(
            icon.glyph,
            if tree.selected == Some(i) {
                theme.accent
            } else {
                dim(theme.status_text)
            },
            column,
        );
        column += 24.0;

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
fn push_sidebar_edit_row(
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
    let (shown, start) = ui_input_window(edit.text, edit.cursor);
    if let Some(range) = &edit.selection {
        let from = range.start.max(start).min(start + shown.len());
        let to = range.end.max(start).min(start + shown.len());
        if from < to {
            let x0 = ui_caret_x(atlas, &shown, from - start).min(input.width);
            let x1 = ui_caret_x(atlas, &shown, to - start).min(input.width);
            push_rect(
                out,
                atlas,
                [input.x + x0, input.y + 3.0],
                [(x1 - x0).max(0.0), input.height - 6.0],
                theme.selection,
            );
        }
    }
    push_ui_text(out, atlas, input, &shown, theme.text);
    let caret_x = ui_caret_x(atlas, &shown, edit.cursor.saturating_sub(start)).min(input.width);
    push_rect(
        out,
        atlas,
        [input.x + caret_x, input.y + 3.0],
        [1.0, input.height - 6.0],
        theme.cursor,
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
    /// The grab band on the terminal panel's top edge.
    TerminalDivider,
    /// A session tab in the terminal panel's header, its close button, and
    /// the button that starts a new shell.
    TerminalTab(usize),
    TerminalClose(usize),
    TerminalNew,
    /// The terminal's screen.
    Terminal,
    SidebarExplorer,
    SidebarSourceControl,
    /// New file, new folder, collapse all, refresh.
    SidebarAction(usize),
    SidebarRow(usize),
    /// The grab band on the divider between sidebar and editor.
    SidebarDivider,
    Tab(usize),
    TabClose(usize),
    /// Empty tab strip, right of the last tab.
    TabStrip,
    ResponseSegment(usize),
    /// Accept and Reject above a change proposed by Claude.
    ReviewAccept,
    ReviewReject,
    Find,
    Text,
    Status,
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
            Hit::TerminalDivider => "terminal.divider".into(),
            Hit::TerminalTab(i) => format!("terminal.tab.{i}"),
            Hit::TerminalClose(i) => format!("terminal.close.{i}"),
            Hit::TerminalNew => "terminal.new".into(),
            Hit::Terminal => "terminal".into(),
            Hit::SidebarExplorer => "sidebar.explorer".into(),
            Hit::SidebarSourceControl => "sidebar.source-control".into(),
            Hit::SidebarAction(i) => format!("sidebar.action.{i}"),
            Hit::SidebarRow(i) => format!("sidebar.row.{i}"),
            Hit::SidebarDivider => "sidebar.divider".into(),
            Hit::Tab(i) => format!("tab.{i}"),
            Hit::TabClose(i) => format!("tab.close.{i}"),
            Hit::TabStrip => "tab.strip".into(),
            Hit::ResponseSegment(i) => format!("response.segment.{i}"),
            Hit::ReviewAccept => "review.accept".into(),
            Hit::ReviewReject => "review.reject".into(),
            Hit::Find => "find".into(),
            Hit::Text => "text".into(),
            Hit::Status => "status".into(),
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

/// A line under `range` of `buffer`'s text, on every visible row it
/// touches: how a diagnostic is shown.
pub fn push_underline(
    out: &mut Vec<GlyphInstance>,
    atlas: &Atlas,
    buffer: &Buffer,
    text: Viewport,
    range: std::ops::Range<usize>,
    color: [f32; 4],
) {
    let m = atlas.metrics;
    let text_x = text.x + gutter_width(buffer, atlas);
    let scroll_x = buffer.scroll_column as f32 * m.advance;
    let start_line = buffer
        .rope
        .byte_to_line(range.start.min(buffer.rope.len_bytes()));
    let end_line = buffer
        .rope
        .byte_to_line(range.end.min(buffer.rope.len_bytes()));
    for row in screen_rows(buffer, text, m.line_height)
        .into_iter()
        .filter(|r| r.line >= start_line && r.line <= end_line)
    {
        let line = row.line;
        let line_start = buffer.rope.line_to_byte(line);
        let content_end = crate::text::wrap::line_end(&buffer.rope, line);
        let row_end = if row.last { content_end } else { row.end };
        let from = if line == start_line {
            range.start.max(row.start)
        } else {
            row.start
        };
        let to = if line == end_line {
            range.end.min(row_end)
        } else {
            row_end
        };
        // Not on this row of a wrapped line.
        if from > row_end
            || to < row.start
            || (from == to && !(row.start..=row_end).contains(&from))
        {
            continue;
        }
        let lead = visual_column_between(buffer, line_start, row.start);
        let c0 = (visual_column_between(buffer, line_start, from.min(to)) - lead) as f32;
        let c1 = (visual_column_between(buffer, line_start, to.max(from)) - lead) as f32;
        let x0 = (text_x + c0 * m.advance - scroll_x).max(text_x);
        // An empty range still gets a mark one cell wide, or it is invisible.
        let x1 = (text_x + c1.max(c0 + 1.0) * m.advance - scroll_x).min(text.x + text.width);
        if x1 <= x0 {
            continue;
        }
        let y = row.y + m.line_height - 2.0;
        if y < text.y || y + 1.5 > text.y + text.height {
            continue;
        }
        // A squiggle: short steps alternating up and down, which is what
        // every editor draws for a problem and what a straight line (a link,
        // a spelling underline) is not.
        const STEP: f32 = 2.0;
        let mut x = x0;
        let mut up = false;
        while x < x1 {
            let width = STEP.min(x1 - x);
            push_rect(
                out,
                atlas,
                [x, if up { y - 1.2 } else { y }],
                [width, 1.2],
                color,
            );
            x += STEP;
            up = !up;
        }
    }
}

/// One suggestion as the ribbon draws it.
pub struct Chip<'a> {
    pub label: &'a str,
    pub icon: char,
}

pub const RIBBON_HEIGHT: f32 = 30.0;
pub const RIBBON_WHY_HEIGHT: f32 = 22.0;
const CHIP_PAD: f32 = 9.0;
const CHIP_GAP: f32 = 2.0;
const CHIP_ICON: f32 = 18.0;
const RIBBON_MAX_CHIPS: usize = 7;

/// The signature of the call the caret is in, above the caret's line, with
/// the parameter being typed in the accent colour. Code, so monospace.
/// Returns the panel it drew.
pub fn build_signature(
    label: &str,
    active: Option<std::ops::Range<usize>>,
    caret: Viewport,
    text: Viewport,
    atlas: &mut Atlas,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) -> Viewport {
    let advance = atlas.metrics.advance;
    let line = atlas.metrics.line_height;
    let room = (((text.width - 32.0) / advance).max(8.0)) as usize;
    let shown: String = label.chars().take(room).collect();
    let width = shown.chars().count() as f32 * advance + 20.0;
    let height = line + 10.0;
    let x = (caret.x - 10.0).clamp(
        text.x + 8.0,
        (text.x + text.width - width - 8.0).max(text.x + 8.0),
    );
    let above = caret.y - height - 4.0;
    let y = if above >= text.y {
        above
    } else {
        caret.y + caret.height + 4.0
    };
    let panel = Viewport {
        x,
        y,
        width,
        height,
    };
    push_rounded_rect(out, panel, 6.0, theme.palette_border);
    push_rounded_rect(
        out,
        Viewport {
            x: panel.x + 0.5,
            y: panel.y + 0.5,
            width: (panel.width - 1.0).max(0.0),
            height: (panel.height - 1.0).max(0.0),
        },
        5.5,
        theme.palette_background,
    );
    let active = active.filter(|r| {
        r.end <= shown.len() && shown.is_char_boundary(r.start) && shown.is_char_boundary(r.end)
    });
    let parts: [(&str, [f32; 4]); 3] = match &active {
        Some(r) => [
            (&shown[..r.start], theme.status_text),
            (&shown[r.clone()], theme.accent),
            (&shown[r.end..], theme.status_text),
        ],
        None => [
            (&shown[..], theme.status_text),
            ("", theme.text),
            ("", theme.text),
        ],
    };
    let mut cx = panel.x + 10.0;
    for (part, color) in parts {
        push_text(out, atlas, cx, panel.y + 5.0, part, color);
        cx += part.chars().count() as f32 * advance;
    }
    panel
}

/// Completion as a ribbon, not a list box: the best guess as ghost text at
/// the caret, the alternatives as one row of chips under the line (above it
/// near the bottom of the view), and under them why the picked one is
/// offered. Nothing covers the lines around the caret beyond that row.
///
/// `word_x` is where the word being completed starts, so the chips line up
/// with it. Returns each drawn chip's rectangle with its index in `chips`,
/// for clicks.
#[allow(clippy::too_many_arguments)]
pub fn build_completion_ribbon(
    chips: &[Chip<'_>],
    selected: usize,
    why: &str,
    ghost: Option<&str>,
    caret: Viewport,
    word_x: f32,
    text: Viewport,
    atlas: &mut Atlas,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) -> Vec<(Viewport, usize)> {
    // The ghost: what Tab would add, dimmed, on the caret's own line.
    if let Some(ghost) = ghost.filter(|g| !g.is_empty()) {
        let room = ((text.x + text.width - caret.x) / atlas.metrics.advance).max(0.0) as usize;
        let shown: String = ghost.chars().take(room).collect();
        let [r, g, b, _] = theme.text;
        push_text(out, atlas, caret.x, caret.y, &shown, [r, g, b, 0.38]);
    }
    if chips.is_empty() {
        return Vec::new();
    }

    // Which chips fit: a window around the selected one.
    let widths: Vec<f32> = chips
        .iter()
        .map(|c| CHIP_PAD * 2.0 + CHIP_ICON + ui_text_width(atlas, c.label).min(260.0))
        .collect();
    let max_width = (text.width - 16.0).max(80.0);
    let mut first = selected.saturating_sub(RIBBON_MAX_CHIPS - 1);
    let fits = |first: usize, widths: &[f32]| {
        let mut total = 8.0;
        let mut last = first;
        for (i, w) in widths.iter().enumerate().skip(first).take(RIBBON_MAX_CHIPS) {
            if total + w > max_width && i > first {
                break;
            }
            total += w + CHIP_GAP;
            last = i;
        }
        (last, total)
    };
    let (mut last, mut width) = fits(first, &widths);
    while last < selected && first < selected {
        first += 1;
        (last, width) = fits(first, &widths);
    }
    let why_height = if why.is_empty() {
        0.0
    } else {
        RIBBON_WHY_HEIGHT
    };
    let width = width.max(if why.is_empty() {
        0.0
    } else {
        ui_text_width(atlas, why).min(max_width) + 20.0
    });
    let height = RIBBON_HEIGHT + why_height;
    let x = (word_x - 12.0).clamp(
        text.x + 8.0,
        (text.x + text.width - width - 8.0).max(text.x + 8.0),
    );
    let below = caret.y + caret.height + 4.0;
    let y = if below + height <= text.y + text.height || caret.y - height - 4.0 < text.y {
        below
    } else {
        caret.y - height - 4.0
    };
    let panel = Viewport {
        x,
        y,
        width,
        height,
    };

    push_rounded_rect(
        out,
        Viewport {
            x: panel.x - 3.0,
            y: panel.y + 2.0,
            width: panel.width + 6.0,
            height: panel.height + 3.0,
        },
        10.0,
        [0.0, 0.0, 0.0, 0.16],
    );
    push_rounded_rect(out, panel, 8.0, theme.palette_border);
    push_rounded_rect(
        out,
        Viewport {
            x: panel.x + 0.5,
            y: panel.y + 0.5,
            width: (panel.width - 1.0).max(0.0),
            height: (panel.height - 1.0).max(0.0),
        },
        7.5,
        theme.palette_background,
    );

    let (cell_w, cell_h) = atlas.cell_size();
    let glyph_dy = atlas.metrics.glyph_dy(RIBBON_HEIGHT - 6.0);
    let mut hits = Vec::new();
    let mut cx = panel.x + 4.0;
    for (index, chip) in chips.iter().enumerate().skip(first).take(last + 1 - first) {
        let rect = Viewport {
            x: cx,
            y: panel.y + 3.0,
            width: widths[index],
            height: RIBBON_HEIGHT - 6.0,
        };
        let picked = index == selected;
        if picked {
            push_rounded_rect(out, rect, 5.0, theme.palette_selected);
        }
        if let Some(slot) = atlas.slot_for(chip.icon) {
            out.push(GlyphInstance {
                pos: [
                    atlas.metrics.snap(rect.x + CHIP_PAD - 2.0),
                    rect.y + glyph_dy,
                ],
                size: [cell_w * slot.cells as f32 * 0.8, cell_h * 0.8],
                uv: slot.uv,
                flags: slot.flags(),
                color: if picked {
                    theme.accent
                } else {
                    theme.status_text
                },
                ..Default::default()
            });
        }
        push_ui_text(
            out,
            atlas,
            Viewport {
                x: rect.x + CHIP_PAD + CHIP_ICON,
                width: (rect.width - CHIP_PAD * 2.0 - CHIP_ICON).max(0.0),
                ..rect
            },
            chip.label,
            if picked {
                theme.text
            } else {
                theme.status_text
            },
        );
        hits.push((rect, index));
        cx += widths[index] + CHIP_GAP;
    }
    if !why.is_empty() {
        push_rect(
            out,
            atlas,
            [panel.x + 8.0, panel.y + RIBBON_HEIGHT],
            [panel.width - 16.0, 1.0],
            theme.palette_border,
        );
        push_ui_text(
            out,
            atlas,
            Viewport {
                x: panel.x + 12.0,
                y: panel.y + RIBBON_HEIGHT,
                width: panel.width - 20.0,
                height: RIBBON_WHY_HEIGHT,
            },
            why,
            theme.status_text,
        );
    }
    hits
}

/// Which sidebar row is at `y`, or `None` past the last row.
pub fn sidebar_row_at(tree: &Tree, _atlas: &Atlas, viewport: Viewport, y: f32) -> Option<usize> {
    if y < viewport.y + SIDEBAR_HEADER_HEIGHT || y >= viewport.y + viewport.height {
        return None;
    }
    let row = ((y - viewport.y - SIDEBAR_HEADER_HEIGHT) / SIDEBAR_ROW_HEIGHT).floor() as usize;
    let index = tree.scroll + row;
    (index < tree.len()).then_some(index)
}

#[derive(Clone, Debug)]
pub struct MarkdownHit {
    pub rect: Viewport,
    pub lines: std::ops::Range<usize>,
    pub copy: Option<(Viewport, String)>,
    /// Source byte offset and screen position for each visible caret stop.
    pub caret_stops: Vec<(usize, [f32; 2])>,
}

/// Renders parsed Markdown into `out`, replacing its contents.
///
/// One monospace atlas means structure is carried by colour, indentation and
/// spacing rather than by type size and weight. That is a real limitation:
/// proportional text with real bold and heading sizes needs a second atlas,
/// which is its own piece of work. What is here is legible and honest about
/// what it is.
#[allow(clippy::too_many_arguments)]
pub fn build_markdown(
    blocks: &[SpannedBlock],
    source: &str,
    active: Option<usize>,
    copied_block: Option<usize>,
    scroll: usize,
    atlas: &mut Atlas,
    viewport: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
    hits: &mut Vec<MarkdownHit>,
) -> usize {
    out.clear();
    atlas.begin_frame();
    build_markdown_appending(
        blocks,
        source,
        active,
        copied_block,
        scroll,
        atlas,
        viewport,
        theme,
        out,
        hits,
    )
}

/// [`build_markdown`] without clearing the frame first.
#[allow(clippy::too_many_arguments)]
pub fn build_markdown_appending(
    blocks: &[SpannedBlock],
    source: &str,
    active: Option<usize>,
    copied_block: Option<usize>,
    scroll: usize,
    atlas: &mut Atlas,
    viewport: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
    hits: &mut Vec<MarkdownHit>,
) -> usize {
    atlas.finish_shaping_frame();
    hits.clear();
    let m = atlas.metrics;
    let (cell_w, cell_h) = atlas.cell_size();
    let solid = atlas.solid_uv();
    let glyph_dy = m.glyph_dy(m.line_height);
    let hairline = 1.0 / m.scale;

    // Generous margins: a preview is for reading, not for editing, so it does
    // not want the gutter's density.
    let left = viewport.x + MD_MARGIN;
    let usable = (viewport.width - MD_MARGIN * 2.0).max(m.advance);
    let columns = (usable / m.advance).floor().max(1.0) as usize;

    let mut y = viewport.y + MD_MARGIN;
    let bottom = viewport.y + viewport.height;
    let mut rows_drawn = 0usize;

    let source_lines: Vec<&str> = source.lines().collect();
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(source.match_indices('\n').map(|(at, _)| at + 1))
        .collect();
    let active_line = active.map(|at| {
        source[..at.min(source.len())]
            .bytes()
            .filter(|b| *b == b'\n')
            .count()
    });
    for spanned in blocks.iter().skip(scroll) {
        if y >= bottom {
            break;
        }

        let start_y = y;
        let mut copy = None;
        let mut visual: Option<(String, Vec<[f32; 2]>)> = None;
        let mut visual_range: Option<std::ops::Range<usize>> = None;
        let block = &spanned.block;
        let pretty_editable = matches!(
            block,
            Block::Heading { .. }
                | Block::Paragraph { .. }
                | Block::ListItem { .. }
                | Block::Quote { .. }
                | Block::Code { .. }
                | Block::TableRow { .. }
        );
        if let Some(line) =
            active_line.filter(|line| spanned.lines.contains(line) && !pretty_editable)
        {
            let raw = &source_lines[spanned.lines.clone()];
            let height = m.line_height * raw.len().max(1) as f32 + m.line_height * 0.4;
            out.push(GlyphInstance {
                pos: [left, y],
                size: [usable, height],
                uv: solid,
                color: theme.md_code_background,
                ..Default::default()
            });
            y += m.line_height * 0.2;
            for (index, text) in raw.iter().enumerate() {
                push_text(out, atlas, left + m.advance, y, text, theme.text);
                if spanned.lines.start + index == line {
                    let line_start = source
                        .lines()
                        .take(line)
                        .map(|line| line.len() + 1)
                        .sum::<usize>();
                    let column = source[line_start..active.unwrap_or(line_start)]
                        .chars()
                        .count();
                    push_rect(
                        out,
                        atlas,
                        [left + m.advance * (column as f32 + 1.0), y],
                        [hairline.max(1.0), m.line_height],
                        theme.cursor,
                    );
                }
                y += m.line_height;
            }
            y += m.line_height * 0.2;
            hits.push(MarkdownHit {
                rect: Viewport {
                    x: left,
                    y: start_y,
                    width: usable,
                    height: y - start_y,
                },
                lines: spanned.lines.clone(),
                copy: None,
                caret_stops: Vec::new(),
            });
            rows_drawn += 1;
            continue;
        }

        match block {
            Block::Blank => {
                y += m.snap(m.line_height * 0.5);
            }

            Block::Rule => {
                out.push(GlyphInstance {
                    pos: [left, y + m.line_height * 0.5],
                    size: [usable, hairline],
                    uv: solid,
                    color: theme.md_rule,
                    ..Default::default()
                });
                y += m.line_height;
            }

            Block::Heading { level, runs } => {
                // Extra air above a heading, but not at the very top.
                if y > viewport.y + MD_MARGIN {
                    y += m.line_height * 0.6;
                }
                // Headings are shaped at their own size in the bold face,
                // not monospace bitmaps stretched by the quad size, which is
                // what made them soft.
                let size = MD_BODY_PT * md_heading_scale(*level);
                let (text, positions, end) = draw_prose(
                    out,
                    atlas,
                    runs,
                    left,
                    y,
                    usable,
                    size,
                    Face::Bold,
                    theme,
                    Some(theme.md_heading),
                    bottom,
                );
                visual = Some((text, positions));
                y = end;

                // A rule under the top two levels, as a document would have.
                if *level <= 2 {
                    out.push(GlyphInstance {
                        pos: [left, y],
                        size: [usable, hairline],
                        uv: solid,
                        color: theme.md_rule,
                        ..Default::default()
                    });
                    y += m.line_height * 0.35;
                }
                rows_drawn += 1;
            }

            Block::Paragraph { runs } => {
                let (text, positions, end) = draw_prose(
                    out,
                    atlas,
                    runs,
                    left,
                    y,
                    usable,
                    MD_BODY_PT,
                    Face::Regular,
                    theme,
                    None,
                    bottom,
                );
                visual = Some((text, positions));
                y = end;
                rows_drawn += 1;
            }

            Block::Quote { runs, depth } => {
                let start = y;
                // Each level of quoting steps in and gets its own bar, so a
                // nested quote reads as nested instead of showing a literal
                // ">" in the text.
                let indent = MD_QUOTE_INDENT * (*depth as f32 + 1.0);
                let (text, positions, end) = draw_prose(
                    out,
                    atlas,
                    runs,
                    left + indent,
                    y,
                    (usable - indent).max(0.0),
                    MD_BODY_PT,
                    Face::Regular,
                    theme,
                    Some(theme.syn_comment),
                    bottom,
                );
                visual = Some((text, positions));
                // One bar per level, each spanning however many rows the text
                // wrapped onto.
                for level in 0..=*depth {
                    out.push(GlyphInstance {
                        pos: [left + MD_QUOTE_INDENT * level as f32, start],
                        size: [2.0, (end - start).max(m.line_height)],
                        uv: solid,
                        color: theme.md_quote_bar,
                        ..Default::default()
                    });
                }
                y = end;
                rows_drawn += 1;
            }

            Block::Code { lang, lines } => {
                let height = m.line_height * (lines.len() + 1) as f32 + m.line_height * 0.4;
                out.push(GlyphInstance {
                    pos: [left, y],
                    size: [usable, height],
                    uv: solid,
                    color: theme.md_code_background,
                    ..Default::default()
                });
                y += m.line_height * 0.2;
                if !lang.is_empty() {
                    push_text(out, atlas, left + m.advance, y, lang, theme.gutter_text);
                }
                let copied = copied_block == Some(spanned.lines.start);
                let label = if copied { "✓ Copied" } else { "⧉ Copy" };
                let button_width = 10.0 * m.advance;
                let copy_x = left + usable - button_width;
                push_rect(
                    out,
                    atlas,
                    [copy_x, y],
                    [button_width, m.line_height],
                    theme.tab_hover,
                );
                push_text(
                    out,
                    atlas,
                    copy_x + m.advance,
                    y,
                    label,
                    if copied {
                        theme.syn_string
                    } else {
                        theme.accent
                    },
                );
                copy = Some((
                    Viewport {
                        x: copy_x,
                        y,
                        width: button_width,
                        height: m.line_height,
                    },
                    lines.join("\n"),
                ));
                y += m.line_height;
                let mut shown = String::new();
                let mut positions = Vec::new();
                let mut last_x = left + m.advance;
                let mut last_y = y;
                for (index, line) in lines.iter().enumerate() {
                    if y >= bottom {
                        break;
                    }
                    push_text(out, atlas, left + m.advance, y, line, theme.syn_string);
                    let mut column = 0usize;
                    for ch in line.chars() {
                        positions.push([left + m.advance * (column + 1) as f32, y]);
                        shown.push(ch);
                        column += display_width(ch);
                    }
                    last_x = left + m.advance * (column + 1) as f32;
                    last_y = y;
                    if index + 1 < lines.len() {
                        positions.push([last_x, y]);
                        shown.push('\n');
                    }
                    y += m.line_height;
                }
                positions.push([last_x, last_y]);
                visual = Some((shown, positions));
                let opening = source_lines.get(spanned.lines.start).is_some_and(|line| {
                    let trimmed = line.trim_start();
                    trimmed.starts_with("```") || trimmed.starts_with("~~~")
                });
                let first_body = spanned.lines.start + usize::from(opening);
                let after_body = first_body + lines.len();
                visual_range = Some(
                    *line_starts.get(first_body).unwrap_or(&source.len())
                        ..*line_starts.get(after_body).unwrap_or(&source.len()),
                );
                y += m.line_height * 0.2;
                rows_drawn += 1;
            }

            Block::ListItem {
                depth,
                number,
                task,
                runs,
            } => {
                let indent = left + (*depth as f32) * MD_LIST_INDENT;
                let marker = match (task, number) {
                    (Some(true), _) => "\u{2611} ".to_string(),
                    (Some(false), _) => "\u{2610} ".to_string(),
                    (None, Some(n)) => format!("{n}. "),
                    (None, None) => "\u{2022} ".to_string(),
                };
                push_text(out, atlas, indent, y, &marker, theme.accent);
                let text_x = indent + marker.chars().count() as f32 * m.advance;
                let (text, positions, end) = draw_prose(
                    out,
                    atlas,
                    runs,
                    text_x,
                    y,
                    (usable - (text_x - left)).max(0.0),
                    MD_BODY_PT,
                    Face::Regular,
                    theme,
                    None,
                    bottom,
                );
                visual = Some((text, positions));
                y = end;
                rows_drawn += 1;
            }

            Block::TableRow { cells, header } => {
                // Even columns. Measuring every row to fit content would need
                // a pass over the whole table, which the flat block list
                // deliberately does not give us.
                let per = (columns / cells.len().max(1)).max(4);
                let mut x = left;
                let mut shown = String::new();
                let mut positions = Vec::new();
                let mut last_x = x;
                for cell in cells {
                    let color = if *header {
                        Some(theme.md_heading)
                    } else {
                        None
                    };
                    draw_runs(out, atlas, cell, x, y, per.saturating_sub(1), theme, color);
                    let mut column = 0usize;
                    for ch in cell.iter().flat_map(|run| run.text.chars()) {
                        if column >= per.saturating_sub(1) {
                            break;
                        }
                        positions.push([x + column as f32 * m.advance, y]);
                        shown.push(ch);
                        column += display_width(ch);
                    }
                    last_x = x + column as f32 * m.advance;
                    x += per as f32 * m.advance;
                }
                positions.push([last_x, y]);
                visual = Some((shown, positions));
                y += m.line_height;
                if *header {
                    out.push(GlyphInstance {
                        pos: [left, y],
                        size: [usable, hairline],
                        uv: solid,
                        color: theme.md_rule,
                        ..Default::default()
                    });
                    y += m.line_height * 0.25;
                }
                rows_drawn += 1;
            }
        }
        let caret_stops = visual
            .map(|(text, positions)| {
                let range = visual_range.unwrap_or_else(|| {
                    *line_starts
                        .get(spanned.lines.start)
                        .unwrap_or(&source.len())
                        ..*line_starts.get(spanned.lines.end).unwrap_or(&source.len())
                });
                let offsets = crate::markdown::source_offsets_in_range(source, range, &text);
                let stops: Vec<_> = offsets.into_iter().zip(positions).collect();
                if let Some(at) = active
                    && active_line.is_some_and(|line| spanned.lines.contains(&line))
                {
                    let position = stops
                        .iter()
                        .take_while(|(offset, _)| *offset <= at)
                        .last()
                        .or_else(|| stops.first())
                        .map(|(_, point)| *point);
                    if let Some([x, y]) = position {
                        push_rect(
                            out,
                            atlas,
                            [x, y],
                            [hairline.max(1.0), m.line_height],
                            theme.cursor,
                        );
                    }
                }
                stops
            })
            .unwrap_or_default();
        hits.push(MarkdownHit {
            rect: Viewport {
                x: left,
                y: start_y,
                width: usable,
                height: (y - start_y).max(m.line_height * 0.5),
            },
            lines: spanned.lines.clone(),
            copy,
            caret_stops,
        });
        let _ = (cell_w, cell_h, glyph_dy);
    }

    rows_drawn
}

/// Draws runs on one line, truncating at `columns`.
#[allow(clippy::too_many_arguments)]
fn draw_runs(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    runs: &[Run],
    x: f32,
    y: f32,
    columns: usize,
    theme: &Theme,
    override_color: Option<[f32; 4]>,
) {
    let advance = atlas.metrics.advance;
    let mut column = 0usize;
    for run in runs {
        let color = override_color.unwrap_or_else(|| style_color(run.style, theme));
        for ch in run.text.chars() {
            if column >= columns {
                return;
            }
            push_text(
                out,
                atlas,
                x + column as f32 * advance,
                y,
                &ch.to_string(),
                color,
            );
            column += display_width(ch);
        }
    }
}

/// Draws wrapped prose and reports where every character landed.
///
/// One pass produces both, deliberately. The drawing and the caret map used
/// to be two functions walking the same runs with the same arithmetic; the
/// moment prose stopped being a fixed-width grid they would have disagreed,
/// and the caret in the live-edited line would sit beside the text instead of
/// in it. This is the same lesson `layout::Chrome` exists for.
///
/// Returns the visible text, one position per character, and the y after the
/// last line.
#[allow(clippy::too_many_arguments)]
fn draw_prose(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    runs: &[Run],
    x: f32,
    mut y: f32,
    width: f32,
    size_pt: f32,
    base: Face,
    theme: &Theme,
    override_color: Option<[f32; 4]>,
    bottom: f32,
) -> (String, Vec<[f32; 2]>, f32) {
    let m = atlas.metrics;
    let line_height = prose_line_height(m, size_pt);
    let mut text = String::new();
    let mut positions: Vec<[f32; 2]> = Vec::new();
    let mut cursor = 0.0f32;

    for run in runs {
        let color = override_color.unwrap_or_else(|| style_color(run.style, theme));
        let face = match style_face(run.style) {
            Face::Regular => base,
            Face::Bold => base.with_bold(true),
            Face::Italic => base.with_italic(true),
            Face::BoldItalic => base.with_bold(true).with_italic(true),
        };
        let code = style_is_code(run.style);
        // Wrap on whitespace, so words stay whole.
        for word in run.text.split_inclusive(' ') {
            let advance = if code {
                word.chars().map(display_width).sum::<usize>() as f32 * m.advance
            } else {
                prose_width(atlas, word, size_pt, face)
            };
            if cursor + advance > width && cursor > 0.0 {
                y += line_height;
                cursor = 0.0;
                if y >= bottom {
                    positions.push([x + cursor, y]);
                    return (text, positions, y);
                }
            }
            // Per-character positions come from the shaped word, so they are
            // the real glyph edges rather than a column count.
            let (shaped_pt, glyph_scale) = prose_fit(atlas, size_pt);
            let shaped = (!code)
                .then(|| atlas.shape_prose(word, shaped_pt, face))
                .flatten();
            let mut utf16 = 0usize;
            let mut column = 0usize;
            for ch in word.chars() {
                let dx = match &shaped {
                    Some(line) => line.caret_offset(utf16) * glyph_scale,
                    None => column as f32 * m.advance,
                };
                positions.push([x + cursor + dx, y]);
                text.push(ch);
                utf16 += ch.len_utf16();
                column += display_width(ch);
            }
            if code {
                let mut column = 0usize;
                for ch in word.chars() {
                    push_text(
                        out,
                        atlas,
                        x + cursor + column as f32 * m.advance,
                        y,
                        &ch.to_string(),
                        color,
                    );
                    column += display_width(ch);
                }
            } else {
                push_prose(
                    out,
                    atlas,
                    x + cursor,
                    y,
                    size_pt,
                    face,
                    word,
                    color,
                    (width - cursor).max(0.0),
                );
            }
            cursor += advance;
        }
    }
    positions.push([x + cursor, y]);
    (text, positions, y + line_height)
}

/// Row height for prose at `size_pt`, keeping the monospace line height as
/// the floor so body text still sits on the view's rhythm.
fn prose_line_height(m: crate::render::font::Metrics, size_pt: f32) -> f32 {
    (m.line_height * size_pt / MD_BODY_PT).max(m.line_height)
}

/// Point size of body prose in the Markdown view.
pub const MD_BODY_PT: f32 = 13.0;

/// Size of a heading relative to body prose.
fn md_heading_scale(level: u8) -> f32 {
    match level {
        1 => 1.9,
        2 => 1.55,
        3 => 1.3,
        4 => 1.15,
        _ => 1.05,
    }
}

fn style_color(style: Style, theme: &Theme) -> [f32; 4] {
    match style {
        // Weight and slant carry emphasis now, so body, bold and italic share
        // the reading colour instead of being told apart by brightness.
        Style::Plain | Style::Strong | Style::Emphasis | Style::StrongEmphasis => theme.text,
        Style::Code => theme.syn_string,
        Style::Link => theme.accent,
        Style::Image => theme.syn_type,
        Style::Strike => theme.gutter_text,
    }
}

/// The proportional face a run is drawn in.
fn style_face(style: Style) -> Face {
    match style {
        Style::Strong => Face::Bold,
        Style::Emphasis => Face::Italic,
        Style::StrongEmphasis => Face::BoldItalic,
        _ => Face::Regular,
    }
}

/// Code inside prose stays monospace; everything else is proportional.
fn style_is_code(style: Style) -> bool {
    matches!(style, Style::Code)
}

/// Draws a proportional run and returns the width it took.
///
/// The Markdown view was drawn entirely in the code font, headings included,
/// so a document read as terminal output. Prose goes through the system font
/// at a real size and weight; only code keeps the monospace cell grid.
#[allow(clippy::too_many_arguments)]
fn push_prose(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    x: f32,
    y: f32,
    size_pt: f32,
    face: Face,
    text: &str,
    color: [f32; 4],
    limit: f32,
) -> f32 {
    let (shaped_pt, scale) = prose_fit(atlas, size_pt);
    let Some(line) = atlas.shape_prose(text, shaped_pt, face) else {
        return 0.0;
    };
    let m = atlas.metrics;
    let (cw, ch) = atlas.cell_size();
    for glyph in &line.glyphs {
        if glyph.x * scale > limit {
            break;
        }
        let Some(slot) = atlas.slot_for_shaped(&line, glyph) else {
            continue;
        };
        let mut quad = GlyphInstance {
            pos: [m.snap(x + glyph.x * scale), y],
            size: [cw * slot.cells as f32 * scale, ch * scale],
            uv: slot.uv,
            flags: slot.flags(),
            color,
            ..Default::default()
        };
        clip_horizontal(&mut quad, x, x + limit);
        out.push(quad);
    }
    line.offsets.last().copied().unwrap_or(0.0) * scale
}

/// The size prose is actually rasterized at, and the factor the quads are
/// scaled by to reach the size that was asked for.
///
/// Glyphs live in cells sized for the monospace face, and a proportional face
/// at the same point size already fills one, so nothing larger than body text
/// can be rasterized without losing its ascenders. Body, bold and italic are
/// therefore crisp; a heading is shaped at the largest size that fits and its
/// quads are scaled the rest of the way, which is soft but keeps the document
/// hierarchy and the correct proportional metrics.
///
/// Lifting this means a second cell grid with taller cells, which changes the
/// allocator, the uv arithmetic and the eviction model. It is written up in
/// the handoff rather than smuggled into this change.
fn prose_fit(atlas: &mut Atlas, size_pt: f32) -> (f32, f32) {
    let cap = atlas.max_prose_pt();
    if size_pt <= cap {
        (size_pt, 1.0)
    } else {
        (cap, size_pt / cap)
    }
}

/// Width of a prose run without drawing it, at the size that was asked for.
pub fn prose_width(atlas: &mut Atlas, text: &str, size_pt: f32, face: Face) -> f32 {
    let (shaped_pt, scale) = prose_fit(atlas, size_pt);
    atlas
        .shape_prose(text, shaped_pt, face)
        .and_then(|line| line.offsets.last().copied())
        .unwrap_or(0.0)
        * scale
}

/// Margins and indents for the preview, in points.
const MD_MARGIN: f32 = 28.0;
const MD_LIST_INDENT: f32 = 20.0;
const MD_QUOTE_INDENT: f32 = 16.0;

/// Width of the line-number gutter, in logical points.
///
/// Public because mouse hit-testing needs the same number the renderer used.
/// Deriving it twice is how a click ends up one column off from where the
/// caret is drawn.
pub fn gutter_width(buffer: &Buffer, atlas: &Atlas) -> f32 {
    let digits = digit_count(buffer.rope.len_lines());
    (digits as f32 + 2.0) * atlas.metrics.advance
}

/// Byte offset of the character nearest a point in the view, for click and
/// drag positioning. `x` and `y` are logical points from the view's top-left.
pub fn offset_at_point(buffer: &Buffer, atlas: &Atlas, x: f32, y: f32) -> usize {
    let m = atlas.metrics;
    let total_lines = buffer.rope.len_lines();

    if buffer.row_mode() {
        // Horizontal scroll only exists without wrapping.
        let scrolled = buffer.scroll_column as f32 * m.advance;
        // Rows from the top of the text; y is measured from there too.
        let rows = screen_rows(
            buffer,
            Viewport {
                x: 0.0,
                y: 0.0,
                width: f32::MAX,
                height: y.max(0.0) + m.line_height,
            },
            m.line_height,
        );
        let Some(row) = rows
            .iter()
            .rev()
            .find(|r| r.y <= y)
            .or(rows.first())
            .copied()
        else {
            return buffer.rope.len_bytes();
        };
        let text_x = gutter_width(buffer, atlas);
        let line_start = buffer.rope.line_to_byte(row.line);
        let content_end = crate::text::wrap::line_end(&buffer.rope, row.line);
        let row_end = if row.last { content_end } else { row.end };
        if let Some(shaped) = atlas.cached_editor_line((buffer.id(), row.line), &buffer.rope) {
            let index = shaped
                .source_bytes
                .partition_point(|&b| b < row.start - line_start);
            let x0 = shaped.caret_offset(index.min(shaped.offsets.len() - 1));
            let at = line_start + shaped.byte_at_x(x - text_x + x0 + scrolled);
            return at.clamp(
                row.start,
                if row.last {
                    row_end
                } else {
                    row_end.saturating_sub(1).max(row.start)
                },
            );
        }
        let column = ((x - text_x + scrolled) / m.advance).max(0.0);
        let at = crate::text::wrap::byte_at_column(
            &buffer.rope,
            row.start,
            row_end,
            column.round() as usize,
        );
        // The end of a continued row is the next row's start: a click past
        // the text stays on the row that was clicked.
        return if !row.last && at >= row_end {
            buffer
                .rope
                .char_to_byte(buffer.rope.byte_to_char(row_end).saturating_sub(1))
        } else {
            at
        };
    }

    let row = ((y + scroll_offset(buffer, m.line_height)) / m.line_height)
        .floor()
        .max(0.0) as usize;
    let line = (buffer.scroll_line + row).min(total_lines.saturating_sub(1));

    // Round rather than floor: clicking the right half of a character should
    // put the caret after it, which is what every editor does.
    let text_x = gutter_width(buffer, atlas);
    let start = buffer.rope.line_to_byte(line);
    if let Some(shaped) = atlas.cached_editor_line((buffer.id(), line), &buffer.rope) {
        let target_x = x - text_x + buffer.scroll_column as f32 * m.advance;
        return start + shaped.byte_at_x(target_x);
    }
    let column = (((x - text_x) / m.advance).round()).max(0.0) as usize + buffer.scroll_column;

    byte_at_visual_column(buffer, line, column)
}

/// The line whose fold chevron is under a point in the text area's own
/// coordinates (as [`offset_at_point`] takes them), when there is one: the
/// cell after the line number, on a line's first row, of a line that is
/// folded or could fold.
pub fn fold_chevron_at(buffer: &Buffer, atlas: &Atlas, x: f32, y: f32) -> Option<usize> {
    let m = atlas.metrics;
    let gutter = gutter_width(buffer, atlas);
    if x < gutter - m.advance * 1.2 || x >= gutter {
        return None;
    }
    let rows = screen_rows(
        buffer,
        Viewport {
            x: 0.0,
            y: 0.0,
            width: f32::MAX,
            height: y.max(0.0) + m.line_height,
        },
        m.line_height,
    );
    let row = rows.iter().find(|r| r.y <= y && y < r.y + m.line_height)?;
    (row.first && (buffer.is_folded_at(row.line) || buffer.can_fold(row.line))).then_some(row.line)
}

/// Where the primary caret is, in the same coordinates as `text`: the top
/// left of its cell and the cell's size. `None` when it is scrolled out of
/// view. The inverse of [`offset_at_point`], and what the input method asks
/// for so it can put its candidate window next to what is being typed.
pub fn caret_rect(buffer: &Buffer, atlas: &Atlas, text: Viewport) -> Option<Viewport> {
    let m = atlas.metrics;
    let (line, _) = buffer.cursor_position();
    if buffer.row_mode() {
        let caret = buffer.cursor();
        let row = screen_rows(buffer, text, m.line_height)
            .into_iter()
            .find(|r| r.holds(line, caret))?;
        let line_start = buffer.rope.line_to_byte(line);
        let x = if let Some(shaped) = atlas.cached_editor_line((buffer.id(), line), &buffer.rope) {
            let at = |byte: usize| {
                let index = shaped
                    .source_bytes
                    .partition_point(|&b| b < byte - line_start);
                shaped.caret_offset(index.min(shaped.offsets.len() - 1))
            };
            at(caret) - at(row.start)
        } else {
            crate::text::wrap::column_in_row(&buffer.rope, row.start, caret) as f32 * m.advance
        };
        let x = x - buffer.scroll_column as f32 * m.advance;
        if x < 0.0 {
            return None;
        }
        return Some(Viewport {
            x: text.x + gutter_width(buffer, atlas) + x,
            y: row.y,
            width: m.advance,
            height: m.line_height,
        });
    }
    let row = line.checked_sub(buffer.scroll_line)?;
    if row >= text.rows(m.line_height).max(1) {
        return None;
    }
    let line_start = buffer.rope.line_to_byte(line);
    if let Some(shaped) = atlas.cached_editor_line((buffer.id(), line), &buffer.rope) {
        let utf16 = shaped
            .source_bytes
            .partition_point(|&b| b < buffer.cursor() - line_start);
        let x = text.x
            + gutter_width(buffer, atlas)
            + shaped.caret_offset(utf16.min(shaped.offsets.len() - 1))
            - buffer.scroll_column as f32 * m.advance;
        if x < text.x + gutter_width(buffer, atlas) {
            return None;
        }
        return Some(Viewport {
            x,
            y: text.y + row as f32 * m.line_height - scroll_offset(buffer, m.line_height),
            width: m.advance,
            height: m.line_height,
        });
    }
    let column = visual_column_between(buffer, line_start, buffer.cursor());
    let column = column.checked_sub(buffer.scroll_column)?;
    Some(Viewport {
        x: text.x + gutter_width(buffer, atlas) + column as f32 * m.advance,
        y: text.y + row as f32 * m.line_height - scroll_offset(buffer, m.line_height),
        width: m.advance,
        height: m.line_height,
    })
}

/// Draws text an input method is still composing, over the caret: the
/// accent waiting for its letter, or the syllables waiting to become a word.
/// Underlined, which is how every Mac text field marks "not committed yet".
pub fn push_marked_text(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    at: Viewport,
    text: &str,
    theme: &Theme,
) {
    let m = atlas.metrics;
    let cells: usize = text.chars().map(display_width).sum();
    let width = cells.max(1) as f32 * m.advance;
    // Its own background, since it is drawn over whatever follows the caret
    // rather than pushing it along.
    let opaque = [
        theme.background[0] as f32,
        theme.background[1] as f32,
        theme.background[2] as f32,
        1.0,
    ];
    push_rect(out, atlas, [at.x, at.y], [width, at.height], opaque);
    push_text(out, atlas, at.x, at.y, text, theme.text);
    let thickness = (2.0 / m.scale).max(1.0 / m.scale);
    push_rect(
        out,
        atlas,
        [at.x, m.snap(at.y + at.height - thickness * 2.0)],
        [width, thickness],
        theme.accent,
    );
}

/// Descends tab-aware rope summaries to `target` visual column, and
/// returns the byte offset there. Clamps to the end of the line.
fn byte_at_visual_column(buffer: &Buffer, line: usize, target: usize) -> usize {
    let start = buffer.rope.line_to_byte(line);
    let end = if line + 1 < buffer.rope.len_lines() {
        buffer.rope.line_to_byte(line + 1)
    } else {
        buffer.rope.len_bytes()
    };

    let (byte, column) = buffer.rope.visual_seek(start..end, target);
    let Some(ch) = buffer
        .rope
        .chunks_in(byte..end)
        .next()
        .and_then(|s| s.chars().next())
    else {
        return byte;
    };
    if ch == '\r' || ch == '\n' || column >= target {
        return byte;
    }
    let next = advance(column, ch);
    if target - column < next - target {
        byte
    } else {
        byte + ch.len_utf8()
    }
}

/// Appends a run of text at an arbitrary position. Used for chrome such as
/// the status line, which is not part of the buffer.
pub fn push_text(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    x: f32,
    y: f32,
    text: &str,
    color: [f32; 4],
) {
    let (cell_w, cell_h) = atlas.cell_size();
    let m = atlas.metrics;
    let advance = m.advance;
    let glyph_dy = m.glyph_dy(m.line_height);
    let mut column = 0usize;
    for ch in text.chars() {
        if let Some(slot) = atlas.slot_for(ch) {
            out.push(GlyphInstance {
                pos: [x + column as f32 * advance, y + glyph_dy],
                size: [cell_w * slot.cells as f32, cell_h],
                uv: slot.uv,
                flags: slot.flags(),
                color,
                ..Default::default()
            });
        }
        column += display_width(ch);
    }
}

/// How far above its viewport the text starts, in points: the part of the
/// first line a trackpad has scrolled off. Everything that places a line by
/// its row subtracts this, or it lands a fraction of a line off the text.
pub fn scroll_offset(buffer: &Buffer, line_height: f32) -> f32 {
    buffer.scroll_fraction.clamp(0.0, 1.0) * line_height
}

/// The lines with any part in `viewport`: the top one, partly scrolled
/// off, down to the one the bottom edge cuts through.
pub fn visible_lines(
    buffer: &Buffer,
    viewport: Viewport,
    line_height: f32,
) -> std::ops::Range<usize> {
    let total = buffer.rope.len_lines();
    let first = buffer.scroll_line.min(total.saturating_sub(1));
    if line_height <= 0.0 || viewport.height <= 0.0 {
        return first..first;
    }
    if buffer.row_mode() {
        let rows = screen_rows(buffer, viewport, line_height);
        return match (rows.first(), rows.last()) {
            (Some(a), Some(b)) => a.line..b.line + 1,
            _ => first..first,
        };
    }
    let reach = viewport.height + scroll_offset(buffer, line_height);
    let count = (reach / line_height).ceil() as usize;
    first..(first + count).min(total)
}

/// One row of text on screen: a whole line, or one row of a wrapped line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScreenRow {
    pub line: usize,
    /// The row's text. The last row of a line ends past its newline, where
    /// the next line starts, as `line_to_byte(line + 1)` does.
    pub start: usize,
    pub end: usize,
    /// The first and last rows of the line.
    pub first: bool,
    pub last: bool,
    /// Top of the row.
    pub y: f32,
}

impl ScreenRow {
    /// Whether a caret at `byte` on `line` is drawn on this row. A caret on
    /// a break belongs to the row the break starts.
    pub fn holds(&self, line: usize, byte: usize) -> bool {
        self.line == line && byte >= self.start && (byte < self.end || self.last)
    }
}

/// The rows with any part in `viewport`, top to bottom. Every place that
/// puts text on screen or finds text under the pointer reads these, so the
/// two cannot disagree about where a wrapped row is.
pub fn screen_rows(buffer: &Buffer, viewport: Viewport, line_height: f32) -> Vec<ScreenRow> {
    let total = buffer.rope.len_lines();
    let mut rows = Vec::new();
    if line_height <= 0.0 || viewport.height <= 0.0 || total == 0 {
        return rows;
    }
    let bottom = viewport.y + viewport.height;
    let mut y = viewport.y - scroll_offset(buffer, line_height);
    let mut line = buffer.scroll_line.min(total - 1);
    let mut skip = if buffer.row_mode() {
        buffer.scroll_row
    } else {
        0
    };
    while line < total && y < bottom {
        let next = if line + 1 < total {
            buffer.rope.line_to_byte(line + 1)
        } else {
            buffer.rope.len_bytes()
        };
        let starts = buffer.row_starts(line);
        let count = starts.len();
        if count == 0 {
            // Folded away: jump past the whole fold.
            line = buffer.hidden_until(line).map_or(line + 1, |end| end + 1);
            skip = 0;
            continue;
        }
        for (index, &start) in starts.iter().enumerate().skip(skip.min(count - 1)) {
            if y >= bottom {
                break;
            }
            rows.push(ScreenRow {
                line,
                start,
                end: starts.get(index + 1).copied().unwrap_or(next),
                first: index == 0,
                last: index + 1 == count,
                y,
            });
            y += line_height;
        }
        skip = 0;
        line += 1;
    }
    rows
}

/// Cuts a quad to the rows between `top` and `bottom`, texture included,
/// so a line half scrolled out of the text does not draw over the chrome.
fn clip_vertical(quad: &mut GlyphInstance, top: f32, bottom: f32) {
    let start = quad.pos[1];
    let end = start + quad.size[1];
    if start >= top && end <= bottom {
        return;
    }
    let from = start.max(top).min(bottom);
    let to = end.min(bottom).max(from);
    if quad.size[1] > 0.0 {
        let uv_height = quad.uv[3] - quad.uv[1];
        quad.uv[1] += uv_height * (from - start) / quad.size[1];
        quad.uv[3] = quad.uv[1] + uv_height * (to - from) / quad.size[1];
    }
    quad.pos[1] = from;
    quad.size[1] = to - from;
}

/// Crop textured geometry, including its UVs, rather than painting over it.
fn clip_horizontal(quad: &mut GlyphInstance, left: f32, right: f32) {
    let start = quad.pos[0];
    let end = start + quad.size[0];
    let from = start.max(left).min(right);
    let to = end.min(right).max(from);
    if quad.size[0] > 0.0 {
        let uv_width = quad.uv[2] - quad.uv[0];
        quad.uv[0] += uv_width * (from - start) / quad.size[0];
        quad.uv[2] = quad.uv[0] + uv_width * (to - from) / quad.size[0];
    }
    quad.pos[0] = from;
    quad.size[0] = to - from;
}

/// Short proportional UI labels, shaped and cached independently from code.
pub fn ui_input_window(text: &str, cursor: usize) -> (String, usize) {
    let cursor = cursor.min(text.len());
    let start = text[..cursor]
        .char_indices()
        .rev()
        .nth(79)
        .map_or(0, |(at, _)| at);
    let end = text[cursor..]
        .char_indices()
        .nth(20)
        .map_or(text.len(), |(at, _)| cursor + at);
    (text[start..end].to_owned(), start)
}

pub fn ui_caret_x(atlas: &mut Atlas, text: &str, cursor: usize) -> f32 {
    let utf16 = text[..cursor.min(text.len())].encode_utf16().count();
    atlas
        .shape_ui(text)
        .map_or(0.0, |line| line.caret_offset(utf16))
}

/// Short proportional UI labels, shaped and cached independently from code.
pub fn push_ui_text(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    rect: Viewport,
    text: &str,
    color: [f32; 4],
) {
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return;
    }
    // Keep even an adversarial file name bounded on the main thread.
    let bounded: String = text.chars().take(120).collect();
    let Some(line) = atlas.shape_ui(&bounded) else {
        return;
    };
    let m = atlas.metrics;
    let (cw, ch) = atlas.cell_size();
    for glyph in &line.glyphs {
        if glyph.x > rect.width {
            continue;
        }
        let Some(slot) = atlas.slot_for_shaped(&line, glyph) else {
            continue;
        };
        let mut quad = GlyphInstance {
            pos: [m.snap(rect.x + glyph.x), rect.y + m.glyph_dy(rect.height)],
            size: [cw * slot.cells as f32, ch],
            uv: slot.uv,
            flags: slot.flags(),
            color,
            ..Default::default()
        };
        clip_horizontal(&mut quad, rect.x, rect.x + rect.width);
        out.push(quad);
    }
}

/// The advance width of a UI label, in points.
pub fn ui_text_width(atlas: &mut Atlas, text: &str) -> f32 {
    let bounded: String = text.chars().take(120).collect();
    ui_caret_x(atlas, &bounded, bounded.len())
}

/// A UI label centred horizontally in `rect`.
///
/// Controls used to centre their text by hand, with a fixed left inset chosen
/// for one particular string: `rect.x + 9.0` puts "Refresh" 9pt from the left
/// of an 82pt pill and 23pt from the right, and the narrower "×" 9pt from the
/// left of a 32pt one and 15pt from the right. Every label of a new length was
/// off by a new amount, which is how cramped toolbar labels came back after
/// being "fixed" by widening their bounds. Measure, then centre.
///
/// Text wider than the control falls back to the left inset it would have had,
/// so a long label clips at the right edge instead of losing its own start.
pub fn push_ui_text_centered(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    rect: Viewport,
    text: &str,
    color: [f32; 4],
) {
    let width = ui_text_width(atlas, text);
    let inset = ((rect.width - width) * 0.5).max(0.0);
    push_ui_text(
        out,
        atlas,
        Viewport {
            x: rect.x + inset,
            width: (rect.width - inset).max(0.0),
            ..rect
        },
        text,
        color,
    );
}

/// A focus ring: drawn *behind* the control's own fill, which then covers
/// all but `width` of it. Drawing a transparent rectangle on top would not
/// erase anything, since it is blended rather than punched out.
pub fn push_focus_ring(out: &mut Vec<GlyphInstance>, rect: Viewport, radius: f32, width: f32) {
    push_rounded_rect(
        out,
        Viewport {
            x: rect.x - width,
            y: rect.y - width,
            width: rect.width + width * 2.0,
            height: rect.height + width * 2.0,
        },
        radius + width,
        [0.651, 0.867, 0.761, 0.55],
    );
}

/// An icon-font glyph centred in `rect`, both axes.
///
/// Icons occupy two cells, so centring them means measuring those cells
/// rather than assuming a single advance the way text placement does.
pub fn push_icon_centered(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    rect: Viewport,
    glyph: char,
    color: [f32; 4],
) {
    let Some(slot) = atlas.slot_for(glyph) else {
        return;
    };
    let (cell_w, cell_h) = atlas.cell_size();
    let width = cell_w * slot.cells as f32;
    out.push(GlyphInstance {
        pos: [
            atlas.metrics.snap(rect.x + (rect.width - width) * 0.5),
            rect.y + (rect.height - cell_h) * 0.5,
        ],
        size: [width, cell_h],
        uv: slot.uv,
        flags: slot.flags(),
        color,
        ..Default::default()
    });
}

/// A UI label aligned to the right edge of `rect`.
///
/// For trailing hints like a keyboard shortcut, which were previously placed
/// by guessing a width and subtracting it from the right edge. The guess and
/// the real advance width disagreed, so the hint floated.
pub fn push_ui_text_right(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    rect: Viewport,
    text: &str,
    color: [f32; 4],
) {
    let width = ui_text_width(atlas, text);
    let inset = (rect.width - width).max(0.0);
    push_ui_text(
        out,
        atlas,
        Viewport {
            x: rect.x + inset,
            width: (rect.width - inset).max(0.0),
            ..rect
        },
        text,
        color,
    );
}

/// File location uses the same geometry in the app and the offline frame.
/// The segment buttons of a response strip, in [`Segment::ALL`] order.
pub fn response_segments(atlas: &mut Atlas, strip: Viewport, labels: [&str; 3]) -> [Viewport; 3] {
    let mut x = strip.x + 16.0;
    let y = strip.y + 38.0;
    let mut rects = [strip; 3];
    for (rect, label) in rects.iter_mut().zip(labels) {
        let width = ui_text_width(atlas, label) + 24.0;
        *rect = Viewport {
            x,
            y,
            width,
            height: 26.0,
        };
        x += width + 6.0;
    }
    rects
}

/// Status, facts and the segment switch above a response body.
pub fn build_response_strip(
    view: &crate::http::curl::View,
    atlas: &mut Atlas,
    strip: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    use crate::http::curl::{Segment, Verdict};
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
        height: 26.0,
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
        let active = segment == view.segment;
        push_rounded_rect(
            out,
            rect,
            6.0,
            if active {
                theme.tab_active
            } else {
                theme.tab_hover
            },
        );
        push_ui_text_centered(
            out,
            atlas,
            rect,
            label,
            if active {
                theme.text
            } else {
                theme.status_text
            },
        );
    }
    push_rect(
        out,
        atlas,
        [strip.x, strip.y + strip.height - 1.0],
        [strip.width, 1.0],
        theme.hairline,
    );
}

pub fn build_breadcrumbs(
    buffer: &Buffer,
    tree: &Tree,
    atlas: &mut Atlas,
    rect: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    let path = buffer.path.as_deref();
    let label = path
        .map(|path| {
            tree.root()
                .and_then(|root| path.strip_prefix(root).ok())
                .unwrap_or(path)
        })
        .map(|path| {
            path.components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("  ›  ")
        })
        .or_else(|| buffer.label.clone())
        .unwrap_or_else(|| "Untitled".into());
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
    push_rect(
        out,
        atlas,
        [rect.x, rect.y + rect.height - 1.0 / atlas.metrics.scale],
        [rect.width, 1.0 / atlas.metrics.scale],
        theme.hairline,
    );
}

/// A single analytically antialiased quad; no extra textures or draw calls.
pub fn push_rounded_rect(
    out: &mut Vec<GlyphInstance>,
    rect: Viewport,
    radius: f32,
    color: [f32; 4],
) {
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return;
    }
    out.push(GlyphInstance {
        pos: [rect.x, rect.y],
        size: [rect.width, rect.height],
        color,
        flags: crate::render::metal::ROUNDED,
        _pad: [
            radius
                .min(rect.width * 0.5)
                .min(rect.height * 0.5)
                .to_bits(),
            0,
            0,
        ],
        ..Default::default()
    });
}

/// Appends a solid rectangle.
pub fn push_rect(
    out: &mut Vec<GlyphInstance>,
    atlas: &Atlas,
    pos: [f32; 2],
    size: [f32; 2],
    color: [f32; 4],
) {
    out.push(GlyphInstance {
        pos,
        size,
        uv: atlas.solid_uv(),
        color,
        ..Default::default()
    });
}

/// Visual column from `from` to `to`, using tab-aware rope summaries.
/// Shared by the cursor and the selection so they cannot disagree about
/// where a column is.
fn visual_column_between(buffer: &Buffer, from: usize, to: usize) -> usize {
    buffer.rope.visual_column(from..to)
}

/// Leading whitespace of `line` in visual columns, or `None` when the line
/// is blank (whitespace only), which has no indent of its own.
fn indent_columns(buffer: &Buffer, line: usize) -> Option<usize> {
    let rope = &buffer.rope;
    let start = rope.line_to_byte(line);
    let end = if line + 1 < rope.len_lines() {
        rope.line_to_byte(line + 1)
    } else {
        rope.len_bytes()
    };
    let mut columns = 0;
    for chunk in rope.bytes_in(start..end) {
        for &byte in chunk {
            match byte {
                b' ' => columns += 1,
                b'\t' => columns += TAB_WIDTH - columns % TAB_WIDTH,
                b'\n' | b'\r' => return None,
                _ => return Some(columns),
            }
        }
    }
    None
}

/// The indent step the guides are drawn at: the greatest common divisor of
/// the indents on screen, so two-space and four-space files both get one
/// guide per level. Nothing to go on means the tab width.
fn indent_unit(indents: &[Option<usize>]) -> usize {
    fn gcd(a: usize, b: usize) -> usize {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    let unit = indents
        .iter()
        .flatten()
        .filter(|&&n| n > 0)
        .fold(0, |acc, &n| gcd(acc, n));
    if unit == 0 || unit > TAB_WIDTH {
        TAB_WIDTH
    } else {
        unit
    }
}

/// A blank line inside a block keeps the block's guides: the smaller of
/// the indents of the nearest non-blank lines above and below. Lines off
/// screen are read from the buffer, a bounded distance away.
fn blank_line_indent(
    buffer: &Buffer,
    line: usize,
    lines: &[usize],
    indents: &[Option<usize>],
) -> usize {
    const REACH: usize = 200;
    let at = |l: usize| -> Option<usize> {
        match lines.binary_search(&l) {
            Ok(i) => indents[i],
            Err(_) => indent_columns(buffer, l),
        }
    };
    let above = (line.saturating_sub(REACH)..line).rev().find_map(at);
    let total = buffer.rope.len_lines();
    let below = (line + 1..(line + 1 + REACH).min(total)).find_map(at);
    match (above, below) {
        (Some(a), Some(b)) => a.min(b),
        _ => 0,
    }
}

/// The bracket touching the caret and its partner, as byte offsets in
/// order, or `None`. A closing bracket just before the caret is tried first,
/// then an opening one just after, which is how the caret usually lands
/// when typing or arrowing past a bracket. The search counts only the same
/// pair, ignores strings and comments, and gives up after a bounded run so
/// an unmatched bracket in a huge file costs the same as a matched one.
pub fn bracket_match(buffer: &Buffer) -> Option<(usize, usize)> {
    const LIMIT: usize = 128 * 1024;
    let rope = &buffer.rope;
    let caret = buffer.cursor();
    for at in [caret.checked_sub(1), Some(caret)].into_iter().flatten() {
        let (open, close, forward) = match rope.byte_at(at)? {
            b'(' => (b'(', b')', true),
            b'[' => (b'[', b']', true),
            b'{' => (b'{', b'}', true),
            b')' => (b'(', b')', false),
            b']' => (b'[', b']', false),
            b'}' => (b'{', b'}', false),
            _ => continue,
        };
        let found = if forward {
            let end = (at + 1).saturating_add(LIMIT).min(rope.len_bytes());
            let mut depth = 0usize;
            let mut pos = at + 1;
            let mut hit = None;
            'scan: for chunk in rope.bytes_in(at + 1..end) {
                for &byte in chunk {
                    if byte == open {
                        depth += 1;
                    } else if byte == close {
                        if depth == 0 {
                            hit = Some(pos);
                            break 'scan;
                        }
                        depth -= 1;
                    }
                    pos += 1;
                }
            }
            hit
        } else {
            let start = at.saturating_sub(LIMIT);
            let chunks: Vec<&[u8]> = rope.bytes_in(start..at).collect();
            let mut depth = 0usize;
            let mut pos = at;
            let mut hit = None;
            'scan: for chunk in chunks.iter().rev() {
                for &byte in chunk.iter().rev() {
                    pos -= 1;
                    if byte == close {
                        depth += 1;
                    } else if byte == open {
                        if depth == 0 {
                            hit = Some(pos);
                            break 'scan;
                        }
                        depth -= 1;
                    }
                }
            }
            hit
        };
        if let Some(other) = found {
            return Some((at.min(other), at.max(other)));
        }
    }
    None
}

#[cfg(test)]
mod guide_tests {
    use super::*;
    use crate::text::buffer::Motion;

    #[test]
    fn indent_is_counted_in_columns_and_blank_lines_have_none() {
        let b = Buffer::from_text("fn a() {\n    x;\n\t\ty;\n  \n}\n");
        assert_eq!(indent_columns(&b, 0), Some(0));
        assert_eq!(indent_columns(&b, 1), Some(4));
        assert_eq!(indent_columns(&b, 2), Some(8), "two tabs");
        assert_eq!(indent_columns(&b, 3), None, "spaces only");
        assert_eq!(indent_columns(&b, 5), None, "the empty last line");
    }

    #[test]
    fn the_unit_follows_the_file() {
        assert_eq!(indent_unit(&[Some(0), Some(2), Some(4), None]), 2);
        assert_eq!(indent_unit(&[Some(4), Some(8), Some(12)]), 4);
        assert_eq!(indent_unit(&[Some(0), None]), TAB_WIDTH, "nothing indented");
        assert_eq!(
            indent_unit(&[Some(8), Some(16)]),
            TAB_WIDTH,
            "deep in a block"
        );
    }

    #[test]
    fn a_blank_line_keeps_the_guides_of_its_block() {
        let b = Buffer::from_text("{\n    a\n\n    b\n}\n");
        let lines: Vec<usize> = (0..5).collect();
        let indents: Vec<_> = lines.iter().map(|&l| indent_columns(&b, l)).collect();
        assert_eq!(blank_line_indent(&b, 2, &lines, &indents), 4);
        // Looking past the visible range reads the buffer.
        assert_eq!(blank_line_indent(&b, 2, &lines[3..], &indents[3..]), 4);
    }

    #[test]
    fn brackets_match_forward_backward_and_nested() {
        let mut b = Buffer::from_text("f(a, [b, (c)], d)");
        b.place_cursor(1, Motion::Move);
        assert_eq!(bracket_match(&b), Some((1, 16)), "before the opening paren");
        b.place_cursor(17, Motion::Move);
        assert_eq!(bracket_match(&b), Some((1, 16)), "after the closing paren");
        b.place_cursor(5, Motion::Move);
        assert_eq!(
            bracket_match(&b),
            Some((5, 12)),
            "the bracket, skipping the inner parens"
        );
        b.place_cursor(11, Motion::Move);
        assert_eq!(
            bracket_match(&b),
            Some((9, 11)),
            "the inner close under the caret"
        );
        b.place_cursor(3, Motion::Move);
        assert_eq!(bracket_match(&b), None);
    }

    #[test]
    fn an_unmatched_bracket_matches_nothing() {
        let mut b = Buffer::from_text("(((\n");
        b.place_cursor(0, Motion::Move);
        assert_eq!(bracket_match(&b), None);
        b.place_cursor(3, Motion::Move);
        assert_eq!(bracket_match(&b), None);
    }
}

/// Visual bands occupied by a source range. CoreText exposes two offsets at
/// direction boundaries; the pair with the shortest nonzero span belongs to
/// the adjacent character. Merge touching bands but preserve bidi gaps.
#[cfg(test)]
fn shaped_intervals(
    shaped: &ShapedLine,
    source_bytes: &[usize],
    from: usize,
    to: usize,
) -> Vec<(f32, f32)> {
    let mut intervals = Vec::new();
    let first = source_bytes.partition_point(|&byte| byte < from);
    let last = source_bytes
        .partition_point(|&byte| byte < to)
        .min(source_bytes.len() - 1);
    for index in first..last {
        let starts = [shaped.offsets[index], shaped.secondary_offsets[index]];
        let ends = [
            shaped.offsets[index + 1],
            shaped.secondary_offsets[index + 1],
        ];
        let mut best = (0.0, 0.0, f32::INFINITY);
        for start in starts {
            for end in ends {
                let width = (end - start).abs();
                if width > 0.01 && width < best.2 {
                    best = (start.min(end), start.max(end), width);
                }
            }
        }
        if best.2.is_finite() {
            intervals.push((best.0, best.1));
        }
    }
    intervals.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<(f32, f32)> = Vec::new();
    for (left, right) in intervals {
        if let Some(last) = merged.last_mut()
            && left <= last.1 + 0.5
        {
            last.1 = last.1.max(right);
            continue;
        }
        merged.push((left, right));
    }
    merged
}

fn digit_count(n: usize) -> usize {
    let mut n = n.max(1);
    let mut d = 0;
    while n > 0 {
        d += 1;
        n /= 10;
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::metal::COLORED;
    use crate::text::buffer::Motion;

    fn atlas() -> Atlas {
        Atlas::build("SF Mono", 13.0, 2.0)
    }

    #[test]
    fn project_menu_sits_beside_sidebar_toggle_without_overlapping_git() {
        let mut atlas = atlas();
        let mut tree = Tree::new();
        tree.set_root("/tmp/a-personal-project-with-a-long-name");
        for width in [420.0, 480.0, 640.0, 1000.0] {
            let toolbar = Viewport {
                x: 0.0,
                y: 0.0,
                width,
                height: 48.0,
            };
            let button = toolbar_project(&tree, &mut atlas, toolbar);
            let toggle = toolbar_sidebar(toolbar);
            let next = toolbar_search(toolbar);
            assert!(button.x >= toggle.x + toggle.width);
            assert!(button.x + button.width <= next.x);
            assert!(button.width >= 100.0, "project menu vanished at {width} pt");
            assert!(
                button.y >= toolbar.y && button.y + button.height <= toolbar.y + toolbar.height
            );
        }
    }

    #[test]
    fn long_project_title_keeps_both_ends_inside_its_button() {
        let mut atlas = atlas();
        let label = project_label(
            &mut atlas,
            "caio-personal-long-workspace-name.F12TR6",
            170.0,
        );
        assert!(label.starts_with("caio"), "{label}");
        assert!(label.ends_with("F12TR6"), "{label}");
        assert!(label.contains('…'), "{label}");
        assert!(ui_text_width(&mut atlas, &label) <= 170.0);
    }

    /// The bug this catches: button text was placed at a fixed left inset, so
    /// the gap left of the label and the gap right of it disagreed, by a
    /// different amount for every label length. "Refresh" sat 9pt from the
    /// left of its pill and 23pt from the right.
    #[test]
    fn centered_button_labels_have_equal_gaps_at_any_length() {
        let mut atlas = atlas();
        let control = Viewport {
            x: 100.0,
            y: 40.0,
            width: 82.0,
            height: 30.0,
        };
        for text in ["×", "Refresh", "Stage", "Unstage", "Commit"] {
            let width = ui_text_width(&mut atlas, text);
            assert!(width < control.width, "{text} needs a wider fixture");
            let mut quads = Vec::new();
            push_ui_text_centered(&mut quads, &mut atlas, control, text, Theme::default().text);
            let first = quads.iter().map(|q| q.pos[0]).fold(f32::INFINITY, f32::min);
            let left = first - control.x;
            let right = control.x + control.width - (first + width);
            assert!(
                (left - right).abs() <= 1.0,
                "{text}: {left}pt left of the label, {right}pt right of it"
            );
        }
    }

    /// A label wider than its control keeps the leading edge and clips at the
    /// trailing one. Centring an overflowing label would hide its start, which
    /// is the half that identifies it.
    #[test]
    fn a_label_wider_than_its_control_starts_at_the_leading_edge() {
        let mut atlas = atlas();
        let control = Viewport {
            x: 100.0,
            y: 40.0,
            width: 40.0,
            height: 30.0,
        };
        let text = "Git · a-very-long-branch-name";
        assert!(ui_text_width(&mut atlas, text) > control.width);
        let mut quads = Vec::new();
        push_ui_text_centered(&mut quads, &mut atlas, control, text, Theme::default().text);
        let first = quads.iter().map(|q| q.pos[0]).fold(f32::INFINITY, f32::min);
        assert!(
            (first - control.x).abs() <= 1.0,
            "label start moved to {first}"
        );
        for quad in &quads {
            assert!(quad.pos[0] <= control.x + control.width + 1.0);
        }
    }

    /// A trailing hint sits against the right edge whatever it says, instead
    /// of at a hand-guessed offset that only matched one string.
    #[test]
    fn right_aligned_hints_end_at_the_trailing_edge() {
        let mut atlas = atlas();
        let control = Viewport {
            x: 60.0,
            y: 0.0,
            width: 120.0,
            height: 28.0,
        };
        for text in ["⌘ P", "⌘⌥ G"] {
            let width = ui_text_width(&mut atlas, text);
            let mut quads = Vec::new();
            push_ui_text_right(&mut quads, &mut atlas, control, text, Theme::default().text);
            let first = quads.iter().map(|q| q.pos[0]).fold(f32::INFINITY, f32::min);
            let gap = control.x + control.width - (first + width);
            assert!(gap.abs() <= 1.0, "{text} ends {gap}pt from the right edge");
        }
    }

    #[test]
    fn ui_labels_are_proportional_cached_and_bounded() {
        let mut atlas = atlas();
        let narrow = atlas.shape_ui("iiii").unwrap();
        let wide = atlas.shape_ui("WWWW").unwrap();
        assert!(wide.offsets.last().unwrap() > &(narrow.offsets.last().unwrap() * 2.0));
        assert!(std::rc::Rc::ptr_eq(
            &narrow,
            &atlas.shape_ui("iiii").unwrap()
        ));
        assert!(atlas.shape_ui(&"x".repeat(513)).is_none());
        let mut quads = Vec::new();
        push_ui_text(
            &mut quads,
            &mut atlas,
            Viewport {
                x: 20.0,
                y: 0.0,
                width: 30.0,
                height: 26.0,
            },
            "long name 漢字 👩‍💻",
            Theme::default().text,
        );
        assert!(!quads.is_empty());
        assert!(
            quads
                .iter()
                .all(|q| q.pos[0] >= 20.0 && q.pos[0] + q.size[0] <= 50.01)
        );
    }

    #[test]
    fn horizontal_clipping_preserves_texture_mapping() {
        let mut q = GlyphInstance {
            pos: [10.0, 0.0],
            size: [20.0, 10.0],
            uv: [0.2, 0.3, 0.6, 0.5],
            ..Default::default()
        };
        clip_horizontal(&mut q, 15.0, 25.0);
        assert_eq!(q.pos[0], 15.0);
        assert_eq!(q.size[0], 10.0);
        assert!((q.uv[0] - 0.3).abs() < 0.001);
        assert!((q.uv[2] - 0.5).abs() < 0.001);
    }

    #[test]
    fn frame_hits_the_first_region_listed_and_finds_regions_by_name() {
        let mut frame = Frame::default();
        frame.push(Hit::TabClose(1), Viewport::new(10.0, 10.0));
        frame.regions[0].1 = Viewport {
            x: 40.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
        };
        frame.push(
            Hit::Tab(1),
            Viewport {
                x: 0.0,
                y: 0.0,
                width: 60.0,
                height: 10.0,
            },
        );
        frame.push(Hit::Text, Viewport::new(0.0, 0.0));
        assert_eq!(
            frame.hit(45.0, 5.0),
            Some(&Hit::TabClose(1)),
            "the close button wins inside its tab"
        );
        assert_eq!(frame.hit(5.0, 5.0), Some(&Hit::Tab(1)));
        assert_eq!(frame.hit(5.0, 50.0), None);
        assert_eq!(frame.named("tab.close.1").map(|r| r.x), Some(40.0));
        assert!(
            frame.rect(&Hit::Text).is_none(),
            "an empty rectangle is not a target"
        );
        assert_eq!(Hit::ResponseSegment(2).name(), "response.segment.2");
    }

    #[test]
    fn ignored_rows_are_dimmed() {
        let root = std::env::temp_dir().join(format!("crc-sidebar-ignored-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("dist")).unwrap();
        std::fs::write(root.join("dist/app.js"), "").unwrap();
        std::fs::write(root.join("main.rs"), "").unwrap();
        let mut atlas = atlas();
        let mut tree = Tree::new();
        tree.open(&root);
        let dist = tree.rows().iter().position(|e| e.name == "dist").unwrap();
        tree.selected = None;
        tree.toggle(dist);
        let theme = Theme::default();
        let rect = Viewport::new(260.0, 500.0);
        let dim = |[r, g, b, a]: [f32; 4]| [r, g, b, a * 0.45];

        let mut plain = Vec::new();
        build_sidebar(&tree, false, &mut atlas, rect, &theme, &mut plain);
        tree.set_ignored(std::sync::Arc::new(
            [root.join("dist")].into_iter().collect(),
        ));
        let mut out = Vec::new();
        build_sidebar(&tree, false, &mut atlas, rect, &theme, &mut out);
        assert_eq!(out.len(), plain.len(), "dimmed, nothing added");
        let count = |c: [f32; 4]| out.iter().filter(|q| q.color == c).count();
        assert_eq!(
            count(dim(theme.status_text)),
            2,
            "dist's icon and app.js's icon"
        );
        assert!(count(dim(theme.sidebar_directory)) > 0, "dist's name");
        assert!(count(dim(theme.sidebar_text)) > 0, "app.js's name");
        assert!(
            count(theme.sidebar_text) > 0,
            "main.rs stays at full strength"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn sidebar_header_is_not_a_file_hit() {
        let atlas = atlas();
        let mut tree = Tree::new();
        tree.open(std::env::current_dir().unwrap());
        let rect = Viewport::new(260.0, 500.0);
        assert!(sidebar_row_at(&tree, &atlas, rect, 20.0).is_none());
        assert_eq!(
            sidebar_row_at(&tree, &atlas, rect, SIDEBAR_HEADER_HEIGHT + 13.0),
            Some(0)
        );
        assert_eq!(
            sidebar_row_at(
                &tree,
                &atlas,
                rect,
                SIDEBAR_HEADER_HEIGHT + SIDEBAR_ROW_HEIGHT + 13.0
            ),
            Some(1)
        );
    }

    #[test]
    fn active_overflow_tab_backfills_the_visible_strip() {
        let mut first = Buffer::from_text("");
        first.path = Some("/tmp/caio-tab-0.md".into());
        let mut docs = Documents::new(first);
        for index in 1..7 {
            let mut buffer = Buffer::from_text("");
            buffer.path = Some(format!("/tmp/caio-tab-{index}.md").into());
            docs.add(buffer);
        }
        let active = docs.active_index();
        let mut atlas = atlas();
        let mut out = Vec::new();
        let mut hits = Vec::new();
        build_tab_bar(
            &docs,
            active,
            None,
            &mut atlas,
            Viewport::new(760.0, TAB_BAR_HEIGHT),
            &Theme::default(),
            &mut out,
            &mut hits,
        );
        assert!(hits.len() > 1, "overflow must not collapse to one tab");
        assert!(hits.iter().any(|hit| hit.index == active));
    }

    /// A tab that is not the active one still has to be closable, which means
    /// its cross appears under the pointer. Only on the active tab, the rest
    /// of the strip could not be closed without selecting each tab first.
    #[test]
    fn hovering_a_tab_draws_its_close_cross() {
        let mut first = Buffer::from_text("");
        first.path = Some("/tmp/caio-hover-0.md".into());
        let mut docs = Documents::new(first);
        let mut second = Buffer::from_text("");
        second.path = Some("/tmp/caio-hover-1.md".into());
        docs.add(second);
        docs.switch(0);
        let mut atlas = atlas();
        let rect = Viewport::new(760.0, TAB_BAR_HEIGHT);
        let theme = Theme::default();
        let draw = |atlas: &mut Atlas, hovered| {
            let mut out = Vec::new();
            let mut hits = Vec::new();
            build_tab_bar(&docs, 0, hovered, atlas, rect, &theme, &mut out, &mut hits);
            (out.len(), hits)
        };
        let (plain, hits) = draw(&mut atlas, None);
        let (hovered, _) = draw(&mut atlas, Some(1));
        assert!(
            hovered > plain,
            "hovering the inactive tab drew nothing extra"
        );
        // And the cross it drew is inside that tab's own close target.
        let hit = hits.iter().find(|h| h.index == 1).expect("second tab");
        assert!(hit.close_x0 >= hit.x0 && hit.close_x1 <= hit.x1);
    }

    #[test]
    fn deep_fallback_matches_unscrolled_geometry_and_clicks() {
        let prefix = "é漢\t🌍x\t".repeat(150_000);
        // Past MAX_SHAPED_LINE_BYTES on its own, so the unscrolled line takes
        // the fallback path too rather than being shaped by the worker.
        let suffix = "é漢\t🌍x\t".repeat(180_000);
        let mut near = Buffer::from_text(&suffix);
        let mut far = Buffer::from_text(&(prefix.clone() + &suffix));
        near.select_range(0, 11);
        far.select_range(prefix.len(), prefix.len() + 11);
        far.scroll_column = 1_200_000;
        let mut atlas = atlas();
        let viewport = Viewport::new(600.0, 100.0);
        let mut expected = Vec::new();
        let mut actual = Vec::new();
        // Cold fallback glyphs rasterize on a worker and draw as `?` until
        // it delivers. Warm the atlas first, or whether the two frames agree
        // depends on the worker finishing between them.
        build(
            &near,
            &mut atlas,
            viewport,
            &Theme::default(),
            &mut expected,
        );
        let started = std::time::Instant::now();
        while atlas.has_pending_shaping() && started.elapsed().as_secs() < 5 {
            std::thread::sleep(std::time::Duration::from_millis(2));
            atlas.begin_frame();
        }
        build(
            &near,
            &mut atlas,
            viewport,
            &Theme::default(),
            &mut expected,
        );
        build(&far, &mut atlas, viewport, &Theme::default(), &mut actual);
        assert_eq!(actual.len(), expected.len());
        for (a, b) in actual.iter().zip(&expected) {
            // Large pixel coordinates have f32 rounding; positions should
            // agree to a physical pixel at the default scale.
            assert!((a.pos[0] - b.pos[0]).abs() <= 0.5);
            assert_eq!(a.pos[1], b.pos[1]);
            assert_eq!(a.size, b.size);
            assert_eq!(a.uv, b.uv);
            assert_eq!(a.color, b.color);
            assert_eq!(a.flags, b.flags);
        }
        for x in (24..580).step_by(3) {
            assert_eq!(
                offset_at_point(&far, &atlas, x as f32, 2.0),
                prefix.len() + offset_at_point(&near, &atlas, x as f32, 2.0)
            );
        }
        let a = caret_rect(&far, &atlas, viewport).unwrap();
        let b = caret_rect(&near, &atlas, viewport).unwrap();
        assert!((a.x - b.x).abs() <= 0.5);
    }

    #[test]
    fn editor_geometry_reuses_snapshots_and_invalidates_edits_and_undo() {
        let mut atlas = atlas();
        let mut buffer = Buffer::from_text("é\t👩‍💻\r\n");
        let get = |atlas: &mut Atlas, b: &Buffer| {
            atlas
                .shape_editor_line((b.id(), 0), &b.rope, 0..b.rope.line_to_byte(1))
                .unwrap()
        };
        let before = get(&mut atlas, &buffer);
        assert!(std::rc::Rc::ptr_eq(&before, &get(&mut atlas, &buffer)));
        assert_eq!(*before.source_bytes.last().unwrap(), "é\t👩‍💻".len());
        buffer.insert("漢");
        assert!(
            atlas
                .cached_editor_line((buffer.id(), 0), &buffer.rope)
                .is_none()
        );
        assert_eq!(
            *get(&mut atlas, &buffer).source_bytes.last().unwrap(),
            "漢é\t👩‍💻".len()
        );
        buffer.undo();
        assert_eq!(get(&mut atlas, &buffer).source_bytes, before.source_bytes);
        let other = Buffer::from_text("אב\r\n");
        assert_eq!(
            *get(&mut atlas, &other).source_bytes.last().unwrap(),
            "אב".len()
        );
        buffer.rope.insert(0, "x");
        assert_eq!(
            *get(&mut atlas, &buffer).source_bytes.last().unwrap(),
            "xé\t👩‍💻".len()
        );
    }

    #[test]
    fn shaped_caret_and_click_use_coretext_offsets() {
        let mut atlas = atlas();
        let mut buffer = Buffer::from_text("e\u{301}x\n");
        let viewport = Viewport::new(600.0, 200.0);
        let mut glyphs = Vec::new();
        build(
            &buffer,
            &mut atlas,
            viewport,
            &Theme::default(),
            &mut glyphs,
        );
        let after_accent = "e\u{301}".len();
        buffer.place_cursor(after_accent, Motion::Move);
        let rect = caret_rect(&buffer, &atlas, viewport).unwrap();
        assert_eq!(
            offset_at_point(&buffer, &atlas, rect.x, rect.y + 2.0),
            after_accent
        );
        assert!(rect.x < gutter_width(&buffer, &atlas) + atlas.metrics.advance * 2.0);
    }

    #[test]
    fn long_unicode_line_shapes_clusters_and_maps_scrolled_caret() {
        let mut atlas = atlas();
        // Beyond the old source and expanded-text limits, with a cluster at
        // the visible caret and tabs that must retain source byte mapping.
        let prefix = "a\t".repeat(3000);
        let source = format!("{prefix}e\u{301}x 👩‍💻 لا שלום\r\n");
        let mut buffer = Buffer::from_text(&source);
        let after_accent = prefix.len() + "e\u{301}".len();
        buffer.place_cursor(after_accent, Motion::Move);
        buffer.scroll_column = 11990;
        let viewport = Viewport::new(600.0, 200.0);
        let mut glyphs = Vec::new();
        build(
            &buffer,
            &mut atlas,
            viewport,
            &Theme::default(),
            &mut glyphs,
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while atlas.has_pending_shaping() {
            assert!(
                std::time::Instant::now() < deadline,
                "shaping worker timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
            build(
                &buffer,
                &mut atlas,
                viewport,
                &Theme::default(),
                &mut glyphs,
            );
        }
        let shaped = atlas
            .cached_editor_line((buffer.id(), 0), &buffer.rope)
            .expect("long line uses CoreText");
        assert!(shaped.glyphs.iter().any(|g| g.source_utf16 == 12000));
        assert_eq!(shaped.offsets[12001], shaped.offsets[12002]);
        buffer.scroll_column = (shaped.caret_offset(12002) / atlas.metrics.advance) as usize - 10;
        build(
            &buffer,
            &mut atlas,
            viewport,
            &Theme::default(),
            &mut glyphs,
        );
        let rect = caret_rect(&buffer, &atlas, viewport).unwrap();
        assert_eq!(
            offset_at_point(&buffer, &atlas, rect.x, rect.y + 2.0),
            after_accent
        );
        assert!(glyphs.len() < 100, "only visible glyphs emit quads");
    }

    #[test]
    fn asynchronous_geometry_tracks_latest_rope_snapshot() {
        let mut atlas = atlas();
        let mut buffer = Buffer::from_text(&"é\t👩‍💻 שלום ".repeat(700));
        let viewport = Viewport::new(600.0, 200.0);
        let mut glyphs = Vec::new();
        build(
            &buffer,
            &mut atlas,
            viewport,
            &Theme::default(),
            &mut glyphs,
        );
        assert!(atlas.has_pending_shaping());
        buffer.insert("最新");
        build(
            &buffer,
            &mut atlas,
            viewport,
            &Theme::default(),
            &mut glyphs,
        );
        buffer.undo();
        buffer.rope.insert(0, "changed ");
        build(
            &buffer,
            &mut atlas,
            viewport,
            &Theme::default(),
            &mut glyphs,
        );
        let other = Buffer::from_text(&format!("other e\u{301}{}", "x".repeat(5000)));
        build(&other, &mut atlas, viewport, &Theme::default(), &mut glyphs);
        assert!(
            atlas
                .cached_editor_line((buffer.id(), 0), &buffer.rope)
                .is_none()
        );
        let deadline = std::time::Instant::now();
        while atlas.has_pending_shaping() {
            assert!(deadline.elapsed().as_secs() < 10);
            std::thread::sleep(std::time::Duration::from_millis(2));
            build(&other, &mut atlas, viewport, &Theme::default(), &mut glyphs);
        }
        let shaped = atlas
            .cached_editor_line((other.id(), 0), &other.rope)
            .unwrap();
        assert_eq!(*shaped.source_bytes.last().unwrap(), other.rope.len_bytes());
        assert!(
            atlas
                .cached_editor_line((buffer.id(), 0), &buffer.rope)
                .is_none()
        );
        assert_eq!(shaped.byte_at_x(0.0), 0);
    }

    #[test]
    fn worker_shapes_beyond_one_mib_without_rasterizing_offscreen_glyphs() {
        let mut atlas = atlas();
        let source = format!("e\u{301}{} שלום", "a".repeat(1_200_000));
        let buffer = Buffer::from_text(&source);
        let viewport = Viewport::new(600.0, 200.0);
        let mut glyphs = Vec::new();
        build(
            &buffer,
            &mut atlas,
            viewport,
            &Theme::default(),
            &mut glyphs,
        );
        assert!(atlas.has_pending_shaping());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while atlas.has_pending_shaping() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(2));
            build(
                &buffer,
                &mut atlas,
                viewport,
                &Theme::default(),
                &mut glyphs,
            );
        }
        assert!(
            atlas
                .cached_editor_line((buffer.id(), 0), &buffer.rope)
                .is_some()
        );
        assert!(
            atlas.resident() < 110,
            "offscreen fonts must not use atlas space"
        );
        assert!(glyphs.len() < 100);
        assert_eq!(
            offset_at_point(&buffer, &atlas, 32.0, 2.0),
            "e\u{301}".len()
        );
    }

    #[test]
    fn indexed_shaped_clicks_match_grapheme_scan() {
        let mut atlas = atlas();
        for source in [
            "abc שלום xyz",
            "a\té",
            "a   é", // Same pixels, different source-byte indexes.
            "\t\u{301}é\t👩‍💻 e\u{301}",
            "لا שלום 👨‍👩‍👧‍👦 🇪🇸 क्‍ष",
            "a\u{2067}אב\u{2069} z",
            "é\u{200b}\u{200b}\u{200b}x",
        ] {
            let buffer = Buffer::from_text(source);
            let shaped = atlas
                .shape_editor_line((buffer.id(), 0), &buffer.rope, 0..buffer.rope.len_bytes())
                .unwrap();
            let (_, bytes) = shape_input(source);
            let composed = NSString::from_str(source);
            let scan = |x: f32| {
                let mut closest = (0, f32::INFINITY);
                let mut utf16 = 0;
                for (byte, ch) in source.char_indices() {
                    if composed
                        .rangeOfComposedCharacterSequenceAtIndex(utf16)
                        .location
                        == utf16
                    {
                        let index = bytes.partition_point(|&b| b < byte);
                        let distance = (shaped.offsets[index] - x)
                            .abs()
                            .min((shaped.secondary_offsets[index] - x).abs());
                        if distance < closest.1 {
                            closest = (byte, distance);
                        }
                    }
                    utf16 += ch.len_utf16();
                }
                if (shaped.offsets.last().unwrap() - x).abs() < closest.1 {
                    closest.0 = source.len();
                }
                closest.0
            };
            let gutter = gutter_width(&buffer, &atlas);
            let mut edges = shaped.offsets.clone();
            edges.extend_from_slice(&shaped.secondary_offsets);
            edges.sort_by(f32::total_cmp);
            let mut samples = edges.clone();
            samples.extend(edges.windows(2).map(|w| (w[0] + w[1]) / 2.0));
            samples.extend((-40..2000).map(|x| x as f32 / 4.0));
            for x in samples {
                assert_eq!(shaped.byte_at_x(x), scan(x), "{source:?} at {x}");
                // Use the actual rounded screen coordinate for the scan too.
                let screen_x = gutter + x;
                assert_eq!(
                    offset_at_point(&buffer, &atlas, screen_x, 2.0),
                    scan(screen_x - gutter),
                    "composed click: {source:?} at {x}"
                );
            }
        }
    }

    #[test]
    fn tabs_keep_source_offsets_when_shaping_unicode() {
        assert_eq!(
            shape_input("a\té"),
            ("a   é".to_string(), vec![0, 1, 1, 1, 2, 4])
        );
        let mut atlas = atlas();
        let mut buffer = Buffer::from_text("a\té\n");
        let viewport = Viewport::new(600.0, 200.0);
        let mut glyphs = Vec::new();
        build(
            &buffer,
            &mut atlas,
            viewport,
            &Theme::default(),
            &mut glyphs,
        );
        buffer.place_cursor(2, Motion::Move);
        let rect = caret_rect(&buffer, &atlas, viewport).unwrap();
        assert_eq!(offset_at_point(&buffer, &atlas, rect.x, rect.y + 2.0), 2);
    }

    #[test]
    fn clipped_selection_index_matches_full_scan() {
        let mut atlas = atlas();
        let source = "abc שלום\t e\u{301} 👩‍💻 لا \u{2067}אב\u{2069} xyz ".repeat(32);
        let shaped = atlas.shape_line(&source).unwrap();
        let bytes = &shaped.source_bytes;
        let boundaries: Vec<_> = source
            .char_indices()
            .map(|(i, _)| i)
            .chain([source.len()])
            .collect();
        let width = shaped.offsets.iter().copied().fold(0.0, f32::max);
        for from in boundaries.iter().copied().step_by(31).chain([0]) {
            for to in [from, (from + 57).min(source.len()), source.len()] {
                let full = shaped_intervals(&shaped, bytes, from, to);
                for left in (0..width as usize)
                    .step_by(127)
                    .map(|x| x as f32)
                    .chain([-20.0])
                {
                    for extend in [0.0, 4.0] {
                        let right = left + 91.0;
                        let mut expected = full.clone();
                        if let Some(last) = expected.last_mut() {
                            last.1 += extend;
                        }
                        let expected: Vec<_> = expected
                            .into_iter()
                            .filter_map(|(a, b)| {
                                let (a, b) = (a.max(left), b.min(right));
                                (b > a).then_some((a, b))
                            })
                            .collect();
                        let actual = shaped.selection_intervals(from, to, left..right, extend);
                        assert_eq!(
                            actual, expected,
                            "source {from}..{to}, view {left}..{right}, extend {extend}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn mixed_direction_selection_preserves_visual_gap() {
        let mut atlas = atlas();
        let (text, bytes) = shape_input("abc שלום xyz");
        let shaped = atlas.shape_line(&text).unwrap();
        let intervals = shaped_intervals(&shaped, &bytes, 6, 14);
        assert_eq!(intervals.len(), 2);
        assert!(intervals[0].1 < intervals[1].0);
    }

    #[test]
    fn markdown_code_has_a_copy_target_and_live_block_keeps_other_blocks_rendered() {
        let mut atlas = atlas();
        let source = "# Title\n\n```rs\nlet x = 1;\n```\n\nAfter\n";
        let blocks = crate::markdown::parse_spanned(source);
        let mut glyphs = Vec::new();
        let mut hits = Vec::new();
        build_markdown(
            &blocks,
            source,
            None,
            None,
            0,
            &mut atlas,
            Viewport::new(800.0, 600.0),
            &Theme::default(),
            &mut glyphs,
            &mut hits,
        );
        let code = hits
            .iter()
            .find(|hit| hit.lines == (2..5))
            .expect("code block");
        let (button, copied) = code.copy.as_ref().expect("copy control");
        assert!(button.width > 0.0);
        assert_eq!(copied, "let x = 1;");
        assert!(
            !code.caret_stops.is_empty(),
            "code stays rendered while editing"
        );
        assert!(hits.iter().any(|hit| hit.lines == (6..7)));
        let heading = hits.iter().find(|hit| hit.lines == (0..1)).unwrap();
        assert_eq!(heading.caret_stops.first().unwrap().0, 2);
        assert_eq!(heading.caret_stops.last().unwrap().0, 7);

        build_markdown(
            &blocks,
            source,
            Some(3),
            None,
            0,
            &mut atlas,
            Viewport::new(800.0, 600.0),
            &Theme::default(),
            &mut glyphs,
            &mut hits,
        );
        assert!(
            hits.iter().any(|hit| hit.lines == (6..7)),
            "the rest of the page stays rendered"
        );
    }

    #[test]
    fn lays_out_only_the_visible_lines() {
        let mut atlas = atlas();
        let text: String = (0..10_000).map(|i| format!("line {i}\n")).collect();
        let mut buffer = Buffer::from_text(&text);
        buffer.scroll_line = 5_000;

        let vp = Viewport::new(800.0, 600.0);
        let rows = vp.rows(atlas.metrics.line_height);
        let mut out = Vec::new();
        let stats = build(&buffer, &mut atlas, vp, &Theme::default(), &mut out);

        let shown = visible_lines(&buffer, vp, atlas.metrics.line_height).len();
        assert!(shown == rows || shown == rows + 1);
        assert_eq!(stats.lines, shown, "should lay out exactly one viewport");
        assert!(stats.quads > 0);
        // Nowhere near the 10k lines in the buffer.
        assert!(stats.quads < rows * 200, "emitted far too many quads");
    }

    /// Regression: `build` clears its output, so anything drawn before it in
    /// a frame disappears. The tab bar was drawn first for several commits
    /// and was therefore never visible, while every unit test passed.
    #[test]
    fn build_clears_what_was_drawn_before_it() {
        let mut atlas = atlas();
        let buffer = Buffer::from_text("text\n");
        let vp = Viewport::new(800.0, 600.0);
        let theme = Theme::default();

        let mut out = Vec::new();
        push_rect(
            &mut out,
            &atlas,
            [0.0, 0.0],
            [10.0, 10.0],
            [1.0, 0.0, 0.0, 1.0],
        );
        let marker = out[0];
        build(&buffer, &mut atlas, vp, &theme, &mut out);

        assert!(
            !out.iter()
                .any(|q| q.color == marker.color && q.size == marker.size),
            "build clears: anything appended before it is gone, so callers \
             that append chrome must run AFTER it"
        );
    }

    #[test]
    fn reuses_the_output_buffer_without_growing() {
        let mut atlas = atlas();
        let buffer = Buffer::from_text("hello world\nsecond line\n");
        let vp = Viewport::new(800.0, 600.0);
        let mut out = Vec::new();

        build(&buffer, &mut atlas, vp, &Theme::default(), &mut out);
        let first = out.len();
        let capacity = out.capacity();

        for _ in 0..50 {
            build(&buffer, &mut atlas, vp, &Theme::default(), &mut out);
        }
        assert_eq!(out.len(), first, "layout is not deterministic");
        assert_eq!(out.capacity(), capacity, "layout reallocated every frame");
    }

    #[test]
    fn tabs_advance_to_the_next_stop() {
        let mut atlas = atlas();
        let advance = atlas.metrics.advance;
        let vp = Viewport::new(800.0, 600.0);
        let mut out = Vec::new();

        // "\tx": the tab takes column 0 to 4, so 'x' lands at column 4.
        let buffer = Buffer::from_text("\tx");
        build(&buffer, &mut atlas, vp, &Theme::default(), &mut out);
        let x_quad = out.last().expect("something was drawn");

        let buffer2 = Buffer::from_text("    x");
        let mut out2 = Vec::new();
        build(&buffer2, &mut atlas, vp, &Theme::default(), &mut out2);
        let x_quad2 = out2.last().expect("something was drawn");

        assert!(
            (x_quad.pos[0] - x_quad2.pos[0]).abs() < advance * 0.01,
            "a tab should land on the same column as four spaces"
        );
    }

    #[test]
    fn cursor_column_accounts_for_tabs() {
        let mut buffer = Buffer::from_text("\tabc");
        buffer.move_right(Motion::Move); // past the tab
        let line_start = buffer.rope.line_to_byte(0);
        assert_eq!(
            visual_column_between(&buffer, line_start, buffer.cursor()),
            4,
            "cursor ignored tab expansion"
        );
        buffer.move_right(Motion::Move);
        assert_eq!(
            visual_column_between(&buffer, line_start, buffer.cursor()),
            5
        );
    }

    #[test]
    fn non_ascii_is_drawn_rather_than_dropped() {
        let mut atlas = atlas();
        let buffer = Buffer::from_text("a🌍b é 漢");
        let vp = Viewport::new(800.0, 600.0);
        let mut out = Vec::new();
        let theme = Theme::default();
        let stats = build(&buffer, &mut atlas, vp, &theme, &mut out);

        assert_eq!(
            stats.unsupported, 0,
            "every one of these resolves through CoreText fallback"
        );
        // One quad per visible character, and the emoji must carry the
        // colour flag so the shader does not tint it.
        let colored = out.iter().filter(|q| q.flags & COLORED != 0).count();
        assert_eq!(colored, 1, "exactly the emoji should be a colour glyph");
    }

    #[test]
    fn wide_characters_advance_two_columns() {
        let mut atlas = atlas();
        let advance = atlas.metrics.advance;
        let vp = Viewport::new(800.0, 600.0);

        // "漢x" puts x at column 2; "aax" puts it at column 2 as well.
        let mut wide = Vec::new();
        build(
            &Buffer::from_text("漢x"),
            &mut atlas,
            vp,
            &Theme::default(),
            &mut wide,
        );
        let mut narrow = Vec::new();
        build(
            &Buffer::from_text("aax"),
            &mut atlas,
            vp,
            &Theme::default(),
            &mut narrow,
        );

        let x_wide = wide.last().expect("drew something").pos[0];
        let x_narrow = narrow.last().expect("drew something").pos[0];
        assert!(
            (x_wide - x_narrow).abs() < advance * 0.01,
            "a CJK character should occupy two columns"
        );
    }

    #[test]
    fn clicking_maps_back_to_the_caret_position() {
        let atlas = atlas();
        let buffer = Buffer::from_text("hello world\nsecond line\nthird");
        let m = atlas.metrics;
        let gutter = gutter_width(&buffer, &atlas);

        // Middle of the 7th character on line 1 (0-based).
        let x = gutter + 6.5 * m.advance;
        let y = 1.5 * m.line_height;
        let offset = offset_at_point(&buffer, &atlas, x, y);
        assert_eq!(buffer.position_of(offset), (1, 7));
    }

    #[test]
    fn clicking_past_the_end_of_a_line_clamps_to_it() {
        let atlas = atlas();
        let buffer = Buffer::from_text("ab\nlonger line here\n");
        let gutter = gutter_width(&buffer, &atlas);
        // Far to the right of a two-character line.
        let offset = offset_at_point(&buffer, &atlas, gutter + 400.0, 0.0);
        assert_eq!(
            buffer.position_of(offset),
            (0, 2),
            "should stop at the line end"
        );
    }

    #[test]
    fn clicking_in_the_gutter_lands_at_column_zero() {
        let atlas = atlas();
        let buffer = Buffer::from_text("hello\nworld");
        let offset = offset_at_point(&buffer, &atlas, 0.0, 0.0);
        assert_eq!(buffer.position_of(offset), (0, 0));
    }

    #[test]
    fn hit_testing_accounts_for_tabs() {
        let atlas = atlas();
        let buffer = Buffer::from_text("\tx");
        let m = atlas.metrics;
        let gutter = gutter_width(&buffer, &atlas);
        // Column 4 is where 'x' renders, after the tab expands.
        let offset = offset_at_point(&buffer, &atlas, gutter + 4.0 * m.advance, 0.0);
        assert_eq!(offset, 1, "should land between the tab and the x");
    }

    #[test]
    fn selection_draws_a_band_per_visible_line() {
        let mut atlas = atlas();
        let mut buffer = Buffer::from_text("one\ntwo\nthree\n");
        buffer.select_all();
        let vp = Viewport::new(800.0, 600.0);
        let mut out = Vec::new();
        let theme = Theme::default();
        build(&buffer, &mut atlas, vp, &theme, &mut out);

        let bands = out.iter().filter(|q| q.color == theme.selection).count();
        assert_eq!(bands, 3, "one band per selected line with content");
    }

    #[test]
    fn no_selection_means_no_bands() {
        let mut atlas = atlas();
        let buffer = Buffer::from_text("one\ntwo\n");
        let vp = Viewport::new(800.0, 600.0);
        let mut out = Vec::new();
        let theme = Theme::default();
        build(&buffer, &mut atlas, vp, &theme, &mut out);
        assert_eq!(out.iter().filter(|q| q.color == theme.selection).count(), 0);
    }

    #[test]
    fn empty_viewport_produces_nothing() {
        let mut atlas = atlas();
        let buffer = Buffer::from_text("text");
        let vp = Viewport::new(800.0, 0.0);
        let mut out = Vec::new();
        let stats = build(&buffer, &mut atlas, vp, &Theme::default(), &mut out);
        assert_eq!(stats.quads, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn a_click_inside_a_tab_goes_to_its_nearer_edge() {
        let atlas = atlas();
        let m = atlas.metrics;
        let buffer = Buffer::from_text("\tx\n\u{6f22}y\n");
        let gutter = gutter_width(&buffer, &atlas);
        let at = |column: f32, row: f32| {
            offset_at_point(
                &buffer,
                &atlas,
                gutter + column * m.advance,
                row * m.line_height + 1.0,
            )
        };
        // A tab spans columns 0 to 4.
        assert_eq!(at(1.0, 0.0), 0, "the front of the tab");
        assert_eq!(at(3.0, 0.0), 1, "the back of it");
        assert_eq!(at(5.0, 0.0), 2, "and past the x after it");
        // A wide character spans two. Clicks are rounded to a column first.
        // The second line starts at byte 3, after the tab, the x and the
        // newline.
        assert_eq!(at(0.4, 1.0), 3);
        assert_eq!(at(1.6, 1.0), 6, "the far side of a three-byte character");
    }

    // ---- chrome ----------------------------------------------------------

    fn tiles(chrome: &Chrome) -> Vec<Viewport> {
        let mut out = vec![
            chrome.toolbar,
            chrome.tabs,
            chrome.breadcrumbs,
            chrome.text,
            chrome.status,
        ];
        out.extend(chrome.sidebar);
        out.extend(chrome.find);
        out
    }

    #[test]
    fn chrome_rects_never_overlap_and_stay_inside_the_window() {
        let windows = [
            (1100.0, 760.0),
            (400.0, 300.0),
            (200.0, 60.0),
            (90.0, 20.0),
            (0.0, 0.0),
        ];
        for (w, h) in windows {
            for sidebar in [None, Some(240.0), Some(600.0)] {
                for find_rows in 0..=2 {
                    let window = Viewport::new(w, h);
                    let chrome = Chrome::new(window, sidebar, find_rows);
                    let tiles = tiles(&chrome);
                    for (i, a) in tiles.iter().enumerate() {
                        assert!(a.width >= 0.0 && a.height >= 0.0, "{a:?}");
                        assert!(a.x >= 0.0 && a.y >= 0.0, "{a:?}");
                        assert!(
                            a.x + a.width <= w + 0.01 && a.y + a.height <= h + 0.01,
                            "{a:?}"
                        );
                        for b in &tiles[i + 1..] {
                            let apart = a.x + a.width <= b.x
                                || b.x + b.width <= a.x
                                || a.y + a.height <= b.y
                                || b.y + b.height <= a.y;
                            let empty = a.width * a.height == 0.0 || b.width * b.height == 0.0;
                            assert!(apart || empty, "{a:?} overlaps {b:?} in {w}x{h}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn chrome_stacks_tabs_then_find_then_text() {
        let chrome = Chrome::new(Viewport::new(1100.0, 760.0), Some(240.0), 2);
        let find = chrome.find.expect("find bar");
        assert_eq!(chrome.tabs.y, TOOLBAR_HEIGHT, "tabs follow the toolbar");
        assert_eq!(
            find.y,
            TOOLBAR_HEIGHT + TAB_BAR_HEIGHT + BREADCRUMB_HEIGHT,
            "find sits under the tabs, not over them"
        );
        assert_eq!(find.height, 2.0 * FIND_ROW_HEIGHT);
        assert_eq!(
            chrome.text.y,
            TOOLBAR_HEIGHT
                + TAB_BAR_HEIGHT
                + BREADCRUMB_HEIGHT
                + 2.0 * FIND_ROW_HEIGHT
                + TEXT_TOP_PAD
        );
        assert_eq!(chrome.text.x, 240.0);
        assert_eq!(chrome.text.y + chrome.text.height, 760.0 - STATUS_HEIGHT);
    }

    /// The round trip the mouse depends on: where a character is drawn, a
    /// click must land on that character, wherever the text area happens to
    /// sit. The old tests only ever used a viewport at the origin, where
    /// forgetting to subtract it is invisible.
    #[test]
    fn a_click_lands_on_the_character_drawn_there() {
        let mut atlas = atlas();
        let m = atlas.metrics;
        let buffer = Buffer::from_text("zero\none two\nthree\n");
        let theme = Theme::default();

        for (sidebar, find_rows) in [(None, 0), (Some(240.0), 0), (Some(311.0), 2)] {
            let chrome = Chrome::new(Viewport::new(1100.0, 760.0), sidebar, find_rows);
            let mut out = Vec::new();
            build_full(&buffer, &mut atlas, chrome.text, &theme, "", &[], &mut out);

            // Line 1, column 4 is the `t` of "two".
            let gutter = gutter_width(&buffer, &atlas);
            let (wx, wy) = (
                chrome.text.x + gutter + 4.0 * m.advance + 1.0,
                chrome.text.y + m.line_height + 1.0,
            );
            assert!(chrome.text.contains(wx, wy));
            let (x, y) = chrome.to_text(wx, wy);
            let offset = offset_at_point(&buffer, &atlas, x, y);
            assert_eq!(buffer.rope.slice_to_string(offset..offset + 3), "two");

            // And the glyphs really are where that arithmetic says.
            let first_row_y = out.iter().map(|g| g.pos[1]).fold(f32::INFINITY, f32::min);
            assert!(
                first_row_y >= chrome.text.y && first_row_y < chrome.text.y + m.line_height,
                "text starts at y={first_row_y}, the text area at {}",
                chrome.text.y
            );
        }
    }
}

#[cfg(test)]
mod find_geometry_tests {
    use super::*;

    fn bar(width: f32) -> (Viewport, FindGeometry) {
        let rect = Viewport {
            x: 0.0,
            y: 100.0,
            width,
            height: FIND_ROW_HEIGHT * 2.0,
        };
        (rect, FindGeometry::new(rect))
    }

    /// Controls that overlap are controls that fire the wrong action. The
    /// drawing and the hit testing used to carry separate copies of these
    /// numbers, and the option chips were 62pt on screen against 66pt in the
    /// handler, so a click in the gap still toggled one.
    #[test]
    fn no_two_controls_overlap_at_any_width() {
        for width in [700.0, 900.0, 1100.0, 1600.0, 2400.0] {
            let (_, g) = bar(width);
            let mut controls = vec![
                ("find", g.find_field),
                ("previous", g.previous),
                ("next", g.next),
                ("close", g.close),
                ("replace", g.replace_one),
                ("all", g.replace_all),
            ];
            for (index, chip) in g.options.iter().enumerate() {
                controls.push((["Aa", "Word", ".*", "Project"][index], *chip));
            }
            for (i, (a_name, a)) in controls.iter().enumerate() {
                for (b_name, b) in controls.iter().skip(i + 1) {
                    let overlaps = a.x < b.x + b.width
                        && b.x < a.x + a.width
                        && a.y < b.y + b.height
                        && b.y < a.y + a.height;
                    assert!(!overlaps, "{a_name} overlaps {b_name} at width {width}");
                }
            }
        }
    }

    #[test]
    fn every_control_stays_inside_the_bar() {
        for width in [700.0, 1100.0, 2400.0] {
            let (rect, g) = bar(width);
            for (name, r) in [
                ("find", g.find_field),
                ("replace field", g.replace_field),
                ("previous", g.previous),
                ("next", g.next),
                ("close", g.close),
                ("replace", g.replace_one),
                ("all", g.replace_all),
            ] {
                assert!(r.x >= rect.x, "{name} starts left of the bar");
                assert!(
                    r.x + r.width <= rect.x + rect.width + 0.01,
                    "{name} runs past the right edge at width {width}"
                );
                assert!(r.width > 0.0, "{name} has no width at {width}");
            }
        }
    }

    /// The two fields share a left edge and a width, so the eye has one line
    /// to follow down the bar.
    #[test]
    fn the_two_fields_are_aligned() {
        let (_, g) = bar(1100.0);
        assert_eq!(g.find_field.x, g.replace_field.x);
        assert_eq!(g.find_field.width, g.replace_field.width);
        assert!(g.replace_field.y > g.find_field.y);
    }

    /// Result rows begin below both input rows, so a click on a result can
    /// never be read as a click in a field.
    #[test]
    fn result_rows_start_below_the_inputs() {
        let rect = Viewport {
            x: 0.0,
            y: 0.0,
            width: 1100.0,
            height: FIND_ROW_HEIGHT * 6.0,
        };
        let g = FindGeometry::new(rect);
        assert!(g.results.y >= g.replace_field.y + g.replace_field.height);
        assert_eq!(g.result_row(g.results.y + 1.0), Some(0));
        assert_eq!(g.result_row(g.results.y + FIND_ROW_HEIGHT + 1.0), Some(1));
        assert_eq!(g.result_row(g.find_field.y), None);
    }
}

#[cfg(test)]
mod sidebar_action_tests {
    use super::*;

    fn column() -> Viewport {
        Viewport {
            x: 0.0,
            y: 48.0,
            width: 240.0,
            height: 700.0,
        }
    }

    /// The buttons must not sit on top of the switcher above them or the
    /// first tree row below them, or a click creates a file when it meant to
    /// change view.
    #[test]
    fn the_action_row_sits_between_the_switcher_and_the_tree() {
        let column = column();
        let (label, actions) = sidebar_actions(column);
        let (explorer, source) = sidebar_switcher(column);
        for (name, r) in [("label", label)]
            .into_iter()
            .chain(actions.iter().enumerate().map(|(i, r)| match i {
                0 => ("new file", *r),
                1 => ("new folder", *r),
                2 => ("collapse", *r),
                _ => ("refresh", *r),
            }))
        {
            assert!(
                r.y >= explorer.y + explorer.height,
                "{name} overlaps the switcher"
            );
            assert!(
                r.y + r.height <= column.y + SIDEBAR_HEADER_HEIGHT,
                "{name} overlaps the first tree row"
            );
            assert!(r.x >= column.x && r.x + r.width <= column.x + column.width + 0.01);
        }
        assert_eq!(explorer.y, source.y);
    }

    /// Every button is the same square on a single pitch, and the row ends
    /// flush with the switcher above it. Two pills of different
    /// widths floating at the right read as an accident, which is what they
    /// were.
    #[test]
    fn the_buttons_are_identical_squares_on_one_pitch() {
        let column = column();
        let (label, actions) = sidebar_actions(column);
        for r in &actions {
            assert_eq!(r.width, SIDEBAR_ACTION);
            assert_eq!(r.height, SIDEBAR_ACTION);
            assert_eq!(r.y, actions[0].y, "the row is not level");
        }
        let pitch = actions[1].x - actions[0].x;
        for pair in actions.windows(2) {
            assert!(
                (pair[1].x - pair[0].x - pitch).abs() < 0.01,
                "uneven spacing between buttons"
            );
        }
        // Flush with the switcher's trailing edge, which is the card's too.
        let (_, source) = sidebar_switcher(column);
        let last = actions[3];
        assert!(
            (last.x + last.width - (source.x + source.width)).abs() < 0.01,
            "the action row does not end where the switcher does"
        );
        assert!(
            label.x + label.width <= actions[0].x,
            "label runs into the buttons"
        );
    }

    /// A narrow sidebar must not produce negative or inverted rectangles.
    #[test]
    fn a_narrow_sidebar_keeps_the_rectangles_sane() {
        for width in [160.0, 200.0, 240.0, 400.0] {
            let (label, actions) = sidebar_actions(Viewport { width, ..column() });
            assert!(label.width >= 0.0, "negative label width at {width}");
            for r in &actions {
                assert!(r.width > 0.0 && r.height > 0.0);
            }
        }
    }
}

#[cfg(test)]
mod scrollbar_tests {
    use super::*;

    /// Exactly `lines` lines: the trailing newline would otherwise add one.
    fn long(lines: usize) -> Buffer {
        Buffer::from_text(&"x\n".repeat(lines - 1))
    }

    const VIEW: Viewport = Viewport {
        x: 100.0,
        y: 50.0,
        width: 600.0,
        height: 400.0,
    };

    #[test]
    fn a_document_that_fits_has_no_thumb() {
        assert!(scrollbar_thumb(&long(10), VIEW, 20.0).is_none());
        assert!(
            scrollbar_thumb(&long(20), VIEW, 20.0).is_none(),
            "exactly fits"
        );
    }

    #[test]
    fn the_thumb_spans_the_track_from_top_to_bottom_as_the_document_scrolls() {
        let mut buffer = long(2000);
        let track = scrollbar_track(VIEW);
        let top = scrollbar_thumb(&buffer, VIEW, 20.0).expect("thumb");
        assert_eq!(top.y, track.y);
        assert_eq!(
            top.height, SCROLLBAR_MIN_THUMB,
            "20 of 2000 rows, clamped up"
        );
        assert!(top.x + top.width <= VIEW.x + VIEW.width);

        buffer.scroll_by(isize::MAX / 2, 20);
        let bottom = scrollbar_thumb(&buffer, VIEW, 20.0).expect("thumb");
        assert!((bottom.y + bottom.height - (track.y + track.height)).abs() < 0.01);
    }

    #[test]
    fn dragging_the_thumb_back_to_where_it_is_drawn_gives_the_same_line() {
        let mut buffer = long(500);
        for line in [0usize, 7, 123, 480] {
            buffer.scroll_to(line, 20);
            let thumb = scrollbar_thumb(&buffer, VIEW, 20.0).expect("thumb");
            assert_eq!(scrollbar_line_at(&buffer, VIEW, 20.0, thumb.y), line);
        }
        assert_eq!(scrollbar_line_at(&buffer, VIEW, 20.0, -1000.0), 0);
        assert_eq!(
            scrollbar_line_at(&buffer, VIEW, 20.0, 1e6),
            480,
            "500 lines, 20 rows"
        );
    }
}
