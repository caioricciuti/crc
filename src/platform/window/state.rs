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
    /// Buffers whose HEAD may have moved since it was read: their marks
    /// stay drawn until the new HEAD text and its diff replace them.
    pub(super) stale: HashSet<u64>,
    /// Counts the times HEAD may have moved, so a read started before the
    /// last one is not taken for the new HEAD.
    pub(super) epoch: u64,
    pub(super) heads: (mpsc::Sender<HeadText>, mpsc::Receiver<HeadText>),
    /// Changes against HEAD computed on a worker: document, the text they
    /// are for, the changes.
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

/// Blame as heat in the gutter, per open document, while it is on.
pub(super) struct Heat {
    /// `blame_heat` from the settings, or as Git > Blame Heat last left it.
    pub(super) on: bool,
    /// The blame drawn for each document, by buffer id.
    pub(super) docs: HashMap<u64, HeatDoc>,
    /// Documents edited since their blame, and when: blamed again once
    /// typing has stopped for a moment.
    pub(super) dirty: HashMap<u64, Instant>,
    /// Documents being blamed on a worker.
    pub(super) pending: HashSet<u64>,
    /// Bumped when it is turned off, so a blame started before is dropped.
    pub(super) generation: u64,
    pub(super) channel: (mpsc::Sender<HeatBlame>, mpsc::Receiver<HeatBlame>),
    /// Git's `%G?` for each signed commit whose tooltip has been shown,
    /// by hash: checked once, on a worker, as it runs the signature's
    /// checker.
    pub(super) checks: HashMap<String, char>,
    /// Signed commits being checked.
    pub(super) checking: HashSet<String>,
    pub(super) checked: (mpsc::Sender<HeatCheck>, mpsc::Receiver<HeatCheck>),
}

/// A signature check that landed: the commit, and Git's `%G?` letter, or
/// `None` when Git could not say.
pub(super) type HeatCheck = (String, Option<char>);

/// A blame worked out on a worker: document, generation, the gutter's
/// epoch and the text it was asked for, and Git's answer.
pub(super) type HeatBlame = (
    u64,
    u64,
    u64,
    crate::text::rope::Rope,
    Result<crate::project::git::BlameMap, String>,
);

/// One document's blame, as the strip draws it.
pub(super) struct HeatDoc {
    pub(super) map: Arc<crate::project::git::BlameMap>,
    /// The gutter's epoch when it was asked: an older one means HEAD may
    /// have moved since.
    pub(super) epoch: u64,
    /// The text it was blamed for.
    pub(super) text: crate::text::rope::Rope,
    /// Per commit, how strongly it shows, what its tooltip says and
    /// whether it is dashed as unsigned, worked out once rather than
    /// every frame.
    pub(super) alpha: Vec<f32>,
    pub(super) tips: Vec<String>,
    pub(super) dashed: Vec<bool>,
    /// The text `hide_from` was worked out for.
    pub(super) seen: crate::text::rope::Rope,
    /// The first line edited since the blame: from it down, lines may
    /// have moved, and nothing is drawn until the blame is run again.
    pub(super) hide_from: usize,
}

impl HeatDoc {
    pub(super) fn new(
        map: crate::project::git::BlameMap,
        epoch: u64,
        text: crate::text::rope::Rope,
        now: i64,
        checks: &HashMap<String, char>,
    ) -> HeatDoc {
        let alpha = map
            .commits
            .iter()
            .map(|c| layout::heat(now - c.time))
            .collect();
        let dashed = map
            .commits
            .iter()
            .map(|c| c.signed == Some(false))
            .collect();
        let mut doc = HeatDoc {
            map: Arc::new(map),
            epoch,
            seen: text.clone(),
            text,
            alpha,
            tips: Vec::new(),
            dashed,
            hide_from: usize::MAX,
        };
        doc.retip(now, checks);
        doc
    }

    /// Works the tooltips out again, with what `checks` now says of the
    /// signatures.
    pub(super) fn retip(&mut self, now: i64, checks: &HashMap<String, char>) {
        self.tips = self
            .map
            .commits
            .iter()
            .map(|c| heat_tip(c, now, checks.get(&c.sha).copied()))
            .collect();
    }

    /// Catches up with `rope`, the document's text now: `true` when it
    /// differs from the text that was blamed.
    pub(super) fn follow(&mut self, rope: &crate::text::rope::Rope) -> bool {
        if !self.seen.same_as(rope) {
            self.hide_from = first_changed_line(&self.text, rope).unwrap_or(usize::MAX);
            self.seen = rope.clone();
        }
        self.hide_from != usize::MAX
    }

    /// What the strip draws for this document.
    pub(super) fn strip(&self) -> layout::BlameHeat<'_> {
        layout::BlameHeat {
            lines: &self.map.lines,
            alpha: &self.alpha,
            tips: &self.tips,
            dashed: &self.dashed,
            hide_from: self.hide_from,
        }
    }
}

/// What the strip says of commit `c`: who, when and what, its signature
/// as far as `check` (Git's `%G?`, once asked) knows, and where a click
/// takes it.
pub(super) fn heat_tip(
    c: &crate::project::git::CommitInfo,
    now: i64,
    check: Option<char>,
) -> String {
    let mut tip = format!(
        "{}  {}  {}\n{}",
        c.sha.get(..7).unwrap_or(&c.sha),
        c.author,
        crate::project::git::ago(c.time, now),
        c.summary
    );
    if let Some(note) = crate::project::git::signature_note(c.signed, check) {
        tip.push('\n');
        tip.push_str(note);
    }
    tip.push_str("\nCmd-click: open in File History");
    tip
}

/// The first line where `b` differs from `a`, or `None` for the same text.
pub(super) fn first_changed_line(
    a: &crate::text::rope::Rope,
    b: &crate::text::rope::Rope,
) -> Option<usize> {
    a.first_difference(b).map(|at| b.byte_to_line(at))
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

    /// After Git changed something: every document's HEAD is read again
    /// when it is next shown, and what it has stays drawn until then.
    pub(super) fn mark_stale(&mut self) {
        self.epoch += 1;
        self.stale.extend(self.docs.keys().copied());
    }

    /// Takes the HEAD text read for `id`, the read started at `epoch`;
    /// `true` when the document wants diffing against it. A read from
    /// before HEAD last moved is dropped, and the next one asked for. A
    /// HEAD that changed keeps the old marks drawn until its diff lands,
    /// but drops the changes, which the Git commands act on.
    pub(super) fn land_head(&mut self, id: u64, epoch: u64, head: Option<String>) -> bool {
        if epoch != self.epoch && self.stale.contains(&id) {
            return false;
        }
        self.stale.remove(&id);
        if let (Some(entry), Some(head)) = (self.docs.get_mut(&id), head.as_ref()) {
            if entry.head.as_deref() == Some(head) {
                return false;
            }
            entry.head = Some(Arc::new(head.clone()));
            entry.changes.clear();
            entry.text = None;
            return true;
        }
        let has_head = head.is_some();
        self.docs.insert(
            id,
            GutterState {
                head: head.map(Arc::new),
                marks: Vec::new(),
                changes: Vec::new(),
                text: None,
            },
        );
        has_head
    }
}

impl Blame {
    pub(super) fn idle(&self) -> bool {
        self.want.is_none() && self.rx.is_none()
    }
}

impl Heat {
    /// No document waits for typing to stop or for its blame, and no
    /// signature is being checked.
    pub(super) fn idle(&self) -> bool {
        self.dirty.is_empty() && self.pending.is_empty() && self.checking.is_empty()
    }

    /// The document and the signed commit the strip's tooltip `tip` is
    /// for, when that commit's signature has not been checked yet: it is
    /// marked as being checked. `None` for an unsigned commit, one checked
    /// or being checked, or a tip that is not the strip's.
    pub(super) fn check_for_tip(&mut self, tip: &str) -> Option<(u64, String)> {
        let (id, sha) = self.docs.iter().find_map(|(&id, doc)| {
            let at = doc.tips.iter().position(|t| t == tip)?;
            let commit = doc.map.commits.get(at)?;
            (commit.signed == Some(true)).then(|| (id, commit.sha.clone()))
        })?;
        if self.checks.contains_key(&sha) || !self.checking.insert(sha.clone()) {
            return None;
        }
        Some((id, sha))
    }

    /// Takes a signature check that landed: every strip showing the commit
    /// says what it found. A check Git could not make counts as one that
    /// cannot be made here, and is not asked again.
    pub(super) fn land_check(&mut self, sha: String, check: Option<char>, now: i64) {
        self.checking.remove(&sha);
        self.checks.insert(sha.clone(), check.unwrap_or('?'));
        for doc in self.docs.values_mut() {
            if doc.map.commits.iter().any(|c| c.sha == sha) {
                doc.retip(now, &self.checks);
            }
        }
    }

    /// The blame to draw for `id`, caught up with `rope`, its text now.
    /// Typed text is drawn before the display link looks, so the frame
    /// does this itself: an edit since the blame hides the strip from the
    /// edited line down at once, and has it asked again once typing stops.
    pub(super) fn follow(&mut self, id: u64, rope: &crate::text::rope::Rope) -> Option<&HeatDoc> {
        let doc = self.docs.get_mut(&id)?;
        if !doc.seen.same_as(rope) {
            if doc.follow(rope) {
                self.dirty.insert(id, Instant::now());
            } else {
                self.dirty.remove(&id);
            }
        }
        Some(doc)
    }

    /// Off: every blame dropped, and those still running dropped when
    /// they land.
    pub(super) fn turn_off(&mut self) {
        self.on = false;
        self.generation += 1;
        self.docs.clear();
        self.dirty.clear();
        self.pending.clear();
    }

    /// Takes a blame that landed for `id`, asked at `generation` and the
    /// gutter's `epoch` (the gutter is at `current` now) for `text`, when
    /// the document has `rope`. A blame from before it was last turned
    /// off, before HEAD may have moved, or for text since edited is
    /// dropped: `false`, and the document is asked again.
    pub(super) fn land(
        &mut self,
        (id, generation, epoch, text, map): (
            u64,
            u64,
            u64,
            crate::text::rope::Rope,
            crate::project::git::BlameMap,
        ),
        current: u64,
        rope: Option<&crate::text::rope::Rope>,
        now: i64,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        self.pending.remove(&id);
        let Some(rope) = rope else {
            return false;
        };
        if !self.on || epoch != current {
            return false;
        }
        if !rope.same_text(&text) {
            self.dirty.insert(id, Instant::now());
            return false;
        }
        let mut doc = HeatDoc::new(map, epoch, text, now, &self.checks);
        doc.follow(rope);
        self.docs.insert(id, doc);
        true
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
            && !self.syntax.busy()
            && self.blame.idle()
            && self.heat.idle()
            && self.lsp.idle()
            && self.ext.idle()
            && self.watch.idle()
            && self.message.is_none()
            && !self.git.busy()
            && !matches!(self.drag, Some(Drag::Select { point: Some(_), .. }))
            && !(self.terminal.selecting && self.terminal.drag_point.is_some())
            && self.project_search.rx.is_none()
            && self.term_search.as_ref().is_none_or(|s| !s.busy())
            && self.http.is_none()
            && self.query_job.is_none()
            && self.update.is_none()
            && self.update_install.is_none()
            && self.review_scan.is_none()
            && self.review.counting.is_none()
            && self.branch_rx.is_none()
            && self.history_rx.is_none()
            && self.home_rx.is_none()
            && !self.mcp.busy()
            && !self.agents.busy()
            && self.reloads.pending.is_empty()
            && !self.symbols.pending()
            && self.scrub.as_ref().is_none_or(|s| !s.busy())
            && self
                .claude
                .as_ref()
                .is_none_or(|c| c.selection_changed_at.is_none())
            && self.html_preview.as_ref().is_none_or(|p| !p.busy())
    }
}

/// The extension that renders Markdown, as the registry names it.
pub(super) const MARKDOWN_PREVIEW_ID: &str = "crc.markdown-preview";

/// The one thing a status-line message asks for, as a button beside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum MessageAction {
    /// The Extensions page with this extension selected.
    OpenExtension(&'static str),
    /// Quit the way Cmd-Q does and open the app on disk again.
    Restart,
}

impl MessageAction {
    /// The button's label.
    pub(super) fn label(&self) -> &'static str {
        match self {
            MessageAction::OpenExtension(_) => "Open Extensions",
            MessageAction::Restart => "Restart",
        }
    }
}

/// Which mode a status-line message is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    Editor,
    Agents,
    /// Both: news about the app itself, like the terminals started again
    /// at launch.
    Both,
}

/// What whoever set the status-line message said about it beyond its
/// text. Paired with the text: a message set directly leaves an older
/// `Said` behind, which then applies to nothing.
#[derive(Clone, Debug)]
pub(super) struct Said {
    pub(super) text: String,
    pub(super) kind: layout::Feedback,
    /// The whole of what the text shortens (a file's path), for the
    /// pointer's rest over the message.
    pub(super) detail: Option<String>,
    pub(super) action: Option<MessageAction>,
    /// The mode it is about: the one it was said in, unless told
    /// otherwise, which `State::tell` fills in.
    pub(super) mode: Option<Mode>,
    /// Until acted on or said over, not for a while: news that stays
    /// true, like the app on disk having been replaced.
    pub(super) stays: bool,
}

impl Said {
    pub(super) fn new(kind: layout::Feedback, text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind,
            detail: None,
            action: None,
            mode: None,
            stays: false,
        }
    }

    /// The whole of what the text shortens: a file's path.
    pub(super) fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// What the message asks for: a button beside it does it.
    pub(super) fn action(mut self, action: MessageAction) -> Self {
        self.action = Some(action);
        self
    }

    /// About the app as a whole: a mode switch does not clear it.
    pub(super) fn everywhere(mut self) -> Self {
        self.mode = Some(Mode::Both);
        self
    }

    /// For as long as nothing is said over it: what it says stays true.
    pub(super) fn stays(mut self) -> Self {
        self.stays = true;
        self
    }

    /// This, when it is about `text`.
    pub(super) fn about(&self, text: &str) -> Option<&Said> {
        (self.text == text).then_some(self)
    }

    /// What `feedback_for` reads: the kind, with the text it is about.
    pub(super) fn kind_of(said: Option<&Said>) -> Option<(&str, layout::Feedback)> {
        said.map(|s| (s.text.as_str(), s.kind))
    }
}

/// Whether the message `text` goes on `dismissal`: by its kind (see
/// `layout::message_clears_on`), except that news about both modes
/// outlasts a switch between them.
pub(super) fn message_goes_on(
    text: &str,
    said: Option<&Said>,
    dismissal: layout::Dismissal,
) -> bool {
    let said = said.and_then(|s| s.about(text));
    let kind = layout::feedback_for(text, Said::kind_of(said));
    let everywhere = said.is_some_and(|s| s.mode == Some(Mode::Both));
    layout::message_clears_on(kind, dismissal)
        && !(everywhere && dismissal == layout::Dismissal::ModeSwitch)
}

/// How long the message `text` stays: by its kind, long enough to reach
/// for its button when it has one, or for good when it was said to stay.
pub(super) fn message_lasts_of(text: &str, said: Option<&Said>) -> Duration {
    let said = said.and_then(|s| s.about(text));
    if said.is_some_and(|s| s.stays) {
        Duration::MAX
    } else if said.is_some_and(|s| s.action.is_some()) {
        layout::MESSAGE_ACTION_LASTS
    } else {
        layout::message_lasts_for(layout::feedback_for(text, Said::kind_of(said)))
    }
}

/// How a status-line message names `path`: by its file name alone, or
/// with its folder when another of the `open` documents has the same
/// name; a path without a name (a root) as it is shown elsewhere.
pub(super) fn message_name<'a>(path: &Path, open: impl Iterator<Item = &'a Path>) -> String {
    let Some(name) = path.file_name() else {
        return shown_path(path);
    };
    let name = name.to_string_lossy();
    let twin = open.filter(|other| *other != path).any(|other| {
        other
            .file_name()
            .is_some_and(|n| n.to_string_lossy() == name)
    });
    match path.parent().and_then(Path::file_name) {
        Some(folder) if twin => format!("{}/{name}", folder.to_string_lossy()),
        _ => name.into_owned(),
    }
}

impl State {
    /// Says `text` in the status line as `kind`: a failure shows red and
    /// stays longer whatever its wording, a success green.
    pub(super) fn say(&mut self, kind: layout::Feedback, text: impl Into<String>) {
        self.tell(Said::new(kind, text));
    }

    /// Says what `said` holds in the status line: its text now, the rest
    /// for as long as the text shows. About the mode of the moment
    /// unless it says otherwise.
    pub(super) fn tell(&mut self, mut said: Said) {
        if said.mode.is_none() {
            said.mode = Some(if self.agents.on {
                Mode::Agents
            } else {
                Mode::Editor
            });
        }
        self.message = Some((said.text.clone(), Instant::now()));
        self.said = Some(said);
    }

    /// Clears the message showing when `dismissal` makes it stale; see
    /// [`message_goes_on`].
    pub(super) fn dismiss(&mut self, dismissal: layout::Dismissal) {
        let goes = self
            .message
            .as_ref()
            .is_some_and(|(text, _)| message_goes_on(text, self.said.as_ref(), dismissal));
        if goes {
            self.message = None;
        }
    }

    /// How a status-line message names `path`; see [`message_name`]. The
    /// active document is what most messages are about, so it is not its
    /// own twin however its path is spelled.
    pub(super) fn name_for(&self, path: &Path) -> String {
        let active = self.docs.active().id();
        message_name(
            path,
            self.docs
                .iter()
                .filter(|b| b.id() != active)
                .filter_map(|b| b.path.as_deref()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_names_a_file_by_its_name_and_its_folder_only_for_a_twin() {
        let a = Path::new("/home/me/garden/src/main.rs");
        let b = Path::new("/home/me/garden/tools/main.rs");
        let c = Path::new("/home/me/garden/docs/notes.md");
        // Alone, or beside documents named otherwise: the name.
        assert_eq!(message_name(a, std::iter::empty()), "main.rs");
        assert_eq!(message_name(a, [a, c].into_iter()), "main.rs");
        // Another open document of the same name: the folder tells them apart.
        assert_eq!(message_name(a, [a, b, c].into_iter()), "src/main.rs");
        assert_eq!(message_name(b, [a, b].into_iter()), "tools/main.rs");
        // No name to show: the path as it is shown elsewhere.
        assert_eq!(message_name(Path::new("/"), std::iter::empty()), "/");
    }

    #[test]
    fn news_about_the_app_outlasts_a_mode_switch() {
        use layout::{Dismissal, Feedback};
        let opened = Said::new(Feedback::Info, "opened notes.md");
        let restored = Said::new(Feedback::Info, "Started again 2 terminals").everywhere();
        let failed = Said::new(Feedback::Failure, "save failed: full").everywhere();
        assert!(message_goes_on(
            "opened notes.md",
            Some(&opened),
            Dismissal::ModeSwitch
        ));
        assert!(message_goes_on(
            "opened notes.md",
            Some(&opened),
            Dismissal::TabClosed
        ));
        assert!(!message_goes_on(
            "Started again 2 terminals",
            Some(&restored),
            Dismissal::ModeSwitch
        ));
        assert!(message_goes_on(
            "Started again 2 terminals",
            Some(&restored),
            Dismissal::TabClosed
        ));
        assert!(!message_goes_on(
            "save failed: full",
            Some(&failed),
            Dismissal::ModeSwitch
        ));
        // Said about another text, or nothing said: the wording decides.
        assert!(message_goes_on(
            "opened notes.md",
            Some(&restored),
            Dismissal::ModeSwitch
        ));
        assert!(message_goes_on(
            "opened notes.md",
            None,
            Dismissal::ModeSwitch
        ));
        assert!(!message_goes_on("push failed", None, Dismissal::ModeSwitch));
    }

    #[test]
    fn a_message_with_a_button_stays_longer_than_its_kind_alone() {
        let said = Said::new(layout::Feedback::Info, "no preview")
            .action(MessageAction::OpenExtension(MARKDOWN_PREVIEW_ID));
        assert_eq!(
            message_lasts_of("no preview", Some(&said)),
            layout::MESSAGE_ACTION_LASTS
        );
        // Said about another text: this message is read by its wording.
        assert_eq!(
            message_lasts_of("saved notes.md", Some(&said)),
            layout::message_lasts_for(layout::Feedback::Success)
        );
        assert_eq!(
            message_lasts_of("saved notes.md", None),
            layout::message_lasts_for(layout::Feedback::Success)
        );
    }

    #[test]
    fn a_message_said_to_stay_does_not_go_on_its_own() {
        let said = Said::new(layout::Feedback::Failure, "crc was updated on disk")
            .action(MessageAction::Restart)
            .stays();
        assert_eq!(
            message_lasts_of("crc was updated on disk", Some(&said)),
            Duration::MAX
        );
        // Another text in front of it is read by its own wording.
        assert_eq!(
            message_lasts_of("saved notes.md", Some(&said)),
            layout::message_lasts_for(layout::Feedback::Success)
        );
    }

    fn gutter() -> Gutter {
        Gutter {
            docs: HashMap::new(),
            dirty: HashMap::new(),
            pending: HashSet::new(),
            stale: HashSet::new(),
            epoch: 0,
            heads: mpsc::channel(),
            computed: mpsc::channel(),
            diffing: HashSet::new(),
        }
    }

    /// An entry diffed against `head`: one added last line.
    fn diffed(gutter: &mut Gutter, head: &str) {
        let text = format!("{head}added\n");
        let changes = crate::ide::diff::changes(head, &text);
        gutter.docs.insert(
            1,
            GutterState {
                head: Some(Arc::new(head.to_string())),
                marks: crate::project::git::marks(&changes),
                changes,
                text: Some(crate::text::rope::Rope::from_text(&text)),
            },
        );
    }

    #[test]
    fn marks_stay_drawn_while_head_is_read_again() {
        let mut gutter = gutter();
        diffed(&mut gutter, "one\n");
        gutter.mark_stale();
        let entry = &gutter.docs[&1];
        assert_eq!(entry.marks.len(), 1);
        assert_eq!(entry.changes.len(), 1);
        assert!(gutter.stale.contains(&1));
        // The same HEAD: nothing to diff again, and the changes still hold.
        assert!(!gutter.land_head(1, gutter.epoch, Some("one\n".into())));
        assert!(!gutter.stale.contains(&1));
        assert_eq!(gutter.docs[&1].changes.len(), 1);
        assert!(gutter.docs[&1].text.is_some());
    }

    #[test]
    fn a_moved_head_keeps_the_marks_until_its_diff_lands() {
        let mut gutter = gutter();
        diffed(&mut gutter, "one\n");
        gutter.mark_stale();
        assert!(gutter.land_head(1, gutter.epoch, Some("two\n".into())));
        let entry = &gutter.docs[&1];
        assert_eq!(entry.head.as_deref().map(String::as_str), Some("two\n"));
        assert_eq!(entry.marks.len(), 1, "the old marks are still drawn");
        assert!(entry.changes.is_empty(), "no command acts on the old HEAD");
        assert!(entry.text.is_none());
    }

    #[test]
    fn a_read_from_before_head_moved_is_dropped() {
        let mut gutter = gutter();
        diffed(&mut gutter, "one\n");
        let before = gutter.epoch;
        gutter.mark_stale();
        assert!(!gutter.land_head(1, before, Some("old\n".into())));
        assert!(gutter.stale.contains(&1), "still wants the new HEAD");
        assert_eq!(
            gutter.docs[&1].head.as_deref().map(String::as_str),
            Some("one\n")
        );
    }

    fn rope(text: &str) -> crate::text::rope::Rope {
        crate::text::rope::Rope::from_text(text)
    }

    #[test]
    fn the_first_changed_line_is_found_wherever_the_texts_part() {
        let a = rope("one\ntwo\nthree\n");
        assert_eq!(first_changed_line(&a, &a.clone()), None);
        assert_eq!(first_changed_line(&a, &rope("one\ntwo\nthree\n")), None);
        assert_eq!(first_changed_line(&a, &rope("one\ntwX\nthree\n")), Some(1));
        assert_eq!(
            first_changed_line(&a, &rope("one\n\ntwo\nthree\n")),
            Some(1)
        );
        assert_eq!(
            first_changed_line(&a, &rope("one\ntwo\nthree\nfour\n")),
            Some(3)
        );
        assert_eq!(first_changed_line(&a, &rope("one\ntwo\n")), Some(2));
        assert_eq!(first_changed_line(&a, &rope("")), Some(0));
        // Past the size of one chunk of the rope.
        let long = "line\n".repeat(5000);
        let edited = format!("{}LINE\n{}", "line\n".repeat(4000), "line\n".repeat(999));
        assert_eq!(first_changed_line(&rope(&long), &rope(&edited)), Some(4000));
    }

    fn blamed(text: &str) -> HeatDoc {
        use crate::project::git::{BlameMap, CommitInfo, NOT_COMMITTED};
        let commit = |sha: &str, time: i64, signed| CommitInfo {
            sha: sha.repeat(40),
            author: "Robin".into(),
            time,
            summary: "plant beans".into(),
            signed,
        };
        let map = BlameMap {
            commits: vec![
                commit("a", 40_000_000 - 3600, Some(false)),
                commit("b", 0, Some(true)),
                commit("c", 0, None),
            ],
            lines: vec![0, NOT_COMMITTED, 1, 0],
            origins: vec![0, 1, 0, 3],
        };
        HeatDoc::new(map, 0, rope(text), 40_000_000, &HashMap::new())
    }

    #[test]
    fn a_blame_works_out_each_commits_heat_and_tooltip_once() {
        let doc = blamed("a\nb\nc\nd\n");
        assert_eq!(doc.alpha, [1.0, 0.0, 0.0]);
        assert_eq!(
            doc.tips[0],
            "aaaaaaa  Robin  1 hour ago\nplant beans\nunsigned\nCmd-click: open in File History"
        );
        assert!(doc.tips[1].contains("\nsigned\n"));
        assert!(
            !doc.tips[2].contains("signed"),
            "nothing said when it is not known"
        );
        assert_eq!(doc.dashed, [true, false, false]);
        assert_eq!(doc.strip().dashed, [true, false, false]);
        assert_eq!(doc.hide_from, usize::MAX);
    }

    #[test]
    fn a_signed_commit_is_checked_once_when_its_tip_shows() {
        let mut heat = heat();
        heat.docs.insert(7, blamed("a\nb\nc\nd\n"));
        let tips = heat.docs[&7].tips.clone();
        // Unsigned, unknown, or not the strip's: nothing to check.
        assert_eq!(heat.check_for_tip(&tips[0]), None);
        assert_eq!(heat.check_for_tip(&tips[2]), None);
        assert_eq!(heat.check_for_tip("Refresh"), None);
        let b = "b".repeat(40);
        assert_eq!(heat.check_for_tip(&tips[1]), Some((7, b.clone())));
        assert!(!heat.idle(), "the display link waits for it");
        assert_eq!(heat.check_for_tip(&tips[1]), None, "asked once");
        heat.land_check(b.clone(), Some('G'), 40_000_000);
        assert!(heat.idle());
        let tip = heat.docs[&7].tips[1].clone();
        assert!(tip.contains("\nsigned, verified\n"), "{tip}");
        assert_eq!(heat.check_for_tip(&tip), None, "and not again");
        // A blame landing later knows it already.
        let doc = HeatDoc::new(
            (*heat.docs[&7].map).clone(),
            0,
            rope("x\n"),
            40_000_000,
            &heat.checks,
        );
        assert!(doc.tips[1].contains("signed, verified"));
        // Git could not say: not unsigned.
        heat.land_check(b, None, 40_000_000);
        assert!(heat.docs[&7].tips[1].contains("\nsigned, cannot verify here\n"));
    }

    #[test]
    fn an_edit_hides_the_strip_from_its_line_down_until_undone() {
        let mut doc = blamed("a\nb\nc\nd\n");
        assert!(doc.follow(&rope("a\nb\nC\nd\n")));
        assert_eq!(doc.strip().hide_from, 2);
        // Further down changes nothing above the first edit.
        assert!(doc.follow(&rope("a\nb\nC\nD\n")));
        assert_eq!(doc.hide_from, 2);
        // An edit higher up hides more.
        assert!(doc.follow(&rope("A\nb\nC\nD\n")));
        assert_eq!(doc.hide_from, 0);
        // Undone, back to the text that was blamed: all of it again.
        assert!(!doc.follow(&rope("a\nb\nc\nd\n")));
        assert_eq!(doc.hide_from, usize::MAX);
    }

    fn heat() -> Heat {
        Heat {
            on: true,
            docs: HashMap::new(),
            dirty: HashMap::new(),
            pending: HashSet::new(),
            generation: 0,
            channel: mpsc::channel(),
            checks: HashMap::new(),
            checking: HashSet::new(),
            checked: mpsc::channel(),
        }
    }

    #[test]
    fn a_blame_lands_only_for_the_text_and_head_it_was_asked_for() {
        let text = rope("a\nb\n");
        let map = || crate::project::git::BlameMap {
            commits: Vec::new(),
            lines: vec![crate::project::git::NOT_COMMITTED; 2],
            origins: vec![0, 1],
        };
        let mut heat = heat();
        heat.pending.insert(1);
        // HEAD may have moved since it was asked.
        assert!(!heat.land((1, 0, 0, text.clone(), map()), 1, Some(&text), 0));
        assert!(heat.pending.is_empty() && heat.docs.is_empty());
        // The text moved on: asked again once typing stops.
        assert!(!heat.land((1, 0, 1, text.clone(), map()), 1, Some(&rope("a\nB\n")), 0));
        assert!(heat.dirty.contains_key(&1));
        heat.dirty.clear();
        // The same text, even as another tree: drawn.
        assert!(heat.land((1, 0, 1, text.clone(), map()), 1, Some(&rope("a\nb\n")), 0));
        assert_eq!(heat.docs[&1].hide_from, usize::MAX);
        // An edit since: hidden from the edit, and asked again later.
        let edited = rope("a\nB\n");
        assert_eq!(heat.follow(1, &edited).map(|d| d.hide_from), Some(1));
        assert!(heat.dirty.contains_key(&1));
        // Undone: nothing to ask.
        assert_eq!(heat.follow(1, &text).map(|d| d.hide_from), Some(usize::MAX));
        assert!(heat.dirty.is_empty());
        // Off: everything dropped, and a blame from before is not taken,
        // nor does it end one started since.
        heat.pending.insert(2);
        heat.turn_off();
        assert!(!heat.on && heat.docs.is_empty() && heat.idle());
        heat.on = true;
        heat.pending.insert(2);
        assert!(!heat.land((2, 0, 1, text.clone(), map()), 1, Some(&text), 0));
        assert!(heat.pending.contains(&2));
        assert!(heat.land((2, 1, 1, text.clone(), map()), 1, Some(&text), 0));
        assert!(heat.idle());
    }

    #[test]
    fn a_file_that_left_the_repository_loses_its_marks() {
        let mut gutter = gutter();
        diffed(&mut gutter, "one\n");
        gutter.mark_stale();
        assert!(!gutter.land_head(1, gutter.epoch, None));
        let entry = &gutter.docs[&1];
        assert!(entry.head.is_none());
        assert!(entry.marks.is_empty() && entry.changes.is_empty());
    }
}
