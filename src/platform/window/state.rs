//! The parts of `State` that carry background work: each group owns its
//! fields and says when nothing of it is in flight, so the display link
//! can pause.

use super::*;

/// Git marks in the gutter, per open document.
pub(super) struct Gutter {
    /// Git marks per open buffer, keyed by buffer id.
    pub(super) docs: HashMap<u64, GutterState>,
    /// Buffers whose marks are stale, and since when. Recomputed after a
    /// pause, like the language server sync.
    pub(super) dirty: HashMap<u64, Instant>,
    /// Buffers whose HEAD text is being fetched on a worker.
    pub(super) pending: HashSet<u64>,
    pub(super) heads: (mpsc::Sender<HeadText>, mpsc::Receiver<HeadText>),
    /// Gutter marks computed on a worker: document, the text they are for,
    /// the marks.
    pub(super) computed: (mpsc::Sender<GutterMarks>, mpsc::Receiver<GutterMarks>),
    pub(super) diffing: HashSet<u64>,
}

/// Blame for the caret's line.
pub(super) struct Blame {
    /// Who last changed the caret's line: document, line, and what the
    /// status line says.
    pub(super) shown: Option<(u64, usize, String)>,
    /// The caret's document and line, and since when, so blame is asked
    /// once the caret rests.
    pub(super) want: Option<(u64, usize, Instant)>,
    pub(super) rx: Option<mpsc::Receiver<(u64, usize, String)>>,
}

/// Language servers for the current root, and requests in flight.
pub(super) struct Lsp {
    /// Language servers by their shared language key, for the current root.
    pub(super) servers: HashMap<Language, crate::lsp::client::Server>,
    /// Languages whose server could not be started, and why. Said once.
    pub(super) unavailable: HashMap<Language, String>,
    /// How often each language's server has stopped this session: it is
    /// started again a few times, then left alone.
    pub(super) restarts: HashMap<Language, u32>,
    /// Buffers edited since their server last heard, and when. Changes are
    /// sent once typing pauses.
    pub(super) dirty: HashMap<u64, Instant>,
    /// The signature of the call the caret is in, while the server has one.
    pub(super) signature: Option<SignatureTip>,
    /// A format in flight: the file and its text when asked, so a reply
    /// that no longer fits the text is dropped rather than applied.
    pub(super) formatting: Option<(std::path::PathBuf, crate::text::rope::Rope)>,
    /// The palette is picking a code action: the server that offered them
    /// and the actions. `None` in every other mode.
    pub(super) action_list: Option<(Language, Vec<crate::lsp::CodeAction>)>,
    /// The Quick Fix request the palette waits on.
    pub(super) quick_fix_request: Option<u64>,
    /// Organize Imports in flight.
    pub(super) organizing: Option<Organizing>,
    /// A code action being resolved before it runs; `true` when a save ran
    /// it, and goes on once it has.
    pub(super) resolving: Option<bool>,
    /// The code actions at the caret, asked for when it rested there.
    pub(super) bulb: Option<Bulb>,
    /// The caret's document and offset, and since when, so the bulb is
    /// asked for once the caret rests.
    pub(super) bulb_want: Option<(u64, usize, Instant)>,
    /// The bulb request in flight: request, document and caret.
    pub(super) bulb_request: Option<(u64, u64, usize)>,
    /// Where the bulb was last drawn.
    pub(super) bulb_rect: Option<Viewport>,
}

/// Extension threads, commands and calls.
pub(super) struct Ext {
    /// The registry list being fetched, and a download being installed.
    pub(super) registry_rx: Option<mpsc::Receiver<Result<crate::ext::registry::Index, String>>>,
    pub(super) install_rx: Option<mpsc::Receiver<Result<crate::ext::store::Package, String>>>,
    /// The extension thread, started on the first command.
    pub(super) worker: Option<ExtWorker>,
    /// Previews run on their own thread: a slow page must not hold up a
    /// command someone asked for.
    pub(super) preview_worker: Option<ExtWorker>,
    /// Failures in a row by extension id. Three turn it off.
    pub(super) failures: HashMap<String, u32>,
    /// Extension commands by menu tag.
    pub(super) commands: Vec<ExtCommand>,
    /// Calls in flight, by job tag.
    pub(super) pending: HashMap<u64, ExtCall>,
    pub(super) next_job: u64,
    /// Bumped whenever what is installed changes, so the extension thread
    /// drops instances of what was replaced.
    pub(super) generation: u64,
    /// Recent log lines per extension.
    pub(super) logs: HashMap<String, Vec<String>>,
}

/// The project watcher, the index and tree reads in flight.
pub(super) struct Watch {
    /// FSEvents on the project root. Dropped and remade when the root moves.
    pub(super) watcher: Option<crate::project::watch::Watcher>,
    /// Keeps the project index current; one per open project.
    pub(super) indexer: Option<crate::index::store::Indexer>,
    /// When the watcher last reported a tree change that has not been
    /// acted on. Refreshes are debounced against it, and a change that
    /// arrives while a refresh is running is kept for the next one.
    pub(super) tree_changed_at: Option<Instant>,
    /// The same for `.git` bookkeeping.
    pub(super) git_changed_at: Option<Instant>,
    /// Git's ignored paths being read for this root.
    pub(super) ignored_rx: Option<
        mpsc::Receiver<(
            std::path::PathBuf,
            std::collections::HashSet<std::path::PathBuf>,
        )>,
    >,
    pub(super) index_rx: Option<mpsc::Receiver<ProjectIndexResult>>,
    pub(super) children_tx: mpsc::Sender<(std::path::PathBuf, std::path::PathBuf, Vec<TreeEntry>)>,
    pub(super) children_rx:
        mpsc::Receiver<(std::path::PathBuf, std::path::PathBuf, Vec<TreeEntry>)>,
    pub(super) children_pending: HashSet<std::path::PathBuf>,
}
/// Large files changed on disk, read on a worker before they reload.
pub(super) struct Reloads {
    pub(super) channel: (mpsc::Sender<ReloadRead>, mpsc::Receiver<ReloadRead>),
    /// The documents being read, by buffer id.
    pub(super) pending: HashSet<u64>,
}

/// A project search or Find References running on a worker.
pub(super) struct ProjectSearch {
    pub(super) rx: Option<mpsc::Receiver<Result<Vec<ProjectHit>, String>>>,
    /// The search running is Find References, not a text search.
    pub(super) references: bool,
    pub(super) cancel: Option<Arc<AtomicBool>>,
}

impl Gutter {
    /// No document waits for a pause, a HEAD read or a diff.
    pub(super) fn idle(&self) -> bool {
        self.dirty.is_empty() && self.pending.is_empty() && self.diffing.is_empty()
    }
}

impl Blame {
    pub(super) fn idle(&self) -> bool {
        self.want.is_none() && self.rx.is_none()
    }
}

impl Lsp {
    /// No change waits to be sent and no bulb waits to be asked for.
    pub(super) fn idle(&self) -> bool {
        self.dirty.is_empty() && self.bulb_want.is_none()
    }
}

impl Ext {
    pub(super) fn idle(&self) -> bool {
        self.registry_rx.is_none() && self.install_rx.is_none() && self.pending.is_empty()
    }
}

impl Watch {
    pub(super) fn idle(&self) -> bool {
        self.tree_changed_at.is_none()
            && self.git_changed_at.is_none()
            && self.ignored_rx.is_none()
            && self.index_rx.is_none()
            && self.children_pending.is_empty()
    }
}

impl State {
    /// Nothing is in flight that the display link has to poll for. A job
    /// left out of here pauses the link while its reply waits.
    pub(super) fn idle(&self) -> bool {
        self.gutter.idle()
            && self.blame.idle()
            && self.lsp.idle()
            && self.ext.idle()
            && self.watch.idle()
            && self.message.is_none()
            && !self.git.busy()
            && !matches!(self.drag, Some(Drag::Select { point: Some(_), .. }))
            && self.project_search.rx.is_none()
            && self.http.is_none()
            && self.update.is_none()
            && self.update_install.is_none()
            && self.branch_rx.is_none()
            && self.home_rx.is_none()
            && !self.mcp.busy()
            && self.reloads.pending.is_empty()
            && !self.symbols.pending()
            && self
                .claude
                .as_ref()
                .is_none_or(|c| c.selection_changed_at.is_none())
            && self.html_preview.as_ref().is_none_or(|p| !p.busy())
    }
}

impl State {
    /// Says `text` in the status line as `kind`: a failure shows red and
    /// stays longer whatever its wording, a success green.
    pub(super) fn say(&mut self, kind: layout::Feedback, text: impl Into<String>) {
        let text = text.into();
        self.message_kind = Some((text.clone(), kind));
        self.message = Some((text, Instant::now()));
    }
}
