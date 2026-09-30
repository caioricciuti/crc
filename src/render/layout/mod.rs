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

use crate::text::columns::TAB_WIDTH;

mod chrome;
mod draw;
mod editor;
mod markdown;
mod theme;
pub use chrome::*;
pub use draw::*;
pub use editor::*;
pub use markdown::*;
pub use theme::*;

/// A rectangle of the window, in logical points, that something draws into.
///
/// Panels need an origin now that the sidebar exists; the editor no longer
/// starts at x = 0.
#[derive(Clone, Copy, Debug, PartialEq)]
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

#[cfg(test)]
mod guide_tests {
    use super::*;
    use crate::text::buffer::Motion;

    #[test]
    fn indent_is_counted_in_columns_and_blank_lines_have_none() {
        let b = Buffer::from_text("fn a() {\n    x;\n\t\ty;\n  \n}\n");
        assert_eq!(b.indent_columns(0), Some(0));
        assert_eq!(b.indent_columns(1), Some(4));
        assert_eq!(b.indent_columns(2), Some(8), "two tabs");
        assert_eq!(b.indent_columns(3), None, "spaces only");
        assert_eq!(b.indent_columns(5), None, "the empty last line");
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
        let indents: Vec<_> = lines.iter().map(|&l| b.indent_columns(l)).collect();
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
    fn the_preview_takes_the_right_half_of_the_focused_text() {
        let window = Viewport {
            x: 0.0,
            y: 0.0,
            width: 1201.0,
            height: 800.0,
        };
        let plain = Chrome::new(window, None, 0);
        let mut split = Chrome::new(window, None, 0);
        split.split_preview();
        let preview = split.preview.unwrap();
        assert_eq!(split.text.x, plain.text.x);
        assert_eq!(split.text.width.fract(), 0.0, "text on a whole point");
        assert_eq!(preview.x, split.text.x + split.text.width + PANE_GAP);
        assert_eq!(preview.x + preview.width, plain.text.x + plain.text.width);
        assert_eq!(
            (preview.y, preview.height),
            (plain.text.y, plain.text.height)
        );
        assert!(plain.preview.is_none());
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
        let _ = atlas;
        let row = |n: f32| SIDEBAR_HEADER_HEIGHT + SIDEBAR_ROW_HEIGHT * n + 13.0;
        assert!(sidebar_row_at(&tree, None, rect, 20.0).is_none());
        assert_eq!(sidebar_row_at(&tree, None, rect, row(0.0)), Some(0));
        assert_eq!(sidebar_row_at(&tree, None, rect, row(1.0)), Some(1));
        // New File's field inserted at visible row 1: row 1 is the field,
        // and what was row 1 is drawn, and hit, one lower.
        let new_file = Some(SidebarField {
            row: 1,
            inserted: true,
        });
        assert_eq!(sidebar_row_at(&tree, new_file, rect, row(0.0)), Some(0));
        assert_eq!(sidebar_row_at(&tree, new_file, rect, row(1.0)), None);
        assert_eq!(sidebar_row_at(&tree, new_file, rect, row(2.0)), Some(1));
        // Rename covers its row and moves nothing.
        let rename = Some(SidebarField {
            row: 1,
            inserted: false,
        });
        assert_eq!(sidebar_row_at(&tree, rename, rect, row(1.0)), None);
        assert_eq!(sidebar_row_at(&tree, rename, rect, row(2.0)), Some(2));
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

    #[test]
    fn the_strip_keeps_its_start_while_the_active_tab_shows() {
        let tab = |index: usize| {
            let mut buffer = Buffer::from_text("");
            buffer.path = Some(format!("/tmp/caio-strip-{index}.md").into());
            buffer
        };
        let mut docs = Documents::new(tab(0));
        for index in 1..10 {
            docs.add(tab(index));
        }
        assert_eq!(docs.len(), 10);
        let advance = 8.0;
        let one = tab_width(&docs, 0, advance);
        docs.switch(4);
        assert_eq!(
            tab_strip_start(&docs, 2, one * 10.0, advance),
            2,
            "4 shows from 2"
        );
        assert_eq!(
            tab_strip_start(&docs, 6, one * 10.0, advance),
            0,
            "4 is before 6"
        );
        // Room for three: the active tab and the two before it.
        assert_eq!(tab_strip_start(&docs, 0, one * 3.0, advance), 2);
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
                offset_at_point(&far, &atlas, &Markdown::default(), x as f32, 2.0),
                prefix.len() + offset_at_point(&near, &atlas, &Markdown::default(), x as f32, 2.0)
            );
        }
        let a = caret_rect(&far, &atlas, &Markdown::default(), viewport).unwrap();
        let b = caret_rect(&near, &atlas, &Markdown::default(), viewport).unwrap();
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
        let rect = caret_rect(&buffer, &atlas, &Markdown::default(), viewport).unwrap();
        assert_eq!(
            offset_at_point(&buffer, &atlas, &Markdown::default(), rect.x, rect.y + 2.0),
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
        let rect = caret_rect(&buffer, &atlas, &Markdown::default(), viewport).unwrap();
        assert_eq!(
            offset_at_point(&buffer, &atlas, &Markdown::default(), rect.x, rect.y + 2.0),
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
            offset_at_point(&buffer, &atlas, &Markdown::default(), 32.0, 2.0),
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
                    offset_at_point(&buffer, &atlas, &Markdown::default(), screen_x, 2.0),
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
        let rect = caret_rect(&buffer, &atlas, &Markdown::default(), viewport).unwrap();
        assert_eq!(
            offset_at_point(&buffer, &atlas, &Markdown::default(), rect.x, rect.y + 2.0),
            2
        );
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
    fn a_wide_code_block_stays_inside_the_preview() {
        let mut atlas = atlas();
        let source = format!("```\n{}\n```\n", "x".repeat(400));
        let blocks = crate::markdown::parse_spanned(&source);
        let mut glyphs = Vec::new();
        let view = Viewport {
            x: 100.0,
            y: 50.0,
            width: 300.0,
            height: 200.0,
        };
        build_markdown_appending(&blocks, 0, &mut atlas, view, &Theme::default(), &mut glyphs);
        assert!(!glyphs.is_empty());
        for quad in &glyphs {
            assert!(
                quad.pos[0] >= view.x - 0.01
                    && quad.pos[0] + quad.size[0] <= view.x + view.width + 0.01
            );
            assert!(
                quad.pos[1] >= view.y - 0.01
                    && quad.pos[1] + quad.size[1] <= view.y + view.height + 0.01
            );
        }
    }

    #[test]
    fn a_word_too_long_to_shape_still_takes_its_room() {
        let mut atlas = atlas();
        let url = format!("https://example.invalid/{}", "a".repeat(700));
        let source = format!("{url} after\n");
        let blocks = crate::markdown::parse_spanned(&source);
        let mut glyphs = Vec::new();
        let view = Viewport::new(800.0, 2000.0);
        build_markdown_appending(&blocks, 0, &mut atlas, view, &Theme::default(), &mut glyphs);
        // The word wraps over several rows rather than running off the
        // side, and "after" comes after it.
        let rows: std::collections::BTreeSet<i64> =
            glyphs.iter().map(|q| q.pos[1] as i64).collect();
        assert!(rows.len() > 2, "the long word wraps");
        assert!(
            glyphs
                .iter()
                .all(|q| q.pos[0] + q.size[0] <= view.width + 0.01)
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
            buffer.rope.visual_column(line_start..buffer.cursor()),
            4,
            "cursor ignored tab expansion"
        );
        buffer.move_right(Motion::Move);
        assert_eq!(buffer.rope.visual_column(line_start..buffer.cursor()), 5);
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
        let offset = offset_at_point(&buffer, &atlas, &Markdown::default(), x, y);
        assert_eq!(buffer.position_of(offset), (1, 7));
    }

    #[test]
    fn clicking_past_the_end_of_a_line_clamps_to_it() {
        let atlas = atlas();
        let buffer = Buffer::from_text("ab\nlonger line here\n");
        let gutter = gutter_width(&buffer, &atlas);
        // Far to the right of a two-character line.
        let offset = offset_at_point(&buffer, &atlas, &Markdown::default(), gutter + 400.0, 0.0);
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
        let offset = offset_at_point(&buffer, &atlas, &Markdown::default(), 0.0, 0.0);
        assert_eq!(buffer.position_of(offset), (0, 0));
    }

    #[test]
    fn markdown_hides_syntax_off_the_caret_line() {
        let atlas = atlas();
        let text = "x\n**bold** end\n";
        let styled = crate::markdown::source::style(text);
        let markdown = Markdown::of(Some(&styled));
        let mut buffer = Buffer::from_text(text);
        let m = atlas.metrics;
        let gutter = gutter_width(&buffer, &atlas);
        let y = m.line_height + 2.0;
        let at = |buffer: &Buffer, column: f32| {
            offset_at_point(buffer, &atlas, &markdown, gutter + column * m.advance, y)
        };
        // Drawn as "bold end": the stars take no room.
        assert_eq!(at(&buffer, 0.0), 2, "the start of the hidden stars");
        assert_eq!(at(&buffer, 2.0), 6, "between o and l");
        assert_eq!(at(&buffer, 5.0), 11, "after the space");
        // With the caret on it the line is drawn as written.
        buffer.place_cursor(11, crate::text::buffer::Motion::Move);
        assert_eq!(at(&buffer, 2.0), 4, "before b, after the stars");
        let viewport = Viewport::new(800.0, 600.0);
        let rect = caret_rect(&buffer, &atlas, &markdown, viewport).unwrap();
        assert!((rect.x - (gutter + 9.0 * m.advance)).abs() < 0.01);
    }

    #[test]
    fn markdown_table_cells_pad_to_their_column() {
        let atlas = atlas();
        let text = "| a | bbb |\n|---|---|\n| cc | d |\n";
        let styled = crate::markdown::source::style(text);
        let markdown = Markdown::of(Some(&styled));
        let mut buffer = Buffer::from_text(text);
        let m = atlas.metrics;
        let gutter = gutter_width(&buffer, &atlas);
        // After "| a |", which a blank column widens to "| cc |": the pads
        // hold on the caret's own line too.
        buffer.place_cursor(5, crate::text::buffer::Motion::Move);
        let rect = caret_rect(&buffer, &atlas, &markdown, Viewport::new(800.0, 600.0)).unwrap();
        assert!((rect.x - (gutter + 6.0 * m.advance)).abs() < 0.01);
        // Before the pipe: the caret stays by the text, not the pad.
        buffer.place_cursor(4, crate::text::buffer::Motion::Move);
        let rect = caret_rect(&buffer, &atlas, &markdown, Viewport::new(800.0, 600.0)).unwrap();
        assert!((rect.x - (gutter + 4.0 * m.advance)).abs() < 0.01);
        // A click on the pad lands at the pipe or after it.
        let at = offset_at_point(&buffer, &atlas, &markdown, gutter + 5.0 * m.advance, 2.0);
        assert!(matches!(at, 4 | 5), "{at}");
    }

    #[test]
    fn markdown_hides_syntax_on_a_shaped_line_too() {
        let mut atlas = atlas();
        // "é" makes the line shaped by CoreText rather than drawn by cells.
        let text = "x\n**café** fim\n";
        let styled = crate::markdown::source::style(text);
        let markdown = Markdown::of(Some(&styled));
        let mut buffer = Buffer::from_text(text);
        let range = buffer.rope.line_to_byte(1)..buffer.rope.line_to_byte(2);
        // Measured by CoreText's own advances, not the rounded grid's.
        let shaped = atlas
            .shape_editor_line((buffer.id(), 1), &buffer.rope, range)
            .unwrap();
        let m = atlas.metrics;
        let gutter = gutter_width(&buffer, &atlas);
        let y = m.line_height + 2.0;
        let at = |buffer: &Buffer, column: f32| {
            offset_at_point(buffer, &atlas, &markdown, gutter + column * m.advance, y)
        };
        // Drawn as "café fim": the stars take no room.
        assert_eq!(at(&buffer, 0.0), 2, "the start of the hidden stars");
        assert_eq!(at(&buffer, 2.0), 6, "between a and f");
        assert_eq!(at(&buffer, 4.0), 9, "after é, before the closing stars");
        assert_eq!(at(&buffer, 5.0), 12, "after the space");
        // With the caret on it the line is drawn as written.
        buffer.place_cursor(12, crate::text::buffer::Motion::Move);
        assert_eq!(at(&buffer, 2.0), 4, "before c, after the stars");
        let rect = caret_rect(&buffer, &atlas, &markdown, Viewport::new(800.0, 600.0)).unwrap();
        assert!(
            (rect.x - (gutter + shaped.x_of_byte(10))).abs() < 0.01,
            "{}",
            rect.x
        );
    }

    #[test]
    fn markdown_table_cells_pad_on_a_shaped_line_too() {
        let mut atlas = atlas();
        let text = "| é | bbb |\n|---|---|\n| cc | d |\n";
        let styled = crate::markdown::source::style(text);
        let markdown = Markdown::of(Some(&styled));
        let mut buffer = Buffer::from_text(text);
        let range = 0..buffer.rope.line_to_byte(1);
        // Measured by CoreText's own advances, not the rounded grid's.
        let shaped = atlas
            .shape_editor_line((buffer.id(), 0), &buffer.rope, range)
            .unwrap();
        let m = atlas.metrics;
        let gutter = gutter_width(&buffer, &atlas);
        let viewport = Viewport::new(800.0, 600.0);
        // After "| é |", which a blank column widens to "| cc |".
        buffer.place_cursor(6, crate::text::buffer::Motion::Move);
        let rect = caret_rect(&buffer, &atlas, &markdown, viewport).unwrap();
        let after_pad = gutter + shaped.x_of_byte(6) + m.advance;
        assert!((rect.x - after_pad).abs() < 0.01, "{}", rect.x);
        // Before the pipe: the caret stays by the text, not the pad.
        buffer.place_cursor(5, crate::text::buffer::Motion::Move);
        let rect = caret_rect(&buffer, &atlas, &markdown, viewport).unwrap();
        assert!(
            (rect.x - (gutter + shaped.x_of_byte(5))).abs() < 0.01,
            "{}",
            rect.x
        );
    }

    #[test]
    fn a_right_to_left_line_keeps_its_markdown_syntax() {
        let mut atlas = atlas();
        let text = "x\n**שלום** a\n";
        let styled = crate::markdown::source::style(text);
        let markdown = Markdown::of(Some(&styled));
        let buffer = Buffer::from_text(text);
        let range = buffer.rope.line_to_byte(1)..buffer.rope.line_to_byte(2);
        let shaped = atlas
            .shape_editor_line((buffer.id(), 1), &buffer.rope, range)
            .unwrap();
        let map = markdown.line(&buffer, &[], 1);
        assert!(
            map.is_some(),
            "the stars are hidden on a left-to-right line"
        );
        let placed = editor::Placed::new(&shaped, &buffer, 2, map, atlas.metrics.advance);
        assert!(!placed.remapped());
        assert!(!placed.hides(2));
    }

    #[test]
    fn hit_testing_accounts_for_tabs() {
        let atlas = atlas();
        let buffer = Buffer::from_text("\tx");
        let m = atlas.metrics;
        let gutter = gutter_width(&buffer, &atlas);
        // Column 4 is where 'x' renders, after the tab expands.
        let offset = offset_at_point(
            &buffer,
            &atlas,
            &Markdown::default(),
            gutter + 4.0 * m.advance,
            0.0,
        );
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
    fn a_selected_empty_line_shows_its_newline() {
        let mut atlas = atlas();
        let mut buffer = Buffer::from_text("one\n\nthree\n");
        buffer.select_all();
        let mut out = Vec::new();
        let theme = Theme::default();
        build(
            &buffer,
            &mut atlas,
            Viewport::new(800.0, 600.0),
            &theme,
            &mut out,
        );
        let rows: std::collections::BTreeSet<i64> = out
            .iter()
            .filter(|q| q.color == theme.selection)
            .map(|q| q.pos[1] as i64)
            .collect();
        assert_eq!(rows.len(), 3, "the empty middle line is marked too");
    }

    /// The selection bands in `out`, as (row top, left, width), in order.
    fn bands(out: &[GlyphInstance], theme: &Theme) -> Vec<(i64, f32, f32)> {
        let mut bands: Vec<_> = out
            .iter()
            .filter(|q| q.color == theme.selection)
            .map(|q| (q.pos[1] as i64, q.pos[0], q.size[0]))
            .collect();
        bands.sort_by(|a, b| a.partial_cmp(b).unwrap());
        bands
    }

    #[test]
    fn a_selection_ending_at_the_next_line_start_marks_the_newline() {
        let mut atlas = atlas();
        let theme = Theme::default();
        let half = atlas.metrics.advance * 0.5;
        let draw = |atlas: &mut Atlas, text: &str, from: usize, to: usize| {
            let mut buffer = Buffer::from_text(text);
            buffer.select_range(from, to);
            let mut out = Vec::new();
            build(
                &buffer,
                atlas,
                Viewport::new(800.0, 600.0),
                &theme,
                &mut out,
            );
            bands(&out, &theme)
        };
        let advance = atlas.metrics.advance;
        // "ab\n" selected the way Shift-Down from column 0 does: its text
        // and half a cell for the newline, nothing on the next line.
        let one = draw(&mut atlas, "ab\ncd\n", 0, 3);
        assert_eq!(one.len(), 1, "{one:?}");
        assert!((one[0].2 - (2.0 * advance + half)).abs() < 0.01, "{one:?}");
        // An empty line taken the same way: its newline shows.
        let empty = draw(&mut atlas, "ab\n\ncd\n", 3, 4);
        assert_eq!(empty.len(), 1, "{empty:?}");
        assert!((empty[0].2 - half).abs() < 0.01, "{empty:?}");
        // A selection starting at the next line's start: this line's
        // newline is not in it, so nothing is drawn on this line.
        let next = draw(&mut atlas, "ab\ncd\n", 3, 5);
        assert_eq!(next.len(), 1, "{next:?}");
        assert!(
            (next[0].2 - 2.0 * advance).abs() < 0.01,
            "only 'cd': {next:?}"
        );
    }

    #[test]
    fn the_scrollbar_counts_rows_when_wrapping_and_skips_folds() {
        let vp = Viewport::new(400.0, 200.0);
        let line_height = 20.0;
        // Three lines, each wrapping into many rows: taller than the view.
        let mut wrapped = Buffer::from_text(&format!("{0}\n{0}\n{0}\n", "word ".repeat(100)));
        wrapped.wrap = Some(40);
        assert!(scrollbar_thumb(&wrapped, vp, line_height).is_some());
        // A fold hides lines: fewer to scroll through.
        let text: String = (0..40)
            .map(|i| {
                if i % 20 == 0 {
                    "fn f() {\n".to_string()
                } else {
                    "    x;\n".to_string()
                }
            })
            .collect();
        let mut folded = Buffer::from_text(&text);
        let open = scrollbar_thumb(&folded, vp, line_height).unwrap();
        folded.fold(0);
        let closed = scrollbar_thumb(&folded, vp, line_height).unwrap();
        assert!(closed.height > open.height);
        let (shown, _, _) = scroll_extent(&folded);
        assert_eq!(line_at_unit(&folded, 1, &[]), 20, "the line after the fold");
        assert!(shown < 41);
    }

    #[test]
    fn ellipsis_budgets_cells_and_keeps_joined_emoji_whole() {
        let wide: Vec<char> = "日本語のファイル名.txt".chars().collect();
        // Four cells hold two wide characters, not four.
        assert_eq!(fit_cells(wide.iter(), 4), 2);
        let family: Vec<char> = "a👨\u{200d}👩\u{200d}👧b".chars().collect();
        let n = fit_cells(family.iter(), 4);
        assert!(
            n == 1 || n == family.len() - 1,
            "not inside the family: {n}"
        );
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
                &Markdown::default(),
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
        assert_eq!(
            chrome.text.x,
            ACTIVITY_WIDTH + 240.0,
            "after the strip and the sidebar"
        );
        assert_eq!(chrome.activity.x, 0.0);
        assert_eq!(chrome.activity.width, ACTIVITY_WIDTH);
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
            let offset = offset_at_point(&buffer, &atlas, &Markdown::default(), x, y);
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
