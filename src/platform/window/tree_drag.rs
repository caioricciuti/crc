//! Dragging in the Explorer's tree: what a press on a row carries, where it
//! may be dropped, and the drop.
//!
//! A plain press on a row does nothing but select it until it is let go:
//! let go where it was pressed it is a click, which opens the file or opens
//! and closes the folder, and moved first it is a drag, which opens
//! nothing. A drag that starts on one of the selected rows carries all of
//! them; on any other row, that row. Let go over a folder it moves them
//! there, or
//! copies them while Option is held, which is read as the key goes down and
//! up and not only when the pointer moves. A closed folder the drag rests
//! on opens, and the tree scrolls while the pointer is near its top or its
//! bottom: both by the frame, since a pointer held still sends no events.
//! Escape calls the drag off.
//!
//! Where the items and the folder under the pointer really are is looked
//! up once each (see [`DropCheck`]); a move of the pointer compares paths.

use super::*;
use crate::project::fileops::{DropCheck, Planned};
use crate::project::tree::DropOn;

/// How far the pointer travels from the press before it is a drag: one that
/// started on every click would make selecting a file impossible.
const DRAG_THRESHOLD: f32 = 4.0;

/// How long a drag rests on a closed folder before the folder opens.
const HOVER_EXPAND: Duration = Duration::from_millis(600);

/// A test instance's own [`HOVER_EXPAND`], in milliseconds, so that a
/// script does not have to wait the real one out.
const HOVER_EXPAND_ENV: &str = "CRC_SELFTEST_HOVER_MS";

/// How near the top or the bottom of the rows a drag scrolls them.
const EDGE_BAND: f32 = 28.0;

/// Rows a second where the band begins, and at the edge and past it.
const EDGE_SLOWEST: f32 = 4.0;
const EDGE_FASTEST: f32 = 40.0;

/// The widest the name beside the pointer is; a longer one loses its
/// middle, so that its extension still shows.
const LABEL_WIDTH: f32 = 180.0;

/// How far on from the pointer the chip is told the pointer is. The chip
/// hangs below and right of the point it is given; this much further, it
/// and the mark of a copy on its corner start a whole row below the
/// pointer and well right of it, so the row being aimed at stays readable
/// wherever in that row the pointer is.
const CHIP_SHIFT: (f32, f32) = (14.0, 19.0);

/// How the chip is laid out from the point it is given, as
/// `layout::push_drag_chip` does it: where its corner hangs, and the room
/// either side of its label together. Only the mark of a copy needs it,
/// to sit on the chip's corner.
const CHIP_HANGS: (f32, f32) = (12.0, 16.0);
const CHIP_PAD: f32 = 16.0;

/// A press on a row of the tree, and the drag it becomes once the pointer
/// has travelled far enough to mean it.
pub(super) struct TreeDrag {
    /// The row pressed: what the press acts on if it never becomes a drag.
    pub(super) path: std::path::PathBuf,
    /// What a drag from that row carries, until it is one.
    items: Vec<std::path::PathBuf>,
    origin: (f32, f32),
    /// The drag, once it is one.
    live: Option<Live>,
}

/// A drag under way.
struct Live {
    /// What is carried and the folder under the pointer, each looked up
    /// once.
    check: DropCheck,
    /// What is carried, as it is said beside the pointer: cut to fit when
    /// the drag starts, so a frame of it cuts nothing again.
    label: String,
    /// How wide that label is, measured then.
    label_width: f32,
    /// Where the pointer is. Kept because a pointer held still sends no
    /// events, and the frame still has to scroll and open folders under it.
    at: (f32, f32),
    /// Option is held: letting go copies.
    copy: bool,
    /// The pointer is over the Explorer, where letting go is a drop.
    inside: bool,
    /// Where the drop would go, while anything would: what lights up.
    on: Option<DropOn>,
    hover: Option<Hover>,
    /// The part of a row the scrolling has not paid out yet.
    carry: f32,
}

/// A drag resting on a closed folder.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Hover {
    row: usize,
    /// When the folder opens; `None` once it has been told to.
    due: Option<Instant>,
}

/// One look at what the drag is over: `row` is the closed folder under the
/// pointer, if there is one. Whether that folder opens now, having been
/// rested on for `delay`. Another row starts the wait again, and a folder
/// is told to open once however long the drag stays on it.
fn hover_step(
    hover: &mut Option<Hover>,
    row: Option<usize>,
    now: Instant,
    delay: Duration,
) -> bool {
    let Some(row) = row else {
        *hover = None;
        return false;
    };
    match hover {
        Some(resting) if resting.row == row => {
            let due = resting.due.is_some_and(|due| now >= due);
            if due {
                resting.due = None;
            }
            due
        }
        _ => {
            *hover = Some(Hover {
                row,
                due: Some(now + delay),
            });
            false
        }
    }
}

/// How long a drag rests on a closed folder before it opens: the real
/// time, or what a test instance was told.
fn hover_expand(testing: bool) -> Duration {
    static SCRIPTED: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    if !testing {
        return HOVER_EXPAND;
    }
    *SCRIPTED.get_or_init(|| {
        std::env::var(HOVER_EXPAND_ENV)
            .ok()
            .and_then(|ms| ms.parse().ok())
            .map_or(HOVER_EXPAND, Duration::from_millis)
    })
}

/// How fast the tree scrolls for a drag at `y`, in rows a second, upwards
/// when negative. The rows are between `top` and `bottom`: nothing in the
/// middle of them, slowly where the band at either end begins, faster
/// toward the edge, and fastest at the edge and past it.
fn edge_speed(y: f32, top: f32, bottom: f32) -> f32 {
    let band = EDGE_BAND.min((bottom - top) * 0.5);
    if band <= 0.0 {
        return 0.0;
    }
    let (depth, direction) = if y < top + band {
        (top + band - y, -1.0)
    } else if y >= bottom - band {
        (y - (bottom - band), 1.0)
    } else {
        return 0.0;
    };
    let share = (depth / band).min(1.0);
    direction * (EDGE_SLOWEST + (EDGE_FASTEST - EDGE_SLOWEST) * share)
}

/// The top left corner of the chip drawn for the point `at`, its label
/// `label_width` wide, kept inside `bounds` as the chip is: where the mark
/// of a copy sits.
fn chip_corner(at: (f32, f32), label_width: f32, bounds: Viewport) -> (f32, f32) {
    let width = (label_width + CHIP_PAD).ceil().min(bounds.width);
    let x = (at.0 + CHIP_HANGS.0).min(bounds.x + bounds.width - width);
    let y = (at.1 + CHIP_HANGS.1).min(bounds.y + bounds.height - layout::UI_CONTROL_SM);
    (x.max(bounds.x), y.max(bounds.y))
}

/// What the pill beside the pointer says: the one item's name, or how many
/// there are.
fn carried_label(items: &[std::path::PathBuf]) -> String {
    match items {
        [item] => super::sidebar::item_name(item),
        _ => format!("{} items", items.len()),
    }
}

impl TreeDrag {
    /// A press on the tree row at `index`, at `origin`.
    pub(super) fn new(tree: &Tree, index: usize, origin: (f32, f32)) -> Option<TreeDrag> {
        let entry = tree.rows().get(index)?;
        Some(TreeDrag {
            path: entry.path.clone(),
            items: tree.dragged_from(index),
            origin,
            live: None,
        })
    }

    /// Past the threshold, so this is a drag rather than a click.
    pub(super) fn active(&self) -> bool {
        self.live.is_some()
    }

    /// The row a release acts on: the one pressed, when the press never
    /// became a drag. A drag opens nothing, wherever it is let go.
    pub(super) fn click(&self) -> Option<&Path> {
        (!self.active()).then_some(self.path.as_path())
    }

    /// The pointer is at `at` with the press held. Far enough from where
    /// it went down, the press becomes a drag of what it carries; whether
    /// it is one now. `label` is asked once, then, for what the chip says
    /// of it: the text cut to fit, and how wide that is.
    fn begin(&mut self, at: (f32, f32), label: impl FnOnce(&str) -> (String, f32)) -> bool {
        if self.live.is_some() {
            return true;
        }
        let far = (at.0 - self.origin.0).abs() > DRAG_THRESHOLD
            || (at.1 - self.origin.1).abs() > DRAG_THRESHOLD;
        if !far || self.items.is_empty() {
            return false;
        }
        let (label, label_width) = label(&carried_label(&self.items));
        self.live = Some(Live {
            check: DropCheck::new(std::mem::take(&mut self.items)),
            label,
            label_width,
            at,
            copy: false,
            inside: false,
            on: None,
            hover: None,
            carry: 0.0,
        });
        true
    }

    /// What the tree lights up: where letting go now would drop.
    pub(super) fn drop_on(&self) -> Option<DropOn> {
        self.live.as_ref().and_then(|live| live.on)
    }

    /// What is carried, beside the pointer, over everything in `bounds`.
    pub(super) fn draw_pill(
        &self,
        out: &mut Vec<GlyphInstance>,
        atlas: &mut Atlas,
        bounds: Viewport,
        theme: &Theme,
    ) {
        let Some(live) = &self.live else {
            return;
        };
        let at = (live.at.0 + CHIP_SHIFT.0, live.at.1 + CHIP_SHIFT.1);
        layout::push_drag_chip(out, atlas, at, bounds, &live.label, theme);
        if live.copy {
            let corner = chip_corner(at, live.label_width, bounds);
            layout::push_copy_mark(out, corner, bounds, theme.palette_background, theme);
        }
    }

    /// The drag as a `dump` says it: `none` for a press that is not one,
    /// otherwise what the pill says, `copy` while Option is held, and the
    /// folder it would drop into by its path in the project (`.` for the
    /// root, `nowhere` where nothing would go).
    pub(super) fn report(&self, root: Option<&Path>) -> String {
        let Some(live) = &self.live else {
            return "none".into();
        };
        let onto = match (live.on, live.check.dir()) {
            (Some(_), Some(dir)) => match root.and_then(|root| dir.strip_prefix(root).ok()) {
                Some(rest) if rest.as_os_str().is_empty() => ".".to_string(),
                Some(rest) => rest.to_string_lossy().into_owned(),
                None => dir.to_string_lossy().into_owned(),
            },
            _ => "nowhere".to_string(),
        };
        format!(
            "{}{} onto {onto}",
            live.label,
            if live.copy { " copy" } else { "" }
        )
    }
}

/// Looks again at what the drag is over, from where the pointer last was:
/// the folder a drop would go into, whether anything would, and the closed
/// folder being rested on, which is told to open once `delay` is up.
/// Whether anything a frame shows changed.
///
/// Asked on every move of the pointer and every frame of the drag. It
/// reads the file system only when the folder under the pointer is another
/// one than last time.
fn aim(state: &mut State, now: Instant, delay: Duration) -> bool {
    let rect = chrome_of(state)
        .sidebar
        .filter(|_| super::sidebar::explorer_showing(state));
    let field = sidebar_field(state);
    let State { drag, tree, .. } = &mut *state;
    let Some(Drag::Tree(TreeDrag {
        live: Some(live), ..
    })) = drag
    else {
        return false;
    };
    let (x, y) = live.at;
    let rect = rect.filter(|rect| rect.contains(x, y));
    let row = rect.and_then(|rect| layout::sidebar_row_at(tree, field, rect, y));
    let aimed = rect.and_then(|_| tree.drop_folder(row));
    live.check.aim(aimed.map(|(_, dir)| dir));
    let on = aimed
        .map(|(on, _)| on)
        .filter(|_| live.check.allowed(live.copy));
    let closed = row.filter(|&row| {
        tree.rows()
            .get(row)
            .is_some_and(|entry| entry.is_dir && !entry.expanded)
    });
    let open = hover_step(&mut live.hover, closed, now, delay)
        .then_some(closed)
        .flatten();
    let changed = live.on != on || live.inside != rect.is_some();
    live.on = on;
    live.inside = rect.is_some();
    if let Some(row) = open {
        super::sidebar::set_tree_folder(state, row, Some(true));
    }
    changed || open.is_some()
}

impl EditorView {
    /// The pointer moved with a row of the tree held. Past the threshold
    /// the press becomes a drag: what it carries is looked up, once, and
    /// from then on the pill follows the pointer and the folder under it
    /// lights up where a drop would go.
    pub(super) fn tree_drag_moved(&self, x: f32, y: f32, flags: NSEventModifierFlags) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let state = &mut *state;
            let Some(Drag::Tree(drag)) = &mut state.drag else {
                return;
            };
            let atlas = &mut state.renderer.atlas;
            let began = drag.begin((x, y), |label| {
                let label = layout::elide_middle(atlas, label, LABEL_WIDTH);
                let width = layout::ui_text_width(atlas, &label);
                (label, width)
            });
            if !began {
                return;
            }
            if let Some(live) = &mut drag.live {
                live.at = (x, y);
                live.copy = flags.contains(NSEventModifierFlags::Option);
            }
            aim(state, Instant::now(), hover_expand(self.ivars().testing));
        }
        // The pill is wherever the pointer is: every move is a frame.
        self.request_redraw();
        self.resume_display_link();
        self.pump();
    }

    /// The modifiers changed while a drag is held, the pointer still or
    /// not: with Option down letting go copies, and the pill says so.
    pub(super) fn tree_drag_flags(&self, flags: NSEventModifierFlags) {
        let copy = flags.contains(NSEventModifierFlags::Option);
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            match &mut state.drag {
                Some(Drag::Tree(TreeDrag {
                    live: Some(live), ..
                })) if live.copy != copy => live.copy = copy,
                _ => return,
            }
            // A copy may go where a move has nothing to do.
            aim(
                &mut state,
                Instant::now(),
                hover_expand(self.ivars().testing),
            );
        }
        self.request_redraw();
        self.pump();
    }

    /// A frame of a drag, `elapsed` after the last one (the display's own
    /// interval when not said): the tree scrolls while the pointer is near
    /// its top or bottom edge, and a closed folder rested on long enough
    /// opens. Whether there is anything new to draw.
    pub(super) fn tree_drag_tick(&self, elapsed: Option<Duration>) -> bool {
        let Some(mut state) = self.state_mut() else {
            return false;
        };
        let state = &mut *state;
        if !matches!(&state.drag, Some(Drag::Tree(drag)) if drag.active()) {
            return false;
        }
        let rect = chrome_of(state)
            .sidebar
            .filter(|_| super::sidebar::explorer_showing(state));
        let elapsed = elapsed.unwrap_or(state.frame_interval);
        let mut scrolled = false;
        if let (
            Some(rect),
            Some(Drag::Tree(TreeDrag {
                live: Some(live), ..
            })),
        ) = (rect, &mut state.drag)
        {
            let (x, y) = live.at;
            let speed = if x >= rect.x && x < rect.x + rect.width {
                edge_speed(
                    y,
                    rect.y + layout::SIDEBAR_HEADER_HEIGHT,
                    rect.y + rect.height,
                )
            } else {
                0.0
            };
            if speed == 0.0 {
                live.carry = 0.0;
            } else {
                live.carry += speed * elapsed.as_secs_f32();
                let rows = live.carry.trunc();
                live.carry -= rows;
                let before = state.tree.scroll;
                state
                    .tree
                    .scroll_by(rows as isize, layout::sidebar_rows(rect));
                scrolled = state.tree.scroll != before;
            }
        }
        // Other rows are under the pointer now, or the same ones for
        // longer.
        let aimed = aim(state, Instant::now(), hover_expand(self.ivars().testing));
        scrolled || aimed
    }

    /// Escape while a drag is held: it is called off, and nothing moves.
    /// Whether there was one.
    pub(super) fn cancel_tree_drag(&self) -> bool {
        {
            let Some(mut state) = self.state_mut() else {
                return false;
            };
            if !matches!(&state.drag, Some(Drag::Tree(drag)) if drag.active()) {
                return false;
            }
            state.drag = None;
        }
        self.request_redraw();
        self.pump();
        true
    }

    /// A drag let go: over the Explorer, what it carries goes into the
    /// folder under the pointer, moved or with Option held copied, each
    /// with its tabs and its local history. Anywhere else nothing happens.
    pub(super) fn drop_tree_items(&self, drag: TreeDrag) {
        let Some(live) = drag.live else {
            return;
        };
        let Some(into) = live.check.dir().filter(|_| live.inside) else {
            return;
        };
        let plan = live.check.plan(live.copy);
        // The tree has the keyboard after a drop that does anything: what
        // landed is selected there, and Escape stops a copy that runs.
        let acts = plan
            .iter()
            .any(|planned| matches!(planned, Planned::Copy { .. } | Planned::Move { .. }));
        if acts && let Some(mut state) = self.state_mut() {
            super::sidebar::take_tree_keys(&mut state);
        }
        self.drop_items(plan, live.copy, into);
        self.request_redraw();
        self.pump();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A drag opens the closed folder it rests on, after the delay and not
    /// before, once, and the wait starts again on another folder.
    #[test]
    fn a_folder_opens_once_the_drag_has_rested_on_it_long_enough() {
        let delay = Duration::from_millis(600);
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut hover = None;
        // Over nothing to open: no wait starts.
        assert!(!hover_step(&mut hover, None, at(0), delay));
        assert_eq!(hover, None);
        // On a closed folder: not at once, not just before, then once.
        assert!(!hover_step(&mut hover, Some(3), at(0), delay));
        assert!(!hover_step(&mut hover, Some(3), at(300), delay));
        assert!(!hover_step(&mut hover, Some(3), at(599), delay));
        assert!(hover_step(&mut hover, Some(3), at(600), delay));
        // Still there while its rows are read: it is not told again.
        assert!(!hover_step(&mut hover, Some(3), at(700), delay));
        assert!(!hover_step(&mut hover, Some(3), at(5000), delay));
        // Passing over another folder on the way starts the wait again
        // from there, and so does coming back.
        assert!(!hover_step(&mut hover, Some(5), at(5100), delay));
        assert!(!hover_step(&mut hover, Some(3), at(5400), delay));
        assert!(!hover_step(&mut hover, Some(3), at(5999), delay));
        assert!(hover_step(&mut hover, Some(3), at(6000), delay));
        // Off the folders, the wait is forgotten: a visit of a moment and
        // another later do not add up.
        assert!(!hover_step(&mut hover, Some(7), at(7000), delay));
        assert!(!hover_step(&mut hover, None, at(7300), delay));
        assert!(!hover_step(&mut hover, Some(7), at(7500), delay));
        assert!(!hover_step(&mut hover, Some(7), at(7700), delay));
        assert!(hover_step(&mut hover, Some(7), at(8100), delay));
        // The real wait, and a test instance's when it is given none.
        assert_eq!(hover_expand(false), Duration::from_millis(600));
    }

    /// The tree scrolls only while the pointer is in a band at its top or
    /// bottom, faster the nearer the edge, and no faster past it.
    #[test]
    fn a_drag_scrolls_the_tree_near_its_edges_faster_toward_them() {
        let (top, bottom) = (88.0, 688.0);
        // The middle, and the last point outside either band.
        for y in [300.0, top + EDGE_BAND, bottom - EDGE_BAND - 0.5] {
            assert_eq!(edge_speed(y, top, bottom), 0.0, "at {y}");
        }
        // Just inside the band: the slowest, up at the top and down at
        // the bottom.
        assert!((edge_speed(top + EDGE_BAND - 0.01, top, bottom) + EDGE_SLOWEST).abs() < 0.1);
        assert!((edge_speed(bottom - EDGE_BAND, top, bottom) - EDGE_SLOWEST).abs() < 0.1);
        // Faster toward the edge, on both sides.
        let down: Vec<f32> = [27.0, 20.0, 10.0, 2.0, 0.0]
            .iter()
            .map(|inside| edge_speed(bottom - inside, top, bottom))
            .collect();
        assert!(down.windows(2).all(|pair| pair[0] < pair[1]), "{down:?}");
        let up: Vec<f32> = [27.0, 20.0, 10.0, 2.0, 0.0]
            .iter()
            .map(|inside| edge_speed(top + inside, top, bottom))
            .collect();
        assert!(up.windows(2).all(|pair| pair[0] > pair[1]), "{up:?}");
        // At the edge and past it, over the header or below the window:
        // the fastest, and no more.
        for y in [top, top - 30.0, -500.0] {
            assert_eq!(edge_speed(y, top, bottom), -EDGE_FASTEST, "at {y}");
        }
        for y in [bottom, bottom + 30.0, 5000.0] {
            assert_eq!(edge_speed(y, top, bottom), EDGE_FASTEST, "at {y}");
        }
        // A list shorter than two bands shares its height between them,
        // and one with no height scrolls nowhere.
        assert!(edge_speed(89.0, 88.0, 108.0) < 0.0);
        assert!(edge_speed(107.0, 88.0, 108.0) > 0.0);
        assert_eq!(edge_speed(88.0, 88.0, 88.0), 0.0);
    }

    /// A press knows what a drag from its row would carry: the row, or
    /// the whole selection when the row is one of several selected.
    #[test]
    fn a_press_carries_its_row_or_the_selection_it_is_part_of() {
        let files = crate::project::TempTree::new(
            "tree-drag-press",
            &[
                ("recipes/bread.md", ""),
                ("garden.txt", ""),
                ("journal.md", ""),
            ],
        );
        let mut tree = Tree::new();
        tree.open(&files.0);
        // recipes/, garden.txt, journal.md
        let press = |tree: &Tree, row| TreeDrag::new(tree, row, (0.0, 0.0)).unwrap();
        let path = |name: &str| files.0.join(name);
        let file = press(&tree, 1);
        assert!(!file.active());
        assert_eq!(file.path, path("garden.txt"));
        assert_eq!(file.items, [path("garden.txt")]);
        assert_eq!(file.report(tree.root()), "none");
        let folder = press(&tree, 0);
        assert_eq!(folder.items, [path("recipes")]);
        // Two selected: either carries both; the folder, not one of them,
        // carries itself.
        tree.select(1);
        tree.toggle_selected(2);
        for row in [1, 2] {
            let among = press(&tree, row);
            assert_eq!(among.items, [path("garden.txt"), path("journal.md")]);
        }
        assert_eq!(press(&tree, 0).items, [path("recipes")]);
        assert!(TreeDrag::new(&tree, 9, (0.0, 0.0)).is_none());
    }

    /// A file is opened, and a folder opened or closed, by a press let go
    /// where it went down. A press that travelled is a drag: letting go
    /// of it opens nothing, back on its own row included.
    #[test]
    fn a_press_let_go_is_a_click_and_one_that_became_a_drag_is_not() {
        let files = crate::project::TempTree::new(
            "tree-drag-click",
            &[("recipes/bread.md", ""), ("garden.txt", "")],
        );
        let mut tree = Tree::new();
        tree.open(&files.0);
        // recipes/, garden.txt
        let said = |label: &str| (label.to_string(), 60.0);
        for (row, name) in [(1, "garden.txt"), (0, "recipes")] {
            let path = files.0.join(name);
            let mut press = TreeDrag::new(&tree, row, (100.0, 120.0)).unwrap();
            assert_eq!(press.click(), Some(path.as_path()), "{name}");
            // A hand is never quite still: a few points are no drag.
            assert!(!press.begin((103.0, 117.0), said));
            assert!(!press.active());
            assert_eq!(press.click(), Some(path.as_path()), "{name}");
            // Further, it is one, said by the name of what it carries.
            assert!(press.begin((103.0, 125.0), said));
            assert!(press.active());
            assert_eq!(press.click(), None, "{name}");
            assert_eq!(press.report(tree.root()), format!("{name} onto nowhere"));
            // Brought back to where it was pressed, it is still a drag.
            assert!(press.begin((100.0, 120.0), said));
            assert_eq!(press.click(), None, "{name}");
        }
    }

    /// The mark of a copy sits on the corner of the chip, which hangs a
    /// whole row below the pointer, and follows the chip where the window's
    /// edge holds it back.
    #[test]
    fn the_chip_hangs_clear_of_the_row_under_the_pointer() {
        let window = Viewport {
            x: 0.0,
            y: 0.0,
            width: 900.0,
            height: 600.0,
        };
        let row = layout::SIDEBAR_ROW_HEIGHT;
        let corner = |pointer: (f32, f32)| {
            let at = (pointer.0 + CHIP_SHIFT.0, pointer.1 + CHIP_SHIFT.1);
            chip_corner(at, 60.0, window)
        };
        // The pointer at the very top of a row: the chip, and the mark
        // that reaches half its size up and left from the chip's corner,
        // start below that row and right of the pointer.
        let (x, y) = corner((150.0, 200.0));
        let reach = layout::COPY_MARK * 0.5 + 2.0;
        assert_eq!((x, y), (176.0, 235.0));
        assert!(y - reach >= 200.0 + row, "{y}");
        assert!(x - reach >= 150.0 + 14.0, "{x}");
        // Near the bottom and the right edge the chip stays inside, and
        // its corner with it.
        let (x, y) = corner((890.0, 595.0));
        assert_eq!((x, y), (900.0 - 76.0, 600.0 - layout::UI_CONTROL_SM));
        let (x, y) = corner((-100.0, -100.0));
        assert_eq!((x, y), (0.0, 0.0));
    }

    #[test]
    fn the_pill_names_one_item_and_counts_several() {
        let paths = |names: &[&str]| -> Vec<std::path::PathBuf> {
            names.iter().map(std::path::PathBuf::from).collect()
        };
        assert_eq!(
            carried_label(&paths(&["/p/notes/a walk in spring.md"])),
            "a walk in spring.md"
        );
        assert_eq!(carried_label(&paths(&["/p/recipes"])), "recipes");
        assert_eq!(
            carried_label(&paths(&["/p/a.txt", "/p/recipes", "/p/b.txt"])),
            "3 items"
        );
    }
}
