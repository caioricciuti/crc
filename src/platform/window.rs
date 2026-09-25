//! The window, the event loop, and the bridge from AppKit into the editor.
//!
//! Frames are drawn on demand, not on a timer. A keystroke lays out and
//! presents immediately in the same call stack as the event, which is both
//! the lowest-latency way to do it and the easiest to measure honestly.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ProtocolObject, Sel};
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSAlert, NSAlertStyle, NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate,
    NSApplicationTerminateReply, NSBackingStoreType, NSCursor, NSEvent, NSEventModifierFlags,
    NSEventType, NSMenu, NSMenuItem, NSOpenPanel, NSSavePanel, NSScreen, NSTextInputClient,
    NSTrackingArea, NSTrackingAreaOptions, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
    NSWindowTitleVisibility,
};
use objc2_foundation::{
    NSArray, NSAttributedString, NSAttributedStringKey, NSFileManager, NSNotFound, NSNotification,
    NSObject, NSObjectProtocol, NSPoint, NSRange, NSRangePointer, NSRect, NSRunLoop,
    NSRunLoopCommonModes, NSSize, NSString, NSUInteger, NSURL,
};
use objc2_metal::MTLCreateSystemDefaultDevice;
use objc2_quartz_core::{CADisplayLink, CALayer, CAMetalLayer};

use crate::markdown;
use crate::platform::clipboard;
use crate::platform::commands::{self, Command};
use crate::platform::latency::Latency;
use crate::platform::recovery;
use crate::platform::search::{self, Options as SearchOptions};
use crate::platform::selftest::{self, Step};
use crate::platform::session::Session;
use crate::platform::symbols;
use crate::project::finder::Finder;
use crate::project::tree::{Entry as TreeEntry, Tree, move_without_replace};
use crate::render::font::Atlas;
use crate::render::layout::{self, Chrome, Frame, Hit, Theme, Viewport};
use crate::render::metal::{DRAWABLE_FORMAT, FrameTiming, GlyphInstance, Renderer};
use crate::syntax::{Language, Span, SyntaxStore};
use crate::text::buffer::{Buffer, DiskState, Motion};
use crate::text::documents::Documents;

struct ProjectIndexResult {
    root: std::path::PathBuf,
    tree: Tree,
    finder: Finder,
    tree_version: u64,
}

fn spawn_project_index(root: std::path::PathBuf) -> mpsc::Receiver<ProjectIndexResult> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut tree = Tree::new();
        tree.open(&root);
        let mut finder = Finder::new();
        finder.scan(&root);
        let _ = tx.send(ProjectIndexResult {
            root,
            tree,
            finder,
            tree_version: 0,
        });
    });
    rx
}

fn spawn_project_refresh(mut tree: Tree, tree_version: u64) -> mpsc::Receiver<ProjectIndexResult> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let Some(root) = tree.root().map(Path::to_path_buf) else {
            return;
        };
        tree.refresh();
        let mut finder = Finder::new();
        finder.scan(&root);
        let _ = tx.send(ProjectIndexResult {
            root,
            tree,
            finder,
            tree_version,
        });
    });
    rx
}

/// macOS virtual key codes. These are physical positions and do not shift
/// with the keyboard layout, which is what navigation keys want.
mod key {
    pub const RETURN: u16 = 36;
    /// Enter on the numeric keypad. Its `characters` is U+0003, a control
    /// character, so treated as text it was filtered out and did nothing.
    pub const KEYPAD_ENTER: u16 = 76;
    pub const TAB: u16 = 48;
    pub const DELETE: u16 = 51;
    pub const FORWARD_DELETE: u16 = 117;
    pub const HOME: u16 = 115;
    pub const END: u16 = 119;
    pub const PAGE_UP: u16 = 116;
    pub const PAGE_DOWN: u16 = 121;
    pub const LEFT: u16 = 123;
    pub const RIGHT: u16 = 124;
    pub const DOWN: u16 = 125;
    pub const UP: u16 = 126;
    pub const SPACE: u16 = 49;
    pub const F1: u16 = 122;
    pub const F2: u16 = 120;
    pub const F12: u16 = 111;
    pub const F: u16 = 3;
}

/// What a key event's `characters` contributes as typed text.
///
/// AppKit reports keys that have no character, the function keys, Help,
/// Insert, keypad Clear and the rest, as code points in the private-use block
/// U+F700..U+F8FF. They are not control characters, so a filter that only
/// dropped those let fn-F5 insert an invisible U+F708 into the file.
fn typed_text(characters: &str) -> String {
    characters
        .chars()
        .filter(|c| !c.is_control() && !('\u{f700}'..='\u{f8ff}').contains(c))
        .collect()
}

/// A pasted string cut down to what a one-row field can hold: its first
/// line, without control characters.
fn single_line(text: &str) -> String {
    typed_text(text.lines().next().unwrap_or(""))
}

/// Makes a held key repeat, rather than open the accent menu.
///
/// Being a real text input client is what makes dead keys and input methods
/// work, and it also opts the view into press-and-hold: hold `e`, get a menu
/// of accents instead of `eeee`. In a code editor the repeat is worth more,
/// and the accents are still a dead key away. Registered, not written: it is
/// only this app's default, and `defaults write dev.ricciuti.crc
/// ApplePressAndHoldEnabled -bool true` turns the menu back on.
fn prefer_key_repeat() {
    let key = NSString::from_str("ApplePressAndHoldEnabled");
    let off = objc2_foundation::NSNumber::new_bool(false);
    let defaults = objc2_foundation::NSDictionary::from_slices(&[&*key], &[&*off]);
    // SAFETY: a dictionary of string keys to property-list values, which is
    // what registerDefaults takes.
    unsafe {
        objc2_foundation::NSUserDefaults::standardUserDefaults()
            .registerDefaults(defaults.cast_unchecked());
    }
}

/// The action of the menu item whose key equivalent is `chars` with `flags`,
/// searching submenus. What AppKit's own lookup does, minus the dependence on
/// which window is key.
fn menu_action_for(menu: &NSMenu, chars: &str, flags: NSEventModifierFlags) -> Option<Sel> {
    let relevant = NSEventModifierFlags::Command
        | NSEventModifierFlags::Shift
        | NSEventModifierFlags::Option
        | NSEventModifierFlags::Control;
    for item in menu.itemArray().iter() {
        if let Some(submenu) = item.submenu()
            && let Some(action) = menu_action_for(&submenu, chars, flags)
        {
            return Some(action);
        }
        let key = item.keyEquivalent().to_string();
        if key.is_empty() || !key.eq_ignore_ascii_case(chars) {
            continue;
        }
        // An upper-case key equivalent is AppKit's way of writing Shift.
        let mut wanted = item.keyEquivalentModifierMask() & relevant;
        if key.chars().any(char::is_uppercase) {
            wanted |= NSEventModifierFlags::Shift;
        }
        if wanted == flags & relevant {
            return item.action();
        }
    }
    None
}

/// The text in what the input system hands over, which is an `NSString` or
/// an `NSAttributedString` depending on who is asking.
fn text_of(string: &AnyObject) -> String {
    if let Some(attributed) = string.downcast_ref::<NSAttributedString>() {
        return attributed.string().to_string();
    }
    string
        .downcast_ref::<NSString>()
        .map(NSString::to_string)
        .unwrap_or_default()
}

/// What a drag selects in, set by how many clicks started it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectUnit {
    Character,
    Word,
    Line,
}

/// Completion at the caret: what the server and the worker offered, merged
/// and ranked, and which suggestion is picked.
struct CompletionPopup {
    /// The buffer it belongs to; a switch closes it.
    buffer: u64,
    /// Where the word (or path segment) being completed starts. The text
    /// from here to the caret is the prefix.
    anchor: usize,
    /// The server request whose reply fills `items`; older replies are
    /// ignored. Zero when there is no server.
    request: u64,
    /// The worker question whose answer fills `local`.
    generation: u64,
    items: Vec<crate::lsp::Completion>,
    local: Vec<crate::complete::Candidate>,
    boosts: Vec<crate::complete::Boost>,
    /// What is offered, best first.
    shown: Vec<crate::complete::Candidate>,
    selected: usize,
    /// Picked with the arrows: Return accepts only then. Until you choose,
    /// Return is a new line and Tab takes the suggestion.
    chosen: bool,
    /// Completing a path segment rather than a word.
    path: bool,
    /// What the history is keyed on.
    context: String,
    language: String,
}

impl CompletionPopup {
    fn refilter(&mut self, prefix: &str) {
        let mut all: Vec<crate::complete::Candidate> = self
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| crate::complete::Candidate {
                label: item.label.clone(),
                insert: crate::lsp::client::completion_text(item).to_owned(),
                source: crate::complete::Source::Server,
                why: item.detail.clone().unwrap_or_default(),
                // The server's own order, gently.
                weight: 1.0 - (index as f32 * 0.002).min(0.5),
                server: Some(index),
            })
            .collect();
        all.extend(self.local.iter().cloned());
        let previous = self.shown.get(self.selected).map(|c| c.label.clone());
        self.shown = crate::complete::rank(prefix, all, &self.boosts, 40);
        // Keep the pick when it is still there; otherwise the best.
        self.selected = previous
            .filter(|_| self.chosen)
            .and_then(|label| self.shown.iter().position(|c| c.label == label))
            .unwrap_or(0);
    }
}

/// The view, for a callback on another thread to wake the main thread
/// with. The view lives for the process; see the display link.
struct ViewPointer(*const EditorView);
unsafe impl Send for ViewPointer {}
unsafe impl Sync for ViewPointer {}

unsafe extern "C" fn lsp_wake_on_main(context: *mut std::ffi::c_void) {
    let view = unsafe { &*(context as *const EditorView) };
    view.poll_lsp();
}

unsafe extern "C" fn terminal_wake_on_main(context: *mut std::ffi::c_void) {
    let view = unsafe { &*(context as *const EditorView) };
    view.poll_terminal();
}

unsafe extern "C" fn completion_wake_on_main(context: *mut std::ffi::c_void) {
    let view = unsafe { &*(context as *const EditorView) };
    view.poll_completion();
}

unsafe extern "C" fn claude_wake_on_main(context: *mut std::ffi::c_void) {
    let view = unsafe { &*(context as *const EditorView) };
    view.poll_claude();
}

/// The per-pane state of a pane that does not have the keyboard.
struct PaneStore {
    docs: Documents,
    preview: Option<usize>,
    tab_scroll: usize,
    tab_hits: Vec<layout::TabHit>,
    live_line: Option<(u64, usize)>,
}

fn pane_count(state: &State) -> usize {
    state.panes.len() + 1
}

/// Every pane in order, the focused one taken out of the state's own
/// fields. The inverse is [`restore_panes`].
fn take_panes(state: &mut State) -> Vec<PaneStore> {
    let focused = PaneStore {
        docs: std::mem::replace(&mut state.docs, Documents::new(Buffer::new())),
        preview: state.preview.take(),
        tab_scroll: std::mem::take(&mut state.tab_scroll),
        tab_hits: std::mem::take(&mut state.tab_hits),
        live_line: state.live_line.take(),
    };
    let mut all = std::mem::take(&mut state.panes);
    let at = state.focused_pane.min(all.len());
    all.insert(at, focused);
    all
}

/// Puts `all` back with pane `focus` in the state's own fields.
fn restore_panes(state: &mut State, mut all: Vec<PaneStore>, focus: usize) {
    let focus = focus.min(all.len().saturating_sub(1));
    let focused = all.remove(focus);
    state.docs = focused.docs;
    state.preview = focused.preview;
    state.tab_scroll = focused.tab_scroll;
    state.tab_hits = focused.tab_hits;
    state.live_line = focused.live_line;
    state.panes = all;
    state.focused_pane = focus;
}

/// Every pane's documents, focused first.
fn all_docs(state: &State) -> impl Iterator<Item = &Documents> {
    std::iter::once(&state.docs).chain(state.panes.iter().map(|p| &p.docs))
}

fn all_docs_mut(state: &mut State) -> Vec<&mut Documents> {
    let State { docs, panes, .. } = state;
    std::iter::once(docs)
        .chain(panes.iter_mut().map(|p| &mut p.docs))
        .collect()
}

/// What the inline sidebar field will do with its name when committed.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SidebarEditKind {
    NewFile,
    NewFolder,
    Rename(std::path::PathBuf),
}

struct SidebarEdit {
    kind: SidebarEditKind,
    /// The folder the name is relative to.
    parent: std::path::PathBuf,
    /// The visible row the field occupies, and its indent.
    row: usize,
    depth: usize,
    field: Buffer,
}

impl SidebarEdit {
    fn replaces(&self) -> bool {
        matches!(self.kind, SidebarEditKind::Rename(_))
    }
}

/// Which text the Edit menu is acting on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Document,
    /// The find bar's query, where a change moves the match.
    FindQuery,
    /// The line-number field, which holds digits only.
    Goto,
    /// The replacement field or the palette query.
    Field,
}

/// Whether to log scroll events. Checked once: an env lookup per scroll
/// event would be a syscall on the input path.
fn debug_scroll() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("CRC_DEBUG_SCROLL").is_some())
}

/// Lines scrolled per notch of a wheel that reports notches, not points.
const WHEEL_LINES_PER_NOTCH: f64 = 3.0;

/// Each half of a caret blink, the macOS default.
const CARET_BLINK: Duration = Duration::from_millis(530);

/// Whether the caret is showing `since` the last input, and how long until
/// that changes. It starts in the on half, so typing keeps it solid.
fn caret_phase(since: Duration) -> (bool, Duration) {
    let period = CARET_BLINK.as_millis();
    let at = since.as_millis();
    let visible = (at / period).is_multiple_of(2);
    (
        visible,
        Duration::from_millis((period - at % period) as u64),
    )
}

/// Bounds on the sidebar, so a drag cannot make it useless or swallow the
/// editor.
const SIDEBAR_MIN: f32 = 140.0;
const SIDEBAR_MAX: f32 = 600.0;
/// How close to the divider counts as grabbing it.
const DIVIDER_GRAB: f32 = 5.0;

struct State {
    git: crate::platform::git_panel::Panel,
    git_open: bool,
    /// Whether the commit message field has the keyboard. Source control is
    /// docked, not modal, so it only takes typing when it is asked to.
    git_focus: bool,
    /// The tab the pointer is over, so its close button can appear. A row of
    /// crosses on every tab is a wall of targets; one under the pointer is
    /// the affordance without the clutter.
    hovered_tab: Option<usize>,
    /// A sidebar row being dragged onto a folder.
    tree_drag: Option<TreeDrag>,
    /// FSEvents on the project root. Dropped and remade when the root moves.
    watcher: Option<crate::project::watch::Watcher>,
    /// Language servers by their shared language key, for the current root.
    lsp: HashMap<Language, crate::lsp::client::Server>,
    /// Languages whose server could not be started, and why. Said once.
    lsp_unavailable: HashMap<Language, String>,
    /// Buffers edited since their server last heard, and when. Changes are
    /// sent once typing pauses.
    lsp_dirty: HashMap<u64, Instant>,
    /// F2's field: the new name, and where the rename was asked.
    rename: Option<RenameField>,
    /// The signature of the call the caret is in, while the server has one.
    signature: Option<SignatureTip>,
    /// A format in flight: the file and its text when asked, so a reply
    /// that no longer fits the text is dropped rather than applied.
    formatting: Option<(std::path::PathBuf, crate::text::rope::Rope)>,
    /// `format_on_save` from the settings.
    format_on_save: bool,
    /// Set while saving what a format-on-save produced, so that save does
    /// not ask for another format.
    saving_formatted: bool,
    /// `word_wrap` from the settings.
    word_wrap: crate::platform::settings::WordWrap,
    /// `ssh_auth_sock` from the settings, for fetch, pull and push.
    ssh_auth_sock: Option<std::path::PathBuf>,
    /// The palette is picking a branch: the local branches, read when it
    /// opened. `None` in every other mode.
    branch_list: Option<Vec<crate::project::git::Branch>>,
    /// Who last changed the caret's line: document, line, and what the
    /// status line says.
    blame: Option<(u64, usize, String)>,
    /// The caret's document and line, and since when, so blame is asked
    /// once the caret rests.
    blame_want: Option<(u64, usize, Instant)>,
    blame_rx: Option<mpsc::Receiver<(u64, usize, String)>>,
    /// Git marks per open buffer, keyed by buffer id.
    gutter: HashMap<u64, GutterState>,
    /// The code font as asked for, and its size in points. The atlas holds
    /// the resolved face; this is what a rebuild at a new size starts from.
    font: String,
    font_size: f32,
    /// From the settings file: system, dark or light.
    theme_choice: crate::platform::settings::ThemeChoice,
    /// From the settings file: whether the caret blinks at all.
    caret_blink: bool,
    /// The last frame drew a line too long to shape; the status says so.
    unshaped_on_screen: bool,
    /// The completion worker, started with the first question.
    completer: Option<crate::complete::worker::Worker>,
    completion_generation: u64,
    /// Where the suggestion chips were drawn, for clicks.
    completion_chips: Vec<(layout::Viewport, usize)>,
    /// Keeps the project index current; one per open project.
    indexer: Option<crate::index::store::Indexer>,
    /// Git's ignored paths being read for this root.
    ignored_rx: Option<
        mpsc::Receiver<(
            std::path::PathBuf,
            std::collections::HashSet<std::path::PathBuf>,
        )>,
    >,
    /// An update check in flight, and whether someone asked for it (which
    /// changes what an answer says and does).
    update: Option<(bool, mpsc::Receiver<crate::platform::update::Outcome>)>,
    /// The last key or click. The caret is solid for a blink phase after
    /// it, so it never vanishes under your typing.
    caret_since: Instant,
    /// Buffers whose marks are stale, and since when. Recomputed after a
    /// pause, like the language server sync.
    gutter_dirty: HashMap<u64, Instant>,
    /// Buffers whose HEAD text is being fetched on a worker.
    gutter_pending: HashSet<u64>,
    gutter_channel: (mpsc::Sender<HeadText>, mpsc::Receiver<HeadText>),
    /// The completion list, while one is open.
    completion: Option<CompletionPopup>,
    /// When the watcher last reported a tree change that has not been
    /// acted on. Refreshes are debounced against it, and a change that
    /// arrives while a refresh is running is kept for the next one.
    project_changed_at: Option<Instant>,
    /// The same for `.git` bookkeeping.
    git_changed_at: Option<Instant>,
    /// A name being typed into the tree, for a file or folder about to be
    /// created or an item being renamed. The way VS Code does it: no panel.
    sidebar_edit: Option<SidebarEdit>,
    /// Every open document of the focused pane. There is always at least
    /// one. The other panes keep theirs in `panes`, and focusing a pane
    /// swaps its documents in here, so everything that edits, saves, finds
    /// or draws "the document" keeps working on whichever pane has the keys.
    docs: Documents,
    /// The panes without the keyboard, in pane order with the focused one
    /// left out. See [`pane_stores`] for the full ordering.
    panes: Vec<PaneStore>,
    /// Which pane, in the full order, is the one in the fields above.
    focused_pane: usize,
    native_preview: Option<NativePreview>,
    /// The project tree behind the sidebar.
    tree: Tree,
    tree_version: u64,
    tree_children_tx: mpsc::Sender<(std::path::PathBuf, std::path::PathBuf, Vec<TreeEntry>)>,
    tree_children_rx: mpsc::Receiver<(std::path::PathBuf, std::path::PathBuf, Vec<TreeEntry>)>,
    tree_children_pending: HashSet<std::path::PathBuf>,
    /// The find bar. Both fields are `Buffer`s, which means editing them
    /// gets the cursor, selection and boundary behaviour that already exists
    /// rather than two more, worse, text inputs.
    find: Option<FindBar>,
    /// Whether the sidebar is showing.
    sidebar: bool,
    /// Sidebar width in logical points, draggable.
    sidebar_width: f32,
    /// Set while the divider is being dragged.
    dragging_divider: bool,
    /// Set while the terminal panel's top edge is being dragged.
    dragging_terminal: bool,
    /// Set while the editor's scrollbar thumb is held: where on the thumb
    /// the press landed, so the thumb does not jump under the pointer.
    scrollbar_drag: Option<f32>,
    /// Set while a press that began in the text is held, which is the only
    /// kind of drag that selects: the unit it selects in, and what the press
    /// itself selected, which a drag never shrinks below.
    selecting: Option<(SelectUnit, std::ops::Range<usize>)>,
    /// Where the pointer last was during that drag. Kept because a pointer
    /// held still past the edge sends no events, and the view still has to
    /// keep scrolling under it.
    drag_point: Option<(f32, f32)>,
    /// Lines of autoscroll owed but not yet amounting to a whole one.
    autoscroll_carry: f32,
    /// Text an input method is still composing: the accent waiting for its
    /// letter, syllables waiting to become a word. Shown at the caret, and
    /// not part of the document until it is committed.
    marked: Option<String>,
    /// Input events still to be played, when `CRC_SELFTEST` named a script.
    selftest: std::collections::VecDeque<Step>,
    /// Confirms that a scripted click reached the native project menu action.
    project_menu_requested: bool,
    /// Where the last scripted press was, for drags given relative to it.
    selftest_pointer: (f64, f64),
    /// Duration of the most recent scripted click through the AppKit handler.
    last_selftest_click_ms: f64,
    /// The layout the current cursor rects were built for.
    cursor_rects_for: Option<(f32, f32, f32, f32, Option<f32>)>,
    /// Scroll distance not yet amounting to a whole column or line.
    scroll_carry: (f64, f64),
    renderer: Renderer,
    /// Reused every frame so steady-state typing allocates nothing.
    glyphs: Vec<GlyphInstance>,
    theme: Theme,
    latency: Latency,
    layer: Retained<CAMetalLayer>,
    /// Logical size of the drawing area.
    viewport: Viewport,
    /// Set once a frame has been presented, so the status line can say
    /// whether the numbers mean anything yet.
    drew_once: bool,
    /// The slowest keystroke seen, with the phase breakdown that explains it.
    /// A single outlier is worth keeping in full rather than averaging away.
    worst: Option<(std::time::Duration, FrameTiming)>,
    /// A transient note for the status line: save results, errors.
    message: Option<(String, Instant)>,
    /// Where the tab bar last drew each tab, for hit-testing clicks.
    tab_hits: Vec<layout::TabHit>,
    /// First tab shown when the strip is narrower than all open tabs.
    tab_scroll: usize,
    tab_scroll_carry: f64,
    /// Tab being dragged across the strip.
    tab_drag: Option<usize>,
    /// Markdown preview: `Some(scroll)` when showing the rendered view of
    /// the active document rather than its source.
    preview: Option<usize>,
    /// Only the block under the caret reveals its Markdown markers.
    live_line: Option<(u64, usize)>,
    md_hits: Vec<layout::MarkdownHit>,
    /// The home screen's rows, as of the last frame that drew it.
    home_hits: Vec<layout::HomeHit>,
    /// Project folders opened before, most recent first.
    recent_projects: Vec<std::path::PathBuf>,
    copied_code: Option<(usize, Instant)>,
    /// Which tab a context menu was opened on. The menu action fires later,
    /// by which time the pointer has moved, so the target has to be recorded
    /// at click time rather than looked up again.
    context_tab: Option<usize>,
    /// Parse trees, one per open document that has a grammar.
    syntax: SyntaxStore,
    /// Reused each frame so highlighting allocates nothing in steady state.
    spans: Vec<Span>,
    /// Indexed project files for the Cmd-P palette.
    finder: Finder,
    project_index_rx: Option<mpsc::Receiver<ProjectIndexResult>>,
    project_search_rx: Option<mpsc::Receiver<Result<Vec<ProjectHit>, String>>>,
    project_search_cancel: Option<Arc<AtomicBool>>,
    /// A request in flight: the id of its response tab's buffer, and where
    /// the reply arrives.
    http: Option<(
        u64,
        mpsc::Receiver<Result<crate::http::curl::Response, String>>,
    )>,
    /// Response tabs, by buffer id. The buffer holds the text of the chosen
    /// segment; this holds the request and the reply it came from.
    responses: HashMap<u64, crate::http::curl::View>,
    /// Whether the project tree has the keyboard: after a click on a folder
    /// row or a right-click on any row, until a click elsewhere or typing
    /// in the document. Only then does Cmd-Delete trash the tree's
    /// selection; the selection itself outlives focus, and used to be enough,
    /// so Cmd-Delete while typing moved a file to the Trash.
    sidebar_keys: bool,
    /// Terminal sessions under the editor, Claude Code among them.
    terminal: crate::platform::terminal::Panel,
    /// Claude Code's way into this window, while a project is open.
    claude: Option<crate::platform::claude::Bridge>,
    /// The root a bridge was last started for, so a failure is reported
    /// once rather than retried every frame.
    claude_tried: Option<std::path::PathBuf>,
    /// Palette state: the query buffer and which row is selected. `None`
    /// when it is closed. The query is a Buffer for the same reason the find
    /// bar's is: editing it should behave like editing text.
    palette: Option<(Buffer, usize)>,
    /// The menu's enabled commands, read when the palette opens, for its
    /// `>` mode.
    commands: Vec<Command>,
    /// The palette's `@` and `#` modes: what the active document and the
    /// project index define.
    symbols: symbols::Symbols,
    /// The palette's first visible row, and the part of a scroll that has
    /// not made a whole row yet.
    palette_scroll: usize,
    palette_scroll_carry: f64,
    /// Cmd-L's line-number field. `None` when closed.
    goto: Option<Buffer>,
    /// Launched as `crc <file>` and no folder opened since. That is a
    /// quick edit, often as `$EDITOR`, and quitting it must not replace the
    /// saved session of the real project with one stray file.
    ephemeral_session: bool,
    /// Every dirty document has already been asked about on the way to
    /// closing the window. Closing the last window terminates the app, and
    /// without this the terminate path asks the same questions again, by
    /// then with no window left to cancel back to.
    discard_confirmed: bool,
    /// What was open when those questions started. Answering Don't Save
    /// closes the tab, and the session written at quit should still list it.
    quit_session: Option<Session>,

    // ---- frame pacing ----------------------------------------------------
    /// When the last frame was presented, to decide whether drawing now would
    /// outrun the display.
    last_draw: Option<Instant>,
    /// When the oldest unpresented input arrived. Latency is measured from
    /// here to the frame that actually shows it, so time spent waiting for a
    /// vsync counts against us rather than hiding.
    pending_input: Option<Instant>,
    /// Opening a file from the palette updates AppKit's title after the new
    /// content frame has been submitted; setTitle can take several ms.
    title_sync_pending: bool,
    /// One refresh interval, learned from the display link. 120Hz until it
    /// tells us otherwise.
    frame_interval: Duration,
    /// Paused whenever there is nothing to draw, so an idle editor does not
    /// wake the CPU 120 times a second.
    display_link: Option<Retained<CADisplayLink>>,
}

struct NativePreview {
    path: std::path::PathBuf,
    view: Retained<AnyObject>,
}

impl NativePreview {
    fn new(path: &Path, frame: NSRect) -> Option<Self> {
        // QuickLookUI is linked in build.rs. NSURL implements QLPreviewItem,
        // so no data source or format-specific decoder is needed here.
        let class = AnyClass::get(c"QLPreviewView")?;
        let allocated: *mut AnyObject = unsafe { msg_send![class, alloc] };
        let initialized: *mut AnyObject = unsafe { msg_send![allocated, initWithFrame: frame] };
        let view = unsafe { Retained::from_raw(initialized)? };
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        unsafe {
            let _: () = msg_send![&*view, setPreviewItem: &*url];
        }
        Some(Self {
            path: path.to_path_buf(),
            view,
        })
    }
}

fn default_preview(buffer: &Buffer) -> Option<usize> {
    buffer
        .extension()
        .filter(|e| matches!(e.as_str(), "md" | "markdown" | "mdown"))
        .map(|_| 0)
}

/// Opens `view` in the response tab titled `title`, reusing the tab that
/// already has that title. Returns the id of the tab's buffer.
fn show_response(state: &mut State, title: &str, view: crate::http::curl::View) -> u64 {
    let (text, ext) = view.text(view.segment);
    match state.docs.index_of_label(title) {
        Some(index) => {
            state.docs.switch(index);
            state.docs.active_mut().regenerate(&text);
        }
        None => state
            .docs
            .push(Buffer::generated(title, ext.unwrap_or("txt"), &text)),
    }
    let buffer = state.docs.active_mut();
    buffer.display_ext = ext;
    let id = buffer.id();
    state.responses.insert(id, view);
    state.preview = default_preview(state.docs.active());
    reveal_active_tab(state);
    id
}

/// Whether Cmd-Return has a request to send: a `.http` document, or a
/// response tab, which re-sends its own request.
fn can_send_from(state: &State) -> bool {
    let buffer = state.docs.active();
    state.responses.contains_key(&buffer.id())
        || buffer
            .path
            .as_deref()
            .is_some_and(crate::http::is_request_file)
}

fn reveal_active_tab(state: &mut State) {
    let active = state.docs.active_index();
    if !state.tab_hits.iter().any(|hit| hit.index == active) {
        if let Some(last) = state.tab_hits.last()
            && active == last.index + 1
            && last.x1
                + layout::tab_width(&state.docs, active, state.renderer.atlas.metrics.advance)
                <= chrome_of(state).tabs.x + chrome_of(state).tabs.width
        {
            return;
        }
        state.tab_scroll = active;
    }
}

/// The find bar's state: two fields and which one has focus.
struct FindBar {
    query: Buffer,
    replacement: Buffer,
    /// Tab moves between them.
    replacing: bool,
    options: SearchOptions,
    project: bool,
    results: Vec<ProjectHit>,
    selected: usize,
    result_scroll: usize,
    searching: bool,
}

/// F2's field. The name goes to the server that knows `path`.
struct RenameField {
    field: Buffer,
    path: std::path::PathBuf,
    at: crate::lsp::Position,
    language: Language,
}

/// Signature help, shown while the caret stays in the call.
struct SignatureTip {
    buffer: u64,
    /// The caret when it was asked for; moving before it closes the tip.
    anchor: usize,
    signature: crate::lsp::Signature,
}

/// Dragging a file or folder onto another folder to move it.
///
/// Held from the mouse going down on a row, but not acted on until the
/// pointer has travelled far enough to mean it: a drag that starts on every
/// click would make selecting a file impossible.
struct TreeDrag {
    path: std::path::PathBuf,
    origin: (f32, f32),
    /// Past the threshold, so this is a move rather than a click.
    active: bool,
    /// The row it would drop onto, or `None` for the project root.
    over: Option<usize>,
    /// Whether the pointer is somewhere a drop is allowed at all.
    valid: bool,
}

struct ProjectHit {
    path: std::path::PathBuf,
    range: std::ops::Range<usize>,
    line: usize,
    snippet: String,
}

pub struct Ivars {
    /// Scripted NSEvents have timestamp zero. Do not let physical input alter
    /// fixtures when AppKit temporarily makes an automated window key.
    testing: bool,
    state: RefCell<State>,
    /// Input has changed something that is not on screen yet.
    ///
    /// Outside the `RefCell` on purpose. Asking for a redraw is what every
    /// path does when it cannot do anything else, including the path taken
    /// because the state is already borrowed, so it must not need a borrow.
    needs_redraw: Cell<bool>,
    /// Set while `keyDown:` is handing an event to the input system. Text
    /// that arrives then is a keystroke, and `keyDown:` redraws and times it.
    /// Text that arrives at any other moment (the emoji picker, dictation, a
    /// text replacement) has to get itself onto the screen.
    in_key_down: Cell<bool>,
    /// Delay nested redraw requests until the outer key handler can attach
    /// its input timestamp to the changed frame.
    handling_key: Cell<bool>,
    /// Native self-test guard against drawing an unmeasured intermediate frame.
    draws_during_key_handler: Cell<u64>,
}

define_class!(
    // SAFETY:
    // - NSView has no subclassing requirements beyond being used on the main
    //   thread, which `thread_kind` enforces.
    // - EditorView does not implement Drop.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CrcEditorView"]
    #[ivars = Ivars]
    struct EditorView;

    impl EditorView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        /// Top-left origin, y increasing downward, matching the coordinate
        /// space the layout and the shader already use.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        /// One tracking area covering the view, rebuilt whenever it resizes.
        /// Without it the window never hears about the pointer moving, which
        /// is why nothing could react to hover.
        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            let _: () = unsafe { msg_send![super(self), updateTrackingAreas] };
            let existing = self.trackingAreas();
            for area in existing.iter() {
                self.removeTrackingArea(&area);
            }
            let options = NSTrackingAreaOptions::MouseEnteredAndExited
                | NSTrackingAreaOptions::MouseMoved
                | NSTrackingAreaOptions::ActiveInKeyWindow
                | NSTrackingAreaOptions::InVisibleRect;
            let area = unsafe {
                NSTrackingArea::initWithRect_options_owner_userInfo(
                    NSTrackingArea::alloc(),
                    self.visibleRect(),
                    options,
                    Some(self),
                    None,
                )
            };
            self.addTrackingArea(&area);
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            if self.ivars().testing && event.timestamp() != 0.0 { return; }
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            self.note_hover(point.x as f32, point.y as f32);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.note_hover(f32::NAN, f32::NAN);
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            if self.ivars().testing && event.timestamp() != 0.0 { return; }
            // Timestamp before any of our own work, so nothing we do hides
            // inside the measurement.
            let started = Instant::now();
            // Mid-composition every key belongs to the input method: arrows
            // move through its candidates, Return commits, Escape cancels.
            // Acting on them here as well would move the caret out from
            // under the text being composed.
            self.ivars().handling_key.set(true);
            let composing = self.ivars().state.borrow().marked.is_some();
            let changed = if composing {
                self.interpret(event)
            } else {
                self.handle_key(event)
            };
            self.ivars().handling_key.set(false);
            if changed {
                let mut state = self.ivars().state.borrow_mut();
                if state.preview.is_some() && default_preview(state.docs.active()).is_some() {
                    state.live_line = Some((state.docs.active().id(), state.docs.active().cursor_position().0));
                }
                let edited = state.docs.active().has_pending_edits();
                drop(state);
                self.lsp_after_key(edited);
                self.reparse();
                self.note_input(started);
                self.pump();
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if self.ivars().testing && event.timestamp() != 0.0 { return; }
            let started = Instant::now();
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let (x, y) = (point.x as f32, point.y as f32);
            let chrome = self.chrome();
            // A press only starts a text selection if it lands in the text.
            self.ivars().state.borrow_mut().selecting = None;

            // The scrollbar thumb, before anything that treats a press in
            // the text as a caret placement. A press on the track outside
            // the thumb brings the thumb to the pointer.
            {
                let mut state = self.ivars().state.borrow_mut();
                let plain = state.palette.is_none() && !state.git_open && state.goto.is_none();
                let m = state.renderer.atlas.metrics;
                let track = layout::scrollbar_track(chrome.text);
                let thumb = layout::scrollbar_thumb(state.docs.active(), chrome.text, m.line_height);
                if plain && let Some(thumb) = thumb && track.contains(x, y) {
                    let rows = chrome.text.rows(m.line_height);
                    let grab = if thumb.contains(x, y) {
                        y - thumb.y
                    } else {
                        let line = layout::scrollbar_line_at(
                            state.docs.active(),
                            chrome.text,
                            m.line_height,
                            y - thumb.height / 2.0,
                        );
                        state.docs.active_mut().scroll_to(line, rows);
                        thumb.height / 2.0
                    };
                    state.scrollbar_drag = Some(grab);
                    drop(state);
                    self.request_redraw();
                    self.pump();
                    return;
                }
            }
            // Anywhere but the sidebar takes the keyboard from the tree, and
            // the terminal has it exactly when the press is in the terminal.
            {
                let mut state = self.ivars().state.borrow_mut();
                if !chrome.sidebar.is_some_and(|rect| rect.contains(x, y)) {
                    state.sidebar_keys = false;
                }
                state.terminal.focus = chrome.terminal.is_some_and(|rect| rect.contains(x, y));
            }

            // Clicking away from a half-typed accent abandons it, here and in
            // the input system, which would otherwise finish it wherever the
            // caret went.
            let composing = self.ivars().state.borrow_mut().marked.take().is_some();
            if composing && let Some(context) = self.inputContext() {
                context.discardMarkedText();
            }

            // Control-click is a right click. AppKit turns it into one inside
            // NSView's own mouseDown:, which this override replaces.
            if event.modifierFlags().contains(NSEventModifierFlags::Control) {
                // Asked through the runtime, as AppKit itself would ask.
                let menu: Option<Retained<NSMenu>> =
                    unsafe { msg_send![self, menuForEvent: event] };
                if let Some(menu) = menu {
                    NSMenu::popUpContextMenu_withEvent_forView(&menu, event, self);
                }
                return;
            }

            if self.ivars().state.borrow().palette.is_some() {
                self.palette_click(x, y);
                return;
            }
            // A click anywhere else is the end of a name being typed: kept
            // if there is one, dropped if the field is empty. A click on the
            // tree itself goes no further, so the row it hit cannot shift
            // under it as the field disappears.
            if self.ivars().state.borrow().sidebar_edit.is_some() {
                self.finish_sidebar_edit(true);
                if chrome.sidebar.is_some_and(|rect| rect.contains(x, y)) {
                    return;
                }
            }
            let hit = {
                let mut state = self.ivars().state.borrow_mut();
                frame_of(&mut state).hit(x, y).cloned()
            };
            match hit {
                Some(Hit::ToolbarSidebar) => {
                    self.action_toggle_sidebar(sel!(toggleSidebar:), None);
                    return;
                }
                Some(Hit::ToolbarProject) => {
                    let menu = project_menu(MainThreadMarker::from(self));
                    if self.ivars().testing {
                        // A real AppKit popup starts its own event loop; the
                        // script cannot send its next step until it returns.
                        self.ivars().state.borrow_mut().project_menu_requested = true;
                    } else {
                        NSMenu::popUpContextMenu_withEvent_forView(&menu, event, self);
                    }
                    return;
                }
                Some(Hit::ToolbarSearch) => {
                    self.open_palette();
                    return;
                }
                // Grabbing the divider starts a resize rather than anything else.
                Some(Hit::SidebarDivider) => {
                    self.ivars().state.borrow_mut().dragging_divider = true;
                    return;
                }
                Some(Hit::SidebarExplorer) => {
                    self.set_sidebar_view(false);
                    return;
                }
                Some(Hit::SidebarSourceControl) => {
                    self.set_sidebar_view(true);
                    return;
                }
                Some(Hit::SidebarAction(slot)) => {
                    match slot {
                        0 => { if self.new_file() { self.request_redraw(); self.pump(); } }
                        1 => { if self.new_folder() { self.request_redraw(); self.pump(); } }
                        2 => self.collapse_tree(),
                        _ => self.refresh_tree(),
                    }
                    return;
                }
                Some(Hit::ResponseSegment(index)) => {
                    self.response_select(index);
                    return;
                }
                Some(Hit::ToolbarTerminal) => {
                    let _: () = unsafe { msg_send![self, toggleTerminal: None::<&AnyObject>] };
                    return;
                }
                Some(Hit::TerminalDivider) => {
                    self.ivars().state.borrow_mut().dragging_terminal = true;
                    return;
                }
                Some(Hit::TerminalTab(index)) => {
                    let mut state = self.ivars().state.borrow_mut();
                    state.terminal.active = index;
                    state.terminal.back = 0;
                    state.terminal.selection = None;
                    drop(state);
                    self.request_redraw();
                    self.pump();
                    return;
                }
                Some(Hit::TerminalClose(index)) => {
                    self.ivars().state.borrow_mut().terminal.close_tab(index);
                    self.after_terminal_layout();
                    return;
                }
                Some(Hit::TerminalNew) => {
                    self.spawn_terminal(false);
                    return;
                }
                Some(Hit::Terminal) => {
                    self.terminal_press(event, x, y);
                    return;
                }
                Some(Hit::ReviewAccept) => {
                    self.claude_decide(true);
                    return;
                }
                Some(Hit::ReviewReject) => {
                    self.claude_decide(false);
                    return;
                }
                // A review draws a diff, not its text: there is no caret
                // to place in it.
                Some(Hit::Text) if active_review(&self.ivars().state.borrow()).is_some() => {
                    return;
                }
                Some(Hit::Tab(index)) => {
                    self.ivars().state.borrow_mut().tab_drag = Some(index);
                    self.tab_click(x);
                    return;
                }
                Some(Hit::TabClose(_)) => {
                    self.ivars().state.borrow_mut().tab_drag = None;
                    self.tab_click(x);
                    return;
                }
                Some(Hit::TabStrip) => {
                    if event.clickCount() >= 2 {
                        let mut state = self.ivars().state.borrow_mut();
                        state.docs.push(Buffer::new());
                        state.preview = None;
                        state.tab_scroll = state.docs.active_index();
                        drop(state);
                        self.sync_title();
                        self.request_redraw();
                        self.pump();
                    }
                    return;
                }
                Some(Hit::Find) => {
                    if let Some(rect) = chrome.find {
                        self.find_click(x, y, rect);
                    }
                    return;
                }
                Some(Hit::Pane(index)) => {
                    // The click gives the pane the keyboard and then lands
                    // in it as a click in the focused pane would.
                    self.focus_pane(index);
                    let _: () = unsafe { msg_send![self, mouseDown: event] };
                    return;
                }
                Some(Hit::Text) => {
                    // The fold chevron beside a line number.
                    let fold = {
                        let state = self.ivars().state.borrow();
                        let (tx, ty) = chrome_of(&state).to_text(x, y);
                        layout::fold_chevron_at(state.docs.active(), &state.renderer.atlas, tx, ty)
                    };
                    if let Some(line) = fold {
                        self.toggle_fold(Some(line));
                        return;
                    }
                }
                Some(Hit::SidebarRow(_)) | Some(Hit::Status) | None => {}
            }

            if chrome.toolbar.contains(x, y) {
                if x > 120.0 && let Some(window) = self.window() {
                    window.performWindowDragWithEvent(event);
                }
                return;
            }

            // A click in the sidebar body is a tree action, not a caret move.
            if let Some(rect) = chrome.sidebar
                && rect.contains(x, y)
            {
                if self.ivars().state.borrow().git_open {
                    self.git_click(rect, x, y);
                    return;
                }
                // Remember what is under the pointer in case this turns into
                // a drag. Selecting still happens now, so a plain click is
                // unaffected.
                {
                    let mut state = self.ivars().state.borrow_mut();
                    let row = layout::sidebar_row_at(&state.tree, &state.renderer.atlas, rect, y);
                    state.tree_drag = row
                        .and_then(|index| state.tree.rows().get(index))
                        .map(|entry| TreeDrag {
                            path: entry.path.clone(),
                            origin: (x, y),
                            active: false,
                            over: None,
                            valid: false,
                        });
                }
                self.sidebar_click(y, rect);
                return;
            }

            if chrome.text.contains(x, y)
                && self.ivars().state.borrow().git_open
                && self.ivars().state.borrow().git.showing_diff
            {
                let mut state = self.ivars().state.borrow_mut();
                if let Some(hunk) = state.git.hunk_action_at(chrome.text, x, y) {
                    state.git.stage_hunk(hunk);
                }
                drop(state);
                self.request_redraw();
                self.resume_display_link();
                self.pump();
                if let Some(window) = self.window() {
                    window.invalidateCursorRectsForView(self);
                }
                return;
            }

            // Everything that is not text stops here: the find bar, the
            // padding above the first line, the status line. A click on
            // chrome used to fall through and move the caret behind it.
            if !chrome.text.contains(x, y) {
                return;
            }
            {
                let hit = {
                    let state = self.ivars().state.borrow();
                    (state.docs.is_home() && state.palette.is_none() && !state.git_open)
                        .then(|| state.home_hits.iter().find(|h| h.rect.contains(x, y)).cloned())
                        .flatten()
                };
                if let Some(hit) = hit {
                    match hit.action {
                        layout::HomeAction::OpenFolder => {
                            self.open_folder();
                        }
                        layout::HomeAction::NewFile => {
                            self.new_file();
                        }
                        layout::HomeAction::FindFile => self.open_palette(),
                        layout::HomeAction::OpenProject(path) => {
                            self.load_folder_path(&path.to_string_lossy());
                        }
                    }
                    self.request_redraw();
                    self.pump();
                    return;
                }
            }
            {
                let mut state = self.ivars().state.borrow_mut();
                if state.preview.is_some() && default_preview(state.docs.active()).is_some() {
                    let hit = state.md_hits.iter().find(|h| h.rect.contains(x, y)).cloned();
                    if let Some(hit) = hit {
                        if let Some((button, code)) = hit.copy
                            && button.contains(x, y) {
                            clipboard::write_text(&code);
                            state.copied_code = Some((hit.lines.start, Instant::now()));
                            state.message = Some(("copied code block".to_string(), Instant::now()));
                        } else {
                            let offset = if hit.caret_stops.is_empty() {
                                let m = state.renderer.atlas.metrics;
                                let row = ((y - hit.rect.y) / m.line_height).floor().max(0.0) as usize;
                                let line = (hit.lines.start + row).min(hit.lines.end.saturating_sub(1));
                                let column = ((x - hit.rect.x - m.advance) / m.advance).round().max(0.0) as usize;
                                let source = state.docs.active().rope.line(line);
                                let content = source.trim_end_matches('\n').trim_end_matches('\r');
                                let byte = content.chars().take(column).map(char::len_utf8).sum::<usize>();
                                state.docs.active().rope.line_to_byte(line) + byte
                            } else {
                                hit.caret_stops.iter().min_by(|(_, a), (_, b)| {
                                    let da = (a[1] - y).abs() * 1000.0 + (a[0] - x).abs();
                                    let db = (b[1] - y).abs() * 1000.0 + (b[0] - x).abs();
                                    da.total_cmp(&db)
                                }).map_or(0, |(at, _)| *at)
                            };
                            let line = state.docs.active().rope.byte_to_line(offset);
                            let id = state.docs.active().id();
                            state.docs.active_mut().place_cursor(offset, Motion::Move);
                            state.live_line = Some((id, line));
                        }
                    } else {
                        let buffer = state.docs.active_mut();
                        let end = buffer.rope.len_bytes();
                        buffer.place_cursor(end, Motion::Move);
                        let at = (buffer.id(), buffer.cursor_position().0);
                        state.live_line = Some(at);
                    }
                    drop(state);
                    self.request_redraw();
                    self.pump();
                    return;
                }
            }

            let offset = self.offset_for_event(event);
            let flags = event.modifierFlags();
            let shift = flags.contains(NSEventModifierFlags::Shift);
            let option = flags.contains(NSEventModifierFlags::Option);
            if flags.contains(NSEventModifierFlags::Command) && !shift && !option {
                // Cmd-click: go to the definition of what is under the pointer.
                self.ivars().state.borrow_mut().docs.active_mut().place_cursor(offset, Motion::Move);
                self.goto_definition(Some(offset));
                self.note_input(started);
                self.pump();
                return;
            }
            let chip = {
                let state = self.ivars().state.borrow();
                let point = self.convertPoint_fromView(event.locationInWindow(), None);
                state
                    .completion_chips
                    .iter()
                    .find(|(rect, _)| rect.contains(point.x as f32, point.y as f32))
                    .map(|(_, index)| *index)
            };
            if let Some(index) = chip {
                self.accept_completion(Some(index));
                self.note_input(started);
                self.pump();
                return;
            }
            self.ivars().state.borrow_mut().completion = None;
            {
                let mut state = self.ivars().state.borrow_mut();
                let buffer = state.docs.active_mut();
                // One click places the caret, two take a word, three a line,
                // and a drag that follows keeps selecting in the same unit.
                let pressed = match event.clickCount() {
                    2 => buffer.word_range_at(offset),
                    n if n >= 3 => buffer.line_range_at(offset),
                    _ => offset..offset,
                };
                let unit = match event.clickCount() {
                    2 => SelectUnit::Word,
                    n if n >= 3 => SelectUnit::Line,
                    _ => SelectUnit::Character,
                };
                if unit != SelectUnit::Character && !option && !shift {
                    buffer.select_range(pressed.start, pressed.end);
                    state.selecting = Some((unit, pressed));
                    drop(state);
                    self.note_input(started);
                    self.pump();
                    return;
                }
                state.selecting = Some((unit, pressed));
            }
            {
                let mut state = self.ivars().state.borrow_mut();
                if option {
                    // Option-click drops an extra cursor instead of moving
                    // the one you have.
                    state.docs.active_mut().add_cursor(offset, offset);
                } else {
                    // Shift-click extends an existing selection rather than
                    // starting a new one, matching every other editor.
                    let motion = if shift { Motion::Extend } else { Motion::Move };
                    state.docs.active_mut().place_cursor(offset, motion);
                }
            }
            self.note_input(started);
            self.pump();
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            if self.ivars().testing && event.timestamp() != 0.0 { return; }
            let point = self.convertPoint_fromView(event.locationInWindow(), None);

            let moved = {
                let mut state = self.ivars().state.borrow_mut();
                if let Some(from) = state.tab_drag {
                    let x = point.x as f32;
                    let to = state.tab_hits.iter().find(|h| x >= h.x0 && x < h.x1).map(|h| h.index);
                    if let Some(to) = to && state.docs.move_tab(from, to) {
                        state.tab_drag = Some(to);
                        true
                    } else { false }
                } else { false }
            };
            if moved {
                self.request_redraw();
                self.pump();
                return;
            }
            if self.ivars().state.borrow().tab_drag.is_some() { return; }

            let grab = self.ivars().state.borrow().scrollbar_drag;
            if let Some(grab) = grab {
                let chrome = self.chrome();
                {
                    let mut state = self.ivars().state.borrow_mut();
                    let m = state.renderer.atlas.metrics;
                    let rows = chrome.text.rows(m.line_height);
                    let line = layout::scrollbar_line_at(
                        state.docs.active(),
                        chrome.text,
                        m.line_height,
                        point.y as f32 - grab,
                    );
                    state.docs.active_mut().scroll_to(line, rows);
                }
                self.request_redraw();
                self.pump();
                return;
            }

            if self.ivars().state.borrow().tree_drag.is_some() {
                self.tree_drag_moved(point.x as f32, point.y as f32);
                return;
            }

            if self.ivars().state.borrow().terminal.selecting {
                self.terminal_drag(point.x as f32, point.y as f32);
                return;
            }

            if self.ivars().state.borrow().dragging_terminal {
                let chrome = self.chrome();
                {
                    let mut state = self.ivars().state.borrow_mut();
                    // From the pointer down to the status line, and never
                    // so tall that the editor loses its tabs and a few lines.
                    let bottom = chrome.status.y;
                    let top = chrome.toolbar.y + chrome.toolbar.height;
                    let most = (bottom - top - 200.0).max(crate::platform::terminal::MIN_HEIGHT);
                    state.terminal.height = (bottom - point.y as f32)
                        .clamp(crate::platform::terminal::MIN_HEIGHT, most);
                }
                self.after_terminal_layout();
                return;
            }

            if self.ivars().state.borrow().dragging_divider {
                {
                    let mut state = self.ivars().state.borrow_mut();
                    state.sidebar_width =
                        (point.x as f32).clamp(SIDEBAR_MIN, SIDEBAR_MAX);
                }
                self.request_redraw();
                self.pump();
                return;
            }

            // Only a drag that began in the text selects. One that began on a
            // tab or a sidebar row and wandered over the editor used to select
            // from the old caret to the pointer, in whatever had just opened.
            if self.ivars().state.borrow().selecting.is_none() {
                return;
            }
            self.ivars().state.borrow_mut().drag_point = Some((point.x as f32, point.y as f32));
            self.drag_select();
            self.request_redraw();
            self.pump();
        }

        /// Builds the right-click menu for wherever the click landed.
        ///
        /// Returning a menu from here is all AppKit needs: it handles the
        /// popup, tracking and dismissal, and the items route through the
        /// same responder chain (and the same validateMenuItem:) as the
        /// main menu bar, so nothing is duplicated.
        #[unsafe(method_id(menuForEvent:))]
        fn menu_for_event(&self, event: &NSEvent) -> Option<Retained<NSMenu>> {
            let mtm = MainThreadMarker::from(self);
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let chrome = self.chrome();
            let sidebar = chrome.sidebar;
            let (x, y) = (point.x as f32, point.y as f32);

            let in_sidebar = sidebar.is_some_and(|r| r.contains(x, y));
            let in_tab_bar = chrome.tabs.contains(x, y);
            let in_project = if chrome.toolbar.contains(x, y) {
                let mut state = self.ivars().state.borrow_mut();
                let State { tree, renderer, .. } = &mut *state;
                layout::toolbar_project(tree, &mut renderer.atlas, chrome.toolbar).contains(x, y)
            } else {
                false
            };

            // A right-click in a panel should also select what is under the
            // pointer, so the action applies to what was clicked rather than
            // to whatever happened to be selected before.
            if in_project {
                Some(project_menu(mtm))
            } else if in_tab_bar {
                let hit = self
                    .ivars()
                    .state
                    .borrow()
                    .tab_hits
                    .iter()
                    .find(|h| x >= h.x0 && x < h.x1)
                    .map(|h| h.index);
                self.ivars().state.borrow_mut().context_tab = hit;
                // Empty strip still belongs to the bar, so it is consumed
                // with an empty menu rather than falling through.
                Some(match hit {
                    Some(_) => tab_menu(mtm),
                    None => NSMenu::new(mtm),
                })
            } else if in_sidebar {
                let rect = sidebar.expect("in_sidebar implies a sidebar rectangle");
                let index = {
                    let state = self.ivars().state.borrow();
                    layout::sidebar_row_at(&state.tree, &state.renderer.atlas, rect, y)
                };
                if let Some(index) = index {
                    let mut state = self.ivars().state.borrow_mut();
                    state.tree.select(index);
                    state.sidebar_keys = true;
                    drop(state);
                    self.request_redraw();
                    self.pump();
                    Some(sidebar_item_menu(mtm))
                } else {
                    Some(project_menu(mtm))
                }
            } else {
                Some(editor_context_menu(mtm))
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            if self.ivars().testing && _event.timestamp() != 0.0 { return; }
            let drop = {
                let mut state = self.ivars().state.borrow_mut();
                state.dragging_divider = false;
                state.dragging_terminal = false;
                state.scrollbar_drag = None;
                // A press that never moved selects nothing.
                if std::mem::take(&mut state.terminal.selecting)
                    && state.terminal.selection.is_some_and(|(a, b)| a == b)
                {
                    state.terminal.selection = None;
                }
                state.tab_drag = None;
                state.selecting = None;
                state.drag_point = None;
                state.autoscroll_carry = 0.0;
                state.tree_drag.take().filter(|drag| drag.active && drag.valid)
            };
            if let Some(drag) = drop {
                self.drop_tree_item(drag);
            }
        }

        /// A resize cursor over the divider, so it reads as draggable.
        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            let Ok(mut state) = self.ivars().state.try_borrow_mut() else { return; };
            let chrome = chrome_of(&state);
            let add = |r: Viewport, cursor: &NSCursor| {
                if r.width > 0.0 && r.height > 0.0 {
                    self.addCursorRect_cursor(NSRect::new(NSPoint::new(r.x as f64, r.y as f64), NSSize::new(r.width as f64, r.height as f64)), cursor);
                }
            };
            add(state.viewport, &NSCursor::arrowCursor());
            if state.docs.is_home() && state.palette.is_none() && !state.git_open {
                for hit in &state.home_hits {
                    add(hit.rect, &NSCursor::pointingHandCursor());
                }
            }
            if let Some((query, _)) = &state.palette {
                let count = palette_rows(&palette_sources(&state), &query.rope.to_string()).len();
                let rect = layout::palette_rect(state.viewport, count);
                add(Viewport { x: rect.x + 12.0, y: rect.y + 8.0, width: rect.width - 68.0, height: 40.0 }, &NSCursor::IBeamCursor());
                add(Viewport { x: rect.x + rect.width - 52.0, y: rect.y + 8.0, width: 40.0, height: 40.0 }, &NSCursor::pointingHandCursor());
                let first = state.palette_scroll.min(layout::palette_max_scroll(count, layout::palette_visible_rows(rect)));
                let rows = count.saturating_sub(first).min(layout::palette_visible_rows(rect));
                add(Viewport { x: rect.x + 8.0, y: rect.y + layout::PALETTE_HEADER, width: rect.width - 16.0, height: rows as f32 * layout::PALETTE_ROW }, &NSCursor::pointingHandCursor());
                return;
            }
            for (hit, rect) in frame_of(&mut state).regions {
                let pointing = matches!(
                    hit,
                    Hit::ToolbarSidebar
                        | Hit::ToolbarProject
                        | Hit::ToolbarSearch
                        | Hit::SidebarExplorer
                        | Hit::SidebarSourceControl
                        | Hit::SidebarAction(_)
                        | Hit::Tab(_)
                        | Hit::TabClose(_)
                        | Hit::ResponseSegment(_)
                        | Hit::ReviewAccept
                        | Hit::ReviewReject
                        | Hit::ToolbarTerminal
                        | Hit::TerminalTab(_)
                        | Hit::TerminalClose(_)
                        | Hit::TerminalNew
                );
                if pointing {
                    add(rect, &NSCursor::pointingHandCursor());
                }
            }
            if state.native_preview.is_none() && !(state.git_open && state.git.showing_diff) && active_review(&state).is_none() {
                let gutter = layout::gutter_width(state.docs.active(), &state.renderer.atlas);
                add(chrome.text.inset_left(gutter), &NSCursor::IBeamCursor());
            }
            if state.git_open && state.git.showing_diff {
                for action in state.git.hunk_action_rects(chrome.text) {
                    add(action, &NSCursor::pointingHandCursor());
                }
            }
            if let Some(find) = chrome.find {
                add(Viewport { width: (find.width - 180.0).max(0.0), height: find.height.min(layout::FIND_ROW_HEIGHT * 2.0), ..find }, &NSCursor::IBeamCursor());
                add(Viewport { x: find.x + (find.width - 180.0).max(0.0), width: find.width.min(180.0), ..find }, &NSCursor::pointingHandCursor());
                if find.height > layout::FIND_ROW_HEIGHT * 2.0 { add(Viewport { y: find.y + layout::FIND_ROW_HEIGHT * 2.0, height: find.height - layout::FIND_ROW_HEIGHT * 2.0, ..find }, &NSCursor::pointingHandCursor()); }
            }
            if let Some(rect) = chrome.sidebar {
                let (explorer, source) = layout::sidebar_switcher(rect);
                add(explorer, &NSCursor::pointingHandCursor());
                add(source, &NSCursor::pointingHandCursor());
                if state.git_open {
                    // Only over the controls that are actually there: the
                    // empty column below the last change is not clickable.
                    let g = crate::platform::git_panel::Sidebar::new(rect);
                    if !state.git.busy() { add(g.refresh, &NSCursor::pointingHandCursor()); }
                    add(g.message, &NSCursor::IBeamCursor());
                    if state.git.can_commit() { add(g.commit, &NSCursor::pointingHandCursor()); }
                    for (entry, row) in state.git.rows(g) {
                        if matches!(entry, crate::platform::git_panel::Entry::File { .. }) {
                            add(row, &NSCursor::pointingHandCursor());
                        }
                    }
                } else {
                let (_, actions) = layout::sidebar_actions(rect);
                for action in actions { add(action, &NSCursor::pointingHandCursor()); }
                let rows = state.tree.len().saturating_sub(state.tree.scroll).min(layout::sidebar_rows(rect));
                add(Viewport { y: rect.y + layout::SIDEBAR_HEADER_HEIGHT, height: rows as f32 * layout::SIDEBAR_ROW_HEIGHT, ..rect }, &NSCursor::pointingHandCursor());
                }
                let divider = Viewport { x: rect.x + rect.width - DIVIDER_GRAB, width: DIVIDER_GRAB * 2.0, ..rect };
                use objc2::ClassType;
                let cursor = if NSCursor::class().class_method(objc2::sel!(columnResizeCursor)).is_some() { NSCursor::columnResizeCursor() } else { #[allow(deprecated)] NSCursor::resizeLeftRightCursor() };
                add(divider, &cursor);
            }
            if let Some(rect) = chrome.terminal {
                use objc2::ClassType;
                let band = Viewport { y: rect.y - DIVIDER_GRAB, height: DIVIDER_GRAB * 2.0, ..rect };
                let cursor = if NSCursor::class().class_method(objc2::sel!(rowResizeCursor)).is_some() { NSCursor::rowResizeCursor() } else { #[allow(deprecated)] NSCursor::resizeUpDownCursor() };
                add(band, &cursor);
            }

        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            // The palette is modal: the wheel scrolls its list, and never
            // the document or terminal behind it.
            if self.ivars().state.borrow().palette.is_some() {
                self.palette_wheel(event);
                return;
            }
            {
                let point = self.convertPoint_fromView(event.locationInWindow(), None);
                let panel = self.chrome().terminal;
                if panel.is_some_and(|rect| rect.contains(point.x as f32, point.y as f32)) {
                    let dy = event.scrollingDeltaY();
                    let mut state = self.ivars().state.borrow_mut();
                    let history = state.terminal.active_tab().map_or(0, |tab| {
                        tab.session.term.lock().unwrap_or_else(|e| e.into_inner()).scrollback_len()
                    });
                    let back = state.terminal.back;
                    state.terminal.back = if dy > 0.0 {
                        (back + 3).min(history)
                    } else if dy < 0.0 {
                        back.saturating_sub(3)
                    } else {
                        back
                    };
                    drop(state);
                    self.request_redraw();
                    self.pump();
                    return;
                }
            }
            {
                let point = self.convertPoint_fromView(event.locationInWindow(), None);
                let text = self.chrome().text;
                let mut state = self.ivars().state.borrow_mut();
                let id = state.docs.active().id();
                if text.contains(point.x as f32, point.y as f32)
                    && let Some(review) = state.claude.as_mut().and_then(|c| c.reviews.get_mut(&id))
                {
                    let dy = event.scrollingDeltaY();
                    if dy.abs() >= 0.01 {
                        review.scroll_by(if dy < 0.0 { 3 } else { -3 }, text);
                        drop(state);
                        self.request_redraw();
                        self.pump();
                    }
                    return;
                }
            }
            if self.ivars().state.borrow().git_open {
                let point = self.convertPoint_fromView(event.locationInWindow(), None);
                let (x, y) = (point.x as f32, point.y as f32);
                let chrome = self.chrome();
                // The change list and the diff scroll independently, each
                // under the pointer, like the two columns they are.
                if let Some(rect) = chrome.sidebar && (rect.contains(x, y) || chrome.text.contains(x, y)) {
                    let mut state = self.ivars().state.borrow_mut();
                    let g = crate::platform::git_panel::Sidebar::new(rect);
                    let delta = if event.scrollingDeltaY() < 0.0 { 3 } else { -3 };
                    state.git.scroll(delta, g.list.contains(x, y), g, chrome.text);
                    drop(state);
                    self.request_redraw();
                    if let Some(window) = self.window() {
                        window.invalidateCursorRectsForView(self);
                    }
                    self.pump();
                    return;
                }
            }
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            self.scroll_at(
                point.x as f32,
                point.y as f32,
                event.scrollingDeltaX(),
                event.scrollingDeltaY(),
                event.hasPreciseScrollingDeltas(),
            );
        }

        /// Plays the next step of a `CRC_SELFTEST` script. See `selftest.rs`.
        #[unsafe(method(selfTestStep:))]
        fn selftest_step(&self, _sender: Option<&AnyObject>) {
            let step = self.ivars().state.borrow_mut().selftest.pop_front();
            let Some(step) = step else {
                return;
            };
            self.play(&step);
            // Spaced out, so each event is handled and drawn before the next
            // arrives, as a person's would be.
            let _: () = unsafe {
                msg_send![self, performSelector: sel!(selfTestStep:),
                    withObject: None::<&AnyObject>, afterDelay: 0.03f64]
            };
        }

        /// Fires once per display refresh while there is work. Pauses itself
        /// when the buffer is idle.
        #[unsafe(method(onDisplayLink:))]
        fn on_display_link(&self, link: &CADisplayLink) {
            self.poll_tree_children();
            let git_changed = self.ivars().state.try_borrow_mut().is_ok_and(|mut state| {
                let changed = state.git.poll();
                if let Some(note) = state.git.take_announcement() {
                    state.message = Some((note, Instant::now()));
                }
                changed
            });
            if git_changed {
                self.request_redraw();
                if let Some(window) = self.window() {
                    window.invalidateCursorRectsForView(self);
                }
            }
            self.poll_project_index();
            self.poll_project_search();
            self.poll_http();
            self.poll_update();
            self.poll_ignored();
            self.refresh_after_watch();
            self.lsp_flush_changes();
            self.gutter_refresh();
            self.blame_refresh();
            self.claude_flush_selection();
            {
                // The link fires on any run-loop iteration, nested modal
                // loops included. Busy means try again next refresh.
                let Ok(mut state) = self.ivars().state.try_borrow_mut() else {
                    return;
                };
                // The link knows the real refresh interval, which is not
                // necessarily 120Hz: an external 60Hz display, or ProMotion
                // throttling on battery, both change it.
                let interval = link.targetTimestamp() - link.timestamp();
                if interval > 0.0 && interval < 1.0 {
                    state.frame_interval = Duration::from_secs_f64(interval);
                }
                if state.message.as_ref().is_some_and(|(_, at)| at.elapsed() >= Duration::from_secs(4)) {
                    state.message = None;
                    self.ivars().needs_redraw.set(true);
                }
                if !self.ivars().needs_redraw.get() && state.message.is_none() && !state.git.busy() && state.drag_point.is_none() && state.project_search_rx.is_none() && state.project_index_rx.is_none() && state.http.is_none() && state.project_changed_at.is_none() && state.git_changed_at.is_none() && state.lsp_dirty.is_empty() && state.gutter_dirty.is_empty() && state.gutter_pending.is_empty() && state.tree_children_pending.is_empty() && state.claude.as_ref().is_none_or(|c| c.selection_changed_at.is_none()) && state.update.is_none() && state.ignored_rx.is_none() && state.blame_rx.is_none() && state.blame_want.is_none() {
                    link.setPaused(true);
                    return;
                }
            }
            // A pointer held still past the edge of the text sends no events,
            // so this is what keeps the view moving under it.
            if self.drag_select() {
                self.request_redraw();
            }
            if self.ivars().needs_redraw.get() {
                self.draw_now();
            }
        }

        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, size: NSSize) {
            unsafe { msg_send![super(self), setFrameSize: size] }
            self.resize(size);
        }

        /// System Settings switched between light and dark. Followed only
        /// when the settings file says `system`.
        #[unsafe(method(viewDidChangeEffectiveAppearance))]
        fn appearance_changed(&self) {
            unsafe { msg_send![super(self), viewDidChangeEffectiveAppearance] }
            let follows = self.ivars().state.borrow().theme_choice
                == crate::platform::settings::ThemeChoice::System;
            if follows {
                self.apply_theme();
            }
        }

        #[unsafe(method(viewDidChangeBackingProperties))]
        fn backing_changed(&self) {
            unsafe { msg_send![super(self), viewDidChangeBackingProperties] }
            let size = self.frame().size;
            self.resize(size);
        }
    }

    /// Menu actions.
    ///
    /// These have to exist as real selectors. A menu item carrying a key
    /// equivalent is matched by AppKit *before* the event reaches `keyDown:`,
    /// so a Cmd-S item whose action nothing implements does not fall through
    /// to our handler, it just disables itself and swallows the shortcut.
    impl EditorView {
        #[unsafe(method(openDocument:))]
        fn action_open(&self, _sender: Option<&AnyObject>) {
            // No unsaved-changes prompt: opening adds a tab, so nothing is
            // being discarded.
            if self.open_file() {
                self.reparse();
                self.sync_title();
                self.request_redraw();
                self.pump();
            }
        }

        #[unsafe(method(openFolder:))]
        fn action_open_folder(&self, _sender: Option<&AnyObject>) {
            if self.open_folder() {
                self.request_redraw();
                self.pump();
            }
        }

        #[unsafe(method(newDocument:))]
        fn action_new_file(&self, _sender: Option<&AnyObject>) {
            if self.new_file() {
                self.request_redraw();
                self.pump();
            }
        }

        #[unsafe(method(newFolder:))]
        fn action_new_folder(&self, _sender: Option<&AnyObject>) {
            if self.new_folder() {
                self.request_redraw();
                self.pump();
            }
        }

        /// Toggles the rendered Markdown view.
        #[unsafe(method(togglePreview:))]
        fn action_toggle_preview(&self, _sender: Option<&AnyObject>) {
            {
                let mut state = self.ivars().state.borrow_mut();
                // Only where there is something to preview. Toggling the flag
                // on a buffer the renderer will not preview changed hidden
                // state and drew nothing, so the command looked dead and the
                // next Markdown file opened in whichever mode it had left.
                if default_preview(state.docs.active()).is_none() {
                    state.message = Some((
                        "Preview is for Markdown files".to_string(),
                        Instant::now(),
                    ));
                } else {
                    state.preview = match state.preview {
                        Some(_) => None,
                        None => Some(0),
                    };
                }
            }
            self.request_redraw();
            self.pump();
        }

        #[unsafe(method(openSettings:))]
        fn action_open_settings(&self, _sender: Option<&AnyObject>) {
            self.open_settings();
            self.request_redraw();
            self.pump();
        }

        #[unsafe(method(aboutCrc:))]
        fn action_about(&self, _sender: Option<&AnyObject>) {
            let mtm = MainThreadMarker::from(self);
            let alert = NSAlert::new(mtm);
            alert.setMessageText(&NSString::from_str("crc"));
            alert.setInformativeText(&NSString::from_str(&format!(
                "Version {}\nBuilt from {}",
                env!("CARGO_PKG_VERSION"),
                option_env!("CRC_BUILD_REVISION").unwrap_or("unknown")
            )));
            alert.runModal();
        }

        #[unsafe(method(selectNextOccurrence:))]
        fn action_select_next(&self, _sender: Option<&AnyObject>) {
            let changed = self
                .ivars()
                .state
                .borrow_mut()
                .docs
                .active_mut()
                .select_next_occurrence();
            if changed {
                self.after_edit();
            }
        }

        #[unsafe(method(goToLine:))]
        fn action_goto_line(&self, _sender: Option<&AnyObject>) {
            self.open_goto();
        }

        #[unsafe(method(replaceAll:))]
        fn action_replace_all(&self, _sender: Option<&AnyObject>) {
            // Only meaningful with the bar open, since that is where the
            // search and replacement text live.
            if self.ivars().state.borrow().find.is_none() {
                self.open_find();
                return;
            }
            if self.ivars().state.borrow().find.as_ref().is_some_and(|bar| bar.project) {
                self.replace_in_project();
                return;
            }
            self.replace_all();
        }

        #[unsafe(method(toggleComment:))]
        fn action_toggle_comment(&self, _sender: Option<&AnyObject>) {
            let token = {
                let state = self.ivars().state.borrow();
                state
                    .docs
                    .active()
                    .path
                    .as_deref()
                    .and_then(Language::from_path)
                    // Plain text gets `//`; a language with no line comment
                    // (HTML, CSS, JSON) gets nothing, not a wrong token.
                    .map_or(Some("//"), |l| l.line_comment())
            };
            let Some(token) = token else {
                return;
            };
            self.ivars().state.borrow_mut().docs.active_mut().toggle_comment(token);
            self.after_edit();
        }

        #[unsafe(method(duplicateLines:))]
        fn action_duplicate(&self, _sender: Option<&AnyObject>) {
            self.ivars().state.borrow_mut().docs.active_mut().duplicate_lines();
            self.after_edit();
        }

        #[unsafe(method(moveLineUp:))]
        fn action_move_line_up(&self, _sender: Option<&AnyObject>) {
            self.ivars().state.borrow_mut().docs.active_mut().move_lines(false);
            self.after_edit();
        }

        #[unsafe(method(moveLineDown:))]
        fn action_move_line_down(&self, _sender: Option<&AnyObject>) {
            self.ivars().state.borrow_mut().docs.active_mut().move_lines(true);
            self.after_edit();
        }

        #[unsafe(method(openQuickly:))]
        fn action_quick_open(&self, _sender: Option<&AnyObject>) {
            self.open_palette();
        }

        #[unsafe(method(switchBranch:))]
        fn action_switch_branch(&self, _sender: Option<&AnyObject>) {
            self.open_branch_picker();
        }

        #[unsafe(method(gitFetch:))]
        fn action_git_fetch(&self, _sender: Option<&AnyObject>) {
            self.git_remote(crate::project::git::Remote::Fetch);
        }

        #[unsafe(method(gitPull:))]
        fn action_git_pull(&self, _sender: Option<&AnyObject>) {
            self.git_remote(crate::project::git::Remote::Pull);
        }

        #[unsafe(method(gitPush:))]
        fn action_git_push(&self, _sender: Option<&AnyObject>) {
            self.git_remote(crate::project::git::Remote::Push);
        }

        #[unsafe(method(foldBlock:))]
        fn action_fold(&self, _sender: Option<&AnyObject>) {
            self.fold_command(Some(true));
        }

        #[unsafe(method(unfoldBlock:))]
        fn action_unfold(&self, _sender: Option<&AnyObject>) {
            self.fold_command(Some(false));
        }

        #[unsafe(method(foldAll:))]
        fn action_fold_all(&self, _sender: Option<&AnyObject>) {
            self.fold_command(None);
        }

        #[unsafe(method(unfoldAll:))]
        fn action_unfold_all(&self, _sender: Option<&AnyObject>) {
            let mut state = self.ivars().state.borrow_mut();
            state.docs.active_mut().folds.clear();
            drop(state);
            self.request_redraw();
            self.pump();
        }

        #[unsafe(method(toggleWordWrap:))]
        fn action_toggle_word_wrap(&self, _sender: Option<&AnyObject>) {
            self.toggle_word_wrap();
        }

        #[unsafe(method(findReferences:))]
        fn action_find_references(&self, _sender: Option<&AnyObject>) {
            self.find_references();
        }

        #[unsafe(method(renameSymbol:))]
        fn action_rename_symbol(&self, _sender: Option<&AnyObject>) {
            self.start_rename();
        }

        #[unsafe(method(formatDocument:))]
        fn action_format_document(&self, _sender: Option<&AnyObject>) {
            self.format_document(false);
        }

        #[unsafe(method(goToSymbol:))]
        fn action_go_to_symbol(&self, _sender: Option<&AnyObject>) {
            self.open_palette_with("@");
        }

        #[unsafe(method(goToProjectSymbol:))]
        fn action_go_to_project_symbol(&self, _sender: Option<&AnyObject>) {
            self.open_palette_with("#");
        }

        #[unsafe(method(performFindPanelAction:))]
        fn action_find(&self, _sender: Option<&AnyObject>) {
            self.open_find();
        }

        #[unsafe(method(findInProject:))]
        fn action_find_project(&self, _sender: Option<&AnyObject>) {
            self.open_find();
            if let Some(bar) = &mut self.ivars().state.borrow_mut().find { bar.project = true; }
            self.request_redraw();
            self.pump();
        }

        #[unsafe(method(findNext:))]
        fn action_find_next(&self, _sender: Option<&AnyObject>) {
            self.find_step(true);
            self.request_redraw();
            self.pump();
        }

        #[unsafe(method(findPrevious:))]
        fn action_find_previous(&self, _sender: Option<&AnyObject>) {
            self.find_step(false);
            self.request_redraw();
            self.pump();
        }

        #[unsafe(method(selectNextTab:))]
        fn action_next_tab(&self, _sender: Option<&AnyObject>) {
            let mut state = self.ivars().state.borrow_mut();
            state.docs.cycle(1);
            state.preview = default_preview(state.docs.active());
            reveal_active_tab(&mut state);
            drop(state);
            self.sync_title();
            self.reparse();
            self.request_redraw();
            self.pump();
        }

        #[unsafe(method(selectPreviousTab:))]
        fn action_prev_tab(&self, _sender: Option<&AnyObject>) {
            let mut state = self.ivars().state.borrow_mut();
            state.docs.cycle(-1);
            state.preview = default_preview(state.docs.active());
            reveal_active_tab(&mut state);
            drop(state);
            self.sync_title();
            self.reparse();
            self.request_redraw();
            self.pump();
        }

        /// Cmd-W closes the tab while more than one is open, and only falls
        /// through to closing the window on the last one. That is what every
        /// tabbed editor does and what muscle memory expects.
        #[unsafe(method(closeTabOrWindow:))]
        fn action_close_tab(&self, _sender: Option<&AnyObject>) {
            // Cmd-W in the terminal closes its session, as in any terminal.
            if self.terminal_has_keys() {
                let mut state = self.ivars().state.borrow_mut();
                let active = state.terminal.active;
                state.terminal.close_tab(active);
                drop(state);
                self.after_terminal_layout();
                return;
            }
            let (count, active) = {
                let state = self.ivars().state.borrow();
                (state.docs.len(), state.docs.active_index())
            };
            if count > 1 {
                self.close_tab(active);
            } else if self.confirm_discard_all()
                && let Some(window) = self.window() {
                    window.close();
                }
        }

        #[unsafe(method(closeContextTab:))]
        fn action_close_context_tab(&self, _sender: Option<&AnyObject>) {
            // Copied out first. As an `if let` scrutinee the borrow would
            // live for the whole block, across the unsaved-changes alert in
            // `close_tab`, whose `borrow_mut` then aborts the process.
            let index = self.ivars().state.borrow().context_tab;
            if let Some(index) = index {
                self.close_tab(index);
            }
        }

        #[unsafe(method(moveContextTabLeft:))]
        fn action_move_context_tab_left(&self, _sender: Option<&AnyObject>) {
            self.move_context_tab(-1);
        }

        #[unsafe(method(moveContextTabRight:))]
        fn action_move_context_tab_right(&self, _sender: Option<&AnyObject>) {
            self.move_context_tab(1);
        }

        /// Closes everything except the tab that was right-clicked.
        ///
        /// Back to front, so each close does not shift the indices of the
        /// ones still to go.
        #[unsafe(method(closeOtherTabs:))]
        fn action_close_other_tabs(&self, _sender: Option<&AnyObject>) {
            let Some(keep) = self.ivars().state.borrow().context_tab else {
                return;
            };
            let count = self.ivars().state.borrow().docs.len();
            for index in (0..count).rev() {
                if index != keep {
                    self.close_tab(index);
                }
            }
            self.sync_title();
            self.reparse();
            self.request_redraw();
            self.pump();
        }

        #[unsafe(method(closeAllTabs:))]
        fn action_close_all_tabs(&self, _sender: Option<&AnyObject>) {
            let count = self.ivars().state.borrow().docs.len();
            for index in (0..count).rev() {
                self.close_tab(index);
            }
            self.sync_title();
            self.reparse();
            self.request_redraw();
            self.pump();
        }

        #[unsafe(method(copyContextTabPath:))]
        fn action_copy_context_path(&self, _sender: Option<&AnyObject>) {
            let path = {
                let state = self.ivars().state.borrow();
                state
                    .context_tab
                    .and_then(|i| state.docs.iter().nth(i))
                    .and_then(|b| b.path.clone())
            };
            if let Some(path) = path {
                clipboard::write_text(&path.to_string_lossy());
                self.ivars().state.borrow_mut().message =
                    Some((format!("copied {}", path.display()), Instant::now()));
                self.request_redraw();
                self.pump();
            }
        }

        #[unsafe(method(revealContextTab:))]
        fn action_reveal_context_tab(&self, _sender: Option<&AnyObject>) {
            let path = {
                let state = self.ivars().state.borrow();
                state
                    .context_tab
                    .and_then(|i| state.docs.iter().nth(i))
                    .and_then(|b| b.path.clone())
            };
            if let Some(path) = path {
                let _ = std::process::Command::new("/usr/bin/open")
                    .arg("-R")
                    .arg(&path)
                    .spawn();
            }
        }

        #[unsafe(method(revealInFinder:))]
        fn action_reveal(&self, _sender: Option<&AnyObject>) {
            let path = {
                let state = self.ivars().state.borrow();
                state
                    .tree
                    .selected
                    .and_then(|i| state.tree.rows().get(i))
                    .map(|e| e.path.clone())
                    .or_else(|| state.docs.active().path.clone())
            };
            let Some(path) = path else {
                return;
            };
            // `open -R` is the documented way to reveal a path in Finder and
            // avoids pulling in the NSWorkspace surface for one action.
            let _ = std::process::Command::new("/usr/bin/open")
                .arg("-R")
                .arg(&path)
                .spawn();
        }

        /// Opens a new GitHub issue in the browser with the version, macOS
        /// and the latest crash filled in. Nothing is sent from here: the
        /// person reads the form and submits it, or does not.
        #[unsafe(method(reportProblem:))]
        fn action_report_problem(&self, _sender: Option<&AnyObject>) {
            use crate::platform::report;
            let crash = report::logs_dir().and_then(|dir| report::latest_crash(&dir));
            let body = report::issue_body(&crate::build_label(), &report::macos_version(), crash.as_ref());
            let url = report::issue_url(&body);
            // A test instance must not open a browser; it says what it would open.
            let note = if self.ivars().testing {
                format!("would open {url}")
            } else {
                match std::process::Command::new("/usr/bin/open").arg(&url).spawn() {
                    Ok(_) => "a new issue is open in your browser; nothing is sent until you submit it".to_string(),
                    Err(e) => format!("could not open the browser: {e}"),
                }
            };
            self.ivars().state.borrow_mut().message = Some((note, Instant::now()));
            self.request_redraw();
            self.pump();
        }

        #[unsafe(method(checkForUpdates:))]
        fn action_check_for_updates(&self, _sender: Option<&AnyObject>) {
            self.start_update_check(true);
            self.ivars().state.borrow_mut().message =
                Some(("checking for updates…".to_string(), Instant::now()));
            self.request_redraw();
            self.pump();
        }

        #[unsafe(method(showCrashLogs:))]
        fn action_show_crash_logs(&self, _sender: Option<&AnyObject>) {
            let Some(dir) = crate::platform::report::logs_dir() else {
                return;
            };
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::process::Command::new("/usr/bin/open").arg(&dir).spawn();
        }

        #[unsafe(method(revealProjectInFinder:))]
        fn action_reveal_project(&self, _sender: Option<&AnyObject>) {
            let root = self.ivars().state.borrow().tree.root().map(Path::to_path_buf);
            if let Some(root) = root {
                let _ = std::process::Command::new("/usr/bin/open")
                    .arg("-R")
                    .arg(root)
                    .spawn();
            }
        }

        #[unsafe(method(renameProjectItem:))]
        fn action_rename_project_item(&self, _sender: Option<&AnyObject>) {
            let path = {
                let state = self.ivars().state.borrow();
                state
                    .tree
                    .selected
                    .and_then(|index| state.tree.rows().get(index))
                    .map(|entry| entry.path.clone())
            };
            let Some(path) = path else { return };
            self.start_sidebar_edit(SidebarEditKind::Rename(path));
        }

        #[unsafe(method(trashProjectItem:))]
        fn action_trash_project_item(&self, _sender: Option<&AnyObject>) {
            let path = {
                let state = self.ivars().state.borrow();
                state
                    .tree
                    .selected
                    .and_then(|index| state.tree.rows().get(index))
                    .map(|entry| entry.path.clone())
            };
            let Some(path) = path else { return };
            let source_key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            if all_docs(&self.ivars().state.borrow()).any(|d| d.has_dirty_under(&source_key)) {
                let mtm = MainThreadMarker::from(self);
                let alert = NSAlert::new(mtm);
                alert.setAlertStyle(NSAlertStyle::Warning);
                alert.setMessageText(&NSString::from_str("Unsaved changes are open"));
                alert.setInformativeText(&NSString::from_str(
                    "Save or close the affected tabs before moving this item to Trash.",
                ));
                alert.addButtonWithTitle(&NSString::from_str("OK"));
                alert.runModal();
                return;
            }

            let mtm = MainThreadMarker::from(self);
            let alert = NSAlert::new(mtm);
            alert.setAlertStyle(NSAlertStyle::Warning);
            alert.setMessageText(&NSString::from_str(&format!("Move “{name}” to Trash?")));
            alert.setInformativeText(&NSString::from_str(
                "The item can be recovered from the Trash.",
            ));
            alert.addButtonWithTitle(&NSString::from_str("Move to Trash"));
            alert.addButtonWithTitle(&NSString::from_str("Cancel"));
            const FIRST: isize = 1000;
            if alert.runModal() != FIRST {
                return;
            }

            let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
            let result = NSFileManager::defaultManager()
                .trashItemAtURL_resultingItemURL_error(&url, None);
            match result {
                Ok(()) => {
                    {
                        let mut state = self.ivars().state.borrow_mut();
                        for docs in all_docs_mut(&mut state) {
                            docs.close_under(&source_key);
                        }
                        state.preview = default_preview(state.docs.active());
                        reveal_active_tab(&mut state);
                        state.message = Some((format!("moved {name} to Trash"), Instant::now()));
                    }
                    self.refresh_project_after_disk_change();
                    self.sync_title();
                    self.reparse();
                }
                Err(error) => {
                    self.ivars().state.borrow_mut().message = Some((
                        format!("could not move to Trash: {}", error.localizedDescription()),
                        Instant::now(),
                    ));
                }
            }
            self.request_redraw();
            self.pump();
        }

        #[unsafe(method(sendRequest:))]
        fn action_send_request(&self, _sender: Option<&AnyObject>) {
            self.send_request();
        }

        #[unsafe(method(splitEditor:))]
        fn action_split_editor(&self, _sender: Option<&AnyObject>) {
            self.split_pane();
        }

        #[unsafe(method(goToDefinition:))]
        fn action_go_to_definition(&self, _sender: Option<&AnyObject>) {
            self.goto_definition(None);
        }

        #[unsafe(method(showHover:))]
        fn action_show_hover(&self, _sender: Option<&AnyObject>) {
            self.show_hover();
        }

        #[unsafe(method(forgetCompletionHistory:))]
        fn action_forget_completion_history(&self, _sender: Option<&AnyObject>) {
            self.forget_completion_history();
        }

        #[unsafe(method(triggerCompletion:))]
        fn action_trigger_completion(&self, _sender: Option<&AnyObject>) {
            self.request_completion(true);
        }

        #[unsafe(method(closePane:))]
        fn action_close_pane(&self, _sender: Option<&AnyObject>) {
            let focused = self.ivars().state.borrow().focused_pane;
            self.close_pane(focused);
        }

        #[unsafe(method(focusNextPane:))]
        fn action_focus_next_pane(&self, _sender: Option<&AnyObject>) {
            let (focused, count) = {
                let state = self.ivars().state.borrow();
                (state.focused_pane, pane_count(&state))
            };
            self.focus_pane((focused + 1) % count);
        }

        #[unsafe(method(focusPreviousPane:))]
        fn action_focus_previous_pane(&self, _sender: Option<&AnyObject>) {
            let (focused, count) = {
                let state = self.ivars().state.borrow();
                (state.focused_pane, pane_count(&state))
            };
            self.focus_pane((focused + count - 1) % count);
        }

        #[unsafe(method(openClaude:))]
        fn action_open_claude(&self, _sender: Option<&AnyObject>) {
            self.open_terminal(true);
        }

        /// Shows the terminal with the keyboard, or hides it when it has
        /// the keyboard already. Its sessions keep running while hidden.
        #[unsafe(method(toggleTerminal:))]
        fn action_toggle_terminal(&self, _sender: Option<&AnyObject>) {
            let hide = self.ivars().state.borrow().terminal.has_keys();
            if hide {
                let mut state = self.ivars().state.borrow_mut();
                state.terminal.open = false;
                state.terminal.focus = false;
                drop(state);
                self.after_terminal_layout();
            } else {
                self.open_terminal(false);
            }
        }

        #[unsafe(method(zoomIn:))]
        fn action_zoom_in(&self, _sender: Option<&AnyObject>) {
            let size = self.ivars().state.borrow().font_size + 1.0;
            self.set_font_size(size);
        }

        #[unsafe(method(zoomOut:))]
        fn action_zoom_out(&self, _sender: Option<&AnyObject>) {
            let size = self.ivars().state.borrow().font_size - 1.0;
            self.set_font_size(size);
        }

        #[unsafe(method(zoomActual:))]
        fn action_zoom_actual(&self, _sender: Option<&AnyObject>) {
            self.set_font_size(crate::platform::settings::DEFAULT_FONT_SIZE);
        }

        #[unsafe(method(toggleSidebar:))]
        fn action_toggle_sidebar(&self, _sender: Option<&AnyObject>) {
            {
                let mut state = self.ivars().state.borrow_mut();
                state.sidebar = !state.sidebar;
            }
            self.request_redraw();
            self.pump();
        }

        #[unsafe(method(showSourceControl:))]
        fn action_source_control(&self, _sender: Option<&AnyObject>) {
            self.open_git();
        }

        #[unsafe(method(gitRefresh:))]
        fn action_git_refresh(&self, _sender: Option<&AnyObject>) {
            // Showing the view refreshes it.
            self.set_sidebar_view(true);
        }

        /// Commits what is staged when there is a message, and otherwise
        /// shows the view with the message field taking the keys.
        #[unsafe(method(gitCommit:))]
        fn action_git_commit(&self, _sender: Option<&AnyObject>) {
            let ready = {
                let state = self.ivars().state.borrow();
                state.git_open && state.git.can_commit()
            };
            if ready {
                self.ivars().state.borrow_mut().git.commit();
            } else {
                if !self.ivars().state.borrow().git_open {
                    self.set_sidebar_view(true);
                }
                self.ivars().state.borrow_mut().git_focus = true;
            }
            self.request_redraw();
            self.resume_display_link();
            self.pump();
        }

        #[unsafe(method(newTerminal:))]
        fn action_new_terminal(&self, _sender: Option<&AnyObject>) {
            self.spawn_terminal(false);
        }

        #[unsafe(method(saveDocument:))]
        fn action_save(&self, _sender: Option<&AnyObject>) {
            self.save(false);
            self.request_redraw();
            self.pump();
        }

        #[unsafe(method(saveDocumentAs:))]
        fn action_save_as(&self, _sender: Option<&AnyObject>) {
            self.save(true);
            self.request_redraw();
            self.pump();
        }

        #[unsafe(method(revertDocumentToSaved:))]
        fn action_revert(&self, _sender: Option<&AnyObject>) {
            self.revert_to_saved();
            self.request_redraw();
            self.pump();
        }

        #[unsafe(method(undo:))]
        fn action_undo(&self, _sender: Option<&AnyObject>) {
            let (changed, focus) = self.edit_focused(|b, _| b.undo());
            if changed {
                self.after_focused_edit(focus);
            }
        }

        #[unsafe(method(redo:))]
        fn action_redo(&self, _sender: Option<&AnyObject>) {
            let (changed, focus) = self.edit_focused(|b, _| b.redo());
            if changed {
                self.after_focused_edit(focus);
            }
        }

        #[unsafe(method(selectAll:))]
        fn action_select_all(&self, _sender: Option<&AnyObject>) {
            if self.terminal_has_keys() {
                return;
            }
            let ((), focus) = self.edit_focused(|b, _| b.select_all());
            // A selection is not an edit: nothing to search again for.
            match focus {
                Focus::Document => self.after_edit(),
                _ => {
                    self.request_redraw();
                    self.pump();
                }
            }
        }

        #[unsafe(method(copy:))]
        fn action_copy(&self, _sender: Option<&AnyObject>) {
            // In the terminal, its own selection; never the document's.
            if self.terminal_has_keys() {
                if let Some(text) = self.terminal_selected_text() {
                    clipboard::write_text(&text);
                }
                return;
            }
            let (text, _) = self.edit_focused(|b, _| b.selected_text());
            if let Some(text) = text {
                clipboard::write_text(&text);
            }
        }

        #[unsafe(method(cut:))]
        fn action_cut(&self, _sender: Option<&AnyObject>) {
            let (text, focus) = self.edit_focused(|b, _| {
                let text = b.selected_text();
                if text.is_some() {
                    b.backspace();
                }
                text
            });
            if let Some(text) = text {
                clipboard::write_text(&text);
                self.after_focused_edit(focus);
            }
        }

        #[unsafe(method(paste:))]
        fn action_paste(&self, _sender: Option<&AnyObject>) {
            let Some(text) = clipboard::read_text() else {
                return;
            };
            if self.terminal_has_keys() {
                self.terminal_write(&{
                    let state = self.ivars().state.borrow();
                    let bracketed = state.terminal.active_tab().is_some_and(|tab| {
                        tab.session.term.lock().unwrap_or_else(|e| e.into_inner()).modes.bracketed_paste
                    });
                    crate::term::keys::paste(&text, bracketed)
                });
                return;
            }
            let (pasted, focus) = self.edit_focused(|b, focus| {
                let text = match focus {
                    Focus::Document => text,
                    Focus::Goto => text.chars().filter(char::is_ascii_digit).collect(),
                    Focus::FindQuery | Focus::Field => single_line(&text),
                };
                if !text.is_empty() {
                    b.insert(&text);
                }
                !text.is_empty()
            });
            if pasted {
                self.after_focused_edit(focus);
            }
        }

        /// Greys out what cannot be done right now. Without this, Undo and
        /// Paste stay enabled forever and the menu lies about the state of
        /// the document.
        #[unsafe(method(validateMenuItem:))]
        fn validate_menu_item(&self, item: &NSMenuItem) -> Bool {
            let Some(action) = item.action() else {
                return Bool::YES;
            };
            let state = self.ivars().state.borrow();
            let terminal = state.terminal.has_keys() && !field_has_keys(&state);
            let enabled = if terminal && action == sel!(copy:) {
                state.terminal.selection.is_some()
            } else if terminal && (action == sel!(cut:) || action == sel!(undo:) || action == sel!(redo:)) {
                false
            } else if action == sel!(copy:) || action == sel!(cut:) {
                focused_buffer(&state).selection().is_some()
            } else if action == sel!(undo:) {
                focused_buffer(&state).can_undo()
            } else if action == sel!(redo:) {
                focused_buffer(&state).can_redo()
            } else if action == sel!(saveDocument:) {
                state.docs.active().is_dirty() || state.docs.active().path.is_none()
            } else if action == sel!(revertDocumentToSaved:) {
                let active = state.docs.active();
                active.path.is_some()
                    && (active.is_dirty() || active.disk_state() != DiskState::Unchanged)
            } else if action == sel!(paste:) {
                clipboard::read_text().is_some_and(|t| !t.is_empty())
            } else if action == sel!(moveContextTabLeft:) {
                state.context_tab.is_some_and(|index| index > 0)
            } else if action == sel!(moveContextTabRight:) {
                state.context_tab.is_some_and(|index| index + 1 < state.docs.len())
            } else if action == sel!(renameProjectItem:) || action == sel!(trashProjectItem:) {
                // Only while the tree has the keyboard, and never while a
                // field does: Cmd-Delete anywhere else edits text, and must
                // not reach the tree's selection.
                state.tree.selected.is_some() && state.sidebar_keys && !field_has_keys(&state)
            } else if action == sel!(revealProjectInFinder:)
                || action == sel!(gitRefresh:)
                || action == sel!(gitCommit:)
            {
                state.tree.root().is_some()
            } else if action == sel!(sendRequest:) {
                state.http.is_none() && can_send_from(&state)
            } else if action == sel!(closePane:)
                || action == sel!(focusNextPane:)
                || action == sel!(focusPreviousPane:)
            {
                pane_count(&state) > 1
            } else if action == sel!(triggerCompletion:) {
                !field_has_keys(&state)
            } else if action == sel!(goToDefinition:) || action == sel!(showHover:) {
                !field_has_keys(&state) && lsp_server_for(&state, state.docs.active()).is_some()
            } else {
                true
            };
            Bool::new(enabled)
        }
    }

    unsafe impl NSObjectProtocol for EditorView {}

    // Text input. With this the view is a text field as far as macOS is
    // concerned, and everything that types into text fields works: dead
    // keys, the press-and-hold accent menu, input methods for Chinese,
    // Japanese and Korean, the emoji picker, dictation, text replacements.
    //
    // Ranges are UTF-16 units counted from the start of the caret's line.
    // See `Buffer::input_selection` for why not from the start of the file.
    //
    // Every method borrows with `try_borrow`. AppKit calls these whenever it
    // likes, and the honest answer when the state is busy is "nothing".
    unsafe impl NSTextInputClient for EditorView {
        #[unsafe(method(insertText:replacementRange:))]
        fn insert_text(&self, string: &AnyObject, replacement: NSRange) {
            let text = text_of(string);
            if let Ok(mut state) = self.ivars().state.try_borrow_mut() {
                state.marked = None;
            }
            self.commit_text(&text, replacement);
            self.show_input_change();
        }

        /// Key bindings the input system resolved to a command rather than
        /// to text, such as `insertNewline:`. Every one that matters here was
        /// already handled by key code before the event got this far.
        #[unsafe(method(doCommandBySelector:))]
        fn do_command(&self, _selector: Sel) {}

        #[unsafe(method(setMarkedText:selectedRange:replacementRange:))]
        fn set_marked_text(&self, string: &AnyObject, _selected: NSRange, _replacement: NSRange) {
            let text = text_of(string);
            if let Ok(mut state) = self.ivars().state.try_borrow_mut() {
                state.marked = (!text.is_empty()).then_some(text);
            }
            self.request_redraw();
            self.show_input_change();
        }

        /// Commit whatever is being composed, as it stands.
        #[unsafe(method(unmarkText))]
        fn unmark_text(&self) {
            let marked = match self.ivars().state.try_borrow_mut() {
                Ok(mut state) => state.marked.take(),
                Err(_) => None,
            };
            if let Some(text) = marked {
                let nowhere = NSRange::new(NSNotFound as usize, 0);
                self.commit_text(&text, nowhere);
                self.show_input_change();
            }
        }

        #[unsafe(method(selectedRange))]
        fn selected_range(&self) -> NSRange {
            match self.ivars().state.try_borrow() {
                Ok(state) => {
                    let (location, length) = focused_buffer(&state).input_selection();
                    NSRange::new(location, length)
                }
                Err(_) => NSRange::new(NSNotFound as usize, 0),
            }
        }

        #[unsafe(method(markedRange))]
        fn marked_range(&self) -> NSRange {
            let Ok(state) = self.ivars().state.try_borrow() else {
                return NSRange::new(NSNotFound as usize, 0);
            };
            match &state.marked {
                Some(text) => {
                    let (location, _) = focused_buffer(&state).input_selection();
                    NSRange::new(location, text.encode_utf16().count())
                }
                None => NSRange::new(NSNotFound as usize, 0),
            }
        }

        #[unsafe(method(hasMarkedText))]
        fn has_marked_text(&self) -> bool {
            self.ivars()
                .state
                .try_borrow()
                .is_ok_and(|state| state.marked.is_some())
        }

        /// The text around the caret, for input methods that look at context.
        /// Declined: none of the ones this was written for need it, and the
        /// ranges it would be asked for are in a coordinate space the rope
        /// cannot answer cheaply.
        #[unsafe(method_id(attributedSubstringForProposedRange:actualRange:))]
        fn attributed_substring(
            &self,
            _range: NSRange,
            _actual: NSRangePointer,
        ) -> Option<Retained<NSAttributedString>> {
            None
        }

        /// No styling is taken from the input method: marked text is drawn
        /// one way, underlined.
        #[unsafe(method_id(validAttributesForMarkedText))]
        fn valid_attributes(&self) -> Retained<NSArray<NSAttributedStringKey>> {
            NSArray::new()
        }

        /// Where on the screen the caret is, so the accent menu and the
        /// candidate window open beside what is being typed.
        #[unsafe(method(firstRectForCharacterRange:actualRange:))]
        fn first_rect(&self, _range: NSRange, _actual: NSRangePointer) -> NSRect {
            let in_view = match self.ivars().state.try_borrow() {
                Ok(state) => {
                    let text = chrome_of(&state).text;
                    let caret =
                        layout::caret_rect(state.docs.active(), &state.renderer.atlas, text)
                            .unwrap_or(Viewport { width: 0.0, height: 0.0, ..text });
                    NSRect::new(
                        NSPoint::new(caret.x as f64, caret.y as f64),
                        NSSize::new(caret.width as f64, caret.height as f64),
                    )
                }
                Err(_) => NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
            };
            let in_window = self.convertRect_toView(in_view, None);
            match self.window() {
                Some(window) => window.convertRectToScreen(in_window),
                None => in_window,
            }
        }

        #[unsafe(method(characterIndexForPoint:))]
        fn character_index(&self, _point: NSPoint) -> NSUInteger {
            NSNotFound as NSUInteger
        }
    }

    unsafe impl NSWindowDelegate for EditorView {
        /// The last line of defence against closing a window with unsaved
        /// changes.
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _window: &NSWindow) -> bool {
            self.confirm_discard_all()
        }

        /// Back from another app: anything it wrote to an open file shows
        /// now, not at the next save. Files outside the project have no
        /// watcher, so this is their only trigger.
        #[unsafe(method(windowDidBecomeKey:))]
        fn window_did_become_key(&self, _notification: &NSNotification) {
            self.check_open_files();
            if let Ok(mut state) = self.ivars().state.try_borrow_mut() {
                state.caret_since = Instant::now();
            }
            self.request_redraw();
            self.pump();
        }

        /// Blinking stops with the keyboard gone, and the caret must not be
        /// left in its off half.
        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _notification: &NSNotification) {
            self.request_redraw();
            self.pump();
        }

        /// A blink toggle, armed by the frame before it.
        #[unsafe(method(caretBlink:))]
        fn caret_blink(&self, _sender: Option<&AnyObject>) {
            self.request_redraw();
            self.pump();
        }
    }
);

/// A fetched HEAD text arriving from a worker: the buffer it is for, and
/// the text, or none when the file is not in a repository.
type HeadText = (u64, Option<String>);

/// What the gutter knows about one open file.
struct GutterState {
    /// The file as of HEAD, with a file HEAD lacks as the empty string.
    /// `None` means the file is not in a repository, so there are no marks.
    head: Option<String>,
    marks: Vec<crate::project::git::Mark>,
}

/// Files past this size get no gutter marks: the diff reads the whole
/// text after every pause in typing.
const GUTTER_MAX_BYTES: usize = 2 * 1024 * 1024;

/// How a question about a file changed behind the editor was answered.
enum Conflict {
    /// Write the buffer over what is on disk.
    Overwrite,
    /// Take what is on disk and drop the buffer's changes.
    Reload,
    Cancel,
}

/// How a question about unsaved changes was answered.
enum Discard {
    /// Nothing to lose: it was clean, or it has just been saved.
    Saved,
    /// Don't Save.
    Dropped,
    /// Cancel, or a save that did not reach disk.
    Cancel,
}

impl EditorView {
    fn new(mtm: MainThreadMarker, state: State, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars {
            testing: std::env::var_os("CRC_SELFTEST").is_some(),
            state: RefCell::new(state),
            needs_redraw: Cell::new(false),
            in_key_down: Cell::new(false),
            handling_key: Cell::new(false),
            draws_during_key_handler: Cell::new(0),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };

        // Layer-hosting: hand AppKit our CAMetalLayer rather than letting it
        // make one. Order matters, setLayer must come before setWantsLayer.
        let layer = this.ivars().state.borrow().layer.clone();
        let base: &CALayer = &layer;
        this.setLayer(Some(base));
        this.setWantsLayer(true);
        this.resize(frame.size);
        this
    }

    /// Scrolls whatever is under `x, y`: the tab strip, the sidebar, the
    /// Markdown blocks or the text. `precise` is a trackpad, whose deltas are
    /// points; otherwise they are wheel notches.
    fn scroll_at(&self, x: f32, y: f32, dx: f64, dy: f64, precise: bool) {
        if dy.abs() < 0.01 && dx.abs() < 0.01 {
            return;
        }
        // Scrolling another pane scrolls it, which means focusing it:
        // there is one scroll position per focused document here.
        let over_pane = {
            let mut state = self.ivars().state.borrow_mut();
            match frame_of(&mut state).hit(x, y) {
                Some(Hit::Pane(index)) => Some(*index),
                _ => None,
            }
        };
        if let Some(index) = over_pane {
            self.focus_pane(index);
        }
        let chrome = self.chrome();
        let (rows, _) = self.grid();

        let mut state = self.ivars().state.borrow_mut();
        let m = state.renderer.atlas.metrics;
        if chrome.tabs.contains(x, y) {
            let delta = if dx.abs() > dy.abs() { dx } else { dy };
            state.tab_scroll_carry -= if precise {
                delta / 40.0
            } else {
                delta.signum()
            };
            let tabs = state.tab_scroll_carry.trunc() as isize;
            state.tab_scroll_carry -= tabs as f64;
            if tabs != 0 {
                state.tab_scroll = state
                    .tab_scroll
                    .saturating_add_signed(tabs)
                    .min(state.docs.len().saturating_sub(1));
                drop(state);
                self.request_redraw();
                self.pump();
            }
            return;
        }

        // A trackpad reports points, a wheel reports notches. Either way
        // the part that does not make a whole line is kept for the next
        // event rather than rounded away: rounding each event alone meant
        // a slow two-finger scroll, whose deltas are all under half a
        // line, never moved at all, and the tail of every flick was lost.
        let (per_line, per_column) = if precise {
            (m.line_height as f64, m.advance as f64)
        } else {
            (1.0 / WHEEL_LINES_PER_NOTCH, 1.0 / WHEEL_LINES_PER_NOTCH)
        };
        // A trackpad moves the text itself by points. Wheel notches, the
        // sidebar and the Markdown blocks still go a whole line at a time.
        let over_sidebar = chrome.sidebar.is_some_and(|r| r.contains(x, y));
        let previewing = state.preview.is_some() && default_preview(state.docs.active()).is_some();
        if precise && !over_sidebar && !previewing {
            state.scroll_carry.1 = 0.0;
            state
                .docs
                .active_mut()
                .scroll_smooth_by((-dy / m.line_height as f64) as f32, rows);
            state.scroll_carry.0 -= dx / per_column;
            let columns = state.scroll_carry.0.trunc();
            state.scroll_carry.0 -= columns;
            state
                .docs
                .active_mut()
                .scroll_columns_by(columns as isize, rows);
            drop(state);
            self.request_redraw();
            self.pump();
            return;
        }
        state.scroll_carry.0 -= dx / per_column;
        state.scroll_carry.1 -= dy / per_line;
        let (columns, lines) = (state.scroll_carry.0.trunc(), state.scroll_carry.1.trunc());
        state.scroll_carry.0 -= columns;
        state.scroll_carry.1 -= lines;
        let (columns, lines) = (columns as isize, lines as isize);
        if columns == 0 && lines == 0 {
            return;
        }

        if debug_scroll() {
            eprintln!(
                "scroll dy={dy:.1} precise={} | lines={lines} rows={rows} | scroll_line={} of {}",
                precise,
                state.docs.active().scroll_line,
                state.docs.active().rope.len_lines(),
            );
        }

        if state.preview.is_some()
            && default_preview(state.docs.active()).is_some()
            && !chrome.sidebar.is_some_and(|r| r.contains(x, y))
        {
            let blocks = markdown::parse(&state.docs.active().rope.to_string());
            let last = blocks.len().saturating_sub(1);
            let current = state.preview.unwrap_or(0);
            state.preview = Some(current.saturating_add_signed(lines).min(last));
            drop(state);
            self.request_redraw();
            self.pump();
            return;
        }

        match chrome.sidebar {
            Some(rect) if rect.contains(x, y) => {
                state.tree.scroll_by(lines, layout::sidebar_rows(rect));
            }
            _ => {
                let buffer = state.docs.active_mut();
                buffer.scroll_by(lines, rows);
                buffer.scroll_columns_by(columns, rows);
            }
        }
        drop(state);
        self.request_redraw();
        self.pump();
    }

    /// Starts an update check unless one is already out.
    fn start_update_check(&self, manual: bool) {
        {
            let mut state = self.ivars().state.borrow_mut();
            match &mut state.update {
                // Asking while a launch check is out makes it an asked one.
                Some((asked, _)) => *asked |= manual,
                None => state.update = Some((manual, crate::platform::update::spawn())),
            }
        }
        self.resume_display_link();
    }

    /// Takes a finished update check's answer. A launch check speaks only
    /// when there is something new; an asked one always answers, and opens
    /// the new release's page.
    fn poll_update(&self) {
        let (manual, outcome) = {
            let Ok(mut state) = self.ivars().state.try_borrow_mut() else {
                return;
            };
            let Some((manual, rx)) = &state.update else {
                return;
            };
            let manual = *manual;
            let outcome = match rx.try_recv() {
                Ok(outcome) => outcome,
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => Err("the check stopped".to_owned()),
            };
            state.update = None;
            (manual, outcome)
        };
        let current = env!("CARGO_PKG_VERSION");
        let note = match (outcome, manual) {
            (Ok(Some(release)), false) => Some(format!(
                "crc {} is out: Help > Check for Updates opens its page",
                release.version
            )),
            (Ok(Some(release)), true) => Some(if self.ivars().testing {
                format!("crc {} is out; would open {}", release.version, release.url)
            } else {
                let _ = std::process::Command::new("/usr/bin/open")
                    .arg(&release.url)
                    .spawn();
                format!(
                    "crc {} is out; its release page is open in your browser",
                    release.version
                )
            }),
            (Ok(None), true) => Some(format!("crc {current} is the latest release")),
            (Err(e), true) => Some(format!("could not check for updates: {e}")),
            (_, false) => None,
        };
        if let Some(note) = note {
            self.ivars().state.borrow_mut().message = Some((note, Instant::now()));
            self.request_redraw();
            self.pump();
        }
    }

    /// Whether the caret is blinking now: the setting allows it, the window
    /// has the keyboard, and this is not a test instance, whose captures
    /// must not depend on when they were taken.
    fn caret_blinks(&self, state: &State) -> bool {
        state.caret_blink
            && !self.ivars().testing
            && self.window().is_some_and(|window| window.isKeyWindow())
    }

    /// Schedules one redraw at the next blink toggle, replacing any already
    /// scheduled. A timer rather than the display link, which would draw
    /// every refresh to change the screen twice a second.
    fn arm_caret_blink(&self) {
        let cancel = || unsafe {
            let _: () = msg_send![
                objc2::class!(NSObject),
                cancelPreviousPerformRequestsWithTarget: self,
                selector: sel!(caretBlink:),
                object: None::<&AnyObject>
            ];
        };
        let Ok(state) = self.ivars().state.try_borrow() else {
            return;
        };
        if !self.caret_blinks(&state) {
            drop(state);
            cancel();
            return;
        }
        let wait = caret_phase(state.caret_since.elapsed()).1;
        drop(state);
        cancel();
        let _: () = unsafe {
            msg_send![self, performSelector: sel!(caretBlink:),
                withObject: None::<&AnyObject>, afterDelay: wait.as_secs_f64()]
        };
    }

    /// Marks the view as needing a redraw, with no latency implications.
    ///
    /// Most redraws are not responses to a keypress: a resize, a folder
    /// opening, a modal panel closing. Timing those as "input latency" is
    /// how a save dialog left open for three seconds ends up reported as a
    /// three-second keystroke.
    fn request_redraw(&self) {
        if let Ok(mut state) = self.ivars().state.try_borrow_mut() {
            state.cursor_rects_for = None;
        }
        self.ivars().needs_redraw.set(true);
    }

    /// Records that a human pressed a key or clicked at `at`.
    ///
    /// Only the *oldest* unpresented input timestamp is kept: if three
    /// keystrokes land inside one refresh, the latency that matters is how
    /// long the first one waited to appear.
    fn note_input(&self, at: Instant) {
        self.ivars().needs_redraw.set(true);
        let Ok(mut state) = self.ivars().state.try_borrow_mut() else {
            return;
        };
        state.caret_since = at;
        if state.pending_input.is_none() {
            state.pending_input = Some(at);
        }
    }

    /// Draws now if the display is ready for another frame, otherwise arms
    /// the display link to draw at the next refresh.
    ///
    /// This is the whole frame-pacing policy. Drawing straight from the event
    /// handler is the lowest-latency thing to do and is what happens for
    /// ordinary typing, which is far slower than the refresh rate. But
    /// `nextDrawable` blocks when the layer's drawable pool is empty, so
    /// input that outruns the display (key repeat, a trackpad flick, a
    /// resize storm) would stall the main thread inside AppKit's event
    /// dispatch. Deferring those to the next refresh coalesces them into one
    /// frame instead.
    fn pump(&self) {
        if self.ivars().handling_key.get() {
            self.request_redraw();
            return;
        }
        self.sync_native_preview();
        let ready = match self.ivars().state.try_borrow() {
            Ok(state) => match state.last_draw {
                Some(t) => t.elapsed() >= state.frame_interval,
                None => true,
            },
            // Busy: leave it to the display link.
            Err(_) => false,
        };

        if ready {
            self.draw_now();
        } else {
            self.resume_display_link();
        }
    }

    fn sync_native_preview(&self) {
        let Ok(mut state) = self.ivars().state.try_borrow_mut() else {
            return;
        };
        let path = state
            .docs
            .active()
            .is_preview_file()
            .then(|| state.docs.active().path.clone())
            .flatten();
        let text = chrome_of(&state).text;
        let frame = NSRect::new(
            NSPoint::new(text.x as f64, text.y as f64),
            NSSize::new(text.width as f64, text.height as f64),
        );
        if let Some(current) = &state.native_preview
            && path.as_deref() == Some(current.path.as_path())
        {
            let view = current.view.clone();
            drop(state);
            unsafe {
                let _: () = msg_send![&*view, setFrame: frame];
            }
            return;
        }
        let previous = state.native_preview.take();
        drop(state);
        if let Some(previous) = previous {
            unsafe {
                let _: () = msg_send![&*previous.view, close];
                let _: () = msg_send![&*previous.view, removeFromSuperview];
            }
        }
        if let Some(path) = path
            && let Some(preview) = NativePreview::new(&path, frame)
        {
            unsafe {
                let _: () = msg_send![self, addSubview: &*preview.view];
            }
            self.ivars().state.borrow_mut().native_preview = Some(preview);
        }
    }

    /// Renders, then records how long the oldest pending input waited.
    fn draw_now(&self) {
        let render_started = Instant::now();
        if self.ivars().handling_key.get() {
            self.ivars()
                .draws_during_key_handler
                .set(self.ivars().draws_during_key_handler.get() + 1);
        }
        let size = self.frame().size;
        if size.width <= 0.0 || size.height <= 0.0 {
            // Nothing to draw into, and retrying every refresh until there is
            // would burn battery for a window nobody can see. The resize
            // that gives it a size asks for a redraw itself.
            self.ivars().needs_redraw.set(false);
            return;
        }
        let Some(timing) = self.render() else {
            // No drawable, or the state was busy: nothing reached the screen.
            // The request stands, whatever it was for, and the display link
            // retries it at the next refresh. Any pending input keeps its
            // timestamp, so the frame that does appear is timed honestly.
            self.ivars().needs_redraw.set(true);
            self.resume_display_link();
            return;
        };
        self.ivars().needs_redraw.set(false);
        self.arm_caret_blink();

        let Ok(mut state) = self.ivars().state.try_borrow_mut() else {
            return;
        };
        if state.renderer.atlas.has_pending_shaping() {
            self.ivars().needs_redraw.set(true);
            if let Some(link) = &state.display_link {
                link.setPaused(false);
            }
        }
        state.last_draw = Some(Instant::now());

        // Cursor rects are AppKit's copy of the layout, and it only asks for
        // them again when told to. It was never told, so after dragging the
        // divider the resize cursor stayed where the divider used to be.
        let chrome = chrome_of(&state);
        let key = (
            chrome.text.x,
            chrome.text.y,
            chrome.text.width,
            chrome.text.height,
            chrome.sidebar.map(|r| r.width),
        );
        if state.cursor_rects_for != Some(key) {
            state.cursor_rects_for = Some(key);
            if let Some(window) = self.window() {
                // Queued, not called: AppKit rebuilds them on its next pass
                // through the run loop, after this borrow is long gone.
                window.invalidateCursorRectsForView(self);
            }
        }

        if let Some(input_at) = state.pending_input.take() {
            let total = input_at.elapsed();
            state.latency.record(total);
            if state.worst.is_none_or(|(w, _)| total > w) {
                state.worst = Some((total, timing));
            }
            // The pre-render wait includes input handling and frame pacing;
            // Metal's phases alone cannot explain those delays.
            if total > Duration::from_micros(8333) {
                eprintln!(
                    "slow frame #{}: {:.2}ms total, {:.2}ms until render = acquire {:.2} + encode {:.2} + commit {:.2}  ({} quads)",
                    state.latency.count(),
                    total.as_secs_f64() * 1e3,
                    render_started
                        .saturating_duration_since(input_at)
                        .as_secs_f64()
                        * 1e3,
                    timing.acquire.as_secs_f64() * 1e3,
                    timing.encode.as_secs_f64() * 1e3,
                    timing.commit.as_secs_f64() * 1e3,
                    state.glyphs.len(),
                );
            }
        }
        // AppKit's setTitle can take several milliseconds. Let the content
        // frame reach Metal before asking AppKit to update the window chrome.
        let sync_title = std::mem::take(&mut state.title_sync_pending);
        drop(state);
        if sync_title {
            self.sync_title();
        }
        self.claude_after_frame();
        self.terminal_after_frame();
    }

    /// Creates the display link on first use and unpauses it.
    ///
    /// Created lazily because `displayLinkWithTarget:selector:` binds to the
    /// display the view is currently on, which is only known once the view is
    /// in a window.
    fn resume_display_link(&self) {
        let Ok(existing) = self
            .ivars()
            .state
            .try_borrow()
            .map(|state| state.display_link.clone())
        else {
            return;
        };
        if let Some(link) = existing {
            link.setPaused(false);
            return;
        }

        // CADisplayLink retains its target, so this view and its link hold
        // each other. The view lives for the process lifetime, so that is a
        // cycle by design rather than a leak; it would need breaking if
        // views ever became closable.
        let link =
            unsafe { self.displayLinkWithTarget_selector(self, objc2::sel!(onDisplayLink:)) };
        // Common modes, not the default mode. During a trackpad scroll or a
        // live window resize AppKit switches the run loop into event-tracking
        // mode, and a link registered only on the default mode goes silent
        // for exactly the gestures that most need paced frames.
        unsafe {
            link.addToRunLoop_forMode(&NSRunLoop::currentRunLoop(), NSRunLoopCommonModes);
        }
        link.setPaused(false);
        // Not busy: the borrow above succeeded and nothing since re-enters.
        self.ivars().state.borrow_mut().display_link = Some(link);
    }

    /// Applies a key event. Returns whether anything changed.
    fn handle_key(&self, event: &NSEvent) -> bool {
        // Ctrl-` by its key, not its character: on layouts where that key
        // types something else the menu's shortcut never matches.
        const GRAVE: u16 = 50;
        let flags = event.modifierFlags();
        if event.keyCode() == GRAVE
            && flags.contains(NSEventModifierFlags::Control)
            && !flags.contains(NSEventModifierFlags::Command)
        {
            let _: () = unsafe { msg_send![self, toggleTerminal: None::<&AnyObject>] };
            return true;
        }
        if self.terminal_has_keys() {
            return self.terminal_key(event);
        }
        // Typing goes to the document, so the tree no longer has the keys.
        if !event
            .modifierFlags()
            .contains(NSEventModifierFlags::Command)
        {
            self.ivars().state.borrow_mut().sidebar_keys = false;
        }
        let reviewing = {
            let state = self.ivars().state.borrow();
            active_review(&state).is_some() && !field_has_keys(&state)
        };
        if reviewing && let Some(handled) = self.handle_review_key(event) {
            return handled;
        }
        // Escape with no overlay open drops back to a single cursor, which
        // is the only way out of a multi-cursor edit.
        if event
            .modifierFlags()
            .contains(NSEventModifierFlags::Command | NSEventModifierFlags::Option)
            && event.keyCode() == 5
        {
            self.open_git();
            return true;
        }
        // Backspace with Command or Option in a field edits the field: to
        // the start of it, or one word. It never reaches the document behind
        // the field, and it never reaches the tree, where Cmd-Delete is Move
        // to Trash. The menu item is greyed out while a field has the keys,
        // which is what lets the key arrive here at all.
        {
            let flags = event.modifierFlags();
            let command = flags.contains(NSEventModifierFlags::Command);
            let option = flags.contains(NSEventModifierFlags::Option);
            if event.keyCode() == key::DELETE
                && (command || option)
                && !field_has_keys(&self.ivars().state.borrow())
            {
                // The document's own handling below.
            } else if event.keyCode() == key::DELETE && (command || option) {
                let ((), focus) = self.edit_focused(|b, _| {
                    if command {
                        b.delete_to_line_start();
                    } else {
                        b.delete_word_backward();
                    }
                });
                self.after_focused_edit(focus);
                self.pump();
                return true;
            }
        }
        if self.ivars().state.borrow().completion.is_some() && self.handle_completion_key(event) {
            return true;
        }
        {
            let flags = event.modifierFlags();
            let plain = !flags.contains(NSEventModifierFlags::Command)
                && !flags.contains(NSEventModifierFlags::Option);
            let overlay = field_has_keys(&self.ivars().state.borrow());
            let shift = flags.contains(NSEventModifierFlags::Shift);
            match event.keyCode() {
                // The menu has these too; handled here as well because a
                // key equivalent without Command is not always offered to
                // the menu first, and Shift-F12 must not fall to F12.
                key::F12 if plain && shift && !overlay => {
                    self.find_references();
                    return true;
                }
                key::F12 if plain && !overlay => {
                    self.goto_definition(None);
                    return true;
                }
                key::F2 if plain && !shift && !overlay => {
                    self.start_rename();
                    return true;
                }
                key::F
                    if shift
                        && flags.contains(NSEventModifierFlags::Option)
                        && !flags.contains(NSEventModifierFlags::Command)
                        && !overlay =>
                {
                    self.format_document(false);
                    return true;
                }
                key::F1 if plain && !overlay => {
                    self.show_hover();
                    return true;
                }
                key::SPACE if flags.contains(NSEventModifierFlags::Control) && !overlay => {
                    self.request_completion(true);
                    return true;
                }
                _ => {}
            }
        }
        if self.ivars().state.borrow().sidebar_edit.is_some() {
            return self.handle_sidebar_edit_key(event);
        }
        if self.ivars().state.borrow().git_open {
            return self.handle_git_key(event);
        }
        const ESCAPE_KEY: u16 = 53;
        if event.keyCode() == ESCAPE_KEY {
            let state = self.ivars().state.borrow();
            let idle = state.palette.is_none()
                && state.find.is_none()
                && state.goto.is_none()
                && state.rename.is_none();
            let tip = state.signature.is_some();
            drop(state);
            if idle && tip {
                self.ivars().state.borrow_mut().signature = None;
                self.request_redraw();
                return true;
            }
            if idle {
                return self
                    .ivars()
                    .state
                    .borrow_mut()
                    .docs
                    .active_mut()
                    .collapse_cursors();
            }
        }

        // The palette is modal over everything, then the find bar. Only the
        // keys that mean something to each escape it.
        if self.ivars().state.borrow().rename.is_some() && self.handle_rename_key(event) {
            return true;
        }
        if self.ivars().state.borrow().goto.is_some() && self.handle_goto_key(event) {
            return true;
        }
        if self.ivars().state.borrow().palette.is_some() && self.handle_palette_key(event) {
            return true;
        }
        if self.ivars().state.borrow().find.is_some() && self.handle_find_key(event) {
            return true;
        }
        if self.ivars().state.borrow().docs.active().is_preview_file() {
            return false;
        }
        // Cmd-Return in a request file. A real keypress reaches this through
        // the Run menu's key equivalent; a scripted one arrives here.
        if event.keyCode() == key::RETURN
            && event
                .modifierFlags()
                .contains(NSEventModifierFlags::Command)
            && can_send_from(&self.ivars().state.borrow())
        {
            self.send_request();
            return true;
        }

        let flags = event.modifierFlags();
        let command = flags.contains(NSEventModifierFlags::Command);
        let option = flags.contains(NSEventModifierFlags::Option);
        let control = flags.contains(NSEventModifierFlags::Control);
        let shift = flags.contains(NSEventModifierFlags::Shift);

        // Shift turns every motion into a selection extension. This is the
        // only place that mapping happens, so the buffer never has to know
        // what a modifier key is.
        let motion = if shift { Motion::Extend } else { Motion::Move };
        let code = match event.keyCode() {
            key::KEYPAD_ENTER => key::RETURN,
            code => code,
        };

        {
            let mut state = self.ivars().state.borrow_mut();
            let m = state.renderer.atlas.metrics;
            let gutter = layout::gutter_width(state.docs.active(), &state.renderer.atlas);
            // The text area's rows and columns, not the window's. Measured
            // against the whole window, the caret went three or four rows
            // under the last visible line before the view followed it.
            let text = chrome_of(&state).text;
            let rows = text.rows(m.line_height);
            let cols = text.columns(m.advance, gutter);
            let b = &mut state.docs.active_mut();

            // Navigation and deletion keys. Unlike letters, these take their
            // meaning from the modifiers, following the macOS conventions:
            // Option is word-wise, Command is line- or document-wise.
            let handled = match code {
                key::LEFT => {
                    if command {
                        b.move_line_start(motion)
                    } else if option {
                        b.move_word_left(motion)
                    } else {
                        b.move_left(motion)
                    }
                    true
                }
                key::RIGHT => {
                    if command {
                        b.move_line_end(motion)
                    } else if option {
                        b.move_word_right(motion)
                    } else {
                        b.move_right(motion)
                    }
                    true
                }
                key::UP => {
                    if command {
                        b.move_buffer_start(motion)
                    } else {
                        b.move_up(motion)
                    }
                    true
                }
                key::DOWN => {
                    if command {
                        b.move_buffer_end(motion)
                    } else {
                        b.move_down(motion)
                    }
                    true
                }
                key::DELETE => {
                    if command {
                        b.delete_to_line_start()
                    } else if option {
                        b.delete_word_backward()
                    } else {
                        b.backspace_paired()
                    }
                    true
                }
                key::FORWARD_DELETE => {
                    if command {
                        b.delete_to_line_end()
                    } else if option {
                        b.delete_word_forward()
                    } else {
                        b.delete_forward()
                    }
                    true
                }
                key::HOME => {
                    b.move_line_start(motion);
                    true
                }
                key::END => {
                    b.move_line_end(motion);
                    true
                }
                key::PAGE_UP => {
                    for _ in 0..rows.max(1) {
                        b.move_up(motion);
                    }
                    true
                }
                key::PAGE_DOWN => {
                    for _ in 0..rows.max(1) {
                        b.move_down(motion);
                    }
                    true
                }
                key::RETURN if !command => {
                    b.insert_newline_indented();
                    true
                }
                key::TAB if !command => {
                    // Tab indents a selection or a block; it only inserts a
                    // character when there is nothing to indent.
                    if b.selection().is_some() {
                        if shift { b.outdent() } else { b.indent() }
                    } else if shift {
                        b.outdent()
                    } else {
                        b.insert_tab()
                    }
                    true
                }
                _ => false,
            };

            if handled {
                b.scroll_to_cursor(rows, cols);
                return true;
            }

            // The Emacs-style Control bindings that every macOS text view
            // supports. Leaving these out is immediately noticeable to
            // anyone used to the platform.
            if control && !command {
                let base = event
                    .charactersIgnoringModifiers()
                    .and_then(|c| c.to_string().chars().next())
                    .map(|c| c.to_ascii_lowercase());
                let handled = match base {
                    Some('a') => {
                        b.move_line_start(motion);
                        true
                    }
                    Some('e') => {
                        b.move_line_end(motion);
                        true
                    }
                    Some('b') => {
                        b.move_left(motion);
                        true
                    }
                    Some('f') => {
                        b.move_right(motion);
                        true
                    }
                    Some('p') => {
                        b.move_up(motion);
                        true
                    }
                    Some('n') => {
                        b.move_down(motion);
                        true
                    }
                    Some('d') => {
                        b.delete_forward();
                        true
                    }
                    Some('k') => {
                        b.delete_to_line_end();
                        true
                    }
                    Some('h') => {
                        b.backspace();
                        true
                    }
                    _ => false,
                };
                if handled {
                    b.scroll_to_cursor(rows, cols);
                    return true;
                }
                return false;
            }
        }

        if command {
            // Cmd-1..9 jumps straight to a tab. Not menu items, because nine
            // of them would bury the rest of the Window menu.
            if let Some(digit) = event
                .charactersIgnoringModifiers()
                .and_then(|c| c.to_string().chars().next())
                .and_then(|c| c.to_digit(10))
                .filter(|d| (1..=9).contains(d))
            {
                let switched = {
                    let mut state = self.ivars().state.borrow_mut();
                    let switched = state.docs.switch(digit as usize - 1);
                    if switched {
                        state.preview = default_preview(state.docs.active());
                        reveal_active_tab(&mut state);
                    }
                    switched
                };
                if switched {
                    self.sync_title();
                    self.reparse();
                }
                return switched;
            }
            // Every other Command shortcut is a menu item, and AppKit matches
            // those before the event reaches here.
            return false;
        }

        // Anything else is text, and text is the input system's to work out.
        // Reading `characters` off the event gets the key, not what the key
        // means: a dead key has no characters at all, so on a Portuguese or
        // Spanish layout the tilde, the circumflex and the backtick could not
        // be typed, and no input method could work.
        self.interpret(event)
    }

    /// Performs one self-test step as a real event through the window.
    fn play(&self, step: &Step) {
        let mtm = MainThreadMarker::from(self);
        let Some(window) = self.window() else {
            return;
        };
        let mouse_with = |kind: NSEventType,
                          x: f64,
                          y: f64,
                          count: isize,
                          flags: NSEventModifierFlags| {
            let at = self.convertPoint_toView(NSPoint::new(x, y), None);
            NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                kind,
                at,
                flags,
                0.0,
                window.windowNumber(),
                None,
                0,
                count,
                1.0,
            )
        };
        let mouse = |kind: NSEventType, x: f64, y: f64, count: isize| {
            mouse_with(kind, x, y, count, NSEventModifierFlags::empty())
        };
        match step {
            Step::ClickIn {
                name,
                dx,
                dy,
                count,
                mods,
            } => {
                let target = {
                    let mut state = self.ivars().state.borrow_mut();
                    frame_of(&mut state).named(name)
                };
                let Some(rect) = target else {
                    eprintln!("selftest: no region named {name} in this frame");
                    return;
                };
                let (x, y) = (f64::from(rect.x) + dx, f64::from(rect.y) + dy);
                let mut flags = NSEventModifierFlags::empty();
                for (on, flag) in [
                    (mods.command, NSEventModifierFlags::Command),
                    (mods.shift, NSEventModifierFlags::Shift),
                    (mods.option, NSEventModifierFlags::Option),
                    (mods.control, NSEventModifierFlags::Control),
                ] {
                    if on {
                        flags |= flag;
                    }
                }
                if let Some(event) = mouse_with(NSEventType::LeftMouseDown, x, y, *count, flags) {
                    let _: () = unsafe { msg_send![self, mouseDown: &*event] };
                }
                if let Some(event) = mouse_with(NSEventType::LeftMouseUp, x, y, *count, flags) {
                    let _: () = unsafe { msg_send![self, mouseUp: &*event] };
                }
            }
            Step::Key { code, mods, chars } => {
                let mut flags = NSEventModifierFlags::empty();
                for (on, flag) in [
                    (mods.command, NSEventModifierFlags::Command),
                    (mods.shift, NSEventModifierFlags::Shift),
                    (mods.option, NSEventModifierFlags::Option),
                    (mods.control, NSEventModifierFlags::Control),
                ] {
                    if on {
                        flags |= flag;
                    }
                }
                let characters = NSString::from_str(chars);
                let plain = NSString::from_str(&chars.to_lowercase());
                let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                    NSEventType::KeyDown,
                    NSPoint::new(0.0, 0.0),
                    flags,
                    0.0,
                    window.windowNumber(),
                    None,
                    &characters,
                    &plain,
                    false,
                    *code,
                );
                let Some(event) = event else {
                    return;
                };
                // What NSApplication does with a key: the menu bar gets the
                // first look, for its key equivalents, then the window.
                //
                // The item is found and its action sent here, rather than
                // through performKeyEquivalent:, which delivers to the key
                // window's responder chain. A test instance launched behind
                // whatever the person is using is often not key, and then the
                // shortcut went nowhere and every click after it was aimed at
                // a layout that had not changed.
                let app = NSApplication::sharedApplication(mtm);
                let action = app
                    .mainMenu()
                    .filter(|_| mods.command)
                    .and_then(|menu| menu_action_for(&menu, chars, flags));
                match action {
                    Some(action) => {
                        let target: &AnyObject = self;
                        unsafe { app.sendAction_to_from(action, Some(target), None) };
                    }
                    None => window.sendEvent(&event),
                }
            }
            // Mouse events go to the view's own handlers, not through the
            // window. A window in an app that is not frontmost keeps a single
            // click for itself, to come forward with, and hands on only the
            // double and triple ones, so half a script would vanish depending
            // on what else was on screen. The handlers are what is under
            // test; which window is in front is not.
            Step::Click { x, y, count } => {
                let started = Instant::now();
                if let Some(event) = mouse(NSEventType::LeftMouseDown, *x, *y, *count) {
                    let _: () = unsafe { msg_send![self, mouseDown: &*event] };
                }
                if let Some(event) = mouse(NSEventType::LeftMouseUp, *x, *y, *count) {
                    let _: () = unsafe { msg_send![self, mouseUp: &*event] };
                }
                self.ivars().state.borrow_mut().last_selftest_click_ms =
                    started.elapsed().as_secs_f64() * 1000.0;
            }
            Step::ClickNamed { name, count } => {
                let target = {
                    let mut state = self.ivars().state.borrow_mut();
                    frame_of(&mut state).named(name)
                };
                let Some(rect) = target else {
                    eprintln!("selftest: no region named {name} in this frame");
                    return;
                };
                let (x, y) = (
                    f64::from(rect.x + rect.width / 2.0),
                    f64::from(rect.y + rect.height / 2.0),
                );
                if let Some(event) = mouse(NSEventType::LeftMouseDown, x, y, *count) {
                    let _: () = unsafe { msg_send![self, mouseDown: &*event] };
                }
                if let Some(event) = mouse(NSEventType::LeftMouseUp, x, y, *count) {
                    let _: () = unsafe { msg_send![self, mouseUp: &*event] };
                }
            }
            Step::DownNamed { name } => {
                let target = {
                    let mut state = self.ivars().state.borrow_mut();
                    frame_of(&mut state).named(name)
                };
                let Some(rect) = target else {
                    eprintln!("selftest: no region named {name} in this frame");
                    return;
                };
                let (x, y) = (
                    f64::from(rect.x + rect.width / 2.0),
                    f64::from(rect.y + rect.height / 2.0),
                );
                self.ivars().state.borrow_mut().selftest_pointer = (x, y);
                if let Some(event) = mouse(NSEventType::LeftMouseDown, x, y, 1) {
                    let _: () = unsafe { msg_send![self, mouseDown: &*event] };
                }
            }
            Step::DragBy { dx, dy } => {
                let (x, y) = self.ivars().state.borrow().selftest_pointer;
                if let Some(event) = mouse(NSEventType::LeftMouseDragged, x + dx, y + dy, 1) {
                    let _: () = unsafe { msg_send![self, mouseDragged: &*event] };
                }
            }
            Step::UpBy { dx, dy } => {
                let (x, y) = self.ivars().state.borrow().selftest_pointer;
                if let Some(event) = mouse(NSEventType::LeftMouseUp, x + dx, y + dy, 1) {
                    let _: () = unsafe { msg_send![self, mouseUp: &*event] };
                }
            }
            Step::Down { x, y, count } => {
                self.ivars().state.borrow_mut().selftest_pointer = (*x, *y);
                if let Some(event) = mouse(NSEventType::LeftMouseDown, *x, *y, *count) {
                    let _: () = unsafe { msg_send![self, mouseDown: &*event] };
                }
            }
            Step::Drag { x, y } => {
                if let Some(event) = mouse(NSEventType::LeftMouseDragged, *x, *y, 1) {
                    let _: () = unsafe { msg_send![self, mouseDragged: &*event] };
                }
            }
            Step::Up { x, y } => {
                if let Some(event) = mouse(NSEventType::LeftMouseUp, *x, *y, 1) {
                    let _: () = unsafe { msg_send![self, mouseUp: &*event] };
                }
            }
            Step::Resize { width, height } => {
                window.setContentSize(NSSize::new(*width, *height));
                self.request_redraw();
                self.pump();
            }
            Step::Touch(path) => {
                if let Err(error) = std::fs::write(path, "personal sample\n") {
                    eprintln!("crc: selftest could not create {path}: {error}");
                }
            }
            Step::Write(path, text) => {
                let result = std::fs::write(path, format!("{text}\n")).and_then(|()| {
                    std::fs::File::options().write(true).open(path)?.set_times(
                        std::fs::FileTimes::new()
                            .set_modified(std::time::SystemTime::now() + Duration::from_secs(2)),
                    )
                });
                if let Err(error) = result {
                    eprintln!("crc: selftest could not write {path}: {error}");
                }
            }
            Step::Wheel(dy) => {
                if self.ivars().state.borrow().palette.is_some() {
                    self.palette_wheel_by(*dy, false);
                } else {
                    eprintln!("selftest: wheel only drives the palette");
                }
            }
            Step::Trackpad { x, y, dy } => {
                self.scroll_at(*x as f32, *y as f32, 0.0, *dy, true);
            }
            Step::Wait(ms) => {
                std::thread::sleep(Duration::from_millis(*ms));
                self.check_open_files();
                self.poll_tree_children();
                self.poll_project_index();
                self.poll_project_search();
                self.poll_http();
                self.poll_update();
                self.poll_ignored();
                self.poll_completion();
                self.poll_claude();
                self.ivars().state.borrow_mut().git.poll();
            }
            Step::Dump(path) => {
                let state = self.ivars().state.borrow();
                let titles: Vec<String> =
                    (0..state.docs.len()).map(|i| state.docs.title(i)).collect();
                // Where things are, so a script whose clicks miss can be told
                // from one whose clicks were ignored.
                let chrome = chrome_of(&state);
                let layout = format!(
                    "window {}x{} sidebar {:?} text {},{} {}x{} font {} theme {}",
                    state.viewport.width,
                    state.viewport.height,
                    chrome.sidebar.map(|r| r.width),
                    chrome.text.x,
                    chrome.text.y,
                    chrome.text.width,
                    chrome.text.height,
                    state.font_size,
                    if state.theme.is_dark() {
                        "dark"
                    } else {
                        "light"
                    },
                );
                let buffer = state.docs.active();
                let caret_shaped = state
                    .renderer
                    .atlas
                    .cached_editor_line(
                        (buffer.id(), buffer.rope.byte_to_line(buffer.cursor())),
                        &buffer.rope,
                    )
                    .is_some();
                let report = format!(
                    "git_open: {}\ngit_focus: {}\ngit_diff: {}\ngit_pending: {}\ngit_changes: {}\ngit_staged: {}\ngit_hunks_staged: {}\ngit_hunks_working: {}\ngit_message: {}\nproject_menu_requested: {}\nlast_click_ms: {:.3}\nfinder_entries: {}\nsidebar_edit: {}\npalette_query: {}\npalette_first: {}\npalette_scroll: {}\npanes: {}\nfocused_pane: {}\nlsp: {}\ndiagnostics: {}\ncompletion: {}\nkey_handler_draws: {}\nwindow_title: {}\nshaping_pending: {}\ncaret_shaped: {}\nclaude: {}\nterminal: {}\n{}",
                    state.git_open,
                    state.git_focus,
                    state.git.showing_diff,
                    state.git.busy(),
                    state.git.snapshot.as_ref().map_or(0, |s| s.changes.len()),
                    state.git.snapshot.as_ref().map_or(0, |s| s
                        .changes
                        .iter()
                        .filter(|c| c.staged())
                        .count()),
                    state.git.hunk_counts().0,
                    state.git.hunk_counts().1,
                    state.git.message.rope,
                    state.project_menu_requested,
                    state.last_selftest_click_ms,
                    state.finder.len(),
                    state
                        .sidebar_edit
                        .as_ref()
                        .map(|e| e.field.rope.to_string())
                        .unwrap_or_default(),
                    state
                        .palette
                        .as_ref()
                        .map(|(query, _)| query.rope.to_string())
                        .unwrap_or_default(),
                    state
                        .palette
                        .as_ref()
                        .and_then(|(query, _)| {
                            palette_rows(&palette_sources(&state), &query.rope.to_string())
                                .into_iter()
                                .next()
                        })
                        .map(|(row, _)| row.title)
                        .unwrap_or_default(),
                    state.palette_scroll,
                    pane_count(&state),
                    state.focused_pane,
                    state
                        .lsp
                        .values()
                        .map(|s| format!("{} {:?}", s.name, s.phase))
                        .collect::<Vec<_>>()
                        .join(", "),
                    lsp_server_for(&state, state.docs.active())
                        .and_then(|s| state
                            .docs
                            .active()
                            .path
                            .as_ref()
                            .and_then(|p| s.diagnostics.get(p)))
                        .map_or(0, Vec::len),
                    state
                        .completion
                        .as_ref()
                        .map(|c| c
                            .shown
                            .iter()
                            .map(|i| i.label.as_str())
                            .collect::<Vec<_>>()
                            .join("|"))
                        .unwrap_or_default(),
                    self.ivars().draws_during_key_handler.get(),
                    window.title(),
                    state.renderer.atlas.has_pending_shaping(),
                    caret_shaped,
                    state.claude.as_ref().map_or("off".to_string(), |c| format!(
                        "connected={} reviews={} pending={}",
                        c.is_connected(),
                        c.reviews.len(),
                        c.reviews.values().filter(|r| r.decided.is_none()).count()
                    )),
                    format_args!(
                        "open={} focus={} height={} tabs={} selection={:?} screen={:?}",
                        state.terminal.open,
                        state.terminal.focus,
                        state.terminal.height,
                        state
                            .terminal
                            .tabs
                            .iter()
                            .map(|t| t.title.as_str())
                            .collect::<Vec<_>>()
                            .join("|"),
                        state.terminal.selection.and_then(|(a, b)| {
                            state.terminal.active_tab().map(|tab| {
                                tab.session
                                    .term
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .text_between(a, b)
                            })
                        }),
                        state
                            .terminal
                            .active_tab()
                            .map(|t| t
                                .session
                                .term
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .screen_text())
                            .unwrap_or_default()
                    ),
                    selftest::report(
                        state.docs.active(),
                        &titles,
                        state.docs.active_index(),
                        state.marked.as_deref(),
                        &layout,
                        (
                            state.preview.is_some()
                                && default_preview(state.docs.active()).is_some(),
                            state.native_preview.is_some(),
                        ),
                        state
                            .live_line
                            .is_some_and(|(id, _)| id == state.docs.active().id()),
                    )
                );
                let report = format!(
                    // First: the report ends with the document's text.
                    "message: {}\nbranch: {}\nblame: {}\nfind_results: {}\nsignature: {}\nrename: {}\nread_only: {}\nunshaped: {}\nignored_rows: {}\ncompletion_why: {}\n{report}",
                    state.message.as_ref().map_or("", |(text, _)| text.as_str()),
                    state.git.branch_status(),
                    state
                        .blame
                        .as_ref()
                        .map_or("", |(_, _, text)| text.as_str()),
                    state
                        .find
                        .as_ref()
                        .map(|bar| {
                            bar.results
                                .iter()
                                .map(|h| {
                                    format!(
                                        "{}:{}",
                                        h.path
                                            .file_name()
                                            .map(|n| n.to_string_lossy().into_owned())
                                            .unwrap_or_default(),
                                        h.line + 1
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("|")
                        })
                        .unwrap_or_default(),
                    state
                        .signature
                        .as_ref()
                        .map(|t| {
                            let label = &t.signature.label;
                            match &t.signature.active {
                                Some(r) => format!("{} [{}]", label, &label[r.clone()]),
                                None => label.clone(),
                            }
                        })
                        .unwrap_or_default(),
                    state
                        .rename
                        .as_ref()
                        .map(|r| r.field.rope.to_string())
                        .unwrap_or_default(),
                    state.docs.active().is_read_only(),
                    state.unshaped_on_screen,
                    state
                        .tree
                        .rows()
                        .iter()
                        .map(|e| state.tree.ignored(&e.path))
                        .map(|i| match i {
                            crate::project::tree::Ignored::No => "-",
                            crate::project::tree::Ignored::Here => "H",
                            crate::project::tree::Ignored::Inside => "I",
                            crate::project::tree::Ignored::Preview => "P",
                        })
                        .collect::<String>(),
                    state
                        .completion
                        .as_ref()
                        .and_then(|c| c.shown.get(c.selected))
                        .map_or("", |c| c.why.as_str()),
                );
                if let Err(e) = std::fs::write(path, report) {
                    eprintln!("crc: selftest could not write {path}: {e}");
                }
            }
            // Straight out: no unsaved-changes prompt to sit waiting for a
            // person, and no session written over the real one.
            Step::Panic => {
                panic!("selftest: forced panic");
            }
            Step::Quit => {
                self.claude_shutdown();
                std::process::exit(0)
            }
        }
    }

    /// Hands a key event to the system's text input machinery, which answers
    /// through the `NSTextInputClient` methods: `insertText:` for text,
    /// `setMarkedText:` for a composition in progress.
    fn interpret(&self, event: &NSEvent) -> bool {
        // Nothing may be borrowed here. The answers arrive before this
        // returns, on this same stack.
        self.ivars().in_key_down.set(true);
        self.interpretKeyEvents(&NSArray::from_slice(&[event]));
        self.ivars().in_key_down.set(false);
        true
    }

    /// Puts committed text into whatever has the keyboard. Returns which.
    fn commit_text(&self, text: &str, replacement: NSRange) -> Focus {
        let (rows, cols) = self.grid();
        let ((), focus) = self.edit_focused(|buffer, focus| {
            if focus == Focus::Document && buffer.is_preview_file() {
                return;
            }
            // A replacement range is the input system saying "instead of
            // that": the accent menu replacing the letter that was held.
            if replacement.location != NSNotFound as usize {
                buffer.select_input_range(replacement.location, replacement.length);
            }
            let text = match focus {
                Focus::Document => typed_text(text),
                Focus::Goto => text.chars().filter(char::is_ascii_digit).collect(),
                Focus::FindQuery | Focus::Field => single_line(text),
            };
            if text.is_empty() {
                return;
            }
            // One character at a time through the pairing path; a longer
            // run came from an input method and goes in verbatim.
            let mut chars = text.chars();
            match (focus, chars.next(), chars.next()) {
                (Focus::Document, Some(ch), None) => buffer.insert_char_paired(ch),
                _ => buffer.insert(&text),
            }
            if focus == Focus::Document {
                buffer.scroll_to_cursor(rows, cols);
            }
        });
        match focus {
            Focus::FindQuery => self.refresh_find(),
            Focus::Field => {
                let mut state = self.ivars().state.borrow_mut();
                if let Some((_, selected)) = &mut state.palette {
                    *selected = 0;
                }
            }
            Focus::Document => {
                self.lsp_after_key(true);
                self.lsp_typed(text);
            }
            Focus::Goto => {}
        }
        focus
    }

    /// Gets a change that did not come from a key press onto the screen.
    fn show_input_change(&self) {
        if self.ivars().in_key_down.get() {
            return;
        }
        self.reparse();
        self.sync_title();
        self.request_redraw();
        self.pump();
    }

    /// Runs the open panel and opens the chosen file in a tab.
    fn open_file(&self) -> bool {
        let mtm = MainThreadMarker::from(self);
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseFiles(true);
        panel.setAllowsMultipleSelection(false);
        // NSModalResponseOK is 1. The constant is not in the generated
        // bindings, and its value is fixed API.
        const MODAL_RESPONSE_OK: isize = 1;
        if panel.runModal() != MODAL_RESPONSE_OK {
            return false;
        }

        let Some(url) = panel.URL() else {
            return false;
        };
        let Some(path) = url.path() else {
            return false;
        };
        self.load_path(&path.to_string())
    }

    /// Opens `path` in a tab, or switches to its tab when it is already open.
    ///
    /// Shared by the open panel, the sidebar, the palette and the Apple Event
    /// that Finder and `open -a` send, so every route reports failures the
    /// same way and none of them can replace a document somebody is editing.
    pub fn load_path(&self, path: &str) -> bool {
        if std::path::Path::new(path).is_dir() {
            self.load_folder_path(path);
            return true;
        }
        // Finder and `open -a` deliver documents after launch, by which
        // point the argv path has already decided there is no project. Adopt
        // the file's own directory so the sidebar is useful either way.
        let mut adopted_folder = false;
        {
            let mut state = self.ivars().state.borrow_mut();
            if state.tree.root().is_none()
                && let Some(dir) = std::path::Path::new(path).parent()
                && dir.is_dir()
            {
                state.tree.set_root(dir);
                state.git = crate::platform::git_panel::Panel::new(dir.to_path_buf());
                state.project_index_rx = Some(spawn_project_index(dir.to_path_buf()));
                adopted_folder = true;
            }
        }
        if adopted_folder {
            self.watch_project();
            self.resume_display_link();
        }

        // A document lives in one pane. Open elsewhere, it comes to the
        // front there rather than opening a second copy that could diverge.
        let elsewhere = {
            let state = self.ivars().state.borrow();
            let key =
                std::fs::canonicalize(path).unwrap_or_else(|_| std::path::PathBuf::from(path));
            state
                .panes
                .iter()
                .enumerate()
                .find_map(|(slot, pane)| pane.docs.index_of(&key).map(|tab| (slot, tab)))
                .map(|(slot, tab)| (slot + usize::from(slot >= state.focused_pane), tab))
        };
        if let Some((pane, tab)) = elsewhere {
            self.focus_pane(pane);
            let mut state = self.ivars().state.borrow_mut();
            state.docs.switch(tab);
            state.preview = default_preview(state.docs.active());
            reveal_active_tab(&mut state);
            drop(state);
            self.sync_title();
            self.reparse();
            return true;
        }

        let mut state = self.ivars().state.borrow_mut();
        match state.docs.open(path) {
            Ok(()) => {
                state.preview = default_preview(state.docs.active());
                reveal_active_tab(&mut state);
                let format = state.docs.active().disk_format();
                let note = if state.docs.active().is_read_only() {
                    format!(
                        "opened {path} read-only: it is over {}",
                        crate::text::buffer::human_size(crate::text::buffer::READ_ONLY_BYTES)
                    )
                } else if format.encoding == crate::text::file_format::Encoding::Windows1252 {
                    format!("opened {path} as Windows-1252; unsupported characters cannot be saved")
                } else if format.line_ending_label() == "mixed endings" {
                    format!("opened {path} with mixed line endings")
                } else {
                    format!("opened {path}")
                };
                state.message = Some((note, Instant::now()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::FileTooLarge => {
                state.message = Some((format!("not opened: {e}"), Instant::now()));
                drop(state);
                // A test instance cannot answer a modal; the status says it.
                if !self.ivars().testing {
                    let mtm = MainThreadMarker::from(self);
                    let alert = NSAlert::new(mtm);
                    alert.setAlertStyle(NSAlertStyle::Warning);
                    alert.setMessageText(&NSString::from_str("File too large to open"));
                    alert.setInformativeText(&NSString::from_str(&e.to_string()));
                    alert.addButtonWithTitle(&NSString::from_str("OK"));
                    alert.runModal();
                }
                return true;
            }
            Err(e) => {
                state.message = Some((format!("open failed: {e}"), Instant::now()));
            }
        }
        drop(state);
        self.lsp_sync_open();
        true
    }

    /// Snapshots what is open, for the next launch.
    fn capture_session(&self) -> Session {
        let state = self.ivars().state.borrow();
        let frame = self.window().map(|w| {
            let f = w.frame();
            (f.origin.x, f.origin.y, f.size.width, f.size.height)
        });
        Session {
            frame,
            sidebar_width: state.sidebar_width,
            folder: state.tree.root().map(Path::to_path_buf),
            // Every pane's files, focused pane first; they come back in one
            // pane. Splits are a way of looking, not something to restore.
            files: all_docs(&state)
                .flat_map(|d| d.iter().filter_map(|b| b.path.clone()))
                .collect(),
            // An index into `files`, which leaves out untitled documents, so
            // count only the named ones in front of the active tab.
            active: state
                .docs
                .iter()
                .take(state.docs.active_index())
                .filter(|b| b.path.is_some())
                .count(),
            sidebar: state.sidebar,
            recent: state.recent_projects.clone(),
        }
    }

    /// Asks about every document with unsaved changes, one at a time, showing
    /// each as it is asked about. Returns whether the window may go.
    ///
    /// Closing the window and quitting take every tab with them, not only the
    /// one in front, so asking about the active document alone let the rest
    /// vanish without a word.
    fn confirm_discard_all(&self) -> bool {
        if self.ivars().state.borrow().discard_confirmed {
            return true;
        }
        let original = self.ivars().state.borrow().docs.active_index();
        let session = self.capture_session();
        self.ivars().state.borrow_mut().quit_session = Some(session);
        loop {
            // Whichever pane has a dirty document comes to the front first.
            let elsewhere = {
                let state = self.ivars().state.borrow();
                (0..pane_count(&state)).find(|p| {
                    *p != state.focused_pane
                        && state.panes[p - usize::from(*p > state.focused_pane)]
                            .docs
                            .iter()
                            .any(|b| b.is_dirty())
                })
            };
            let next = {
                let state = self.ivars().state.borrow();
                state.docs.iter().position(|b| b.is_dirty())
            };
            let index = match (next, elsewhere) {
                (Some(index), _) => index,
                (None, Some(pane)) => {
                    self.focus_pane(pane);
                    continue;
                }
                (None, None) => break,
            };

            // The alert names the active document and Save acts on it, so
            // bring the one in question to the front. It spins a nested run
            // loop, which is why nothing is borrowed across it.
            self.ivars().state.borrow_mut().docs.switch(index);
            self.sync_title();
            self.reparse();
            self.request_redraw();
            self.pump();

            match self.ask_about_active() {
                Discard::Cancel => {
                    {
                        let mut state = self.ivars().state.borrow_mut();
                        state.quit_session = None;
                        state.docs.switch(original);
                    }
                    self.sync_title();
                    self.reparse();
                    self.request_redraw();
                    self.pump();
                    return false;
                }
                // Saved: no longer dirty, so the scan moves on by itself.
                Discard::Saved => {}
                // Not saved and not wanted. Closing the tab is what stops the
                // scan finding it again, and the window is going anyway.
                Discard::Dropped => {
                    self.ivars().state.borrow_mut().docs.close(index);
                }
            }
        }
        self.ivars().state.borrow_mut().discard_confirmed = true;
        true
    }

    /// Asks about unsaved changes in the active document. Returns whether it
    /// is safe to proceed.
    ///
    /// This is the only thing standing between a stray Cmd-W and losing an
    /// afternoon's work, so it is a real modal, not a status-line note.
    fn confirm_discard(&self) -> bool {
        !matches!(self.ask_about_active(), Discard::Cancel)
    }

    /// Save found the file changed (or gone) since it was opened.
    fn ask_about_conflict(&self, missing: bool) -> Conflict {
        let name = {
            let state = self.ivars().state.borrow();
            state
                .docs
                .active()
                .path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Untitled".to_string())
        };
        let mtm = MainThreadMarker::from(self);
        let alert = NSAlert::new(mtm);
        alert.setAlertStyle(NSAlertStyle::Warning);
        if missing {
            alert.setMessageText(&NSString::from_str(&format!(
                "\u{201c}{name}\u{201d} was deleted on disk."
            )));
            alert.setInformativeText(&NSString::from_str(
                "Saving will create the file again with the text in this tab.",
            ));
            alert.addButtonWithTitle(&NSString::from_str("Save Anyway"));
            alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        } else {
            alert.setMessageText(&NSString::from_str(&format!(
                "\u{201c}{name}\u{201d} has changed on disk since you opened it."
            )));
            alert.setInformativeText(&NSString::from_str(
                "Overwrite keeps the text in this tab. Reload takes the version on \
                 disk and drops your changes; Undo brings them back.",
            ));
            alert.addButtonWithTitle(&NSString::from_str("Overwrite"));
            alert.addButtonWithTitle(&NSString::from_str("Cancel"));
            alert.addButtonWithTitle(&NSString::from_str("Reload"));
        }
        const FIRST: isize = 1000;
        const SECOND: isize = 1001;
        match alert.runModal() {
            FIRST => Conflict::Overwrite,
            SECOND => Conflict::Cancel,
            _ if missing => Conflict::Cancel,
            _ => Conflict::Reload,
        }
    }

    /// File > Revert to Saved: back to what is on disk, asking first when
    /// that drops unsaved changes.
    fn revert_to_saved(&self) {
        let (has_path, dirty) = {
            let state = self.ivars().state.borrow();
            let active = state.docs.active();
            (active.path.is_some(), active.is_dirty())
        };
        if !has_path {
            return;
        }
        if dirty && !self.confirm_revert() {
            return;
        }
        let result = self.ivars().state.borrow_mut().docs.active_mut().reload();
        self.after_reload(true);
        let mut state = self.ivars().state.borrow_mut();
        state.message = Some((
            match result {
                Ok(()) => "reverted to the saved version".to_string(),
                Err(e) => format!("revert failed: {e}"),
            },
            Instant::now(),
        ));
        drop(state);
        self.sync_title();
    }

    fn confirm_revert(&self) -> bool {
        let name = {
            let state = self.ivars().state.borrow();
            state
                .docs
                .active()
                .path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Untitled".to_string())
        };
        let mtm = MainThreadMarker::from(self);
        let alert = NSAlert::new(mtm);
        alert.setAlertStyle(NSAlertStyle::Warning);
        alert.setMessageText(&NSString::from_str(&format!(
            "Revert \u{201c}{name}\u{201d} to the saved version?"
        )));
        alert.setInformativeText(&NSString::from_str(
            "Your unsaved changes will be replaced by the file on disk. Undo brings them back.",
        ));
        alert.addButtonWithTitle(&NSString::from_str("Revert"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        const FIRST: isize = 1000;
        alert.runModal() == FIRST
    }

    /// Looks at every open file and takes in what changed behind the editor.
    ///
    /// A clean tab follows the disk: this is what makes an edit by Claude
    /// Code, a `git checkout` or another editor show up instead of sitting
    /// stale until a save fails. A tab with unsaved changes is left alone
    /// and told, since choosing between two versions is the user's call,
    /// which Save then asks. A deleted file marks its tab unsaved: the text
    /// in the tab is now the only copy.
    fn check_open_files(&self) {
        let Ok(mut state) = self.ivars().state.try_borrow_mut() else {
            return;
        };
        let active_id = state.docs.active().id();
        let mut reloaded = Vec::new();
        let mut conflicts = Vec::new();
        let mut missing = Vec::new();
        let mut lsp_dirty = Vec::new();
        let mut active_changed = false;
        for docs in all_docs_mut(&mut state) {
            for buffer in docs.iter_mut() {
                let Some(name) = buffer
                    .path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                else {
                    continue;
                };
                match buffer.disk_state() {
                    DiskState::Unchanged => {}
                    DiskState::Changed if buffer.is_dirty() => {
                        if !buffer.conflict_noticed {
                            buffer.conflict_noticed = true;
                            conflicts.push(name);
                        }
                    }
                    DiskState::Changed => match buffer.reload() {
                        Ok(()) => {
                            buffer.conflict_noticed = false;
                            reloaded.push(name);
                            lsp_dirty.push(buffer.id());
                            active_changed |= buffer.id() == active_id;
                        }
                        Err(_) => {
                            // Unreadable right now, mid-write most likely.
                            // The next batch or save tries again.
                        }
                    },
                    DiskState::Missing => {
                        if !buffer.is_dirty() || !buffer.conflict_noticed {
                            buffer.note_missing_on_disk();
                            buffer.conflict_noticed = true;
                            missing.push(name);
                        }
                    }
                }
            }
        }
        for id in lsp_dirty {
            state.lsp_dirty.insert(id, Instant::now());
            state.gutter_dirty.insert(id, Instant::now());
        }
        let message = match (reloaded.len(), conflicts.len(), missing.len()) {
            (0, 0, 0) => None,
            (1, 0, 0) => Some(format!("{} changed on disk, reloaded", reloaded[0])),
            (n, 0, 0) => Some(format!("{n} files changed on disk, reloaded")),
            (_, 1, 0) => Some(format!(
                "{} changed on disk; you have unsaved changes, Save will ask",
                conflicts[0]
            )),
            (_, 0, 1) => Some(format!("{} was deleted on disk", missing[0])),
            _ => Some("files changed on disk; see each tab".to_string()),
        };
        if let Some(message) = message {
            state.message = Some((message, Instant::now()));
        }
        drop(state);
        if active_changed {
            self.after_reload(false);
        }
        self.sync_title();
        self.resume_display_link();
        self.request_redraw();
    }

    /// The active document's text was replaced from disk: everything keyed
    /// on it starts over. `announce` is for the explicit reloads, which
    /// also drop the completion popup and any live search.
    fn after_reload(&self, announce: bool) {
        self.reparse();
        {
            let mut state = self.ivars().state.borrow_mut();
            let active = state.docs.active();
            let id = active.id();
            if active.path.is_some() {
                state.lsp_dirty.insert(id, Instant::now());
                state.gutter_dirty.insert(id, Instant::now());
            }
            state.completion = None;
            if announce {
                state.preview = default_preview(state.docs.active());
            }
        }
        self.request_redraw();
    }

    fn ask_about_active(&self) -> Discard {
        let (dirty, name) = {
            let state = self.ivars().state.borrow();
            (
                state.docs.active().is_dirty(),
                state
                    .docs
                    .active()
                    .path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Untitled".to_string()),
            )
        };
        if !dirty {
            return Discard::Saved;
        }

        let mtm = MainThreadMarker::from(self);
        let alert = NSAlert::new(mtm);
        {
            alert.setAlertStyle(NSAlertStyle::Warning);
            alert.setMessageText(&NSString::from_str(&format!(
                "Do you want to save the changes to \u{201c}{name}\u{201d}?"
            )));
            alert.setInformativeText(&NSString::from_str(
                "Your changes will be lost if you don't save them.",
            ));
            // Order matters: the first button is the default and maps to
            // NSAlertFirstButtonReturn (1000).
            alert.addButtonWithTitle(&NSString::from_str("Save"));
            alert.addButtonWithTitle(&NSString::from_str("Cancel"));
            alert.addButtonWithTitle(&NSString::from_str("Don't Save"));
        }

        const FIRST: isize = 1000;
        const SECOND: isize = 1001;
        match alert.runModal() {
            // A save that was cancelled from its panel, or failed, is a
            // cancel: the work is still only in memory.
            FIRST if self.save(false) => Discard::Saved,
            FIRST | SECOND => Discard::Cancel,
            _ => Discard::Dropped, // Don't Save
        }
    }

    /// Saves, falling back to a Save As panel when there is no path yet.
    /// Returns whether the file actually reached disk.
    fn save(&self, force_panel: bool) -> bool {
        let needs_panel = force_panel || self.ivars().state.borrow().docs.active().path.is_none();

        let chosen = if needs_panel {
            let mtm = MainThreadMarker::from(self);
            let panel = NSSavePanel::savePanel(mtm);
            let suggested = {
                let state = self.ivars().state.borrow();
                state
                    .docs
                    .active()
                    .path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Untitled.txt".to_string())
            };
            panel.setNameFieldStringValue(&NSString::from_str(&suggested));
            const MODAL_RESPONSE_OK: isize = 1;
            if panel.runModal() != MODAL_RESPONSE_OK {
                return false;
            }
            let Some(url) = panel.URL() else {
                return false;
            };
            let Some(path) = url.path() else {
                return false;
            };
            Some(std::path::PathBuf::from(path.to_string()))
        } else {
            None
        };

        let mut state = self.ivars().state.borrow_mut();
        let mut result = state.docs.active_mut().save(chosen.as_deref());
        if let Err(e) = &result
            && matches!(
                e.kind(),
                std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::NotFound
            )
            && chosen.is_none()
        {
            // Someone else changed or removed the file. The alert spins a
            // nested run loop, so the borrow goes first.
            let missing = e.kind() == std::io::ErrorKind::NotFound;
            drop(state);
            match self.ask_about_conflict(missing) {
                Conflict::Overwrite => {
                    state = self.ivars().state.borrow_mut();
                    result = state.docs.active_mut().save_overwriting(None);
                }
                Conflict::Reload => {
                    state = self.ivars().state.borrow_mut();
                    result = state.docs.active_mut().reload();
                    drop(state);
                    self.after_reload(true);
                    let mut state = self.ivars().state.borrow_mut();
                    state.message = Some((
                        match &result {
                            Ok(()) => "reloaded from disk".to_string(),
                            Err(e) => format!("reload failed: {e}"),
                        },
                        Instant::now(),
                    ));
                    drop(state);
                    self.sync_title();
                    return false;
                }
                Conflict::Cancel => {
                    state = self.ivars().state.borrow_mut();
                    state.message = Some(("not saved".to_string(), Instant::now()));
                    return false;
                }
            }
        }
        let ok = result.is_ok();
        state.message = Some((
            match result {
                Ok(()) => match &state.docs.active_mut().path {
                    Some(p) => format!("saved {}", p.display()),
                    None => "saved".to_string(),
                },
                Err(e) => format!("save failed: {e}"),
            },
            Instant::now(),
        ));
        if ok {
            state.git.refresh();
            if let Some(indexer) = &state.indexer {
                indexer.poke();
            }
        }
        let saved_path = ok.then(|| state.docs.active().path.clone()).flatten();
        drop(state);
        if saved_path.is_some() && saved_path == crate::platform::settings::Settings::path() {
            self.apply_settings_file();
        }
        // The watcher ignores this process's own writes, so a .gitignore
        // saved here would otherwise leave the Explorer showing the old rules.
        if saved_path.as_deref().is_some_and(changes_ignore_rules) {
            let mut state = self.ivars().state.borrow_mut();
            if let Some(root) = state.tree.root().map(Path::to_path_buf) {
                state.ignored_rx = Some(spawn_ignored(root));
            }
        }
        if ok {
            self.resume_display_link();
        }
        if let Some(path) = saved_path {
            self.lsp_flush_changes();
            self.lsp_sync_open();
            let mut state = self.ivars().state.borrow_mut();
            for server in state.lsp.values_mut() {
                server.did_save(&path);
            }
            // Saved first, formatted after: a server that never answers
            // cannot hold a save hostage. The formatted text is saved again.
            let format = state.format_on_save && !state.saving_formatted;
            drop(state);
            if format {
                self.format_document(true);
            }
        }
        self.sync_title();
        ok
    }

    /// Brings the active document's parse tree up to date.
    ///
    /// Called after anything that changes the text or which document is
    /// showing. The rules, which trees belong to which document and when an
    /// incremental parse is allowed, live in [`SyntaxStore::update`], where
    /// they can be tested without a window. Files past [`reparse_budget`] get
    /// no highlighting rather than a stalled editor: the first parse is
    /// linear in file size even though later ones are incremental.
    fn reparse(&self) {
        let mut state = self.ivars().state.borrow_mut();
        let State {
            docs,
            syntax,
            spans,
            responses,
            panes,
            ..
        } = &mut *state;
        syntax.update(docs.active_mut(), reparse_budget());
        // Closed tabs take their trees with them.
        let open = |id: u64| {
            docs.iter().any(|b| b.id() == id)
                || panes.iter().any(|p| p.docs.iter().any(|b| b.id() == id))
        };
        syntax.retain(open);
        responses.retain(|id, _| open(*id));
        if !syntax.has(docs.active().id()) {
            spans.clear();
        }
    }

    /// Mirrors the filename and dirty state into the title bar, including the
    /// dot in the close button that macOS users read as "unsaved".
    fn sync_title(&self) {
        let Some(window) = self.window() else {
            return;
        };
        let state = self.ivars().state.borrow();
        // With nothing open the window is the app, not a document.
        let title = if state.docs.is_home() {
            "crc".to_string()
        } else {
            state.docs.title(state.docs.active_index())
        };
        let dirty = state.docs.active().is_dirty();
        drop(state);

        window.setTitle(&NSString::from_str(&title));
        window.setDocumentEdited(dirty);
    }

    /// Keys while the find bar has focus. Returns whether it consumed them.
    fn handle_find_key(&self, event: &NSEvent) -> bool {
        self.poll_project_search();
        const ESCAPE: u16 = 53;
        let flags = event.modifierFlags();
        let command = flags.contains(NSEventModifierFlags::Command);
        let shift = flags.contains(NSEventModifierFlags::Shift);
        let code = match event.keyCode() {
            key::KEYPAD_ENTER => key::RETURN,
            code => code,
        };

        const TAB: u16 = 48;
        if matches!(code, key::UP | key::DOWN)
            && self
                .ivars()
                .state
                .borrow()
                .find
                .as_ref()
                .is_some_and(|b| b.project && !b.results.is_empty())
        {
            let mut state = self.ivars().state.borrow_mut();
            let bar = state.find.as_mut().expect("checked above");
            bar.selected = if code == key::DOWN {
                (bar.selected + 1).min(bar.results.len() - 1)
            } else {
                bar.selected.saturating_sub(1)
            };
            if bar.selected < bar.result_scroll {
                bar.result_scroll = bar.selected;
            }
            if bar.selected >= bar.result_scroll + 8 {
                bar.result_scroll = bar.selected - 7;
            }
            drop(state);
            self.request_redraw();
            self.pump();
            return true;
        }
        match code {
            ESCAPE => {
                self.close_find();
                return true;
            }
            TAB => {
                // Tab crosses between Find and Replace rather than inserting.
                let mut state = self.ivars().state.borrow_mut();
                if let Some(find) = &mut state.find {
                    find.replacing = !find.replacing;
                }
                drop(state);
                self.request_redraw();
                self.pump();
                return true;
            }
            key::RETURN => {
                let (replacing, project) = self
                    .ivars()
                    .state
                    .borrow()
                    .find
                    .as_ref()
                    .map(|f| (f.replacing, f.project))
                    .unwrap_or((false, false));
                if project {
                    let selected = {
                        let state = self.ivars().state.borrow();
                        state
                            .find
                            .as_ref()
                            .and_then(|bar| (!bar.results.is_empty()).then_some(bar.selected))
                    };
                    if let Some(index) = selected {
                        self.open_project_result(index);
                    } else {
                        self.search_project();
                    }
                } else if replacing {
                    self.replace_one();
                } else {
                    self.find_step(!shift);
                }
                return true;
            }
            _ => {}
        }

        // The find fields are real editors. Handle the familiar shortcuts
        // here as well as through menu actions, because AppKit does not send
        // every key equivalent through the menu on every keyboard layout.
        if command {
            let letter = event
                .charactersIgnoringModifiers()
                .and_then(|s| s.to_string().chars().next())
                .map(|c| c.to_ascii_lowercase());
            let mut state = self.ivars().state.borrow_mut();
            let Some(bar) = &mut state.find else {
                return false;
            };
            let replacing = bar.replacing;
            let field = if replacing {
                &mut bar.replacement
            } else {
                &mut bar.query
            };
            let mut changed = false;
            match (code, letter) {
                (key::DELETE, _) => {
                    field.delete_to_line_start();
                    changed = true;
                }
                (key::FORWARD_DELETE, _) => {
                    field.delete_to_line_end();
                    changed = true;
                }
                (key::LEFT, _) => {
                    field.move_line_start(if shift { Motion::Extend } else { Motion::Move })
                }
                (key::RIGHT, _) => {
                    field.move_line_end(if shift { Motion::Extend } else { Motion::Move })
                }
                (_, Some('a')) => field.select_all(),
                (_, Some('c')) => {
                    if let Some(text) = field.selected_text() {
                        clipboard::write_text(&text);
                    }
                }
                (_, Some('x')) => {
                    if let Some(text) = field.selected_text() {
                        clipboard::write_text(&text);
                        field.backspace();
                        changed = true;
                    }
                }
                (_, Some('v')) => {
                    if let Some(text) = clipboard::read_text() {
                        field.insert(&single_line(&text));
                        changed = true;
                    }
                }
                (_, Some('z')) => {
                    if shift {
                        field.redo();
                    } else {
                        field.undo();
                    }
                    changed = true;
                }
                _ => return false,
            }
            drop(state);
            if changed && !replacing {
                self.refresh_find();
            }
            self.request_redraw();
            self.pump();
            return true;
        }

        let motion = if shift { Motion::Extend } else { Motion::Move };
        {
            let mut as_text = false;
            let mut state = self.ivars().state.borrow_mut();
            let Some(bar) = &mut state.find else {
                return false;
            };
            let replacing = bar.replacing;
            let field = if replacing {
                &mut bar.replacement
            } else {
                &mut bar.query
            };
            match code {
                key::LEFT => {
                    if flags.contains(NSEventModifierFlags::Option) {
                        field.move_word_left(motion)
                    } else {
                        field.move_left(motion)
                    }
                }
                key::RIGHT => {
                    if flags.contains(NSEventModifierFlags::Option) {
                        field.move_word_right(motion)
                    } else {
                        field.move_right(motion)
                    }
                }
                key::HOME | key::UP => field.move_line_start(motion),
                key::END | key::DOWN => field.move_line_end(motion),
                key::DELETE => {
                    if flags.contains(NSEventModifierFlags::Option) {
                        field.delete_word_backward()
                    } else {
                        field.backspace()
                    }
                }
                key::FORWARD_DELETE => {
                    if flags.contains(NSEventModifierFlags::Option) {
                        field.delete_word_forward()
                    } else {
                        field.delete_forward()
                    }
                }
                _ => as_text = true,
            }
            if as_text {
                // The borrow has to end before the input system is asked,
                // because it answers by calling back into this view.
                drop(state);
                return self.interpret(event);
            }
            // Editing the replacement does not move the match.
            if replacing {
                drop(state);
                self.request_redraw();
                self.pump();
                return true;
            }
        }

        self.refresh_find();
        true
    }

    /// Re-runs the search from the top of the current match, so the selection
    /// tracks the query as it changes, however it changed.
    fn refresh_find(&self) {
        {
            let mut state = self.ivars().state.borrow_mut();
            if let Some(cancel) = state.project_search_cancel.take() {
                cancel.store(true, Ordering::Relaxed);
            }
            state.project_search_rx = None;
            if let Some(bar) = &mut state.find
                && bar.project
            {
                bar.results.clear();
                bar.searching = false;
                return;
            }
        }
        let (at, query, options, text) = {
            let state = self.ivars().state.borrow();
            let at = state
                .docs
                .active()
                .selection()
                .map_or(state.docs.active().cursor(), |r| r.start);
            let Some(bar) = &state.find else { return };
            (
                at,
                bar.query.rope.to_string(),
                bar.options,
                state.docs.active().rope.to_string(),
            )
        };
        if query.is_empty() {
            return;
        }
        let matches = match search::find(&text, &query, "", options) {
            Ok(matches) => matches,
            Err(error) => {
                self.ivars().state.borrow_mut().message =
                    Some((format!("invalid regex: {error}"), Instant::now()));
                return;
            }
        };
        if let Some(found) = matches
            .iter()
            .find(|m| m.range.start >= at)
            .or_else(|| matches.first())
        {
            let (rows, cols) = self.grid();
            let mut state = self.ivars().state.borrow_mut();
            state
                .docs
                .active_mut()
                .select_range(found.range.start, found.range.end);
            state.docs.active_mut().scroll_to_cursor(rows, cols);
        }
    }

    /// Opens the go-to-line field.
    fn open_goto(&self) {
        self.ivars().state.borrow_mut().goto = Some(Buffer::new());
        self.request_redraw();
        self.pump();
    }

    /// Keys while the go-to-line field is open.
    fn handle_goto_key(&self, event: &NSEvent) -> bool {
        const ESCAPE: u16 = 53;
        if event
            .modifierFlags()
            .contains(NSEventModifierFlags::Command)
        {
            return false;
        }
        let code = match event.keyCode() {
            key::KEYPAD_ENTER => key::RETURN,
            code => code,
        };

        match code {
            ESCAPE => {
                self.ivars().state.borrow_mut().goto = None;
            }
            key::RETURN => {
                let target = {
                    let state = self.ivars().state.borrow();
                    state
                        .goto
                        .as_ref()
                        .and_then(|b| b.rope.to_string().trim().parse::<usize>().ok())
                };
                {
                    let mut state = self.ivars().state.borrow_mut();
                    state.goto = None;
                    if let Some(line) = target {
                        // People count lines from one; the buffer counts from
                        // zero.
                        state.docs.active_mut().goto_line(line.saturating_sub(1));
                    }
                }
                let (rows, cols) = self.grid();
                self.ivars()
                    .state
                    .borrow_mut()
                    .docs
                    .active_mut()
                    .scroll_to_cursor(rows, cols);
            }
            key::DELETE => {
                let mut state = self.ivars().state.borrow_mut();
                if let Some(b) = &mut state.goto {
                    b.backspace();
                }
            }
            // Text, through the input system like everywhere else.
            // `commit_text` keeps only the digits for this field.
            _ => return self.interpret(event),
        }
        self.request_redraw();
        self.pump();
        true
    }

    /// Tracks a sidebar drag and works out whether it could be dropped here.
    fn tree_drag_moved(&self, x: f32, y: f32) {
        let Some(rect) = self.chrome().sidebar else {
            return;
        };
        let (changed, active) = 'drag: {
            let mut state = self.ivars().state.borrow_mut();
            let row = layout::sidebar_row_at(&state.tree, &state.renderer.atlas, rect, y);
            // The destination directory: the folder under the pointer, the
            // parent of a file under it, or the project root below the tree.
            let destination = match row.and_then(|index| state.tree.rows().get(index)) {
                Some(entry) if entry.is_dir => Some(entry.path.clone()),
                Some(entry) => entry.path.parent().map(Path::to_path_buf),
                None => state.tree.root().map(Path::to_path_buf),
            };
            let State { tree_drag, .. } = &mut *state;
            let Some(drag) = tree_drag else {
                break 'drag (false, false);
            };
            if !drag.active {
                let far = (x - drag.origin.0).abs() > 4.0 || (y - drag.origin.1).abs() > 4.0;
                if !far {
                    break 'drag (false, false);
                }
                drag.active = true;
            }
            let valid = rect.contains(x, y)
                && destination
                    .as_deref()
                    .is_some_and(|dir| valid_drop(&drag.path, dir));
            let changed = drag.over != row || drag.valid != valid;
            drag.over = row;
            drag.valid = valid;
            (changed, true)
        };
        if active && changed {
            self.request_redraw();
            self.pump();
        }
    }

    /// Moves the dragged item into the folder it was dropped on.
    fn drop_tree_item(&self, drag: TreeDrag) {
        let Some(rect) = self.chrome().sidebar else {
            return;
        };
        let destination = {
            let state = self.ivars().state.borrow();
            match drag.over.and_then(|index| state.tree.rows().get(index)) {
                Some(entry) if entry.is_dir => Some(entry.path.clone()),
                Some(entry) => entry.path.parent().map(Path::to_path_buf),
                None => state.tree.root().map(Path::to_path_buf),
            }
        };
        let _ = rect;
        let Some(destination) = destination else {
            return;
        };
        let Some(name) = drag.path.file_name() else {
            return;
        };
        let target = destination.join(name);
        // Canonical identity before the move, for the same reason rename
        // captures it: afterwards the old path cannot be canonicalised.
        let source_key = std::fs::canonicalize(&drag.path).unwrap_or_else(|_| drag.path.clone());
        // Unsaved work under the item would be stranded at a path that no
        // longer exists, the same guard Move to Trash uses.
        if self
            .ivars()
            .state
            .borrow()
            .docs
            .has_dirty_under(&source_key)
        {
            self.ivars().state.borrow_mut().message = Some((
                "save the affected tabs before moving this item".into(),
                Instant::now(),
            ));
            self.request_redraw();
            self.pump();
            return;
        }
        match move_without_replace(&drag.path, &target) {
            Ok(()) => {
                {
                    let mut state = self.ivars().state.borrow_mut();
                    for docs in all_docs_mut(&mut state) {
                        docs.rename_path(&source_key, &target);
                    }
                    state.message = Some((
                        format!(
                            "moved {} to {}",
                            name.to_string_lossy(),
                            destination.display()
                        ),
                        Instant::now(),
                    ));
                }
                self.refresh_project_after_disk_change();
                self.sync_title();
                self.reparse();
            }
            Err(error) => {
                self.ivars().state.borrow_mut().message =
                    Some((format!("move failed: {error}"), Instant::now()));
            }
        }
        self.request_redraw();
        self.pump();
    }

    /// Closes every expanded directory in the tree.
    fn collapse_tree(&self) {
        self.ivars().state.borrow_mut().tree.collapse_all();
        self.invalidate_tab_cursors();
        self.request_redraw();
        self.pump();
    }

    /// Re-reads the tree from disk. Nothing watches the filesystem, so a file
    /// created outside the editor needs asking for.
    fn refresh_tree(&self) {
        self.refresh_project_after_disk_change();
    }

    /// Records which tab the pointer is over, redrawing only when it changes.
    ///
    /// `mouseMoved:` fires on every pixel of movement; redrawing each time
    /// would spend a frame on nothing. A non-finite point means the pointer
    /// left the window.
    fn note_hover(&self, x: f32, y: f32) {
        let chrome = self.chrome();
        let over = (x.is_finite() && chrome.tabs.contains(x, y))
            .then(|| {
                let state = self.ivars().state.borrow();
                state
                    .tab_hits
                    .iter()
                    .find(|hit| x >= hit.x0 && x < hit.x1)
                    .map(|hit| hit.index)
            })
            .flatten();
        let mut state = self.ivars().state.borrow_mut();
        if state.hovered_tab == over {
            return;
        }
        state.hovered_tab = over;
        drop(state);
        self.invalidate_tab_cursors();
        self.request_redraw();
        self.pump();
    }

    /// The close button appears and disappears with hover, so its pointer
    /// rectangle has to be rebuilt when it does.
    fn invalidate_tab_cursors(&self) {
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
    }

    fn open_git(&self) {
        let scm = self.ivars().state.borrow().git_open;
        self.set_sidebar_view(!scm);
    }

    /// Switches the sidebar between the file tree and source control.
    ///
    /// Source control is a view of the project, not a modal: opening it does
    /// not take the keyboard, and leaving it gives the editor column back to
    /// the active document.
    fn set_sidebar_view(&self, scm: bool) {
        {
            let mut state = self.ivars().state.borrow_mut();
            state.git_open = scm;
            // Typing belongs to the editor until the message field is asked
            // for. Focusing it here would swallow the next keystroke.
            state.git_focus = false;
            if scm {
                state.palette = None;
                state.goto = None;
                state.git.refresh();
            } else {
                state.git.showing_diff = false;
            }
        }
        // A sidebar that has been hidden cannot show either view.
        if scm && !self.ivars().state.borrow().sidebar {
            self.action_toggle_sidebar(sel!(toggleSidebar:), None);
        }
        self.request_redraw();
        self.resume_display_link();
        self.pump();
    }

    /// A click inside the source-control column. Returns whether it landed on
    /// something; the caller falls through to the file tree otherwise.
    fn git_click(&self, column: Viewport, x: f32, y: f32) -> bool {
        use crate::platform::git_panel::{Entry, Sidebar};
        let mut state = self.ivars().state.borrow_mut();
        let g = Sidebar::new(column);
        let mut handled = true;
        if g.refresh.contains(x, y) {
            state.git.refresh();
        } else if g.commit.contains(x, y) {
            state.git.commit();
        } else if g.message.contains(x, y) {
            state.git_focus = true;
            let State { git, renderer, .. } = &mut *state;
            let text = git.message.rope.to_string();
            let (shown, start) = layout::ui_input_window(&text, git.message.cursor());
            if let Some(line) = renderer.atlas.shape_ui(&shown) {
                git.message
                    .place_cursor(start + line.byte_at_x(x - g.message.x - 9.0), Motion::Move);
            }
        } else if let Some((entry, rect)) = state.git.entry_at(g, x, y) {
            match entry {
                Entry::File { change, staged } => {
                    // The staging control is on the row, so a click near the
                    // trailing edge stages rather than selects.
                    if g.toggle(rect).contains(x, y) {
                        state.git.stage_index(change, !staged);
                    } else {
                        state.git_focus = false;
                        state.git.showing_diff = true;
                        state.git.select(change);
                    }
                }
                Entry::Section { .. } => handled = false,
            }
        } else {
            handled = false;
        }
        drop(state);
        self.request_redraw();
        self.resume_display_link();
        self.pump();
        handled
    }

    /// Keys while source control is showing. Only the commit message field
    /// claims them, and only once it has been clicked: a docked view that ate
    /// every keystroke would make the editor beside it unusable.
    fn handle_git_key(&self, event: &NSEvent) -> bool {
        let flags = event.modifierFlags();
        if flags.contains(NSEventModifierFlags::Command) {
            if event.keyCode() == key::RETURN {
                self.ivars().state.borrow_mut().git.commit();
                self.resume_display_link();
                return true;
            }
            return false;
        }
        if event.keyCode() == 53 {
            // Escape steps back out: first the diff, then the field, then the
            // view itself.
            let mut state = self.ivars().state.borrow_mut();
            if state.git_focus {
                state.git_focus = false;
            } else if state.git.showing_diff {
                state.git.showing_diff = false;
            } else {
                drop(state);
                self.set_sidebar_view(false);
                return true;
            }
            drop(state);
            self.request_redraw();
            return true;
        }
        if !self.ivars().state.borrow().git_focus {
            return false;
        }
        let mut state = self.ivars().state.borrow_mut();
        let motion = if flags.contains(NSEventModifierFlags::Shift) {
            Motion::Extend
        } else {
            Motion::Move
        };
        let option = flags.contains(NSEventModifierFlags::Option);
        match event.keyCode() {
            key::DELETE => {
                if option && state.git.message.selection().is_none() {
                    state.git.message.move_word_left(Motion::Extend);
                }
                state.git.message.backspace();
            }
            key::LEFT if option => state.git.message.move_word_left(motion),
            key::RIGHT if option => state.git.message.move_word_right(motion),
            key::LEFT => state.git.message.move_left(motion),
            key::RIGHT => state.git.message.move_right(motion),
            key::RETURN | key::TAB => return true,
            _ => {
                drop(state);
                return self.interpret(event);
            }
        }
        drop(state);
        self.resume_display_link();
        true
    }

    /// Opens the Cmd-P palette. The project index arrives from its worker.
    fn open_palette(&self) {
        self.open_palette_with("");
    }

    /// Opens the palette with `prefix` typed: `@` for the document's
    /// symbols, `#` for the project's.
    fn open_palette_with(&self, prefix: &str) {
        self.ivars().state.borrow_mut().git_open = false;
        // Before the field takes the keyboard: validation asks the editor
        // which commands apply, and a focused field changes the answer.
        let commands = commands::from_menu(MainThreadMarker::from(self), |item| {
            let Some(action) = item.action() else {
                return false;
            };
            let handles: bool = unsafe { msg_send![self, respondsToSelector: action] };
            // Hide, Quit and Minimize belong to the app and the window.
            !handles || unsafe { msg_send![self, validateMenuItem: item] }
        });
        {
            let mut state = self.ivars().state.borrow_mut();
            state.commands = commands;
            let document = {
                let buffer = state.docs.active();
                buffer
                    .extension()
                    .and_then(|e| Language::from_extension(&e))
                    .map(|language| (language, buffer.rope.clone()))
            };
            let root = state.tree.root().map(Path::to_path_buf);
            state.symbols.reset(document, root);
            state.branch_list = None;
            let mut query = Buffer::new();
            if !prefix.is_empty() {
                query.insert(prefix);
            }
            state.palette = Some((query, 0));
            state.palette_scroll = 0;
        }
        self.request_redraw();
        self.pump();
    }

    fn palette_click(&self, x: f32, y: f32) {
        let chosen = {
            let mut state = self.ivars().state.borrow_mut();
            let count = state
                .palette
                .as_ref()
                .map(|(query, _)| {
                    palette_rows(&palette_sources(&state), &query.rope.to_string()).len()
                })
                .unwrap_or(0);
            let rect = layout::palette_rect(state.viewport, count);
            if !rect.contains(x, y) || (x >= rect.x + rect.width - 52.0 && y < rect.y + 50.0) {
                drop(state);
                self.close_palette();
                return;
            }
            if y < rect.y + 52.0 {
                let State {
                    palette, renderer, ..
                } = &mut *state;
                if let Some((query, _)) = palette {
                    let text = query.rope.to_string();
                    let (shown, start) = layout::ui_input_window(&text, query.cursor());
                    if let Some(line) = renderer.atlas.shape_ui(&shown) {
                        query.place_cursor(start + line.byte_at_x(x - rect.x - 18.0), Motion::Move);
                    }
                }
                drop(state);
                self.request_redraw();
                self.pump();
                return;
            }
            let Some((query, _)) = &state.palette else {
                return;
            };
            let Some(row) = layout::palette_row_at(rect, x, y) else {
                return;
            };
            let first = state.palette_scroll.min(layout::palette_max_scroll(
                count,
                layout::palette_visible_rows(rect),
            ));
            palette_rows(&palette_sources(&state), &query.rope.to_string())
                .into_iter()
                .nth(first + row)
                .map(|(_, pick)| pick)
        };
        if let Some(Pick::Branch(name)) = chosen {
            self.close_palette();
            self.switch_branch(name, false);
        } else if let Some(Pick::NewBranch(name)) = chosen {
            self.close_palette();
            self.switch_branch(name, true);
        } else if let Some(Pick::Symbol(path, line)) = chosen {
            self.close_palette();
            self.go_to_symbol(path, line);
        } else if let Some(Pick::Command(at)) = chosen {
            self.close_palette();
            self.run_command(at);
        } else if let Some(Pick::File(path)) = chosen {
            self.close_palette();
            self.load_path(&path.to_string_lossy());
            self.sync_title();
            self.reparse();
            self.ivars().state.borrow_mut().tree.reveal(&path);
            self.request_redraw();
            self.pump();
        }
    }

    /// Sends a command's action as its menu item would: to the editor when
    /// it handles it, otherwise down the responder chain to the window and
    /// the app. The editor first, because it is where the palette lives
    /// whether or not its window is key.
    fn run_command(&self, action: Sel) {
        let handles: bool = unsafe { msg_send![self, respondsToSelector: action] };
        if handles {
            let _: () =
                unsafe { msg_send![self, performSelector: action, withObject: None::<&AnyObject>] };
        } else {
            let app = NSApplication::sharedApplication(MainThreadMarker::from(self));
            // SAFETY: the action is a menu item's, taking one sender argument
            // as every action does; a nil target is the responder chain.
            unsafe { app.sendAction_to_from(action, None, None) };
        }
    }

    /// Wheel and trackpad scrolling of the palette's list, a row at a time,
    /// keeping the part of a trackpad delta that has not made a row yet.
    fn palette_wheel(&self, event: &NSEvent) {
        self.palette_wheel_by(event.scrollingDeltaY(), event.hasPreciseScrollingDeltas());
    }

    fn palette_wheel_by(&self, dy: f64, precise: bool) {
        let mut state = self.ivars().state.borrow_mut();
        let Some((query, _)) = &state.palette else {
            return;
        };
        let count = palette_rows(&palette_sources(&state), &query.rope.to_string()).len();
        let visible = layout::palette_visible_rows(layout::palette_rect(state.viewport, count));
        state.palette_scroll_carry -= if precise {
            dy / (layout::PALETTE_ROW as f64 * 0.5)
        } else {
            dy
        };
        let rows = state.palette_scroll_carry.trunc() as isize;
        state.palette_scroll_carry -= rows as f64;
        let max = layout::palette_max_scroll(count, visible);
        let scroll = state
            .palette_scroll
            .min(max)
            .saturating_add_signed(rows)
            .min(max);
        if scroll != state.palette_scroll {
            state.palette_scroll = scroll;
            drop(state);
            self.request_redraw();
            self.pump();
        }
    }

    fn close_palette(&self) {
        {
            let mut state = self.ivars().state.borrow_mut();
            state.palette = None;
            state.branch_list = None;
        }
        self.request_redraw();
        self.pump();
    }

    /// Keys while the palette is open. Returns whether it consumed them.
    fn handle_palette_key(&self, event: &NSEvent) -> bool {
        const ESCAPE: u16 = 53;
        let flags = event.modifierFlags();
        let code = match event.keyCode() {
            key::KEYPAD_ENTER => key::RETURN,
            code => code,
        };
        if flags.contains(NSEventModifierFlags::Command) {
            // Cmd-Delete clears the field to the left of the caret. It has no
            // menu item, so returning false here left it doing nothing at all;
            // everything else with Command still belongs to the menus.
            if code == key::DELETE {
                let mut state = self.ivars().state.borrow_mut();
                if let Some((query, selected)) = &mut state.palette {
                    if query.selection().is_none() {
                        query.move_line_start(Motion::Extend);
                    }
                    query.backspace();
                    *selected = 0;
                    state.palette_scroll = 0;
                    drop(state);
                    self.request_redraw();
                    self.pump();
                    return true;
                }
            }
            return false;
        }

        match code {
            ESCAPE => {
                self.close_palette();
                return true;
            }
            key::RETURN => {
                let chosen = {
                    let state = self.ivars().state.borrow();
                    let Some((query, selected)) = &state.palette else {
                        return false;
                    };
                    palette_rows(&palette_sources(&state), &query.rope.to_string())
                        .into_iter()
                        .nth(*selected)
                        .map(|(_, pick)| pick)
                };
                self.close_palette();
                if let Some(Pick::Branch(name)) = chosen {
                    self.switch_branch(name, false);
                } else if let Some(Pick::NewBranch(name)) = chosen {
                    self.switch_branch(name, true);
                } else if let Some(Pick::Symbol(path, line)) = chosen {
                    self.go_to_symbol(path, line);
                } else if let Some(Pick::Command(at)) = chosen {
                    self.run_command(at);
                } else if let Some(Pick::File(path)) = chosen {
                    self.load_path(&path.to_string_lossy());
                    self.ivars().state.borrow_mut().title_sync_pending = true;
                    self.reparse();
                    {
                        let mut state = self.ivars().state.borrow_mut();
                        state.tree.reveal(&path);
                    }
                    self.request_redraw();
                    self.pump();
                }
                return true;
            }
            key::UP | key::DOWN => {
                let mut state = self.ivars().state.borrow_mut();
                let count = state
                    .palette
                    .as_ref()
                    .map(|(query, _)| {
                        palette_rows(&palette_sources(&state), &query.rope.to_string()).len()
                    })
                    .unwrap_or(0);
                let visible =
                    layout::palette_visible_rows(layout::palette_rect(state.viewport, count));
                if let Some((_, selected)) = &mut state.palette {
                    if code == key::DOWN {
                        *selected = (*selected + 1).min(count.saturating_sub(1));
                    } else {
                        *selected = selected.saturating_sub(1);
                    }
                    let selected = *selected;
                    state.palette_scroll =
                        layout::palette_follow(state.palette_scroll, selected, count, visible);
                }
                drop(state);
                self.request_redraw();
                self.pump();
                return true;
            }
            _ => {}
        }

        {
            let mut as_text = false;
            let mut state = self.ivars().state.borrow_mut();
            let Some((query, selected)) = &mut state.palette else {
                return false;
            };
            // The field used to ignore modifiers on every key, so Option-
            // Delete removed one character instead of a word and Cmd-Delete
            // did the same. A query field is a text field; the standard
            // editing combinations have to reach it.
            let option = flags.contains(NSEventModifierFlags::Option);
            let motion = if flags.contains(NSEventModifierFlags::Shift) {
                Motion::Extend
            } else {
                Motion::Move
            };
            match code {
                key::DELETE => {
                    if query.selection().is_none() {
                        if option {
                            query.move_word_left(Motion::Extend);
                        } else if flags.contains(NSEventModifierFlags::Control) {
                            query.select_all();
                        }
                    }
                    query.backspace();
                }
                key::LEFT if option => query.move_word_left(motion),
                key::RIGHT if option => query.move_word_right(motion),
                key::LEFT => query.move_left(motion),
                key::RIGHT => query.move_right(motion),
                key::TAB => return true,
                _ => as_text = true,
            }
            if as_text {
                drop(state);
                return self.interpret(event);
            }
            // Any change to the query invalidates which row was selected.
            *selected = 0;
        }
        self.request_redraw();
        self.pump();
        true
    }

    /// Opens the find bar, seeding it from the selection when there is one.
    fn open_find(&self) {
        let seed = {
            let state = self.ivars().state.borrow();
            state
                .docs
                .active()
                .selected_text()
                .filter(|t| !t.contains('\n') && t.len() < 200)
        };
        let mut query = Buffer::new();
        if let Some(seed) = seed {
            query.insert(&seed);
            query.select_all();
        }
        self.ivars().state.borrow_mut().find = Some(FindBar {
            query,
            replacement: Buffer::new(),
            replacing: false,
            options: SearchOptions::default(),
            project: false,
            results: Vec::new(),
            selected: 0,
            result_scroll: 0,
            searching: false,
        });
        self.request_redraw();
        self.pump();
    }

    fn close_find(&self) {
        let mut state = self.ivars().state.borrow_mut();
        if let Some(cancel) = state.project_search_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        state.project_search_rx = None;
        state.find = None;
        drop(state);
        self.request_redraw();
        self.pump();
    }

    fn search_project(&self) {
        let (query, options, root) = {
            let state = self.ivars().state.borrow();
            let Some(bar) = &state.find else { return };
            (
                bar.query.rope.to_string(),
                bar.options,
                state.tree.root().map(Path::to_path_buf),
            )
        };
        let Some(root) = root else { return };
        if query.is_empty() {
            return;
        }
        if let Err(error) = search::find("", &query, "", options) {
            self.ivars().state.borrow_mut().message =
                Some((format!("invalid regex: {error}"), Instant::now()));
            return;
        }
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        std::thread::spawn(move || {
            let mut finder = Finder::new();
            finder.scan(root);
            let mut results = Vec::new();
            for i in 0..finder.len() {
                if worker_cancel.load(Ordering::Relaxed) {
                    return;
                }
                let Some(path) = finder.entry(i).map(|e| e.path.clone()) else {
                    continue;
                };
                if !std::fs::metadata(&path).is_ok_and(|m| m.len() <= 2 * 1024 * 1024) {
                    continue;
                }
                let Ok(bytes) = std::fs::read(&path) else {
                    continue;
                };
                let Ok((text, _)) = crate::text::file_format::decode(&bytes) else {
                    continue;
                };
                let matches = match search::find(&text, &query, "", options) {
                    Ok(found) => found,
                    Err(error) => {
                        let _ = tx.send(Err(error));
                        return;
                    }
                };
                for found in matches {
                    let line = text[..found.range.start]
                        .bytes()
                        .filter(|b| *b == b'\n')
                        .count();
                    let start = text[..found.range.start].rfind('\n').map_or(0, |i| i + 1);
                    let end = text[found.range.end..]
                        .find('\n')
                        .map_or(text.len(), |i| found.range.end + i);
                    results.push(ProjectHit {
                        path: path.clone(),
                        range: found.range,
                        line,
                        snippet: text[start..end].trim().chars().take(100).collect(),
                    });
                    if results.len() >= 500 {
                        break;
                    }
                }
                if results.len() >= 500 {
                    break;
                }
            }
            if !worker_cancel.load(Ordering::Relaxed) {
                let _ = tx.send(Ok(results));
            }
        });
        let mut state = self.ivars().state.borrow_mut();
        if let Some(old) = state.project_search_cancel.replace(cancel) {
            old.store(true, Ordering::Relaxed);
        }
        state.project_search_rx = Some(rx);
        if let Some(bar) = &mut state.find {
            bar.results.clear();
            bar.selected = 0;
            bar.result_scroll = 0;
            bar.searching = true;
        }
        drop(state);
        self.request_redraw();
        self.pump();
    }

    /// Sends the request under the caret of the active `.http` document,
    /// or re-sends the request behind the active response tab.
    ///
    /// The response tab opens at once, marked as sending, and is filled in
    /// when curl answers. Re-sending reuses the tab, so a request edited and
    /// sent ten times leaves one tab, not ten.
    fn send_request(&self) {
        let mut state = self.ivars().state.borrow_mut();
        if state.http.is_some() {
            state.message = Some(("a request is still in flight".into(), Instant::now()));
            drop(state);
            self.request_redraw();
            return;
        }
        let prepared = if let Some(view) = state.responses.get(&state.docs.active().id()) {
            Ok(view.request.clone())
        } else {
            let buffer = state.docs.active();
            if !buffer
                .path
                .as_deref()
                .is_some_and(crate::http::is_request_file)
            {
                state.message = Some(("Send Request works in a .http file".into(), Instant::now()));
                drop(state);
                self.request_redraw();
                return;
            }
            crate::http::prepare(
                &buffer.rope.to_string(),
                buffer.cursor(),
                buffer.path.as_deref(),
            )
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                state.message = Some((error, Instant::now()));
                drop(state);
                self.request_redraw();
                return;
            }
        };
        let title = prepared.title();
        let id = show_response(
            &mut state,
            &title,
            crate::http::curl::View::pending(prepared.clone()),
        );
        state.http = Some((id, crate::http::curl::spawn(prepared)));
        state.message = Some((format!("sending {title}"), Instant::now()));
        drop(state);
        self.resume_display_link();
        self.sync_title();
        self.reparse();
        self.request_redraw();
        self.pump();
    }

    fn poll_http(&self) {
        let mut state = self.ivars().state.borrow_mut();
        let Some((_, rx)) = &state.http else {
            return;
        };
        let outcome = match rx.try_recv() {
            Ok(outcome) => outcome,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("the request worker stopped".to_owned()),
        };
        let Some((id, _)) = state.http.take() else {
            return;
        };
        // The tab may have been closed while the request was out.
        let State {
            docs,
            responses,
            message,
            ..
        } = &mut *state;
        let Some(view) = responses.get_mut(&id) else {
            return;
        };
        view.outcome = Some(outcome);
        let note = format!("{}: {}", view.request.title(), view.status());
        let (text, ext) = view.text(view.segment);
        if let Some(buffer) = docs.iter_mut().find(|b| b.id() == id) {
            buffer.regenerate(&text);
            buffer.display_ext = ext;
        }
        *message = Some((note, Instant::now()));
        drop(state);
        self.sync_title();
        self.reparse();
        self.request_redraw();
    }

    /// Shows segment `index` of the active response tab.
    fn response_select(&self, index: usize) {
        let mut state = self.ivars().state.borrow_mut();
        let id = state.docs.active().id();
        let State {
            docs, responses, ..
        } = &mut *state;
        let Some(view) = responses.get_mut(&id) else {
            return;
        };
        let Some(hit) = Some(index).filter(|i| *i < crate::http::curl::Segment::ALL.len()) else {
            return;
        };
        let segment = crate::http::curl::Segment::ALL[hit];
        if segment == view.segment {
            return;
        }
        view.segment = segment;
        let (text, ext) = view.text(segment);
        let buffer = docs.active_mut();
        buffer.regenerate(&text);
        buffer.display_ext = ext;
        drop(state);
        self.reparse();
        self.request_redraw();
        self.pump();
    }

    fn poll_project_search(&self) {
        let mut state = self.ivars().state.borrow_mut();
        let Some(rx) = &state.project_search_rx else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("project search stopped".into()),
        };
        state.project_search_rx = None;
        state.project_search_cancel = None;
        if let Some(bar) = &mut state.find {
            bar.searching = false;
            match result {
                Ok(results) => {
                    let count = results.len();
                    bar.results = results;
                    bar.selected = 0;
                    bar.result_scroll = 0;
                    state.message = Some((
                        format!(
                            "{count} project matches{}",
                            if count == 500 { " (first 500)" } else { "" }
                        ),
                        Instant::now(),
                    ));
                }
                Err(error) => {
                    state.message = Some((format!("project search: {error}"), Instant::now()))
                }
            }
        }
        drop(state);
        self.request_redraw();
    }

    fn open_project_result(&self, index: usize) {
        let target = {
            let state = self.ivars().state.borrow();
            state
                .find
                .as_ref()
                .and_then(|bar| bar.results.get(index))
                .map(|hit| (hit.path.clone(), hit.range.clone()))
        };
        let Some((path, range)) = target else { return };
        self.load_path(&path.to_string_lossy());
        let (rows, cols) = self.grid();
        {
            let mut state = self.ivars().state.borrow_mut();
            state.docs.active_mut().select_range(range.start, range.end);
            state.docs.active_mut().scroll_to_cursor(rows, cols);
        }
        self.sync_title();
        self.reparse();
        self.request_redraw();
        self.pump();
    }

    fn find_click(&self, x: f32, y: f32, rect: Viewport) {
        // The same rectangles the bar was drawn from. This used to re-derive
        // them: `right - 180.0` here and `right - 180.0` there, option slots
        // 66pt wide in the handler and 62pt wide on screen, so a click beside
        // a chip still toggled it.
        let g = layout::FindGeometry::new(rect);
        let project = self
            .ivars()
            .state
            .borrow()
            .find
            .as_ref()
            .is_some_and(|bar| bar.project);

        for (slot, hit) in g.options.iter().enumerate() {
            if !hit.contains(x, y) {
                continue;
            }
            {
                let mut state = self.ivars().state.borrow_mut();
                if let Some(bar) = &mut state.find {
                    match slot {
                        0 => bar.options.case_sensitive = !bar.options.case_sensitive,
                        1 => bar.options.whole_word = !bar.options.whole_word,
                        2 => bar.options.regex = !bar.options.regex,
                        _ => bar.project = !bar.project,
                    }
                    bar.results.clear();
                }
            }
            self.refresh_find();
            self.request_redraw();
            self.pump();
            return;
        }

        if g.close.contains(x, y) {
            self.close_find();
        } else if g.previous.contains(x, y) || g.next.contains(x, y) {
            let forward = g.next.contains(x, y);
            if project {
                let mut state = self.ivars().state.borrow_mut();
                if let Some(bar) = &mut state.find
                    && !bar.results.is_empty()
                {
                    bar.selected = if forward {
                        (bar.selected + 1).min(bar.results.len() - 1)
                    } else {
                        bar.selected.saturating_sub(1)
                    };
                    if bar.selected < bar.result_scroll {
                        bar.result_scroll = bar.selected;
                    }
                    if bar.selected >= bar.result_scroll + 8 {
                        bar.result_scroll = bar.selected - 7;
                    }
                }
            } else {
                self.find_step(forward);
            }
        } else if g.replace_one.contains(x, y) {
            if project {
                self.search_project();
            } else {
                self.replace_one();
            }
        } else if g.replace_all.contains(x, y) {
            if project {
                self.replace_in_project();
            } else {
                self.replace_all();
            }
        } else if let Some(row) = g.result_row(y).filter(|_| project) {
            let selected = {
                let state = self.ivars().state.borrow();
                state
                    .find
                    .as_ref()
                    .filter(|bar| bar.result_scroll + row < bar.results.len())
                    .map(|bar| bar.result_scroll + row)
            };
            if let Some(index) = selected {
                self.open_project_result(index);
            }
            return;
        } else if g.find_field.contains(x, y) || g.replace_field.contains(x, y) {
            // Clicking a field focuses it and puts the caret where the
            // pointer is, measured through the same shaping that drew it.
            let replace = g.replace_field.contains(x, y);
            let mut state = self.ivars().state.borrow_mut();
            let State { find, renderer, .. } = &mut *state;
            if let Some(bar) = find {
                bar.replacing = replace;
                let box_rect = if replace {
                    g.replace_field
                } else {
                    g.find_field
                };
                let field = if replace {
                    &mut bar.replacement
                } else {
                    &mut bar.query
                };
                let text = field.rope.to_string();
                let (shown, start) = layout::ui_input_window(&text, field.cursor());
                if let Some(line) = renderer.atlas.shape_ui(&shown) {
                    let at = line.byte_at_x(x - box_rect.x - 10.0);
                    field.place_cursor(start + at, Motion::Move);
                }
            }
        }
        self.request_redraw();
        self.pump();
    }

    /// Replaces the current match, then advances to the next one.
    fn replace_one(&self) {
        let (needle, replacement, options, text, selected) = {
            let state = self.ivars().state.borrow();
            let Some(bar) = &state.find else {
                return;
            };
            (
                bar.query.rope.to_string(),
                bar.replacement.rope.to_string(),
                bar.options,
                state.docs.active().rope.to_string(),
                state.docs.active().selection(),
            )
        };
        if needle.is_empty() {
            return;
        }
        let matches = match search::find(&text, &needle, &replacement, options) {
            Ok(matches) => matches,
            Err(error) => {
                self.ivars().state.borrow_mut().message =
                    Some((format!("invalid regex: {error}"), Instant::now()));
                return;
            }
        };
        let replaced = selected
            .and_then(|range| matches.iter().find(|m| m.range == range))
            .is_some_and(|found| {
                self.ivars()
                    .state
                    .borrow_mut()
                    .docs
                    .active_mut()
                    .insert(&found.replacement);
                true
            });

        // Whether or not this one matched, move on: pressing Return in the
        // replace field should always make progress through the file.
        self.find_step(true);
        if replaced {
            self.reparse();
        }
        self.request_redraw();
        self.pump();
    }

    /// Replace All in project mode: every file in the results, searched
    /// again now so the edits fit the text as it is. Open documents are
    /// edited in place, one undo step each, and left unsaved; other files
    /// are written through the normal save path. Refused when the results
    /// were cut at 500, since files past the cut would be missed.
    fn replace_in_project(&self) {
        let (needle, replacement, options, files, matches) = {
            let state = self.ivars().state.borrow();
            let Some(bar) = &state.find else { return };
            let mut files: Vec<std::path::PathBuf> = Vec::new();
            for hit in &bar.results {
                if !files.contains(&hit.path) {
                    files.push(hit.path.clone());
                }
            }
            (
                bar.query.rope.to_string(),
                bar.replacement.rope.to_string(),
                bar.options,
                files,
                bar.results.len(),
            )
        };
        let say = |text: String| {
            self.ivars().state.borrow_mut().message = Some((text, Instant::now()));
            self.request_redraw();
        };
        if needle.is_empty() || files.is_empty() {
            return say("search the project first; Replace All uses its results".into());
        }
        if matches >= 500 {
            return say("over 500 matches: narrow the search before replacing".into());
        }
        if !self.ivars().testing {
            let mtm = MainThreadMarker::from(self);
            let alert = NSAlert::new(mtm);
            alert.setAlertStyle(NSAlertStyle::Warning);
            alert.setMessageText(&NSString::from_str(&format!(
                "Replace {matches} match{} in {} file{}?",
                if matches == 1 { "" } else { "es" },
                files.len(),
                if files.len() == 1 { "" } else { "s" }
            )));
            alert.setInformativeText(&NSString::from_str(
                "Open files are changed in their tabs and can be undone there. \
                 Files that are not open are saved to disk.",
            ));
            alert.addButtonWithTitle(&NSString::from_str("Replace"));
            alert.addButtonWithTitle(&NSString::from_str("Cancel"));
            const FIRST: isize = 1000;
            if alert.runModal() != FIRST {
                return;
            }
        }
        let (mut replaced, mut open, mut written) = (0, 0, 0);
        let mut failed = Vec::new();
        {
            let mut state = self.ivars().state.borrow_mut();
            let mut touched = Vec::new();
            for path in &files {
                let buffer = all_docs_mut(&mut state)
                    .into_iter()
                    .flat_map(|d| d.iter_mut())
                    .find(|b| b.path.as_deref().is_some_and(|p| same_file(p, path)));
                let edit = |buffer: &mut Buffer| -> Option<usize> {
                    let text = buffer.rope.to_string();
                    let found = search::find(&text, &needle, &replacement, options).ok()?;
                    let edits: Vec<_> = found
                        .into_iter()
                        .map(|m| (m.range, m.replacement))
                        .collect();
                    Some(if edits.is_empty() {
                        0
                    } else {
                        buffer.replace_ranges(&edits)
                    })
                };
                match buffer {
                    Some(buffer) => match edit(buffer) {
                        Some(0) => {}
                        Some(n) => {
                            replaced += n;
                            open += 1;
                            touched.push(buffer.id());
                        }
                        None => failed.push(path.clone()),
                    },
                    None => {
                        let done = Buffer::open(path.clone()).ok().and_then(|mut buffer| {
                            let n = edit(&mut buffer)?;
                            if n > 0 {
                                buffer.save(None).ok()?;
                            }
                            Some(n)
                        });
                        match done {
                            Some(0) => {}
                            Some(n) => {
                                replaced += n;
                                written += 1;
                            }
                            None => failed.push(path.clone()),
                        }
                    }
                }
            }
            for id in touched {
                state.lsp_dirty.insert(id, Instant::now());
            }
            if written > 0 {
                state.git.refresh();
                if let Some(indexer) = &state.indexer {
                    indexer.poke();
                }
            }
        }
        let mut note = format!(
            "replaced {replaced} in {} file{}",
            open + written,
            if open + written == 1 { "" } else { "s" }
        );
        if open > 0 {
            note.push_str(&format!(", {open} open and unsaved"));
        }
        if !failed.is_empty() {
            note.push_str(&format!(
                "; could not edit {}",
                failed
                    .iter()
                    .filter_map(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        // The list described text that is gone. Not searched again: project
        // search reads the disk, and the open files' changes are not saved.
        {
            let mut state = self.ivars().state.borrow_mut();
            if let Some(bar) = &mut state.find {
                bar.results.clear();
                bar.selected = 0;
                bar.result_scroll = 0;
            }
        }
        self.reparse();
        self.sync_title();
        say(note);
        self.pump();
    }

    /// Replaces every match in the active document.
    fn replace_all(&self) {
        let (needle, replacement, options, text) = {
            let state = self.ivars().state.borrow();
            let Some(bar) = &state.find else { return };
            (
                bar.query.rope.to_string(),
                bar.replacement.rope.to_string(),
                bar.options,
                state.docs.active().rope.to_string(),
            )
        };
        if needle.is_empty() {
            return;
        }
        let matches = match search::find(&text, &needle, &replacement, options) {
            Ok(matches) => matches,
            Err(error) => {
                self.ivars().state.borrow_mut().message =
                    Some((format!("invalid regex: {error}"), Instant::now()));
                return;
            }
        };
        let edits: Vec<_> = matches
            .into_iter()
            .map(|m| (m.range, m.replacement))
            .collect();

        let count = {
            let mut state = self.ivars().state.borrow_mut();
            let n = state.docs.active_mut().replace_ranges(&edits);
            state.message = Some((
                match n {
                    0 => format!("no matches for {needle:?}"),
                    1 => "replaced 1 occurrence".to_string(),
                    n => format!("replaced {n} occurrences"),
                },
                Instant::now(),
            ));
            n
        };
        if count > 0 {
            self.reparse();
        }
        self.request_redraw();
        self.pump();
    }

    /// Moves the cursor to the next or previous match and selects it.
    fn find_step(&self, forward: bool) {
        let (query, options, text, from, selected) = {
            let state = self.ivars().state.borrow();
            let Some(bar) = &state.find else {
                return;
            };
            (
                bar.query.rope.to_string(),
                bar.options,
                state.docs.active().rope.to_string(),
                state.docs.active().cursor(),
                state.docs.active().selection(),
            )
        };
        if query.is_empty() {
            return;
        }

        let (rows, cols) = self.grid();
        let mut state = self.ivars().state.borrow_mut();

        let matches = match search::find(&text, &query, "", options) {
            Ok(matches) => matches,
            Err(error) => {
                state.message = Some((format!("invalid regex: {error}"), Instant::now()));
                return;
            }
        };
        let found = if forward {
            let start = selected.map_or(from, |r| r.start.saturating_add(1));
            matches
                .iter()
                .find(|m| m.range.start >= start)
                .or_else(|| matches.first())
        } else {
            let before = selected.map_or(from, |r| r.start);
            matches
                .iter()
                .rev()
                .find(|m| m.range.start < before)
                .or_else(|| matches.last())
        };

        let Some(found) = found else {
            state.message = Some((format!("no match for {query:?}"), Instant::now()));
            return;
        };

        state
            .docs
            .active_mut()
            .select_range(found.range.start, found.range.end);
        state.docs.active_mut().scroll_to_cursor(rows, cols);
    }

    /// Where everything in the window is, right now. The only place that is
    /// worked out: drawing, hit-testing and scrolling all read this.
    fn chrome(&self) -> Chrome {
        // Also re-entrant: AppKit calls resetCursorRects during tracking,
        // which can land mid-edit. An empty layout for one call is invisible;
        // panicking is not.
        let Ok(state) = self.ivars().state.try_borrow() else {
            return Chrome::new(Viewport::new(0.0, 0.0), None, 0);
        };
        chrome_of(&state)
    }

    /// Text area size in whole rows and columns, which is what the buffer
    /// needs to keep the cursor on screen in both axes.
    fn grid(&self) -> (usize, usize) {
        let chrome = self.chrome();
        let state = self.ivars().state.borrow();
        let m = state.renderer.atlas.metrics;
        let gutter = layout::gutter_width(state.docs.active(), &state.renderer.atlas);
        (
            chrome.text.rows(m.line_height),
            chrome.text.columns(m.advance, gutter),
        )
    }

    /// Handles a click at `x` inside the tab bar.
    fn tab_click(&self, x: f32) {
        let (hit, active) = {
            let state = self.ivars().state.borrow();
            let hit = state
                .tab_hits
                .iter()
                .find(|h| x >= h.x0 && x < h.x1)
                .copied();
            (hit, state.docs.active_index())
        };
        // The empty part of the bar does nothing, and does not fall through
        // to the text behind it either.
        let Some(hit) = hit else {
            return;
        };

        // Only the active tab draws its close cross, so only there is that
        // corner a close button. On the others it used to be an invisible
        // one: a click near the right edge of a tab closed it.
        let on_cross = x >= hit.close_x0 && x < hit.close_x1;
        if on_cross && hit.index == active {
            self.close_tab(hit.index);
        } else {
            let mut state = self.ivars().state.borrow_mut();
            state.docs.switch(hit.index);
            state.preview = default_preview(state.docs.active());
            drop(state);
            self.sync_title();
            self.reparse();
        }
        self.request_redraw();
        self.pump();
    }

    fn move_context_tab(&self, direction: isize) {
        let mut state = self.ivars().state.borrow_mut();
        let Some(from) = state.context_tab else {
            return;
        };
        let Some(to) = from.checked_add_signed(direction) else {
            return;
        };
        if state.docs.move_tab(from, to) {
            state.context_tab = Some(to);
            drop(state);
            self.request_redraw();
            self.pump();
        }
    }

    /// Closes a tab, asking about unsaved changes first.
    fn close_tab(&self, index: usize) {
        // Closing a review is answering it.
        {
            let mut state = self.ivars().state.borrow_mut();
            let id = state.docs.iter().nth(index).map(Buffer::id);
            if let Some(id) = id
                && let Some(bridge) = state.claude.as_mut()
            {
                bridge.decide(id, false);
                bridge.reviews.remove(&id);
            }
        }
        let dirty = {
            let state = self.ivars().state.borrow();
            state.docs.iter().nth(index).is_some_and(|b| b.is_dirty())
        };
        if dirty {
            // Show the prompt against the document being closed, which means
            // switching to it first so the alert names the right file. The
            // alert spins a nested run loop, so nothing may be borrowed
            // across it.
            self.ivars().state.borrow_mut().docs.switch(index);
            self.sync_title();
            if !self.confirm_discard() {
                return;
            }
        }
        {
            let Ok(mut state) = self.ivars().state.try_borrow_mut() else {
                return;
            };
            let closed = state.docs.close(index);
            if let Some(path) = closed.as_ref().and_then(|b| b.path.clone()) {
                for server in state.lsp.values_mut() {
                    server.did_close(&path);
                }
            }
            state.completion = None;
            state.preview = default_preview(state.docs.active());
            reveal_active_tab(&mut state);
        }
        // A pane whose last tab just closed goes with it, unless it is
        // the only one.
        let emptied = {
            let state = self.ivars().state.borrow();
            state.docs.is_home() && pane_count(&state) > 1
        };
        if emptied {
            let focused = self.ivars().state.borrow().focused_pane;
            self.close_pane(focused);
            return;
        }
        self.sync_title();
        self.reparse();
        self.request_redraw();
        self.pump();
    }

    /// Gives pane `index` the keyboard.
    fn focus_pane(&self, index: usize) {
        {
            let mut state = self.ivars().state.borrow_mut();
            if index == state.focused_pane || index >= pane_count(&state) {
                return;
            }
            let all = take_panes(&mut state);
            restore_panes(&mut state, all, index);
            state.find = None;
            state.goto = None;
            state.selecting = None;
        }
        self.sync_title();
        self.reparse();
        self.request_redraw();
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
        self.pump();
    }

    /// Opens a new, empty pane to the right of the focused one and focuses
    /// it. A document lives in one pane only, so the new pane starts at
    /// Home; Cmd-P or the sidebar fills it.
    fn split_pane(&self) {
        {
            let mut state = self.ivars().state.borrow_mut();
            if pane_count(&state) >= 4 {
                state.message = Some(("four panes is the limit".into(), Instant::now()));
                drop(state);
                self.request_redraw();
                return;
            }
            let at = state.focused_pane + 1;
            let mut all = take_panes(&mut state);
            all.insert(
                at,
                PaneStore {
                    docs: Documents::new(Buffer::new()),
                    preview: None,
                    tab_scroll: 0,
                    tab_hits: Vec::new(),
                    live_line: None,
                },
            );
            restore_panes(&mut state, all, at);
            state.find = None;
            state.goto = None;
        }
        self.sync_title();
        self.reparse();
        self.request_redraw();
        self.pump();
    }

    /// Closes pane `index`, asking about its unsaved documents first.
    fn close_pane(&self, index: usize) {
        if self.ivars().state.borrow().panes.is_empty() {
            return;
        }
        self.focus_pane(index);
        // Its documents are asked about one by one, like closing tabs.
        loop {
            let dirty = {
                let state = self.ivars().state.borrow();
                state.docs.iter().position(|b| b.is_dirty())
            };
            let Some(at) = dirty else { break };
            self.ivars().state.borrow_mut().docs.switch(at);
            self.sync_title();
            if !self.confirm_discard() {
                return;
            }
            self.ivars().state.borrow_mut().docs.close(at);
        }
        {
            let mut state = self.ivars().state.borrow_mut();
            if state.panes.is_empty() {
                return;
            }
            let focused = state.focused_pane;
            let mut all = take_panes(&mut state);
            all.remove(focused);
            restore_panes(&mut state, all, focused.saturating_sub(1));
            state.find = None;
            state.goto = None;
        }
        self.sync_title();
        self.reparse();
        self.request_redraw();
        self.pump();
    }

    /// Handles a click in the sidebar: select, and toggle or open.
    fn sidebar_click(&self, y: f32, rect: Viewport) {
        let index = {
            let state = self.ivars().state.borrow();
            layout::sidebar_row_at(&state.tree, &state.renderer.atlas, rect, y)
        };
        let Some(index) = index else {
            return;
        };

        let (path, is_dir, expanded, depth) = {
            let mut state = self.ivars().state.borrow_mut();
            let Some(entry) = state.tree.select(index) else {
                return;
            };
            let result = (
                entry.path.clone(),
                entry.is_dir,
                entry.expanded,
                entry.depth,
            );
            state.tree_version += 1;
            // A folder keeps the keyboard in the tree; a file opens, and
            // the document has it.
            state.sidebar_keys = result.1;
            result
        };

        if is_dir {
            // Single click toggles a folder: a tree where you have to
            // double-click to see inside is needlessly slow to browse.
            let mut state = self.ivars().state.borrow_mut();
            if expanded {
                state.tree.toggle(index);
            } else if state.tree_children_pending.insert(path.clone()) {
                let root = state.tree.root().map(Path::to_path_buf);
                let tx = state.tree_children_tx.clone();
                std::thread::spawn(move || {
                    if let Some(root) = root {
                        let children = Tree::children(&path, depth + 1);
                        let _ = tx.send((root, path, children));
                    }
                });
            } else {
                // A second click before the worker finishes cancels expansion.
                state.tree_children_pending.remove(&path);
            }
            drop(state);
            self.resume_display_link();
        } else {
            // Opening a file now adds a tab rather than replacing what is
            // showing, so nothing has to be saved or discarded first.
            self.load_path(&path.to_string_lossy());
            self.sync_title();
            self.reparse();
        }

        self.request_redraw();
        self.pump();
    }

    /// Prompts for a directory and loads it as the project root.
    fn open_folder(&self) -> bool {
        // A test instance never shows the panel: it opens on the person's
        // real folders, and scripted keys meant for the editor would pick
        // one and index it.
        if self.ivars().testing {
            self.ivars().state.borrow_mut().message =
                Some(("would show the Open Folder panel".into(), Instant::now()));
            return true;
        }
        let mtm = MainThreadMarker::from(self);
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseFiles(false);
        panel.setCanChooseDirectories(true);
        panel.setAllowsMultipleSelection(false);
        const MODAL_RESPONSE_OK: isize = 1;
        if panel.runModal() != MODAL_RESPONSE_OK {
            return false;
        }
        let Some(url) = panel.URL() else {
            return false;
        };
        let Some(path) = url.path() else {
            return false;
        };
        self.load_folder_path(&path.to_string());
        true
    }

    fn load_folder_path(&self, path: &str) {
        let mut state = self.ivars().state.borrow_mut();
        state.tree.set_root(path);
        let recent = std::mem::take(&mut state.recent_projects);
        state.recent_projects = with_recent(recent, state.tree.root());
        state.git = crate::platform::git_panel::Panel::new(path.into());
        state.tree_version = 0;
        state.tree_children_pending.clear();
        state.finder = Finder::new();
        state.project_index_rx = Some(spawn_project_index(path.into()));
        state.sidebar = true;
        // A project is open now, which is worth coming back to.
        state.ephemeral_session = false;
        state.message = Some(("Folder opened".to_string(), Instant::now()));
        state.lsp.clear();
        state.lsp_unavailable.clear();
        state.completion = None;
        drop(state);
        self.watch_project();
        self.lsp_sync_open();
        self.request_redraw();
        self.pump();
        self.resume_display_link();
    }

    fn poll_project_index(&self) {
        let Ok(mut state) = self.ivars().state.try_borrow_mut() else {
            return;
        };
        let Some(rx) = &state.project_index_rx else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                state.project_index_rx = None;
                return;
            }
        };
        state.project_index_rx = None;
        let ProjectIndexResult {
            root,
            mut tree,
            finder,
            tree_version,
        } = result;
        if state.tree.root() != Some(root.as_path()) {
            return;
        }
        if state.tree_version != tree_version {
            state.finder = finder;
            state.project_index_rx = Some(spawn_project_refresh(
                state.tree.clone(),
                state.tree_version,
            ));
            return;
        }
        if let Some(path) = state.docs.active().path.as_deref() {
            tree.reveal(path);
        }
        // The worker's tree carries the ignore set from when it started;
        // keep the newer one, then ask Git again, since whatever changed
        // the tree may have changed what is ignored.
        tree.set_ignored(state.tree.ignored_set());
        state.tree = tree;
        state.finder = finder;
        state.tree_version += 1;
        state.ignored_rx = Some(spawn_ignored(root));
        drop(state);
        self.request_redraw();
        self.resume_display_link();
    }

    /// Takes Git's answer about ignored paths, if it is for this root.
    fn poll_ignored(&self) {
        let Ok(mut state) = self.ivars().state.try_borrow_mut() else {
            return;
        };
        let Some(rx) = &state.ignored_rx else {
            return;
        };
        let (root, ignored) = match rx.try_recv() {
            Ok(answer) => answer,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                state.ignored_rx = None;
                return;
            }
        };
        state.ignored_rx = None;
        if state.tree.root() == Some(root.as_path()) {
            state.tree.set_ignored(std::sync::Arc::new(ignored));
            drop(state);
            self.request_redraw();
        }
    }

    fn poll_tree_children(&self) {
        let Ok(mut state) = self.ivars().state.try_borrow_mut() else {
            return;
        };
        let mut changed = false;
        while let Ok((root, path, children)) = state.tree_children_rx.try_recv() {
            if state.tree.root() != Some(root.as_path())
                || !state.tree_children_pending.remove(&path)
            {
                continue;
            }
            if state.tree.install_children(&path, children) {
                state.tree_version += 1;
                changed = true;
            }
        }
        drop(state);
        if changed {
            self.request_redraw();
        }
    }

    // ---- terminal -------------------------------------------------------

    /// Whether keys go to the terminal: it is open with the keyboard, and
    /// no field (palette, find, go to line) has taken it.
    fn terminal_has_keys(&self) -> bool {
        let state = self.ivars().state.borrow();
        state.terminal.has_keys() && !field_has_keys(&state)
    }

    fn terminal_wake(&self) -> crate::term::pty::Wake {
        let pointer = ViewPointer(self as *const EditorView);
        Box::new(move || {
            let pointer = &pointer;
            // The view outlives every session; see `ViewPointer`.
            unsafe {
                crate::platform::dispatch::on_main(
                    pointer.0 as *mut std::ffi::c_void,
                    terminal_wake_on_main,
                )
            };
        })
    }

    /// A session printed something, or its program exited.
    fn poll_terminal(&self) {
        let Ok(state) = self.ivars().state.try_borrow() else {
            self.resume_display_link();
            return;
        };
        let mut changed = false;
        for tab in &state.terminal.tabs {
            changed |= tab.session.drain_wake();
        }
        let exited = state
            .terminal
            .tabs
            .iter()
            .any(|tab| tab.session.has_exited());
        drop(state);
        // A program that exits takes its tab with it, and the last tab
        // takes the panel, as in any terminal.
        if exited {
            let mut state = self.ivars().state.borrow_mut();
            while let Some(index) = state
                .terminal
                .tabs
                .iter()
                .position(|tab| tab.session.has_exited())
            {
                state.terminal.close_tab(index);
            }
            drop(state);
            self.after_terminal_layout();
            return;
        }
        if changed {
            self.request_redraw();
            self.pump();
        }
    }

    /// Shows the panel with the keyboard: on the Claude Code session when
    /// `claude`, started if there is none, or on the current session,
    /// started if there is none.
    fn open_terminal(&self, claude: bool) {
        let existing = {
            let mut state = self.ivars().state.borrow_mut();
            state.terminal.open = true;
            state.terminal.focus = true;
            state.sidebar_keys = false;
            let found = if claude {
                state.terminal.tabs.iter().position(|tab| tab.claude)
            } else {
                (!state.terminal.tabs.is_empty()).then_some(state.terminal.active)
            };
            if let Some(index) = found {
                state.terminal.active = index;
            }
            found
        };
        if existing.is_none() {
            self.spawn_terminal(claude);
        }
        self.after_terminal_layout();
    }

    /// What a new session runs: the login shell, or `claude` through it so
    /// it finds what the shell profile puts on PATH. Claude Code is told
    /// this window's IDE port, so it connects without `/ide`.
    fn terminal_launch(&self, claude: bool) -> crate::platform::terminal::Launch {
        let state = self.ivars().state.borrow();
        let shell = std::env::var("CRC_TERMINAL_SHELL")
            .or_else(|_| std::env::var("SHELL"))
            .unwrap_or_else(|_| "/bin/zsh".into());
        let home = std::env::var_os("HOME").map_or_else(|| "/".into(), std::path::PathBuf::from);
        let cwd = state.tree.root().map_or(home, Path::to_path_buf);
        let mut env = vec![
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("COLORTERM".to_owned(), "truecolor".to_owned()),
            ("TERM_PROGRAM".to_owned(), "crc".to_owned()),
            (
                "TERM_PROGRAM_VERSION".to_owned(),
                env!("CARGO_PKG_VERSION").to_owned(),
            ),
        ];
        // Every session knows this window's IDE port, so `claude` typed in
        // any of them connects without `/ide`.
        if let Some(bridge) = &state.claude {
            env.push(("CLAUDE_CODE_SSE_PORT".to_owned(), bridge.port().to_string()));
            env.push(("ENABLE_IDE_INTEGRATION".to_owned(), "true".to_owned()));
        }
        let args = if claude {
            let command = std::env::var("CRC_CLAUDE_COMMAND").unwrap_or_else(|_| "claude".into());
            vec!["-l".to_owned(), "-i".to_owned(), "-c".to_owned(), command]
        } else {
            vec!["-l".to_owned()]
        };
        // A Claude Code session that started this editor (from its own
        // terminal, say) left its markers in our environment. Passed on,
        // they make the `claude` here think it is that session's child.
        let unset = std::env::vars_os()
            .filter_map(|(name, _)| name.into_string().ok())
            .filter(|name| name == "CLAUDECODE" || name.starts_with("CLAUDE_CODE_"))
            .collect();
        crate::platform::terminal::Launch {
            program: shell.into(),
            args,
            cwd,
            env,
            unset,
        }
    }

    /// The panel's grid, for the window as it is now.
    fn terminal_grid(&self) -> (usize, usize) {
        let chrome = self.chrome();
        let state = self.ivars().state.borrow();
        chrome.terminal.map_or((80, 24), |rect| {
            let (_, screen) = crate::platform::terminal::split(rect);
            crate::platform::terminal::grid_size(&state.renderer.atlas, screen)
        })
    }

    fn spawn_terminal(&self, claude: bool) {
        {
            let mut state = self.ivars().state.borrow_mut();
            state.terminal.open = true;
            state.terminal.focus = true;
        }
        let launch = self.terminal_launch(claude);
        let (cols, rows) = self.terminal_grid();
        let args: Vec<&str> = launch.args.iter().map(String::as_str).collect();
        let env: Vec<(&str, &str)> = launch
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let unset: Vec<&str> = launch.unset.iter().map(String::as_str).collect();
        let spawned = crate::term::pty::Session::spawn(
            &launch.program,
            &args,
            &launch.cwd,
            &env,
            &unset,
            cols,
            rows,
            self.terminal_wake(),
        );
        let mut state = self.ivars().state.borrow_mut();
        match spawned {
            Ok(session) => {
                let title = if claude {
                    "✻ Claude".to_owned()
                } else {
                    launch
                        .program
                        .file_name()
                        .map_or_else(|| "shell".into(), |n| n.to_string_lossy().into_owned())
                };
                state.terminal.tabs.push(crate::platform::terminal::Tab {
                    session,
                    title,
                    claude,
                    launch,
                });
                state.terminal.active = state.terminal.tabs.len() - 1;
                state.terminal.back = 0;
            }
            Err(e) => {
                state.message = Some((
                    format!("could not start {}: {e}", launch.program.display()),
                    Instant::now(),
                ));
            }
        }
        drop(state);
        self.after_terminal_layout();
    }

    /// The panel appeared, went, or changed sessions: the editor column
    /// changed size, and so did the cursor rectangles.
    fn after_terminal_layout(&self) {
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
        self.request_redraw();
        self.pump();
    }

    /// Keeps every session's grid the size of the panel.
    fn terminal_after_frame(&self) {
        let Ok(state) = self.ivars().state.try_borrow() else {
            return;
        };
        if !state.terminal.open || state.terminal.tabs.is_empty() {
            return;
        }
        let Some(rect) = chrome_of(&state).terminal else {
            return;
        };
        let (_, screen) = crate::platform::terminal::split(rect);
        let (cols, rows) = crate::platform::terminal::grid_size(&state.renderer.atlas, screen);
        for tab in &state.terminal.tabs {
            tab.session.resize(cols, rows);
        }
    }

    fn terminal_write(&self, bytes: &[u8]) {
        let mut state = self.ivars().state.borrow_mut();
        state.terminal.back = 0;
        state.terminal.selection = None;
        if let Some(tab) = state.terminal.active_tab_mut() {
            tab.session.write(bytes);
        }
    }

    /// The active session's screen rect and grid size, for mapping points.
    fn terminal_screen(&self) -> Option<(Viewport, usize, usize)> {
        let rect = self.chrome().terminal?;
        let (_, screen) = crate::platform::terminal::split(rect);
        let state = self.ivars().state.borrow();
        let tab = state.terminal.active_tab()?;
        let term = tab.session.term.lock().unwrap_or_else(|e| e.into_inner());
        Some((screen, term.cols(), term.rows()))
    }

    /// A press in the terminal's screen: starts a selection, selects a word
    /// or path on a double click and the line on a triple, and with Command
    /// opens the file reference under the pointer.
    fn terminal_press(&self, event: &NSEvent, x: f32, y: f32) {
        let Some((screen, cols, rows)) = self.terminal_screen() else {
            return;
        };
        let command = event
            .modifierFlags()
            .contains(NSEventModifierFlags::Command);
        let clicks = event.clickCount();
        let target = {
            let mut state = self.ivars().state.borrow_mut();
            let (row, boundary, cell) =
                crate::platform::terminal::point(&state.renderer.atlas, screen, x, y, cols, rows);
            let back = state.terminal.back;
            let Some(tab) = state.terminal.active_tab() else {
                return;
            };
            let term = tab.session.term.lock().unwrap_or_else(|e| e.into_inner());
            let line = term.view_line(row, back);
            if command {
                let chars = term.line_chars(line);
                let index = chars.iter().rposition(|(col, _)| *col <= cell).unwrap_or(0);
                let text: Vec<char> = chars.iter().map(|(_, c)| *c).collect();
                let cwd = tab.launch.cwd.clone();
                drop(term);
                crate::term::path_at(&text, index).map(|found| (found, cwd))
            } else {
                let selection = match clicks {
                    2 => term
                        .word_at(line, cell)
                        .map(|(a, b)| ((line, a), (line, b))),
                    n if n >= 3 => Some(((line, 0), (line, cols))),
                    _ => Some(((line, boundary), (line, boundary))),
                };
                drop(term);
                state.terminal.selection = selection;
                state.terminal.selecting = clicks < 2;
                None
            }
        };
        if let Some(((path, line, column), cwd)) = target {
            self.open_reference(&path, line, column, &cwd);
        }
        self.request_redraw();
        self.pump();
    }

    fn terminal_drag(&self, x: f32, y: f32) {
        let Some((screen, cols, rows)) = self.terminal_screen() else {
            return;
        };
        {
            let mut state = self.ivars().state.borrow_mut();
            let (row, boundary, _) =
                crate::platform::terminal::point(&state.renderer.atlas, screen, x, y, cols, rows);
            let back = state.terminal.back;
            let line = state.terminal.active_tab().map(|tab| {
                tab.session
                    .term
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .view_line(row, back)
            });
            if let (Some(line), Some((_, head))) = (line, state.terminal.selection.as_mut()) {
                *head = (line, boundary);
            }
        }
        self.request_redraw();
        self.pump();
    }

    fn terminal_selected_text(&self) -> Option<String> {
        let state = self.ivars().state.borrow();
        let (a, b) = state.terminal.selection?;
        let tab = state.terminal.active_tab()?;
        let text = tab
            .session
            .term
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .text_between(a, b);
        (!text.is_empty()).then_some(text)
    }

    /// Opens a file named in the terminal, at its line and column when it
    /// gave them. Relative paths are tried against the session's folder,
    /// then the project's.
    /// Git > Switch Branch: the palette lists the local branches, and a
    /// name that is not one offers to create it.
    fn open_branch_picker(&self) {
        let root = self
            .ivars()
            .state
            .borrow()
            .git
            .root()
            .map(Path::to_path_buf);
        let Some(root) = root else {
            self.ivars().state.borrow_mut().message =
                Some(("not a Git repository".into(), Instant::now()));
            self.request_redraw();
            return;
        };
        let branches = match crate::project::git::branches(&root) {
            Ok(list) => list,
            Err(error) => {
                self.ivars().state.borrow_mut().message = Some((error, Instant::now()));
                self.request_redraw();
                return;
            }
        };
        self.open_palette_with("");
        self.ivars().state.borrow_mut().branch_list = Some(branches);
        self.request_redraw();
        self.pump();
    }

    fn switch_branch(&self, name: String, create: bool) {
        let mut state = self.ivars().state.borrow_mut();
        let current = state.git.branch();
        if !create && name == current {
            state.message = Some((format!("already on {name}"), Instant::now()));
        } else if create {
            state.git.create_branch(name);
        } else {
            state.git.switch_branch(name);
        }
        drop(state);
        self.resume_display_link();
        self.request_redraw();
    }

    fn git_remote(&self, what: crate::project::git::Remote) {
        let mut state = self.ivars().state.borrow_mut();
        if state.git.root().is_none() {
            state.message = Some(("not a Git repository".into(), Instant::now()));
        } else {
            let sock = state.ssh_auth_sock.clone();
            state.git.remote(what, sock);
            state.message = Some((format!("{}…", what.verb()), Instant::now()));
        }
        drop(state);
        self.resume_display_link();
        self.request_redraw();
    }

    /// Blame for the caret's line, once the caret has rested on it. Runs
    /// from the display link; the answer lands in `blame`.
    fn blame_refresh(&self) {
        const REST: Duration = Duration::from_millis(400);
        let Ok(mut state) = self.ivars().state.try_borrow_mut() else {
            return;
        };
        if let Some(rx) = &state.blame_rx {
            match rx.try_recv() {
                Ok((id, line, text)) => {
                    state.blame_rx = None;
                    state.blame = Some((id, line, text));
                    self.ivars().needs_redraw.set(true);
                }
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => state.blame_rx = None,
            }
        }
        let buffer = state.docs.active();
        let (id, line) = (buffer.id(), buffer.cursor_position().0);
        if state
            .blame
            .as_ref()
            .is_some_and(|(i, l, _)| (*i, *l) == (id, line))
        {
            return;
        }
        match state.blame_want {
            Some((i, l, at)) if (i, l) == (id, line) => {
                if at.elapsed() < REST {
                    return;
                }
            }
            _ => {
                state.blame_want = Some((id, line, Instant::now()));
                if state.blame.take().is_some() {
                    self.ivars().needs_redraw.set(true);
                }
                return;
            }
        }
        state.blame_want = None;
        // Only files Git knows: their HEAD text was found for the gutter.
        let tracked = state
            .gutter
            .get(&id)
            .is_some_and(|g| g.head.as_ref().is_some_and(|h| !h.is_empty()));
        let buffer = state.docs.active();
        let (Some(root), Some(path)) =
            (state.git.root().map(Path::to_path_buf), buffer.path.clone())
        else {
            return;
        };
        if !tracked || buffer.rope.len_bytes() > GUTTER_MAX_BYTES {
            return;
        }
        let text = buffer.rope.to_string();
        let (tx, rx) = mpsc::channel();
        state.blame_rx = Some(rx);
        std::thread::spawn(move || {
            let relative = path.strip_prefix(&root).unwrap_or(&path).to_path_buf();
            let note = match crate::project::git::blame_line(&root, &relative, line, &text) {
                Ok(blame) => match blame.author {
                    None => "not committed yet".to_string(),
                    Some(author) => {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| d.as_secs() as i64);
                        format!(
                            "{author}, {}: {}",
                            crate::project::git::ago(blame.time, now),
                            blame.summary
                        )
                    }
                },
                Err(_) => String::new(),
            };
            let _ = tx.send((id, line, note));
        });
    }

    /// Folds or opens the block `line` heads: a chevron click.
    fn toggle_fold(&self, line: Option<usize>) {
        let (rows, cols) = self.grid();
        let mut state = self.ivars().state.borrow_mut();
        let buffer = state.docs.active_mut();
        let line = line.unwrap_or_else(|| buffer.cursor_position().0);
        if !buffer.unfold(line) {
            buffer.fold(line);
        }
        buffer.clamp_scroll(rows, cols);
        drop(state);
        self.request_redraw();
        self.pump();
    }

    /// View > Fold (`Some(true)`), Unfold (`Some(false)`) at the caret, or
    /// Fold All (`None`). Fold at a line that does not head a block folds
    /// the block the caret is in.
    fn fold_command(&self, fold: Option<bool>) {
        let (rows, cols) = self.grid();
        let mut state = self.ivars().state.borrow_mut();
        let buffer = state.docs.active_mut();
        let line = buffer.cursor_position().0;
        let done = match fold {
            None => {
                if !buffer.fold_all() {
                    state.message = Some((
                        "Fold All works on files up to 100,000 lines".into(),
                        Instant::now(),
                    ));
                    drop(state);
                    self.request_redraw();
                    return;
                }
                true
            }
            Some(false) => buffer.unfold(line),
            Some(true) => {
                // The nearest line above, or this one, that heads a block
                // containing the caret.
                let head = (0..=line).rev().take(2000).find(|&l| {
                    buffer
                        .fold_range(l)
                        .is_some_and(|(_, b)| l == line || b >= line)
                });
                head.is_some_and(|l| buffer.fold(l))
            }
        };
        if !done {
            state.message = Some(("nothing to fold here".into(), Instant::now()));
        }
        let buffer = state.docs.active_mut();
        buffer.clamp_scroll(rows, cols);
        buffer.scroll_to_cursor(rows, cols);
        drop(state);
        self.request_redraw();
        self.pump();
    }

    /// View > Word Wrap: flips wrapping for the active document.
    fn toggle_word_wrap(&self) {
        let (rows, cols) = self.grid();
        let mut state = self.ivars().state.borrow_mut();
        let setting = state.word_wrap;
        let buffer = state.docs.active_mut();
        let on = buffer.wrap.is_none();
        buffer.wrap_choice = Some(on);
        apply_wrap(buffer, setting, cols);
        buffer.scroll_column = 0;
        buffer.scroll_to_cursor(rows, cols);
        state.message = Some((
            if on { "word wrap on" } else { "word wrap off" }.into(),
            Instant::now(),
        ));
        drop(state);
        self.request_redraw();
        self.pump();
    }

    /// Sends a request to the server for the active document, about the
    /// caret. Changes typed so far go first, so it answers about the text
    /// on screen. Says so in the status line when there is no server.
    fn ask_server(
        &self,
        ask: impl FnOnce(&mut crate::lsp::client::Server, &Path, crate::lsp::Position),
    ) -> bool {
        self.lsp_flush_now();
        let mut state = self.ivars().state.borrow_mut();
        let buffer = state.docs.active();
        let (Some(path), Some(language)) = (buffer.path.clone(), lsp_language(buffer)) else {
            state.message = Some(("no language server for this file".into(), Instant::now()));
            return false;
        };
        let at = crate::lsp::position_of(&buffer.rope, buffer.cursor());
        let key = crate::lsp::servers::server_key(language);
        match state
            .lsp
            .get_mut(&key)
            .filter(|s| s.is_ready() && s.knows(&path))
        {
            Some(server) => {
                ask(server, &path, at);
                true
            }
            None => {
                state.message = Some(("no language server for this file".into(), Instant::now()));
                false
            }
        }
    }

    fn find_references(&self) {
        if self.ask_server(|server, path, at| {
            server.references(path, at);
        }) {
            self.ivars().state.borrow_mut().message =
                Some(("finding references…".into(), Instant::now()));
        }
        self.request_redraw();
    }

    /// The references as project search results: the find bar in project
    /// mode, the name as its query, one row per place.
    fn show_references(&self, locations: Vec<crate::lsp::Location>) {
        let mut state = self.ivars().state.borrow_mut();
        if locations.is_empty() {
            state.message = Some(("no references found".into(), Instant::now()));
            drop(state);
            self.request_redraw();
            return;
        }
        let mut texts: HashMap<std::path::PathBuf, Option<crate::text::rope::Rope>> =
            HashMap::new();
        let mut results = Vec::new();
        for location in &locations {
            let rope = texts.entry(location.path.clone()).or_insert_with(|| {
                all_docs(&state)
                    .flat_map(|d| d.iter())
                    .find(|b| {
                        b.path
                            .as_deref()
                            .is_some_and(|p| same_file(p, &location.path))
                    })
                    .map(|b| b.rope.clone())
                    .or_else(|| {
                        let bytes = std::fs::read(&location.path).ok()?;
                        let (text, _) = crate::text::file_format::decode(&bytes).ok()?;
                        Some(crate::text::rope::Rope::from_text(&text))
                    })
            });
            let Some(rope) = rope else {
                continue;
            };
            let start = crate::lsp::offset_of(rope, location.start);
            let end = crate::lsp::offset_of(rope, location.end).max(start);
            let line = rope.byte_to_line(start);
            let line_start = rope.line_to_byte(line);
            let line_end = if line + 1 < rope.len_lines() {
                rope.line_to_byte(line + 1)
            } else {
                rope.len_bytes()
            };
            results.push(ProjectHit {
                path: location.path.clone(),
                range: start..end,
                line,
                snippet: rope
                    .slice_to_string(line_start..line_end)
                    .trim()
                    .chars()
                    .take(100)
                    .collect(),
            });
        }
        let buffer = state.docs.active();
        let name = buffer
            .rope
            .slice_to_string(buffer.word_range_at(buffer.cursor()));
        let mut query = Buffer::new();
        query.insert(&name);
        let count = results.len();
        state.find = Some(FindBar {
            query,
            replacement: Buffer::new(),
            replacing: false,
            options: SearchOptions {
                case_sensitive: true,
                whole_word: true,
                regex: false,
            },
            project: true,
            results,
            selected: 0,
            result_scroll: 0,
            searching: false,
        });
        state.message = Some((
            format!("{count} reference{}", if count == 1 { "" } else { "s" }),
            Instant::now(),
        ));
        drop(state);
        self.request_redraw();
        self.pump();
    }

    /// F2: the rename field in the status line, holding the current name.
    fn start_rename(&self) {
        self.lsp_flush_now();
        let mut state = self.ivars().state.borrow_mut();
        let buffer = state.docs.active();
        let (Some(path), Some(language)) = (buffer.path.clone(), lsp_language(buffer)) else {
            state.message = Some(("no language server for this file".into(), Instant::now()));
            return;
        };
        let word = buffer.word_range_at(buffer.cursor());
        let name = buffer.rope.slice_to_string(word.clone());
        if name.trim().is_empty() {
            state.message = Some(("nothing to rename here".into(), Instant::now()));
            return;
        }
        let at = crate::lsp::position_of(&buffer.rope, word.start);
        let mut field = Buffer::new();
        field.insert(&name);
        field.select_all();
        state.rename = Some(RenameField {
            field,
            path,
            at,
            language,
        });
        drop(state);
        self.request_redraw();
        self.pump();
    }

    fn handle_rename_key(&self, event: &NSEvent) -> bool {
        const ESCAPE: u16 = 53;
        if event
            .modifierFlags()
            .contains(NSEventModifierFlags::Command)
        {
            return false;
        }
        let code = match event.keyCode() {
            key::KEYPAD_ENTER => key::RETURN,
            code => code,
        };
        match code {
            ESCAPE => self.ivars().state.borrow_mut().rename = None,
            key::RETURN => {
                let mut state = self.ivars().state.borrow_mut();
                let Some(rename) = state.rename.take() else {
                    return true;
                };
                let name = rename.field.rope.to_string().trim().to_owned();
                let key = crate::lsp::servers::server_key(rename.language);
                let sent = !name.is_empty()
                    && state
                        .lsp
                        .get_mut(&key)
                        .filter(|s| s.is_ready())
                        .map(|server| server.rename(&rename.path, rename.at, &name))
                        .is_some();
                state.message = Some((
                    if sent {
                        format!("renaming to {name}…")
                    } else {
                        "rename not sent".into()
                    },
                    Instant::now(),
                ));
            }
            key::DELETE => {
                let mut state = self.ivars().state.borrow_mut();
                if let Some(rename) = &mut state.rename {
                    rename.field.backspace();
                }
            }
            key::LEFT | key::RIGHT => {
                let shift = event.modifierFlags().contains(NSEventModifierFlags::Shift);
                let motion = if shift { Motion::Extend } else { Motion::Move };
                let mut state = self.ivars().state.borrow_mut();
                if let Some(rename) = &mut state.rename {
                    if code == key::LEFT {
                        rename.field.move_left(motion);
                    } else {
                        rename.field.move_right(motion);
                    }
                }
            }
            _ => return self.interpret(event),
        }
        self.request_redraw();
        self.pump();
        true
    }

    /// A rename's edits. Open documents are edited in place, one undo step
    /// each, and left unsaved; files that are not open are edited on disk
    /// through the same save path as everything else.
    fn apply_rename(&self, files: Vec<(std::path::PathBuf, Vec<crate::lsp::TextEdit>)>) {
        let (rows, cols) = self.grid();
        let mut open = 0;
        let mut written = 0;
        let mut failed = Vec::new();
        {
            let mut state = self.ivars().state.borrow_mut();
            let mut touched = Vec::new();
            for (path, edits) in &files {
                if edits.is_empty() {
                    continue;
                }
                let buffer = all_docs_mut(&mut state)
                    .into_iter()
                    .flat_map(|d| d.iter_mut())
                    .find(|b| b.path.as_deref().is_some_and(|p| same_file(p, path)));
                match buffer {
                    Some(buffer) => match crate::lsp::edit_ranges(&buffer.rope, edits) {
                        Some(ranges) if buffer.replace_ranges(&ranges) > 0 => {
                            touched.push(buffer.id());
                            open += 1;
                        }
                        _ => failed.push(path.clone()),
                    },
                    None => {
                        let done = Buffer::open(path.clone()).ok().and_then(|mut buffer| {
                            let ranges = crate::lsp::edit_ranges(&buffer.rope, edits)?;
                            (buffer.replace_ranges(&ranges) > 0).then_some(())?;
                            buffer.save(None).ok()
                        });
                        match done {
                            Some(()) => written += 1,
                            None => failed.push(path.clone()),
                        }
                    }
                }
            }
            for id in touched {
                state
                    .lsp_dirty
                    .insert(id, Instant::now() - Duration::from_secs(1));
            }
            state.docs.active_mut().scroll_to_cursor(rows, cols);
            state.message = Some((
                if files.is_empty() {
                    "the server had nothing to rename".into()
                } else if failed.is_empty() {
                    format!(
                        "renamed in {} file{}{}",
                        open + written,
                        if open + written == 1 { "" } else { "s" },
                        if open > 0 {
                            format!(", {open} open and unsaved")
                        } else {
                            String::new()
                        }
                    )
                } else {
                    format!(
                        "renamed in {} files; could not edit {}",
                        open + written,
                        failed
                            .iter()
                            .filter_map(|p| p.file_name())
                            .map(|n| n.to_string_lossy().into_owned())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                },
                Instant::now(),
            ));
            if written > 0 {
                state.git.refresh();
                if let Some(indexer) = &state.indexer {
                    indexer.poke();
                }
            }
        }
        self.lsp_flush_changes();
        self.reparse();
        self.sync_title();
        self.request_redraw();
        self.pump();
    }

    /// Asks the server to format the active document. `save` when a save
    /// asked, which saves again once the edits are in.
    fn format_document(&self, save: bool) {
        let (spaces, tab) = {
            let state = self.ivars().state.borrow();
            let buffer = state.docs.active();
            match buffer.indent_style {
                Some(style) => (!style.tabs, style.width as u32),
                None => indent_style(&buffer.rope),
            }
        };
        let snapshot = self.ivars().state.borrow().docs.active().rope.clone();
        let mut asked = None;
        let sent = self.ask_server(|server, path, _| {
            if server.formats() {
                server.formatting(path, tab, spaces, save);
                asked = Some(path.to_path_buf());
            }
        });
        let mut state = self.ivars().state.borrow_mut();
        match asked {
            Some(path) => state.formatting = Some((path, snapshot)),
            None if sent && !save => {
                state.message = Some((
                    "this language server does not format".into(),
                    Instant::now(),
                ))
            }
            None => {}
        }
    }

    fn apply_format(&self, path: &Path, edits: &[crate::lsp::TextEdit], save: bool) {
        let (rows, cols) = self.grid();
        let mut state = self.ivars().state.borrow_mut();
        let Some((asked, snapshot)) = state.formatting.take() else {
            return;
        };
        let buffer = state.docs.active_mut();
        let here = asked == path && buffer.path.as_deref() == Some(path);
        if !here || buffer.rope.to_string() != snapshot.to_string() {
            state.message = Some(("format skipped: the text changed".into(), Instant::now()));
            return;
        }
        let changed = match crate::lsp::edit_ranges(&buffer.rope, edits) {
            Some(ranges) if !ranges.is_empty() => buffer.replace_ranges(&ranges) > 0,
            _ => false,
        };
        buffer.scroll_to_cursor(rows, cols);
        let id = buffer.id();
        if changed {
            state.lsp_dirty.insert(id, Instant::now());
        }
        state.message = Some((
            if changed {
                "formatted"
            } else {
                "already formatted"
            }
            .into(),
            Instant::now(),
        ));
        drop(state);
        if changed {
            self.after_edit();
            if save {
                self.ivars().state.borrow_mut().saving_formatted = true;
                self.save(false);
                self.ivars().state.borrow_mut().saving_formatted = false;
            }
        }
        self.request_redraw();
        self.pump();
    }

    fn request_signature(&self) {
        self.ask_server(|server, path, at| {
            server.signature_help(path, at);
        });
    }

    /// Jumps to a palette symbol: its line in the active document, or opens
    /// its file first.
    fn go_to_symbol(&self, path: Option<std::path::PathBuf>, line: u32) {
        if let Some(path) = path {
            let cwd = path.parent().map(Path::to_path_buf).unwrap_or_default();
            self.open_reference(&path.to_string_lossy(), Some(line as usize + 1), None, &cwd);
            self.ivars().state.borrow_mut().tree.reveal(&path);
        } else {
            let (rows, cols) = self.grid();
            let mut state = self.ivars().state.borrow_mut();
            let buffer = state.docs.active_mut();
            buffer.goto_line(line as usize);
            buffer.scroll_to_cursor(rows, cols);
        }
        self.request_redraw();
        self.pump();
    }

    fn open_reference(&self, path: &str, line: Option<usize>, column: Option<usize>, cwd: &Path) {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let root = self
            .ivars()
            .state
            .borrow()
            .tree
            .root()
            .map(Path::to_path_buf);
        let given = Path::new(path);
        let candidates: Vec<std::path::PathBuf> = if given.is_absolute() {
            vec![given.to_path_buf()]
        } else if let (Some(rest), Some(home)) = (path.strip_prefix("~/"), home) {
            vec![home.join(rest)]
        } else {
            std::iter::once(cwd.join(given))
                .chain(root.map(|root| root.join(given)))
                .collect()
        };
        let Some(found) = candidates.into_iter().find(|p| p.is_file()) else {
            self.ivars().state.borrow_mut().message =
                Some((format!("no file {path}"), Instant::now()));
            self.resume_display_link();
            return;
        };
        if !self.load_path(&found.to_string_lossy()) {
            return;
        }
        if let Some(line) = line {
            let (rows, cols) = self.grid();
            let mut state = self.ivars().state.borrow_mut();
            let buffer = state.docs.active_mut();
            buffer.goto_line(line - 1);
            if let Some(column) = column {
                let start = buffer.cursor();
                let text = buffer
                    .rope
                    .slice_to_string(start..buffer.rope.len_bytes().min(start + 4096));
                let offset: usize = text
                    .chars()
                    .take_while(|c| *c != '\n')
                    .take(column - 1)
                    .map(char::len_utf8)
                    .sum();
                buffer.select_range(start + offset, start + offset);
            }
            buffer.scroll_to_cursor(rows, cols);
        }
        // The editor has the file now, so it has the keys too.
        self.ivars().state.borrow_mut().terminal.focus = false;
        self.sync_title();
        self.reparse();
    }

    /// A key while the terminal has the keyboard. Always handled: anything
    /// unhandled would otherwise reach the text system and the document.
    fn terminal_key(&self, event: &NSEvent) -> bool {
        let flags = event.modifierFlags();
        let code = event.keyCode();
        if flags.contains(NSEventModifierFlags::Command) {
            // The Mac line-editing shortcuts, as the shell's own keys.
            let bytes: &[u8] = match code {
                key::DELETE => b"\x15",
                key::LEFT => b"\x01",
                key::RIGHT => b"\x05",
                _ => return true,
            };
            self.terminal_write(bytes);
            return true;
        }
        let text = |s: Option<Retained<NSString>>| s.map(|s| s.to_string()).unwrap_or_default();
        let key = crate::term::keys::Key {
            code,
            chars: text(event.characters()),
            bare: text(event.charactersIgnoringModifiers()),
            shift: flags.contains(NSEventModifierFlags::Shift),
            control: flags.contains(NSEventModifierFlags::Control),
            option: flags.contains(NSEventModifierFlags::Option),
        };
        let app_cursor = self
            .ivars()
            .state
            .borrow()
            .terminal
            .active_tab()
            .is_some_and(|tab| {
                tab.session
                    .term
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .modes
                    .app_cursor
            });
        if let Some(bytes) = crate::term::keys::encode(&key, app_cursor) {
            self.terminal_write(&bytes);
        }
        true
    }

    // ---- Claude Code ----------------------------------------------------

    /// The wake-up the IDE server's threads use: a main-thread poll.
    fn claude_wake(&self) -> crate::ide::ws::Wake {
        let pointer = ViewPointer(self as *const EditorView);
        Box::new(move || {
            let pointer = &pointer;
            // The view outlives the server; see `ViewPointer`.
            unsafe {
                crate::platform::dispatch::on_main(
                    pointer.0 as *mut std::ffi::c_void,
                    claude_wake_on_main,
                )
            };
        })
    }

    /// After every frame: the bridge follows the project root, and a moved
    /// selection is noted so Claude hears about it once it settles. A
    /// comparison or two when nothing changed, which keeps it off the
    /// typing budget.
    fn claude_after_frame(&self) {
        let Ok(mut state) = self.ivars().state.try_borrow_mut() else {
            return;
        };
        let root = state.tree.root().map(Path::to_path_buf);
        if let Some(new_root) = root.as_ref().filter(|_| state.claude_tried != root) {
            let new_root = new_root.clone();
            state.claude_tried = root;
            drop(state);
            self.claude_follow_root(&new_root);
            state = match self.ivars().state.try_borrow_mut() {
                Ok(state) => state,
                Err(_) => return,
            };
        }
        let buffer = state.docs.active();
        let range = buffer
            .selection()
            .unwrap_or(buffer.cursor()..buffer.cursor());
        let key = (buffer.id(), range.start, range.end);
        let Some(bridge) = state.claude.as_mut() else {
            return;
        };
        if bridge.selection_seen == Some(key) {
            return;
        }
        bridge.selection_seen = Some(key);
        if !bridge.is_connected() {
            return;
        }
        bridge.selection_changed_at = Some(Instant::now());
        drop(state);
        self.resume_display_link();
    }

    /// Starts the bridge for `root`, or points the running one at it.
    fn claude_follow_root(&self, root: &Path) {
        if std::env::var_os("CRC_NO_CLAUDE").is_some() {
            return;
        }
        let mut state = self.ivars().state.borrow_mut();
        let result = match state.claude.as_mut() {
            Some(bridge) if bridge.serves(root) => Ok(()),
            Some(bridge) => bridge.set_root(root),
            None => crate::platform::claude::Bridge::start(root, self.claude_wake())
                .map(|bridge| state.claude = Some(bridge)),
        };
        if let Err(e) = result {
            state.message = Some((format!("Claude Code cannot connect: {e}"), Instant::now()));
            drop(state);
            self.resume_display_link();
        }
    }

    /// Sends the selection once it has been still for 100 ms, if it is not
    /// what Claude already has.
    fn claude_flush_selection(&self) {
        let Ok(mut state) = self.ivars().state.try_borrow_mut() else {
            return;
        };
        let due = state
            .claude
            .as_ref()
            .and_then(|c| c.selection_changed_at)
            .is_some_and(|at| at.elapsed() >= Duration::from_millis(100));
        if !due {
            return;
        }
        let selection = claude_selection(&state);
        let Some(bridge) = state.claude.as_mut() else {
            return;
        };
        bridge.selection_changed_at = None;
        if let Some(selection) = selection
            && bridge.is_connected()
            && bridge.selection_sent.as_ref() != Some(&selection)
        {
            bridge.send(&crate::ide::mcp::selection_changed(&selection));
            bridge.selection_sent = Some(selection);
        }
    }

    /// Handles what the IDE server's threads delivered.
    fn poll_claude(&self) {
        use crate::ide::ws::Event;
        loop {
            let event = {
                let Ok(mut state) = self.ivars().state.try_borrow_mut() else {
                    // Busy: the next wake or frame tries again.
                    self.resume_display_link();
                    return;
                };
                match state.claude.as_mut() {
                    Some(bridge) => bridge.next_event(),
                    None => None,
                }
            };
            let Some(event) = event else {
                break;
            };
            match event {
                Event::Connected(_) => self.claude_note("Claude Code connected"),
                Event::Closed(_) => {
                    let gone = self
                        .ivars()
                        .state
                        .borrow()
                        .claude
                        .as_ref()
                        .is_some_and(|c| !c.is_connected());
                    if gone {
                        self.claude_note("Claude Code disconnected");
                    }
                }
                Event::Text(id, text) => {
                    let current = self
                        .ivars()
                        .state
                        .borrow()
                        .claude
                        .as_ref()
                        .is_some_and(|c| c.is_current(id));
                    if !current {
                        continue;
                    }
                    let reply = crate::ide::mcp::handle(&text, &mut ClaudeHost(self));
                    if let Some(reply) = reply {
                        let state = self.ivars().state.borrow();
                        if let Some(bridge) = &state.claude {
                            bridge.send(&reply);
                        }
                    }
                }
            }
        }
        self.request_redraw();
        self.pump();
    }

    /// On the way out: Claude hears no to anything still waiting, and the
    /// lock file goes with the bridge so `claude` stops offering this window.
    fn claude_shutdown(&self) {
        let bridge = self.ivars().state.borrow_mut().claude.take();
        if let Some(mut bridge) = bridge {
            bridge.reject_all();
        }
    }

    fn claude_note(&self, text: &str) {
        self.ivars().state.borrow_mut().message = Some((text.to_owned(), Instant::now()));
        self.resume_display_link();
    }

    /// Answers the review in the active tab.
    fn claude_decide(&self, accept: bool) {
        let answered = {
            let mut state = self.ivars().state.borrow_mut();
            let id = state.docs.active().id();
            state
                .claude
                .as_mut()
                .is_some_and(|bridge| bridge.decide(id, accept))
        };
        if !answered {
            return;
        }
        self.claude_note(if accept {
            "Accepted. Claude writes the file."
        } else {
            "Rejected. The file is unchanged."
        });
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
        self.request_redraw();
        self.pump();
    }

    /// Keys in a review tab. `None` to handle the key as usual.
    fn handle_review_key(&self, event: &NSEvent) -> Option<bool> {
        const ESCAPE: u16 = 53;
        let flags = event.modifierFlags();
        let command = flags.contains(NSEventModifierFlags::Command);
        let control = flags.contains(NSEventModifierFlags::Control);
        let text = self.chrome().text;
        let page = ((text.height / crate::platform::git_panel::DIFF_LINE) as isize - 2).max(1);
        let delta = match event.keyCode() {
            key::RETURN | key::KEYPAD_ENTER if command => {
                self.claude_decide(true);
                return Some(true);
            }
            ESCAPE => {
                self.claude_decide(false);
                return Some(true);
            }
            key::UP => -1,
            key::DOWN => 1,
            key::PAGE_UP => -page,
            key::PAGE_DOWN => page,
            key::HOME => isize::MIN / 2,
            key::END => isize::MAX / 2,
            // Shortcuts keep working: closing the tab, switching tabs.
            _ if command || control => return None,
            // Anything else would type into Claude's text.
            _ => return Some(true),
        };
        {
            let mut state = self.ivars().state.borrow_mut();
            let id = state.docs.active().id();
            if let Some(review) = state.claude.as_mut().and_then(|c| c.reviews.get_mut(&id)) {
                review.scroll_by(delta, text);
            }
        }
        self.request_redraw();
        self.pump();
        Some(true)
    }

    /// Closes the tab of the review `id`, wherever it is.
    fn claude_close_review(&self, id: u64) {
        let focused = {
            let mut state = self.ivars().state.borrow_mut();
            if let Some(bridge) = state.claude.as_mut() {
                bridge.decide(id, false);
                bridge.reviews.remove(&id);
            }
            let focused = state.docs.iter().position(|b| b.id() == id);
            if focused.is_none() {
                for pane in &mut state.panes {
                    let found = pane.docs.iter().position(|b| b.id() == id);
                    if let Some(index) = found {
                        if pane.docs.len() > 1 {
                            pane.docs.close(index);
                        }
                        break;
                    }
                }
            }
            focused
        };
        if let Some(index) = focused {
            self.close_tab(index);
        }
        self.request_redraw();
        self.pump();
    }

    /// The wake-up a server's reader thread uses: a main-thread poll.
    fn lsp_wake(&self) -> crate::lsp::transport::Wake {
        let pointer = ViewPointer(self as *const EditorView);
        Box::new(move || {
            let pointer = &pointer;
            // The view outlives every server; see `ViewPointer`.
            unsafe {
                crate::platform::dispatch::on_main(
                    pointer.0 as *mut std::ffi::c_void,
                    lsp_wake_on_main,
                )
            };
        })
    }

    /// Starts servers for the languages of open documents and tells each
    /// ready server about the documents it does not know yet. Safe to call
    /// often; it only sends what is new.
    fn lsp_sync_open(&self) {
        let root = self
            .ivars()
            .state
            .borrow()
            .tree
            .root()
            .map(Path::to_path_buf);
        let Some(root) = root else {
            return;
        };
        // Which languages are open, across panes.
        let mut wanted: Vec<(Language, std::path::PathBuf, u64)> = Vec::new();
        {
            let state = self.ivars().state.borrow();
            for docs in all_docs(&state) {
                for buffer in docs.iter() {
                    if let Some(path) = &buffer.path
                        && let Some(language) = lsp_language(buffer)
                    {
                        wanted.push((language, path.clone(), buffer.id()));
                    }
                }
            }
        }
        for (language, _, _) in &wanted {
            let key = crate::lsp::servers::server_key(*language);
            let known = {
                let state = self.ivars().state.borrow();
                state.lsp.contains_key(&key) || state.lsp_unavailable.contains_key(&key)
            };
            if known {
                continue;
            }
            match crate::lsp::servers::launch_for(*language) {
                Ok(launch) => {
                    let args: Vec<&str> = launch.args.iter().map(String::as_str).collect();
                    let started = crate::lsp::client::Server::start(
                        &launch.name,
                        &launch.program,
                        &args,
                        &root,
                        self.lsp_wake(),
                    );
                    let mut state = self.ivars().state.borrow_mut();
                    match started {
                        Ok(server) => {
                            state.message =
                                Some((format!("{} starting", launch.name), Instant::now()));
                            state.lsp.insert(key, server);
                        }
                        Err(error) => {
                            let reason = format!("{} failed to start: {error}", launch.name);
                            state.message = Some((reason.clone(), Instant::now()));
                            state.lsp_unavailable.insert(key, reason);
                        }
                    }
                }
                Err(reason) => {
                    let mut state = self.ivars().state.borrow_mut();
                    // Said once per language, in the status line, and then
                    // the editor is simply an editor for that file.
                    state.message = Some((reason.clone(), Instant::now()));
                    state.lsp_unavailable.insert(key, reason);
                }
            }
        }
        // Open documents in ready servers.
        let mut state = self.ivars().state.borrow_mut();
        let State {
            lsp, docs, panes, ..
        } = &mut *state;
        for (language, path, id) in wanted {
            let key = crate::lsp::servers::server_key(language);
            let Some(server) = lsp.get_mut(&key) else {
                continue;
            };
            if !server.is_ready() || server.knows(&path) {
                continue;
            }
            let text = std::iter::once(&*docs)
                .chain(panes.iter().map(|p| &p.docs))
                .flat_map(|d| d.iter())
                .find(|b| b.id() == id)
                .map(|b| b.rope.to_string());
            if let Some(text) = text {
                server.did_open(&path, crate::lsp::servers::language_id(language), &text);
            }
        }
        drop(state);
        self.request_redraw();
    }

    /// Sends the text of buffers that changed once typing has paused.
    /// Keeps the Git gutter marks of open files current: fetches the HEAD
    /// text of the active file when it has none yet, and re-diffs files
    /// edited since their marks were computed, once typing pauses.
    fn gutter_refresh(&self) {
        const PAUSE: Duration = Duration::from_millis(200);
        let Ok(mut state) = self.ivars().state.try_borrow_mut() else {
            return;
        };
        // Fetched HEAD texts landing.
        while let Ok((id, head)) = state.gutter_channel.1.try_recv() {
            state.gutter_pending.remove(&id);
            let has_head = head.is_some();
            state.gutter.insert(
                id,
                GutterState {
                    head,
                    marks: Vec::new(),
                },
            );
            if has_head {
                state.gutter_dirty.insert(id, Instant::now() - PAUSE);
            }
            self.ivars().needs_redraw.set(true);
        }
        // The active file, if it has never been looked at.
        {
            let active = state.docs.active();
            let id = active.id();
            if let Some(path) = active.path.clone()
                && !active.is_preview_file()
                && active.rope.len_bytes() <= GUTTER_MAX_BYTES
                && !state.gutter.contains_key(&id)
                && !state.gutter_pending.contains(&id)
            {
                state.gutter_pending.insert(id);
                let tx = state.gutter_channel.0.clone();
                std::thread::spawn(move || {
                    let head = path
                        .parent()
                        .ok_or_else(|| "no parent directory".to_string())
                        .and_then(crate::project::git::toplevel)
                        .and_then(|root| crate::project::git::head_text(&root, &path))
                        .ok()
                        .map(|text| text.unwrap_or_default());
                    let _ = tx.send((id, head));
                });
            }
        }
        // Files edited a moment ago.
        let due: Vec<u64> = state
            .gutter_dirty
            .iter()
            .filter(|(_, at)| at.elapsed() >= PAUSE)
            .map(|(id, _)| *id)
            .collect();
        if due.is_empty() {
            return;
        }
        let State {
            gutter,
            gutter_dirty,
            docs,
            panes,
            ..
        } = &mut *state;
        for id in due {
            gutter_dirty.remove(&id);
            let Some(entry) = gutter.get_mut(&id) else {
                continue;
            };
            let Some(head) = &entry.head else {
                continue;
            };
            let buffer = std::iter::once(&*docs)
                .chain(panes.iter().map(|p| &p.docs))
                .flat_map(|d| d.iter())
                .find(|b| b.id() == id);
            let Some(buffer) = buffer else {
                continue;
            };
            if buffer.rope.len_bytes() > GUTTER_MAX_BYTES {
                entry.marks.clear();
                continue;
            }
            let text = buffer.rope.to_string();
            entry.marks = crate::project::git::marks(&crate::ide::diff::diff(head, &text));
            self.ivars().needs_redraw.set(true);
        }
    }

    fn lsp_flush_changes(&self) {
        const PAUSE: Duration = Duration::from_millis(150);
        let Ok(mut state) = self.ivars().state.try_borrow_mut() else {
            return;
        };
        if state.lsp_dirty.is_empty() {
            return;
        }
        let due: Vec<u64> = state
            .lsp_dirty
            .iter()
            .filter(|(_, at)| at.elapsed() >= PAUSE)
            .map(|(id, _)| *id)
            .collect();
        if due.is_empty() {
            return;
        }
        let State {
            lsp,
            lsp_dirty,
            docs,
            panes,
            ..
        } = &mut *state;
        for id in due {
            lsp_dirty.remove(&id);
            let buffer = std::iter::once(&*docs)
                .chain(panes.iter().map(|p| &p.docs))
                .flat_map(|d| d.iter())
                .find(|b| b.id() == id);
            let Some(buffer) = buffer else {
                continue;
            };
            let (Some(path), Some(language)) = (&buffer.path, lsp_language(buffer)) else {
                continue;
            };
            if let Some(server) = lsp.get_mut(&crate::lsp::servers::server_key(language))
                && server.is_ready()
            {
                if server.knows(path) {
                    server.did_change(path, &buffer.rope.to_string());
                } else {
                    server.did_open(
                        path,
                        crate::lsp::servers::language_id(language),
                        &buffer.rope.to_string(),
                    );
                }
            }
        }
    }

    /// Main thread, whenever a server has something to say.
    fn poll_lsp(&self) {
        let Ok(mut state) = self.ivars().state.try_borrow_mut() else {
            // Busy: the display link will try again.
            self.resume_display_link();
            return;
        };
        let mut events = Vec::new();
        for server in state.lsp.values_mut() {
            events.extend(server.poll());
        }
        drop(state);
        let mut sync = false;
        for event in events {
            use crate::lsp::client::Event;
            match event {
                Event::Ready => sync = true,
                Event::Diagnostics(_) => {}
                Event::Completions { items, request, .. } => {
                    let mut state = self.ivars().state.borrow_mut();
                    let caret = state.docs.active().cursor();
                    let id = state.docs.active().id();
                    let prefix = state.completion.as_ref().and_then(|popup| {
                        (popup.request == request && popup.buffer == id && caret >= popup.anchor)
                            .then(|| {
                                state
                                    .docs
                                    .active()
                                    .rope
                                    .slice_to_string(popup.anchor..caret)
                            })
                    });
                    if let (Some(prefix), Some(popup)) = (prefix, state.completion.as_mut()) {
                        popup.items = items;
                        popup.refilter(&prefix);
                    }
                }
                Event::Definition(locations) => {
                    let Some(location) = locations.into_iter().next() else {
                        self.ivars().state.borrow_mut().message =
                            Some(("no definition found".into(), Instant::now()));
                        continue;
                    };
                    self.load_path(&location.path.to_string_lossy());
                    let (rows, cols) = self.grid();
                    let mut state = self.ivars().state.borrow_mut();
                    let buffer = state.docs.active_mut();
                    if buffer.path.as_deref() == Some(location.path.as_path()) {
                        let offset = crate::lsp::offset_of(&buffer.rope, location.start);
                        buffer.place_cursor(offset, Motion::Move);
                        buffer.scroll_to_cursor(rows, cols);
                    }
                    drop(state);
                    self.sync_title();
                    self.reparse();
                }
                Event::Hover { text, .. } => {
                    let first: String = text.lines().take(2).collect::<Vec<_>>().join("  ");
                    self.ivars().state.borrow_mut().message = Some((first, Instant::now()));
                }
                Event::References(locations) => self.show_references(locations),
                Event::Rename(files) => self.apply_rename(files),
                Event::Formatting { path, edits, save } => self.apply_format(&path, &edits, save),
                Event::Signature { path, signature } => {
                    let mut state = self.ivars().state.borrow_mut();
                    let buffer = state.docs.active();
                    let here = buffer.path.as_deref() == Some(path.as_path());
                    let (id, caret) = (buffer.id(), buffer.cursor());
                    state.signature = signature.filter(|_| here).map(|signature| SignatureTip {
                        buffer: id,
                        anchor: caret,
                        signature,
                    });
                }
                Event::Refused(reason) => {
                    self.ivars().state.borrow_mut().message = Some((reason, Instant::now()));
                }
                Event::Failed(reason) => {
                    self.ivars().state.borrow_mut().message = Some((reason, Instant::now()));
                }
            }
        }
        if sync {
            self.lsp_sync_open();
        }
        self.request_redraw();
        self.pump();
    }

    /// After any key reached the document: remember the edit for the
    /// server, and keep the completion list honest about the caret.
    fn lsp_after_key(&self, edited: bool) {
        let mut state = self.ivars().state.borrow_mut();
        let id = state.docs.active().id();
        if let Some(tip) = &state.signature {
            let buffer = state.docs.active();
            let caret = buffer.cursor();
            let line = |at: usize| buffer.rope.byte_to_line(at.min(buffer.rope.len_bytes()));
            if tip.buffer != id || caret < tip.anchor || line(caret) != line(tip.anchor) {
                state.signature = None;
            }
        }
        if edited && state.docs.active().path.is_some() {
            state.lsp_dirty.insert(id, Instant::now());
        }
        let caret = state.docs.active().cursor();
        let prefix = state.completion.as_ref().and_then(|popup| {
            (popup.buffer == id && caret >= popup.anchor).then(|| {
                state
                    .docs
                    .active()
                    .rope
                    .slice_to_string(popup.anchor..caret)
            })
        });
        let still = match prefix {
            Some(prefix) if !prefix.contains(char::is_whitespace) && !prefix.contains('/') => {
                if let Some(popup) = &mut state.completion {
                    popup.refilter(&prefix);
                }
                true
            }
            _ => {
                state.completion = None;
                false
            }
        };
        drop(state);
        if edited {
            // A shorter prefix can match more than the worker was asked
            // about; ask again. Typing forward is asked by `lsp_typed`.
            if still {
                self.ask_worker();
            }
            self.resume_display_link();
        }
    }

    /// After text was typed into the document: ask for completions when
    /// the character is part of a word, one the server asked to hear, or
    /// the text before the caret has become a path.
    fn lsp_typed(&self, text: &str) {
        let mut chars = text.chars();
        let (Some(ch), None) = (chars.next(), chars.next()) else {
            return;
        };
        let (trigger, path, signature) = {
            let state = self.ivars().state.borrow();
            let buffer = state.docs.active();
            let server = lsp_server_for(&state, buffer);
            let trigger = server.is_some_and(|s| s.trigger_characters.iter().any(|t| t == text));
            let signature =
                server.is_some_and(|s| s.signature_triggers().iter().any(|t| t == text));
            (trigger, completion_path_query(&state).is_some(), signature)
        };
        if signature {
            self.request_signature();
        } else if ch == ')' {
            self.ivars().state.borrow_mut().signature = None;
        }
        if crate::complete::is_word_char(ch) || trigger || path {
            self.request_completion(false);
        } else {
            self.ivars().state.borrow_mut().completion = None;
        }
    }

    /// Asks for completions at the caret: the server when the file has one,
    /// and the worker for words, the project index, paths and history.
    /// `manual` asks even when nothing has been typed yet.
    fn request_completion(&self, manual: bool) {
        // Whatever was typed goes to the server first, so it answers about
        // the text as it is now.
        self.lsp_flush_now();
        let mut state = self.ivars().state.borrow_mut();
        let path = completion_path_query(&state);
        let buffer = state.docs.active();
        let caret = buffer.cursor();
        let id = buffer.id();
        let line_start = buffer.rope.line_to_byte(buffer.rope.byte_to_line(caret));
        let before = buffer.rope.slice_to_string(line_start..caret);
        let anchor = match &path {
            Some(query) => caret - query.partial.len(),
            None => caret - crate::complete::word_len(&before),
        };
        let prefix = buffer.rope.slice_to_string(anchor..caret);
        let context = crate::complete::context_before(&before[..anchor - line_start]);
        let language = completion_language(buffer);
        // A word needs two letters before it is worth a list of its own;
        // a path, a server trigger or a request by hand needs none.
        let worth = manual || path.is_some() || prefix.chars().count() >= 2;

        // The server, for words.
        let mut request = 0;
        if path.is_none()
            && let (Some(file), Some(lang)) = (buffer.path.clone(), lsp_language(buffer))
        {
            let at = crate::lsp::position_of(&buffer.rope, caret);
            let key = crate::lsp::servers::server_key(lang);
            if let Some(server) = state
                .lsp
                .get_mut(&key)
                .filter(|s| s.is_ready() && s.knows(&file))
            {
                request = server.completion(&file, at);
            }
        }
        if request == 0 && !worth {
            state.completion = None;
            return;
        }
        let previous = state
            .completion
            .take()
            .filter(|p| p.buffer == id && p.anchor == anchor && p.path == path.is_some());
        let (items, local, boosts, selected, chosen) = match previous {
            Some(p) => (p.items, p.local, p.boosts, p.selected, p.chosen),
            None => (Vec::new(), Vec::new(), Vec::new(), 0, false),
        };
        let mut popup = CompletionPopup {
            buffer: id,
            anchor,
            request,
            generation: 0,
            items,
            local,
            boosts,
            shown: Vec::new(),
            selected,
            chosen,
            path: path.is_some(),
            context,
            language,
        };
        popup.refilter(&prefix);
        state.completion = Some(popup);
        drop(state);
        if worth {
            self.ask_worker();
        }
    }

    /// Sends the worker the current question for the open list.
    fn ask_worker(&self) {
        self.start_completer();
        let mut state = self.ivars().state.borrow_mut();
        let path = completion_path_query(&state);
        let root = project_key(&state);
        let buffer = state.docs.active();
        let caret = buffer.cursor();
        let Some(popup) = state
            .completion
            .as_ref()
            .filter(|p| p.buffer == buffer.id() && caret >= p.anchor)
        else {
            return;
        };
        let (anchor, context, language) =
            (popup.anchor, popup.context.clone(), popup.language.clone());
        let query = crate::complete::worker::Query {
            generation: state.completion_generation + 1,
            rope: buffer.rope.clone(),
            anchor,
            caret,
            prefix: buffer.rope.slice_to_string(anchor..caret),
            context,
            root,
            language,
            path,
        };
        state.completion_generation += 1;
        let generation = state.completion_generation;
        if let Some(popup) = state.completion.as_mut() {
            popup.generation = generation;
        }
        if let Some(worker) = &state.completer {
            worker.ask(query);
        }
    }

    fn start_completer(&self) {
        if self.ivars().state.borrow().completer.is_some() {
            return;
        }
        let pointer = ViewPointer(self as *const EditorView);
        let wake: Box<dyn Fn() + Send> = Box::new(move || {
            let pointer = &pointer;
            // The view outlives the worker; see `ViewPointer`.
            unsafe {
                crate::platform::dispatch::on_main(
                    pointer.0 as *mut std::ffi::c_void,
                    completion_wake_on_main,
                )
            };
        });
        let worker = crate::complete::worker::Worker::start(
            crate::complete::history::History::default_path(),
            wake,
        );
        self.ivars().state.borrow_mut().completer = worker;
    }

    /// Main thread, when the worker has answered.
    fn poll_completion(&self) {
        let Ok(mut state) = self.ivars().state.try_borrow_mut() else {
            self.resume_display_link();
            return;
        };
        if let Some(rows) = state.completer.as_ref().and_then(|w| w.forgotten()) {
            state.message = Some((
                format!(
                    "forgot {rows} remembered completion{} for this project",
                    if rows == 1 { "" } else { "s" }
                ),
                Instant::now(),
            ));
        }
        let Some(answer) = state.completer.as_ref().and_then(|w| w.take()) else {
            drop(state);
            self.request_redraw();
            self.pump();
            return;
        };
        let caret = state.docs.active().cursor();
        let id = state.docs.active().id();
        let prefix = state
            .completion
            .as_ref()
            .filter(|p| p.buffer == id && p.generation == answer.generation && caret >= p.anchor)
            .map(|p| state.docs.active().rope.slice_to_string(p.anchor..caret));
        if let (Some(prefix), Some(popup)) = (prefix, state.completion.as_mut()) {
            popup.local = answer.candidates;
            popup.boosts = answer.boosts;
            popup.refilter(&prefix);
        }
        drop(state);
        self.request_redraw();
        self.pump();
    }

    /// Sends pending changes without waiting for the pause, so a request
    /// that follows sees the current text.
    fn lsp_flush_now(&self) {
        {
            let mut state = self.ivars().state.borrow_mut();
            for at in state.lsp_dirty.values_mut() {
                *at = Instant::now() - Duration::from_secs(1);
            }
        }
        self.lsp_flush_changes();
    }

    fn handle_completion_key(&self, event: &NSEvent) -> bool {
        const ESCAPE: u16 = 53;
        // Option-1 to Option-9 take that suggestion, by key code: the
        // character Option makes depends on the keyboard layout.
        const DIGITS: [u16; 9] = [18, 19, 20, 21, 23, 22, 26, 28, 25];
        let flags = event.modifierFlags();
        if flags.contains(NSEventModifierFlags::Command) {
            return false;
        }
        let code = match event.keyCode() {
            key::KEYPAD_ENTER => key::RETURN,
            code => code,
        };
        let (count, chosen) = self
            .ivars()
            .state
            .borrow()
            .completion
            .as_ref()
            .map_or((0, false), |p| (p.shown.len(), p.chosen));
        if flags.contains(NSEventModifierFlags::Option)
            && let Some(n) = DIGITS.iter().position(|&d| d == code)
        {
            if n >= count {
                return false;
            }
            self.accept_completion(Some(n));
            self.request_redraw();
            self.pump();
            return true;
        }
        match code {
            ESCAPE => {
                self.ivars().state.borrow_mut().completion = None;
            }
            key::UP | key::DOWN if count > 0 => {
                let mut state = self.ivars().state.borrow_mut();
                if let Some(popup) = &mut state.completion {
                    popup.selected = if code == key::DOWN {
                        (popup.selected + 1) % count
                    } else {
                        (popup.selected + count - 1) % count
                    };
                    popup.chosen = true;
                }
            }
            key::TAB if count > 0 => self.accept_completion(None),
            key::RETURN if count > 0 && chosen => self.accept_completion(None),
            key::RETURN => {
                // Not chosen: a new line, as it would be with no list.
                self.ivars().state.borrow_mut().completion = None;
                return false;
            }
            _ => return false,
        }
        self.request_redraw();
        self.pump();
        true
    }

    /// Replaces the prefix with the chosen suggestion (`index`, or the
    /// selected one), remembers the choice, and in a path goes on into a
    /// folder that was just completed.
    fn accept_completion(&self, index: Option<usize>) {
        let (range, text, remember, into_folder) = {
            let mut state = self.ivars().state.borrow_mut();
            let Some(popup) = state.completion.take() else {
                return;
            };
            let Some(candidate) = popup.shown.get(index.unwrap_or(popup.selected)).cloned() else {
                return;
            };
            let buffer = state.docs.active();
            let caret = buffer.cursor();
            let (range, text) = match candidate.server.and_then(|i| popup.items.get(i)) {
                Some(item) => (
                    crate::lsp::client::completion_range(item, &buffer.rope, caret),
                    crate::lsp::client::completion_text(item).to_owned(),
                ),
                None => (popup.anchor..caret, candidate.insert.clone()),
            };
            let remember = project_key(&state).map(|root| {
                (
                    root,
                    popup.language,
                    popup.context,
                    candidate.insert.clone(),
                )
            });
            (
                range,
                text,
                remember,
                popup.path && candidate.insert.ends_with('/'),
            )
        };
        {
            let mut state = self.ivars().state.borrow_mut();
            let buffer = state.docs.active_mut();
            buffer.select_range(range.start, range.end);
            buffer.insert(&text);
        }
        self.after_edit();
        self.ivars().state.borrow_mut().completion = None;
        if let Some((root, language, context, text)) = remember
            && let Some(worker) = &self.ivars().state.borrow().completer
        {
            worker.accepted(root, language, context, text);
        }
        if into_folder {
            self.request_completion(false);
        }
    }

    /// Edit > Forget Completion History: this project's remembered picks.
    fn forget_completion_history(&self) {
        self.start_completer();
        let state = self.ivars().state.borrow();
        if let (Some(root), Some(worker)) = (project_key(&state), &state.completer) {
            worker.forget(root);
        }
    }

    /// Asks where the symbol at `offset` (or the caret) is defined.
    fn goto_definition(&self, offset: Option<usize>) {
        let mut state = self.ivars().state.borrow_mut();
        let buffer = state.docs.active();
        let (Some(path), Some(language)) = (buffer.path.clone(), lsp_language(buffer)) else {
            return;
        };
        let at = crate::lsp::position_of(&buffer.rope, offset.unwrap_or(buffer.cursor()));
        let key = crate::lsp::servers::server_key(language);
        match state
            .lsp
            .get_mut(&key)
            .filter(|s| s.is_ready() && s.knows(&path))
        {
            Some(server) => {
                server.definition(&path, at);
            }
            None => {
                state.message = Some(("no language server for this file".into(), Instant::now()));
            }
        }
        drop(state);
        self.lsp_flush_now();
        self.request_redraw();
    }

    fn show_hover(&self) {
        let mut state = self.ivars().state.borrow_mut();
        let buffer = state.docs.active();
        let (Some(path), Some(language)) = (buffer.path.clone(), lsp_language(buffer)) else {
            return;
        };
        let at = crate::lsp::position_of(&buffer.rope, buffer.cursor());
        let key = crate::lsp::servers::server_key(language);
        if let Some(server) = state
            .lsp
            .get_mut(&key)
            .filter(|s| s.is_ready() && s.knows(&path))
        {
            server.hover(&path, at);
        }
        drop(state);
        self.lsp_flush_now();
    }

    /// Starts, or restarts, the FSEvents watcher on the current root.
    fn watch_project(&self) {
        let root = self
            .ivars()
            .state
            .borrow()
            .tree
            .root()
            .map(Path::to_path_buf);
        let mut state = self.ivars().state.borrow_mut();
        state.watcher = None;
        let Some(root) = root else {
            state.indexer = None;
            return;
        };
        if state.indexer.as_ref().is_none_or(|i| i.root != root) {
            state.indexer = crate::index::store::Indexer::start(root.clone());
        }
        // The view lives for the process; the display link holds it the
        // same way. The handler only ever runs on the main thread.
        let view = self as *const EditorView;
        state.watcher = crate::project::watch::Watcher::new(
            &root,
            Box::new(move |change| {
                let view = unsafe { &*view };
                view.project_changed(change);
            }),
        );
    }

    /// Main thread, from the watcher: something outside the editor touched
    /// the project.
    fn project_changed(&self, change: crate::project::watch::Change) {
        {
            let mut state = self.ivars().state.borrow_mut();
            match change {
                crate::project::watch::Change::Tree => {
                    state.project_changed_at = Some(Instant::now());
                }
                crate::project::watch::Change::Git => {
                    // The panel's own operations change the repository too;
                    // those it already re-read.
                    if state.git_open && !state.git.busy() && !state.git.settled_recently() {
                        state.git_changed_at = Some(Instant::now());
                    }
                }
            }
        }
        self.resume_display_link();
    }

    /// Rebuilds the tree and finder once the watcher has been quiet for a
    /// moment, and never while a rebuild is already running.
    fn refresh_after_watch(&self) {
        const SETTLE: Duration = Duration::from_millis(300);
        let Ok(mut state) = self.ivars().state.try_borrow_mut() else {
            return;
        };
        if let Some(at) = state.git_changed_at
            && at.elapsed() >= SETTLE
        {
            state.git_changed_at = None;
            state.gutter.clear();
            if state.git_open && !state.git.busy() && !state.git.settled_recently() {
                state.git.refresh();
            }
        }
        let Some(at) = state.project_changed_at else {
            return;
        };
        if at.elapsed() < SETTLE || state.project_index_rx.is_some() {
            return;
        }
        state.project_changed_at = None;
        if let Some(indexer) = &state.indexer {
            indexer.poke();
        }
        state.tree_version += 1;
        state.tree_children_pending.clear();
        state.project_index_rx = Some(spawn_project_refresh(
            state.tree.clone(),
            state.tree_version,
        ));
        if state.git_open {
            state.git.refresh();
        }
        drop(state);
        self.check_open_files();
        self.resume_display_link();
    }

    fn refresh_project_after_disk_change(&self) {
        let mut state = self.ivars().state.borrow_mut();
        state.tree_version += 1;
        state.tree_children_pending.clear();
        state.project_index_rx = Some(spawn_project_refresh(
            state.tree.clone(),
            state.tree_version,
        ));
        state.git.refresh();
        drop(state);
        self.resume_display_link();
    }

    /// Creates a new file next to the current selection and opens it.
    ///
    /// Uses the standard save panel rather than an inline text field: it
    /// already handles naming, overwrite confirmation, and picking a
    /// different folder, none of which exist here yet.
    /// New File: a name field in the tree when a project is open, the save
    /// panel when there is no tree to put one in.
    fn new_file(&self) -> bool {
        if self.ivars().state.borrow().tree.root().is_some() {
            self.start_sidebar_edit(SidebarEditKind::NewFile);
            return true;
        }
        self.new_file_with_panel()
    }

    fn new_folder(&self) -> bool {
        if self.ivars().state.borrow().tree.root().is_none() {
            return false;
        }
        self.start_sidebar_edit(SidebarEditKind::NewFolder);
        true
    }

    /// Opens a name field in the sidebar for `kind`, at the row where the
    /// item will appear.
    fn start_sidebar_edit(&self, kind: SidebarEditKind) {
        // Source control shares the column; the field belongs to the tree.
        if self.ivars().state.borrow().git_open {
            self.set_sidebar_view(false);
        }
        let mut state = self.ivars().state.borrow_mut();
        state.sidebar = true;
        state.palette = None;
        state.goto = None;
        let Some(root) = state.tree.root().map(Path::to_path_buf) else {
            return;
        };
        let (parent, row, depth, mut field) = match &kind {
            SidebarEditKind::Rename(path) => {
                let Some(index) = state.tree.rows().iter().position(|e| &e.path == path) else {
                    return;
                };
                let entry = &state.tree.rows()[index];
                let name = entry.name.clone();
                let mut field = Buffer::from_text(&name);
                // The stem is selected, as in Finder: typing replaces the
                // name and keeps the extension.
                let stem = if entry.is_dir {
                    name.len()
                } else {
                    Path::new(&name).file_stem().map_or(name.len(), |s| s.len())
                };
                field.select_input_range(0, stem);
                let parent = path.parent().map_or(root.clone(), Path::to_path_buf);
                (parent, index, entry.depth, field)
            }
            SidebarEditKind::NewFile | SidebarEditKind::NewFolder => {
                let parent = state.tree.target_dir().unwrap_or(root.clone());
                let at = state
                    .tree
                    .rows()
                    .iter()
                    .position(|e| e.is_dir && e.path == parent);
                let (row, depth) = match at {
                    Some(index) => (index + 1, state.tree.rows()[index].depth + 1),
                    None => (0, 0),
                };
                (parent, row, depth, Buffer::new())
            }
        };
        field.select_input_range(field.cursor(), 0);
        if let SidebarEditKind::Rename(_) = &kind {
            let stem_end = field.selection().map_or(field.cursor(), |r| r.end);
            field.select_input_range(0, stem_end);
        }
        // Scroll the row into view.
        let chrome = chrome_of(&state);
        if let Some(rect) = chrome.sidebar {
            let rows = layout::sidebar_rows(rect).max(1);
            if row < state.tree.scroll {
                state.tree.scroll = row;
            } else if row >= state.tree.scroll + rows {
                state.tree.scroll = row + 1 - rows;
            }
        }
        state.sidebar_edit = Some(SidebarEdit {
            kind,
            parent,
            row,
            depth,
            field,
        });
        drop(state);
        self.request_redraw();
        self.pump();
    }

    /// Keys while a name is being typed in the tree.
    fn handle_sidebar_edit_key(&self, event: &NSEvent) -> bool {
        const ESCAPE: u16 = 53;
        let flags = event.modifierFlags();
        if flags.contains(NSEventModifierFlags::Command) {
            // Cmd-A, Cmd-V and friends arrive through the Edit menu.
            return false;
        }
        let motion = if flags.contains(NSEventModifierFlags::Shift) {
            Motion::Extend
        } else {
            Motion::Move
        };
        let option = flags.contains(NSEventModifierFlags::Option);
        let code = match event.keyCode() {
            key::KEYPAD_ENTER => key::RETURN,
            code => code,
        };
        match code {
            ESCAPE => self.finish_sidebar_edit(false),
            key::RETURN => self.finish_sidebar_edit(true),
            key::TAB => return true,
            key::DELETE => {
                let mut state = self.ivars().state.borrow_mut();
                if let Some(edit) = &mut state.sidebar_edit {
                    if option {
                        edit.field.delete_word_backward();
                    } else {
                        edit.field.backspace();
                    }
                }
            }
            key::LEFT | key::RIGHT | key::HOME | key::END => {
                let mut state = self.ivars().state.borrow_mut();
                if let Some(edit) = &mut state.sidebar_edit {
                    match code {
                        key::LEFT => edit.field.move_left(motion),
                        key::RIGHT => edit.field.move_right(motion),
                        key::HOME => edit.field.move_line_start(motion),
                        _ => edit.field.move_line_end(motion),
                    }
                }
            }
            key::UP | key::DOWN => return true,
            _ => return self.interpret(event),
        }
        self.request_redraw();
        self.pump();
        true
    }

    /// Ends the inline field: creates, renames, or does nothing.
    fn finish_sidebar_edit(&self, commit: bool) {
        let Some(edit) = self.ivars().state.borrow_mut().sidebar_edit.take() else {
            return;
        };
        let name = edit.field.rope.to_string();
        let name = name.trim();
        if !commit || name.is_empty() {
            self.request_redraw();
            return;
        }
        let relative = Path::new(name);
        let single = relative.file_name().is_some_and(|part| part == name);
        let nested_ok = !name.starts_with('/')
            && relative.components().all(
                |c| matches!(c, std::path::Component::Normal(part) if part != "." && part != ".."),
            );
        let valid = match edit.kind {
            SidebarEditKind::NewFile | SidebarEditKind::NewFolder => nested_ok,
            SidebarEditKind::Rename(_) => single,
        };
        if !valid {
            self.ivars().state.borrow_mut().message =
                Some((format!("not a valid name: {name}"), Instant::now()));
            self.request_redraw();
            return;
        }
        let target = edit.parent.join(relative);
        match edit.kind {
            SidebarEditKind::NewFile => {
                if target.exists() {
                    self.ivars().state.borrow_mut().message =
                        Some((format!("{name} already exists"), Instant::now()));
                } else {
                    let created = target
                        .parent()
                        .map_or(Ok(()), std::fs::create_dir_all)
                        .and_then(|()| std::fs::write(&target, ""));
                    match created {
                        Ok(()) => self.open_created_file(&target),
                        Err(error) => {
                            self.ivars().state.borrow_mut().message =
                                Some((format!("could not create: {error}"), Instant::now()));
                        }
                    }
                }
            }
            SidebarEditKind::NewFolder => {
                let result = if target.exists() {
                    Err(std::io::Error::other("already exists"))
                } else {
                    std::fs::create_dir_all(&target)
                };
                self.ivars().state.borrow_mut().message = Some((
                    match &result {
                        Ok(()) => format!("created folder {name}"),
                        Err(error) => format!("could not create folder: {error}"),
                    },
                    Instant::now(),
                ));
                if result.is_ok() {
                    self.refresh_project_after_disk_change();
                }
            }
            SidebarEditKind::Rename(path) => {
                if path.file_name().is_some_and(|current| current != name) {
                    self.rename_item(&path, &target);
                }
            }
        }
        self.request_redraw();
        self.pump();
    }

    /// A file just written to disk gets a tab and the keyboard.
    fn open_created_file(&self, path: &Path) {
        {
            let mut state = self.ivars().state.borrow_mut();
            match Buffer::open(path) {
                // add, not push: the file may already be open.
                Ok(buffer) => state.docs.add(buffer),
                Err(e) => {
                    state.message = Some((format!("created but not opened: {e}"), Instant::now()));
                }
            }
            state.preview = default_preview(state.docs.active());
            reveal_active_tab(&mut state);
            state.message = Some((format!("created {}", path.display()), Instant::now()));
        }
        self.refresh_project_after_disk_change();
        self.sync_title();
        self.reparse();
        self.lsp_sync_open();
    }

    /// Moves `path` to `destination`, re-keying open tabs.
    fn rename_item(&self, path: &Path, destination: &Path) {
        // Open buffers use canonical identities. Capture the source key
        // before the rename makes it impossible to canonicalise.
        let source_key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        match move_without_replace(path, destination) {
            Ok(()) => {
                {
                    let mut state = self.ivars().state.borrow_mut();
                    for docs in all_docs_mut(&mut state) {
                        docs.rename_path(&source_key, destination);
                    }
                    state.message = Some((
                        format!("renamed to {}", destination.display()),
                        Instant::now(),
                    ));
                }
                self.refresh_project_after_disk_change();
                self.sync_title();
                self.reparse();
            }
            Err(error) => {
                self.ivars().state.borrow_mut().message =
                    Some((format!("rename failed: {error}"), Instant::now()));
            }
        }
    }

    fn new_file_with_panel(&self) -> bool {
        // No unsaved-changes prompt: the new file gets its own tab.
        let start_dir = self.ivars().state.borrow().tree.target_dir();

        let mtm = MainThreadMarker::from(self);
        let panel = NSSavePanel::savePanel(mtm);
        panel.setNameFieldStringValue(&NSString::from_str("untitled.txt"));
        if let Some(dir) = &start_dir {
            let url = NSURL::fileURLWithPath(&NSString::from_str(&dir.to_string_lossy()));
            panel.setDirectoryURL(Some(&url));
        }
        const MODAL_RESPONSE_OK: isize = 1;
        if panel.runModal() != MODAL_RESPONSE_OK {
            return false;
        }
        let Some(url) = panel.URL() else {
            return false;
        };
        let Some(path) = url.path() else {
            return false;
        };
        let path = std::path::PathBuf::from(path.to_string());

        if let Err(e) = std::fs::write(&path, "") {
            self.ivars().state.borrow_mut().message =
                Some((format!("could not create: {e}"), Instant::now()));
            return true;
        }

        {
            let mut state = self.ivars().state.borrow_mut();
            match Buffer::open(&path) {
                // add, not push: the file may already be open.
                Ok(buffer) => state.docs.add(buffer),
                Err(e) => {
                    state.message = Some((format!("created but not opened: {e}"), Instant::now()));
                }
            }
            state.preview = default_preview(state.docs.active());
            reveal_active_tab(&mut state);
            // The directory walk and finder rebuild happen off the UI thread.
            state.project_index_rx = Some(spawn_project_refresh(
                state.tree.clone(),
                state.tree_version,
            ));
        }
        self.resume_display_link();
        self.sync_title();
        true
    }

    /// Runs `edit` on whatever text has the keyboard: an open overlay's field
    /// before the document, in the order `handle_key` offers them the keys.
    ///
    /// The Edit menu used to act on the document regardless, so Cmd-V with
    /// the find bar open pasted into the file behind it.
    fn edit_focused<R>(&self, edit: impl FnOnce(&mut Buffer, Focus) -> R) -> (R, Focus) {
        // A review tab holds Claude's text, which is answered, not edited.
        // Menu edits land in a buffer nobody sees.
        let mut inert = Buffer::new();
        let reviewing =
            active_review(&self.ivars().state.borrow()).is_some() || self.terminal_has_keys();
        let mut state = self.ivars().state.borrow_mut();
        let State {
            docs,
            find,
            palette,
            goto,
            git,
            git_open,
            sidebar_edit,
            rename,
            ..
        } = &mut *state;
        let (buffer, focus) = if let Some(edit) = sidebar_edit {
            (&mut edit.field, Focus::Field)
        } else if let Some(rename) = rename {
            (&mut rename.field, Focus::Field)
        } else if *git_open {
            (&mut git.message, Focus::Field)
        } else if let Some(field) = goto {
            (field, Focus::Goto)
        } else if let Some((query, _)) = palette {
            (query, Focus::Field)
        } else if let Some(bar) = find {
            if bar.replacing {
                (&mut bar.replacement, Focus::Field)
            } else {
                (&mut bar.query, Focus::FindQuery)
            }
        } else if reviewing {
            (&mut inert, Focus::Goto)
        } else {
            (docs.active_mut(), Focus::Document)
        };
        (edit(buffer, focus), focus)
    }

    /// What has to happen after [`Self::edit_focused`] changed some text.
    fn after_focused_edit(&self, focus: Focus) {
        match focus {
            Focus::Document => return self.after_edit(),
            Focus::FindQuery => self.refresh_find(),
            Focus::Goto => {}
            Focus::Field => {
                // A different palette query invalidates the selected row.
                let mut state = self.ivars().state.borrow_mut();
                if let Some((_, selected)) = &mut state.palette {
                    *selected = 0;
                    state.palette_scroll = 0;
                }
            }
        }
        self.request_redraw();
        self.pump();
    }

    /// Shared tail for the menu actions: refresh the title and draw.
    fn after_edit(&self) {
        self.reparse();
        let (rows, cols) = self.grid();
        let mut state = self.ivars().state.borrow_mut();
        let id = state.docs.active().id();
        if state.docs.active().path.is_some() {
            state.lsp_dirty.insert(id, Instant::now());
            state.gutter_dirty.insert(id, Instant::now());
        }
        // The list follows the prefix as it is typed and goes away when the
        // caret leaves the word it was opened for.
        let caret = state.docs.active().cursor();
        let prefix = state.completion.as_ref().and_then(|popup| {
            (popup.buffer == id && caret >= popup.anchor).then(|| {
                state
                    .docs
                    .active()
                    .rope
                    .slice_to_string(popup.anchor..caret)
            })
        });
        match prefix {
            Some(prefix) if !prefix.contains(char::is_whitespace) => {
                if let Some(popup) = &mut state.completion {
                    popup.refilter(&prefix);
                }
            }
            _ => state.completion = None,
        }
        if state.preview.is_some() && default_preview(state.docs.active()).is_some() {
            let id = state.docs.active().id();
            let line = state.docs.active().cursor_position().0;
            state.live_line = Some((id, line));
        }
        state.docs.active_mut().scroll_to_cursor(rows, cols);
        drop(state);
        self.resume_display_link();
        self.sync_title();
        self.request_redraw();
        self.pump();
    }

    /// Extends the selection to the last known drag point, scrolling first
    /// if that point is past the top or bottom of the text. Returns whether
    /// it is, which is to say whether this needs calling again next frame.
    fn drag_select(&self) -> bool {
        let mut state = self.ivars().state.borrow_mut();
        let (Some((unit, pressed)), Some((x, y))) = (state.selecting.clone(), state.drag_point)
        else {
            return false;
        };
        let chrome = chrome_of(&state);
        let text = chrome.text;
        let m = state.renderer.atlas.metrics;
        let rows = text.rows(m.line_height);

        // How far past the edge sets the speed: a nudge creeps, a long way
        // out runs. In lines per second, paid out by the frame.
        let past = if y < text.y {
            y - text.y
        } else if y > text.y + text.height {
            y - (text.y + text.height)
        } else {
            0.0
        };
        if past != 0.0 {
            let speed = past.signum() * (8.0 + past.abs() * 1.5).min(240.0);
            state.autoscroll_carry += speed * state.frame_interval.as_secs_f32();
            let lines = state.autoscroll_carry.trunc();
            state.autoscroll_carry -= lines;
            state.docs.active_mut().scroll_by(lines as isize, rows);
        } else {
            state.autoscroll_carry = 0.0;
        }

        // The point is pulled inside the text first, so dragging above the
        // window selects up to the first visible line, not to somewhere
        // computed from a negative row.
        let inside = (
            x.clamp(text.x, text.x + text.width - 1.0),
            y.clamp(text.y, text.y + (text.height - 1.0).max(0.0)),
        );
        let (tx, ty) = chrome.to_text(inside.0, inside.1);
        let offset = layout::offset_at_point(state.docs.active(), &state.renderer.atlas, tx, ty);

        let buffer = state.docs.active_mut();
        let reached = match unit {
            SelectUnit::Character => offset..offset,
            SelectUnit::Word => buffer.word_range_at(offset),
            SelectUnit::Line => buffer.line_range_at(offset),
        };
        // Whatever the press selected stays selected; the drag grows it
        // towards the pointer on whichever side the pointer is.
        if reached.start < pressed.start {
            buffer.select_range(pressed.end, reached.start);
        } else if unit == SelectUnit::Character {
            buffer.place_cursor(offset, Motion::Extend);
        } else {
            buffer.select_range(pressed.start, reached.end.max(pressed.end));
        }
        past != 0.0
    }

    /// Converts a mouse event to a byte offset in the buffer.
    fn offset_for_event(&self, event: &NSEvent) -> usize {
        let window_point = event.locationInWindow();
        let point = self.convertPoint_fromView(window_point, None);
        let state = self.ivars().state.borrow();
        // The hit test works in the text area's own coordinates. Handing it
        // window coordinates is what put every click two rows low and, with
        // the sidebar showing, a sidebar's width to the right.
        let (x, y) = chrome_of(&state).to_text(point.x as f32, point.y as f32);
        layout::offset_at_point(state.docs.active(), &state.renderer.atlas, x, y)
    }

    /// Cmd-= and friends: a new code size, remembered in the settings file.
    fn set_font_size(&self, size: f32) {
        use crate::platform::settings::{MAX_FONT_SIZE, MIN_FONT_SIZE, Settings};
        let size = size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        let font = {
            let state = self.ivars().state.borrow();
            if (state.font_size - size).abs() < 0.01 {
                return;
            }
            state.font.clone()
        };
        self.apply_font(font.clone(), size);
        let saved = Settings {
            font,
            font_size: size,
            theme: self.ivars().state.borrow().theme_choice,
            caret_blink: self.ivars().state.borrow().caret_blink,
            update_check: true,
            format_on_save: self.ivars().state.borrow().format_on_save,
            word_wrap: self.ivars().state.borrow().word_wrap,
            ssh_auth_sock: None,
        }
        .save();
        self.ivars().state.borrow_mut().message = Some((
            match saved {
                Ok(()) => format!("font size {}", size as i32),
                Err(e) => format!("font size {}, not saved: {e}", size as i32),
            },
            Instant::now(),
        ));
    }

    /// crc > Settings: the settings file as a tab, created from the template
    /// when there is none. Saving it applies it.
    fn open_settings(&self) {
        let path = match crate::platform::settings::Settings::ensure_file() {
            Ok(path) => path,
            Err(e) => {
                self.ivars().state.borrow_mut().message =
                    Some((format!("settings: {e}"), Instant::now()));
                return;
            }
        };
        // Straight into a tab, without adopting ~/.config/crc as the project
        // the way opening an ordinary file with no project would.
        let mut state = self.ivars().state.borrow_mut();
        match state.docs.open(&path) {
            Ok(()) => {
                state.preview = default_preview(state.docs.active());
                reveal_active_tab(&mut state);
            }
            Err(e) => {
                state.message = Some((format!("settings: {e}"), Instant::now()));
            }
        }
        drop(state);
        self.sync_title();
        self.reparse();
    }

    /// The settings file was just saved from a tab: read it back and apply
    /// what changed.
    fn apply_settings_file(&self) {
        let settings = crate::platform::settings::Settings::load();
        let (font, size) = {
            let state = self.ivars().state.borrow();
            (state.font.clone(), state.font_size)
        };
        if settings.font != font || (settings.font_size - size).abs() > 0.01 {
            self.apply_font(settings.font.clone(), settings.font_size);
        }
        {
            let mut state = self.ivars().state.borrow_mut();
            state.theme_choice = settings.theme;
            state.caret_blink = settings.caret_blink;
            state.format_on_save = settings.format_on_save;
            state.word_wrap = settings.word_wrap;
            state.ssh_auth_sock = settings.ssh_auth_sock.clone();
        }
        self.apply_theme();
        self.ivars().state.borrow_mut().message =
            Some(("settings applied".to_string(), Instant::now()));
    }

    /// Whether the view is being shown in the dark appearance.
    fn system_is_dark(&self) -> bool {
        use objc2_app_kit::{
            NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
        };
        // The names are AppKit statics, which Rust cannot vouch for; they
        // are the documented constants and never written.
        let (aqua, dark) = unsafe { (NSAppearanceNameAqua, NSAppearanceNameDarkAqua) };
        let names = NSArray::from_slice(&[aqua, dark]);
        self.effectiveAppearance()
            .bestMatchFromAppearancesWithNames(&names)
            .is_some_and(|best| &*best == dark)
    }

    /// Picks the colour table from the settings and the system, and tells
    /// the window so its own alerts and menus agree.
    fn apply_theme(&self) {
        use crate::platform::settings::ThemeChoice;
        use objc2_app_kit::{
            NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
        };
        let choice = self.ivars().state.borrow().theme_choice;
        let dark = match choice {
            ThemeChoice::System => self.system_is_dark(),
            ThemeChoice::Dark => true,
            ThemeChoice::Light => false,
        };
        if let Some(window) = self.window() {
            let (aqua, dark_aqua) = unsafe { (NSAppearanceNameAqua, NSAppearanceNameDarkAqua) };
            let forced = match choice {
                ThemeChoice::System => None,
                ThemeChoice::Dark => NSAppearance::appearanceNamed(dark_aqua),
                ThemeChoice::Light => NSAppearance::appearanceNamed(aqua),
            };
            window.setAppearance(forced.as_deref());
        }
        {
            let mut state = self.ivars().state.borrow_mut();
            if state.theme.is_dark() == dark {
                return;
            }
            state.theme = if dark { Theme::dark() } else { Theme::light() };
        }
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
        self.request_redraw();
    }

    /// Rebuilds the atlas for `font` at `size`, keeping the UI at its own
    /// size, and re-clamps every pane's scroll to the rows that fit now.
    fn apply_font(&self, font: String, size: f32) {
        let scale = self.window().map_or(2.0, |w| w.backingScaleFactor()) as f32;
        {
            let mut state = self.ivars().state.borrow_mut();
            let atlas = Atlas::build_with_ui(
                &font,
                size,
                crate::platform::settings::DEFAULT_FONT_SIZE,
                scale,
            );
            state.renderer.replace_atlas(atlas);
            state.font = font;
            state.font_size = size;
        }
        // Fewer or more rows fit now; the scroll positions are re-clamped.
        let (rows, cols) = self.grid();
        {
            let mut state = self.ivars().state.borrow_mut();
            let setting = state.word_wrap;
            for docs in all_docs_mut(&mut state) {
                apply_wrap(docs.active_mut(), setting, cols);
                docs.active_mut().clamp_scroll(rows, cols);
            }
        }
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
        self.request_redraw();
        self.pump();
    }

    fn resize(&self, size: NSSize) {
        let scale = self.window().map_or(2.0, |w| w.backingScaleFactor());
        let Ok(mut state) = self.ivars().state.try_borrow_mut() else {
            self.request_redraw();
            return;
        };
        state.viewport = Viewport::new(size.width as f32, size.height as f32);
        if (state.renderer.atlas.metrics.scale - scale as f32).abs() > 0.01 {
            // Moved to a display with a different scale: the glyphs were
            // rasterised for the old one and would be resampled, which is
            // the blur the whole atlas exists to avoid.
            let atlas = Atlas::build_with_ui(
                &state.font,
                state.font_size,
                crate::platform::settings::DEFAULT_FONT_SIZE,
                scale as f32,
            );
            state.renderer.replace_atlas(atlas);
        }
        state.layer.setContentsScale(scale);
        state.layer.setDrawableSize(objc2_core_foundation::CGSize {
            width: size.width * scale,
            height: size.height * scale,
        });
        drop(state);
        self.request_redraw();
        self.pump();
    }

    fn render(&self) -> Option<FrameTiming> {
        // try_borrow_mut, not borrow_mut. AppKit re-enters this view at times
        // we do not choose: the display link fires on any run-loop iteration
        // including inside the nested loop of a modal alert or panel, and
        // resetCursorRects is called during tracking. If a borrow is already
        // live, the honest answer is to skip this frame and draw on the next
        // one, not to abort the process.
        let Ok(mut state) = self.ivars().state.try_borrow_mut() else {
            return None;
        };
        let chrome = chrome_of(&state);
        let carets_on = !self.caret_blinks(&state) || caret_phase(state.caret_since.elapsed()).0;
        let State {
            docs,
            tree,
            git,
            git_open,
            find,
            tab_hits,
            tab_scroll,
            hovered_tab,
            tree_drag,
            syntax,
            spans,
            finder,
            palette,
            commands: command_list,
            symbols: symbol_list,
            palette_scroll,
            goto,
            rename,
            signature,
            word_wrap,
            branch_list,
            blame,
            preview,
            live_line,
            md_hits,
            copied_code,
            renderer,
            glyphs,
            theme,
            latency,
            layer,
            viewport,
            drew_once,
            worst,
            message,
            marked,
            responses,
            sidebar_edit,
            panes,
            focused_pane,
            lsp,
            completion,
            claude,
            terminal: terminal_panel,
            gutter,
            home_hits,
            recent_projects,
            unshaped_on_screen,
            completion_chips,
            ..
        } = &mut *state;
        *unshaped_on_screen = false;

        // First, before anything in the frame can go wrong: what the panic
        // hook would write is the text as of this frame.
        recovery::publish(docs.iter().chain(panes.iter().flat_map(|p| p.docs.iter())));

        let viewport = *viewport;
        if viewport.width <= 0.0 || viewport.height <= 0.0 {
            return None;
        }

        let query = find
            .as_ref()
            .map(|f| f.query.rope.to_string())
            .unwrap_or_default();

        // The one layout. `chrome_of` needs the whole state, so it is asked
        // before the state is taken apart field by field below.
        let Chrome {
            toolbar: toolbar_rect,
            sidebar: sidebar_rect,
            tabs: tab_rect,
            breadcrumbs: breadcrumb_rect,
            find: find_rect,
            response: response_rect,
            terminal: terminal_rect,
            text: editor_rect,
            status: status_rect,
            panes: _,
            others: other_panes,
        } = chrome;

        // Scroll positions are only ever clamped when something scrolls, and
        // what they are clamped against changes without any scrolling: the
        // window grows, text is deleted, a folder collapses. Left alone that
        // is blank rows under the last line, which reads as having scrolled
        // past the end. Re-clamped here, every frame, against the rows this
        // frame really has.
        {
            let m = renderer.atlas.metrics;
            let gutter = layout::gutter_width(docs.active(), &renderer.atlas);
            let (rows, cols) = (
                editor_rect.rows(m.line_height),
                editor_rect.columns(m.advance, gutter),
            );
            apply_wrap(docs.active_mut(), *word_wrap, cols);
            docs.active_mut().clamp_scroll(rows, cols);
            if let Some(rect) = sidebar_rect {
                tree.scroll_by(0, layout::sidebar_rows(rect));
            }
        }
        let buffer = docs.active();
        let search_matches = find.as_ref().and_then(|bar| {
            if bar.query.rope.len_bytes() == 0 || buffer.rope.len_bytes() > 2 * 1024 * 1024 {
                return None;
            }
            search::find(
                &buffer.rope.to_string(),
                &bar.query.rope.to_string(),
                "",
                bar.options,
            )
            .ok()
        });

        // Markdown preview replaces the editor body. Parsing per frame is
        // fine: a README is kilobytes, and the alternative is a cache that
        // has to be invalidated on every edit.
        let previewing = preview.is_some() && default_preview(buffer).is_some();

        // Nothing open: the home screen, drawn by the same renderer. There is
        // no home "mode" to get stuck in. Typing lands in the untouched buffer
        // underneath, which stops being untouched, and the editor is back.
        let home = docs.is_home();

        // A change picked in Source Control takes the editor column, the way
        // a diff editor does. The document keeps its tab and comes back when
        // one is clicked.
        let diffing = *git_open && git.showing_diff;
        let reviewing = claude.as_ref().and_then(|c| c.reviews.get(&buffer.id()));

        if diffing {
            glyphs.clear();
            layout::push_rect(
                glyphs,
                &renderer.atlas,
                [editor_rect.x, editor_rect.y],
                [editor_rect.width, editor_rect.height],
                theme.tab_active,
            );
            git.draw_diff(&mut renderer.atlas, editor_rect, theme, glyphs);
        } else if let Some(review) = reviewing {
            glyphs.clear();
            layout::push_rect(
                glyphs,
                &renderer.atlas,
                [editor_rect.x, editor_rect.y],
                [editor_rect.width, editor_rect.height],
                theme.tab_active,
            );
            crate::platform::claude::draw_review(
                review,
                &mut renderer.atlas,
                editor_rect,
                theme,
                glyphs,
            );
        } else if home {
            glyphs.clear();
            layout::push_rect(
                glyphs,
                &renderer.atlas,
                [editor_rect.x, editor_rect.y],
                [editor_rect.width, editor_rect.height],
                theme.tab_active,
            );
            layout::build_home(
                glyphs,
                &mut renderer.atlas,
                editor_rect,
                theme,
                tree.root(),
                recent_projects,
                home_hits,
            );
        } else if previewing {
            let source = buffer.rope.to_string();
            let blocks = markdown::parse_spanned(&source);
            let scroll = preview.unwrap_or(0).min(blocks.len().saturating_sub(1));
            let active = live_line
                .filter(|(id, _)| *id == buffer.id())
                .map(|_| buffer.cursor());
            layout::build_markdown(
                &blocks,
                &source,
                active,
                copied_code.and_then(|(index, at)| {
                    (at.elapsed() < Duration::from_secs(3)).then_some(index)
                }),
                scroll,
                &mut renderer.atlas,
                editor_rect,
                theme,
                glyphs,
                md_hits,
            );
        } else {
            // Highlight only what is on screen. Querying a whole file to draw
            // sixty lines of it would cost more than everything else in the
            // frame put together.
            spans.clear();
            if syntax.has(buffer.id()) {
                let total = buffer.rope.len_lines();
                let std::ops::Range {
                    start: first,
                    end: last,
                } = layout::visible_lines(buffer, editor_rect, renderer.atlas.metrics.line_height);
                let from = buffer.rope.line_to_byte(first);
                let to = if last < total {
                    buffer.rope.line_to_byte(last)
                } else {
                    buffer.rope.len_bytes()
                };
                // spans_with, not spans: predicates need the captured text, and
                // without it a `#match?` pattern matches everything.
                spans.extend(
                    syntax.spans_with(buffer.id(), from..to, |r| buffer.rope.slice_to_string(r)),
                );
            }

            let ranges: Vec<_> = search_matches
                .as_ref()
                .map(|found| found.iter().map(|m| m.range.clone()).collect())
                .unwrap_or_default();
            let stats = layout::build_full_search(
                buffer,
                &mut renderer.atlas,
                editor_rect,
                theme,
                &query,
                find.as_ref().map(|_| ranges.as_slice()),
                spans,
                carets_on,
                glyphs,
            );
            *unshaped_on_screen = stats.unshaped > 0;
            if let Some(marks) = gutter.get(&buffer.id()) {
                layout::push_gutter_marks(
                    glyphs,
                    &renderer.atlas,
                    buffer,
                    editor_rect,
                    theme,
                    &marks.marks,
                );
            }

            // A composition in progress, drawn at the caret it will land at.
            if let Some(text) = marked.as_deref()
                && find.is_none()
                && palette.is_none()
                && goto.is_none()
                && let Some(at) = layout::caret_rect(buffer, &renderer.atlas, editor_rect)
            {
                layout::push_marked_text(glyphs, &mut renderer.atlas, at, text, theme);
            }

            // What the language server thinks of the visible lines.
            if let (Some(path), Some(language)) = (&buffer.path, lsp_language(buffer))
                && let Some(server) = lsp.get(&crate::lsp::servers::server_key(language))
                && let Some(list) = server.diagnostics.get(path)
            {
                use crate::lsp::Severity;
                for diagnostic in list {
                    let start = crate::lsp::offset_of(&buffer.rope, diagnostic.start);
                    let end = crate::lsp::offset_of(&buffer.rope, diagnostic.end);
                    let color = match diagnostic.severity {
                        Severity::Error => theme.diff_removed,
                        Severity::Warning => theme.syn_constant,
                        Severity::Information | Severity::Hint => theme.status_text,
                    };
                    layout::push_underline(
                        glyphs,
                        &renderer.atlas,
                        buffer,
                        editor_rect,
                        start..end,
                        color,
                    );
                }
            }

            // Completion: ghost text at the caret, chips under the line.
            completion_chips.clear();
            if let Some(popup) = completion
                .as_ref()
                .filter(|p| p.buffer == buffer.id() && !p.shown.is_empty())
                && let Some(caret) = layout::caret_rect(buffer, &renderer.atlas, editor_rect)
            {
                let cursor = buffer.cursor();
                let prefix = buffer
                    .rope
                    .slice_to_string(popup.anchor.min(cursor)..cursor);
                let line = buffer.rope.byte_to_line(cursor);
                let line_end = if line + 1 < buffer.rope.len_lines() {
                    buffer.rope.line_to_byte(line + 1)
                } else {
                    buffer.rope.len_bytes()
                };
                let rest = buffer.rope.slice_to_string(cursor..line_end);
                let picked = &popup.shown[popup.selected.min(popup.shown.len() - 1)];
                // Only at the end of a line: drawn over text that follows the
                // caret, it would read as if it were there.
                let ghost = (rest.trim().is_empty() && picked.insert.starts_with(prefix.as_str()))
                    .then(|| &picked.insert[prefix.len()..]);
                let chips: Vec<layout::Chip> = popup
                    .shown
                    .iter()
                    .map(|c| layout::Chip {
                        label: c.label.as_str(),
                        icon: completion_icon(c, &popup.items),
                    })
                    .collect();
                let word_x =
                    caret.x - prefix.chars().count() as f32 * renderer.atlas.metrics.advance;
                *completion_chips = layout::build_completion_ribbon(
                    &chips,
                    popup.selected,
                    &picked.why,
                    ghost,
                    caret,
                    word_x,
                    editor_rect,
                    &mut renderer.atlas,
                    theme,
                    glyphs,
                );
            }

            // Signature help, above the caret's line.
            if let Some(tip) = signature.as_ref().filter(|t| t.buffer == buffer.id())
                && let Some(caret) = layout::caret_rect(buffer, &renderer.atlas, editor_rect)
            {
                layout::build_signature(
                    &tip.signature.label,
                    tip.signature.active.clone(),
                    caret,
                    editor_rect,
                    &mut renderer.atlas,
                    theme,
                    glyphs,
                );
            }
        }

        // After build_full, not before: that call clears the glyph buffer it
        // is handed, so anything drawn earlier in the frame is silently
        // erased. The sidebar and status line already append after it for
        // the same reason.
        for (index, rects) in &other_panes {
            let Some(store) = panes.get_mut(index - usize::from(*index > *focused_pane)) else {
                continue;
            };
            draw_other_pane(
                store, rects, tree, syntax, responses, gutter, renderer, theme, glyphs, *word_wrap,
            );
        }
        layout::build_toolbar(tree, &mut renderer.atlas, toolbar_rect, theme, glyphs);

        layout::build_tab_bar(
            docs,
            *tab_scroll,
            *hovered_tab,
            &mut renderer.atlas,
            tab_rect,
            theme,
            glyphs,
            tab_hits,
        );

        if diffing {
            // The breadcrumb row says which change is on screen, so the
            // editor column is never an unlabelled wall of diff.
            layout::push_rect(
                glyphs,
                &renderer.atlas,
                [breadcrumb_rect.x, breadcrumb_rect.y],
                [breadcrumb_rect.width, breadcrumb_rect.height],
                theme.tab_active,
            );
            layout::push_ui_text(
                glyphs,
                &mut renderer.atlas,
                Viewport {
                    x: breadcrumb_rect.x + 12.0,
                    width: (breadcrumb_rect.width - 120.0).max(0.0),
                    ..breadcrumb_rect
                },
                &git.diff_title(),
                theme.text,
            );
            layout::push_ui_text_right(
                glyphs,
                &mut renderer.atlas,
                Viewport {
                    width: (breadcrumb_rect.width - 12.0).max(0.0),
                    ..breadcrumb_rect
                },
                &git.diff_summary(),
                theme.status_text,
            );
        } else {
            layout::build_breadcrumbs(
                buffer,
                tree,
                &mut renderer.atlas,
                breadcrumb_rect,
                theme,
                glyphs,
            );
            if let Some(strip) = response_rect
                && let Some(view) = responses.get(&buffer.id())
            {
                layout::build_response_strip(view, &mut renderer.atlas, strip, theme, glyphs);
            }
            if let Some(strip) = response_rect
                && let Some(review) = claude.as_ref().and_then(|c| c.reviews.get(&buffer.id()))
            {
                crate::platform::claude::draw_review_strip(
                    review,
                    tree.root(),
                    &mut renderer.atlas,
                    strip,
                    theme,
                    glyphs,
                );
            }
            // A shortcut nobody can see is a shortcut nobody uses. The
            // breadcrumb row has the space, and it is the row that belongs to
            // the open file, which is what the command acts on.
            if default_preview(buffer).is_some() {
                layout::push_ui_text_right(
                    glyphs,
                    &mut renderer.atlas,
                    Viewport {
                        width: (breadcrumb_rect.width - 14.0).max(0.0),
                        ..breadcrumb_rect
                    },
                    if preview.is_some() {
                        "⌘E  Edit"
                    } else {
                        "⌘E  Preview"
                    },
                    theme.gutter_text,
                );
            }
        }

        if let Some(rect) = find_rect {
            layout::push_rect(
                glyphs,
                &renderer.atlas,
                [rect.x, rect.y],
                [rect.width, rect.height],
                theme.find_background,
            );
            let bar = find.as_ref().expect("find_rect implies a bar");
            let g = layout::FindGeometry::new(rect);

            // A field that looks like a field. There was no box at all: a
            // "Find" label sat at the far left and the text began eleven
            // monospace columns later, with nothing to say where you could
            // type or how far the field reached.
            let field = |glyphs: &mut Vec<GlyphInstance>,
                         atlas: &mut Atlas,
                         box_rect: Viewport,
                         placeholder: &str,
                         buffer: &Buffer,
                         focused: bool,
                         trailing: Option<&str>| {
                if focused {
                    layout::push_focus_ring(glyphs, box_rect, 6.0, 1.5);
                }
                layout::push_rounded_rect(glyphs, box_rect, 6.0, theme.tab_active);
                let text = buffer.rope.to_string();
                let inner = Viewport {
                    x: box_rect.x + 10.0,
                    width: (box_rect.width - 20.0).max(0.0),
                    ..box_rect
                };
                // The match count first, so the text knows how much room is
                // left and never runs underneath it.
                let mut room = inner.width;
                if let Some(trailing) = trailing {
                    layout::push_ui_text_right(glyphs, atlas, inner, trailing, theme.status_text);
                    room = (room - layout::ui_text_width(atlas, trailing) - 12.0).max(0.0);
                }
                let (shown, start) = layout::ui_input_window(&text, buffer.cursor());
                layout::push_ui_text(
                    glyphs,
                    atlas,
                    Viewport {
                        width: room,
                        ..inner
                    },
                    if shown.is_empty() {
                        placeholder
                    } else {
                        &shown
                    },
                    if shown.is_empty() {
                        theme.status_text
                    } else {
                        theme.text
                    },
                );
                if focused {
                    let caret =
                        layout::ui_caret_x(atlas, &shown, buffer.cursor().saturating_sub(start))
                            .min(room);
                    layout::push_rect(
                        glyphs,
                        atlas,
                        [inner.x + caret, box_rect.y + 5.0],
                        [1.0, box_rect.height - 10.0],
                        theme.cursor,
                    );
                }
            };

            // "3 of 17" in the field it belongs to, which the bar never
            // reported at all: there was no way to tell a search that found
            // nothing from one that found everything.
            let count = if bar.project {
                (!bar.results.is_empty()).then(|| format!("{} found", bar.results.len()))
            } else {
                search_matches.as_ref().map(|found| {
                    if found.is_empty() {
                        "No matches".to_string()
                    } else {
                        let at = found
                            .iter()
                            .position(|m| m.range.start >= buffer.cursor())
                            .unwrap_or(0);
                        format!("{} of {}", at + 1, found.len())
                    }
                })
            };
            field(
                glyphs,
                &mut renderer.atlas,
                g.find_field,
                if bar.project {
                    "Search the project"
                } else {
                    "Find"
                },
                &bar.query,
                !bar.replacing,
                count.as_deref(),
            );
            field(
                glyphs,
                &mut renderer.atlas,
                g.replace_field,
                if bar.project {
                    "Replace in every file listed"
                } else {
                    "Replace with"
                },
                &bar.replacement,
                bar.replacing,
                None,
            );

            // Buttons with their text centred, in the UI font the rest of the
            // window uses, rather than monospace pushed in by one advance.
            let button = |glyphs: &mut Vec<GlyphInstance>,
                          atlas: &mut Atlas,
                          r: Viewport,
                          label: &str,
                          enabled: bool| {
                layout::push_rounded_rect(glyphs, r, 5.0, theme.tab_hover);
                layout::push_ui_text_centered(
                    glyphs,
                    atlas,
                    r,
                    label,
                    if enabled {
                        theme.tab_text
                    } else {
                        theme.gutter_text
                    },
                );
            };
            let has_matches = if bar.project {
                !bar.results.is_empty()
            } else {
                search_matches.as_ref().is_some_and(|f| !f.is_empty())
            };
            button(
                glyphs,
                &mut renderer.atlas,
                g.previous,
                "\u{2039}",
                has_matches,
            );
            button(glyphs, &mut renderer.atlas, g.next, "\u{203a}", has_matches);
            button(glyphs, &mut renderer.atlas, g.close, "\u{2715}", true);
            if !bar.project {
                button(
                    glyphs,
                    &mut renderer.atlas,
                    g.replace_one,
                    "Replace",
                    has_matches,
                );
                button(
                    glyphs,
                    &mut renderer.atlas,
                    g.replace_all,
                    "All",
                    has_matches,
                );
            } else {
                button(glyphs, &mut renderer.atlas, g.replace_one, "Search", true);
                // Rows open with a click or Return; this button replaces
                // in every file the results list.
                button(
                    glyphs,
                    &mut renderer.atlas,
                    g.replace_all,
                    "Replace All",
                    has_matches,
                );
            }

            // Toggles that show their state: filled and accented when on,
            // quiet when off. They were the same flat rectangle either way.
            for (slot, (label, on)) in [
                ("Aa", bar.options.case_sensitive),
                ("Word", bar.options.whole_word),
                (".*", bar.options.regex),
                ("Project", bar.project),
            ]
            .into_iter()
            .enumerate()
            {
                let r = g.options[slot];
                layout::push_rounded_rect(
                    glyphs,
                    r,
                    5.0,
                    if on {
                        theme.palette_selected
                    } else {
                        theme.tab_hover
                    },
                );
                layout::push_ui_text_centered(
                    glyphs,
                    &mut renderer.atlas,
                    r,
                    label,
                    if on { theme.accent } else { theme.status_text },
                );
            }
            if bar.project {
                for (row, hit) in bar
                    .results
                    .iter()
                    .skip(bar.result_scroll)
                    .take(8)
                    .enumerate()
                {
                    let y = g.results.y + layout::FIND_ROW_HEIGHT * row as f32;
                    let row_rect = Viewport {
                        x: g.results.x + 8.0,
                        y,
                        width: (g.results.width - 16.0).max(0.0),
                        height: layout::FIND_ROW_HEIGHT,
                    };
                    if bar.selected == bar.result_scroll + row {
                        layout::push_rounded_rect(
                            glyphs,
                            Viewport {
                                y: y + 1.0,
                                height: layout::FIND_ROW_HEIGHT - 2.0,
                                ..row_rect
                            },
                            5.0,
                            theme.palette_selected,
                        );
                    }
                    let path = tree
                        .root()
                        .and_then(|root| hit.path.strip_prefix(root).ok())
                        .unwrap_or(&hit.path);
                    // Where it is, then what it says, told apart by colour
                    // rather than run together into one monospace string.
                    let where_it_is = format!("{}:{}", path.display(), hit.line + 1);
                    let width = layout::ui_text_width(&mut renderer.atlas, &where_it_is);
                    layout::push_ui_text(
                        glyphs,
                        &mut renderer.atlas,
                        Viewport {
                            x: row_rect.x + 8.0,
                            ..row_rect
                        },
                        &where_it_is,
                        theme.accent,
                    );
                    layout::push_ui_text(
                        glyphs,
                        &mut renderer.atlas,
                        Viewport {
                            x: row_rect.x + 20.0 + width,
                            width: (row_rect.width - 28.0 - width).max(0.0),
                            ..row_rect
                        },
                        hit.snippet.trim(),
                        theme.sidebar_text,
                    );
                }
                if bar.searching {
                    layout::push_ui_text(
                        glyphs,
                        &mut renderer.atlas,
                        Viewport {
                            x: g.results.x + 16.0,
                            ..g.results
                        },
                        "Searching project\u{2026}",
                        theme.status_text,
                    );
                }
            }
        }

        if let Some(rect) = sidebar_rect {
            let edit_text = sidebar_edit.as_ref().map(|e| e.field.rope.to_string());
            let edit = sidebar_edit
                .as_ref()
                .zip(edit_text.as_deref())
                .map(|(e, text)| layout::SidebarEdit {
                    row: e.row,
                    depth: e.depth,
                    replaces: e.replaces(),
                    is_dir: matches!(e.kind, SidebarEditKind::NewFolder)
                        || matches!(&e.kind, SidebarEditKind::Rename(path) if path.is_dir()),
                    text,
                    cursor: e.field.cursor(),
                    selection: e.field.selection(),
                });
            tree.set_preview(ignore_preview(docs.active(), completion.as_ref(), tree));
            layout::build_sidebar_with_edit(
                tree,
                *git_open,
                edit,
                &mut renderer.atlas,
                rect,
                theme,
                glyphs,
            );
            if *git_open {
                git.draw_sidebar(&mut renderer.atlas, rect, theme, glyphs);
            } else if let Some(drag) = tree_drag.as_ref().filter(|d| d.active && d.valid) {
                // Where it would land. A drag with no target drawn is a drag
                // you have to guess at.
                let band = match drag.over {
                    Some(index) => Viewport {
                        x: rect.x + 4.0,
                        y: rect.y
                            + layout::SIDEBAR_HEADER_HEIGHT
                            + (index.saturating_sub(tree.scroll)) as f32
                                * layout::SIDEBAR_ROW_HEIGHT,
                        width: (rect.width - 8.0).max(0.0),
                        height: layout::SIDEBAR_ROW_HEIGHT,
                    },
                    // The project root: the whole column below the tree.
                    None => Viewport {
                        x: rect.x + 4.0,
                        y: rect.y + layout::SIDEBAR_HEADER_HEIGHT,
                        width: (rect.width - 8.0).max(0.0),
                        height: (rect.height - layout::SIDEBAR_HEADER_HEIGHT).max(0.0),
                    },
                };
                layout::push_focus_ring(glyphs, band, 5.0, 1.5);
            }
        }

        if let Some(rect) = terminal_rect {
            crate::platform::terminal::draw(
                terminal_panel,
                &mut renderer.atlas,
                rect,
                theme,
                glyphs,
            );
        }

        // Status line along the bottom, spanning the full width.
        let (y, status_height) = (status_rect.y, status_rect.height);
        layout::push_rect(
            glyphs,
            &renderer.atlas,
            [status_rect.x, y],
            [status_rect.width, status_height],
            theme.status_background,
        );
        let (line, column) = buffer.cursor_position();

        // A transient note (save result, open error) takes over the status
        // line briefly, then yields back to the steady-state readout.
        let note = match message {
            Some((text, at)) if at.elapsed() < Duration::from_secs(4) => Some(text.clone()),
            _ => {
                *message = None;
                None
            }
        };

        let name = buffer
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .or_else(|| buffer.label.clone())
            .unwrap_or_else(|| "Untitled".into());
        // The diagnostic under the caret, or the file's counts.
        let (diag_note, diag_counts) = {
            let list =
                buffer
                    .path
                    .as_ref()
                    .zip(lsp_language(buffer))
                    .and_then(|(path, language)| {
                        lsp.get(&crate::lsp::servers::server_key(language))
                            .and_then(|s| s.diagnostics.get(path))
                    });
            match list {
                Some(list) if !list.is_empty() => {
                    let (line, _) = buffer.cursor_position();
                    let here = list
                        .iter()
                        .find(|d| (d.start.line as usize..=d.end.line as usize).contains(&line))
                        .map(|d| d.message.lines().next().unwrap_or("").to_owned());
                    let errors = list
                        .iter()
                        .filter(|d| d.severity == crate::lsp::Severity::Error)
                        .count();
                    let warnings = list.len() - errors;
                    (here, format!("✕ {errors}  ⚠ {warnings}     "))
                }
                _ => (None, String::new()),
            }
        };
        let blamed = blame
            .as_ref()
            .filter(|(id, l, text)| *id == buffer.id() && *l == line && !text.is_empty())
            .map(|(_, _, text)| format!("   ·   {text}"))
            .unwrap_or_default();
        let status = note.or(diag_note).unwrap_or_else(|| {
            format!(
                "{}   {}{}{}",
                name,
                if buffer.is_read_only() {
                    "Read-only: over 512 MB"
                } else if buffer.is_dirty() {
                    "Unsaved changes"
                } else {
                    "All changes saved"
                },
                // Known only from drawing: finding such a line up front
                // would mean scanning the whole file.
                if *unshaped_on_screen {
                    "   Long lines drawn without shaping"
                } else {
                    ""
                },
                blamed
            )
        });
        let detail = if std::env::var_os("CRC_SHOW_LATENCY").is_some() {
            format!("{}  {:?}", latency.summary(), worst)
        } else {
            let branch = git.branch_status();
            format!(
                "{}{}{}Ln {}, Col {}     {}     {}",
                if claude.as_ref().is_some_and(|c| c.is_connected()) {
                    "✻ Claude     "
                } else {
                    ""
                },
                if branch.is_empty() {
                    String::new()
                } else {
                    format!("{branch}     ")
                },
                diag_counts,
                line + 1,
                column + 1,
                buffer.disk_format().label(),
                buffer.disk_format().line_ending_label()
            )
        };
        let right_width = 320.0f32.min(status_rect.width * 0.55);
        layout::push_ui_text(
            glyphs,
            &mut renderer.atlas,
            Viewport {
                x: 16.0,
                width: (status_rect.width - right_width - 32.0).max(0.0),
                ..status_rect
            },
            &status,
            theme.status_text,
        );
        layout::push_ui_text(
            glyphs,
            &mut renderer.atlas,
            Viewport {
                x: status_rect.width - right_width,
                width: (right_width - 12.0).max(0.0),
                ..status_rect
            },
            &detail,
            theme.status_text,
        );
        let advance = renderer.atlas.metrics.advance;

        // Go-to-line takes over the status line: it is a line number, and
        // that is where line numbers already live.
        let prompt = goto
            .as_ref()
            .map(|field| ("Go to line: ", field))
            .or_else(|| rename.as_ref().map(|r| ("Rename to: ", &r.field)));
        if let Some((label, field)) = prompt {
            let text = field.rope.to_string();
            layout::push_rect(
                glyphs,
                &renderer.atlas,
                [0.0, y],
                [viewport.width, status_height],
                theme.find_background,
            );
            layout::push_text(
                glyphs,
                &mut renderer.atlas,
                advance,
                y,
                label,
                theme.gutter_text,
            );
            let x = advance * (label.len() as f32 + 1.0);
            layout::push_text(glyphs, &mut renderer.atlas, x, y, &text, theme.text);
            layout::push_rect(
                glyphs,
                &renderer.atlas,
                [x + text.chars().count() as f32 * advance, y],
                [(advance * 0.15).max(1.0), status_height],
                theme.cursor,
            );
        }

        // The palette floats over everything, so it is drawn last.
        if let Some((query, selected)) = palette {
            let text = query.rope.to_string();
            let rows: Vec<layout::PaletteRow> = palette_rows(
                &PaletteSources {
                    finder,
                    commands: command_list,
                    symbols: symbol_list,
                    root: tree.root(),
                    branches: branch_list.as_deref(),
                },
                &text,
            )
            .into_iter()
            .map(|(row, _)| row)
            .collect();
            let rect = layout::palette_rect(viewport, rows.len());
            layout::push_rect(
                glyphs,
                &renderer.atlas,
                [viewport.x, viewport.y],
                [viewport.width, viewport.height],
                theme.scrim,
            );
            layout::build_palette(
                layout::PaletteView {
                    rows: &rows,
                    heading: palette_heading(&text, branch_list.is_some()).0,
                    empty: palette_heading(&text, branch_list.is_some()).1,
                    placeholder: if branch_list.is_some() {
                        "Branch name"
                    } else {
                        "Find a file  ·  > commands  ·  @ symbols  ·  # in project"
                    },
                    action: if branch_list.is_some() {
                        "Switch"
                    } else if commands::query(&text).is_some() {
                        "Run"
                    } else {
                        "Open"
                    },
                    query: &text,
                    selected: *selected,
                    scroll: *palette_scroll,
                    cursor: query.cursor(),
                    selection: query.selection(),
                },
                &mut renderer.atlas,
                rect,
                theme,
                glyphs,
            );
        }

        let background = theme.background;
        let timing = renderer.draw(layer, glyphs, (viewport.width, viewport.height), background);
        if timing.is_some() {
            *drew_once = true;
        }
        timing
    }
}

/// Holds the view so the delegate can route opened documents to it.
pub struct DelegateIvars {
    view: Retained<EditorView>,
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - AppDelegate does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CrcAppDelegate"]
    #[ivars = DelegateIvars]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        /// Finder double-clicks and `open -a crc <file>` arrive here as
        /// an Apple Event, not as argv. Without this the bundle would launch
        /// but silently ignore the file it was asked to open.
        #[unsafe(method(application:openFile:))]
        fn open_file(&self, _app: &NSApplication, filename: &NSString) -> bool {
            let view = &self.ivars().view;
            let opened = view.load_path(&filename.to_string());
            // Without this the window keeps whatever title it had, which for
            // a freshly launched app is "Untitled" over somebody's file.
            view.sync_title();
            view.reparse();
            view.request_redraw();
            view.pump();
            opened
        }

        /// One window, and closing it means you are done.
        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_close(&self, _app: &NSApplication) -> bool {
            true
        }

        /// Cmd-Q is the other way to lose unsaved work, so it asks too.
        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _app: &NSApplication) -> NSApplicationTerminateReply {
            let view = &self.ivars().view;
            if !view.confirm_discard_all() {
                return NSApplicationTerminateReply::TerminateCancel;
            }
            // Written only once quitting is certain: recording a session for
            // a quit the user then cancelled would overwrite the real one.
            let (session, ephemeral) = {
                let mut state = view.ivars().state.borrow_mut();
                (state.quit_session.take(), state.ephemeral_session)
            };
            if !ephemeral && let Some(session) = session {
                session.save();
            }
            for server in view.ivars().state.borrow_mut().lsp.values_mut() {
                server.shutdown();
            }
            view.claude_shutdown();
            NSApplicationTerminateReply::TerminateNow
        }
    }
);

/// Largest file that gets syntax highlighting.
///
/// Set from measurement, not taste. With incremental parsing an edit costs
/// (examples/syntax_latency.rs, M2 Pro):
///
/// ```text
///   256 KiB   0.34 ms
///     1 MiB   1.38 ms
///     4 MiB   9.86 ms   <- past the 8.33 ms frame budget
/// ```
///
/// 2 MiB keeps a keystroke comfortably inside a frame. Past that,
/// highlighting is dropped rather than the responsiveness, because an editor
/// that stutters is worse than one without colours.
///
/// The *first* parse of a file is still linear (~190 ms at 2 MiB), but that
/// is paid once on open rather than per keystroke.
fn reparse_budget() -> usize {
    2 * 1024 * 1024
}

/// Whether a document indents with spaces, and how wide a level is, from
/// its first indented lines. Spaces and four when nothing says otherwise.
fn indent_style(rope: &crate::text::rope::Rope) -> (bool, u32) {
    let lines = rope.len_lines().min(400);
    let mut widths = Vec::new();
    for line in 0..lines {
        let start = rope.line_to_byte(line);
        let end = rope.len_bytes().min(start + 64);
        let head = rope.slice_to_string(start..end);
        if head.starts_with('\t') {
            return (false, 4);
        }
        let n = head.chars().take_while(|c| *c == ' ').count();
        if n > 0 && head.chars().nth(n).is_some_and(|c| !c.is_whitespace()) {
            widths.push(n);
        }
    }
    let unit = widths.iter().copied().min().unwrap_or(4);
    (true, if unit == 2 { 2 } else { 4 })
}

/// Sets whether `buffer` wraps, and at what width, for a text area `cols`
/// columns wide. One column is left for the caret at the end of a row.
fn apply_wrap(buffer: &mut Buffer, setting: crate::platform::settings::WordWrap, cols: usize) {
    use crate::platform::settings::WordWrap;
    let on = buffer.wrap_choice.unwrap_or(match setting {
        WordWrap::On => true,
        WordWrap::Off => false,
        WordWrap::Auto => wraps_by_default(buffer),
    });
    let wrap = on.then(|| cols.saturating_sub(1).max(crate::text::wrap::MIN_COLUMNS));
    if buffer.wrap != wrap {
        buffer.wrap = wrap;
        buffer.scroll_row = 0;
    }
}

/// Prose wraps unless told otherwise; code does not.
fn wraps_by_default(buffer: &Buffer) -> bool {
    matches!(
        buffer.extension().as_deref(),
        Some("md" | "markdown" | "mdx" | "txt" | "text" | "rst" | "adoc" | "org")
    )
}

/// The language a buffer's server speaks, from its extension.
fn lsp_language(buffer: &Buffer) -> Option<Language> {
    buffer
        .extension()
        .and_then(|e| Language::from_extension(&e))
        .filter(|_| buffer.path.is_some())
}

/// The server that knows `buffer`, when there is one and it is ready.
/// The chip icon for a suggestion: what it is when that is known,
/// otherwise where it came from.
fn completion_icon(
    candidate: &crate::complete::Candidate,
    items: &[crate::lsp::Completion],
) -> char {
    use crate::complete::Source;
    use crate::project::icons;
    match candidate.source {
        Source::Server => candidate
            .server
            .and_then(|i| items.get(i))
            .map_or(icons::SYMBOL_VARIABLE, |item| {
                icons::for_lsp_kind(item.kind)
            }),
        Source::Symbol => icons::for_kind(candidate.why.split(' ').next().unwrap_or("")),
        Source::History => icons::HISTORY,
        Source::Word => icons::SYMBOL_TEXT,
        Source::Path if candidate.insert.ends_with('/') => icons::FOLDER_OUTLINE,
        Source::Path => icons::FILE,
    }
}

/// While a `.gitignore` is being edited: what the line at the caret would
/// ignore, with the picked suggestion in place of what is typed, so the
/// Explorer shows the effect before the file is saved.
fn ignore_preview(
    buffer: &Buffer,
    completion: Option<&CompletionPopup>,
    tree: &crate::project::tree::Tree,
) -> std::collections::HashSet<std::path::PathBuf> {
    let mut found = std::collections::HashSet::new();
    let Some(file) = buffer
        .path
        .as_deref()
        .filter(|f| crate::complete::is_ignore_file(f))
    else {
        return found;
    };
    let open = completion.is_some_and(|c| c.buffer == buffer.id() && c.path);
    if !buffer.is_dirty() && !open {
        return found;
    }
    let caret = buffer.cursor();
    let line = buffer.rope.byte_to_line(caret);
    let start = buffer.rope.line_to_byte(line);
    let mut text = buffer.rope.line(line);
    if let Some(popup) = completion.filter(|c| c.buffer == buffer.id() && c.path)
        && let Some(picked) = popup.shown.get(popup.selected)
        && popup.anchor >= start
    {
        text = format!(
            "{}{}",
            buffer.rope.slice_to_string(start..popup.anchor),
            picked.insert
        );
    }
    let Some(pattern) = crate::complete::glob::parse(&text) else {
        return found;
    };
    let base = if file.ends_with(".git/info/exclude") {
        file.parent().and_then(Path::parent).and_then(Path::parent)
    } else {
        file.parent()
    };
    let Some(base) = base else {
        return found;
    };
    for row in tree.rows() {
        let Ok(relative) = row.path.strip_prefix(base) else {
            continue;
        };
        if pattern.matches(&relative.to_string_lossy(), row.is_dir) {
            found.insert(row.path.clone());
        }
    }
    found
}

/// When the text before the caret is a path, where it points.
fn completion_path_query(state: &State) -> Option<crate::complete::PathQuery> {
    let buffer = state.docs.active();
    let caret = buffer.cursor();
    let line_start = buffer.rope.line_to_byte(buffer.rope.byte_to_line(caret));
    let before = buffer.rope.slice_to_string(line_start..caret);
    crate::complete::path_query(&before, buffer.path.as_deref(), state.tree.root())
}

/// What completion history and the index are kept per: the project, or the
/// file's folder when there is none.
fn project_key(state: &State) -> Option<std::path::PathBuf> {
    state.tree.root().map(Path::to_path_buf).or_else(|| {
        state
            .docs
            .active()
            .path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
    })
}

/// The history's language key: the extension, or the name of a file that
/// has none (`.gitignore`, `Makefile`).
fn completion_language(buffer: &Buffer) -> String {
    buffer
        .extension()
        .map(|e| e.to_string())
        .unwrap_or_else(|| {
            buffer
                .path
                .as_deref()
                .and_then(Path::file_name)
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
        })
}

fn lsp_server_for<'a>(state: &'a State, buffer: &Buffer) -> Option<&'a crate::lsp::client::Server> {
    let language = lsp_language(buffer)?;
    state
        .lsp
        .get(&crate::lsp::servers::server_key(language))
        .filter(|s| s.is_ready())
}

/// The review in the active tab, when it is one.
fn active_review(state: &State) -> Option<&crate::platform::claude::Review> {
    state
        .claude
        .as_ref()?
        .reviews
        .get(&state.docs.active().id())
}

/// The active document's selection as Claude is told it: the caret as an
/// empty selection, nothing for a document without a file. Very long
/// selections are cut at 1 MiB; the range still says where they end.
fn claude_selection(state: &State) -> Option<crate::ide::mcp::Selection> {
    const MAX_TEXT: usize = 1024 * 1024;
    let buffer = state.docs.active();
    if active_review(state).is_some() {
        return None;
    }
    let path = buffer.path.clone()?;
    let range = buffer
        .selection()
        .unwrap_or(buffer.cursor()..buffer.cursor());
    let mut end = range.end.min(range.start + MAX_TEXT);
    let text = loop {
        // A cut inside a character moves back to its start.
        let text = buffer.rope.slice_to_string(range.start..end);
        if end == range.end || !text.ends_with(char::REPLACEMENT_CHARACTER) {
            break text;
        }
        end -= 1;
    };
    Some(crate::ide::mcp::Selection {
        path,
        text,
        start: crate::lsp::position_of(&buffer.rope, range.start),
        end: crate::lsp::position_of(&buffer.rope, range.end),
    })
}

/// Whether `a` and `b` name the same file, by path or by what it resolves to.
fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || std::fs::canonicalize(a)
            .ok()
            .zip(std::fs::canonicalize(b).ok())
            .is_some_and(|(a, b)| a == b)
}

/// The window, as the tools Claude calls see it. Every method borrows the
/// state only for itself, since some of them call back into the view.
struct ClaudeHost<'a>(&'a EditorView);

impl crate::ide::mcp::Host for ClaudeHost<'_> {
    fn diagnostics(
        &self,
        path: Option<&Path>,
    ) -> Vec<(std::path::PathBuf, Vec<crate::lsp::Diagnostic>)> {
        let state = self.0.ivars().state.borrow();
        let mut out: Vec<_> = state
            .lsp
            .values()
            .flat_map(|server| server.diagnostics.iter())
            .filter(|(file, list)| match path {
                Some(path) => same_file(path, file),
                None => !list.is_empty(),
            })
            .map(|(file, list)| (file.clone(), list.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn open_editors(&self) -> Vec<crate::ide::mcp::Editor> {
        let state = self.0.ivars().state.borrow();
        let active = state.docs.active().id();
        let reviews = state.claude.as_ref().map(|c| &c.reviews);
        all_docs(&state)
            .flat_map(|docs| docs.iter())
            .filter(|b| reviews.is_none_or(|r| !r.contains_key(&b.id())))
            .map(|b| crate::ide::mcp::Editor {
                path: b.path.clone(),
                label: b
                    .path
                    .as_deref()
                    .and_then(Path::file_name)
                    .map(|n| n.to_string_lossy().into_owned())
                    .or_else(|| b.label.clone())
                    .unwrap_or_else(|| "Untitled".into()),
                language_id: lsp_language(b)
                    .map(crate::lsp::servers::language_id)
                    .unwrap_or("plaintext")
                    .to_owned(),
                active: b.id() == active,
                dirty: b.is_dirty(),
            })
            .collect()
    }

    fn workspace_folders(&self) -> Vec<std::path::PathBuf> {
        let state = self.0.ivars().state.borrow();
        state
            .claude
            .as_ref()
            .map(|c| vec![c.root().to_path_buf()])
            .unwrap_or_default()
    }

    fn selection(&self) -> Option<crate::ide::mcp::Selection> {
        claude_selection(&self.0.ivars().state.borrow())
    }

    fn document_state(&self, path: &Path) -> Option<(bool, bool)> {
        let state = self.0.ivars().state.borrow();
        all_docs(&state)
            .flat_map(|docs| docs.iter())
            .find(|b| b.path.as_deref().is_some_and(|p| same_file(path, p)))
            .map(|b| (b.is_dirty(), false))
    }

    fn save(&mut self, path: &Path) -> Result<bool, String> {
        let saved = {
            let mut state = self.0.ivars().state.borrow_mut();
            let result = all_docs_mut(&mut state)
                .into_iter()
                .flat_map(|docs| docs.iter_mut())
                .find(|b| b.path.as_deref().is_some_and(|p| same_file(path, p)))
                .map(|b| b.save(None).map(|()| b.path.clone()));
            match result {
                None => return Ok(false),
                Some(Err(e)) => return Err(e.to_string()),
                Some(Ok(saved)) => {
                    state.git.refresh();
                    saved
                }
            }
        };
        if let Some(path) = saved {
            self.0.lsp_flush_changes();
            for server in self.0.ivars().state.borrow_mut().lsp.values_mut() {
                server.did_save(&path);
            }
        }
        self.0.sync_title();
        self.0.request_redraw();
        self.0.pump();
        Ok(true)
    }

    fn open_file(
        &mut self,
        path: &Path,
        start_text: Option<&str>,
        end_text: Option<&str>,
    ) -> Result<(), String> {
        if path.is_dir() || !self.0.load_path(&path.to_string_lossy()) {
            return Err(format!("cannot open {}", path.display()));
        }
        if let Some(start_text) = start_text.filter(|s| !s.is_empty()) {
            let (rows, cols) = self.0.grid();
            let mut state = self.0.ivars().state.borrow_mut();
            let buffer = state.docs.active_mut();
            let text = buffer.rope.to_string();
            if let Some(start) = text.find(start_text) {
                let end = end_text
                    .filter(|e| !e.is_empty())
                    .and_then(|e| text[start..].find(e).map(|i| start + i + e.len()))
                    .unwrap_or(start + start_text.len());
                buffer.select_range(start, end);
                buffer.scroll_to_cursor(rows, cols);
            }
        }
        self.0.sync_title();
        self.0.reparse();
        self.0.request_redraw();
        self.0.pump();
        Ok(())
    }

    fn open_diff(
        &mut self,
        request: crate::json::Value,
        diff: crate::ide::mcp::DiffRequest,
    ) -> Result<(), String> {
        let old = match std::fs::read_to_string(&diff.old_path) {
            Ok(text) => text,
            // A new file: everything is an addition.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("cannot read {}: {e}", diff.old_path.display())),
        };
        let review = crate::platform::claude::Review {
            request,
            tab_name: diff.tab_name.clone(),
            path: diff.new_path.clone(),
            diff: crate::ide::diff::diff(&old, &diff.new_contents),
            proposed: diff.new_contents.clone(),
            decided: None,
            scroll: 0,
        };
        let name = diff
            .new_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| diff.tab_name.clone());
        {
            let mut state = self.0.ivars().state.borrow_mut();
            let Some(bridge) = state.claude.as_mut() else {
                return Err("not connected".into());
            };
            // A second proposal under the same name takes over the tab of
            // the first, which is answered as rejected.
            let earlier = bridge.buffer_for_tab(&diff.tab_name);
            if let Some(id) = earlier {
                bridge.decide(id, false);
                bridge.reviews.remove(&id);
            }
            let reuse = earlier.and_then(|id| state.docs.iter().position(|b| b.id() == id));
            match reuse {
                Some(index) => {
                    state.docs.switch(index);
                    state.docs.active_mut().regenerate(&diff.new_contents);
                }
                None => state.docs.push(Buffer::generated(
                    &format!("✻ {name}"),
                    "txt",
                    &diff.new_contents,
                )),
            }
            let id = state.docs.active().id();
            if let Some(bridge) = state.claude.as_mut() {
                bridge.reviews.insert(id, review);
            }
            // The review takes the editor column, so nothing may cover it.
            state.git.showing_diff = false;
            state.preview = None;
            state.completion = None;
            state.message = Some((
                format!("Claude proposes a change to {name}"),
                Instant::now(),
            ));
            reveal_active_tab(&mut state);
        }
        self.0.sync_title();
        if let Some(window) = self.0.window() {
            window.invalidateCursorRectsForView(self.0);
        }
        self.0.request_redraw();
        self.0.pump();
        self.0.resume_display_link();
        Ok(())
    }

    fn close_tab(&mut self, tab_name: &str) -> bool {
        let id = self
            .0
            .ivars()
            .state
            .borrow()
            .claude
            .as_ref()
            .and_then(|c| c.buffer_for_tab(tab_name));
        match id {
            Some(id) => {
                self.0.claude_close_review(id);
                true
            }
            None => false,
        }
    }

    fn close_all_diffs(&mut self) -> usize {
        let ids: Vec<u64> = self
            .0
            .ivars()
            .state
            .borrow()
            .claude
            .as_ref()
            .map(|c| c.reviews.keys().copied().collect())
            .unwrap_or_default();
        for &id in &ids {
            self.0.claude_close_review(id);
        }
        ids.len()
    }
}

/// Whether typing goes to a field (palette, find, go to line, a sidebar
/// name, the commit message) rather than to the document.
fn field_has_keys(state: &State) -> bool {
    state.sidebar_edit.is_some()
        || state.rename.is_some()
        || state.palette.is_some()
        || state.find.is_some()
        || state.goto.is_some()
        || (state.git_open && state.git_focus)
}

/// The field that receives text input and editing menu commands.
fn focused_buffer(state: &State) -> &Buffer {
    if let Some(edit) = &state.sidebar_edit {
        &edit.field
    } else if let Some(rename) = &state.rename {
        &rename.field
    } else if state.git_open {
        &state.git.message
    } else if let Some(field) = &state.goto {
        field
    } else if let Some((query, _)) = &state.palette {
        query
    } else if let Some(bar) = &state.find {
        if bar.replacing {
            &bar.replacement
        } else {
            &bar.query
        }
    } else {
        state.docs.active()
    }
}

/// Draws a pane that does not have the keyboard: its tab bar, breadcrumbs
/// and document, with highlighting but without caret, find or overlays.
#[allow(clippy::too_many_arguments)]
fn draw_other_pane(
    store: &mut PaneStore,
    rects: &layout::PaneChrome,
    tree: &Tree,
    syntax: &mut SyntaxStore,
    responses: &HashMap<u64, crate::http::curl::View>,
    marks: &HashMap<u64, GutterState>,
    renderer: &mut Renderer,
    theme: &Theme,
    glyphs: &mut Vec<GlyphInstance>,
    word_wrap: crate::platform::settings::WordWrap,
) {
    let m = renderer.atlas.metrics;
    let gutter = layout::gutter_width(store.docs.active(), &renderer.atlas);
    let (rows, cols) = (
        rects.text.rows(m.line_height),
        rects.text.columns(m.advance, gutter),
    );
    apply_wrap(store.docs.active_mut(), word_wrap, cols);
    store.docs.active_mut().clamp_scroll(rows, cols);
    layout::build_tab_bar_in(
        &store.docs,
        store.tab_scroll,
        None,
        false,
        &mut renderer.atlas,
        rects.tabs,
        theme,
        glyphs,
        &mut store.tab_hits,
    );
    let buffer = store.docs.active();
    layout::build_breadcrumbs(
        buffer,
        tree,
        &mut renderer.atlas,
        rects.breadcrumbs,
        theme,
        glyphs,
    );
    let mut text = rects.text;
    if let Some(view) = responses.get(&buffer.id()) {
        let (strip, rest) = rects.text.split_top(layout::RESPONSE_STRIP_HEIGHT);
        layout::build_response_strip(view, &mut renderer.atlas, strip, theme, glyphs);
        text = rest;
    }
    if store.docs.is_home() {
        let mut hits = Vec::new();
        layout::build_home(
            glyphs,
            &mut renderer.atlas,
            text,
            theme,
            tree.root(),
            &[],
            &mut hits,
        );
        return;
    }
    if store.preview.is_some() && default_preview(buffer).is_some() {
        let source = buffer.rope.to_string();
        let blocks = markdown::parse_spanned(&source);
        let scroll = store
            .preview
            .unwrap_or(0)
            .min(blocks.len().saturating_sub(1));
        let mut hits = Vec::new();
        layout::build_markdown_appending(
            &blocks,
            &source,
            None,
            None,
            scroll,
            &mut renderer.atlas,
            text,
            theme,
            glyphs,
            &mut hits,
        );
        return;
    }
    let mut spans = Vec::new();
    if syntax.has(buffer.id()) {
        let total = buffer.rope.len_lines();
        let std::ops::Range {
            start: first,
            end: last,
        } = layout::visible_lines(buffer, text, renderer.atlas.metrics.line_height);
        let from = buffer.rope.line_to_byte(first);
        let to = if last < total {
            buffer.rope.line_to_byte(last)
        } else {
            buffer.rope.len_bytes()
        };
        spans.extend(syntax.spans_with(buffer.id(), from..to, |r| buffer.rope.slice_to_string(r)));
    }
    layout::build_text_appending(
        buffer,
        &mut renderer.atlas,
        text,
        theme,
        "",
        None,
        &spans,
        false,
        false,
        glyphs,
    );
    if let Some(entry) = marks.get(&buffer.id()) {
        layout::push_gutter_marks(glyphs, &renderer.atlas, buffer, text, theme, &entry.marks);
    }
    // The seam between panes.
    layout::push_rect(
        glyphs,
        &renderer.atlas,
        [rects.tabs.x - layout::PANE_GAP, rects.tabs.y],
        [
            layout::PANE_GAP,
            rects.text.y + rects.text.height - rects.tabs.y,
        ],
        theme.divider,
    );
}

/// Every rectangle the pointer can land on this frame, from the same
/// functions that draw them, in hit-testing order. Mouse handling, cursor
/// shapes and scripted clicks all read this; none of them measures anything
/// on its own.
fn frame_of(state: &mut State) -> Frame {
    let chrome = chrome_of(state);
    let mut frame = Frame::default();
    let State {
        tree,
        renderer,
        docs,
        responses,
        claude,
        terminal,
        git_open,
        tab_hits,
        sidebar_edit,
        ..
    } = state;
    frame.push(Hit::ToolbarSidebar, layout::toolbar_sidebar(chrome.toolbar));
    frame.push(
        Hit::ToolbarProject,
        layout::toolbar_project(tree, &mut renderer.atlas, chrome.toolbar),
    );
    frame.push(Hit::ToolbarSearch, layout::toolbar_search(chrome.toolbar));
    let terminal_button = layout::toolbar_terminal(chrome.toolbar);
    if terminal_button.width > 0.0 {
        frame.push(Hit::ToolbarTerminal, terminal_button);
    }
    // Before the editor's regions, which the band overlaps by half.
    if let Some(rect) = chrome.terminal {
        frame.push(
            Hit::TerminalDivider,
            Viewport {
                y: rect.y - DIVIDER_GRAB,
                height: DIVIDER_GRAB * 2.0,
                ..rect
            },
        );
    }
    if let Some(rect) = chrome.sidebar {
        frame.push(
            Hit::SidebarDivider,
            Viewport {
                x: rect.x + rect.width - DIVIDER_GRAB,
                y: rect.y,
                width: DIVIDER_GRAB * 2.0,
                height: rect.height,
            },
        );
        let (explorer, source) = layout::sidebar_switcher(rect);
        frame.push(Hit::SidebarExplorer, explorer);
        frame.push(Hit::SidebarSourceControl, source);
        if !*git_open {
            let (_, actions) = layout::sidebar_actions(rect);
            for (index, action) in actions.into_iter().enumerate() {
                frame.push(Hit::SidebarAction(index), action);
            }
            // Rows as drawn: an inserted name field shifts the rows under
            // it, and is not itself a tree row.
            let inserted = sidebar_edit.as_ref().is_some_and(|e| !e.replaces());
            let total = tree.len() + usize::from(inserted);
            let visible_rows = layout::sidebar_rows(rect);
            let first = tree.scroll.min(total.saturating_sub(1));
            for visible in first..(first + visible_rows).min(total) {
                if sidebar_edit.as_ref().is_some_and(|e| e.row == visible) {
                    continue;
                }
                let index = match sidebar_edit.as_ref() {
                    Some(e) if inserted && visible > e.row => visible - 1,
                    _ => visible,
                };
                frame.push(
                    Hit::SidebarRow(index),
                    Viewport {
                        x: rect.x,
                        y: rect.y
                            + layout::SIDEBAR_HEADER_HEIGHT
                            + (visible - first) as f32 * layout::SIDEBAR_ROW_HEIGHT,
                        width: rect.width,
                        height: layout::SIDEBAR_ROW_HEIGHT,
                    },
                );
            }
        }
    }
    for (index, pane) in &chrome.others {
        frame.push(Hit::Pane(*index), pane.whole());
    }
    for hit in tab_hits.iter() {
        if hit.index == docs.active_index() {
            frame.push(
                Hit::TabClose(hit.index),
                Viewport {
                    x: hit.close_x0,
                    width: hit.close_x1 - hit.close_x0,
                    ..chrome.tabs
                },
            );
        }
        frame.push(
            Hit::Tab(hit.index),
            Viewport {
                x: hit.x0,
                width: hit.x1 - hit.x0,
                ..chrome.tabs
            },
        );
    }
    frame.push(Hit::TabStrip, chrome.tabs);
    if let Some(strip) = chrome.response
        && let Some(view) = responses.get(&docs.active().id())
    {
        let headers = format!("Headers {}", view.header_count());
        let segments =
            layout::response_segments(&mut renderer.atlas, strip, ["Body", &headers, "Request"]);
        for (index, segment) in segments.into_iter().enumerate() {
            frame.push(Hit::ResponseSegment(index), segment);
        }
    }
    if let Some(strip) = chrome.response
        && claude
            .as_ref()
            .and_then(|c| c.reviews.get(&docs.active().id()))
            .is_some_and(|review| review.decided.is_none())
    {
        let [accept, reject] = crate::platform::claude::review_buttons(&mut renderer.atlas, strip);
        frame.push(Hit::ReviewAccept, accept);
        frame.push(Hit::ReviewReject, reject);
    }
    if let Some(find) = chrome.find {
        frame.push(Hit::Find, find);
    }
    if let Some(rect) = chrome.terminal {
        let (header, _) = crate::platform::terminal::split(rect);
        let (tabs, new) =
            crate::platform::terminal::header_hits(terminal, &mut renderer.atlas, header);
        for (index, (tab, close)) in tabs.into_iter().enumerate() {
            frame.push(Hit::TerminalClose(index), close);
            frame.push(Hit::TerminalTab(index), tab);
        }
        frame.push(Hit::TerminalNew, new);
        // The screen first, so a script's offsets land on rows and columns;
        // the padding around it is the terminal too.
        frame.push(Hit::Terminal, crate::platform::terminal::split(rect).1);
        frame.push(Hit::Terminal, rect);
    }
    frame.push(Hit::Text, chrome.text);
    frame.push(Hit::Status, chrome.status);
    frame
}

/// Shared layout for rendering and interaction.
fn chrome_of(state: &State) -> Chrome {
    // On a whole device pixel. The divider is dragged to wherever the pointer
    // is, and everything right of it is positioned from it, so a width of
    // 240.37 would put every glyph in the editor between two pixels.
    let sidebar = state.sidebar.then(|| {
        state
            .renderer
            .atlas
            .metrics
            .snap(state.sidebar_width.clamp(SIDEBAR_MIN, SIDEBAR_MAX))
    });
    // Two rows: find and replace. The option chips used to need a third,
    // spanning the window with four small toggles and nothing else on it.
    let find_rows = state.find.as_ref().map_or(0, |bar| {
        2 + if bar.project {
            bar.results.len().min(8)
        } else {
            0
        }
    });
    let response =
        state.responses.contains_key(&state.docs.active().id()) || active_review(state).is_some();
    Chrome::with_panes(
        state.viewport,
        sidebar,
        find_rows,
        response,
        pane_count(state),
        state.focused_pane,
        state.terminal.open.then_some(state.terminal.height),
    )
}

/// `recent` with `root`, if any, moved to the front, deduplicated by
/// canonical path and cut to the session's limit.
fn with_recent(
    mut recent: Vec<std::path::PathBuf>,
    root: Option<&Path>,
) -> Vec<std::path::PathBuf> {
    if let Some(root) = root {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        recent.retain(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()) != root);
        recent.insert(0, root);
    }
    recent.truncate(crate::platform::session::RECENT_LIMIT);
    recent
}

/// Asks whether to bring back documents written out by the panic hook.
fn ask_to_restore(mtm: MainThreadMarker, documents: &[recovery::Recovered]) -> bool {
    let text = restore_summary(documents);
    // A test instance cannot answer a modal: it says what it would have
    // asked, and takes the answer from the environment.
    if std::env::var_os("CRC_SELFTEST").is_some() {
        eprintln!("crc: restore prompt: {text}");
        return std::env::var("CRC_RESTORE_ANSWER").map_or(true, |a| a != "discard");
    }
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Warning);
    alert.setMessageText(&NSString::from_str("crc quit unexpectedly."));
    alert.setInformativeText(&NSString::from_str(&text));
    alert.addButtonWithTitle(&NSString::from_str("Restore"));
    alert.addButtonWithTitle(&NSString::from_str("Discard"));
    const FIRST: isize = 1000;
    alert.runModal() == FIRST
}

/// The restore prompt's text: which documents came back, by name and
/// folder, so the choice is about something you can recognise.
fn restore_summary(documents: &[recovery::Recovered]) -> String {
    const LISTED: usize = 8;
    let names: Vec<String> = documents
        .iter()
        .take(LISTED)
        .map(|d| match &d.path {
            Some(path) => {
                let name = path.file_name().map_or_else(
                    || path.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                match path.parent().and_then(|p| p.file_name()) {
                    Some(folder) => format!("{name} (in {})", folder.to_string_lossy()),
                    None => name,
                }
            }
            None => "Untitled".to_string(),
        })
        .collect();
    let mut list = names.join(", ");
    if documents.len() > LISTED {
        list.push_str(&format!(" and {} more", documents.len() - LISTED));
    }
    let what = if documents.len() == 1 {
        "Unsaved changes to 1 document were".to_string()
    } else {
        format!("Unsaved changes to {} documents were", documents.len())
    };
    format!(
        "{what} written to disk as the app went down: {list}. Restoring opens \
         the recovered text as unsaved changes; nothing on disk is overwritten \
         until you save. Discarding deletes the recovered copies."
    )
}

/// What choosing a palette row does.
enum Pick {
    File(std::path::PathBuf),
    /// A menu item's action.
    Command(Sel),
    /// A definition: in another file, or in the active document (`None`),
    /// and its zero-based line.
    Symbol(Option<std::path::PathBuf>, u32),
    Branch(String),
    NewBranch(String),
}

/// The branch picker's rows: branches matching the query, then a row to
/// create one named by the query when no branch has that name.
fn branch_rows(
    branches: &[crate::project::git::Branch],
    query: &str,
) -> Vec<(layout::PaletteRow, Pick)> {
    let needle: Vec<char> = query.trim().to_lowercase().chars().collect();
    let mut hits: Vec<(i32, usize)> = branches
        .iter()
        .enumerate()
        .filter_map(|(i, b)| {
            crate::project::finder::score(&b.name.to_lowercase(), &needle).map(|(p, _)| (p, i))
        })
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut rows: Vec<(layout::PaletteRow, Pick)> = hits
        .into_iter()
        .map(|(_, i)| {
            let b = &branches[i];
            (
                layout::PaletteRow {
                    icon: None,
                    title: b.name.clone(),
                    detail: match (&b.upstream, b.current) {
                        (_, true) => "current branch".into(),
                        (Some(up), false) => format!("tracks {up}"),
                        (None, false) => "local only".into(),
                    },
                    shortcut: String::new(),
                },
                Pick::Branch(b.name.clone()),
            )
        })
        .collect();
    let name = query.trim();
    if !name.is_empty() && !branches.iter().any(|b| b.name == name) {
        rows.push((
            layout::PaletteRow {
                icon: None,
                title: format!("Create branch \u{201c}{name}\u{201d}"),
                detail: "from here, keeping your changes".into(),
                shortcut: String::new(),
            },
            Pick::NewBranch(name.to_owned()),
        ));
    }
    rows
}

/// What the palette lists from.
struct PaletteSources<'a> {
    finder: &'a Finder,
    commands: &'a [Command],
    symbols: &'a symbols::Symbols,
    root: Option<&'a Path>,
    /// Set while picking a branch.
    branches: Option<&'a [crate::project::git::Branch]>,
}

fn palette_sources(state: &State) -> PaletteSources<'_> {
    PaletteSources {
        finder: &state.finder,
        commands: &state.commands,
        symbols: &state.symbols,
        root: state.tree.root(),
        branches: state.branch_list.as_deref(),
    }
}

/// The palette's heading and what it says when nothing matches, by mode.
fn palette_heading(query: &str, branches: bool) -> (&'static str, &'static str) {
    if branches {
        return ("Switch branch, or type a new name", "No branches");
    }
    match symbols::query(query) {
        Some((symbols::Scope::Document, _)) => ("Symbols in this file", "No matching symbols"),
        Some((symbols::Scope::Project, "")) => ("Symbols in the project", "Type a name"),
        Some((symbols::Scope::Project, _)) => ("Symbols in the project", "No matching symbols"),
        None if commands::query(query).is_some() => ("Commands", "No matching commands"),
        None => ("Project files", "No matching files"),
    }
}

/// The palette's rows for `query`: menu commands after a leading `>`,
/// project files otherwise.
fn palette_rows(sources: &PaletteSources<'_>, query: &str) -> Vec<(layout::PaletteRow, Pick)> {
    let PaletteSources {
        finder,
        commands: command_list,
        symbols: symbol_list,
        root,
        branches,
    } = *sources;
    if let Some(branches) = branches {
        return branch_rows(branches, query);
    }
    if let Some((scope, needle)) = symbols::query(query) {
        return symbol_list
            .search(scope, needle)
            .into_iter()
            .map(|hit| {
                (
                    layout::PaletteRow {
                        icon: Some(crate::project::icons::for_kind(&hit.kind)),
                        title: hit.name.clone(),
                        detail: symbols::detail(&hit, root),
                        shortcut: String::new(),
                    },
                    Pick::Symbol(hit.path, hit.line),
                )
            })
            .collect();
    }
    if let Some(query) = commands::query(query) {
        return commands::search(command_list, query)
            .into_iter()
            .take(layout::PALETTE_RESULTS)
            .map(|index| {
                let command = &command_list[index];
                (
                    layout::PaletteRow {
                        icon: None,
                        title: command.title.clone(),
                        detail: command.group.clone(),
                        shortcut: command.shortcut.clone(),
                    },
                    Pick::Command(command.action),
                )
            })
            .collect();
    }
    finder
        .search(query, layout::PALETTE_RESULTS)
        .iter()
        .filter_map(|hit| {
            let row = layout::palette_file_row(finder, hit)?;
            let path = finder.entry(hit.index)?.path.clone();
            Some((row, Pick::File(path)))
        })
        .collect()
}

/// Right-click menu for a tab.
fn tab_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    let add = |title: &str, action: objc2::runtime::Sel| {
        let item = NSMenuItem::alloc(mtm);
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                item,
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(""),
            )
        };
        menu.addItem(&item);
    };
    add("Close Tab", sel!(closeContextTab:));
    add("Close Other Tabs", sel!(closeOtherTabs:));
    add("Close All Tabs", sel!(closeAllTabs:));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    add("Move Tab Left", sel!(moveContextTabLeft:));
    add("Move Tab Right", sel!(moveContextTabRight:));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    add("Copy Path", sel!(copyContextTabPath:));
    add("Reveal in Finder", sel!(revealContextTab:));
    menu
}

fn menu_item(mtm: MainThreadMarker, title: &str, action: Sel) -> Retained<NSMenuItem> {
    let item = NSMenuItem::alloc(mtm);
    unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            item,
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(""),
        )
    }
}

/// The toolbar title is a project switcher, so its menu deals with the
/// project as a whole rather than the last selected tree row.
fn project_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    // AppKit appends its own services to a context menu unless the menu opts
    // out, which is how an "AutoFill" item ended up under Reveal Project in
    // Finder. This menu's items are the only ones it should have.
    menu.setAllowsContextMenuPlugIns(false);
    // Creating things was only ever offered on the menu of a file that
    // already existed, so an untouched project had no way to make one.
    menu.addItem(&menu_item(mtm, "New File…", sel!(newDocument:)));
    menu.addItem(&menu_item(mtm, "New Folder…", sel!(newFolder:)));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&menu_item(mtm, "Open Folder…", sel!(openFolder:)));
    menu.addItem(&menu_item(
        mtm,
        "Reveal Project in Finder",
        sel!(revealProjectInFinder:),
    ));
    menu
}

fn sidebar_item_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    menu.setAllowsContextMenuPlugIns(false);
    menu.addItem(&menu_item(mtm, "New File…", sel!(newDocument:)));
    menu.addItem(&menu_item(mtm, "New Folder…", sel!(newFolder:)));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&menu_item(mtm, "Rename…", sel!(renameProjectItem:)));
    menu.addItem(&menu_item(mtm, "Move to Trash", sel!(trashProjectItem:)));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&menu_item(mtm, "Reveal in Finder", sel!(revealInFinder:)));
    menu
}

fn editor_context_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    menu.addItem(&menu_item(mtm, "Cut", sel!(cut:)));
    menu.addItem(&menu_item(mtm, "Copy", sel!(copy:)));
    menu.addItem(&menu_item(mtm, "Paste", sel!(paste:)));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&menu_item(mtm, "Select All", sel!(selectAll:)));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&menu_item(mtm, "Find…", sel!(performFindPanelAction:)));
    menu
}

/// Builds the menu bar.
///
/// Not decoration: an app with no main menu has no Cmd-Q, and the only way
/// out is to kill the process. Every item here maps to a selector AppKit
/// already implements on the responder chain, so they all genuinely work.
/// Items that would need editor features we do not have yet (Select All,
/// Copy, Paste) are deliberately absent rather than present and dead.
fn install_menu(mtm: MainThreadMarker, app: &NSApplication) {
    let menubar = NSMenu::new(mtm);

    // Target is left nil so the item travels the responder chain and lands on
    // the first responder, which is the editor view. That is how a native app
    // wires menus, and it is what lets validateMenuItem: grey things out.
    let item = |title: &str, action: objc2::runtime::Sel, key: &str, shift: bool| {
        let item = NSMenuItem::alloc(mtm);
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                item,
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(key),
            )
        };
        if shift {
            item.setKeyEquivalentModifierMask(
                NSEventModifierFlags::Command | NSEventModifierFlags::Shift,
            );
        }
        item
    };
    let submenu = |title: &str| {
        let holder = NSMenuItem::new(mtm);
        let menu = NSMenu::new(mtm);
        menu.setTitle(&NSString::from_str(title));
        (holder, menu)
    };

    // macOS treats the first top-level item's submenu as the application
    // menu, whatever its title.
    let (app_item, app_menu) = submenu("crc");
    app_menu.addItem(&item("About crc", sel!(aboutCrc:), "", false));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&item("Settings\u{2026}", sel!(openSettings:), ",", false));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&item("Hide crc", sel!(hide:), "h", false));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&item("Quit crc", sel!(terminate:), "q", false));
    app_item.setSubmenu(Some(&app_menu));
    menubar.addItem(&app_item);

    let (file_item, file_menu) = submenu("File");
    file_menu.addItem(&item("New File\u{2026}", sel!(newDocument:), "n", false));
    file_menu.addItem(&NSMenuItem::separatorItem(mtm));
    file_menu.addItem(&item("Open\u{2026}", sel!(openDocument:), "o", false));
    file_menu.addItem(&item("Open Folder\u{2026}", sel!(openFolder:), "o", true));
    file_menu.addItem(&item(
        "Open Quickly\u{2026}",
        sel!(openQuickly:),
        "p",
        false,
    ));
    file_menu.addItem(&NSMenuItem::separatorItem(mtm));
    file_menu.addItem(&item("Save", sel!(saveDocument:), "s", false));
    file_menu.addItem(&item("Save As\u{2026}", sel!(saveDocumentAs:), "s", true));
    file_menu.addItem(&item(
        "Revert to Saved",
        sel!(revertDocumentToSaved:),
        "",
        false,
    ));
    file_menu.addItem(&NSMenuItem::separatorItem(mtm));
    // Cmd-Delete on the sidebar selection, the Finder binding. The action
    // already existed with its unsaved-tab guard, but only in the context
    // menu, so there was no way to reach it from the keyboard.
    // `validateMenuItem` greys it out when nothing in the tree is selected.
    file_menu.addItem(&item(
        "Move to Trash",
        sel!(trashProjectItem:),
        "\u{8}",
        false,
    ));
    file_menu.addItem(&NSMenuItem::separatorItem(mtm));
    file_menu.addItem(&item("Close Tab", sel!(closeTabOrWindow:), "w", false));
    file_item.setSubmenu(Some(&file_menu));
    menubar.addItem(&file_item);

    let (edit_item, edit_menu) = submenu("Edit");
    edit_menu.addItem(&item("Undo", sel!(undo:), "z", false));
    edit_menu.addItem(&item("Redo", sel!(redo:), "z", true));
    edit_menu.addItem(&NSMenuItem::separatorItem(mtm));
    edit_menu.addItem(&item("Cut", sel!(cut:), "x", false));
    edit_menu.addItem(&item("Copy", sel!(copy:), "c", false));
    edit_menu.addItem(&item("Paste", sel!(paste:), "v", false));
    edit_menu.addItem(&NSMenuItem::separatorItem(mtm));
    edit_menu.addItem(&item("Select All", sel!(selectAll:), "a", false));
    edit_menu.addItem(&NSMenuItem::separatorItem(mtm));
    edit_menu.addItem(&item(
        "Find\u{2026}",
        sel!(performFindPanelAction:),
        "f",
        false,
    ));
    edit_menu.addItem(&item("Find Next", sel!(findNext:), "g", false));
    edit_menu.addItem(&item("Find Previous", sel!(findPrevious:), "g", true));
    edit_menu.addItem(&item(
        "Find in Project\u{2026}",
        sel!(findInProject:),
        "f",
        true,
    ));
    edit_menu.addItem(&item("Replace All", sel!(replaceAll:), "", false));
    edit_menu.addItem(&item("Go to Line\u{2026}", sel!(goToLine:), "l", false));
    edit_menu.addItem(&NSMenuItem::separatorItem(mtm));
    edit_menu.addItem(&item(
        "Select Next Occurrence",
        sel!(selectNextOccurrence:),
        "d",
        false,
    ));
    edit_menu.addItem(&NSMenuItem::separatorItem(mtm));
    edit_menu.addItem(&item("Toggle Comment", sel!(toggleComment:), "/", false));
    edit_menu.addItem(&item("Duplicate Line", sel!(duplicateLines:), "d", true));
    edit_menu.addItem(&item("Move Line Up", sel!(moveLineUp:), "\u{f700}", true));
    edit_menu.addItem(&item(
        "Move Line Down",
        sel!(moveLineDown:),
        "\u{f701}",
        true,
    ));
    edit_item.setSubmenu(Some(&edit_menu));
    menubar.addItem(&edit_item);

    let (view_item, view_menu) = submenu("View");
    view_menu.addItem(&item("Toggle Sidebar", sel!(toggleSidebar:), "b", false));
    view_menu.addItem(&item(
        "Toggle Markdown Preview",
        sel!(togglePreview:),
        "e",
        false,
    ));
    let fold = item("Fold", sel!(foldBlock:), "\u{f702}", false);
    fold.setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Option);
    view_menu.addItem(&fold);
    let unfold = item("Unfold", sel!(unfoldBlock:), "\u{f703}", false);
    unfold
        .setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Option);
    view_menu.addItem(&unfold);
    view_menu.addItem(&item("Fold All", sel!(foldAll:), "", false));
    view_menu.addItem(&item("Unfold All", sel!(unfoldAll:), "", false));
    let wrap = item("Word Wrap", sel!(toggleWordWrap:), "z", false);
    wrap.setKeyEquivalentModifierMask(NSEventModifierFlags::Option);
    view_menu.addItem(&wrap);
    view_menu.addItem(&NSMenuItem::separatorItem(mtm));
    view_menu.addItem(&item("Zoom In", sel!(zoomIn:), "=", false));
    view_menu.addItem(&item("Zoom Out", sel!(zoomOut:), "-", false));
    view_menu.addItem(&item("Actual Size", sel!(zoomActual:), "0", false));
    view_menu.addItem(&NSMenuItem::separatorItem(mtm));
    view_menu.addItem(&item("Split Editor Right", sel!(splitEditor:), "\\", false));
    let close_pane = item("Close Pane", sel!(closePane:), "w", false);
    close_pane
        .setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Option);
    view_menu.addItem(&close_pane);
    let next_pane = item("Focus Next Pane", sel!(focusNextPane:), "]", false);
    next_pane
        .setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Option);
    view_menu.addItem(&next_pane);
    let previous_pane = item("Focus Previous Pane", sel!(focusPreviousPane:), "[", false);
    previous_pane
        .setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Option);
    view_menu.addItem(&previous_pane);
    view_menu.addItem(&NSMenuItem::separatorItem(mtm));
    view_menu.addItem(&item("Claude Code", sel!(openClaude:), "c", true));
    let terminal = item("Terminal", sel!(toggleTerminal:), "`", false);
    terminal.setKeyEquivalentModifierMask(NSEventModifierFlags::Control);
    view_menu.addItem(&terminal);
    view_menu.addItem(&item("New Terminal", sel!(newTerminal:), "", false));
    view_menu.addItem(&item("Toggle Panel", sel!(toggleTerminal:), "j", false));
    view_item.setSubmenu(Some(&view_menu));
    menubar.addItem(&view_item);

    let (git_item, git_menu) = submenu("Git");
    let source_control = item("Source Control", sel!(showSourceControl:), "g", false);
    source_control
        .setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Option);
    git_menu.addItem(&source_control);
    git_menu.addItem(&item("Refresh", sel!(gitRefresh:), "", false));
    git_menu.addItem(&item("Commit\u{2026}", sel!(gitCommit:), "", false));
    git_menu.addItem(&NSMenuItem::separatorItem(mtm));
    git_menu.addItem(&item(
        "Switch Branch\u{2026}",
        sel!(switchBranch:),
        "",
        false,
    ));
    git_menu.addItem(&item("Fetch", sel!(gitFetch:), "", false));
    git_menu.addItem(&item("Pull", sel!(gitPull:), "", false));
    git_menu.addItem(&item("Push", sel!(gitPush:), "", false));
    git_item.setSubmenu(Some(&git_menu));
    menubar.addItem(&git_item);

    let (go_item, go_menu) = submenu("Go");
    // F12, F1 and Control-Space, the bindings every editor shares. The
    // function keys are AppKit's private-use characters.
    go_menu.addItem(&item(
        "Go to Definition",
        sel!(goToDefinition:),
        "\u{F70F}",
        false,
    ));
    go_menu.addItem(&item("Show Hover", sel!(showHover:), "\u{F704}", false));
    go_menu.addItem(&item(
        "Find References",
        sel!(findReferences:),
        "\u{F70F}",
        true,
    ));
    let rename = item(
        "Rename Symbol\u{2026}",
        sel!(renameSymbol:),
        "\u{F705}",
        false,
    );
    rename.setKeyEquivalentModifierMask(NSEventModifierFlags::empty());
    go_menu.addItem(&rename);
    let format = item("Format Document", sel!(formatDocument:), "f", false);
    format.setKeyEquivalentModifierMask(NSEventModifierFlags::Shift | NSEventModifierFlags::Option);
    go_menu.addItem(&format);
    go_menu.addItem(&NSMenuItem::separatorItem(mtm));
    go_menu.addItem(&item("Go to Symbol\u{2026}", sel!(goToSymbol:), "r", false));
    go_menu.addItem(&item(
        "Go to Symbol in Project\u{2026}",
        sel!(goToProjectSymbol:),
        "t",
        false,
    ));
    go_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let complete = item("Trigger Completion", sel!(triggerCompletion:), " ", false);
    complete.setKeyEquivalentModifierMask(NSEventModifierFlags::Control);
    go_menu.addItem(&complete);
    go_menu.addItem(&item(
        "Forget Completion History",
        sel!(forgetCompletionHistory:),
        "",
        false,
    ));
    go_item.setSubmenu(Some(&go_menu));
    menubar.addItem(&go_item);

    let (run_item, run_menu) = submenu("Run");
    // Cmd-Return. Greyed out unless the active document is a `.http` file,
    // so it never eats the key anywhere else.
    run_menu.addItem(&item("Send Request", sel!(sendRequest:), "\r", false));
    run_item.setSubmenu(Some(&run_menu));
    menubar.addItem(&run_item);

    let (window_item, window_menu) = submenu("Window");
    window_menu.addItem(&item("Show Next Tab", sel!(selectNextTab:), "]", false));
    window_menu.addItem(&item(
        "Show Previous Tab",
        sel!(selectPreviousTab:),
        "[",
        false,
    ));
    window_menu.addItem(&NSMenuItem::separatorItem(mtm));
    window_menu.addItem(&item("Minimize", sel!(performMiniaturize:), "m", false));
    window_item.setSubmenu(Some(&window_menu));
    menubar.addItem(&window_item);

    let (help_item, help_menu) = submenu("Help");
    help_menu.addItem(&item(
        "Check for Updates…",
        sel!(checkForUpdates:),
        "",
        false,
    ));
    help_menu.addItem(&item("Report a Problem…", sel!(reportProblem:), "", false));
    help_menu.addItem(&item("Show Crash Logs", sel!(showCrashLogs:), "", false));
    help_item.setSubmenu(Some(&help_menu));
    menubar.addItem(&help_item);
    app.setHelpMenu(Some(&help_menu));

    app.setMainMenu(Some(&menubar));
}

/// Whether writing `path` can change what Git ignores.
fn changes_ignore_rules(path: &Path) -> bool {
    path.file_name().is_some_and(|n| n == ".gitignore") || path.ends_with(".git/info/exclude")
}

/// Reads what Git ignores under `root` on a worker. Not a repository, or
/// Git failing, is an empty set: nothing is drawn as ignored.
fn spawn_ignored(
    root: std::path::PathBuf,
) -> mpsc::Receiver<(
    std::path::PathBuf,
    std::collections::HashSet<std::path::PathBuf>,
)> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let ignored = crate::project::git::ignored(&root).unwrap_or_default();
        let _ = tx.send((root, ignored));
    });
    rx
}

/// Opens the window and runs until the user quits. Does not return.
pub fn run(buffer: Buffer, folder: Option<std::path::PathBuf>, font: &str, size_pt: f32) -> ! {
    let mtm = MainThreadMarker::new().expect("run() must be called on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    install_menu(mtm, &app);
    prefer_key_repeat();

    let device = MTLCreateSystemDefaultDevice().expect("this machine has no Metal device");

    let scale = NSScreen::mainScreen(mtm).map_or(2.0, |s| s.backingScaleFactor()) as f32;
    let atlas = Atlas::build(font, size_pt, scale);

    let layer = CAMetalLayer::new();
    layer.setDevice(Some(&device));
    layer.setPixelFormat(DRAWABLE_FORMAT);
    // Only we ever draw into it, so let Metal skip preserving contents.
    layer.setFramebufferOnly(true);

    let renderer = Renderer::new(device, atlas);

    // A file named on the command line wins; otherwise pick up where the
    // last session left off.
    let launched_with_file = buffer.path.is_some();
    let launched_with_target = launched_with_file || folder.is_some();
    let mut session = Session::load();
    session.prune();

    let frame = match session.frame.filter(|_| !launched_with_target) {
        Some((x, y, w, h)) => NSRect::new(NSPoint::new(x, y), NSSize::new(w, h)),
        None => NSRect::new(NSPoint::new(120.0, 120.0), NSSize::new(1100.0, 760.0)),
    };
    let window = {
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable
            | NSWindowStyleMask::FullSizeContentView;
        let w = NSWindow::alloc(mtm);
        unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                w,
                frame,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        }
    };
    window.setTitle(&NSString::from_str("crc"));
    window.setTitlebarAppearsTransparent(true);
    window.setTitleVisibility(NSWindowTitleVisibility::Hidden);

    let mut docs = Documents::new(buffer);
    let mut tree = Tree::new();
    let (tree_children_tx, tree_children_rx) = mpsc::channel();
    let finder = Finder::new();
    let mut project_index_rx = None;
    let mut sidebar = true;

    if let Some(folder) = &folder {
        tree.set_root(folder);
        project_index_rx = Some(spawn_project_index(folder.clone()));
    } else if launched_with_file {
        // Open the file's own directory as the project, so the sidebar is
        // useful immediately rather than empty until you find a menu.
        if let Some(dir) = docs.active().path.as_ref().and_then(|p| p.parent()) {
            tree.set_root(dir);
            project_index_rx = Some(spawn_project_index(dir.to_path_buf()));
        }
    } else {
        if let Some(folder) = &session.folder {
            tree.set_root(folder);
            project_index_rx = Some(spawn_project_index(folder.clone()));
        }
        for path in &session.files {
            let _ = docs.open(path);
        }
        docs.switch(session.active);
        if let Some(path) = docs.active().path.clone() {
            tree.reveal(&path);
        }
        sidebar = session.sidebar;
    }

    // Work that outlived a crash, offered back before anything else happens.
    if let Some(dir) = recovery::default_dir() {
        let mut found = recovery::pending(&dir);
        if !found.documents.is_empty() {
            if ask_to_restore(mtm, &found.documents) {
                for document in std::mem::take(&mut found.documents) {
                    docs.restore(Buffer::recovered(document.path, &document.text));
                }
                // Back under the hook's protection before the files go.
                recovery::publish(docs.iter());
            }
            found.clear();
        }
    }

    let initial_preview = default_preview(docs.active());
    let git = crate::platform::git_panel::Panel::new(
        tree.root()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
    );
    let recent_projects = with_recent(session.recent.clone(), tree.root());
    let state = State {
        docs,
        native_preview: None,
        git,
        git_open: false,
        git_focus: false,
        hovered_tab: None,
        tree_drag: None,
        tree,
        tree_version: 0,
        tree_children_tx,
        tree_children_rx,
        tree_children_pending: HashSet::new(),
        find: None,
        sidebar,
        sidebar_width: session.sidebar_width,
        dragging_divider: false,
        dragging_terminal: false,
        scrollbar_drag: None,
        selecting: None,
        drag_point: None,
        autoscroll_carry: 0.0,
        marked: None,
        selftest: std::collections::VecDeque::new(),
        project_menu_requested: false,
        last_selftest_click_ms: 0.0,
        selftest_pointer: (0.0, 0.0),
        cursor_rects_for: None,
        scroll_carry: (0.0, 0.0),
        renderer,
        glyphs: Vec::with_capacity(8192),
        theme: Theme::default(),
        latency: Latency::new(512),
        layer,
        viewport: Viewport::new(frame.size.width as f32, frame.size.height as f32),
        drew_once: false,
        worst: None,
        message: None,
        tab_hits: Vec::new(),
        tab_scroll: 0,
        tab_scroll_carry: 0.0,
        tab_drag: None,
        context_tab: None,
        ephemeral_session: launched_with_file,
        discard_confirmed: false,
        quit_session: None,
        preview: initial_preview,
        live_line: None,
        md_hits: Vec::new(),
        home_hits: Vec::new(),
        recent_projects,
        copied_code: None,
        syntax: SyntaxStore::new(),
        spans: Vec::new(),
        finder,
        project_index_rx,
        project_search_rx: None,
        project_search_cancel: None,
        http: None,
        responses: HashMap::new(),
        claude: None,
        claude_tried: None,
        sidebar_keys: false,
        terminal: crate::platform::terminal::Panel::default(),
        sidebar_edit: None,
        watcher: None,
        lsp: HashMap::new(),
        lsp_unavailable: HashMap::new(),
        lsp_dirty: HashMap::new(),
        rename: None,
        signature: None,
        formatting: None,
        format_on_save: crate::platform::settings::Settings::load().format_on_save,
        saving_formatted: false,
        word_wrap: crate::platform::settings::Settings::load().word_wrap,
        ssh_auth_sock: crate::platform::settings::Settings::load().ssh_auth_sock,
        branch_list: None,
        blame: None,
        blame_want: None,
        blame_rx: None,
        gutter: HashMap::new(),
        font: font.to_owned(),
        font_size: size_pt,
        theme_choice: crate::platform::settings::Settings::load().theme,
        caret_blink: crate::platform::settings::Settings::load().caret_blink,
        caret_since: Instant::now(),
        unshaped_on_screen: false,
        completer: None,
        completion_generation: 0,
        completion_chips: Vec::new(),
        indexer: None,
        update: None,
        ignored_rx: None,
        gutter_dirty: HashMap::new(),
        gutter_pending: HashSet::new(),
        gutter_channel: mpsc::channel(),
        completion: None,
        project_changed_at: None,
        git_changed_at: None,
        panes: Vec::new(),
        focused_pane: 0,
        palette: None,
        commands: Vec::new(),
        symbols: symbols::Symbols::default(),
        palette_scroll: 0,
        palette_scroll_carry: 0.0,
        goto: None,
        last_draw: None,
        pending_input: None,
        title_sync_pending: false,
        frame_interval: Duration::from_secs_f64(1.0 / 120.0),
        display_link: None,
    };

    let content = NSRect::new(NSPoint::new(0.0, 0.0), frame.size);
    let view = EditorView::new(mtm, state, content);
    view.watch_project();
    view.apply_theme();
    view.lsp_sync_open();
    // The delegate must outlive this function; NSApplication only holds a
    // weak reference to it, so it is leaked deliberately rather than dropped
    // at the end of `run` while AppKit still calls into it.
    let delegate = AppDelegate::alloc(mtm).set_ivars(DelegateIvars { view: view.clone() });
    let delegate: Retained<AppDelegate> = unsafe { msg_send![super(delegate), init] };
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    std::mem::forget(delegate);

    window.setContentView(Some(&view));
    window.setDelegate(Some(ProtocolObject::from_ref(&*view)));
    window.makeFirstResponder(Some(&view));

    if let Some(script) = std::env::var_os("CRC_SELFTEST") {
        let steps = std::fs::read_to_string(&script)
            .map_err(|e| e.to_string())
            .and_then(|text| selftest::parse(&text));
        match steps {
            Ok(steps) => {
                view.ivars().state.borrow_mut().selftest = steps.into();
                // After the run loop is up and the first frame has drawn.
                let _: () = unsafe {
                    msg_send![&*view, performSelector: sel!(selfTestStep:),
                        withObject: None::<&AnyObject>, afterDelay: 0.6f64]
                };
            }
            Err(e) => {
                eprintln!("crc: CRC_SELFTEST: {e}");
                std::process::exit(2);
            }
        }
    }
    view.sync_title();
    view.reparse();
    if session.frame.is_none() || launched_with_file {
        window.center();
    }
    let testing = std::env::var_os("CRC_SELFTEST").is_some();
    if testing {
        // Scripted events target this window directly. Keep the user's
        // keyboard focus in their app while the real test window renders.
        window.orderFront(None);
    } else {
        window.makeKeyAndOrderFront(None);
    }
    if view.ivars().state.borrow().project_index_rx.is_some()
        || view.ivars().state.borrow().git.busy()
    {
        view.resume_display_link();
    }

    if !testing
        && crate::platform::settings::Settings::load().update_check
        && crate::platform::update::launch_check_due()
    {
        view.start_update_check(false);
    }

    if !testing {
        app.activate();
    }
    app.run();
    unreachable!("NSApplication::run does not return");
}

#[cfg(test)]
mod tests {
    use super::{caret_phase, single_line, spawn_project_index, spawn_project_refresh, typed_text};
    use std::time::Duration;

    #[test]
    fn the_restore_prompt_names_what_came_back() {
        use crate::platform::recovery::Recovered;
        let one = [Recovered {
            path: Some("/work/garden-log/src/main.rs".into()),
            text: String::new(),
        }];
        let text = super::restore_summary(&one);
        assert!(
            text.starts_with("Unsaved changes to 1 document were"),
            "{text}"
        );
        assert!(text.contains(": main.rs (in src)."), "{text}");

        let many: Vec<Recovered> = (0..10)
            .map(|n| Recovered {
                path: (n > 0).then(|| format!("/notes/{n}.md").into()),
                text: String::new(),
            })
            .collect();
        let text = super::restore_summary(&many);
        assert!(text.contains("10 documents"), "{text}");
        assert!(text.contains(": Untitled, 1.md (in notes),"), "{text}");
        assert!(text.contains("7.md (in notes) and 2 more."), "{text}");
    }

    #[test]
    fn the_caret_is_solid_after_input_then_blinks() {
        let ms = Duration::from_millis;
        assert_eq!(caret_phase(ms(0)), (true, ms(530)));
        assert_eq!(caret_phase(ms(529)), (true, ms(1)));
        assert_eq!(caret_phase(ms(530)), (false, ms(530)));
        assert_eq!(caret_phase(ms(1100)), (true, ms(490)));
    }

    #[test]
    fn project_index_worker_builds_tree_and_finder() {
        let root = std::env::temp_dir().join(format!(
            "caio-project-index-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("probe.txt"), "hello").unwrap();
        let indexed = spawn_project_index(root.clone()).recv().unwrap();
        assert_eq!(indexed.root, root);
        assert_eq!(indexed.tree.rows()[0].name, "probe.txt");
        assert_eq!(indexed.finder.entry(0).unwrap().relative, "probe.txt");
        std::fs::write(root.join("new.txt"), "new").unwrap();
        let refreshed = spawn_project_refresh(indexed.tree, 1).recv().unwrap();
        assert_eq!(refreshed.tree_version, 1);
        assert!(refreshed.tree.rows().iter().any(|e| e.name == "new.txt"));
        assert_eq!(refreshed.finder.len(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn keys_without_a_character_type_nothing() {
        // F5, Help, Insert, keypad Clear: AppKit's private-use code points.
        for key in ['\u{f708}', '\u{f746}', '\u{f727}', '\u{f739}'] {
            assert_eq!(typed_text(&key.to_string()), "", "{key:?}");
        }
        // Control characters, keypad Enter's U+0003 among them.
        assert_eq!(typed_text("\u{3}\r\t\u{1b}"), "");
        // Everything that is text stays, including what sits near that block.
        assert_eq!(
            typed_text("a\u{e9}\u{65e5}\u{1f600}"),
            "a\u{e9}\u{65e5}\u{1f600}"
        );
        assert_eq!(typed_text("\u{f6ff}\u{f900}"), "\u{f6ff}\u{f900}");
    }

    #[test]
    fn a_paste_into_a_field_keeps_its_first_line() {
        assert_eq!(single_line("needle\nand the rest"), "needle");
        assert_eq!(single_line("tab\there\r\n"), "tabhere");
        assert_eq!(single_line(""), "");
        assert_eq!(single_line("\nsecond"), "");
    }

    /// A borrow written as the scrutinee of `if let`, `while let` or `match`
    /// lives until the end of the block, not the end of the expression. With
    /// the state in a `RefCell` and AppKit free to call back into the view
    /// from inside an alert, a panel or a menu, that is how the context
    /// menu's Close Tab came to abort the process: the block called
    /// `close_tab`, which asked about unsaved changes, which borrowed again.
    ///
    /// Nothing in this file can be driven from a unit test, so the rule is
    /// checked against the source instead. Copy the value out first, or bind
    /// the borrow to a name so its scope is visible.
    #[test]
    fn no_state_borrow_is_held_as_a_scrutinee() {
        let source = include_str!("window.rs");
        let offenders: Vec<String> = source
            .lines()
            .enumerate()
            .filter(|(_, line)| {
                let line = line.trim_start();
                let scrutinee = line.starts_with("if let ")
                    || line.starts_with("while let ")
                    || line.starts_with("match ")
                    || line.starts_with("} else if let ");
                scrutinee && line.contains(".state.borrow") && line.ends_with('{')
            })
            .map(|(n, line)| format!("{}: {}", n + 1, line.trim()))
            .collect();
        assert!(
            offenders.is_empty(),
            "borrow held across a block:\n{}",
            offenders.join("\n")
        );
    }
}

/// Whether `source` may be dropped into the directory `destination`.
///
/// Three refusals, and each is a way to lose a directory. Dropping a folder
/// on itself, or anywhere inside itself, asks the filesystem to make a path
/// its own descendant. Dropping something back where it already lives is a
/// no-op worth refusing so the drop target never lights up for it.
fn valid_drop(source: &Path, destination: &Path) -> bool {
    destination != source
        && !destination.starts_with(source)
        && source.parent() != Some(destination)
}

#[cfg(test)]
mod drop_tests {
    use super::valid_drop;
    use std::path::Path;

    #[test]
    fn a_folder_cannot_be_dropped_into_itself_or_its_own_subtree() {
        let src = Path::new("/p/src");
        assert!(!valid_drop(src, src), "onto itself");
        assert!(!valid_drop(src, Path::new("/p/src/render")), "into a child");
        assert!(
            !valid_drop(src, Path::new("/p/src/render/font")),
            "into a grandchild"
        );
    }

    #[test]
    fn an_item_cannot_be_dropped_where_it_already_is() {
        assert!(!valid_drop(
            Path::new("/p/src/main.rs"),
            Path::new("/p/src")
        ));
        assert!(!valid_drop(Path::new("/p/src"), Path::new("/p")));
    }

    #[test]
    fn a_real_move_is_allowed() {
        assert!(valid_drop(
            Path::new("/p/src/main.rs"),
            Path::new("/p/docs")
        ));
        assert!(valid_drop(Path::new("/p/docs"), Path::new("/p/src")));
        assert!(valid_drop(Path::new("/p/a/deep/file.rs"), Path::new("/p")));
    }

    /// A sibling whose name merely starts with the source's is not inside it.
    /// A prefix test on strings would refuse this; `starts_with` on a Path
    /// compares whole components, which is why it is used here.
    #[test]
    fn a_similarly_named_sibling_is_not_a_subtree() {
        assert!(valid_drop(
            Path::new("/p/src"),
            Path::new("/p/src-generated")
        ));
    }
}
