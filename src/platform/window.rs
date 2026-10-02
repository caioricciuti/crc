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
    NSTrackingArea, NSTrackingAreaOptions, NSView, NSWindow, NSWindowCollectionBehavior,
    NSWindowDelegate, NSWindowStyleMask, NSWindowTitleVisibility,
};
use objc2_foundation::{
    NSArray, NSAttributedString, NSAttributedStringKey, NSFileManager, NSNotFound, NSNotification,
    NSObject, NSObjectProtocol, NSPoint, NSRange, NSRangePointer, NSRect, NSRunLoop,
    NSRunLoopCommonModes, NSSize, NSString, NSUInteger, NSURL,
};
use objc2_metal::MTLCreateSystemDefaultDevice;
use objc2_quartz_core::{CADisplayLink, CALayer, CAMetalLayer};

use crate::platform::clipboard;
use crate::platform::commands::{self, Command};
use crate::platform::latency::Latency;
use crate::platform::recovery;
use crate::platform::search::{self, Options as SearchOptions, ProjectHit};
use crate::platform::selftest::{self, Step};
use crate::platform::session::Session;
use crate::platform::symbols;
use crate::project::finder::Finder;
use crate::project::tree::{Entry as TreeEntry, Tree, move_without_replace};
use crate::render::font::Atlas;
use crate::render::layout::{self, Button, Chrome, Frame, Hit, Theme, Tone, Viewport};
use crate::render::metal::{DRAWABLE_FORMAT, FrameTiming, GlyphInstance, Renderer};
use crate::syntax::{Language, Span, SyntaxStore};
use crate::text::buffer::{Buffer, DiskState, Motion};
use crate::text::documents::Documents;

mod appearance;
mod claude;
mod completion;
mod conflicts;
mod extensions;
mod files;
mod find;
mod git_menu;
mod http;
mod input;
mod lsp;
mod lsp_features;
mod mcp;
mod palette;
mod project;
mod render;
mod selftest_play;
mod sidebar;
mod state;
mod tabs;
mod terminal;
use extensions::{ext_details, preview_command, preview_displaced};
use state::{Blame, Ext, Gutter, Lsp, ProjectSearch, Reloads, Watch};

struct ProjectIndexResult {
    root: std::path::PathBuf,
    tree: Tree,
    finder: Finder,
    tree_version: u64,
}

fn spawn_project_index(root: std::path::PathBuf) -> mpsc::Receiver<ProjectIndexResult> {
    spawn_project_scan(move || {
        let mut tree = Tree::new();
        tree.open(&root);
        Some((root, tree, 0))
    })
}

fn spawn_project_refresh(mut tree: Tree, tree_version: u64) -> mpsc::Receiver<ProjectIndexResult> {
    spawn_project_scan(move || {
        let root = tree.root()?.to_path_buf();
        tree.refresh();
        Some((root, tree, tree_version))
    })
}

/// On a worker: `read_tree` reads the sidebar, then the finder scans the
/// same root. Nothing is sent when there is no root.
fn spawn_project_scan(
    read_tree: impl FnOnce() -> Option<(std::path::PathBuf, Tree, u64)> + Send + 'static,
) -> mpsc::Receiver<ProjectIndexResult> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let Some((root, tree, tree_version)) = read_tree() else {
            return;
        };
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

/// Makes `dir` the project: the sidebar, Source Control and the finder
/// start over there, read on workers.
fn set_project_root(state: &mut State, dir: &Path) {
    state.tree.set_root(dir);
    state.workspace = crate::project::workspace::Workspace::open(dir);
    state.git = git_panel_for(&state.workspace);
    state.home = None;
    state.home_since = visit_workspace(dir);
    start_home_summary(state);
    state.tree_version = 0;
    state.watch.children_pending.clear();
    state.finder = Finder::new();
    state.watch.index_rx = Some(spawn_project_index(dir.to_path_buf()));
}

/// Rereads the sidebar and the finder after the project changed on disk.
/// A scan already running is outdated by the version bump.
fn start_project_refresh(state: &mut State) {
    state.tree_version += 1;
    state.watch.children_pending.clear();
    state.watch.index_rx = Some(spawn_project_refresh(
        state.tree.clone(),
        state.tree_version,
    ));
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
    pub const O: u16 = 31;
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

/// Text an input method committed to the document. A lone control
/// character is a key (Return and Tab are handled by key code before they
/// get here, keypad Enter is U+0003); in a longer run, line breaks and tabs
/// are text: dictation's "new line", a text replacement's second line.
fn inserted_text(characters: &str) -> String {
    let mut chars = characters.chars();
    if chars.next().is_none() || chars.next().is_none() {
        return typed_text(characters);
    }
    characters
        .chars()
        .filter(|&c| {
            matches!(c, '\n' | '\r' | '\t')
                || (!c.is_control() && !('\u{f700}'..='\u{f8ff}').contains(&c))
        })
        .collect()
}

/// Whether `dir` is too big a folder to become the project for a file
/// opened from it: home, anything above it, and the standard folders that
/// hold everything. Scanning and watching those costs more than the
/// sidebar is worth.
fn too_broad_to_adopt(dir: &Path) -> bool {
    let dir = crate::platform::canonical(dir);
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return dir.parent().is_none();
    };
    let home = crate::platform::canonical(&home);
    home.starts_with(&dir)
        || ["Desktop", "Documents", "Downloads", "Library"]
            .iter()
            .any(|name| dir == home.join(name))
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

/// An extension command the Extensions menu and the palette offer.
#[derive(Clone)]
struct ExtCommand {
    installed: crate::ext::store::Installed,
    command: String,
    title: String,
}

/// An extension call waiting for its answer: where its text came from, and
/// the whole document then, so a changed document is left alone.
struct ExtCall {
    buffer: u64,
    range: std::ops::Range<usize>,
    snapshot: crate::text::rope::Rope,
    title: String,
    /// A run for the preview pane: its answer is a page, and it says
    /// nothing in the status line unless something is wrong.
    preview: bool,
}

/// The preview pane beside a document: the extension command that makes
/// its page, the web view once WebKit is ready, and the edits it has seen.
struct HtmlPreview {
    buffer: u64,
    command: ExtCommand,
    /// The document's folder: the page's base URL, and the one place it
    /// may load files from.
    folder: Option<std::path::PathBuf>,
    web: Option<crate::platform::webview::WebPreview>,
    /// The newest page, not yet in the view.
    page: Option<String>,
    /// The text as of the last look; a different tree is an edit.
    observed: crate::text::rope::Rope,
    /// The last edit not yet sent for a new page.
    changed_at: Option<Instant>,
    running: bool,
    /// A page has come back. Until one does, a failed run closes the pane.
    answered: bool,
    /// The page takes the whole tab, the text hidden behind it.
    only: bool,
}

impl HtmlPreview {
    /// Something is on its way: keep the display link running.
    fn busy(&self) -> bool {
        self.running
            || self.changed_at.is_some()
            || self.page.is_some()
            || self.web.as_ref().is_none_or(|w| !w.is_shown())
    }
}

/// Quiet time after an edit before the preview is asked again.
const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(150);

/// Organize Imports waiting on the server: the request, the document and
/// its text when asked, and whether a save asked.
struct Organizing {
    request: u64,
    path: std::path::PathBuf,
    snapshot: crate::text::rope::Rope,
    save: bool,
}

/// The lightbulb: code actions the server has at a caret. Empty when it
/// has none, so the caret is not asked about again.
struct Bulb {
    buffer: u64,
    path: std::path::PathBuf,
    caret: usize,
    server: Language,
    actions: Vec<crate::lsp::CodeAction>,
}

impl Bulb {
    /// Whether it is drawn: some action can run.
    fn shows(&self) -> bool {
        self.actions.iter().any(|a| a.disabled.is_none())
    }
}

/// What applying a workspace edit came to.
#[derive(Default)]
struct EditOutcome {
    /// Open documents edited, and left unsaved.
    open: usize,
    /// Files edited on disk.
    written: usize,
    failed: Vec<std::path::PathBuf>,
}

impl EditOutcome {
    /// "2 files", "1 file, 1 open and unsaved", "; could not edit a.rs".
    fn describe(&self) -> String {
        let files = self.open + self.written;
        let mut text = format!("{files} file{}", if files == 1 { "" } else { "s" });
        if self.open > 0 {
            text.push_str(&format!(", {} open and unsaved", self.open));
        }
        if !self.failed.is_empty() {
            let names: Vec<String> = self
                .failed
                .iter()
                .filter_map(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .collect();
            text.push_str(&format!("; could not edit {}", names.join(", ")));
        }
        text
    }
}

/// The view, for a callback on another thread to wake the main thread
/// with. The view lives for the process; see the display link.
struct ViewPointer(*const EditorView);
unsafe impl Send for ViewPointer {}
unsafe impl Sync for ViewPointer {}

/// One wake in flight: the poll to run on the main thread, with its view.
struct QueuedPoll {
    view: *const EditorView,
    poll: fn(&EditorView),
}

/// Runs a queued poll on the main thread and frees what `wake` queued.
unsafe extern "C" fn poll_on_main(context: *mut std::ffi::c_void) {
    // SAFETY: `wake` queued exactly this box, and it runs once.
    let queued = unsafe { Box::from_raw(context as *mut QueuedPoll) };
    // SAFETY: the view lives for the process; see `ViewPointer`.
    (queued.poll)(unsafe { &*queued.view });
}

/// The per-pane state of a pane that does not have the keyboard.
struct PaneStore {
    docs: Documents,
    tab_scroll: usize,
    tab_hits: Vec<layout::TabHit>,
}

fn pane_count(state: &State) -> usize {
    state.panes.len() + 1
}

/// Every pane in order, the focused one taken out of the state's own
/// fields. The inverse is [`restore_panes`].
fn take_panes(state: &mut State) -> Vec<PaneStore> {
    let focused = PaneStore {
        docs: std::mem::replace(&mut state.docs, Documents::new(Buffer::new())),
        tab_scroll: std::mem::take(&mut state.tab_scroll),
        tab_hits: std::mem::take(&mut state.tab_hits),
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
    state.tab_scroll = focused.tab_scroll;
    state.tab_hits = focused.tab_hits;
    state.panes = all;
    state.focused_pane = focus;
}

/// Every pane's documents, focused first.
fn all_docs(state: &State) -> impl Iterator<Item = &Documents> {
    std::iter::once(&state.docs).chain(state.panes.iter().map(|p| &p.docs))
}

/// The pane and tab where `path` is open in a pane without the keyboard,
/// as `focus_pane` numbers panes (the focused one keeps its place).
fn pane_holding(state: &State, path: &Path) -> Option<(usize, usize)> {
    let key = crate::platform::canonical(path);
    state
        .panes
        .iter()
        .enumerate()
        .find_map(|(slot, pane)| pane.docs.index_of(&key).map(|tab| (slot, tab)))
        .map(|(slot, tab)| (slot + usize::from(slot >= state.focused_pane), tab))
}

/// The document `id` in whichever pane has it. Takes the fields, not the
/// state, for callers that hold other fields of it at the same time.
fn buffer_by_id<'a>(docs: &'a Documents, panes: &'a [PaneStore], id: u64) -> Option<&'a Buffer> {
    std::iter::once(docs)
        .chain(panes.iter().map(|p| &p.docs))
        .flat_map(|d| d.iter())
        .find(|b| b.id() == id)
}

fn buffer_by_id_mut<'a>(
    docs: &'a mut Documents,
    panes: &'a mut [PaneStore],
    id: u64,
) -> Option<&'a mut Buffer> {
    std::iter::once(docs)
        .chain(panes.iter_mut().map(|p| &mut p.docs))
        .flat_map(|d| d.iter_mut())
        .find(|b| b.id() == id)
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

    fn field(&self) -> layout::SidebarField {
        layout::SidebarField {
            row: self.row,
            inserted: !self.replaces(),
        }
    }
}

/// The sidebar's open name field, for turning a pointer into a tree row.
fn sidebar_field(state: &State) -> Option<layout::SidebarField> {
    state.sidebar_edit.as_ref().map(SidebarEdit::field)
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
    /// Git marks per open document, and the work that keeps them current.
    gutter: Gutter,
    /// Who last changed the caret's line, for the status line.
    blame: Blame,
    /// Language servers, and what is asked of them.
    lsp: Lsp,
    /// Installed extensions: their threads, commands and calls in flight.
    ext: Ext,
    /// What keeps the project tree and index current.
    watch: Watch,
    git: crate::platform::git_panel::Panel,
    /// The open folder's repositories; Source Control shows one.
    workspace: crate::project::workspace::Workspace,
    /// What Home shows for the workspace, and the read in flight.
    home: Option<crate::project::workspace::Summary>,
    home_rx: Option<mpsc::Receiver<crate::project::workspace::Summary>>,
    /// The workspace's previous visit, for "since you were last here".
    home_since: Option<u64>,
    /// MCP servers and the sidebar panel that lists them.
    mcp: crate::platform::mcp_panel::Panel,
    /// The call behind each MCP answer tab, so Cmd-Return there runs it
    /// again.
    mcp_calls: HashMap<u64, crate::mcp_client::call::Call>,
    git_open: bool,
    /// The tab a Source Control diff is shown in, by buffer id. The diff
    /// is on screen exactly when that tab is the active one.
    diff_tab: Option<u64>,
    /// Whether the commit message field has the keyboard. Source control is
    /// docked, not modal, so it only takes typing when it is asked to.
    git_focus: bool,
    /// The tab the pointer is over, so its close button can appear. A row of
    /// crosses on every tab is a wall of targets; one under the pointer is
    /// the affordance without the clutter.
    hovered_tab: Option<usize>,
    /// F2's field: the new name, and where the rename was asked.
    rename: Option<RenameField>,
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
    branch_list: Option<BranchPick>,
    /// Set while the palette picks a repository of the workspace.
    repo_list: Option<Vec<RepoRow>>,
    /// The branch picker's list, being read by a worker.
    branch_rx: Option<mpsc::Receiver<Result<BranchPick, String>>>,
    /// `organize_imports_on_save` from the settings.
    organize_on_save: bool,
    /// The Extensions page, when it has the editor column.
    extensions: Option<crate::platform::extensions::Page>,
    /// Merge conflicts found in open documents, keyed by buffer id.
    conflict_scans: HashMap<u64, crate::platform::conflicts::Scan>,
    /// Conflicts are shown as columns rather than in the text.
    conflict_side: bool,
    /// What the conflict controls' cursor rects were built for.
    conflict_cursor_key: Option<(u64, usize, usize, usize, usize, bool)>,
    /// The Extensions page's targets and the bulb, as last drawn, so their
    /// pointing-hand cursor rects are rebuilt when they move.
    pointer_targets: Vec<Viewport>,
    /// The controls of the last frame (see `layout::ui`), for cursor
    /// rectangles and tooltips, and the one under the pointer.
    hotspots: Vec<layout::Hotspot>,
    hot: Option<usize>,
    /// Where the status line's right-hand readout starts, as last drawn:
    /// it is right-aligned, so where the branch is depends on its length.
    status_detail_x: Option<f32>,
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
    /// An update check in flight, and whether someone asked for it (which
    /// changes what an answer says and does).
    update: Option<(bool, mpsc::Receiver<crate::platform::update::Outcome>)>,
    /// The last key or click. The caret is solid for a blink phase after
    /// it, so it never vanishes under your typing.
    caret_since: Instant,
    /// Large files changed on disk, being read by a worker for a reload.
    reloads: Reloads,
    /// The completion list, while one is open.
    completion: Option<CompletionPopup>,
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
    /// An extension's page beside the active document (Cmd-E).
    html_preview: Option<HtmlPreview>,
    /// The page's share of the editor column while it is beside the text.
    preview_share: f32,
    /// The project tree behind the sidebar.
    tree: Tree,
    tree_version: u64,
    /// The find bar. Both fields are `Buffer`s, which means editing them
    /// gets the cursor, selection and boundary behaviour that already exists
    /// rather than two more, worse, text inputs.
    find: Option<FindBar>,
    /// The find bar's matches in the active document, kept while neither
    /// changes: drawing asks for them every frame.
    find_cache: Option<FindCache>,
    /// Whether the sidebar is showing.
    sidebar: bool,
    /// Sidebar width in logical points, draggable.
    sidebar_width: f32,
    /// What the held press is dragging, if anything.
    drag: Option<Drag>,
    /// Lines of autoscroll owed but not yet amounting to a whole one.
    autoscroll_carry: f32,
    /// Text an input method is still composing: the accent waiting for its
    /// letter, syllables waiting to become a word. Shown at the caret, and
    /// not part of the document until it is committed.
    marked: Option<String>,
    /// Where the input method puts the caret inside `marked`, in chars.
    marked_caret: usize,
    /// Input events still to be played, when `CRC_SELFTEST` named a script.
    selftest: std::collections::VecDeque<Step>,
    /// Confirms that a scripted click reached the native project menu action.
    project_menu_requested: bool,
    /// The folder a breadcrumb click would list, under the self-test.
    crumb_menu_requested: Option<String>,
    /// The paths behind the open breadcrumb menu's items, by tag.
    crumb_paths: Vec<std::path::PathBuf>,
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
    /// The home screen's rows, as of the last frame that drew it.
    home_hits: Vec<layout::HomeHit>,
    /// Project folders opened before, most recent first.
    recent_projects: Vec<std::path::PathBuf>,
    /// Which tab a context menu was opened on. The menu action fires later,
    /// by which time the pointer has moved, so the target has to be recorded
    /// at click time rather than looked up again.
    context_tab: Option<usize>,
    /// The Source Control row a context menu was opened on: a change and
    /// the section it is listed under.
    context_change: Option<(usize, crate::platform::git_panel::Group)>,
    /// The extension and the MCP row a context menu was opened on.
    context_ext: Option<String>,
    context_mcp: Option<crate::platform::mcp_panel::Action>,
    /// Parse trees, one per open document that has a grammar.
    syntax: SyntaxStore,
    /// Reused each frame so highlighting allocates nothing in steady state.
    spans: Vec<Span>,
    /// Indexed project files for the Cmd-P palette.
    finder: Finder,
    /// A project search or Find References in flight.
    project_search: ProjectSearch,
    /// A request in flight: the id of its response tab's buffer, and where
    /// the reply arrives.
    http: Option<(u64, mpsc::Receiver<crate::http::view::Answer>)>,
    /// Response tabs, by buffer id. The buffer holds the text of the chosen
    /// segment; this holds the request and the reply it came from.
    responses: HashMap<u64, crate::http::view::View>,
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
    /// How many rows the palette showed at the last frame. The ranking is
    /// a search over every file and command; cursor rects and the wheel
    /// only need its length.
    palette_count: usize,
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

/// NSModalResponseOK. The constant is not in the generated bindings, and
/// its value is fixed API.
const MODAL_RESPONSE_OK: isize = 1;

/// Runs the open panel for one file, or with `directories` one folder, and
/// answers the path picked.
/// Whether a self-test asked for the system panels to come back nil.
fn nil_panel() -> bool {
    std::env::var_os("CRC_SELFTEST").is_some() && std::env::var_os("CRC_NIL_PANEL").is_some()
}

/// A Save panel, or `None` when AppKit gives none.
///
/// `+[NSSavePanel savePanel]` returned nil once on macOS 27 (BUG-031).
/// objc2's generated `NSSavePanel::savePanel` panics on nil, and a release
/// build aborts on panic: Cmd-S quit the app. Asking for an `Option`
/// lets the caller say so and keep the text.
pub(super) fn save_panel(mtm: MainThreadMarker) -> Option<Retained<NSSavePanel>> {
    let _ = mtm;
    if nil_panel() {
        return None;
    }
    unsafe { msg_send![<NSSavePanel as objc2::ClassType>::class(), savePanel] }
}

/// An Open panel, or `None` when AppKit gives none; see [`save_panel`].
fn open_panel(mtm: MainThreadMarker) -> Option<Retained<NSOpenPanel>> {
    let _ = mtm;
    if nil_panel() {
        return None;
    }
    unsafe { msg_send![<NSOpenPanel as objc2::ClassType>::class(), openPanel] }
}

/// What the status line says when AppKit gives no panel.
const NO_PANEL: &str = "macOS did not open the panel; nothing was saved or opened, try again";

fn choose_path(
    mtm: MainThreadMarker,
    directories: bool,
    message: Option<&str>,
) -> Option<std::path::PathBuf> {
    let panel = open_panel(mtm)?;
    panel.setCanChooseFiles(!directories);
    panel.setCanChooseDirectories(directories);
    panel.setAllowsMultipleSelection(false);
    panel.setCanCreateDirectories(directories);
    if let Some(message) = message {
        panel.setMessage(Some(&NSString::from_str(message)));
    }
    if panel.runModal() != MODAL_RESPONSE_OK {
        return None;
    }
    Some(panel.URL()?.path()?.to_string().into())
}

/// Keeps the completion list on the word it was opened for: filtered by
/// what has been typed since its anchor, closed once the caret is before
/// the anchor, in another document, or past a space or a `/` (a path
/// completion starts again after the slash). Whether it is still open.
/// What was typed in `buffer` since `popup` opened: its anchor to the caret,
/// while the popup is on this document and the caret has not gone before it.
fn typed_since(buffer: &Buffer, popup: &CompletionPopup) -> Option<String> {
    let caret = buffer.cursor();
    (popup.buffer == buffer.id() && caret >= popup.anchor)
        .then(|| buffer.rope.slice_to_string(popup.anchor..caret))
}

fn follow_completion(state: &mut State) -> bool {
    let prefix = state
        .completion
        .as_ref()
        .and_then(|popup| typed_since(state.docs.active(), popup))
        .filter(|prefix| !prefix.contains(char::is_whitespace) && !prefix.contains('/'));
    match (prefix, &mut state.completion) {
        (Some(prefix), Some(popup)) => {
            popup.refilter(&prefix);
            true
        }
        _ => {
            state.completion = None;
            false
        }
    }
}

/// After files were written to disk by crc: Git status and the project
/// index look again.
fn note_files_written(state: &mut State) {
    state.git.refresh();
    if let Some(indexer) = &state.watch.indexer {
        indexer.poke();
    }
}

/// The documents in a debounce map (`lsp_dirty`, `gutter_dirty`) whose
/// last edit is at least `pause` old.
fn due(dirty: &HashMap<u64, Instant>, pause: Duration) -> Vec<u64> {
    dirty
        .iter()
        .filter(|(_, at)| at.elapsed() >= pause)
        .map(|(id, _)| *id)
        .collect()
}

/// A debounce stamp past any pause: the next flush takes the document.
fn overdue() -> Instant {
    Instant::now() - Duration::from_secs(1)
}

/// After documents were edited in place by something other than typing:
/// the server gets the text on its next flush, without waiting out the
/// pause, and the gutter's change marks are worked out again.
fn note_documents_edited(state: &mut State, ids: impl IntoIterator<Item = u64>) {
    for id in ids {
        state.lsp.dirty.insert(id, overdue());
        state.gutter.dirty.insert(id, Instant::now());
    }
}

/// An extension thread: where jobs go, and where answers come back.
type ExtWorker = (
    mpsc::Sender<crate::ext::run::Job>,
    mpsc::Receiver<crate::ext::run::Done>,
);

/// Consecutive failures that turn an extension off. Running out of time or
/// instructions does not count: that is the size of what it was given.
const EXT_FAILURES_TO_DISABLE: u32 = 3;

/// What the self-test's `webjs probe` asks the preview page: each
/// image's natural width, the scroll offset, the first heading and how
/// much text there is.
const PREVIEW_PROBE: &str = "Array.from(document.images).map(function(i){return i.naturalWidth}).join('/') + ' scroll=' + Math.round(scrollY) + ' h1=' + (document.querySelector('h1') ? document.querySelector('h1').textContent : '-') + ' text=' + document.body.innerText.length";

/// Whether `buffer` is Markdown, which Cmd-E shows rendered. It opens as
/// styled text: the rendered view cannot select, find or scroll by line.
fn is_markdown(buffer: &Buffer) -> bool {
    buffer
        .extension()
        .is_some_and(|e| crate::markdown::is_markdown_extension(&e))
}

/// Opens `view` in the response tab titled `title`, reusing the tab that
/// already has that title. Returns the id of the tab's buffer.
fn show_response(state: &mut State, title: &str, view: crate::http::view::View) -> u64 {
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
    reveal_active_tab(state);
    id
}

/// Whether the active document is an MCP call, or the answer to one.
fn is_mcp_call(state: &State) -> bool {
    let buffer = state.docs.active();
    if state.mcp_calls.contains_key(&buffer.id()) {
        return true;
    }
    let head = buffer
        .rope
        .slice_to_string(0..buffer.rope.len_bytes().min(400));
    crate::mcp_client::call::looks_like_call(&head)
}

/// Whether Cmd-Return has a request to send: a `.http` document, or a
/// response tab, which re-sends its own request.
fn can_send_from(state: &State) -> bool {
    let buffer = state.docs.active();
    is_mcp_call(state)
        || state.responses.contains_key(&buffer.id())
        || buffer
            .path
            .as_deref()
            .is_some_and(crate::http::is_request_file)
}

fn reveal_active_tab(state: &mut State) {
    let active = state.docs.active_index();
    if !state.tab_hits.iter().any(|hit| hit.index == active) {
        let next = layout::tab_width_ui(&state.docs, active, &mut state.renderer.atlas);
        if let Some(last) = state.tab_hits.last()
            && active == last.index + 1
            && last.x1 + next <= chrome_of(state).tabs.x + chrome_of(state).tabs.width
        {
            return;
        }
        state.tab_scroll = active;
    }
}

/// The find bar's state: two fields and which one has focus.
struct FindCache {
    buffer: u64,
    text: crate::text::rope::Rope,
    query: String,
    options: SearchOptions,
    matches: Vec<search::Match>,
}

/// The find bar's matches in `buffer`, from the cache while the text, the
/// query and the options are the ones it was made for. `None` without a
/// query, for an invalid pattern, or past the size find works on.
fn find_matches<'a>(
    cache: &'a mut Option<FindCache>,
    bar: &FindBar,
    buffer: &Buffer,
) -> Option<&'a [search::Match]> {
    if bar.query.rope.len_bytes() == 0 || buffer.rope.len_bytes() > 2 * 1024 * 1024 {
        return None;
    }
    fill_find_cache(cache, bar, buffer).ok()
}

/// Makes `cache` hold the find bar's matches in `buffer`, searching only
/// when the text, query or options changed since it was filled. An invalid
/// pattern is the error.
fn fill_find_cache<'a>(
    cache: &'a mut Option<FindCache>,
    bar: &FindBar,
    buffer: &Buffer,
) -> Result<&'a [search::Match], String> {
    let query = bar.query.rope.to_string();
    let fresh = cache.as_ref().is_some_and(|c| {
        c.buffer == buffer.id()
            && c.text.same_as(&buffer.rope)
            && c.query == query
            && c.options == bar.options
    });
    if !fresh {
        let matches = search::find(&buffer.rope.to_string(), &query, "", bar.options)?;
        *cache = Some(FindCache {
            buffer: buffer.id(),
            text: buffer.rope.clone(),
            query,
            options: bar.options,
            matches,
        });
    }
    Ok(cache.as_ref().map_or(&[], |c| c.matches.as_slice()))
}

/// Project search results shown under the find bar at a time.
const FIND_RESULT_ROWS: usize = 8;

struct FindBar {
    query: Buffer,
    replacement: Buffer,
    /// Tab moves between them.
    replacing: bool,
    /// Whether typing goes to the bar. A click in the text gives the
    /// document the keys and leaves the bar open with its matches drawn;
    /// Cmd-F or a click on a field gives them back.
    has_keys: bool,
    options: SearchOptions,
    project: bool,
    results: Vec<ProjectHit>,
    selected: usize,
    result_scroll: usize,
    searching: bool,
}

impl FindBar {
    /// No results, the first selected, scrolled to the top.
    fn reset_results(&mut self) {
        self.results.clear();
        self.selected = 0;
        self.result_scroll = 0;
    }

    /// Scrolls the result list so the selected row is in its window.
    fn follow_selection(&mut self) {
        if self.selected < self.result_scroll {
            self.result_scroll = self.selected;
        }
        if self.selected >= self.result_scroll + FIND_RESULT_ROWS {
            self.result_scroll = self.selected + 1 - FIND_RESULT_ROWS;
        }
    }
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

/// What a held press is dragging. One at a time: `mouseDown:` clears it
/// before deciding what the press started, `mouseUp:` clears it again.
enum Drag {
    /// The sidebar's divider.
    Divider,
    /// The divider between the text and the page beside it.
    PreviewDivider,
    /// The terminal panel's top edge.
    Terminal,
    /// The editor's scrollbar thumb: where on the thumb the press landed,
    /// so the thumb does not jump under the pointer.
    Scrollbar(f32),
    /// A tab across the strip, by its current index.
    Tab(usize),
    /// A sidebar row onto a folder.
    Tree(TreeDrag),
    /// A press that began in the text, the only kind of drag that selects.
    Select {
        /// What it selects in.
        unit: SelectUnit,
        /// What the press itself selected, which a drag never shrinks below.
        pressed: std::ops::Range<usize>,
        /// Where the pointer last was. Kept because a pointer held still
        /// past the edge sends no events, and the view still has to keep
        /// scrolling under it.
        point: Option<(f32, f32)>,
    },
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
    /// The last frame drew a spinner or a progress bar, so the display
    /// link keeps drawing (at a gentler pace) until one draws none.
    animating: Cell<bool>,
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
    /// Watcher changes (tree, git) and a window size that arrived while the
    /// state was borrowed, from a main-queue block run inside a modal loop.
    /// The display link applies them on its next tick.
    deferred_change: Cell<(bool, bool)>,
    deferred_size: Cell<Option<NSSize>>,
    /// Paused whenever there is nothing to draw, so an idle editor does not
    /// wake the CPU 120 times a second. Outside the state so it can be
    /// resumed while the state is borrowed.
    display_link: std::cell::OnceCell<Retained<CADisplayLink>>,
    /// Scrolling left over from the last event, in lines, for the views
    /// that scroll by whole lines, and which one it belongs to: a rest
    /// built up over the terminal is not paid out in the Git list.
    wheel_rest: Cell<(Option<WheelTarget>, f64)>,
}

/// A view that scrolls by whole lines under the wheel.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WheelTarget {
    Readme,
    Terminal,
    Review,
    Git,
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
        /// A blink toggle, armed by the frame before it.
        #[unsafe(method(caretBlink:))]
        fn caret_blink(&self, _sender: Option<&AnyObject>) {
            self.request_redraw();
            self.pump();
        }

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

        /// The tooltip of the control under `point`, asked by AppKit once
        /// the pointer has rested on one of the rectangles added in
        /// `resetCursorRects`.
        #[unsafe(method_id(view:stringForToolTip:point:userData:))]
        fn tool_tip(
            &self,
            _view: &NSView,
            _tag: isize,
            point: NSPoint,
            _data: *mut std::ffi::c_void,
        ) -> Retained<NSString> {
            let text = self
                .state()
                .and_then(|state| {
                    let at = layout::hotspot_at(&state.hotspots, point.x as f32, point.y as f32)?;
                    state.hotspots[at].tip.clone()
                })
                .unwrap_or_default();
            NSString::from_str(&text)
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
            let Some(composing) = self.state().map(|state| state.marked.is_some()) else {
                return;
            };
            let changed = if composing {
                self.interpret(event)
            } else {
                self.handle_key(event)
            };
            self.ivars().handling_key.set(false);
            if changed {
                let Some(edited) = self.state().map(|state| state.docs.active().has_pending_edits()) else {
                    return;
                };
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
            // The control under the press shows it is held.
            layout::set_pointer(Some((x, y)));
            layout::set_pressed(Some((x, y)));
            if self.state().is_some_and(|state| layout::hotspot_at(&state.hotspots, x, y).is_some()) {
                self.request_redraw();
            }
            let chrome = self.chrome();
            // A press only starts a text selection if it lands in the text.
            if let Some(mut state) = self.state_mut() {
                state.drag = None;
            }

            // The Extensions page owns the editor column while it is open.
            let page_action = {
                let Some(state) = self.state() else {
                    return;
                };
                let in_list = chrome.sidebar.is_some_and(|r| r.contains(x, y));
                match &state.extensions {
                    Some(page)
                        if state.palette.is_none()
                            && ((page.details && details_rect(&chrome).contains(x, y))
                                || in_list) =>
                    {
                        Some(page.hit(x, y))
                    }
                    _ => None,
                }
            };
            if let Some(action) = page_action {
                if let Some(action) = action {
                    self.extensions_action(action);
                }
                return;
            }

            // The scrollbar thumb, before anything that treats a press in
            // the text as a caret placement. A press on the track outside
            // the thumb brings the thumb to the pointer.
            {
                let Some(mut state) = self.state_mut() else {
                    return;
                };
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
                    state.drag = Some(Drag::Scrollbar(grab));
                    drop(state);
                    self.request_redraw();
                    self.pump();
                    return;
                }
            }
            // Anywhere but the sidebar takes the keyboard from the tree, and
            // the terminal has it exactly when the press is in the terminal.
            {
                let Some(mut state) = self.state_mut() else {
                    return;
                };
                if !chrome.sidebar.is_some_and(|rect| rect.contains(x, y)) {
                    state.sidebar_keys = false;
                }
                state.terminal.focus = chrome.terminal.is_some_and(|rect| rect.contains(x, y));
            }

            // Clicking away from a half-typed accent abandons it, here and in
            // the input system, which would otherwise finish it wherever the
            // caret went.
            let Some(composing) = self.state_mut().map(|mut state| state.marked.take().is_some()) else {
                return;
            };
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

            if self.state().is_some_and(|state| state.palette.is_some()) {
                self.palette_click(x, y);
                return;
            }
            // A click anywhere else is the end of a name being typed: kept
            // if there is one, dropped if the field is empty. A click on the
            // tree itself goes no further, so the row it hit cannot shift
            // under it as the field disappears.
            if self.state().is_some_and(|state| state.sidebar_edit.is_some()) {
                self.finish_sidebar_edit(true);
                if chrome.sidebar.is_some_and(|rect| rect.contains(x, y)) {
                    return;
                }
            }
            let hit = {
                let Some(mut state) = self.state_mut() else {
                    return;
                };
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
                        if let Some(mut state) = self.state_mut() {
                            state.project_menu_requested = true;
                        }
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
                    if let Some(mut state) = self.state_mut() {
                        state.drag = Some(Drag::Divider);
                    }
                    return;
                }
                Some(Hit::PreviewDivider) => {
                    if let Some(mut state) = self.state_mut() {
                        state.drag = Some(Drag::PreviewDivider);
                    }
                    return;
                }
                Some(Hit::Activity(index)) => {
                    self.activate(index);
                    return;
                }
                Some(Hit::Breadcrumb(index)) => {
                    self.breadcrumb_menu(index, event);
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
                    if let Some(mut state) = self.state_mut() {
                        state.drag = Some(Drag::Terminal);
                    }
                    return;
                }
                Some(Hit::TerminalTab(index)) => {
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    state.terminal.active = index;
                    state.terminal.back = 0;
                    state.terminal.selection = None;
                    drop(state);
                    self.request_redraw();
                    self.pump();
                    return;
                }
                Some(Hit::TerminalClose(index)) => {
                    if let Some(mut state) = self.state_mut() {
                        state.terminal.close_tab(index);
                    }
                    self.after_terminal_layout();
                    return;
                }
                Some(Hit::TerminalNew) => {
                    self.spawn_terminal(false);
                    return;
                }
                Some(Hit::TerminalHide) => {
                    if let Some(mut state) = self.state_mut() {
                        state.terminal.open = false;
                        state.terminal.focus = false;
                    }
                    self.after_terminal_layout();
                    return;
                }
                Some(Hit::Terminal) => {
                    self.terminal_press(event, x, y);
                    return;
                }
                Some(Hit::ConflictMode(side)) => {
                    self.set_conflict_side(side);
                    return;
                }
                Some(Hit::ConflictStep(forward)) => {
                    self.step_conflict(forward, false);
                    return;
                }
                Some(Hit::ConflictResolve) => {
                    self.mark_conflict_resolved();
                    return;
                }
                Some(Hit::ConflictTake(index, take)) => {
                    self.take_conflict(index, take);
                    return;
                }
                // The columns are a view of the file: a click goes back to
                // the file, at the line clicked.
                Some(Hit::Text) if self.state().is_some_and(|state| side_by_side(&state)) => {
                    let line = {
                        let Some(mut state) = self.state_mut() else {
                            return;
                        };
                        let total = state.docs.active().rope.len_lines();
                        let text = chrome.text;
                        active_conflicts_mut(&mut state).and_then(|view| {
                            crate::platform::conflicts::side_line_at(view, total, text, x, y)
                        })
                    };
                    self.set_conflict_side(false);
                    if let Some(line) = line {
                        self.caret_to_line(line);
                    }
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
                Some(Hit::Text) if self.state().is_some_and(|state| active_review(&state).is_some()) => {
                    return;
                }
                Some(Hit::Tab(index)) => {
                    {
                        let Some(mut state) = self.state_mut() else {
                            return;
                        };
                        if let Some(page) = &mut state.extensions {
                            page.details = false;
                        }
                        state.drag = Some(Drag::Tab(index));
                    }
                    self.tab_click(x);
                    return;
                }
                Some(Hit::TabClose(_)) => {
                    if let Some(mut state) = self.state_mut() {
                        state.drag = None;
                    }
                    self.tab_click(x);
                    return;
                }
                Some(Hit::TabStrip) => {
                    if event.clickCount() >= 2 {
                        let Some(mut state) = self.state_mut() else {
                            return;
                        };
                        state.docs.push(Buffer::new());
                        reveal_active_tab(&mut state);
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
                    // The lightbulb in place of the caret line's number.
                    let on_bulb = {
                        let Some(state) = self.state() else {
                            return;
                        };
                        let (tx, ty) = chrome_of(&state).to_text(x, y);
                        let buffer = state.docs.active();
                        state.lsp.bulb.as_ref().is_some_and(|b| {
                            (b.buffer, b.caret) == (buffer.id(), buffer.cursor()) && b.shows()
                        }) && layout::bulb_at(buffer, &state.renderer.atlas, tx, ty)
                    };
                    if on_bulb {
                        self.quick_fix();
                        return;
                    }
                    // The fold chevron beside a line number.
                    let fold = {
                        let Some(state) = self.state() else {
                            return;
                        };
                        let (tx, ty) = chrome_of(&state).to_text(x, y);
                        layout::fold_chevron_at(state.docs.active(), &state.renderer.atlas, tx, ty)
                    };
                    if let Some(line) = fold {
                        self.toggle_fold(Some(line));
                        return;
                    }
                }
                Some(Hit::StatusBranch) | Some(Hit::GitBranch) => {
                    self.open_branch_picker(BranchIntent::Switch);
                    return;
                }
                Some(Hit::GitRepo) => {
                    self.open_repo_picker();
                    return;
                }
                Some(Hit::SidebarRow(_))
                | Some(Hit::Status)
                | Some(Hit::Git(_))
                | Some(Hit::GitRow(_))
                | Some(Hit::GitToggle(_))
                | None => {}
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
                if self.state().is_some_and(|state| state.git_open) {
                    self.git_click(rect, x, y);
                    return;
                }
                if self.state().is_some_and(|state| state.mcp.open) {
                    self.mcp_click(x, y);
                    return;
                }
                // Remember what is under the pointer in case this turns into
                // a drag. Selecting still happens now, so a plain click is
                // unaffected.
                {
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    let row = layout::sidebar_row_at(&state.tree, sidebar_field(&state), rect, y);
                    state.drag = row
                        .and_then(|index| state.tree.rows().get(index))
                        .map(|entry| Drag::Tree(TreeDrag {
                            path: entry.path.clone(),
                            origin: (x, y),
                            active: false,
                            over: None,
                            valid: false,
                        }));
                }
                self.sidebar_click(y, rect);
                return;
            }

            if chrome.text.contains(x, y) && self.state().is_some_and(|state| diffing(&state)) {
                let Some(mut state) = self.state_mut() else {
                    return;
                };
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
                    let Some(state) = self.state() else {
                        return;
                    };
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
                        layout::HomeAction::OpenFiles(paths) => {
                            for path in paths {
                                self.run_pick(Pick::File(path));
                            }
                        }
                        layout::HomeAction::ShowRepo(path) => {
                            self.select_repo(&path);
                            self.set_sidebar_view(true);
                        }
                        layout::HomeAction::ShowTerminal(index) => {
                            if let Some(mut state) = self.state_mut() {
                                state.terminal.active = index.min(state.terminal.tabs.len().saturating_sub(1));
                            }
                            self.open_terminal(false);
                        }
                        layout::HomeAction::RunCall(path) => {
                            self.run_pick(Pick::File(path));
                            self.run_mcp_call();
                        }
                    }
                    self.request_redraw();
                    self.pump();
                    return;
                }
            }

            // The find bar stays open with its matches, but typing now goes
            // to the document the press put the caret in.
            if let Some(mut state) = self.state_mut()
                && let Some(bar) = &mut state.find
            {
                bar.has_keys = false;
            }
            self.text_press(event, started);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            if self.ivars().testing && event.timestamp() != 0.0 { return; }
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            layout::set_pointer(Some((point.x as f32, point.y as f32)));

            let moved = {
                let Some(mut state) = self.state_mut() else {
                    return;
                };
                if let Some(Drag::Tab(from)) = state.drag {
                    let x = point.x as f32;
                    let to = state.tab_hits.iter().find(|h| x >= h.x0 && x < h.x1).map(|h| h.index);
                    if let Some(to) = to && state.docs.move_tab(from, to) {
                        state.drag = Some(Drag::Tab(to));
                        true
                    } else { false }
                } else { false }
            };
            if moved {
                self.request_redraw();
                self.pump();
                return;
            }
            if self.state().is_some_and(|state| matches!(state.drag, Some(Drag::Tab(_)))) { return; }

            let Some(grab) = self.state().map(|state| match state.drag {
                Some(Drag::Scrollbar(grab)) => Some(grab),
                _ => None,
            }) else {
                return;
            };
            if let Some(grab) = grab {
                let chrome = self.chrome();
                {
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
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

            if self.state().is_some_and(|state| matches!(state.drag, Some(Drag::Tree(_)))) {
                self.tree_drag_moved(point.x as f32, point.y as f32);
                return;
            }

            if self.state().is_some_and(|state| state.terminal.selecting) {
                self.terminal_drag(point.x as f32, point.y as f32);
                return;
            }

            if self.state().is_some_and(|state| matches!(state.drag, Some(Drag::Terminal))) {
                let chrome = self.chrome();
                {
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
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

            if self.state().is_some_and(|state| matches!(state.drag, Some(Drag::PreviewDivider))) {
                {
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    let chrome = chrome_of(&state);
                    if let Some(page) = chrome.preview {
                        let column = page.x + page.width - chrome.text.x;
                        if column > 0.0 {
                            let text = (point.x as f32 - chrome.text.x) / column;
                            state.preview_share = (1.0 - text).clamp(0.0, 1.0);
                        }
                    }
                }
                self.request_redraw();
                self.pump();
                return;
            }

            if self.state().is_some_and(|state| matches!(state.drag, Some(Drag::Divider))) {
                {
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
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
            if let Some(mut state) = self.state_mut() {
                let Some(Drag::Select { point: at, .. }) = &mut state.drag else {
                    return;
                };
                *at = Some((point.x as f32, point.y as f32));
            }
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
            self.context_menu(event)
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            if self.ivars().testing && _event.timestamp() != 0.0 { return; }
            layout::set_pressed(None);
            self.request_redraw();
            let drop = {
                let Some(mut state) = self.state_mut() else {
                    return;
                };
                // A press that never moved selects nothing.
                if std::mem::take(&mut state.terminal.selecting)
                    && state.terminal.selection.is_some_and(|(a, b)| a == b)
                {
                    state.terminal.selection = None;
                }
                state.autoscroll_carry = 0.0;
                match state.drag.take() {
                    Some(Drag::Tree(drag)) if drag.active && drag.valid => Some(drag),
                    _ => None,
                }
            };
            if let Some(drag) = drop {
                self.drop_tree_item(drag);
            }
        }

        /// A resize cursor over the divider, so it reads as draggable.
        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            let Some(mut state) = self.state_mut() else { return; };
            let chrome = chrome_of(&state);
            let add = |r: Viewport, cursor: &NSCursor| {
                if r.width > 0.0 && r.height > 0.0 {
                    self.addCursorRect_cursor(ns_rect(r), cursor);
                }
            };
            add(state.viewport, &NSCursor::arrowCursor());
            if state.docs.is_home() && state.palette.is_none() && !state.git_open {
                for hit in &state.home_hits {
                    add(hit.rect, &NSCursor::pointingHandCursor());
                }
            }
            if state.palette.is_some() {
                let count = state.palette_count;
                let rect = layout::palette_rect(state.viewport, count);
                // The field and esc are shared controls: their hotspots
                // are added below, inside the palette.
                let first = state.palette_scroll.min(layout::palette_max_scroll(count, layout::palette_visible_rows(rect)));
                let rows = count.saturating_sub(first).min(layout::palette_visible_rows(rect));
                add(Viewport { x: rect.x + 8.0, y: rect.y + layout::PALETTE_HEADER, width: rect.width - 16.0, height: rows as f32 * layout::PALETTE_ROW }, &NSCursor::pointingHandCursor());
                self.add_hotspot_rects(&state.hotspots, Some(rect));
                return;
            }
            for (hit, rect) in frame_of(&mut state).regions {
                let pointing = matches!(
                    hit,
                    Hit::ToolbarSidebar
                        | Hit::ToolbarProject
                        | Hit::ToolbarSearch
                        | Hit::Activity(_)
                        | Hit::SidebarAction(_)
                        | Hit::Tab(_)
                        | Hit::TabClose(_)
                        | Hit::ResponseSegment(_)
                        | Hit::ReviewAccept
                        | Hit::ReviewReject
                        | Hit::ConflictMode(_)
                        | Hit::ConflictStep(_)
                        | Hit::ConflictResolve
                        | Hit::ConflictTake(..)
                        | Hit::ToolbarTerminal
                        | Hit::TerminalTab(_)
                        | Hit::TerminalClose(_)
                        | Hit::TerminalNew
                        | Hit::Breadcrumb(_)
                );
                if pointing {
                    add(rect, &NSCursor::pointingHandCursor());
                }
            }
            // The Extensions page's buttons and rows, and the bulb.
            for target in &state.pointer_targets {
                add(*target, &NSCursor::pointingHandCursor());
            }
            if matches!(column_of(&state), Column::Home | Column::Text) && state.native_preview.is_none() {
                let gutter = layout::gutter_width(state.docs.active(), &state.renderer.atlas);
                add(chrome.text.inset_left(gutter), &NSCursor::IBeamCursor());
            }
            if diffing(&state) {
                for action in state.git.hunk_action_rects(chrome.text) {
                    add(action, &NSCursor::pointingHandCursor());
                }
            }
            // The find bar's fields and buttons are shared controls with
            // hotspots of their own; only the project results are added here.
            if let Some(find) = chrome.find
                && find.height > layout::FIND_ROW_HEIGHT * 2.0
            {
                add(Viewport { y: find.y + layout::FIND_ROW_HEIGHT * 2.0, height: find.height - layout::FIND_ROW_HEIGHT * 2.0, ..find }, &NSCursor::pointingHandCursor());
            }
            if let Some(rect) = chrome.sidebar {
                if state.git_open {
                    // Every control of Source Control is a shared one, with
                    // its own hotspot: see `add_hotspot_rects`.
                } else {
                let rows = state.tree.len().saturating_sub(state.tree.scroll).min(layout::sidebar_rows(rect));
                add(Viewport { y: rect.y + layout::SIDEBAR_HEADER_HEIGHT, height: rows as f32 * layout::SIDEBAR_ROW_HEIGHT, ..rect }, &NSCursor::pointingHandCursor());
                }
                let divider = Viewport { x: rect.x + rect.width - DIVIDER_GRAB, width: DIVIDER_GRAB * 2.0, ..rect };
                use objc2::ClassType;
                let cursor = if NSCursor::class().class_method(objc2::sel!(columnResizeCursor)).is_some() { NSCursor::columnResizeCursor() } else { #[allow(deprecated)] NSCursor::resizeLeftRightCursor() };
                add(divider, &cursor);
            }
            if let Some(branch) = status_branch_rect(&mut state, chrome.status) {
                add(branch, &NSCursor::pointingHandCursor());
            }
            if let Some(page) = chrome.preview.filter(|_| chrome.text.width > 0.0) {
                use objc2::ClassType;
                let band = Viewport { x: page.x - layout::PANE_GAP, width: layout::PANE_GAP + layout::PREVIEW_GRAB, ..page };
                let cursor = if NSCursor::class().class_method(objc2::sel!(columnResizeCursor)).is_some() { NSCursor::columnResizeCursor() } else { #[allow(deprecated)] NSCursor::resizeLeftRightCursor() };
                add(band, &cursor);
            }
            if let Some(rect) = chrome.terminal {
                use objc2::ClassType;
                let band = Viewport { y: rect.y - DIVIDER_GRAB, height: DIVIDER_GRAB * 2.0, ..rect };
                let cursor = if NSCursor::class().class_method(objc2::sel!(rowResizeCursor)).is_some() { NSCursor::rowResizeCursor() } else { #[allow(deprecated)] NSCursor::resizeUpDownCursor() };
                add(band, &cursor);
            }
            // The shared controls last: they are the most specific, and
            // each is exactly the rectangle that was drawn.
            self.add_hotspot_rects(&state.hotspots, None);

        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            // The palette is modal: the wheel scrolls its list, and never
            // the document or terminal behind it.
            if self.state().is_some_and(|state| state.palette.is_some()) {
                self.palette_wheel(event);
                return;
            }
            {
                // An extension's README scrolls a block at a time.
                let point = self.convertPoint_fromView(event.locationInWindow(), None);
                let over_readme = self.state().is_some_and(|state| {
                    state.extensions.as_ref().is_some_and(|p| {
                        p.details
                            && p.readme_rect
                                .is_some_and(|r| r.contains(point.x as f32, point.y as f32))
                    })
                });
                if over_readme {
                    let lines = self.wheel_lines(event, WheelTarget::Readme);
                    if lines != 0
                        && let Some(mut state) = self.state_mut()
                        && let Some(page) = state.extensions.as_mut()
                    {
                        page.scroll_readme(lines.signum());
                    }
                    self.request_redraw();
                    self.pump();
                    return;
                }
            }
            {
                let point = self.convertPoint_fromView(event.locationInWindow(), None);
                let panel = self.chrome().terminal;
                if panel.is_some_and(|rect| rect.contains(point.x as f32, point.y as f32)) {
                    let lines = self.wheel_lines(event, WheelTarget::Terminal);
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    let history = state.terminal.active_tab().map_or(0, |tab| {
                        tab.session.term.lock().unwrap_or_else(|e| e.into_inner()).scrollback_len()
                    });
                    let back = state.terminal.back as isize;
                    state.terminal.back = (back - lines).clamp(0, history as isize) as usize;
                    drop(state);
                    self.request_redraw();
                    self.pump();
                    return;
                }
            }
            {
                let point = self.convertPoint_fromView(event.locationInWindow(), None);
                let text = self.chrome().text;
                let reviewing = text.contains(point.x as f32, point.y as f32) && {
                    let Some(state) = self.state() else {
                        return;
                    };
                    let id = state.docs.active().id();
                    state.claude.as_ref().is_some_and(|c| c.reviews.contains_key(&id))
                };
                if reviewing {
                    let lines = self.wheel_lines(event, WheelTarget::Review);
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    let id = state.docs.active().id();
                    if lines != 0
                        && let Some(review) =
                            state.claude.as_mut().and_then(|c| c.reviews.get_mut(&id))
                    {
                        review.scroll_by(lines, text);
                        drop(state);
                        self.request_redraw();
                        self.pump();
                    }
                    return;
                }
            }
            let (git_open, diff_shown) = {
                let Some(state) = self.state() else {
                    return;
                };
                (state.git_open, diffing(&state))
            };
            if git_open || diff_shown {
                let point = self.convertPoint_fromView(event.locationInWindow(), None);
                let (x, y) = (point.x as f32, point.y as f32);
                let chrome = self.chrome();
                // The change list and the diff scroll independently, each
                // under the pointer, like the two columns they are. The
                // list only while Source Control is open; the diff while its
                // tab is showing.
                let over_list = git_open && chrome.sidebar.is_some_and(|r| r.contains(x, y));
                let over_diff = diff_shown && chrome.text.contains(x, y);
                if over_list || over_diff {
                    let lines = self.wheel_lines(event, WheelTarget::Git);
                    let Some(mut state) = self.state_mut() else {
                        return;
                    };
                    let g = crate::platform::git_panel::Sidebar::new(chrome.sidebar.unwrap_or(chrome.text));
                    if lines != 0 {
                        state.git.scroll(lines, g.list.contains(x, y), g, chrome.text);
                    }
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
            let Some(step) = self.state_mut().map(|mut state| state.selftest.pop_front()) else {
                return;
            };
            let Some(step) = step else {
                return;
            };
            self.play(&step);
            // Spaced out, so each event is handled and drawn before the next
            // arrives, as a person's would be.
            let delay = match step {
                Step::Idle(ms) => ms as f64 / 1000.0,
                _ => 0.03,
            };
            let _: () = unsafe {
                msg_send![self, performSelector: sel!(selfTestStep:),
                    withObject: None::<&AnyObject>, afterDelay: delay]
            };
        }

        /// Fires once per display refresh while there is work. Pauses itself
        /// when the buffer is idle.
        #[unsafe(method(onDisplayLink:))]
        fn on_display_link(&self, link: &CADisplayLink) {
            if let Some(size) = self.ivars().deferred_size.take() {
                self.resize(size);
            }
            let (tree, git) = self.ivars().deferred_change.replace((false, false));
            if tree {
                self.project_changed(crate::project::watch::Change::Tree);
            }
            if git {
                self.project_changed(crate::project::watch::Change::Git);
            }
            self.poll_tree_children();
            let git_changed = self.state_mut().is_some_and(|mut state| {
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
            self.poll_branches();
            self.poll_home();
            self.poll_mcp();
            self.poll_reloads();
            if self
                .state_mut().is_some_and(|mut state| state.symbols.poll())
            {
                self.request_redraw();
            }
            self.poll_http();
            self.poll_update();
            self.poll_ignored();
            self.refresh_after_watch();
            self.lsp_flush_changes();
            self.gutter_refresh();
            self.blame_refresh();
            self.bulb_refresh();
            self.ext_poll();
            self.sync_html_preview();
            self.claude_flush_selection();
            {
                // The link fires on any run-loop iteration, nested modal
                // loops included. Busy means try again next refresh.
                let Some(mut state) = self.state_mut() else {
                    return;
                };
                // The link knows the real refresh interval, which is not
                // necessarily 120Hz: an external 60Hz display, or ProMotion
                // throttling on battery, both change it.
                let interval = link.targetTimestamp() - link.timestamp();
                if interval > 0.0 && interval < 1.0 {
                    state.frame_interval = Duration::from_secs_f64(interval);
                }
                if state.message.as_ref().is_some_and(|(text, at)| at.elapsed() >= layout::message_lasts(text)) {
                    state.message = None;
                    self.ivars().needs_redraw.set(true);
                }
                if self.ivars().animating.get()
                    && state.last_draw.is_none_or(|at| at.elapsed() >= Duration::from_millis(33))
                {
                    self.ivars().needs_redraw.set(true);
                }
                if !self.ivars().needs_redraw.get() && !self.ivars().animating.get() && state.idle() {
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
            let follows = self.state().is_some_and(|state| {
                state.theme_choice == crate::platform::settings::ThemeChoice::System
            });
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

        #[unsafe(method(newWorkspace:))]
        fn action_new_workspace(&self, _sender: Option<&AnyObject>) {
            self.new_workspace();
            self.request_redraw();
            self.pump();
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

        /// Opens or closes the preview pane. Rendering belongs to an
        /// extension; with none installed, the command says where to get one.
        #[unsafe(method(togglePreview:))]
        fn action_toggle_preview(&self, _sender: Option<&AnyObject>) {
            let (open, command) = {
                let Some(state) = self.state() else {
                    return;
                };
                let active = state.docs.active().id();
                (
                    state.html_preview.as_ref().is_some_and(|p| p.buffer == active),
                    preview_command(&state),
                )
            };
            if open {
                self.close_preview();
            } else if let Some(command) = command {
                self.open_preview(command);
            } else {
                if let Some(mut state) = self.state_mut() {
                state.message = Some((
                    "No preview installed: get Markdown Preview from crc > Extensions".into(),
                    Instant::now(),
                ));
                }
            }
            self.request_redraw();
            self.pump();
        }

        /// The page in the whole tab, or back beside the text; opened in
        /// the whole tab when it was closed.
        #[unsafe(method(togglePreviewOnly:))]
        fn action_toggle_preview_only(&self, _sender: Option<&AnyObject>) {
            let (open, command) = {
                let Some(state) = self.state() else {
                    return;
                };
                let active = state.docs.active().id();
                (
                    state.html_preview.as_ref().is_some_and(|p| p.buffer == active),
                    preview_command(&state),
                )
            };
            if !open {
                let Some(command) = command else {
                    if let Some(mut state) = self.state_mut() {
                        state.message = Some((
                            "No preview installed: get Markdown Preview from crc > Extensions".into(),
                            Instant::now(),
                        ));
                    }
                    self.request_redraw();
                    return;
                };
                self.open_preview(command);
            }
            if let Some(mut state) = self.state_mut()
                && let Some(preview) = state.html_preview.as_mut()
            {
                preview.only = !open || !preview.only;
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
            if self.state().is_some_and(|state| state.find.is_none()) {
                self.open_find();
                return;
            }
            if self
                .state()
                .is_some_and(|state| state.find.as_ref().is_some_and(|bar| bar.project))
            {
                self.replace_in_project();
                return;
            }
            self.replace_all();
        }

        #[unsafe(method(toggleComment:))]
        fn action_toggle_comment(&self, _sender: Option<&AnyObject>) {
            let token = {
                let Some(state) = self.state() else {
                    return;
                };
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
            if let Some(mut state) = self.state_mut() {
                state.docs.active_mut().toggle_comment(token);
            }
            self.after_edit();
        }

        #[unsafe(method(duplicateLines:))]
        fn action_duplicate(&self, _sender: Option<&AnyObject>) {
            if let Some(mut state) = self.state_mut() {
                state.docs.active_mut().duplicate_lines();
            }
            self.after_edit();
        }

        #[unsafe(method(moveLineUp:))]
        fn action_move_line_up(&self, _sender: Option<&AnyObject>) {
            if let Some(mut state) = self.state_mut() {
                state.docs.active_mut().move_lines(false);
            }
            self.after_edit();
        }

        #[unsafe(method(moveLineDown:))]
        fn action_move_line_down(&self, _sender: Option<&AnyObject>) {
            if let Some(mut state) = self.state_mut() {
                state.docs.active_mut().move_lines(true);
            }
            self.after_edit();
        }

        #[unsafe(method(openQuickly:))]
        fn action_quick_open(&self, _sender: Option<&AnyObject>) {
            self.open_palette();
        }

        /// The palette on its commands: `>` typed already.
        #[unsafe(method(showCommands:))]
        fn action_show_commands(&self, _sender: Option<&AnyObject>) {
            self.open_palette_with(">");
        }

        #[unsafe(method(switchRepository:))]
        fn action_switch_repository(&self, _sender: Option<&AnyObject>) {
            self.open_repo_picker();
        }

        #[unsafe(method(switchBranch:))]
        fn action_switch_branch(&self, _sender: Option<&AnyObject>) {
            self.open_branch_picker(BranchIntent::Switch);
        }

        #[unsafe(method(renameBranch:))]
        fn action_rename_branch(&self, _sender: Option<&AnyObject>) {
            self.open_branch_picker(BranchIntent::Rename);
        }

        #[unsafe(method(deleteBranch:))]
        fn action_delete_branch(&self, _sender: Option<&AnyObject>) {
            self.open_branch_picker(BranchIntent::Delete);
        }

        #[unsafe(method(gitFetch:))]
        fn action_git_fetch(&self, _sender: Option<&AnyObject>) {
            self.git_remote(crate::project::git::Remote::Fetch);
        }

        #[unsafe(method(gitPull:))]
        fn action_git_pull(&self, _sender: Option<&AnyObject>) {
            self.git_remote(crate::project::git::Remote::Pull);
        }

        #[unsafe(method(gitPullRebase:))]
        fn action_git_pull_rebase(&self, _sender: Option<&AnyObject>) {
            self.git_remote(crate::project::git::Remote::PullRebase);
        }

        #[unsafe(method(gitPullMerge:))]
        fn action_git_pull_merge(&self, _sender: Option<&AnyObject>) {
            self.git_remote(crate::project::git::Remote::PullMerge);
        }

        #[unsafe(method(gitAbort:))]
        fn action_git_abort(&self, _sender: Option<&AnyObject>) {
            self.git_abort();
        }

        #[unsafe(method(gitContinueRebase:))]
        fn action_git_continue_rebase(&self, _sender: Option<&AnyObject>) {
            if let Some(mut state) = self.state_mut() {
                state.git.continue_rebase();
            }
            self.resume_display_link();
            self.request_redraw();
        }

        #[unsafe(method(gitPush:))]
        fn action_git_push(&self, _sender: Option<&AnyObject>) {
            self.git_remote(crate::project::git::Remote::Push);
        }

        #[unsafe(method(nextConflict:))]
        fn action_next_conflict(&self, _sender: Option<&AnyObject>) {
            self.step_conflict(true, false);
        }

        #[unsafe(method(previousConflict:))]
        fn action_previous_conflict(&self, _sender: Option<&AnyObject>) {
            self.step_conflict(false, false);
        }

        #[unsafe(method(acceptCurrent:))]
        fn action_accept_current(&self, _sender: Option<&AnyObject>) {
            self.take_conflict_at_caret(crate::project::conflict::Take::Current);
        }

        #[unsafe(method(acceptIncoming:))]
        fn action_accept_incoming(&self, _sender: Option<&AnyObject>) {
            self.take_conflict_at_caret(crate::project::conflict::Take::Incoming);
        }

        #[unsafe(method(acceptBoth:))]
        fn action_accept_both(&self, _sender: Option<&AnyObject>) {
            self.take_conflict_at_caret(crate::project::conflict::Take::Both);
        }

        #[unsafe(method(acceptBase:))]
        fn action_accept_base(&self, _sender: Option<&AnyObject>) {
            self.take_conflict_at_caret(crate::project::conflict::Take::Base);
        }

        #[unsafe(method(toggleConflictColumns:))]
        fn action_toggle_conflict_columns(&self, _sender: Option<&AnyObject>) {
            let Some(side) = self.state().map(|state| state.conflict_side) else {
                return;
            };
            self.set_conflict_side(!side);
        }

        #[unsafe(method(markResolved:))]
        fn action_mark_resolved(&self, _sender: Option<&AnyObject>) {
            self.mark_conflict_resolved();
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
            let Some(mut state) = self.state_mut() else {
                return;
            };
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

        #[unsafe(method(openExtensions:))]
        fn action_open_extensions(&self, _sender: Option<&AnyObject>) {
            self.open_extensions();
        }

        #[unsafe(method(openBreadcrumbPath:))]
        fn action_open_breadcrumb_path(&self, sender: Option<&AnyObject>) {
            let tag = sender
                .and_then(|s| s.downcast_ref::<NSMenuItem>())
                .map_or(-1, |item| item.tag());
            let path = usize::try_from(tag)
                .ok()
                .and_then(|i| self.state()?.crumb_paths.get(i).cloned());
            if let Some(path) = path {
                self.open_crumb_path(&path);
            }
        }

        #[unsafe(method(runExtensionCommand:))]
        fn action_run_extension_command(&self, sender: Option<&AnyObject>) {
            let tag = sender
                .and_then(|s| s.downcast_ref::<NSMenuItem>())
                .map_or(-1, |item| item.tag());
            self.run_extension(tag);
        }

        #[unsafe(method(quickFix:))]
        fn action_quick_fix(&self, _sender: Option<&AnyObject>) {
            self.quick_fix();
        }

        #[unsafe(method(organizeImports:))]
        fn action_organize_imports(&self, _sender: Option<&AnyObject>) {
            self.organize_imports(false);
            self.request_redraw();
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
            if let Some(mut state) = self.state_mut()
                && let Some(bar) = &mut state.find
            {
                bar.project = true;
            }
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
            self.cycle_tab(1);
        }

        #[unsafe(method(selectPreviousTab:))]
        fn action_prev_tab(&self, _sender: Option<&AnyObject>) {
            self.cycle_tab(-1);
        }

        /// Cmd-W closes the tab while more than one is open, and only falls
        /// through to closing the window on the last one. That is what every
        /// tabbed editor does and what muscle memory expects.
        #[unsafe(method(closeTabOrWindow:))]
        fn action_close_tab(&self, _sender: Option<&AnyObject>) {
            // Cmd-W in the terminal closes its session, as in any terminal.
            if self.terminal_has_keys() {
                let Some(mut state) = self.state_mut() else {
                    return;
                };
                let active = state.terminal.active;
                state.terminal.close_tab(active);
                drop(state);
                self.after_terminal_layout();
                return;
            }
            let (count, active, panes) = {
                let Some(state) = self.state() else {
                    return;
                };
                (state.docs.len(), state.docs.active_index(), pane_count(&state))
            };
            // In a split the last tab of a pane closes that pane, which
            // `close_tab` already does; only the last tab of the last pane
            // closes the window.
            if count > 1 || panes > 1 {
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
            let Some(index) = self.state().map(|state| state.context_tab) else {
                return;
            };
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
            let Some(keep) = self.state().and_then(|state| state.context_tab) else {
                return;
            };
            let Some(count) = self.state().map(|state| state.docs.len()) else {
                return;
            };
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
            let Some(count) = self.state().map(|state| state.docs.len()) else {
                return;
            };
            for index in (0..count).rev() {
                self.close_tab(index);
            }
            self.sync_title();
            self.reparse();
            self.request_redraw();
            self.pump();
        }

        #[unsafe(method(gitOpenContextChange:))]
        fn action_git_open_change(&self, _sender: Option<&AnyObject>) {
            if let Some((_, path)) = self.context_change_paths() {
                self.load_path(&path.to_string_lossy());
            }
        }

        #[unsafe(method(gitDiffContextChange:))]
        fn action_git_diff_change(&self, _sender: Option<&AnyObject>) {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if let Some((change, group)) = state.context_change {
                state.git.select(change);
                open_diff_tab(&mut state, change, group == crate::platform::git_panel::Group::Staged);
            }
            drop(state);
            self.sync_title();
            self.request_redraw();
            self.resume_display_link();
        }

        #[unsafe(method(gitStageContextChange:))]
        fn action_git_stage_change(&self, _sender: Option<&AnyObject>) {
            if let Some(mut state) = self.state_mut()
                && let Some((change, _)) = state.context_change
            {
                state.git.stage_index(change, true);
            }
            self.resume_display_link();
        }

        #[unsafe(method(gitUnstageContextChange:))]
        fn action_git_unstage_change(&self, _sender: Option<&AnyObject>) {
            if let Some(mut state) = self.state_mut()
                && let Some((change, _)) = state.context_change
            {
                state.git.stage_index(change, false);
            }
            self.resume_display_link();
        }

        #[unsafe(method(gitStageAll:))]
        fn action_git_stage_all(&self, _sender: Option<&AnyObject>) {
            if let Some(mut state) = self.state_mut() {
                state.git.stage_group(crate::platform::git_panel::Group::Changes);
            }
            self.resume_display_link();
        }

        #[unsafe(method(gitUnstageAll:))]
        fn action_git_unstage_all(&self, _sender: Option<&AnyObject>) {
            if let Some(mut state) = self.state_mut() {
                state.git.stage_group(crate::platform::git_panel::Group::Staged);
            }
            self.resume_display_link();
        }

        #[unsafe(method(gitCopyContextChangePath:))]
        fn action_git_copy_change_path(&self, _sender: Option<&AnyObject>) {
            if let Some((_, path)) = self.context_change_paths() {
                self.copy_and_say(&path.to_string_lossy());
            }
        }

        #[unsafe(method(gitCopyContextChangeRelativePath:))]
        fn action_git_copy_change_relative(&self, _sender: Option<&AnyObject>) {
            if let Some((relative, _)) = self.context_change_paths() {
                self.copy_and_say(&relative.to_string_lossy());
            }
        }

        #[unsafe(method(gitRevealContextChange:))]
        fn action_git_reveal_change(&self, _sender: Option<&AnyObject>) {
            if let Some((_, path)) = self.context_change_paths() {
                let _ = self.open(true, path.as_os_str());
            }
        }

        #[unsafe(method(extContextInstall:))]
        fn action_ext_install(&self, _sender: Option<&AnyObject>) {
            if let Some(id) = self.state().and_then(|state| state.context_ext.clone()) {
                self.extensions_action(crate::platform::extensions::Action::Install(id));
            }
        }

        #[unsafe(method(extContextToggle:))]
        fn action_ext_toggle(&self, _sender: Option<&AnyObject>) {
            if let Some(id) = self.state().and_then(|state| state.context_ext.clone()) {
                self.extensions_action(crate::platform::extensions::Action::Toggle(id));
            }
        }

        #[unsafe(method(extContextUninstall:))]
        fn action_ext_uninstall(&self, _sender: Option<&AnyObject>) {
            if let Some(id) = self.state().and_then(|state| state.context_ext.clone()) {
                self.extensions_action(crate::platform::extensions::Action::Uninstall(id));
            }
        }

        #[unsafe(method(extContextDetails:))]
        fn action_ext_details(&self, _sender: Option<&AnyObject>) {
            if let Some(id) = self.state().and_then(|state| state.context_ext.clone()) {
                self.extensions_action(crate::platform::extensions::Action::Select(id));
            }
        }

        #[unsafe(method(extInstallFolder:))]
        fn action_ext_folder(&self, _sender: Option<&AnyObject>) {
            self.extensions_action(crate::platform::extensions::Action::InstallFolder);
        }

        #[unsafe(method(extRefresh:))]
        fn action_ext_refresh(&self, _sender: Option<&AnyObject>) {
            self.extensions_action(crate::platform::extensions::Action::Refresh);
        }

        #[unsafe(method(mcpContextRun:))]
        fn action_mcp_run(&self, _sender: Option<&AnyObject>) {
            if let Some(action) = self.state().and_then(|state| state.context_mcp.clone()) {
                self.mcp_action(action);
            }
        }

        #[unsafe(method(mcpEdit:))]
        fn action_mcp_edit(&self, _sender: Option<&AnyObject>) {
            self.mcp_action(crate::platform::mcp_panel::Action::Edit);
        }

        #[unsafe(method(mcpReload:))]
        fn action_mcp_reload(&self, _sender: Option<&AnyObject>) {
            self.mcp_action(crate::platform::mcp_panel::Action::Reload);
        }

        #[unsafe(method(clearTerminal:))]
        fn action_clear_terminal(&self, _sender: Option<&AnyObject>) {
            self.clear_terminal();
        }

        #[unsafe(method(closeTabsToRight:))]
        fn action_close_tabs_to_right(&self, _sender: Option<&AnyObject>) {
            let Some(keep) = self.state().and_then(|state| state.context_tab) else {
                return;
            };
            let Some(count) = self.state().map(|state| state.docs.len()) else {
                return;
            };
            for index in ((keep + 1)..count).rev() {
                self.close_tab(index);
            }
            self.sync_title();
            self.reparse();
            self.request_redraw();
            self.pump();
        }

        #[unsafe(method(closeSavedTabs:))]
        fn action_close_saved_tabs(&self, _sender: Option<&AnyObject>) {
            let Some(clean) = self.state().map(|state| {
                state
                    .docs
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| !b.is_dirty())
                    .map(|(i, _)| i)
                    .collect::<Vec<_>>()
            }) else {
                return;
            };
            for index in clean.into_iter().rev() {
                self.close_tab(index);
            }
            self.sync_title();
            self.reparse();
            self.request_redraw();
            self.pump();
        }

        #[unsafe(method(copyProjectItemPath:))]
        fn action_copy_item_path(&self, _sender: Option<&AnyObject>) {
            let path = self.state().and_then(|state| state.tree.selected_path());
            if let Some(path) = path {
                self.copy_and_say(&path.to_string_lossy());
            }
        }

        #[unsafe(method(copyProjectItemRelativePath:))]
        fn action_copy_item_relative_path(&self, _sender: Option<&AnyObject>) {
            let path = self.state().and_then(|state| {
                let path = state.tree.selected_path()?;
                let root = state.tree.root()?;
                Some(path.strip_prefix(root).map(Path::to_path_buf).unwrap_or(path))
            });
            if let Some(path) = path {
                self.copy_and_say(&path.to_string_lossy());
            }
        }

        #[unsafe(method(copyContextTabPath:))]
        fn action_copy_context_path(&self, _sender: Option<&AnyObject>) {
            let Some(path) = self.state().map(|state| context_tab_path(&state)) else {
                return;
            };
            if let Some(path) = path {
                clipboard::write_text(&path.to_string_lossy());
                if let Some(mut state) = self.state_mut() {
                state.message =
                    Some((format!("copied {}", path.display()), Instant::now()));
                }
                self.request_redraw();
                self.pump();
            }
        }

        #[unsafe(method(revealContextTab:))]
        fn action_reveal_context_tab(&self, _sender: Option<&AnyObject>) {
            let Some(path) = self.state().map(|state| context_tab_path(&state)) else {
                return;
            };
            if let Some(path) = path {
                let _ = self.open(true, path.as_os_str());
            }
        }

        #[unsafe(method(revealInFinder:))]
        fn action_reveal(&self, _sender: Option<&AnyObject>) {
            let path = {
                let Some(state) = self.state() else {
                    return;
                };
                state
                    .tree
                    .selected_path()
                    .or_else(|| state.docs.active().path.clone())
            };
            let Some(path) = path else {
                return;
            };
            let _ = self.open(true, path.as_os_str());
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
                match self.open(false, url.as_ref()) {
                    Ok(_) => "a new issue is open in your browser; nothing is sent until you submit it".to_string(),
                    Err(e) => format!("could not open the browser: {e}"),
                }
            };
            if let Some(mut state) = self.state_mut() {
                state.message = Some((note, Instant::now()));
            }
            self.request_redraw();
            self.pump();
        }

        #[unsafe(method(checkForUpdates:))]
        fn action_check_for_updates(&self, _sender: Option<&AnyObject>) {
            self.start_update_check(true);
            if let Some(mut state) = self.state_mut() {
            state.message =
                Some(("checking for updates…".to_string(), Instant::now()));
            }
            self.request_redraw();
            self.pump();
        }

        #[unsafe(method(showCrashLogs:))]
        fn action_show_crash_logs(&self, _sender: Option<&AnyObject>) {
            let Some(dir) = crate::platform::report::logs_dir() else {
                return;
            };
            let _ = std::fs::create_dir_all(&dir);
            let _ = self.open(false, dir.as_os_str());
        }

        #[unsafe(method(revealProjectInFinder:))]
        fn action_reveal_project(&self, _sender: Option<&AnyObject>) {
            let Some(root) = self.state().map(|state| state.tree.root().map(Path::to_path_buf)) else {
                return;
            };
            if let Some(root) = root {
                let _ = self.open(true, root.as_os_str());
            }
        }

        #[unsafe(method(renameProjectItem:))]
        fn action_rename_project_item(&self, _sender: Option<&AnyObject>) {
            let path = {
                let Some(state) = self.state() else {
                    return;
                };
                state.tree.selected_path()
            };
            let Some(path) = path else { return };
            self.start_sidebar_edit(SidebarEditKind::Rename(path));
        }

        #[unsafe(method(trashProjectItem:))]
        fn action_trash_project_item(&self, _sender: Option<&AnyObject>) {
            let path = {
                let Some(state) = self.state() else {
                    return;
                };
                state
                    .tree
                    .selected
                    .and_then(|index| state.tree.rows().get(index))
                    .map(|entry| entry.path.clone())
            };
            let Some(path) = path else { return };
            let source_key = crate::platform::canonical(&path);
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            if self
                .state()
                .is_some_and(|state| all_docs(&state).any(|d| d.has_dirty_under(&source_key)))
            {
                ask(
                    MainThreadMarker::from(self),
                    "Unsaved changes are open",
                    "Save or close the affected tabs before moving this item to Trash.",
                    &["OK"],
                );
                return;
            }

            let answer = ask(
                MainThreadMarker::from(self),
                &format!("Move “{name}” to Trash?"),
                "The item can be recovered from the Trash.",
                &["Move to Trash", "Cancel"],
            );
            if answer != 0 {
                return;
            }

            let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
            let result = NSFileManager::defaultManager()
                .trashItemAtURL_resultingItemURL_error(&url, None);
            match result {
                Ok(()) => {
                    {
                        let Some(mut state) = self.state_mut() else {
                            return;
                        };
                        for docs in all_docs_mut(&mut state) {
                            docs.close_under(&source_key);
                        }
                        reveal_active_tab(&mut state);
                        state.message = Some((format!("moved {name} to Trash"), Instant::now()));
                    }
                    self.refresh_project_after_disk_change();
                    self.sync_title();
                    self.reparse();
                }
                Err(error) => {
                    if let Some(mut state) = self.state_mut() {
                    state.message = Some((
                        format!("could not move to Trash: {}", error.localizedDescription()),
                        Instant::now(),
                    ));
                    }
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
            let Some(focused) = self.state().map(|state| state.focused_pane) else {
                return;
            };
            self.close_pane(focused);
        }

        #[unsafe(method(focusNextPane:))]
        fn action_focus_next_pane(&self, _sender: Option<&AnyObject>) {
            self.cycle_pane(1);
        }

        #[unsafe(method(focusPreviousPane:))]
        fn action_focus_previous_pane(&self, _sender: Option<&AnyObject>) {
            self.cycle_pane(-1);
        }

        #[unsafe(method(openClaude:))]
        fn action_open_claude(&self, _sender: Option<&AnyObject>) {
            self.open_terminal(true);
        }

        /// Shows the terminal with the keyboard, or hides it when it has
        /// the keyboard already. Its sessions keep running while hidden.
        #[unsafe(method(toggleTerminal:))]
        fn action_toggle_terminal(&self, _sender: Option<&AnyObject>) {
            let Some(hide) = self.state().map(|state| state.terminal.has_keys()) else {
                return;
            };
            if hide {
                let Some(mut state) = self.state_mut() else {
                    return;
                };
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
            let Some(size) = self.state().map(|state| state.font_size + 1.0) else {
                return;
            };
            self.set_font_size(size);
        }

        #[unsafe(method(zoomOut:))]
        fn action_zoom_out(&self, _sender: Option<&AnyObject>) {
            let Some(size) = self.state().map(|state| state.font_size - 1.0) else {
                return;
            };
            self.set_font_size(size);
        }

        #[unsafe(method(zoomActual:))]
        fn action_zoom_actual(&self, _sender: Option<&AnyObject>) {
            self.set_font_size(crate::platform::settings::DEFAULT_FONT_SIZE);
        }

        #[unsafe(method(toggleSidebar:))]
        fn action_toggle_sidebar(&self, _sender: Option<&AnyObject>) {
            {
                let Some(mut state) = self.state_mut() else {
                    return;
                };
                state.sidebar = !state.sidebar;
            }
            self.request_redraw();
            self.pump();
        }

        #[unsafe(method(showMcp:))]
        fn action_show_mcp(&self, _sender: Option<&AnyObject>) {
            self.open_mcp();
        }

        #[unsafe(method(copyMcpServerCommand:))]
        fn action_copy_mcp_server_command(&self, _sender: Option<&AnyObject>) {
            self.copy_mcp_server_command();
        }

        #[unsafe(method(saveMcpCall:))]
        fn action_save_mcp_call(&self, _sender: Option<&AnyObject>) {
            self.save_mcp_call();
        }

        #[unsafe(method(runMcpCall:))]
        fn action_run_mcp_call(&self, _sender: Option<&AnyObject>) {
            self.run_mcp_call();
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
                let Some(state) = self.state() else {
                    return;
                };
                state.git_open && state.git.can_commit()
            };
            if ready {
                if let Some(mut state) = self.state_mut() {
                    state.git.commit();
                }
            } else {
                if !self.state().is_some_and(|state| state.git_open) {
                    self.set_sidebar_view(true);
                }
                if let Some(mut state) = self.state_mut() {
                    state.git_focus = true;
                }
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
                    let Some(state) = self.state() else {
                        return;
                    };
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
            let Some(state) = self.state() else {
                return Bool::NO;
            };
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
            } else if action == sel!(switchRepository:) {
                state.workspace.has_several()
            } else if action == sel!(paste:) {
                clipboard::has_text()
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
            } else if action == sel!(saveMcpCall:) {
                is_mcp_call(&state) && !state.mcp_calls.contains_key(&state.docs.active().id())
            } else if action == sel!(runMcpCall:) {
                is_mcp_call(&state)
            } else if action == sel!(sendRequest:) {
                state.http.is_none() && can_send_from(&state)
            } else if action == sel!(closePane:)
                || action == sel!(focusNextPane:)
                || action == sel!(focusPreviousPane:)
            {
                pane_count(&state) > 1
            } else if action == sel!(nextConflict:)
                || action == sel!(previousConflict:)
                || action == sel!(acceptCurrent:)
                || action == sel!(acceptIncoming:)
                || action == sel!(acceptBoth:)
            {
                active_conflicts(&state).is_some_and(|v| !v.conflicts.is_empty())
            } else if action == sel!(acceptBase:) {
                active_conflicts(&state).is_some_and(|v| v.has_base())
            } else if action == sel!(gitFetch:)
                || action == sel!(gitPull:)
                || action == sel!(gitPullRebase:)
                || action == sel!(gitPullMerge:)
                || action == sel!(gitPush:)
            {
                // Busy is answered by the command itself, in words.
                state.git.root().is_some()
            } else if action == sel!(switchBranch:)
                || action == sel!(renameBranch:)
                || action == sel!(deleteBranch:)
            {
                state.git.root().is_some()
            } else if action == sel!(gitStageAll:) {
                state.git.snapshot.as_ref().is_some_and(|s| s.changes.iter().any(|c| c.unstaged())) && !state.git.busy()
            } else if action == sel!(gitUnstageAll:) {
                state.git.staged_count() > 0 && !state.git.busy()
            } else if action == sel!(gitStageContextChange:) {
                state.context_change.is_some_and(|(_, g)| g == crate::platform::git_panel::Group::Changes) && !state.git.busy()
            } else if action == sel!(gitUnstageContextChange:) {
                state.context_change.is_some_and(|(_, g)| g == crate::platform::git_panel::Group::Staged) && !state.git.busy()
            } else if action == sel!(closeOtherTabs:) {
                state.docs.len() > 1
            } else if action == sel!(closeTabsToRight:) {
                state.context_tab.is_some_and(|index| index + 1 < state.docs.len())
            } else if action == sel!(clearTerminal:) {
                !state.terminal.tabs.is_empty()
            } else if action == sel!(copyProjectItemPath:) || action == sel!(copyProjectItemRelativePath:) {
                state.tree.selected.is_some()
            } else if action == sel!(gitAbort:) {
                state.git.in_progress().is_some()
            } else if action == sel!(gitContinueRebase:) {
                state.git.in_progress() == Some(crate::project::git::InProgress::Rebase)
            } else if action == sel!(toggleConflictColumns:) {
                // Checked while the columns show. `setState:` is behind a
                // feature of the AppKit crate crc does not enable.
                let on: isize = if state.conflict_side { 1 } else { 0 };
                let _: () = unsafe { msg_send![item, setState: on] };
                active_conflicts(&state).is_some_and(|v| !v.conflicts.is_empty())
            } else if action == sel!(markResolved:) {
                active_conflicts(&state).is_some_and(|v| v.can_resolve())
            } else if action == sel!(triggerCompletion:) {
                !field_has_keys(&state)
            } else if action == sel!(goToDefinition:)
                || action == sel!(showHover:)
                || action == sel!(quickFix:)
                || action == sel!(organizeImports:)
            {
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
            if let Some(mut state) = self.state_mut() {
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
        fn set_marked_text(&self, string: &AnyObject, selected: NSRange, replacement: NSRange) {
            let text = text_of(string);
            // Composing over committed text (reconversion, some Korean
            // input methods): that text is selected, so committing the
            // composition replaces it.
            if replacement.location != NSNotFound as usize
                && self.state_mut().is_some()
            {
                self.edit_focused(|buffer, _| {
                    buffer.select_input_range(replacement.location, replacement.length)
                });
            }
            if let Some(mut state) = self.state_mut() {
                // The input method's caret inside the composition, given in
                // UTF-16 units.
                let mut units = 0;
                state.marked_caret = text
                    .chars()
                    .take_while(|c| {
                        units += c.len_utf16();
                        units <= selected.location
                    })
                    .count();
                state.marked = (!text.is_empty()).then_some(text);
            }
            self.request_redraw();
            self.show_input_change();
        }

        /// Commit whatever is being composed, as it stands.
        #[unsafe(method(unmarkText))]
        fn unmark_text(&self) {
            let marked = match self.state_mut() {
                Some(mut state) => state.marked.take(),
                None => None,
            };
            if let Some(text) = marked {
                let nowhere = NSRange::new(NSNotFound as usize, 0);
                self.commit_text(&text, nowhere);
                self.show_input_change();
            }
        }

        #[unsafe(method(selectedRange))]
        fn selected_range(&self) -> NSRange {
            match self.state() {
                Some(state) => {
                    let (location, length) = focused_buffer(&state).input_selection();
                    NSRange::new(location, length)
                }
                None => NSRange::new(NSNotFound as usize, 0),
            }
        }

        #[unsafe(method(markedRange))]
        fn marked_range(&self) -> NSRange {
            let Some(state) = self.state() else {
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
            self.state().is_some_and(|state| state.marked.is_some())
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
            let in_view = match self.state() {
                Some(state) => {
                    let chrome = chrome_of(&state);
                    let text = chrome.text;
                    // Where the keys go: an input method's candidates open
                    // beside the field being typed in, not the document.
                    let field = if !field_has_keys(&state) {
                        None
                    } else if state.goto.is_some() || state.rename.is_some() {
                        Some(chrome.status)
                    } else if state.palette.is_some() {
                        let rect = layout::palette_rect(state.viewport, 1);
                        Some(Viewport { height: layout::PALETTE_ROW, ..rect })
                    } else if state.find.is_some() && state.sidebar_edit.is_none() && !state.git_focus {
                        chrome.find
                    } else {
                        chrome.sidebar
                    };
                    let caret = match field {
                        Some(rect) => Viewport { width: 0.0, ..rect },
                        None => layout::caret_rect(
                            state.docs.active(),
                            &state.renderer.atlas,
                            &layout::Markdown::of(state.syntax.markdown(state.docs.active().id())),
                            text,
                        )
                            .unwrap_or(Viewport { width: 0.0, height: 0.0, ..text }),
                    };
                    ns_rect(caret)
                }
                None => NSRect::ZERO,
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
            if let Some(mut state) = self.state_mut() {
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

    }
);

/// A fetched HEAD text arriving from a worker: the buffer it is for, and
/// the text, or none when the file is not in a repository.
type HeadText = (u64, Option<String>);

/// What the gutter knows about one open file.
struct GutterState {
    /// The file as of HEAD, with a file HEAD lacks as the empty string.
    /// `None` means the file is not in a repository, so there are no marks.
    head: Option<std::sync::Arc<String>>,
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
    /// The body of `menuForEvent:`, outside the class so it can return early.
    fn context_menu(&self, event: &NSEvent) -> Option<Retained<NSMenu>> {
        let mtm = MainThreadMarker::from(self);
        let point = self.convertPoint_fromView(event.locationInWindow(), None);
        let chrome = self.chrome();
        let sidebar = chrome.sidebar;
        let (x, y) = (point.x as f32, point.y as f32);

        let in_sidebar = sidebar.is_some_and(|r| r.contains(x, y));
        // Source Control fills the sidebar while it shows: a right-click
        // there is about Git, never about the file tree hidden behind it.
        let git_showing = self
            .state()
            .is_some_and(|state| state.git_open && state.extensions.is_none() && !state.mcp.open);
        if in_sidebar && git_showing {
            let rect = sidebar.expect("in_sidebar implies a sidebar rectangle");
            return Some(self.git_context_menu(rect, x, y));
        }
        // Extensions and MCP Servers fill it the same way, each with its own.
        let (extensions, mcp) = self.state().map_or((false, false), |state| {
            (state.extensions.is_some(), state.mcp.open)
        });
        if in_sidebar && extensions {
            return Some(self.extensions_context_menu(x, y));
        }
        if in_sidebar && mcp {
            return Some(self.mcp_context_menu(x, y));
        }
        // The terminal has its own: copy and paste, never the document's
        // Cut or Find.
        if chrome.terminal.is_some_and(|r| r.contains(x, y)) {
            return Some(terminal_context_menu(mtm));
        }
        let in_tab_bar = chrome.tabs.contains(x, y);
        let in_project = chrome.toolbar.contains(x, y)
            && self.state_mut().is_some_and(|mut state| {
                let State { tree, renderer, .. } = &mut *state;
                layout::toolbar_project(tree, &mut renderer.atlas, chrome.toolbar).contains(x, y)
            });

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
            if let Some(mut state) = self.state_mut() {
                state.context_tab = hit;
            }
            // The empty strip right of the tabs offers a new one.
            Some(match hit {
                Some(_) => tab_menu(mtm),
                None => tab_strip_menu(mtm),
            })
        } else if in_sidebar {
            let rect = sidebar.expect("in_sidebar implies a sidebar rectangle");
            let index = {
                let state = self.state()?;
                layout::sidebar_row_at(&state.tree, sidebar_field(&state), rect, y)
            };
            if let Some(index) = index {
                let mut state = self.state_mut()?;
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
            let (commands, lsp) = self.state().map(|state| {
                let lsp = lsp_server_for(&state, state.docs.active()).is_some();
                (state.ext.commands.clone(), lsp)
            })?;
            Some(editor_context_menu(mtm, &commands, lsp))
        }
    }

    /// The window's state, or `None` while a caller up the stack holds it:
    /// AppKit re-entered through a menu, a modal panel, a cursor rect or a
    /// redraw. Every path in goes through here or [`Self::state`], so a
    /// re-entry skips its work and asks for a redraw; with `panic = "abort"`
    /// a plain `borrow_mut` there would end the process.
    fn state_mut(&self) -> Option<std::cell::RefMut<'_, State>> {
        let state = self.ivars().state.try_borrow_mut().ok();
        if state.is_none() {
            self.ivars().needs_redraw.set(true);
        }
        state
    }

    /// [`Self::state_mut`] for reading.
    fn state(&self) -> Option<std::cell::Ref<'_, State>> {
        let state = self.ivars().state.try_borrow().ok();
        if state.is_none() {
            self.ivars().needs_redraw.set(true);
        }
        state
    }

    fn new(mtm: MainThreadMarker, state: State, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars {
            testing: std::env::var_os("CRC_SELFTEST").is_some(),
            state: RefCell::new(state),
            needs_redraw: Cell::new(false),
            animating: Cell::new(false),
            in_key_down: Cell::new(false),
            handling_key: Cell::new(false),
            draws_during_key_handler: Cell::new(0),
            deferred_change: Cell::new((false, false)),
            deferred_size: Cell::new(None),
            display_link: std::cell::OnceCell::new(),
            wheel_rest: Cell::new((None, 0.0)),
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

    /// Starts an update check unless one is already out.
    fn start_update_check(&self, manual: bool) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
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
            let Some(mut state) = self.state_mut() else {
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
                let _ = self.open(false, release.url.as_ref());
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
            if let Some(mut state) = self.state_mut() {
                state.message = Some((note, Instant::now()));
            }
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
        let Some(state) = self.state() else {
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
        if let Some(mut state) = self.state_mut() {
            state.cursor_rects_for = None;
        }
        self.ivars().needs_redraw.set(true);
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
    /// Hands `target` to `/usr/bin/open`: a URL to the browser, a folder to
    /// Finder, or with `reveal` a path shown selected in its Finder window.
    /// The documented tools for each, without NSWorkspace. A test instance
    /// opens nothing, so a self-test never raises a window of another app.
    fn open(&self, reveal: bool, target: &std::ffi::OsStr) -> std::io::Result<()> {
        if self.ivars().testing {
            return Ok(());
        }
        let mut command = std::process::Command::new("/usr/bin/open");
        if reveal {
            command.arg("-R");
        }
        crate::platform::spawn_reaped(command.arg(target))
    }

    /// A wake for a worker thread: it queues `work` with this view on the
    /// main thread. The view lives as long as the process, which every
    /// worker's thread is inside of; see `ViewPointer`.
    fn wake(&self, poll: fn(&EditorView)) -> crate::platform::dispatch::Wake {
        let pointer = ViewPointer(self as *const EditorView);
        Box::new(move || {
            let pointer = &pointer;
            let queued = Box::new(QueuedPoll {
                view: pointer.0,
                poll,
            });
            // SAFETY: `poll_on_main` takes the box back and frees it.
            unsafe {
                crate::platform::dispatch::on_main(
                    Box::into_raw(queued) as *mut std::ffi::c_void,
                    poll_on_main,
                )
            };
        })
    }

    fn pump(&self) {
        if self.ivars().handling_key.get() {
            self.request_redraw();
            return;
        }
        self.sync_native_preview();
        self.sync_html_preview();
        let ready = match self.state() {
            Some(state) => match state.last_draw {
                Some(t) => t.elapsed() >= state.frame_interval,
                None => true,
            },
            // Busy: leave it to the display link.
            None => false,
        };

        if ready {
            self.draw_now();
        } else {
            self.resume_display_link();
        }
    }

    fn sync_native_preview(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let path = state
            .docs
            .active()
            .is_preview_file()
            .then(|| state.docs.active().path.clone())
            .flatten();
        let text = chrome_of(&state).text;
        let frame = ns_rect(text);
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
            if let Some(mut state) = self.state_mut() {
                state.native_preview = Some(preview);
            }
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
        self.ivars().animating.set(layout::take_animating());
        self.arm_caret_blink();

        let Some(mut state) = self.state_mut() else {
            return;
        };
        if state.renderer.atlas.has_pending_shaping() {
            self.ivars().needs_redraw.set(true);
            if let Some(link) = self.ivars().display_link.get() {
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
        // The conflict buttons move with the text, so their cursor rects
        // are rebuilt when it scrolls or they change.
        let conflict_key = active_conflicts(&state).map(|view| {
            let buffer = state.docs.active();
            (
                buffer.id(),
                buffer.scroll_line,
                buffer.scroll_column,
                view.scroll,
                view.conflicts.len(),
                state.conflict_side,
            )
        });
        if state.conflict_cursor_key != conflict_key {
            state.conflict_cursor_key = conflict_key;
            state.cursor_rects_for = None;
        }
        let pointer_targets: Vec<Viewport> = state
            .extensions
            .iter()
            .flat_map(|page| {
                page.hits
                    .iter()
                    .filter(|_| page.details)
                    .chain(page.list_hits.iter())
                    .map(|(r, _)| *r)
            })
            .chain(state.lsp.bulb_rect)
            .chain(
                state
                    .mcp
                    .hits
                    .iter()
                    .filter(|_| state.mcp.open)
                    .map(|(r, _)| *r),
            )
            .collect();
        if state.pointer_targets != pointer_targets {
            state.pointer_targets = pointer_targets;
            state.cursor_rects_for = None;
        }
        let hotspots = layout::hotspots();
        if state.hotspots != hotspots {
            state.hotspots = hotspots;
            state.cursor_rects_for = None;
        }
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
        if let Some(link) = self.ivars().display_link.get() {
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
        let _ = self.ivars().display_link.set(link);
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
            let Some(mut state) = self.state_mut() else {
                return false;
            };
            if state.tree.root().is_none()
                && let Some(dir) = std::path::Path::new(path).parent()
                && dir.is_dir()
                && !too_broad_to_adopt(dir)
            {
                set_project_root(&mut state, dir);
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
            let Some(state) = self.state() else {
                return false;
            };
            pane_holding(&state, std::path::Path::new(path))
        };
        if let Some((pane, tab)) = elsewhere {
            self.focus_pane(pane);
            self.activate_tab(tab);
            return true;
        }

        let Some(mut state) = self.state_mut() else {
            return false;
        };
        match state.docs.open(path) {
            Ok(()) => {
                reveal_active_tab(&mut state);
                let format = state.docs.active().disk_format();
                let note = if state.docs.active().is_read_only() {
                    format!(
                        "opened {path} read-only: it is over {}",
                        crate::text::buffer::human_size(crate::text::buffer::read_only_limit())
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
                    ask(
                        MainThreadMarker::from(self),
                        "File too large to open",
                        &e.to_string(),
                        &["OK"],
                    );
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

    /// Brings the active document's parse tree up to date.
    ///
    /// Called after anything that changes the text or which document is
    /// showing. The rules, which trees belong to which document and when an
    /// incremental parse is allowed, live in [`SyntaxStore::update`], where
    /// they can be tested without a window. Files past [`reparse_budget`] get
    /// no highlighting rather than a stalled editor: the first parse is
    /// linear in file size even though later ones are incremental.
    fn reparse(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
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
        let Some(state) = self.state() else {
            return;
        };
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

    /// Records which tab the pointer is over, redrawing only when it changes.
    ///
    /// `mouseMoved:` fires on every pixel of movement; redrawing each time
    /// would spend a frame on nothing. A non-finite point means the pointer
    /// left the window.
    fn note_hover(&self, x: f32, y: f32) {
        layout::set_pointer(Some((x, y)));
        let hot_changed = self.state_mut().is_some_and(|mut state| {
            let hot = if x.is_finite() {
                layout::hotspot_at(&state.hotspots, x, y)
            } else {
                None
            };
            let changed = state.hot != hot;
            state.hot = hot;
            changed
        });
        if hot_changed {
            self.request_redraw();
            self.pump();
        }
        let chrome = self.chrome();
        let over = (x.is_finite() && chrome.tabs.contains(x, y))
            .then(|| {
                let state = self.state()?;
                state
                    .tab_hits
                    .iter()
                    .find(|hit| x >= hit.x0 && x < hit.x1)
                    .map(|hit| hit.index)
            })
            .flatten();
        let Some(mut state) = self.state_mut() else {
            return;
        };
        if state.hovered_tab == over {
            return;
        }
        state.hovered_tab = over;
        drop(state);
        self.invalidate_tab_cursors();
        self.request_redraw();
        self.pump();
    }

    /// Cursor rectangles and tooltips for the frame's shared controls,
    /// only those inside `within` when a modal panel covers the rest.
    fn add_hotspot_rects(&self, spots: &[layout::Hotspot], within: Option<Viewport>) {
        self.removeAllToolTips();
        let inside = |r: Viewport| {
            within.is_none_or(|w| {
                r.x >= w.x
                    && r.y >= w.y
                    && r.x + r.width <= w.x + w.width
                    && r.y + r.height <= w.y + w.height
            })
        };
        for spot in spots.iter().filter(|s| inside(s.rect)) {
            let rect = ns_rect(spot.rect);
            match spot.cursor {
                layout::Cursor::Pointing => {
                    self.addCursorRect_cursor(rect, &NSCursor::pointingHandCursor())
                }
                layout::Cursor::Text => self.addCursorRect_cursor(rect, &NSCursor::IBeamCursor()),
                layout::Cursor::Arrow => self.addCursorRect_cursor(rect, &NSCursor::arrowCursor()),
            }
            if spot.tip.is_some() {
                // Asked back through `view:stringForToolTip:point:userData:`,
                // which finds the control by the point.
                unsafe {
                    self.addToolTipRect_owner_userData(rect, self, std::ptr::null_mut());
                }
            }
        }
    }

    /// The close button appears and disappears with hover, so its pointer
    /// rectangle has to be rebuilt when it does.
    fn invalidate_tab_cursors(&self) {
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
    }

    /// Where everything in the window is, right now. The only place that is
    /// worked out: drawing, hit-testing and scrolling all read this.
    fn chrome(&self) -> Chrome {
        // Also re-entrant: AppKit calls resetCursorRects during tracking,
        // which can land mid-edit. An empty layout for one call is invisible;
        // panicking is not.
        let Some(state) = self.state() else {
            return Chrome::new(Viewport::new(0.0, 0.0), None, 0);
        };
        chrome_of(&state)
    }

    /// Text area size in whole rows and columns, which is what the buffer
    /// needs to keep the cursor on screen in both axes.
    fn grid(&self) -> (usize, usize) {
        let chrome = self.chrome();
        let Some(state) = self.state() else {
            return (24, 80);
        };
        let m = state.renderer.atlas.metrics;
        let gutter = layout::gutter_width(state.docs.active(), &state.renderer.atlas);
        (
            chrome.text.rows(m.line_height),
            chrome.text.columns(m.advance, gutter),
        )
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
            // Busy: quit without the session rather than abort.
            let (session, ephemeral) = view.state_mut().map_or((None, true), |mut state| {
                (state.quit_session.take(), state.ephemeral_session)
            });
            if !ephemeral && let Some(session) = session {
                session.save();
            }
            if let Some(mut state) = view.state_mut() {
                for server in state.lsp.servers.values_mut() {
                    server.shutdown();
                }
                // A moment, shared by all of them, to answer and exit.
                let until = Instant::now() + Duration::from_millis(300);
                for server in state.lsp.servers.values_mut() {
                    server.finish_shutdown(until);
                }
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
/// Times a language server is started again after stopping, per session.
const LSP_MAX_RESTARTS: u32 = 3;
/// A server that stopped sooner than this after starting is not restarted.
const LSP_RESTART_AFTER: Duration = Duration::from_secs(30);

/// Documents past this are not given to a language server: every pause in
/// typing would send the whole text again (full sync), and servers choke on
/// generated files that size anyway.
const LSP_MAX_BYTES: usize = 8 * 1024 * 1024;

fn lsp_language(buffer: &Buffer) -> Option<Language> {
    buffer
        .extension()
        .and_then(|e| Language::from_extension(&e))
        .filter(|_| buffer.path.is_some() && buffer.rope.len_bytes() <= LSP_MAX_BYTES)
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

/// Where the Extensions details draw: the focused pane's whole column, tabs
/// and breadcrumbs included, since those name a document the details are
/// not. Close, Escape or the strip gives the column back.
fn details_rect(chrome: &Chrome) -> Viewport {
    let top = chrome.tabs.y;
    Viewport {
        y: top,
        height: chrome.text.y + chrome.text.height - top,
        ..chrome.text
    }
}

fn lsp_server_for<'a>(state: &'a State, buffer: &Buffer) -> Option<&'a crate::lsp::client::Server> {
    let language = lsp_language(buffer)?;
    state
        .lsp
        .servers
        .get(&crate::lsp::servers::server_key(language))
        .filter(|s| s.is_ready())
}

/// The server for `key` (a `server_key`), when it has finished starting.
fn ready_server(
    lsp: &mut HashMap<Language, crate::lsp::client::Server>,
    key: Language,
) -> Option<&mut crate::lsp::client::Server> {
    lsp.get_mut(&key).filter(|s| s.is_ready())
}

/// Brings the active document's merge conflicts up to date with its text
/// and with Git's status. Cheap when nothing changed.
fn sync_conflicts(state: &mut State) {
    let State {
        docs,
        conflict_scans,
        git,
        ..
    } = state;
    let generation = git.generation();
    crate::platform::conflicts::sync(conflict_scans, docs.active(), generation, |path| {
        git.is_conflicted(path)
    });
    let open: Vec<u64> = docs.iter().map(|b| b.id()).collect();
    crate::platform::conflicts::prune(conflict_scans, &open);
}

/// The active document's conflicts, when it has any or Git has it
/// unmerged, and nothing else has the editor column.
fn active_conflicts(state: &State) -> Option<&crate::platform::conflicts::View> {
    if diffing(state) || active_review(state).is_some() {
        return None;
    }
    state
        .conflict_scans
        .get(&state.docs.active().id())?
        .view
        .as_ref()
}

fn active_conflicts_mut(state: &mut State) -> Option<&mut crate::platform::conflicts::View> {
    if diffing(state) || active_review(state).is_some() {
        return None;
    }
    let id = state.docs.active().id();
    state.conflict_scans.get_mut(&id)?.view.as_mut()
}

/// The columns are showing: side-by-side mode, on a document that has
/// conflicts.
fn side_by_side(state: &State) -> bool {
    state.conflict_side && active_conflicts(state).is_some_and(|v| !v.conflicts.is_empty())
}

/// The review in the active tab, when it is one.
/// Shows change `change` of Source Control in the diff tab: the one
/// already open, renamed for this change, or a new one. A diff tab left in
/// another pane moves to this one.
/// A change's file on disk.
fn change_path(state: &State, change: usize) -> Option<std::path::PathBuf> {
    let snapshot = state.git.snapshot.as_ref()?;
    Some(snapshot.root.join(&snapshot.changes.get(change)?.path))
}

fn open_diff_tab(state: &mut State, change: usize, staged: bool) {
    let Some(path) = state
        .git
        .snapshot
        .as_ref()
        .and_then(|s| s.changes.get(change))
        .map(|c| c.path.clone())
    else {
        return;
    };
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let label = format!(
        "{name} ({})",
        if staged { "Staged" } else { "Working tree" }
    );
    let existing = state
        .diff_tab
        .and_then(|id| state.docs.iter().position(|b| b.id() == id));
    match existing {
        Some(index) => {
            state.docs.switch(index);
            state.docs.active_mut().label = Some(label);
        }
        None => {
            if let Some(id) = state.diff_tab {
                for pane in &mut state.panes {
                    let index = pane.docs.iter().position(|b| b.id() == id);
                    if let Some(index) = index {
                        pane.docs.close(index);
                    }
                }
            }
            let mut tab = Buffer::generated(&label, "txt", "");
            tab.set_view_only(true);
            state.diff_tab = Some(tab.id());
            state.docs.push(tab);
        }
    }
    state.completion = None;
    reveal_active_tab(state);
}

/// Drops what was kept about a closed document: its HEAD text for the
/// gutter (up to 2 MB each), and the timers that would work on it.
fn forget_document(state: &mut State, id: u64) {
    if all_docs(state).any(|d| d.iter().any(|b| b.id() == id)) {
        return;
    }
    state.gutter.docs.remove(&id);
    state.gutter.dirty.remove(&id);
    state.gutter.pending.remove(&id);
    state.lsp.dirty.remove(&id);
    state.conflict_scans.remove(&id);
}

/// What has the editor column.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Column {
    /// The Extensions page's details.
    Extensions,
    /// A Source Control change, in its tab.
    GitDiff,
    /// A document's merge conflicts as columns.
    ConflictColumns,
    /// Claude's proposed change, in its tab.
    Review,
    /// Nothing open.
    Home,
    /// The active document, with a preview beside it when one is open.
    Text,
}

/// Which mode draws the editor column. Drawing, hit testing, cursor rects
/// and the preview all ask this, in this one order.
fn column_of(state: &State) -> Column {
    if ext_details(state) {
        Column::Extensions
    } else if diffing(state) {
        Column::GitDiff
    } else if side_by_side(state) {
        Column::ConflictColumns
    } else if active_review(state).is_some() {
        Column::Review
    } else if state.docs.is_home() {
        Column::Home
    } else {
        Column::Text
    }
}

/// Whether the Source Control diff has the editor column: its tab is the
/// active one.
fn diffing(state: &State) -> bool {
    state.diff_tab == Some(state.docs.active().id())
}

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

/// A file read for a reload, by buffer id.
type ReloadRead = (u64, std::io::Result<crate::text::buffer::DiskRead>);
/// Gutter marks worked out on a worker: buffer id, the text they are for,
/// the marks.
type GutterMarks = (u64, crate::text::rope::Rope, Vec<crate::project::git::Mark>);

/// Files changed on disk up to this size are reloaded at once; larger ones
/// are read on a worker so the window stays responsive.
const RELOAD_INLINE_BYTES: u64 = 4 * 1024 * 1024;

/// Opens, edits and saves each of `paths` that is not open, on up to eight
/// threads at once: most of a save is waiting on the disk, and a rename
/// across a few hundred files would otherwise wait on each in turn. `edit`
/// answers how many changes it made, or `None` when it could not. Results
/// come back in the order of `paths`.
fn edit_on_disk<F>(paths: &[std::path::PathBuf], edit: F) -> Vec<Option<usize>>
where
    F: Fn(&Path, &mut Buffer) -> Option<usize> + Sync,
{
    let run = |path: &std::path::PathBuf| {
        Buffer::open(path.clone()).ok().and_then(|mut buffer| {
            let n = edit(path, &mut buffer)?;
            if n > 0 {
                buffer.save(None).ok()?;
            }
            Some(n)
        })
    };
    if paths.len() < 4 {
        return paths.iter().map(run).collect();
    }
    let chunk = paths.len().div_ceil(8);
    std::thread::scope(|scope| {
        let workers: Vec<_> = paths
            .chunks(chunk)
            .map(|part| scope.spawn(|| part.iter().map(run).collect::<Vec<_>>()))
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().unwrap_or_default())
            .collect()
    })
}

/// The open document for `path`, found by one canonical key: open
/// documents keep canonical paths, so only `path` needs resolving.
fn open_doc_index(docs: &[&Documents], path: &Path) -> Option<(usize, usize)> {
    let key = crate::platform::canonical(path);
    docs.iter().enumerate().find_map(|(d, docs)| {
        docs.iter()
            .position(|b| {
                b.path
                    .as_deref()
                    .is_some_and(|p| is_open_path(p, path, &key))
            })
            .map(|i| (d, i))
    })
}

/// Whether an open document's path `open` is `path`, whose canonical form
/// is `key`: open documents keep canonical paths, so no lookup per
/// document. Resolve the key once, then call this for each.
fn is_open_path(open: &Path, path: &Path, key: &Path) -> bool {
    open == key || open == path
}

/// Whether `a` and `b` name the same file, by path or by what it resolves
/// to. Two lookups: for paths that are not both open documents.
fn same_file(a: &Path, b: &Path) -> bool {
    a == b || crate::platform::canonical(a) == crate::platform::canonical(b)
}

/// The window, as the tools Claude calls see it. Every method borrows the
/// state only for itself, since some of them call back into the view.
struct ClaudeHost<'a>(&'a EditorView);

impl crate::ide::mcp::Host for ClaudeHost<'_> {
    fn diagnostics(
        &self,
        path: Option<&Path>,
    ) -> Vec<(std::path::PathBuf, Vec<crate::lsp::Diagnostic>)> {
        let Some(state) = self.0.state() else {
            return Vec::new();
        };
        let mut out: Vec<_> = state
            .lsp
            .servers
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
        let Some(state) = self.0.state() else {
            return Vec::new();
        };
        let active = state.docs.active().id();
        let reviews = state.claude.as_ref().map(|c| &c.reviews);
        all_docs(&state)
            .flat_map(|docs| docs.iter())
            .filter(|b| reviews.is_none_or(|r| !r.contains_key(&b.id())))
            .map(|b| crate::ide::mcp::Editor {
                path: b.path.clone(),
                label: b.display_name(),
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
        let Some(state) = self.0.state() else {
            return Vec::new();
        };
        state
            .claude
            .as_ref()
            .map(|c| vec![c.root().to_path_buf()])
            .unwrap_or_default()
    }

    fn selection(&self) -> Option<crate::ide::mcp::Selection> {
        self.0.state().and_then(|state| claude_selection(&state))
    }

    fn document_state(&self, path: &Path) -> Option<(bool, bool)> {
        let state = self.0.state()?;
        let key = crate::platform::canonical(path);
        all_docs(&state)
            .flat_map(|docs| docs.iter())
            .find(|b| {
                b.path
                    .as_deref()
                    .is_some_and(|p| is_open_path(p, path, &key))
            })
            .map(|b| (b.is_dirty(), false))
    }

    fn save(&mut self, path: &Path) -> Result<bool, String> {
        let saved = {
            let Some(mut state) = self.0.state_mut() else {
                return Err("the editor is busy".into());
            };
            let key = crate::platform::canonical(path);
            let result = all_docs_mut(&mut state)
                .into_iter()
                .flat_map(|docs| docs.iter_mut())
                .find(|b| {
                    b.path
                        .as_deref()
                        .is_some_and(|p| is_open_path(p, path, &key))
                })
                .map(|b| b.save(None).map(|()| b.path.clone()));
            match result {
                None => return Ok(false),
                Some(Err(e)) => return Err(e.to_string()),
                Some(Ok(saved)) => {
                    note_files_written(&mut state);
                    saved
                }
            }
        };
        if let Some(path) = saved {
            self.0.lsp_flush_changes();
            if let Some(mut state) = self.0.state_mut() {
                for server in state.lsp.servers.values_mut() {
                    server.did_save(&path);
                }
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
            let Some(mut state) = self.0.state_mut() else {
                return Err("the editor is busy".into());
            };
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
            let Some(mut state) = self.0.state_mut() else {
                return Err("the editor is busy".into());
            };
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

/// Closes every one-line field that takes the keyboard ahead of the
/// document, so the one about to open is the one that gets the keys: the
/// order they are checked in is not the order they were opened in. The find
/// bar stays; the others sit ahead of it.
fn close_fields(state: &mut State) {
    state.sidebar_edit = None;
    state.rename = None;
    state.palette = None;
    state.goto = None;
    state.git_focus = false;
}

/// The sidebar's rename field for `name`, its stem selected as Finder does:
/// typing replaces the name and keeps the extension. A folder's whole name.
fn rename_field(name: &str, is_dir: bool) -> Buffer {
    let mut field = Buffer::from_text(name);
    let stem = if is_dir {
        name.len()
    } else {
        Path::new(name).file_stem().map_or(name.len(), |s| s.len())
    };
    field.select_range(0, stem);
    field
}

/// The file of the tab the tab-strip context menu was opened on.
fn context_tab_path(state: &State) -> Option<std::path::PathBuf> {
    state
        .context_tab
        .and_then(|i| state.docs.iter().nth(i))
        .and_then(|b| b.path.clone())
}

/// The editing keys every one-line field takes, as a text field does:
/// Left and Right (by word with Option), Home and End, Delete and Forward
/// Delete (by word with Option), Shift extending the selection. Whether
/// `code` was one of them.
fn field_key(field: &mut Buffer, code: u16, flags: NSEventModifierFlags) -> bool {
    let option = flags.contains(NSEventModifierFlags::Option);
    let motion = if flags.contains(NSEventModifierFlags::Shift) {
        Motion::Extend
    } else {
        Motion::Move
    };
    match code {
        key::LEFT if option => field.move_word_left(motion),
        key::RIGHT if option => field.move_word_right(motion),
        key::LEFT => field.move_left(motion),
        key::RIGHT => field.move_right(motion),
        key::HOME => field.move_line_start(motion),
        key::END => field.move_line_end(motion),
        key::DELETE if option => field.delete_word_backward(),
        key::DELETE => field.backspace(),
        key::FORWARD_DELETE if option => field.delete_word_forward(),
        key::FORWARD_DELETE => field.delete_forward(),
        _ => return false,
    }
    true
}

/// The find and replace fields' text inset inside their boxes.
const FIND_FIELD_PAD: f32 = 10.0;

/// Puts the caret of a one-line UI field where a click at `x`, measured
/// from the start of its text, lands, as `layout::push_ui_field` drew it.
fn place_field_caret(atlas: &mut crate::render::font::Atlas, field: &mut Buffer, x: f32) {
    let text = field.rope.to_string();
    let (shown, start) = layout::ui_input_window(&text, field.cursor());
    if let Some(line) = atlas.shape_ui(&shown) {
        field.place_cursor(start + line.byte_at_x(x), Motion::Move);
    }
}

/// What has the keyboard. Typing, the Edit menu, the key handlers and
/// the menu's enabled state all ask [`keys`], so they cannot disagree.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Keys {
    SidebarEdit,
    Rename,
    /// The commit message, once it has been clicked.
    GitMessage,
    Goto,
    Palette,
    FindQuery,
    FindReplace,
    Document,
}

/// Which field has the keyboard, in the one order they are offered it.
/// Opening a field closes the others ahead of the find bar
/// ([`close_fields`]), so normally at most one of them and the find bar
/// are open, and this order only settles that pair.
fn keys(state: &State) -> Keys {
    if state.sidebar_edit.is_some() {
        Keys::SidebarEdit
    } else if state.rename.is_some() {
        Keys::Rename
    } else if state.git_open && state.git_focus {
        Keys::GitMessage
    } else if state.goto.is_some() {
        Keys::Goto
    } else if state.palette.is_some() {
        Keys::Palette
    } else if let Some(bar) = state.find.as_ref().filter(|bar| bar.has_keys) {
        if bar.replacing {
            Keys::FindReplace
        } else {
            Keys::FindQuery
        }
    } else {
        Keys::Document
    }
}

/// Whether the active document's page has the whole tab, its text hidden.
fn preview_only(state: &State) -> bool {
    let active = state.docs.active().id();
    state
        .html_preview
        .as_ref()
        .is_some_and(|p| p.only && p.buffer == active)
        && !preview_displaced(state)
}

/// Whether typing goes to a field (palette, find, go to line, a sidebar
/// name, the commit message) rather than to the document.
fn field_has_keys(state: &State) -> bool {
    keys(state) != Keys::Document
}

/// The field that receives text input and editing menu commands.
fn focused_buffer(state: &State) -> &Buffer {
    let field = match keys(state) {
        Keys::SidebarEdit => state.sidebar_edit.as_ref().map(|edit| &edit.field),
        Keys::Rename => state.rename.as_ref().map(|rename| &rename.field),
        Keys::GitMessage => Some(&state.git.message),
        Keys::Goto => state.goto.as_ref(),
        Keys::Palette => state.palette.as_ref().map(|(query, _)| query),
        Keys::FindQuery => state.find.as_ref().map(|bar| &bar.query),
        Keys::FindReplace => state.find.as_ref().map(|bar| &bar.replacement),
        Keys::Document => None,
    };
    field.unwrap_or_else(|| state.docs.active())
}

/// Draws a pane that does not have the keyboard: its tab bar, breadcrumbs
/// and document, with highlighting but without caret, find or overlays.
#[allow(clippy::too_many_arguments)]
fn draw_other_pane(
    store: &mut PaneStore,
    rects: &layout::PaneChrome,
    tree: &Tree,
    syntax: &mut SyntaxStore,
    responses: &HashMap<u64, crate::http::view::View>,
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
    // A document here can change without the focus (regenerated, reloaded
    // from disk): its tree follows before it is drawn, or colours land on
    // the neighbouring tokens.
    if store.docs.active().has_pending_edits() {
        syntax.update(store.docs.active_mut(), reparse_budget());
    }
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
        store.docs.is_home(),
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
            None,
            &[],
            &mut hits,
        );
        return;
    }
    let mut spans = Vec::new();
    let rows = layout::screen_rows(buffer, text, renderer.atlas.metrics.line_height);
    if syntax.has(buffer.id())
        && let Some(std::ops::Range {
            start: first,
            end: last,
        }) = layout::lines_of(&rows)
    {
        let total = buffer.rope.len_lines();
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
        &rows,
        theme,
        "",
        None,
        &spans,
        &layout::Markdown::of(syntax.markdown(buffer.id())),
        false,
        false,
        glyphs,
    );
    if let Some(entry) = marks.get(&buffer.id()) {
        layout::push_gutter_marks(glyphs, &renderer.atlas, text, &rows, theme, &entry.marks);
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
    sync_conflicts(state);
    let chrome = chrome_of(state);
    let column = column_of(state);
    let side = column == Column::ConflictColumns;
    let conflict_hits = {
        let text_rect = chrome.text;
        let State {
            docs,
            renderer,
            conflict_scans,
            ..
        } = &mut *state;
        // Not under a diff, a review or the Extensions page, which draw
        // over the text the controls belong to.
        let shown = matches!(column, Column::ConflictColumns | Column::Text);
        let buffer = docs.active();
        match conflict_scans
            .get_mut(&buffer.id())
            .and_then(|s| s.view.as_mut())
        {
            Some(view) if shown => {
                let mut hits = match chrome.response {
                    Some(strip) => {
                        crate::platform::conflicts::strip_hits(view, &mut renderer.atlas, strip)
                    }
                    None => Vec::new(),
                };
                if side {
                    hits.extend(crate::platform::conflicts::side_hits(
                        view,
                        buffer.rope.len_lines(),
                        &mut renderer.atlas,
                        text_rect,
                    ));
                } else {
                    let rows =
                        layout::screen_rows(buffer, text_rect, renderer.atlas.metrics.line_height);
                    hits.extend(crate::platform::conflicts::inline_hits(
                        view,
                        buffer,
                        &rows,
                        &mut renderer.atlas,
                        text_rect,
                    ));
                }
                hits
            }
            _ => Vec::new(),
        }
    };
    let mut frame = Frame::default();
    let State {
        tree,
        renderer,
        docs,
        responses,
        claude,
        terminal,
        git_open,
        git,
        mcp,
        tab_hits,
        sidebar_edit,
        extensions,
        diff_tab,
        ..
    } = state;
    for (index, item) in layout::activity_items(chrome.activity)
        .into_iter()
        .enumerate()
    {
        if item.y + item.height <= chrome.activity.y + chrome.activity.height {
            frame.push(Hit::Activity(index), item);
        }
    }
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
    if let Some(page) = chrome.preview.filter(|_| chrome.text.width > 0.0) {
        frame.push(
            Hit::PreviewDivider,
            Viewport {
                x: page.x - layout::PANE_GAP,
                width: layout::PANE_GAP + layout::PREVIEW_GRAB,
                ..page
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
        if *git_open && extensions.is_none() {
            let g = crate::platform::git_panel::Sidebar::new(rect);
            frame.push(Hit::GitBranch, g.branch);
            if git.repo.is_some() {
                frame.push(Hit::GitRepo, g.repo);
            }
            for (name, rect) in [
                ("refresh", g.refresh),
                ("more", g.more),
                ("pull", g.pull),
                ("push", g.push),
                ("message", g.message),
                ("commit", g.commit),
            ] {
                frame.push(Hit::Git(name), rect);
            }
            let rows = git.rows(g);
            for (i, (entry, rect)) in rows.iter().enumerate() {
                if matches!(entry, crate::platform::git_panel::Entry::File { .. }) {
                    frame.push(Hit::GitToggle(i), g.toggle(*rect));
                }
            }
            for (i, (_, rect)) in rows.into_iter().enumerate() {
                frame.push(Hit::GitRow(i), rect);
            }
        }
        if !*git_open && extensions.is_none() && !mcp.open {
            let (_, actions) = layout::sidebar_actions(rect);
            for (index, action) in actions.into_iter().enumerate() {
                frame.push(Hit::SidebarAction(index), action);
            }
            // Rows as drawn: an inserted name field shifts the rows under
            // it, and is not itself a tree row.
            let field = sidebar_edit.as_ref().map(SidebarEdit::field);
            let total = tree.len() + usize::from(field.is_some_and(|f| f.inserted));
            let visible_rows = layout::sidebar_rows(rect);
            let first = tree.scroll.min(total.saturating_sub(1));
            for visible in first..(first + visible_rows).min(total) {
                let Some(index) = layout::tree_row_at(visible, field) else {
                    continue;
                };
                frame.push(
                    Hit::SidebarRow(index),
                    layout::sidebar_row_rect(rect, visible - first),
                );
            }
        }
    }
    for (index, pane) in &chrome.others {
        frame.push(Hit::Pane(*index), pane.whole());
    }
    let covered = extensions.as_ref().is_some_and(|p| p.details);
    for hit in tab_hits.iter().filter(|_| !covered) {
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
    if !covered && !docs.is_home() && *diff_tab != Some(docs.active().id()) {
        let crumbs = layout::breadcrumb_segments(
            docs.active(),
            tree,
            &mut renderer.atlas,
            chrome.breadcrumbs,
        );
        for (index, crumb) in crumbs.into_iter().enumerate() {
            frame.push(Hit::Breadcrumb(index), crumb.rect);
        }
    }
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
    // A review or response owns the strip; conflicts only draw there when
    // neither does.
    if claude
        .as_ref()
        .and_then(|c| c.reviews.get(&docs.active().id()))
        .is_none()
        && !responses.contains_key(&docs.active().id())
    {
        for (hit, rect) in conflict_hits {
            frame.push(hit, rect);
        }
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
        frame.push(
            Hit::TerminalHide,
            crate::platform::terminal::header_hide(header),
        );
        // The screen first, so a script's offsets land on rows and columns;
        // the padding around it is the terminal too.
        frame.push(Hit::Terminal, crate::platform::terminal::split(rect).1);
        frame.push(Hit::Terminal, rect);
    }
    frame.push(Hit::Text, chrome.text);
    if let Some(branch) = status_branch_rect(state, chrome.status) {
        frame.push(Hit::StatusBranch, branch);
    }
    frame.push(Hit::Status, chrome.status);
    frame
}

/// The right half of the status line, where the branch, counts and the
/// caret's position are written.
fn status_detail_rect(status: Viewport) -> Viewport {
    let right_width = 320.0f32.min(status.width * 0.55);
    Viewport {
        x: status.x + status.width - right_width,
        width: (right_width - 12.0).max(0.0),
        ..status
    }
}

/// Where the status line writes the branch, as `status_texts` lays it out:
/// first in the right half, after Claude's mark when it is there.
fn status_branch_rect(state: &mut State, status: Viewport) -> Option<Viewport> {
    let branch = state.git.branch_status();
    if branch.is_empty() {
        return None;
    }
    let detail = status_detail_rect(status);
    let start = state.status_detail_x.unwrap_or(detail.x);
    let claude = if state.claude.as_ref().is_some_and(|c| c.is_connected()) {
        layout::ui_text_width(&mut state.renderer.atlas, "✻ Claude     ")
    } else {
        0.0
    };
    let width = layout::ui_text_width(&mut state.renderer.atlas, &branch);
    Some(Viewport {
        x: start + claude,
        width: width.min((detail.x + detail.width - start - claude).max(0.0)),
        ..status
    })
}

/// A layout rectangle as AppKit takes it. The view is flipped, so the two
/// share an origin at the top left.
fn ns_rect(r: Viewport) -> NSRect {
    NSRect::new(
        NSPoint::new(r.x as f64, r.y as f64),
        NSSize::new(r.width as f64, r.height as f64),
    )
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
            bar.results.len().min(FIND_RESULT_ROWS)
        } else {
            0
        }
    });
    let response = state.responses.contains_key(&state.docs.active().id())
        || active_review(state).is_some()
        || active_conflicts(state).is_some();
    let mut chrome = Chrome::with_panes(
        state.viewport,
        sidebar,
        find_rows,
        response,
        pane_count(state),
        state.focused_pane,
        state.terminal.open.then_some(state.terminal.height),
    );
    if state
        .html_preview
        .as_ref()
        .is_some_and(|p| p.buffer == state.docs.active().id())
        && !preview_displaced(state)
    {
        let only = state.html_preview.as_ref().is_some_and(|p| p.only);
        chrome.split_preview(1.0 - state.preview_share, only);
    }
    chrome
}

/// `recent` with `root`, if any, moved to the front, deduplicated by
/// canonical path and cut to the session's limit.
fn with_recent(
    mut recent: Vec<std::path::PathBuf>,
    root: Option<&Path>,
) -> Vec<std::path::PathBuf> {
    if let Some(root) = root {
        let root = crate::platform::canonical(root);
        recent.retain(|p| crate::platform::canonical(p) != root);
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
    ask(
        mtm,
        "crc quit unexpectedly.",
        &text,
        &["Restore", "Discard"],
    ) == 0
}

/// A warning with `message`, `detail` and `buttons`, the first the
/// default. Answers which button was clicked, from 0.
fn ask(mtm: MainThreadMarker, message: &str, detail: &str, buttons: &[&str]) -> usize {
    // NSAlertFirstButtonReturn; the ones after it count up.
    const FIRST: isize = 1000;
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Warning);
    alert.setMessageText(&NSString::from_str(message));
    alert.setInformativeText(&NSString::from_str(detail));
    for button in buttons {
        alert.addButtonWithTitle(&NSString::from_str(button));
    }
    (alert.runModal() - FIRST).max(0) as usize
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
                let name = match path.parent().and_then(|p| p.file_name()) {
                    Some(folder) => format!("{name} (in {})", folder.to_string_lossy()),
                    None => name,
                };
                if d.changed_on_disk() {
                    format!("{name}, changed on disk since")
                } else {
                    name
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

/// What the branch list is open for.
#[derive(Clone, Debug, PartialEq, Eq)]
enum BranchIntent {
    /// Switch to one, or create one by typing a new name.
    Switch,
    /// Pick one to rename.
    Rename,
    /// Type the new name for this one.
    RenameTo(String),
    /// Pick one to delete; not the current one.
    Delete,
}

/// The branch list in the palette.
struct BranchPick {
    branches: Vec<crate::project::git::Branch>,
    intent: BranchIntent,
    /// Branches whose commits HEAD does not have, read for a delete.
    unmerged: Vec<String>,
}

/// What choosing a palette row does.
enum Pick {
    File(std::path::PathBuf),
    /// A menu item's action, and its tag.
    Command(Sel, isize),
    /// A definition: in another file, or in the active document (`None`),
    /// and its zero-based line.
    Symbol(Option<std::path::PathBuf>, u32),
    Branch(String),
    NewBranch(String),
    /// A branch to delete, and whether it has commits HEAD does not.
    DeleteBranch(String, bool),
    RenameBranch(String),
    RenameBranchTo(String, String),
    /// A repository of the workspace, for Source Control.
    Repo(std::path::PathBuf),
    /// A code action, and the server that offered it.
    Action(Language, crate::lsp::CodeAction),
}

/// The code action picker's rows: the actions whose titles match, in the
/// order given.
fn action_rows(
    server: Language,
    actions: &[crate::lsp::CodeAction],
    query: &str,
) -> Vec<(layout::PaletteRow, Pick)> {
    crate::project::finder::ranked(actions, query, |a| &a.title)
        .into_iter()
        .map(|i| {
            let action = &actions[i];
            let kind = action.kind.split('.').next().unwrap_or("");
            let detail = match (&action.disabled, action.preferred) {
                (Some(reason), _) => reason.clone(),
                (None, true) => "preferred".into(),
                (None, false) => match kind {
                    "quickfix" => "quick fix",
                    "refactor" => "refactor",
                    "source" => "source",
                    _ => "",
                }
                .into(),
            };
            (
                layout::PaletteRow {
                    icon: (kind == "quickfix").then_some(crate::project::icons::LIGHTBULB),
                    title: action.title.clone(),
                    detail,
                    shortcut: String::new(),
                },
                Pick::Action(server, action.clone()),
            )
        })
        .collect()
}

/// A repository in the palette: its label, its folder, and whether
/// Source Control shows it now.
#[derive(Clone, Debug)]
struct RepoRow {
    label: String,
    path: std::path::PathBuf,
    current: bool,
}

/// The workspace's repositories as palette rows, the current one marked.
fn repo_rows(repos: &[RepoRow], query: &str) -> Vec<(layout::PaletteRow, Pick)> {
    crate::project::finder::ranked(repos, query, |r| &r.label)
        .into_iter()
        .map(|i| {
            let repo = &repos[i];
            (
                layout::PaletteRow {
                    icon: Some(crate::project::icons::FOLDER_OUTLINE),
                    title: repo.label.clone(),
                    detail: if repo.current {
                        "shown".into()
                    } else {
                        String::new()
                    },
                    shortcut: String::new(),
                },
                Pick::Repo(repo.path.clone()),
            )
        })
        .collect()
}

/// The previous visit to the workspace at `root`, recording this one.
fn visit_workspace(root: &Path) -> Option<u64> {
    let before = crate::project::workspace::last_seen(root);
    crate::project::workspace::mark_seen(root, crate::platform::unix_seconds());
    before
}

/// Reads what Home shows on a worker; the answer replaces the last one.
fn spawn_home_summary(
    workspace: crate::project::workspace::Workspace,
    since: Option<u64>,
) -> mpsc::Receiver<crate::project::workspace::Summary> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(workspace.summary(since));
    });
    rx
}

/// Reads Home's workspace sections again, when a project is open.
fn start_home_summary(state: &mut State) {
    if state.tree.root().is_some() {
        state.home_rx = Some(spawn_home_summary(
            state.workspace.clone(),
            state.home_since,
        ));
    }
}

/// Source Control for the workspace's selected repository, named in its
/// header when there are several to choose from.
fn git_panel_for(
    workspace: &crate::project::workspace::Workspace,
) -> crate::platform::git_panel::Panel {
    let mut panel = crate::platform::git_panel::Panel::new(workspace.git_dir().to_path_buf());
    panel.repo = workspace.selected_label();
    panel.guard = workspace.guard();
    panel
}

/// The branch list's rows for what it is open for.
fn branch_rows(pick: &BranchPick, query: &str) -> Vec<(layout::PaletteRow, Pick)> {
    let branches = &pick.branches;
    let row = |title: String, detail: String, pick: Pick| {
        (
            layout::PaletteRow {
                icon: None,
                title,
                detail,
                shortcut: String::new(),
            },
            pick,
        )
    };
    match &pick.intent {
        BranchIntent::Switch => switch_rows(branches, query),
        BranchIntent::Rename => crate::project::finder::ranked(branches, query, |b| &b.name)
            .into_iter()
            .map(|i| {
                let b = &branches[i];
                let detail = if b.current { "current branch" } else { "" };
                row(
                    b.name.clone(),
                    detail.into(),
                    Pick::RenameBranch(b.name.clone()),
                )
            })
            .collect(),
        BranchIntent::RenameTo(old) => {
            let name = query.trim();
            if name.is_empty() || name == old || branches.iter().any(|b| b.name == name) {
                return Vec::new();
            }
            vec![row(
                format!("Rename \u{201c}{old}\u{201d} to \u{201c}{name}\u{201d}"),
                "keeps its upstream and history".into(),
                Pick::RenameBranchTo(old.clone(), name.to_owned()),
            )]
        }
        BranchIntent::Delete => crate::project::finder::ranked(branches, query, |b| &b.name)
            .into_iter()
            .filter(|&i| !branches[i].current)
            .map(|i| {
                let name = branches[i].name.clone();
                let unmerged = pick.unmerged.contains(&name);
                let detail = if unmerged {
                    "has commits not on this branch: asks first"
                } else {
                    "merged"
                };
                row(
                    name.clone(),
                    detail.into(),
                    Pick::DeleteBranch(name, unmerged),
                )
            })
            .collect(),
    }
}

/// Switch Branch's rows: branches matching the query, then a row to create
/// one named by the query when no branch has that name.
fn switch_rows(
    branches: &[crate::project::git::Branch],
    query: &str,
) -> Vec<(layout::PaletteRow, Pick)> {
    let mut rows: Vec<(layout::PaletteRow, Pick)> =
        crate::project::finder::ranked(branches, query, |b| &b.name)
            .into_iter()
            .map(|i| {
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
    branches: Option<&'a BranchPick>,
    /// Set while picking a code action.
    actions: Option<&'a (Language, Vec<crate::lsp::CodeAction>)>,
    /// Set while picking a repository.
    repos: Option<&'a [RepoRow]>,
}

/// The rows of the open palette for its query; none when it is closed.
fn open_palette_rows(state: &State) -> Vec<(layout::PaletteRow, Pick)> {
    state.palette.as_ref().map_or_else(Vec::new, |(query, _)| {
        palette_rows(&palette_sources(state), &query.rope.to_string())
    })
}

fn palette_sources(state: &State) -> PaletteSources<'_> {
    PaletteSources {
        finder: &state.finder,
        commands: &state.commands,
        symbols: &state.symbols,
        root: state.tree.root(),
        branches: state.branch_list.as_ref(),
        actions: state.lsp.action_list.as_ref(),
        repos: state.repo_list.as_deref(),
    }
}

/// Which list the palette is picking from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PaletteMode {
    /// Files, and commands and symbols by prefix.
    Open,
    Branch,
    Action,
    Repo,
}

impl PaletteMode {
    fn of(sources: &PaletteSources<'_>) -> PaletteMode {
        if sources.repos.is_some() {
            PaletteMode::Repo
        } else if sources.branches.is_some() {
            PaletteMode::Branch
        } else if sources.actions.is_some() {
            PaletteMode::Action
        } else {
            PaletteMode::Open
        }
    }
}

/// The palette's heading and what it says when nothing matches, by mode.
fn palette_heading(
    query: &str,
    mode: PaletteMode,
    intent: Option<&BranchIntent>,
) -> (&'static str, &'static str) {
    match mode {
        PaletteMode::Branch => {
            return match intent {
                Some(BranchIntent::Rename) => ("Rename which branch?", "No branches"),
                Some(BranchIntent::RenameTo(_)) => {
                    ("Type the new name", "Type a name no branch has")
                }
                Some(BranchIntent::Delete) => ("Delete which branch?", "No other branches"),
                _ => ("Switch branch, or type a new name", "No branches"),
            };
        }
        PaletteMode::Action => return ("Code actions", "No matching actions"),
        PaletteMode::Repo => return ("Show which repository?", "No matching repositories"),
        PaletteMode::Open => {}
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
        actions,
        repos,
    } = *sources;
    if let Some(repos) = repos {
        return repo_rows(repos, query);
    }
    if let Some(branches) = branches {
        return branch_rows(branches, query);
    }
    if let Some((server, actions)) = actions {
        return action_rows(*server, actions, query);
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
                    Pick::Command(command.action, command.tag),
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
    let menu = context_menu_new(mtm);
    let add = |title: &str, action: Sel| menu.addItem(&menu_item(mtm, title, action));
    menu.addItem(&hinted_menu_item(
        mtm,
        "Close Tab",
        sel!(closeContextTab:),
        "w",
        NSEventModifierFlags::Command,
    ));
    add("Close Other Tabs", sel!(closeOtherTabs:));
    add("Close Tabs to the Right", sel!(closeTabsToRight:));
    add("Close Saved Tabs", sel!(closeSavedTabs:));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    add("Move Tab Left", sel!(moveContextTabLeft:));
    add("Move Tab Right", sel!(moveContextTabRight:));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    add("Copy Path", sel!(copyContextTabPath:));
    add("Reveal in Finder", sel!(revealContextTab:));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    // Everything, last and apart, so it is not hit by a slip from Close.
    add("Close All Tabs", sel!(closeAllTabs:));
    menu
}

fn menu_item(mtm: MainThreadMarker, title: &str, action: Sel) -> Retained<NSMenuItem> {
    keyed_menu_item(mtm, title, action, "")
}

/// A context-menu item that shows the shortcut the same command has in the
/// menu bar. The key works from the menu bar either way; showing it here
/// is how a right-click teaches it.
fn hinted_menu_item(
    mtm: MainThreadMarker,
    title: &str,
    action: Sel,
    key: &str,
    mods: NSEventModifierFlags,
) -> Retained<NSMenuItem> {
    let item = keyed_menu_item(mtm, title, action, key);
    item.setKeyEquivalentModifierMask(mods);
    item
}

/// A context menu with no AutoFill or Services items: see project_menu.
fn context_menu_new(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    menu.setAllowsContextMenuPlugIns(false);
    menu
}

/// Source Control's "more" menu and the empty part of its column: every
/// Git command, grouped the way the work goes.
fn git_actions_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let menu = context_menu_new(mtm);
    let add = |title: &str, action: Sel| menu.addItem(&menu_item(mtm, title, action));
    let line = || menu.addItem(&NSMenuItem::separatorItem(mtm));
    add("Fetch", sel!(gitFetch:));
    add("Pull", sel!(gitPull:));
    add("Pull with Rebase", sel!(gitPullRebase:));
    add("Pull with Merge", sel!(gitPullMerge:));
    add("Push", sel!(gitPush:));
    line();
    add("Stage All Changes", sel!(gitStageAll:));
    add("Unstage All Changes", sel!(gitUnstageAll:));
    line();
    add("Switch or Create Branch\u{2026}", sel!(switchBranch:));
    add("Rename Branch\u{2026}", sel!(renameBranch:));
    add("Delete Branch\u{2026}", sel!(deleteBranch:));
    add("Switch Repository\u{2026}", sel!(switchRepository:));
    line();
    add("Abort Merge, Rebase or Cherry-Pick", sel!(gitAbort:));
    add("Continue Rebase", sel!(gitContinueRebase:));
    line();
    add("Refresh", sel!(gitRefresh:));
    menu
}

/// A changed file in Source Control: open it or its diff, stage it, find
/// it. `staged` says which section the row is in.
fn git_change_menu(mtm: MainThreadMarker, staged: bool, conflicted: bool) -> Retained<NSMenu> {
    let menu = context_menu_new(mtm);
    let add = |title: &str, action: Sel| menu.addItem(&menu_item(mtm, title, action));
    let line = || menu.addItem(&NSMenuItem::separatorItem(mtm));
    add("Open File", sel!(gitOpenContextChange:));
    if !conflicted {
        add("Open Changes", sel!(gitDiffContextChange:));
        line();
        if staged {
            add("Unstage Changes", sel!(gitUnstageContextChange:));
        } else {
            add("Stage Changes", sel!(gitStageContextChange:));
        }
    }
    line();
    add("Copy Path", sel!(gitCopyContextChangePath:));
    add(
        "Copy Relative Path",
        sel!(gitCopyContextChangeRelativePath:),
    );
    add("Reveal in Finder", sel!(gitRevealContextChange:));
    menu
}

/// A section heading in Source Control.
fn git_section_menu(mtm: MainThreadMarker, staged: bool) -> Retained<NSMenu> {
    let menu = context_menu_new(mtm);
    if staged {
        menu.addItem(&menu_item(mtm, "Unstage All Changes", sel!(gitUnstageAll:)));
    } else {
        menu.addItem(&menu_item(mtm, "Stage All Changes", sel!(gitStageAll:)));
    }
    menu
}

/// The terminal's own: what a terminal does with text.
fn terminal_context_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let menu = context_menu_new(mtm);
    let cmd = NSEventModifierFlags::Command;
    menu.addItem(&hinted_menu_item(mtm, "Copy", sel!(copy:), "c", cmd));
    menu.addItem(&hinted_menu_item(mtm, "Paste", sel!(paste:), "v", cmd));
    menu.addItem(&hinted_menu_item(
        mtm,
        "Select All",
        sel!(selectAll:),
        "a",
        cmd,
    ));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&menu_item(mtm, "Clear", sel!(clearTerminal:)));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&menu_item(mtm, "New Terminal", sel!(newTerminal:)));
    menu.addItem(&hinted_menu_item(
        mtm,
        "Hide Terminal",
        sel!(toggleTerminal:),
        "`",
        NSEventModifierFlags::Control,
    ));
    menu
}

/// The empty tab strip, right of the last tab.
fn tab_strip_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let menu = context_menu_new(mtm);
    let cmd = NSEventModifierFlags::Command;
    menu.addItem(&hinted_menu_item(
        mtm,
        "New File\u{2026}",
        sel!(newDocument:),
        "n",
        cmd,
    ));
    menu.addItem(&hinted_menu_item(
        mtm,
        "Open\u{2026}",
        sel!(openDocument:),
        "o",
        cmd,
    ));
    menu.addItem(&hinted_menu_item(
        mtm,
        "Go to File\u{2026}",
        sel!(openQuickly:),
        "p",
        cmd,
    ));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&menu_item(mtm, "Close Saved Tabs", sel!(closeSavedTabs:)));
    menu
}

/// A menu item whose Command-`key` shortcut sends `action`.
fn keyed_menu_item(
    mtm: MainThreadMarker,
    title: &str,
    action: Sel,
    key: &str,
) -> Retained<NSMenuItem> {
    let item = NSMenuItem::alloc(mtm);
    unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            item,
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(key),
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

/// What is in `folder`, for a breadcrumb: folders first, then files, by
/// name. A file opens; a folder is a submenu of its own, `depth` levels
/// down, and below that shows the folder in the Explorer. Each item's tag
/// indexes `paths`.
fn crumb_folder_menu(
    mtm: MainThreadMarker,
    folder: &Path,
    current: Option<&Path>,
    depth: usize,
    paths: &mut Vec<std::path::PathBuf>,
) -> Retained<NSMenu> {
    const LIMIT: usize = 200;
    let menu = NSMenu::new(mtm);
    menu.setAllowsContextMenuPlugIns(false);
    let add = |menu: &NSMenu, title: &str, path: &Path, paths: &mut Vec<std::path::PathBuf>| {
        let item = menu_item(mtm, title, sel!(openBreadcrumbPath:));
        item.setTag(paths.len() as isize);
        paths.push(path.to_path_buf());
        menu.addItem(&item);
        item
    };
    add(&menu, "Reveal in Sidebar", folder, paths);
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    let mut entries: Vec<(bool, String, std::path::PathBuf)> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".git" || name == ".DS_Store" {
                return None;
            }
            let dir = entry.file_type().ok()?.is_dir();
            Some((dir, name, entry.path()))
        })
        .collect();
    entries.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
    });
    let total = entries.len();
    for (dir, name, path) in entries.into_iter().take(LIMIT) {
        if dir && depth > 0 {
            let holder = menu_item(mtm, &name, sel!(openBreadcrumbPath:));
            holder.setTag(paths.len() as isize);
            paths.push(path.clone());
            let sub = crumb_folder_menu(mtm, &path, current, depth - 1, paths);
            holder.setSubmenu(Some(&sub));
            menu.addItem(&holder);
        } else {
            let item = add(&menu, &name, &path, paths);
            if current == Some(path.as_path()) {
                let _: () = unsafe { msg_send![&*item, setState: 1isize] };
            }
        }
    }
    if total > LIMIT {
        let more = menu_item(
            mtm,
            &format!("{} more\u{2026}", total - LIMIT),
            sel!(openBreadcrumbPath:),
        );
        more.setTag(paths.len() as isize);
        paths.push(folder.to_path_buf());
        menu.addItem(&more);
    }
    menu
}

fn sidebar_item_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let menu = context_menu_new(mtm);
    let line = || menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&hinted_menu_item(
        mtm,
        "New File\u{2026}",
        sel!(newDocument:),
        "n",
        NSEventModifierFlags::Command,
    ));
    menu.addItem(&menu_item(mtm, "New Folder\u{2026}", sel!(newFolder:)));
    line();
    menu.addItem(&menu_item(mtm, "Copy Path", sel!(copyProjectItemPath:)));
    menu.addItem(&menu_item(
        mtm,
        "Copy Relative Path",
        sel!(copyProjectItemRelativePath:),
    ));
    menu.addItem(&menu_item(mtm, "Reveal in Finder", sel!(revealInFinder:)));
    line();
    menu.addItem(&menu_item(mtm, "Rename\u{2026}", sel!(renameProjectItem:)));
    line();
    // Apart from Rename, the one that throws something away.
    menu.addItem(&hinted_menu_item(
        mtm,
        "Move to Trash",
        sel!(trashProjectItem:),
        "\u{8}",
        NSEventModifierFlags::Command,
    ));
    menu
}

fn editor_context_menu(
    mtm: MainThreadMarker,
    commands: &[ExtCommand],
    lsp: bool,
) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    let cmd = NSEventModifierFlags::Command;
    let line = || menu.addItem(&NSMenuItem::separatorItem(mtm));
    if lsp {
        // What a language server knows about the code under the pointer.
        menu.addItem(&hinted_menu_item(
            mtm,
            "Go to Definition",
            sel!(goToDefinition:),
            "\u{F70F}",
            NSEventModifierFlags::empty(),
        ));
        menu.addItem(&menu_item(mtm, "Find References", sel!(findReferences:)));
        menu.addItem(&menu_item(
            mtm,
            "Rename Symbol\u{2026}",
            sel!(renameSymbol:),
        ));
        menu.addItem(&menu_item(mtm, "Quick Fix\u{2026}", sel!(quickFix:)));
        line();
    }
    menu.addItem(&hinted_menu_item(mtm, "Cut", sel!(cut:), "x", cmd));
    menu.addItem(&hinted_menu_item(mtm, "Copy", sel!(copy:), "c", cmd));
    menu.addItem(&hinted_menu_item(mtm, "Paste", sel!(paste:), "v", cmd));
    line();
    menu.addItem(&hinted_menu_item(
        mtm,
        "Select All",
        sel!(selectAll:),
        "a",
        cmd,
    ));
    menu.addItem(&hinted_menu_item(
        mtm,
        "Toggle Comment",
        sel!(toggleComment:),
        "/",
        cmd,
    ));
    line();
    menu.addItem(&hinted_menu_item(
        mtm,
        "Find\u{2026}",
        sel!(performFindPanelAction:),
        "f",
        cmd,
    ));
    if commands.is_empty() {
        return menu;
    }
    line();
    // Extensions, right where the text is: each installed extension, then
    // its commands, run on the selection (or the document) under the
    // pointer. Two clicks rather than the palette and a typed name.
    let extensions = NSMenu::new(mtm);
    extensions.setTitle(&NSString::from_str("Extensions"));
    for item in extension_items(mtm, commands) {
        extensions.addItem(&item);
    }
    extensions.addItem(&NSMenuItem::separatorItem(mtm));
    extensions.addItem(&menu_item(mtm, "Manage Extensions…", sel!(openExtensions:)));
    let holder = menu_item(mtm, "Extensions", sel!(openExtensions:));
    holder.setSubmenu(Some(&extensions));
    menu.addItem(&holder);
    menu
}

/// One item per enabled extension, named for it, holding its commands; each
/// command's tag is its index in `commands`, which `runExtensionCommand:`
/// runs. The menu bar's Extensions menu and the editor's context menu both
/// list them this way.
fn extension_items(mtm: MainThreadMarker, commands: &[ExtCommand]) -> Vec<Retained<NSMenuItem>> {
    let mut items: Vec<(String, Retained<NSMenu>)> = Vec::new();
    for (tag, command) in commands.iter().enumerate() {
        let id = &command.installed.manifest.id;
        let at = match items.iter().position(|(seen, _)| seen == id) {
            Some(at) => at,
            None => {
                let submenu = NSMenu::new(mtm);
                submenu.setTitle(&NSString::from_str(&command.installed.manifest.name));
                items.push((id.clone(), submenu));
                items.len() - 1
            }
        };
        let item = menu_item(mtm, &command.title, sel!(runExtensionCommand:));
        item.setTag(tag as isize);
        items[at].1.addItem(&item);
    }
    items
        .into_iter()
        .map(|(_, submenu)| {
            let holder = NSMenuItem::new(mtm);
            holder.setTitle(&submenu.title());
            holder.setSubmenu(Some(&submenu));
            holder
        })
        .collect()
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
        let item = keyed_menu_item(mtm, title, action, key);
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
        "New Workspace\u{2026}",
        sel!(newWorkspace:),
        "",
        false,
    ));
    file_menu.addItem(&item(
        "Open Quickly\u{2026}",
        sel!(openQuickly:),
        "p",
        false,
    ));
    file_menu.addItem(&item(
        "Command Palette\u{2026}",
        sel!(showCommands:),
        "P",
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
    view_menu.addItem(&item(
        "Markdown Preview Only",
        sel!(togglePreviewOnly:),
        "E",
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
    let mcp_servers = item("MCP Servers", sel!(showMcp:), "", false);
    source_control
        .setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Option);
    git_menu.addItem(&source_control);
    git_menu.addItem(&mcp_servers);
    git_menu.addItem(&item("Refresh", sel!(gitRefresh:), "", false));
    git_menu.addItem(&item("Commit\u{2026}", sel!(gitCommit:), "", false));
    git_menu.addItem(&NSMenuItem::separatorItem(mtm));
    git_menu.addItem(&item(
        "Switch Repository\u{2026}",
        sel!(switchRepository:),
        "",
        false,
    ));
    git_menu.addItem(&item(
        "Switch Branch\u{2026}",
        sel!(switchBranch:),
        "",
        false,
    ));
    git_menu.addItem(&item(
        "Rename Branch\u{2026}",
        sel!(renameBranch:),
        "",
        false,
    ));
    git_menu.addItem(&item(
        "Delete Branch\u{2026}",
        sel!(deleteBranch:),
        "",
        false,
    ));
    git_menu.addItem(&item("Fetch", sel!(gitFetch:), "", false));
    git_menu.addItem(&item("Pull", sel!(gitPull:), "", false));
    git_menu.addItem(&item("Pull with Rebase", sel!(gitPullRebase:), "", false));
    git_menu.addItem(&item("Pull with Merge", sel!(gitPullMerge:), "", false));
    git_menu.addItem(&item("Push", sel!(gitPush:), "", false));
    git_menu.addItem(&NSMenuItem::separatorItem(mtm));
    git_menu.addItem(&item("Abort Merge or Rebase", sel!(gitAbort:), "", false));
    git_menu.addItem(&item(
        "Continue Rebase",
        sel!(gitContinueRebase:),
        "",
        false,
    ));
    git_menu.addItem(&NSMenuItem::separatorItem(mtm));
    git_menu.addItem(&item("Next Conflict", sel!(nextConflict:), "", false));
    git_menu.addItem(&item(
        "Previous Conflict",
        sel!(previousConflict:),
        "",
        false,
    ));
    git_menu.addItem(&item("Accept Current", sel!(acceptCurrent:), "", false));
    git_menu.addItem(&item("Accept Incoming", sel!(acceptIncoming:), "", false));
    git_menu.addItem(&item("Accept Both", sel!(acceptBoth:), "", false));
    git_menu.addItem(&item("Accept Base", sel!(acceptBase:), "", false));
    git_menu.addItem(&item(
        "Conflicts Side by Side",
        sel!(toggleConflictColumns:),
        "",
        false,
    ));
    git_menu.addItem(&item("Mark Resolved", sel!(markResolved:), "", false));
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
    go_menu.addItem(&item("Quick Fix\u{2026}", sel!(quickFix:), ".", false));
    let organize = item("Organize Imports", sel!(organizeImports:), "o", false);
    organize
        .setKeyEquivalentModifierMask(NSEventModifierFlags::Shift | NSEventModifierFlags::Option);
    go_menu.addItem(&organize);
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

    // Filled by `rebuild_extension_menu` with each enabled extension's
    // commands, after the first two items.
    let (extensions_item, extensions_menu) = submenu("Extensions");
    extensions_menu.addItem(&item(
        "Extensions\u{2026}",
        sel!(openExtensions:),
        "x",
        true,
    ));
    extensions_menu.addItem(&NSMenuItem::separatorItem(mtm));
    extensions_item.setSubmenu(Some(&extensions_menu));
    menubar.addItem(&extensions_item);

    let (run_item, run_menu) = submenu("Run");
    // Cmd-Return. Greyed out unless the active document is a `.http` file,
    // so it never eats the key anywhere else.
    run_menu.addItem(&item("Send Request", sel!(sendRequest:), "\r", false));
    run_menu.addItem(&item("Run MCP Call", sel!(runMcpCall:), "", false));
    run_menu.addItem(&item(
        "Copy crc MCP Server Command",
        sel!(copyMcpServerCommand:),
        "",
        false,
    ));
    run_menu.addItem(&item(
        "Save MCP Call to Workspace",
        sel!(saveMcpCall:),
        "",
        false,
    ));
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
    // A test instance stays out of the way of the person at the machine:
    // no Dock icon, never activated, its window see-through and deaf to the
    // pointer. The script drives the view directly, so none of that matters
    // to it. CRC_SELFTEST_VISIBLE shows it, for capturing the live window.
    // Prohibited, not Accessory: macOS activates an accessory binary started
    // from a terminal, so every launch took the keyboard from whatever the
    // person was typing in. Its window still orders front and draws.
    let hidden_test = std::env::var_os("CRC_SELFTEST").is_some()
        && std::env::var_os("CRC_SELFTEST_VISIBLE").is_none();
    app.setActivationPolicy(if hidden_test {
        NSApplicationActivationPolicy::Prohibited
    } else {
        NSApplicationActivationPolicy::Regular
    });
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
                    docs.restore(Buffer::recovered(
                        document.path,
                        &document.text,
                        document.stamp,
                    ));
                }
                // Back under the hook's protection before the files go.
                recovery::publish(docs.iter());
            }
            found.clear();
        }
    }

    let workspace = match tree.root() {
        Some(root) => crate::project::workspace::Workspace::open(root),
        None => crate::project::workspace::Workspace::unopened(
            &std::env::current_dir().unwrap_or_default(),
        ),
    };
    let git = git_panel_for(&workspace);
    let home_since = tree.root().and_then(visit_workspace);
    let home_rx = tree
        .root()
        .map(|_| spawn_home_summary(workspace.clone(), home_since));
    let recent_projects = with_recent(session.recent.clone(), tree.root());
    // Read once: each field below used to parse config.toml again.
    let settings = crate::platform::settings::Settings::load();
    let state = State {
        docs,
        native_preview: None,
        html_preview: None,
        preview_share: 0.5,
        git,
        git_open: false,
        diff_tab: None,
        git_focus: false,
        hovered_tab: None,
        tree,
        tree_version: 0,
        find: None,
        find_cache: None,
        sidebar,
        sidebar_width: session.sidebar_width,
        drag: None,
        autoscroll_carry: 0.0,
        marked: None,
        marked_caret: 0,
        selftest: std::collections::VecDeque::new(),
        project_menu_requested: false,
        crumb_menu_requested: None,
        crumb_paths: Vec::new(),
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
        context_tab: None,
        context_change: None,
        context_ext: None,
        context_mcp: None,
        ephemeral_session: launched_with_file,
        discard_confirmed: false,
        quit_session: None,
        home_hits: Vec::new(),
        recent_projects,
        syntax: SyntaxStore::new(),
        spans: Vec::new(),
        finder,
        project_search: ProjectSearch {
            rx: None,
            references: false,
            cancel: None,
        },
        http: None,
        responses: HashMap::new(),
        claude: None,
        claude_tried: None,
        sidebar_keys: false,
        terminal: crate::platform::terminal::Panel::default(),
        sidebar_edit: None,
        rename: None,
        format_on_save: settings.format_on_save,
        saving_formatted: false,
        word_wrap: settings.word_wrap,
        ssh_auth_sock: settings.ssh_auth_sock.clone(),
        branch_list: None,
        repo_list: None,
        workspace,
        home: None,
        home_rx,
        home_since,
        mcp: Default::default(),
        mcp_calls: HashMap::new(),
        branch_rx: None,
        organize_on_save: settings.organize_imports_on_save,
        extensions: None,
        conflict_scans: HashMap::new(),
        conflict_side: settings.conflict_side_by_side,
        conflict_cursor_key: None,
        pointer_targets: Vec::new(),
        hotspots: Vec::new(),
        hot: None,
        status_detail_x: None,
        font: font.to_owned(),
        font_size: size_pt,
        theme_choice: settings.theme,
        caret_blink: settings.caret_blink,
        caret_since: Instant::now(),
        unshaped_on_screen: false,
        completer: None,
        completion_generation: 0,
        completion_chips: Vec::new(),
        update: None,
        reloads: Reloads {
            channel: mpsc::channel(),
            pending: HashSet::new(),
        },
        completion: None,
        panes: Vec::new(),
        focused_pane: 0,
        palette: None,
        commands: Vec::new(),
        symbols: symbols::Symbols::default(),
        palette_scroll: 0,
        palette_count: 0,
        palette_scroll_carry: 0.0,
        goto: None,
        last_draw: None,
        pending_input: None,
        title_sync_pending: false,
        frame_interval: Duration::from_secs_f64(1.0 / 120.0),
        gutter: Gutter {
            docs: HashMap::new(),
            dirty: HashMap::new(),
            pending: HashSet::new(),
            heads: mpsc::channel(),
            computed: mpsc::channel(),
            diffing: HashSet::new(),
        },
        blame: Blame {
            shown: None,
            want: None,
            rx: None,
        },
        lsp: Lsp {
            servers: HashMap::new(),
            unavailable: HashMap::new(),
            restarts: HashMap::new(),
            dirty: HashMap::new(),
            signature: None,
            formatting: None,
            action_list: None,
            quick_fix_request: None,
            organizing: None,
            resolving: None,
            bulb: None,
            bulb_want: None,
            bulb_request: None,
            bulb_rect: None,
        },
        ext: Ext {
            registry_rx: None,
            install_rx: None,
            worker: None,
            preview_worker: None,
            failures: HashMap::new(),
            commands: Vec::new(),
            pending: HashMap::new(),
            next_job: 1,
            generation: 0,
            logs: HashMap::new(),
        },
        watch: Watch {
            watcher: None,
            indexer: None,
            tree_changed_at: None,
            git_changed_at: None,
            ignored_rx: None,
            index_rx: project_index_rx,
            children_tx: tree_children_tx,
            children_rx: tree_children_rx,
            children_pending: HashSet::new(),
        },
    };

    let content = NSRect::new(NSPoint::new(0.0, 0.0), frame.size);
    let view = EditorView::new(mtm, state, content);
    view.watch_project();
    view.apply_theme();
    view.lsp_sync_open();
    view.rebuild_extension_menu();
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
        // A hidden one still has to be on screen, or nothing draws. It is
        // see-through, deaf to the pointer, and joins whatever Space is
        // showing, full-screen ones too, so macOS never switches Spaces to
        // show it.
        if hidden_test {
            window.setAlphaValue(0.0);
            window.setIgnoresMouseEvents(true);
            window.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::FullScreenAuxiliary
                    | NSWindowCollectionBehavior::Stationary
                    | NSWindowCollectionBehavior::IgnoresCycle,
            );
        }
        window.orderFront(None);
    } else {
        window.makeKeyAndOrderFront(None);
    }
    if view.ivars().state.borrow().watch.index_rx.is_some()
        || view.ivars().state.borrow().git.busy()
    {
        view.resume_display_link();
    }

    if !testing && settings.update_check && crate::platform::update::launch_check_due() {
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
    use super::{
        caret_phase, inserted_text, rename_field, single_line, spawn_project_index,
        spawn_project_refresh, typed_text,
    };
    use std::time::Duration;

    #[test]
    fn one_line_fields_share_the_editing_keys() {
        use super::{field_key, key};
        use crate::text::buffer::Buffer;
        use objc2_app_kit::NSEventModifierFlags as F;
        let mut field = Buffer::from_text("alpha beta gamma");
        assert!(field_key(&mut field, key::END, F::empty()));
        assert_eq!(field.cursor(), 16);
        assert!(field_key(&mut field, key::LEFT, F::Option));
        assert_eq!(field.cursor(), 11, "a word back");
        assert!(field_key(&mut field, key::FORWARD_DELETE, F::Option));
        assert_eq!(field.rope.to_string(), "alpha beta ");
        assert!(field_key(&mut field, key::HOME, F::Shift));
        assert_eq!(field.selected_text().as_deref(), Some("alpha beta "));
        assert!(
            !field_key(&mut field, key::RETURN, F::empty()),
            "not an editing key"
        );
    }

    #[test]
    fn rename_selects_the_stem_in_bytes() {
        let field = rename_field("caf\u{e9}.txt", false);
        assert_eq!(field.selected_text().as_deref(), Some("caf\u{e9}"));
        let field = rename_field("notes.d", true);
        assert_eq!(field.selected_text().as_deref(), Some("notes.d"));
        let field = rename_field(".profile", false);
        assert_eq!(field.selected_text().as_deref(), Some(".profile"));
    }

    #[test]
    fn the_restore_prompt_names_what_came_back() {
        use crate::platform::recovery::Recovered;
        let one = [Recovered {
            path: Some("/work/garden-log/src/main.rs".into()),
            text: String::new(),
            stamp: None,
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
                stamp: None,
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
    fn home_and_its_big_folders_are_never_adopted() {
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        assert!(super::too_broad_to_adopt(&home));
        assert!(super::too_broad_to_adopt(std::path::Path::new("/")));
        assert!(super::too_broad_to_adopt(&home.join("Downloads")));
        let project = std::env::temp_dir().join(format!("caio-adopt-{}", std::process::id()));
        std::fs::create_dir_all(&project).unwrap();
        assert!(!super::too_broad_to_adopt(&project));
        std::fs::remove_dir_all(project).unwrap();
    }

    #[test]
    fn dictated_line_breaks_are_text() {
        assert_eq!(inserted_text("one\ntwo\tthree"), "one\ntwo\tthree");
        assert_eq!(inserted_text("\r"), "", "a lone Return is a key");
        assert_eq!(inserted_text("\u{3}"), "");
        assert_eq!(inserted_text("a\u{1b}b"), "ab");
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
