//! Git > File History: the file at each commit that changed it, in a
//! read-only tab beside the file's own. Left and Right step to the older
//! and the newer version, Home and End to the oldest and to the working
//! tree, Escape goes back to the file. The file's own tab is never touched.
//!
//! One worker per open history lists the versions, then reads them through
//! one `git cat-file --batch`, nearest the version shown first, with the
//! diff between each pair of neighbours: it moves the caret from one
//! version to the next and draws the gutter marks.

use std::collections::VecDeque;

use super::*;
use crate::ide::diff::Change;
use crate::project::history::{Blob, BlobReader, HISTORY_LIMIT, Version};

/// Versions kept loaded either side of the one shown. This is the memory
/// bound: at most `2 * RADIUS + 1` texts of up to 2 MB each (the gutter's
/// limit, which a file must be under to have a history) and as many diffs,
/// plus the working tree. The worker and the window share them through
/// `Arc`, so none is held twice.
const RADIUS: usize = 16;

/// One piece of work for the worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Job {
    /// A version's text.
    Text(usize),
    /// The changes from version `n` to version `n + 1`.
    Diff(usize),
}

pub(super) enum Request {
    /// What to load around version `center`, in order. Replaces the last
    /// request: whatever it still had queued is no longer wanted.
    Want { center: usize, jobs: Vec<Job> },
    /// The file's own text, the last version, changed.
    Worktree(Arc<str>),
}

pub(super) enum Reply {
    /// The versions, oldest first, and whether the list was cut short; or
    /// why there are none.
    Versions(Result<(Vec<Version>, bool), String>),
    Text(usize, Arc<str>),
    Diff(usize, Arc<Vec<Change>>),
}

/// Where a step goes.
#[derive(Clone, Copy)]
pub(super) enum Step {
    Older,
    Newer,
    Oldest,
    WorkingTree,
}

/// An open file history.
pub(super) struct Scrub {
    /// The file's own tab.
    live: u64,
    /// The tab the versions show in, made when the first one is read.
    sibling: Option<u64>,
    /// The file's name, for the tab's title and the messages.
    name: String,
    ext: &'static str,
    /// Oldest first; `None` while they are listed. The slot past the last
    /// commit is the working tree, the text in the file's own tab.
    versions: Option<Vec<Version>>,
    capped: bool,
    /// The version the tab shows.
    shown: usize,
    /// The version a step goes to, while its text is read.
    pending: Option<usize>,
    texts: HashMap<usize, Arc<str>>,
    /// The changes between versions `n` and `n + 1`, by `n`.
    diffs: HashMap<usize, Arc<Vec<Change>>>,
    /// The file's own text as last read, the working tree's version.
    worktree: crate::text::rope::Rope,
    /// The file's caret (line, column) and first line when the history
    /// opened, which the first version shown starts from.
    start: (usize, usize, usize),
    tx: mpsc::Sender<Request>,
    rx: mpsc::Receiver<Reply>,
    stop: Arc<AtomicBool>,
    /// The worker has stopped: nothing more will arrive.
    gone: bool,
}

impl Drop for Scrub {
    fn drop(&mut self) {
        // The worker stops at its next job; its reader's `git cat-file`
        // is killed with it.
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The versions within [`RADIUS`] of `center`, among `slots`, nearest
/// first, the older of two at the same distance first.
pub(super) fn window_order(center: usize, slots: usize) -> Vec<usize> {
    let mut out = Vec::new();
    if center < slots {
        out.push(center);
    }
    for d in 1..=RADIUS {
        if let Some(older) = center.checked_sub(d) {
            out.push(older);
        }
        if center + d < slots {
            out.push(center + d);
        }
    }
    out
}

/// Whether version `slot` is kept while `center` is shown.
fn in_window(slot: usize, center: usize) -> bool {
    slot.abs_diff(center) <= RADIUS
}

/// What to load around `center`, nearest first: each version's text, then
/// the diff joining it to its neighbour toward `center`, so a step either
/// way has its text and its caret mapping as early as they can be had.
pub(super) fn jobs(
    center: usize,
    slots: usize,
    has_text: impl Fn(usize) -> bool,
    has_diff: impl Fn(usize) -> bool,
) -> Vec<Job> {
    let mut out = Vec::new();
    for slot in window_order(center, slots) {
        if !has_text(slot) {
            out.push(Job::Text(slot));
        }
        let pair = match slot.cmp(&center) {
            std::cmp::Ordering::Less => Some(slot),
            std::cmp::Ordering::Greater => Some(slot - 1),
            std::cmp::Ordering::Equal => None,
        };
        if let Some(pair) = pair
            && !has_diff(pair)
        {
            out.push(Job::Diff(pair));
        }
    }
    out
}

/// Drops what is out of the window around `center`: texts but the working
/// tree's `keep`, and diffs with either end out.
pub(super) fn evict<T, U>(
    texts: &mut HashMap<usize, T>,
    diffs: &mut HashMap<usize, U>,
    center: usize,
    keep: usize,
) {
    texts.retain(|&slot, _| slot == keep || in_window(slot, center));
    diffs.retain(|&pair, _| in_window(pair, center) && in_window(pair + 1, center));
}

/// Line `line` of version `from` in version `to`, through the diffs
/// between them; `None` when one of those is not loaded.
pub(super) fn map_through(
    diffs: &HashMap<usize, Arc<Vec<Change>>>,
    from: usize,
    to: usize,
    line: usize,
) -> Option<usize> {
    let mut line = line;
    if to > from {
        for pair in from..to {
            line = crate::ide::diff::map_line(diffs.get(&pair)?, line);
        }
    } else {
        for pair in (to..from).rev() {
            line = crate::ide::diff::map_line_back(diffs.get(&pair)?, line);
        }
    }
    Some(line)
}

/// `ext` as the `'static` name a pathless document is highlighted by.
/// Each distinct one is leaked once, so a file type opened again costs
/// nothing more.
fn static_ext(ext: &str) -> &'static str {
    static SEEN: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(known) = seen.iter().find(|known| **known == ext) {
        return known;
    }
    let leaked: &'static str = Box::leak(ext.to_owned().into_boxed_str());
    seen.push(leaked);
    leaked
}

fn short(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

impl Scrub {
    /// Whether `id` is the file's tab or the history's.
    pub(super) fn involves(&self, id: u64) -> bool {
        self.live == id || self.sibling == Some(id)
    }

    /// Something is on its way that the display link has to poll for.
    pub(super) fn busy(&self) -> bool {
        !self.gone
            && (self.versions.is_none()
                || self.pending.is_some()
                || (self.sibling.is_some()
                    && self.shown > 0
                    && !self.diffs.contains_key(&(self.shown - 1))))
    }

    fn count(&self) -> usize {
        self.versions.as_ref().map_or(0, Vec::len)
    }

    /// The version the window is loaded around.
    fn center(&self) -> usize {
        self.pending.unwrap_or(self.shown)
    }

    /// Whether version `target` can be shown, coming from `from`: its text
    /// is here, and so is the diff to it from a neighbour.
    fn ready(&self, from: usize, target: usize) -> bool {
        self.texts.contains_key(&target)
            && (from.abs_diff(target) != 1
                || self.diffs.contains_key(&from.min(target))
                || self.gone)
    }

    /// Asks the worker for what the window lacks, and lets go of what
    /// fell out of it.
    fn request(&mut self) {
        let center = self.center();
        let count = self.count();
        evict(&mut self.texts, &mut self.diffs, center, count);
        let jobs = jobs(
            center,
            count + 1,
            |slot| self.texts.contains_key(&slot),
            |pair| self.diffs.contains_key(&pair),
        );
        if !jobs.is_empty() {
            let _ = self.tx.send(Request::Want { center, jobs });
        }
    }

    /// The working tree's version is the file's text now: read again when
    /// it changed since, and its diff worked out again.
    fn refresh_worktree(&mut self, live: &crate::text::rope::Rope) {
        if self.worktree.same_as(live) {
            return;
        }
        let count = self.count();
        let text: Arc<str> = Arc::from(live.to_string());
        self.worktree = live.clone();
        self.texts.insert(count, text.clone());
        if let Some(pair) = count.checked_sub(1) {
            self.diffs.remove(&pair);
        }
        let _ = self.tx.send(Request::Worktree(text));
    }

    fn label(&self, slot: usize) -> String {
        match self.versions.as_ref().and_then(|v| v.get(slot)) {
            Some(version) => format!("{} @ {}", self.name, short(&version.commit)),
            None => format!("{} @ working tree", self.name),
        }
    }

    /// What the status line says of version `slot`.
    fn describe(&self, slot: usize) -> String {
        let count = self.count();
        let more = if self.capped { "+" } else { "" };
        match self.versions.as_ref().and_then(|v| v.get(slot)) {
            Some(version) => {
                let now = crate::platform::unix_seconds() as i64;
                format!(
                    "{} {}, {}: {} ({} of {count}{more})",
                    short(&version.commit),
                    version.author,
                    crate::project::git::ago(version.time, now),
                    version.summary,
                    slot + 1,
                )
            }
            None => format!("working tree, after {count}{more} commits"),
        }
    }
}

/// What the self-test dump says of the history.
pub(super) fn dump(state: &State) -> String {
    let Some(scrub) = &state.scrub else {
        return "closed".into();
    };
    let Some(versions) = &scrub.versions else {
        return "listing".into();
    };
    let Some(sibling) = scrub
        .sibling
        .and_then(|id| buffer_by_id(&state.docs, &state.panes, id))
    else {
        return format!("opening pending={:?}", scrub.pending);
    };
    let at = match versions.get(scrub.shown) {
        Some(version) => format!(
            "{}/{}@{}",
            scrub.shown + 1,
            versions.len(),
            short(&version.commit)
        ),
        None => format!("worktree/{}", versions.len()),
    };
    let first = sibling
        .rope
        .slice_to_string(0..sibling.rope.line_to_byte(1.min(sibling.rope.len_lines())));
    format!(
        "{at} loaded={} pending={:?} first={}",
        scrub.texts.len(),
        scrub.pending,
        first.trim_end_matches('\n')
    )
}

/// Whether the arrows and Escape step the history: its tab is in front and
/// the keyboard is the document's.
pub(super) fn has_keys(state: &State) -> bool {
    let active = state.docs.active().id();
    state
        .scrub
        .as_ref()
        .is_some_and(|s| s.sibling == Some(active))
        && keys(state) == Keys::Document
        && state.completion.is_none()
        && state.peek.is_none()
}

/// What a key does in the history's tab: a step, or `Some(None)` for
/// Escape, which closes it. Shift and the rest keep their own meaning.
pub(super) fn history_step(event: &NSEvent) -> Option<Option<Step>> {
    const ESCAPE: u16 = 53;
    let modified = event.modifierFlags().intersects(
        NSEventModifierFlags::Command
            | NSEventModifierFlags::Option
            | NSEventModifierFlags::Control
            | NSEventModifierFlags::Shift,
    );
    if modified {
        return None;
    }
    Some(Some(match event.keyCode() {
        key::LEFT => Step::Older,
        key::RIGHT => Step::Newer,
        key::HOME => Step::Oldest,
        key::END => Step::WorkingTree,
        ESCAPE => return Some(None),
        _ => return None,
    }))
}

/// Shows the version a step waits for, once it can be: the text goes into
/// the history's tab (made on the first), the caret and the view are moved
/// through the diffs, and the gutter marks what that commit changed.
/// Returns what to say, and whether the tab was just made.
fn show(state: &mut State) -> Option<(String, bool)> {
    let State {
        scrub,
        docs,
        panes,
        gutter,
        ..
    } = state;
    let scrub = scrub.as_mut()?;
    let target = scrub.pending?;
    let count = scrub.count();
    let made = scrub
        .sibling
        .is_some_and(|id| buffer_by_id(docs, panes, id).is_some());
    let from = if made { scrub.shown } else { count };
    if !scrub.ready(from, target) {
        return None;
    }
    let text = scrub.texts.get(&target)?.clone();
    let label = scrub.label(target);
    let ((line, column), top) = match scrub.sibling.and_then(|id| buffer_by_id(docs, panes, id)) {
        Some(buffer) => (buffer.cursor_position(), buffer.scroll_line),
        None => ((scrub.start.0, scrub.start.1), scrub.start.2),
    };
    let map = |line| map_through(&scrub.diffs, from, target, line).unwrap_or(line);
    let (line, top) = (map(line), map(top));
    if !made {
        // Generated: it has no path, so nothing saves it, restores it after
        // a crash or a relaunch, hands it to a language server, or reads
        // its HEAD for the gutter.
        let mut buffer = Buffer::generated(&label, scrub.ext, "");
        buffer.set_frozen(true);
        scrub.sibling = Some(buffer.id());
        docs.push(buffer);
    }
    let id = scrub.sibling?;
    let buffer = buffer_by_id_mut(docs, panes, id)?;
    buffer.label = Some(label);
    buffer.show_text(&text, line, column);
    let top = top.min(buffer.rope.len_lines().saturating_sub(1));
    if top != buffer.scroll_line {
        buffer.scroll_line = top;
        buffer.scroll_fraction = 0.0;
        buffer.scroll_row = 0;
    }
    let rope = buffer.rope.clone();
    scrub.shown = target;
    scrub.pending = None;
    // What this commit changed, against its parent: the oldest version
    // has no parent read, and no marks rather than every line added.
    // No HEAD text: nothing here acts on the changes, and nothing reads
    // HEAD for a document without a path.
    let changes = target
        .checked_sub(1)
        .and_then(|pair| scrub.diffs.get(&pair))
        .map(|c| c.to_vec())
        .unwrap_or_default();
    gutter.docs.insert(
        id,
        GutterState {
            head: None,
            marks: crate::project::git::marks(&changes),
            changes,
            text: Some(rope),
        },
    );
    scrub.request();
    Some((scrub.describe(target), !made))
}

/// Closes the history: its tab goes, wherever it is, and with `back` the
/// file's own tab comes to the front as it was.
fn close(state: &mut State, back: bool) {
    let Some(scrub) = state.scrub.take() else {
        return;
    };
    if let Some(id) = scrub.sibling {
        for docs in all_docs_mut(state) {
            let index = docs.iter().position(|b| b.id() == id);
            if let Some(index) = index {
                docs.close(index);
            }
        }
        forget_document(state, id);
    }
    let live = state.docs.iter().position(|b| b.id() == scrub.live);
    if back && let Some(index) = live {
        state.docs.switch(index);
    }
    reveal_active_tab(state);
}

impl EditorView {
    /// Git > File History.
    pub(super) fn open_history(&self) {
        let opened = self.start_history();
        if let Err(why) = opened
            && let Some(mut state) = self.state_mut()
        {
            state.message = Some((why, Instant::now()));
        }
        self.sync_title();
        self.reparse();
        self.resume_display_link();
        self.request_redraw();
        self.pump();
    }

    fn start_history(&self) -> Result<(), String> {
        let Some(mut state) = self.state_mut() else {
            return Ok(());
        };
        let state = &mut *state;
        let buffer = state.docs.active();
        let id = buffer.id();
        if let Some(scrub) = &state.scrub {
            if scrub.sibling == Some(id) {
                return Err("Escape goes back to the file".into());
            }
            if scrub.live == id {
                let index = scrub
                    .sibling
                    .and_then(|sibling| state.docs.iter().position(|b| b.id() == sibling));
                return match index {
                    Some(index) => {
                        state.docs.switch(index);
                        reveal_active_tab(state);
                        Ok(())
                    }
                    None => Err(format!("reading the history of {}", scrub.name)),
                };
            }
        }
        if buffer.is_frozen() {
            return Err("a version from history has no history of its own".into());
        }
        let Some(path) = buffer.path.clone() else {
            return Err("an unsaved document has no history".into());
        };
        if buffer.is_preview_file() {
            return Err("no history for a file shown as a preview".into());
        }
        if buffer.rope.len_bytes() > GUTTER_MAX_BYTES {
            return Err("no history for files over 2 MB".into());
        }
        // The gutter already found it outside any repository.
        if state.gutter.docs.get(&id).is_some_and(|g| g.head.is_none()) {
            return Err("this file is not in a Git repository".into());
        }
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let ext = static_ext(&buffer.extension().unwrap_or_else(|| "txt".into()));
        let worktree = buffer.rope.clone();
        let (line, column) = buffer.cursor_position();
        let start = (line, column, buffer.scroll_line);
        let text: Arc<str> = Arc::from(worktree.to_string());
        // One history at a time: another file's goes.
        close(state, false);
        let (tx, worker_rx) = mpsc::channel();
        let (worker_tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let halt = stop.clone();
        std::thread::spawn(move || work(&path, text, &worker_rx, &worker_tx, &halt));
        state.scrub = Some(Scrub {
            live: id,
            sibling: None,
            name: name.clone(),
            ext,
            versions: None,
            capped: false,
            shown: 0,
            pending: None,
            texts: HashMap::new(),
            diffs: HashMap::new(),
            worktree,
            start,
            tx,
            rx,
            stop,
            gone: false,
        });
        state.message = Some((format!("reading the history of {name}"), Instant::now()));
        Ok(())
    }

    /// Escape in the history's tab: back to the file.
    pub(super) fn close_history(&self) {
        if let Some(mut state) = self.state_mut() {
            close(&mut state, true);
        }
        self.sync_title();
        self.reparse();
        self.request_redraw();
        self.pump();
    }

    /// A step through the versions, from the keys or the Git menu.
    pub(super) fn step_history(&self, step: Step) {
        let said = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let State {
                scrub, docs, panes, ..
            } = &mut *state;
            let active = docs.active().id();
            match scrub.as_mut().filter(|s| s.sibling == Some(active)) {
                None => Some("File History is not open in this tab".to_string()),
                Some(scrub) => {
                    let count = scrub.count();
                    let at = scrub.center();
                    let target = match step {
                        Step::Older => at.checked_sub(1),
                        Step::Newer => (at < count).then_some(at + 1),
                        Step::Oldest => Some(0),
                        Step::WorkingTree => Some(count),
                    };
                    if target == Some(count)
                        && let Some(live) = buffer_by_id(docs, panes, scrub.live)
                    {
                        let live = live.rope.clone();
                        scrub.refresh_worktree(&live);
                    }
                    match target {
                        Some(target) if target != at => {
                            scrub.pending = Some(target);
                            scrub.request();
                            None
                        }
                        _ if at == count => Some("this is the working tree".to_string()),
                        _ if scrub.capped => {
                            format!("the oldest of the {count} versions read").into()
                        }
                        _ => Some("this is the oldest version".to_string()),
                    }
                }
            }
        };
        if let Some(said) = said {
            if let Some(mut state) = self.state_mut() {
                state.message = Some((said, Instant::now()));
            }
            self.request_redraw();
            return;
        }
        self.show_pending();
        self.resume_display_link();
    }

    /// Shows the version a step waits for if it is here, else says it is
    /// on its way.
    fn show_pending(&self) {
        let made = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            match show(&mut state) {
                Some((said, made)) => {
                    state.message = Some((said, Instant::now()));
                    if made {
                        reveal_active_tab(&mut state);
                    }
                    Some(made)
                }
                None => {
                    if let Some(scrub) = &state.scrub
                        && scrub.sibling.is_some()
                        && let Some(target) = scrub.pending
                    {
                        let said = format!("loading version {} of {}", target + 1, scrub.count());
                        state.message = Some((said, Instant::now()));
                    }
                    None
                }
            }
        };
        if made == Some(true) {
            self.sync_title();
        }
        if made.is_some() {
            self.reparse();
        }
        self.request_redraw();
    }

    /// Takes what the history's worker sent. Runs from the display link.
    pub(super) fn poll_scrub(&self) {
        let mut said = None;
        let mut redraw = false;
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let state = &mut *state;
            let Some(scrub) = state.scrub.as_mut() else {
                return;
            };
            let mut end = false;
            loop {
                match scrub.rx.try_recv() {
                    Ok(Reply::Versions(Ok((versions, capped)))) => {
                        if versions.is_empty() {
                            said = Some(format!(
                                "{} has no history: Git does not track it",
                                scrub.name
                            ));
                            end = true;
                            break;
                        }
                        let count = versions.len();
                        scrub.versions = Some(versions);
                        scrub.capped = capped;
                        scrub.shown = count;
                        scrub
                            .texts
                            .insert(count, Arc::from(scrub.worktree.to_string()));
                        scrub.pending = Some(count - 1);
                    }
                    Ok(Reply::Versions(Err(why))) => {
                        said = Some(why);
                        end = true;
                        break;
                    }
                    Ok(Reply::Text(slot, text)) => {
                        if in_window(slot, scrub.center()) {
                            scrub.texts.insert(slot, text);
                        }
                    }
                    Ok(Reply::Diff(pair, changes)) => {
                        let center = scrub.center();
                        if !(in_window(pair, center) && in_window(pair + 1, center)) {
                            continue;
                        }
                        // The marks of the version shown, which came first.
                        if pair + 1 == scrub.shown
                            && let Some(entry) =
                                scrub.sibling.and_then(|id| state.gutter.docs.get_mut(&id))
                        {
                            entry.marks = crate::project::git::marks(&changes);
                            entry.changes = changes.to_vec();
                            redraw = true;
                        }
                        scrub.diffs.insert(pair, changes);
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if !scrub.gone
                            && scrub.pending.is_some_and(|t| !scrub.texts.contains_key(&t))
                        {
                            scrub.pending = None;
                            said = Some("could not read that version".to_string());
                        }
                        scrub.gone = true;
                        break;
                    }
                }
            }
            // A tab closed some way that did not say so.
            let open = |id: u64| buffer_by_id(&state.docs, &state.panes, id).is_some();
            if let Some(scrub) = &state.scrub
                && (!open(scrub.live) || scrub.sibling.is_some_and(|id| !open(id)))
            {
                end = true;
            }
            if end {
                state.scrub = None;
                redraw = true;
            }
        }
        if let Some(said) = said
            && let Some(mut state) = self.state_mut()
        {
            state.message = Some((said, Instant::now()));
            redraw = true;
        }
        let waiting = self
            .state()
            .is_some_and(|state| state.scrub.as_ref().is_some_and(|s| s.pending.is_some()));
        if waiting {
            self.show_pending();
        } else if redraw {
            self.request_redraw();
        }
    }
}

/// The worker: lists the versions of `path`, then loads what is asked for,
/// the newest commit's neighbourhood until a request says otherwise.
fn work(
    path: &Path,
    worktree: Arc<str>,
    rx: &mpsc::Receiver<Request>,
    tx: &mpsc::Sender<Reply>,
    stop: &AtomicBool,
) {
    let listed = path
        .parent()
        .ok_or_else(|| "no parent directory".to_string())
        .and_then(crate::project::git::toplevel)
        .map_err(|_| "this file is not in a Git repository".to_string())
        .and_then(|root| {
            let relative = path
                .strip_prefix(&root)
                .map_err(|_| "this file is not in its repository".to_string())?
                .to_path_buf();
            let (versions, capped) =
                crate::project::history::file_history(&root, &relative, HISTORY_LIMIT)?;
            Ok((root, versions, capped))
        });
    let (root, versions) = match listed {
        Ok((root, versions, capped)) => {
            let empty = versions.is_empty();
            if tx
                .send(Reply::Versions(Ok((versions.clone(), capped))))
                .is_err()
                || empty
            {
                return;
            }
            (root, versions)
        }
        Err(why) => {
            let _ = tx.send(Reply::Versions(Err(why)));
            return;
        }
    };
    let worktree_slot = versions.len();
    let mut texts: HashMap<usize, Arc<str>> = HashMap::from([(worktree_slot, worktree)]);
    let mut queue: VecDeque<Job> = jobs(
        worktree_slot - 1,
        worktree_slot + 1,
        |slot| slot == worktree_slot,
        |_| false,
    )
    .into();
    let mut reader: Option<BlobReader> = None;
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        // Every request in first: the newest one says what is wanted.
        let request = if queue.is_empty() {
            match rx.recv() {
                Ok(request) => Some(request),
                Err(_) => return,
            }
        } else {
            match rx.try_recv() {
                Ok(request) => Some(request),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        };
        if let Some(request) = request {
            match request {
                Request::Want { center, jobs } => {
                    let mut none: HashMap<usize, ()> = HashMap::new();
                    evict(&mut texts, &mut none, center, worktree_slot);
                    queue = jobs.into();
                }
                Request::Worktree(text) => {
                    texts.insert(worktree_slot, text);
                }
            }
            continue;
        }
        let Some(job) = queue.pop_front() else {
            continue;
        };
        let mut load = |slot: usize| -> Arc<str> {
            texts
                .entry(slot)
                .or_insert_with(|| read_version(&root, versions.get(slot), &mut reader))
                .clone()
        };
        let reply = match job {
            Job::Text(slot) if slot <= worktree_slot => Reply::Text(slot, load(slot)),
            Job::Diff(pair) if pair < worktree_slot => {
                let (old, new) = (load(pair), load(pair + 1));
                Reply::Diff(pair, Arc::new(crate::ide::diff::changes(&old, &new)))
            }
            _ => continue,
        };
        if tx.send(reply).is_err() {
            return;
        }
    }
}

/// The text of `version`, or a line saying why there is none, so a step
/// onto it still shows something.
fn read_version(
    root: &Path,
    version: Option<&Version>,
    reader: &mut Option<BlobReader>,
) -> Arc<str> {
    let Some(version) = version else {
        return Arc::from("");
    };
    if reader.is_none() {
        match BlobReader::open(root) {
            Ok(opened) => *reader = Some(opened),
            Err(why) => return Arc::from(format!("(could not read this version: {why})\n")),
        }
    }
    let Some(open) = reader.as_mut() else {
        return Arc::from("");
    };
    let text = match open.read(&version.blob, GUTTER_MAX_BYTES) {
        Ok(Blob::Text(text)) => return Arc::from(text),
        Ok(Blob::Binary) => "(binary at this commit)\n".to_string(),
        Ok(Blob::TooLarge) => "(over 2 MB at this commit)\n".to_string(),
        Ok(Blob::Missing) => "(not in the repository)\n".to_string(),
        Err(why) => {
            // Out of step: the next read starts a new reader.
            *reader = None;
            format!("(could not read this version: {why})\n")
        }
    };
    Arc::from(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_is_nearest_first_and_older_first() {
        assert_eq!(window_order(3, 6), [3, 2, 4, 1, 5, 0]);
        assert_eq!(window_order(0, 1), [0]);
        let wide = window_order(40, 100);
        assert_eq!(wide.len(), 2 * RADIUS + 1);
        assert_eq!(wide[..3], [40, 39, 41]);
        assert!(wide.iter().all(|&s| s.abs_diff(40) <= RADIUS));
        let end = window_order(99, 100);
        assert_eq!(end.len(), RADIUS + 1);
        assert_eq!(*end.last().unwrap(), 99 - RADIUS);
    }

    #[test]
    fn jobs_put_each_text_before_the_diff_toward_the_center() {
        // Five commits and the working tree, which the window already has.
        let got = jobs(4, 6, |s| s == 5, |_| false);
        assert_eq!(
            got[..6],
            [
                Job::Text(4),
                Job::Text(3),
                Job::Diff(3),
                Job::Diff(4),
                Job::Text(2),
                Job::Diff(2),
            ]
        );
        assert!(!got.contains(&Job::Text(5)));
        // Every diff comes after both of its texts, or they were here.
        for (at, job) in got.iter().enumerate() {
            if let Job::Diff(pair) = *job {
                for slot in [pair, pair + 1] {
                    let loaded = slot == 5 || got[..at].contains(&Job::Text(slot));
                    assert!(loaded, "{job:?} before its texts in {got:?}");
                }
            }
        }
        // What is here is not asked for again.
        let have = jobs(2, 6, |s| s != 0, |p| p != 0);
        assert_eq!(have, [Job::Text(0), Job::Diff(0)]);
        // A jump to the oldest asks for it first: it jumps the queue.
        assert_eq!(jobs(0, 50, |_| false, |_| false)[0], Job::Text(0));
    }

    #[test]
    fn eviction_keeps_the_window_and_the_working_tree() {
        let mut texts: HashMap<usize, ()> = (0..=60).map(|s| (s, ())).collect();
        let mut diffs: HashMap<usize, ()> = (0..60).map(|s| (s, ())).collect();
        evict(&mut texts, &mut diffs, 10, 60);
        let mut kept: Vec<usize> = texts.into_keys().collect();
        kept.sort_unstable();
        let mut expected: Vec<usize> = (0..=10 + RADIUS).collect();
        expected.push(60);
        assert_eq!(kept, expected);
        let mut pairs: Vec<usize> = diffs.into_keys().collect();
        pairs.sort_unstable();
        assert_eq!(pairs, (0..10 + RADIUS).collect::<Vec<_>>());
    }

    #[test]
    fn lines_map_through_several_versions_and_back() {
        let versions = ["a\nb\nc\n", "x\na\nb\nc\n", "x\na\nc\n"];
        let diffs: HashMap<usize, Arc<Vec<Change>>> = (0..2)
            .map(|p| {
                (
                    p,
                    Arc::new(crate::ide::diff::changes(versions[p], versions[p + 1])),
                )
            })
            .collect();
        // `c`: line 2, then 3, then 2 again.
        assert_eq!(map_through(&diffs, 0, 1, 2), Some(3));
        assert_eq!(map_through(&diffs, 0, 2, 2), Some(2));
        assert_eq!(map_through(&diffs, 2, 0, 2), Some(2));
        assert_eq!(map_through(&diffs, 1, 0, 1), Some(0));
        assert_eq!(map_through(&diffs, 1, 1, 7), Some(7));
        assert_eq!(map_through(&diffs, 0, 3, 0), None, "a diff not loaded");
    }

    #[test]
    fn an_extension_is_leaked_once() {
        let a = static_ext("scrubtest");
        let b = static_ext(&String::from("scrubtest"));
        assert!(std::ptr::eq(a, b));
        assert_eq!(a, "scrubtest");
    }
}
