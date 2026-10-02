//! Colours: the dark and light tables and what the window draws with.

use super::*;

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
    /// A control's resting fill: buttons, menu boxes, the tab under the
    /// pointer.
    pub tab_hover: [f32; 4],
    /// A control under the pointer, and one held down.
    pub control_hover: [f32; 4],
    pub control_pressed: [f32; 4],
    /// The wash behind a list row under the pointer.
    pub row_hover: [f32; 4],
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
    /// The incoming side of a merge conflict; the current side is drawn in
    /// `diff_added`, the way Git's own tools pair them.
    pub conflict_incoming: [f32; 4],
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
            // Markdown: the syntax faint, what it makes in the text's own
            // colour with the weight doing the work, and the few things
            // that are not prose (code, links, list markers) set apart.
            Kind::MdMarker | Kind::MdUrl | Kind::MdStrike => self.gutter_text,
            Kind::MdHeading | Kind::MdList => self.accent,
            Kind::MdStrong
            | Kind::MdEmphasis
            | Kind::MdStrongEmphasis
            | Kind::MdCodeBlock
            | Kind::MdTableHeader => self.text,
            Kind::MdCode => self.syn_string,
            Kind::MdLink => self.syn_function,
            Kind::MdQuote => self.syn_comment,
            Kind::MdRule => self.md_rule,
        }
    }
}

impl Theme {
    /// The window background as the other colours are given, for a field
    /// drawn in it.
    pub fn background_f32(&self) -> [f32; 4] {
        self.background.map(|c| c as f32)
    }

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
            control_hover: [0.847, 0.863, 0.863, 1.0],
            control_pressed: [0.800, 0.816, 0.816, 1.0],
            row_hover: [0.000, 0.000, 0.000, 0.045],
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
            conflict_incoming: [0.149, 0.400, 0.780, 1.0],

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
            control_hover: [0.204, 0.227, 0.231, 1.0],
            control_pressed: [0.243, 0.271, 0.275, 1.0],
            row_hover: [1.000, 1.000, 1.000, 0.045],
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
            conflict_incoming: [0.537, 0.706, 0.918, 1.0],
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
                theme.conflict_incoming,
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
