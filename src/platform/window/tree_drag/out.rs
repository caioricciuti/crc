//! A drag in the Explorer's tree that leaves the tree.
//!
//! Over the terminal the pane under the pointer lights up, and letting go
//! there types the paths of what is carried into its session, each quoted
//! for the shell with a space after it, as files dropped from the Finder
//! are typed: nothing moves on disk, and that pane has the keyboard. Over the
//! editor the pane under the pointer lights up and letting go opens the
//! files carried in it; a folder among them is passed over, and a drag of
//! folders alone has nothing to do there.
//!
//! When the pointer leaves the window the drag is handed to the system,
//! which carries it to the Finder and to other apps: one item to a path,
//! each a file URL written as the clipboard's are. Outside crc it may only
//! be copied, so no other app can move a file out of the project. Our own
//! drag ends there, and what the system's carries is on record for as long
//! as it lasts (see [`Out`]). Brought back over this window it is ours
//! again: on the tree it moves, or copies with Option, as a drag that
//! never left; on the terminal and the editor likewise. Escape calls it
//! off wherever it is, the system's being the system's to call off.
//!
//! A test instance never begins a system drag, which would take the
//! pointer and the drag pasteboard of whoever is running it. It writes the
//! items to a pasteboard of its own, where a scripted drag finds them, and
//! a `dump` says what it would have begun.

use std::path::PathBuf;

use objc2::rc::Allocated;
use objc2_app_kit::NSImage;

use super::*;

/// How large the icon of each item the system carries is drawn.
const ICON: f64 = 32.0;

/// How far down and right of the one before it each icon is, and after
/// how many of them the rest lie on the last.
const ICON_STEP: f64 = 4.0;
const ICON_FAN: usize = 4;

/// Where a drag of ours is when it is off the tree and letting go still
/// does something.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Beyond {
    /// Over the terminal: the session `tab` is typed the paths, and its
    /// pane is what lights up.
    Terminal { tab: usize, lit: Viewport },
    /// Over the editor: its pane `pane` opens the files, and lights up.
    Editor { pane: usize, lit: Viewport },
}

impl Beyond {
    /// What lights up.
    pub(super) fn lit(self) -> Viewport {
        match self {
            Beyond::Terminal { lit, .. } | Beyond::Editor { lit, .. } => lit,
        }
    }

    /// As a `dump` says it.
    pub(super) fn name(self) -> &'static str {
        match self {
            Beyond::Terminal { .. } => "terminal",
            Beyond::Editor { .. } => "editor",
        }
    }
}

/// What the system carries of ours, from when the drag was handed to it
/// until its drag is over.
pub(in crate::platform::window) struct Out {
    items: Vec<PathBuf>,
}

/// The files among `paths`: what the editor opens of a drag let go on it.
/// A folder is no document, and is passed over.
fn files_among<'a>(paths: impl Iterator<Item = &'a Path>) -> Vec<PathBuf> {
    paths
        .filter(|path| !path.is_dir())
        .map(Path::to_path_buf)
        .collect()
}

/// Whether any of `items` is a file, which the editor can open.
pub(super) fn any_file(items: &[PathBuf]) -> bool {
    !files_among(items.iter().map(PathBuf::as_path)).is_empty()
}

/// What a drag of ours at `at` is over, off the tree: the terminal's pane
/// there, when it shows a session, or with `files` carried the editor's.
/// Reads no file.
pub(super) fn beyond_at(
    state: &State,
    chrome: &Chrome,
    at: (f32, f32),
    files: bool,
) -> Option<Beyond> {
    let (x, y) = at;
    if let Some(rect) = chrome.terminal.filter(|rect| rect.contains(x, y)) {
        let panel = &state.terminal;
        // No session, or the selected workspace has none and another's
        // is behind its empty state: nothing is typed there.
        if panel.tabs.is_empty() || !panel.showing() || super::super::agents::active_hidden(state) {
            return None;
        }
        let (_, screen) = crate::platform::terminal::split(rect);
        let tab = panel.pane_at(screen, x, y);
        let (_, pane) = panel
            .screens(screen)
            .into_iter()
            .find(|(index, _)| *index == tab)?;
        return Some(Beyond::Terminal {
            tab,
            lit: crate::platform::terminal::lit_pane(pane),
        });
    }
    if !files || state.agents.on {
        return None;
    }
    // The pane with the keyboard, tabs through text, and then the others.
    let own = Viewport {
        x: chrome.tabs.x,
        y: chrome.tabs.y,
        width: chrome.tabs.width,
        height: chrome.text.y + chrome.text.height - chrome.tabs.y,
    };
    std::iter::once((state.focused_pane, own))
        .chain(
            chrome
                .others
                .iter()
                .map(|(index, pane)| (*index, pane.whole())),
        )
        .find(|(_, rect)| rect.contains(x, y))
        .map(|(pane, lit)| Beyond::Editor { pane, lit })
}

/// Whether the drag held, the pointer now at `at`, is the system's to
/// carry from here: a drag of ours under way whose pointer has left the
/// window, the view of which is `view`. A press that is no drag yet takes
/// nothing out, and what the system carries already is not handed to it
/// again.
fn hands_over(drag: &TreeDrag, at: (f32, f32), view: Viewport) -> bool {
    drag.live.as_ref().is_some_and(|live| !live.outside) && !view.contains(at.0, at.1)
}

/// What may be done with a drag of ours the system carries, as the system
/// asks it for where the drag is: in another app copied and nothing else,
/// so that no file leaves the project, and back in crc moved or copied.
pub(in crate::platform::window) fn source_mask(context: NSDraggingContext) -> NSDragOperation {
    if context == NSDraggingContext::WithinApplication {
        NSDragOperation::Copy | NSDragOperation::Move
    } else {
        NSDragOperation::Copy
    }
}

/// What the system is told a drag of ours brought back would do where it
/// is: move into the folder lit up, or with Option held copy there; copy
/// over the terminal and the editor, where what is carried stays where it
/// is; and nothing where letting go does nothing.
fn operation(live: &Live) -> NSDragOperation {
    match (live.beyond, live.on) {
        (Some(_), _) => NSDragOperation::Copy,
        (None, Some(_)) if live.copy => NSDragOperation::Copy,
        (None, Some(_)) => NSDragOperation::Move,
        (None, None) => NSDragOperation::None,
    }
}

/// An operation mask as a `dump` says it.
fn mask_names(mask: NSDragOperation) -> String {
    let names: Vec<&str> = [
        (NSDragOperation::Copy, "copy"),
        (NSDragOperation::Move, "move"),
        (NSDragOperation::Link, "link"),
        (NSDragOperation::Generic, "generic"),
        (NSDragOperation::Delete, "delete"),
    ]
    .into_iter()
    .filter(|(operation, _)| mask.contains(*operation))
    .map(|(_, name)| name)
    .collect();
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join("|")
    }
}

/// What a `dump` says of a drag handed to the system: `none`, or what
/// another app may do with it, what crc may, and the items by their paths
/// in the project.
pub(in crate::platform::window) fn out_report(state: &State) -> String {
    let Some(out) = &state.tree_out else {
        return "none".to_string();
    };
    let items: Vec<String> = out
        .items
        .iter()
        .map(|path| {
            state
                .tree
                .root()
                .and_then(|root| path.strip_prefix(root).ok())
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    format!(
        "outside={} inside={} items={}",
        mask_names(source_mask(NSDraggingContext::OutsideApplication)),
        mask_names(source_mask(NSDraggingContext::WithinApplication)),
        items.join("|")
    )
}

/// Where the icon of the `nth` item carried is, for the pointer at `at`:
/// under it, each a little down and right of the one before.
fn icon_frame(at: (f32, f32), nth: usize) -> NSRect {
    let fan = nth.min(ICON_FAN) as f64 * ICON_STEP;
    NSRect::new(
        NSPoint::new(
            f64::from(at.0) - ICON * 0.5 + fan,
            f64::from(at.1) - ICON * 0.5 + fan,
        ),
        NSSize::new(ICON, ICON),
    )
}

/// One item of a system drag to each of `paths`, the pointer at `at`: its
/// file URL, written as the clipboard's are, and for what is seen of it
/// the icon the Finder shows for the file.
fn dragging_items(paths: &[PathBuf], at: (f32, f32)) -> Vec<Retained<AnyObject>> {
    let workspace: Retained<AnyObject> =
        unsafe { msg_send![objc2::class!(NSWorkspace), sharedWorkspace] };
    paths
        .iter()
        .enumerate()
        .filter_map(|(nth, path)| {
            let writer = clipboard::file_url_writers(std::slice::from_ref(path)).pop()?;
            let item: Retained<AnyObject> = unsafe {
                let item: Allocated<AnyObject> = msg_send![objc2::class!(NSDraggingItem), alloc];
                msg_send![item, initWithPasteboardWriter: &*writer]
            };
            let icon: Retained<NSImage> = unsafe {
                msg_send![&*workspace, iconForFile: &*NSString::from_str(&path.to_string_lossy())]
            };
            let _: () = unsafe {
                msg_send![&*item, setDraggingFrame: icon_frame(at, nth), contents: &*icon]
            };
            Some(item)
        })
        .collect()
}

impl TreeDrag {
    /// What was taken out of the window, brought back over it at `at` by
    /// the system: a drag from the first, as files from another app are,
    /// of items that are ours. Each of `paths` is looked up, once.
    fn returned(paths: Vec<PathBuf>, at: (f32, f32)) -> TreeDrag {
        let label = carried_label(&paths);
        TreeDrag {
            path: PathBuf::new(),
            items: Vec::new(),
            origin: at,
            live: Some(Live {
                files: any_file(&paths),
                check: DropCheck::new(paths),
                // Said in a `dump` only: the system draws what is carried.
                label,
                label_width: 0.0,
                at,
                copy: false,
                outside: true,
                returned: true,
                inside: false,
                beyond: None,
                on: None,
                hover: None,
                carry: 0.0,
            }),
        }
    }

    /// Ours, brought back into the window by the system.
    fn is_returned(&self) -> bool {
        self.live.as_ref().is_some_and(|live| live.returned)
    }
}

impl EditorView {
    /// The pointer is at `(x, y)` with a drag of the tree held, by the
    /// `event` that moved it. Out of the window, the drag is handed to
    /// the system and ours is over: what it carries goes on record, and
    /// nothing in the window is lit for it any more.
    pub(in crate::platform::window) fn tree_drag_leaves(&self, event: &NSEvent, x: f32, y: f32) {
        let items = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let view = state.viewport;
            let Some(Drag::Tree(drag)) = &state.drag else {
                return;
            };
            if !hands_over(drag, (x, y), view) {
                return;
            }
            let items: Vec<PathBuf> = drag
                .live
                .as_ref()
                .map(|live| live.check.paths().map(Path::to_path_buf).collect())
                .unwrap_or_default();
            state.drag = None;
            state.tree_out = Some(Out {
                items: items.clone(),
            });
            items
        };
        if self.ivars().testing {
            // No system drag: the items wait where a scripted one reads.
            clipboard::write_file_urls_on(&clipboard::scripted_drag(), &items);
        } else if !self.begin_dragging(event, &items, (x, y)) {
            self.tree_drag_out_ended();
        }
        self.request_redraw();
        self.pump();
    }

    /// Begins the system's drag of `items` from this view, with the
    /// `event` that took the pointer out of the window at `at`. Whether
    /// one began.
    fn begin_dragging(&self, event: &NSEvent, items: &[PathBuf], at: (f32, f32)) -> bool {
        let carried = dragging_items(items, at);
        if carried.is_empty() {
            return false;
        }
        let source: &AnyObject = self;
        let session: Option<Retained<AnyObject>> = unsafe {
            msg_send![
                self,
                beginDraggingSessionWithItems: &*NSArray::from_retained_slice(&carried),
                event: event,
                source: source
            ]
        };
        let Some(session) = session else {
            return false;
        };
        // Called off, the icons do not fly back to the edge of the window
        // they left by: nothing of them was ever there.
        let _: () =
            unsafe { msg_send![&*session, setAnimatesToStartingPositionsOnCancelOrFail: false] };
        true
    }

    /// The system's drag of what was taken out of the window is over, let
    /// go somewhere or called off: nothing of it is held or lit any more.
    /// A test instance, which has no system drag, says so itself where
    /// the system would.
    pub(in crate::platform::window) fn tree_drag_out_ended(&self) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if state.tree_out.take().is_none() {
                return;
            }
            if matches!(&state.drag, Some(Drag::Tree(drag)) if drag.is_returned()) {
                state.drag = None;
            }
        }
        if self.ivars().testing {
            clipboard::scripted_drag().clearContents();
        }
        self.request_redraw();
        self.pump();
    }

    /// A drag the system carries is at `(x, y)` in the window. When it is
    /// the one handed over from the tree, brought back, it is held again
    /// as ours: the folder, the terminal's pane or the editor's under it
    /// lights up, and the answer is what letting go there would do.
    /// `None` when the drag is another's.
    ///
    /// The pasteboard is read once, when the drag comes back in, to tell
    /// ours from one that follows it.
    pub(in crate::platform::window) fn returned_drag_over(
        &self,
        pasteboard: &NSPasteboard,
        x: f32,
        y: f32,
    ) -> Option<NSDragOperation> {
        let testing = self.ivars().testing;
        let (answer, changed) = {
            let mut state = self.state_mut()?;
            let state = &mut *state;
            let out = state.tree_out.as_ref()?;
            let held = matches!(&state.drag, Some(Drag::Tree(drag)) if drag.is_returned());
            if !held {
                if state.drag.is_some() || clipboard::file_urls(pasteboard) != out.items {
                    return None;
                }
                state.drag = Some(Drag::Tree(TreeDrag::returned(out.items.clone(), (x, y))));
            }
            if let Some(Drag::Tree(TreeDrag {
                live: Some(live), ..
            })) = &mut state.drag
            {
                live.at = (x, y);
                // Option as it is held now: no event of ours says so
                // while the system has the pointer. A test instance is
                // told, and never reads the keyboard of whoever runs it.
                if !testing {
                    live.copy =
                        NSEvent::modifierFlags_class().contains(NSEventModifierFlags::Option);
                }
            }
            let changed = aim(state, Instant::now(), hover_expand(testing));
            let answer = match &state.drag {
                Some(Drag::Tree(TreeDrag {
                    live: Some(live), ..
                })) => operation(live),
                _ => NSDragOperation::None,
            };
            (answer, changed || !held)
        };
        if changed {
            self.request_redraw();
        }
        // Held still it sends nothing: the frame opens the folder it
        // rests on and scrolls the tree under it.
        self.resume_display_link();
        self.pump();
        Some(answer)
    }

    /// The drag the system carries was let go at `(x, y)` in the window.
    /// When it is ours brought back, it is dropped as a drag that never
    /// left: whether that did anything, or `None` when the drag is
    /// another's.
    pub(in crate::platform::window) fn returned_drag_dropped(
        &self,
        pasteboard: &NSPasteboard,
        x: f32,
        y: f32,
    ) -> Option<bool> {
        // Aimed once more, where it was let go.
        self.returned_drag_over(pasteboard, x, y)?;
        let live = {
            let mut state = self.state_mut()?;
            match state.drag.take() {
                Some(Drag::Tree(TreeDrag {
                    live: Some(live), ..
                })) if live.returned => live,
                other => {
                    state.drag = other;
                    return None;
                }
            }
        };
        let done = self.drop_live(live);
        self.request_redraw();
        self.pump();
        Some(done)
    }

    /// A drag of ours let go off the tree: over the terminal its paths
    /// are typed into the session under the pointer, which has the
    /// keyboard from then, and over the editor its files open in the
    /// pane under the pointer. Whether anything was done.
    pub(super) fn drop_beyond(&self, live: &Live) -> bool {
        // Looked at again where it is let go: a session may have ended
        // since the last frame.
        let beyond = self
            .state()
            .and_then(|state| beyond_at(&state, &chrome_of(&state), live.at, live.files));
        match beyond {
            Some(Beyond::Terminal { tab, .. }) => {
                let paths: Vec<PathBuf> = live.check.paths().map(Path::to_path_buf).collect();
                self.type_paths_in(tab, &paths);
                true
            }
            Some(Beyond::Editor { pane, .. }) => {
                let files = files_among(live.check.paths());
                if files.is_empty() {
                    return false;
                }
                self.focus_pane(pane);
                for file in &files {
                    self.open_dropped_file(file);
                }
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Viewport = Viewport {
        x: 0.0,
        y: 0.0,
        width: 1200.0,
        height: 800.0,
    };

    /// A drag of ours is handed to the system when its pointer leaves the
    /// window, by any edge, and not a point before. A press that has not
    /// travelled is no drag and takes nothing out, and what the system
    /// carries already, from another app or ours brought back, is never
    /// handed to it a second time.
    #[test]
    fn a_drag_is_handed_over_once_its_pointer_has_left_the_window() {
        let files = crate::project::TempTree::new(
            "tree-drag-hand-over",
            &[("recipes/bread.md", ""), ("garden.txt", "")],
        );
        let mut tree = Tree::new();
        tree.open(&files.0);
        // recipes/, garden.txt
        let said = |label: &str| (label.to_string(), 60.0);
        let mut press = TreeDrag::new(&tree, 1, (100.0, 120.0)).unwrap();
        // Pressed and not moved: the pointer cannot be outside, and were
        // it, there is no drag to hand over.
        assert!(!hands_over(&press, (100.0, 120.0), WINDOW));
        assert!(!hands_over(&press, (-40.0, 120.0), WINDOW));
        assert!(press.begin((100.0, 160.0), said));
        // Anywhere in the window it stays ours: over the tree, the
        // editor, the terminal, the status line, and at the last point
        // inside each edge.
        for inside in [
            (100.0, 160.0),
            (700.0, 300.0),
            (700.0, 700.0),
            (0.0, 0.0),
            (1199.5, 799.5),
            (0.0, 799.5),
            (1199.5, 0.0),
        ] {
            assert!(!hands_over(&press, inside, WINDOW), "{inside:?}");
        }
        // Past any edge it is the system's.
        for outside in [
            (-0.5, 300.0),
            (-400.0, 300.0),
            (1200.0, 300.0),
            (600.0, -1.0),
            (600.0, 800.0),
            (-30.0, -30.0),
            (5000.0, 5000.0),
        ] {
            assert!(hands_over(&press, outside, WINDOW), "{outside:?}");
        }
        // A window that is not at the view's origin, or has no size.
        let shifted = Viewport {
            x: 10.0,
            y: 20.0,
            width: 100.0,
            height: 100.0,
        };
        assert!(hands_over(&press, (5.0, 60.0), shifted));
        assert!(!hands_over(&press, (10.0, 20.0), shifted));
        assert!(hands_over(&press, (110.0, 60.0), shifted));

        let desk = vec![files.0.join("garden.txt")];
        let foreign = TreeDrag::from_outside(desk.clone(), (40.0, 200.0));
        assert!(!hands_over(&foreign, (-40.0, 200.0), WINDOW));
        let back = TreeDrag::returned(desk, (40.0, 200.0));
        assert!(back.is_returned());
        assert!(back.outside());
        assert!(!foreign.is_returned());
        assert!(!press.is_returned());
        assert!(!hands_over(&back, (-40.0, 200.0), WINDOW));
    }

    /// Another app may copy what is dragged out and nothing else: no
    /// move, no link, no delete, so that no file leaves the project. Back
    /// in crc it moves or copies, as in the tree.
    #[test]
    fn another_app_may_only_copy_and_crc_may_move_as_well() {
        let outside = source_mask(NSDraggingContext::OutsideApplication);
        assert_eq!(outside, NSDragOperation::Copy);
        for never in [
            NSDragOperation::Move,
            NSDragOperation::Link,
            NSDragOperation::Generic,
            NSDragOperation::Delete,
        ] {
            assert!(!outside.contains(never), "{never:?}");
        }
        let inside = source_mask(NSDraggingContext::WithinApplication);
        assert_eq!(inside, NSDragOperation::Copy | NSDragOperation::Move);
        assert!(!inside.contains(NSDragOperation::Delete));
        assert_eq!(mask_names(outside), "copy");
        assert_eq!(mask_names(inside), "copy|move");
        assert_eq!(mask_names(NSDragOperation::None), "none");
    }

    /// The editor opens the files of a drag let go on it and passes over
    /// the folders, a link to a folder among them; a drag of folders
    /// alone has nothing for it.
    #[test]
    fn folders_are_passed_over_when_a_drag_is_let_go_on_the_editor() {
        let files = crate::project::TempTree::new(
            "tree-drag-editor",
            &[
                ("recipes/old soups/leek.md", "leeks"),
                ("a walk in spring.md", "primroses"),
                ("garden.txt", "tomatoes"),
                ("trips/", ""),
            ],
        );
        let at = |name: &str| files.0.join(name);
        std::os::unix::fs::symlink(at("trips"), at("holidays")).unwrap();
        let carried = [
            at("recipes"),
            at("a walk in spring.md"),
            at("trips"),
            at("garden.txt"),
            at("holidays"),
            at("recipes/old soups/leek.md"),
        ];
        assert_eq!(
            files_among(carried.iter().map(PathBuf::as_path)),
            [
                at("a walk in spring.md"),
                at("garden.txt"),
                at("recipes/old soups/leek.md"),
            ]
        );
        assert!(any_file(&carried));
        assert!(!any_file(&[at("recipes"), at("trips"), at("holidays")]));
        assert!(!any_file(&[]));
        assert!(any_file(&[at("garden.txt")]));
    }

    /// What a drag of ours brought back tells the system it would do: a
    /// move into the folder lit up, a copy with Option held, a copy over
    /// the terminal or the editor, and nothing anywhere else.
    #[test]
    fn a_drag_brought_back_moves_in_the_tree_and_copies_with_option() {
        let files = crate::project::TempTree::new(
            "tree-drag-returned",
            &[("recipes/bread.md", ""), ("garden.txt", "")],
        );
        let back = TreeDrag::returned(vec![files.0.join("garden.txt")], (40.0, 200.0));
        assert_eq!(
            back.report(Some(&files.0)),
            "returned: garden.txt onto nowhere"
        );
        assert_eq!(back.click(), None);
        assert_eq!(back.lit(), None);
        let Some(mut live) = back.live else {
            panic!("a drag brought back is live");
        };
        assert!(live.files);
        assert_eq!(operation(&live), NSDragOperation::None);
        live.on = Some(DropOn::Folder(0));
        assert_eq!(operation(&live), NSDragOperation::Move);
        live.copy = true;
        assert_eq!(operation(&live), NSDragOperation::Copy);
        live.copy = false;
        live.on = Some(DropOn::Root);
        assert_eq!(operation(&live), NSDragOperation::Move);
        // Off the tree nothing is moved, whatever Option says.
        let lit = Viewport {
            x: 300.0,
            y: 500.0,
            width: 600.0,
            height: 200.0,
        };
        live.on = None;
        for beyond in [
            Beyond::Terminal { tab: 0, lit },
            Beyond::Editor { pane: 0, lit },
        ] {
            live.beyond = Some(beyond);
            assert_eq!(operation(&live), NSDragOperation::Copy);
            assert_eq!(beyond.lit(), lit);
        }
        let drag = TreeDrag {
            path: PathBuf::new(),
            items: Vec::new(),
            origin: (0.0, 0.0),
            live: Some(live),
        };
        assert_eq!(drag.lit(), Some(lit));
        assert_eq!(
            drag.report(Some(&files.0)),
            "returned: garden.txt onto editor"
        );
    }

    /// The system is given one item to a path, each with the file's URL
    /// byte for byte, and the icons lie under the pointer, each a little
    /// on from the one before until the last few share a place.
    #[test]
    fn the_system_is_given_one_item_to_each_path_carried() {
        let files = crate::project::TempTree::new(
            "tree-drag-items",
            &[
                ("a walk in spring.md", "primroses"),
                ("caf\u{e9}.md", "open late"),
                ("trips/", ""),
            ],
        );
        let paths: Vec<PathBuf> = ["a walk in spring.md", "caf\u{e9}.md", "trips"]
            .iter()
            .map(|name| files.0.join(name))
            .collect();
        let written = |item: &AnyObject| -> String {
            let url: Retained<NSURL> = unsafe { msg_send![item, item] };
            url.absoluteString()
                .map(|s| s.to_string())
                .unwrap_or_default()
        };
        let frame = |item: &AnyObject| -> NSRect { unsafe { msg_send![item, draggingFrame] } };
        let one = dragging_items(&paths[..1], (-20.0, 300.0));
        assert_eq!(one.len(), 1);
        assert!(written(&one[0]).ends_with("/a%20walk%20in%20spring.md"));
        assert_eq!(frame(&one[0]), icon_frame((-20.0, 300.0), 0));
        assert_eq!(
            icon_frame((-20.0, 300.0), 0),
            NSRect::new(NSPoint::new(-36.0, 284.0), NSSize::new(32.0, 32.0))
        );
        let several = dragging_items(&paths, (-20.0, 300.0));
        assert_eq!(several.len(), 3);
        let urls: Vec<String> = several.iter().map(|item| written(item)).collect();
        assert!(urls[0].ends_with("/a%20walk%20in%20spring.md"), "{urls:?}");
        assert!(urls[1].ends_with("/caf%C3%A9.md"), "{urls:?}");
        assert!(urls[2].ends_with("/trips/"), "{urls:?}");
        assert!(
            urls.iter().all(|url| url.starts_with("file:///")),
            "{urls:?}"
        );
        assert_eq!(frame(&several[2]), icon_frame((-20.0, 300.0), 2));
        assert_eq!(icon_frame((0.0, 0.0), 2).origin, NSPoint::new(-8.0, -8.0));
        assert_eq!(icon_frame((0.0, 0.0), 4), icon_frame((0.0, 0.0), 40));
        assert!(dragging_items(&[], (0.0, 0.0)).is_empty());
    }
}
