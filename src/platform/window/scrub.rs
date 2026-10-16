//! Git > File History: the file at each commit that changed it, and the
//! versions local history kept between them, in a read-only tab beside the
//! file's own. Left and Right step to the older and the newer version,
//! Home and End to the oldest and to the working tree, Escape goes back to
//! the file. The file's own tab is never touched. A file Git does not
//! track has its local versions alone.
//!
//! One worker per open history lists the versions, then reads them, a
//! commit's through one `git cat-file --batch` and a local one's from the
//! history database on a read-only connection of its own, nearest the
//! version shown first, with the diff between each pair of neighbours: it
//! moves the caret from one version to the next and draws the gutter
//! marks. The filter (All, Commits, Local) picks which versions are
//! neighbours.
//!
//! A step marks what it changed for a moment: a wash behind the lines it
//! added or changed, and the lines it removed drawn where they were, all
//! fading out over [`FADE`].

use std::collections::VecDeque;

use super::*;
use crate::history::store::{Reason, Store};
use crate::ide::diff::Change;
use crate::project::git::MarkKind;
use crate::project::history::{Blob, BlobReader, HISTORY_LIMIT, Source, Version};

/// Versions kept loaded either side of the one shown. This is the memory
/// bound: at most `2 * RADIUS + 1` texts of up to 2 MB each (the gutter's
/// limit, which a file must be under to have a history) and as many diffs,
/// plus the working tree and the version in the tab while a jump is on its
/// way. The worker and the window share them through `Arc`, so none is
/// held twice.
const RADIUS: usize = 16;

/// One piece of work for the worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Job {
    /// A version's text.
    Text(usize),
    /// The changes from version `n` to version `n + 1`.
    Diff(usize),
    /// The changes from one version to another further off, for a jump.
    Jump(usize, usize),
    /// The changes from the working tree to a version, for its marks when
    /// they are against the working tree.
    Tree(usize),
}

pub(super) enum Request {
    /// What to load around version `center`, in order. Replaces the last
    /// request: whatever it still had queued is no longer wanted. The
    /// version `shown` in the tab stays loaded wherever `center` is.
    Want {
        center: usize,
        shown: usize,
        jobs: Vec<Job>,
    },
    /// The file's own text, the last version, changed: the `n`th text it
    /// has had, which the replies against it carry.
    Worktree(Arc<str>, u64),
    /// The filter changed: these are the versions now, oldest first, and
    /// every reply from here goes to `tx`. What was loaded is numbered the
    /// old way and goes, but for the working tree.
    Filter {
        versions: Vec<Version>,
        tx: mpsc::Sender<Reply>,
    },
}

pub(super) enum Reply {
    /// Every version, commits and local ones, oldest first, whether the
    /// commits were cut short, and whether local history could not be
    /// read; or why there are none. The filter picks from these on both
    /// sides, through [`visible`].
    Versions(Result<(Vec<Version>, bool, bool), String>),
    Text(usize, Arc<str>),
    Diff(usize, Arc<Vec<Change>>),
    /// The changes from one version to another, as [`Job::Jump`] asked.
    Jump(usize, usize, Arc<Vec<Change>>),
    /// The changes from the working tree, the `n`th text it has had, to a
    /// version, as [`Job::Tree`] asked.
    Tree(usize, u64, Arc<Vec<Change>>),
    /// The version's text could not be read: the text sent next is a line
    /// saying why, and it is no version to restore.
    Unreadable(usize),
}

/// Where a step goes.
#[derive(Clone, Copy)]
pub(super) enum Step {
    Older,
    Newer,
    Oldest,
    WorkingTree,
    /// A version by its place, oldest first, from the timeline.
    To(usize),
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
    /// The versions the filter keeps, oldest first; `None` while they are
    /// listed. The slot past the last is the working tree, the text in the
    /// file's own tab.
    versions: Option<Vec<Version>>,
    /// Every version, whatever the filter.
    all: Vec<Version>,
    filter: Filter,
    capped: bool,
    /// The version the tab shows.
    shown: usize,
    /// The tab's text is not version `shown`'s: the filter left out the
    /// one it shows, and the next to show replaces it whole.
    refill: bool,
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
    /// Opened at a commit, from the blame heat strip: where to go once
    /// the versions are listed.
    goto: Option<Goto>,
    /// `start` is already in the first version's lines, and is not
    /// mapped from the working tree's.
    placed: bool,
    /// Said before the first version's own line, once it shows.
    note: Option<String>,
    /// What the last step changed, while it fades.
    fade: Option<Fade>,
    /// The changes from the version shown to the one a jump waits for,
    /// from the worker: (from, to, changes).
    jump: Option<(usize, usize, Arc<Vec<Change>>)>,
    /// The versions whose text could not be read, by slot.
    unreadable: std::collections::HashSet<usize>,
    /// The gutter marks what a version changed against the working tree,
    /// not against the version before it.
    against_tree: bool,
    /// The changes from the working tree to version `n`, by `n`, for those
    /// marks; kept like the diffs, and gone when the working tree changes.
    tree_diffs: HashMap<usize, Arc<Vec<Change>>>,
    /// How many texts the working tree has had, as the worker counts them.
    tree_generation: u64,
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

/// Drops what is out of the window around `center`: texts but those of
/// `keep`, the working tree and the version shown, and diffs with either
/// end out.
pub(super) fn evict<T, U>(
    texts: &mut HashMap<usize, T>,
    diffs: &mut HashMap<usize, U>,
    center: usize,
    keep: [usize; 2],
) {
    texts.retain(|&slot, _| keep.contains(&slot) || in_window(slot, center));
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

/// A version in the self-test dump: its short hash, or `local:` and why
/// it was kept.
fn tag(version: &Version) -> String {
    match version.source {
        Source::Commit => short(&version.commit).to_owned(),
        Source::Local { reason, .. } => format!("local:{}", reason_words(reason)),
    }
}

/// Why a local version was kept, as the strip says it.
pub(super) fn reason_words(reason: Reason) -> &'static str {
    match reason {
        Reason::Opened => "opened",
        Reason::Saved => "saved",
        Reason::Idle => "idle",
        Reason::Reloaded => "before reload",
        Reason::ClosedUnsaved => "closed without saving",
        Reason::Quit => "at quit",
        Reason::BeforeRestore => "before restore",
        Reason::EditedOnDisk => "edited on disk",
    }
}

/// How many versions `versions` are, by kind: "5 commits", "2 local
/// versions", "7 versions". `more` follows the number.
fn counted(versions: &[Version], more: &str) -> String {
    let n = versions.len();
    let local = versions.iter().filter(|v| v.is_local()).count();
    let kind = match local {
        0 => "commit",
        _ if local == n => "local version",
        _ => "version",
    };
    format!("{n}{more} {kind}{}", if n == 1 { "" } else { "s" })
}

/// Which versions a history steps through. The window keeps the last one
/// picked for the next history it opens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Filter {
    #[default]
    All,
    Commits,
    Local,
}

impl Filter {
    fn keeps(self, version: &Version) -> bool {
        match self {
            Filter::All => true,
            Filter::Commits => !version.is_local(),
            Filter::Local => version.is_local(),
        }
    }

    /// The next one the control goes to.
    fn next(self) -> Filter {
        match self {
            Filter::All => Filter::Commits,
            Filter::Commits => Filter::Local,
            Filter::Local => Filter::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Commits => "Commits",
            Filter::Local => "Local",
        }
    }
}

/// The versions of `all` that `filter` keeps, and the filter that kept
/// them: every version when it would keep none, so a history never opens
/// on nothing. The window and the worker both pick through this, so their
/// slots agree.
pub(super) fn visible(all: &[Version], filter: Filter) -> (Vec<Version>, Filter) {
    let kept: Vec<Version> = all.iter().filter(|v| filter.keeps(v)).cloned().collect();
    if kept.is_empty() && filter != Filter::All {
        (all.to_vec(), Filter::All)
    } else {
        (kept, filter)
    }
}

/// Where a filter change leaves the version the tab shows, `shown` of
/// `before`, in `after`: its own slot when kept, and whether it was. One
/// left out gives way to the next newer one kept, or the working tree.
pub(super) fn reslot(
    all: &[Version],
    before: &[Version],
    after: &[Version],
    shown: usize,
) -> (usize, bool) {
    let Some(version) = before.get(shown) else {
        return (after.len(), true);
    };
    if let Some(slot) = after.iter().position(|v| v == version) {
        return (slot, true);
    }
    let from = all.iter().position(|v| v == version).unwrap_or(0);
    let newer = all[from..]
        .iter()
        .find_map(|v| after.iter().position(|a| a == v));
    (newer.unwrap_or(after.len()), false)
}

/// Where a history opened from the blame heat strip goes: a commit, by
/// its hash or a prefix of it, and the caret's line and the first line
/// shown in the file as that commit left it.
pub(super) struct Goto {
    pub(super) commit: String,
    pub(super) line: usize,
    pub(super) top: usize,
}

/// The version of `versions` made by `commit` (a hash, or a prefix of
/// one), and what to say when there is none: the list was cut short
/// before it, and the oldest read stands in, or the file's history does
/// not have it, and the newest does.
pub(super) fn goto_slot(
    versions: &[Version],
    commit: &str,
    capped: bool,
) -> (usize, Option<String>) {
    let found = versions.iter().position(|v| {
        !commit.is_empty()
            && !v.is_local()
            && (v.commit.starts_with(commit) || commit.starts_with(&v.commit))
    });
    match found {
        Some(slot) => (slot, None),
        None if capped => (
            0,
            Some(format!(
                "commit {} is past the loaded history",
                short(commit)
            )),
        ),
        None => (
            versions.len().saturating_sub(1),
            Some(format!(
                "commit {} is not in this file's history",
                short(commit)
            )),
        ),
    }
}

impl Scrub {
    /// Whether `id` is the file's tab or the history's.
    pub(super) fn involves(&self, id: u64) -> bool {
        self.live == id || self.sibling == Some(id)
    }

    /// Whether the strip shows above tab `active`: the history's own, or
    /// the file's while the versions are listed and there is no tab yet.
    pub(super) fn strip_over(&self, active: u64) -> bool {
        self.sibling == Some(active) || (self.sibling.is_none() && self.live == active)
    }

    /// Something is on its way that the display link has to poll for.
    pub(super) fn busy(&self) -> bool {
        !self.gone
            && (self.versions.is_none()
                || self.pending.is_some()
                || (self.sibling.is_some()
                    && self.shown > 0
                    && !self.diffs.contains_key(&(self.shown - 1)))
                || self.wants_tree_diff())
    }

    /// The marks are against the working tree, and the version shown's
    /// changes from it are not here yet.
    fn wants_tree_diff(&self) -> bool {
        self.against_tree
            && self.sibling.is_some()
            && self.pending.is_none()
            && self.shown < self.count()
            && !self.tree_diffs.contains_key(&self.shown)
    }

    /// What the gutter marks on the version shown: what it changed against
    /// the version before it, or against the working tree; `None` while
    /// that is on its way. The oldest version has no parent read, and no
    /// marks rather than every line added; the working tree none against
    /// itself.
    fn shown_marks(&self) -> Option<Vec<Change>> {
        if self.against_tree {
            if self.shown >= self.count() {
                return Some(Vec::new());
            }
            return self.tree_diffs.get(&self.shown).map(|c| c.to_vec());
        }
        match self.shown.checked_sub(1) {
            None => Some(Vec::new()),
            Some(pair) => self.diffs.get(&pair).map(|c| c.to_vec()),
        }
    }

    fn count(&self) -> usize {
        self.versions.as_ref().map_or(0, Vec::len)
    }

    /// The version the window is loaded around.
    fn center(&self) -> usize {
        self.pending.unwrap_or(self.shown)
    }

    /// Whether version `target` can be shown, coming from `from`: its text
    /// is here, and so are the changes to it, the diff from a neighbour or
    /// the worker's for a jump. Until they are, a drag along the timeline
    /// leaves the version shown where it is, rather than replace the whole
    /// text and parse it again on every frame.
    fn ready(&self, from: usize, target: usize) -> bool {
        self.texts.contains_key(&target)
            && match from.abs_diff(target) {
                0 => true,
                1 => self.diffs.contains_key(&from.min(target)) || self.gone,
                _ => self.jump_changes(from, target).is_some() || self.gone,
            }
    }

    /// The worker's changes for a jump from `from` to `target`, if here.
    fn jump_changes(&self, from: usize, target: usize) -> Option<&Arc<Vec<Change>>> {
        self.jump
            .as_ref()
            .filter(|(a, b, _)| (*a, *b) == (from, target))
            .map(|(_, _, changes)| changes)
    }

    /// Asks the worker for what the window lacks, and lets go of what
    /// fell out of it. A jump asks for its changes too, right after the
    /// text it goes to.
    fn request(&mut self) {
        let center = self.center();
        let count = self.count();
        let shown = self.shown;
        evict(&mut self.texts, &mut self.diffs, center, [count, shown]);
        let texts = &self.texts;
        self.unreadable.retain(|slot| texts.contains_key(slot));
        self.tree_diffs
            .retain(|&slot, _| slot == shown || in_window(slot, center));
        let mut jobs = jobs(
            center,
            count + 1,
            |slot| self.texts.contains_key(&slot),
            |pair| self.diffs.contains_key(&pair),
        );
        let jump = self.sibling.is_some() && center.abs_diff(shown) > 1;
        if jump && self.jump_changes(shown, center).is_none() {
            let at = jobs
                .iter()
                .position(|job| *job == Job::Text(center))
                .map_or(0, |i| i + 1);
            jobs.insert(at, Job::Jump(shown, center));
        }
        // The version shown is here: its marks come first.
        if self.wants_tree_diff() {
            jobs.insert(0, Job::Tree(shown));
        }
        if !jobs.is_empty() {
            let _ = self.tx.send(Request::Want {
                center,
                shown,
                jobs,
            });
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
        self.tree_diffs.clear();
        self.tree_generation += 1;
        let _ = self.tx.send(Request::Worktree(text, self.tree_generation));
    }

    /// Steps through the versions `filter` keeps from here on. The version
    /// shown stays when kept; else the next newer one kept replaces it.
    /// What was loaded is numbered the old way, and goes but for the
    /// working tree and the version shown; the worker starts again on a
    /// channel of its own, so nothing numbered the old way arrives after.
    fn set_filter(&mut self, filter: Filter) {
        let Some(before) = self.versions.take() else {
            return;
        };
        let (after, filter) = visible(&self.all, filter);
        self.filter = filter;
        let (slot, kept) = reslot(&self.all, &before, &after, self.shown);
        let text = self.texts.remove(&self.shown);
        let unreadable = self.unreadable.contains(&self.shown);
        let worktree = self.texts.remove(&before.len());
        self.texts.clear();
        self.diffs.clear();
        self.unreadable.clear();
        self.tree_diffs.clear();
        if let Some(worktree) = worktree {
            self.texts.insert(after.len(), worktree);
        }
        self.shown = slot;
        self.pending = None;
        self.fade = None;
        self.jump = None;
        if self.sibling.is_none() {
            // The first version was still on its way: the newest now.
            self.pending = Some(after.len().saturating_sub(1));
        } else if kept {
            if let Some(text) = text {
                self.texts.insert(slot, text);
            }
            if unreadable {
                self.unreadable.insert(slot);
            }
        } else {
            self.refill = true;
            self.pending = Some(slot);
        }
        let (tx, rx) = mpsc::channel();
        self.rx = rx;
        let _ = self.tx.send(Request::Filter {
            versions: after.clone(),
            tx,
        });
        self.versions = Some(after);
        self.request();
    }

    fn label(&self, slot: usize) -> String {
        match self.versions.as_ref().and_then(|v| v.get(slot)) {
            Some(Version {
                source: Source::Local { reason, .. },
                time,
                ..
            }) => format!(
                "{} @ {} {}",
                self.name,
                reason_words(*reason),
                crate::platform::clock_time(*time, crate::platform::unix_seconds() as i64)
            ),
            Some(version) => format!("{} @ {}", self.name, short(&version.commit)),
            None => format!("{} @ working tree", self.name),
        }
    }

    /// Whether the version shown can be restored, and why not.
    pub(super) fn restorable(&self) -> Result<(), String> {
        if self.versions.is_none() || self.sibling.is_none() {
            return Err(format!("reading the history of {}", self.name));
        }
        if let Some(target) = self.pending {
            return Err(format!(
                "version {} of {} is still loading",
                target + 1,
                self.count()
            ));
        }
        if self.shown >= self.count() {
            return Err("this is the working tree: there is nothing to restore".into());
        }
        if self.unreadable.contains(&self.shown) || !self.texts.contains_key(&self.shown) {
            return Err("this version could not be read, so it cannot be restored".into());
        }
        Ok(())
    }

    /// What the status line says of version `slot`.
    fn describe(&self, slot: usize) -> String {
        let count = self.count();
        let more = if self.capped { "+" } else { "" };
        let now = crate::platform::unix_seconds() as i64;
        match self.versions.as_ref().and_then(|v| v.get(slot)) {
            Some(Version {
                source: Source::Local { reason, .. },
                time,
                ..
            }) => format!(
                "local version, {}, {} ({} of {count}{more})",
                reason_words(*reason),
                crate::platform::clock_time(*time, now),
                slot + 1,
            ),
            Some(version) => format!(
                "{} {}, {}: {} ({} of {count}{more})",
                short(&version.commit),
                version.author,
                crate::project::git::ago(version.time, now),
                version.summary,
                slot + 1,
            ),
            None => format!(
                "working tree, after {}",
                counted(self.versions.as_deref().unwrap_or_default(), more)
            ),
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
        Some(version) => format!("{}/{}@{}", scrub.shown + 1, versions.len(), tag(version)),
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

/// What the self-test dump says of the filter: which, how many versions it
/// keeps of how many, and each kept one's tag, oldest first.
pub(super) fn dump_filter(state: &State) -> String {
    let Some(scrub) = &state.scrub else {
        return "closed".into();
    };
    let Some(versions) = &scrub.versions else {
        return format!("{} listing", scrub.filter.label().to_lowercase());
    };
    let tags: Vec<String> = versions.iter().map(tag).collect();
    format!(
        "{} {}/{} {}",
        scrub.filter.label().to_lowercase(),
        versions.len(),
        scrub.all.len(),
        tags.join(",")
    )
}

/// What the self-test dump says of the marks: what they are against, and
/// whether the version shown can be restored or why not.
pub(super) fn dump_compare(state: &State) -> String {
    let Some(scrub) = &state.scrub else {
        return "closed".into();
    };
    let against = if scrub.against_tree {
        "working tree"
    } else {
        "previous"
    };
    let ready = if scrub.against_tree {
        !scrub.wants_tree_diff()
    } else {
        scrub.shown_marks().is_some()
    };
    let restore = match scrub.restorable() {
        Ok(()) => "ready".to_string(),
        Err(why) => why,
    };
    format!("{against} marks={ready} restore={restore}")
}

// ---- the fade ---------------------------------------------------------------
// What a step changed, marked over the version it goes to and faded out.
// macOS has a Reduce Motion setting, which nothing in crc reads yet; a
// fade this short and this faint is drawn either way.

/// How long what a step changed stays marked.
const FADE: Duration = Duration::from_millis(150);
/// Removed lines a ghost shows; past these it says how many more.
const GHOST_LINES: usize = 3;

/// What a step changed: the lines it added or changed in the version it
/// goes to, and the ones it removed, where they were.
pub(super) struct Fade {
    started: Instant,
    /// Held at this point, for a self-test's frames.
    frozen: Option<Duration>,
    tints: Vec<(std::ops::Range<usize>, MarkKind)>,
    ghosts: Vec<layout::Ghost>,
}

impl Fade {
    /// From `changes`, from the version shown to the one a step goes to,
    /// and `old`, the text of the one shown.
    fn new(changes: &[Change], old: &str, started: Instant) -> Self {
        let mut tints = Vec::new();
        let mut ghosts = Vec::new();
        let mut lines: Option<Vec<&str>> = None;
        for change in changes {
            if change.kind != MarkKind::Removed {
                tints.push((change.new_start..change.new_end, change.kind));
                continue;
            }
            let lines = lines.get_or_insert_with(|| old.split_inclusive('\n').collect());
            let removed = change.old_end - change.old_start;
            ghosts.push(layout::Ghost {
                at: change.new_start,
                lines: lines
                    .iter()
                    .skip(change.old_start)
                    .take(removed.min(GHOST_LINES))
                    .map(|line| layout::expanded_line(line))
                    .collect(),
                more: removed.saturating_sub(GHOST_LINES),
            });
        }
        Fade {
            started,
            frozen: None,
            tints,
            ghosts,
        }
    }

    fn elapsed(&self, now: Instant) -> Duration {
        self.frozen
            .unwrap_or_else(|| now.saturating_duration_since(self.started))
    }

    /// How strongly the marks show at `now`; `None` once the fade is over.
    fn alpha(&self, now: Instant) -> Option<f32> {
        let t = self.elapsed(now).as_secs_f32() / FADE.as_secs_f32();
        (t < 1.0).then(|| fade_alpha(t))
    }
}

/// From 1 at the step to 0 at the end, `t` the fraction of the fade gone:
/// an ease-out, most of the change early and the rest settling.
fn fade_alpha(t: f32) -> f32 {
    let left = 1.0 - t.clamp(0.0, 1.0);
    left * left
}

/// `changes` from one text to another as the changes back: what was added
/// is removed, and the other way round.
fn reversed(changes: &[Change]) -> Vec<Change> {
    changes
        .iter()
        .map(|c| Change {
            new_start: c.old_start,
            new_end: c.old_end,
            old_start: c.new_start,
            old_end: c.new_end,
            kind: match c.kind {
                MarkKind::Added => MarkKind::Removed,
                MarkKind::Removed => MarkKind::Added,
                MarkKind::Modified => MarkKind::Modified,
            },
        })
        .collect()
}

/// The wash behind lines of `kind` at `alpha`. There is no band for a
/// changed line: its gutter mark's colour, as strong as the added band.
fn tint_colour(kind: MarkKind, theme: &Theme, alpha: f32) -> [f32; 4] {
    let band = match kind {
        MarkKind::Modified => {
            let [r, g, b, _] = theme.diff_modified;
            [r, g, b, theme.diff_added_band[3]]
        }
        _ => theme.diff_added_band,
    };
    layout::faded(band, alpha)
}

impl Scrub {
    /// What a step from version `from` to `target` changes, from the
    /// neighbours' diff or the worker's for a jump; `None` when it is not
    /// here. None of it is worked out on this thread.
    fn step_changes(&self, from: usize, target: usize) -> Option<Vec<Change>> {
        match from.abs_diff(target) {
            0 => Some(Vec::new()),
            1 => {
                let pair = self.diffs.get(&from.min(target))?;
                Some(if target > from {
                    pair.to_vec()
                } else {
                    reversed(pair)
                })
            }
            _ => self.jump_changes(from, target).map(|c| c.to_vec()),
        }
    }

    /// The fade of a step from version `from` that makes `changes`;
    /// `None` when it changes nothing.
    fn fade_for(&self, from: usize, changes: &[Change]) -> Option<Fade> {
        let old = self.texts.get(&from)?;
        let fade = Fade::new(changes, old, Instant::now());
        (!fade.tints.is_empty() || !fade.ghosts.is_empty()).then_some(fade)
    }

    /// A step's fade is on screen: the frames come at the display's pace.
    pub(super) fn fading(&self) -> bool {
        self.fade
            .as_ref()
            .is_some_and(|f| f.alpha(Instant::now()).is_some())
    }
}

/// The fade over the history's tab, when `buffer` is it: the washes under
/// the text, which go in at the front of `out` as the conflicts' do, and
/// the removed lines over it. Asks for the next frame until it is over,
/// then lets it go.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_fade(
    scrub: &mut Option<Scrub>,
    buffer: &Buffer,
    atlas: &mut Atlas,
    text: Viewport,
    rows: &[layout::ScreenRow],
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    let Some(scrub) = scrub.as_mut() else {
        return;
    };
    let Some(fade) = &scrub.fade else {
        return;
    };
    let Some(alpha) = fade.alpha(Instant::now()) else {
        scrub.fade = None;
        return;
    };
    if scrub.sibling != Some(buffer.id()) {
        return;
    }
    layout::keep_animating();
    let tints: Vec<_> = fade
        .tints
        .iter()
        .map(|(lines, kind)| (lines.clone(), tint_colour(*kind, theme, alpha)))
        .collect();
    out.splice(0..0, layout::line_tints(rows, atlas, text, &tints));
    for ghost in &fade.ghosts {
        layout::push_ghost(out, atlas, buffer, text, rows, ghost, alpha, theme);
    }
}

/// What the self-test dump says of the fade: how far along, and how many
/// washes and ghosts it draws.
pub(super) fn dump_fade(state: &State) -> String {
    let fade = state.scrub.as_ref().and_then(|s| s.fade.as_ref());
    match fade {
        Some(fade) if fade.alpha(Instant::now()).is_some() => format!(
            "t={}ms, tints={} ghosts={}",
            fade.elapsed(Instant::now()).as_millis(),
            fade.tints.len(),
            fade.ghosts.len()
        ),
        _ => "none".into(),
    }
}

/// A self-test holds the fade `ms` in, to take its frames there. It stays
/// until the next step replaces it.
pub(super) fn freeze_fade(state: &mut State, ms: u64) {
    match state.scrub.as_mut().and_then(|s| s.fade.as_mut()) {
        Some(fade) => fade.frozen = Some(Duration::from_millis(ms)),
        None => eprintln!("selftest: no fade to hold"),
    }
}

// ---- the strip --------------------------------------------------------------
// Above the history's tab: which version it is, Older and Newer, the
// filter, what the gutter marks against, a timeline of every version with
// the working tree at its right end, to click or drag along, and Restore.

const OLDER: &str = "Older";
const NEWER: &str = "Newer";
const COMPARE: &str = "vs Working Tree";
const RESTORE: &str = "Restore";

/// The history whose strip is above the active tab.
pub(super) fn strip_scrub(state: &State) -> Option<&Scrub> {
    let active = state.docs.active().id();
    state.scrub.as_ref().filter(|s| s.strip_over(active))
}

/// Where the strip's controls are.
pub(super) struct StripRects {
    pub(super) older: Viewport,
    pub(super) newer: Viewport,
    /// All, Commits or Local; as wide as the widest, so the timeline does
    /// not move when it changes.
    pub(super) filter: Viewport,
    /// Lit while the gutter marks against the working tree.
    pub(super) compare: Viewport,
    /// At the second row's right end.
    pub(super) restore: Viewport,
    pub(super) close: Viewport,
    /// The rest of the second row, between the toggle and Restore.
    pub(super) track: Viewport,
}

pub(super) fn strip_rects(atlas: &mut Atlas, strip: Viewport) -> StripRects {
    let older = layout::strip_button(atlas, strip, strip.x + 16.0, OLDER, 24.0);
    let newer = layout::strip_button(
        atlas,
        strip,
        older.x + older.width + layout::UI_GAP,
        NEWER,
        24.0,
    );
    let widest = [Filter::All, Filter::Commits, Filter::Local]
        .into_iter()
        .max_by(|a, b| {
            let (a, b) = (
                layout::ui_text_width(atlas, a.label()),
                layout::ui_text_width(atlas, b.label()),
            );
            a.total_cmp(&b)
        })
        .unwrap_or_default();
    let filter = layout::strip_button(
        atlas,
        strip,
        newer.x + newer.width + layout::UI_GAP,
        widest.label(),
        24.0,
    );
    let compare = layout::strip_button(
        atlas,
        strip,
        filter.x + filter.width + layout::UI_GAP,
        COMPARE,
        24.0,
    );
    let restore = layout::strip_button(atlas, strip, 0.0, RESTORE, 24.0);
    let restore = Viewport {
        x: strip.x + strip.width - 16.0 - restore.width,
        ..restore
    };
    // On the first row, level with the pill.
    let close = Viewport {
        x: strip.x + strip.width - 16.0 - layout::UI_CONTROL,
        y: strip.y + 6.0,
        width: layout::UI_CONTROL,
        height: layout::UI_CONTROL,
    };
    let left = compare.x + compare.width + layout::UI_INSET;
    let track = Viewport {
        x: left,
        width: (restore.x - layout::UI_INSET - left).max(0.0),
        ..older
    };
    StripRects {
        older,
        newer,
        filter,
        compare,
        restore,
        close,
        track,
    }
}

/// Whether Older and Newer have somewhere to go.
fn steps_open(scrub: &Scrub) -> (bool, bool) {
    let at = scrub.center();
    let listed = scrub.versions.is_some();
    (listed && at > 0, listed && at < scrub.count())
}

/// Whether the filter has a choice to make: the history has commits and
/// local versions both.
fn filter_open(scrub: &Scrub) -> bool {
    scrub.versions.is_some()
        && scrub.all.iter().any(|v| v.is_local())
        && scrub.all.iter().any(|v| !v.is_local())
}

/// The strip's clickable parts: the close button always, the rest once
/// the versions are listed.
pub(super) fn strip_hits(
    scrub: &Scrub,
    atlas: &mut Atlas,
    strip: Viewport,
) -> Vec<(Hit, Viewport)> {
    let rects = strip_rects(atlas, strip);
    let mut hits = vec![(Hit::ScrubClose, rects.close)];
    if scrub.versions.is_some() {
        hits.extend([
            (Hit::ScrubOlder, rects.older),
            (Hit::ScrubNewer, rects.newer),
            (Hit::ScrubFilter, rects.filter),
            (Hit::ScrubCompare, rects.compare),
            (Hit::ScrubRestore, rects.restore),
            (Hit::ScrubTrack, rects.track),
        ]);
    }
    hits
}

/// What the first row says: the facts, the summary apart so it can be
/// cut to the room left, and the place at the right.
struct StripText {
    lead: String,
    summary: String,
    right: String,
}

/// The version the thumb is on, which is the one a step or a drag is
/// waiting for while its text is read.
fn strip_text(scrub: &Scrub, now: i64) -> StripText {
    let more = if scrub.capped { "+" } else { "" };
    let Some(versions) = &scrub.versions else {
        return StripText {
            lead: format!("Reading the history of {}", scrub.name),
            summary: String::new(),
            right: "loading\u{2026}".into(),
        };
    };
    let count = versions.len();
    let at = scrub.center();
    match versions.get(at) {
        Some(Version {
            source: Source::Local { reason, .. },
            time,
            ..
        }) => StripText {
            lead: format!(
                "{}, {}   ",
                reason_words(*reason),
                crate::platform::clock_time(*time, now)
            ),
            summary: String::new(),
            right: format!("{} of {count}{more}", at + 1),
        },
        Some(version) => StripText {
            lead: format!(
                "{}   {}, {}   ",
                short(&version.commit),
                version.author,
                crate::project::git::ago(version.time, now)
            ),
            summary: version.summary.clone(),
            right: format!("{} of {count}{more}", at + 1),
        },
        None => StripText {
            lead: format!(
                "{} as open in its tab, after {}",
                scrub.name,
                counted(versions, more)
            ),
            summary: String::new(),
            right: "working tree".into(),
        },
    }
}

/// What the pointer resting on slot `slot` of the timeline is told.
fn slot_tip(scrub: &Scrub, slot: usize, now: i64) -> String {
    match scrub.versions.as_ref().and_then(|v| v.get(slot)) {
        Some(Version {
            source: Source::Local { reason, .. },
            time,
            ..
        }) => format!(
            "Local version  {}, {}",
            reason_words(*reason),
            crate::platform::clock_time(*time, now)
        ),
        Some(version) => format!(
            "{}  {}, {}\n{}",
            short(&version.commit),
            version.author,
            crate::project::git::ago(version.time, now),
            version.summary
        ),
        None => format!("Working tree: {} as open in its tab", scrub.name),
    }
}

/// The pill, the facts, the place, the buttons and the timeline.
pub(super) fn draw_strip(
    scrub: &Scrub,
    atlas: &mut Atlas,
    strip: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    let now = crate::platform::unix_seconds() as i64;
    // A local version has a pill of its own, in the local ticks' colour.
    let local = scrub
        .versions
        .as_ref()
        .and_then(|v| v.get(scrub.center()))
        .is_some_and(Version::is_local);
    let pill = if local {
        layout::strip_pill(out, atlas, strip, "Local", theme.diff_modified)
    } else {
        layout::strip_pill(out, atlas, strip, "History", theme.accent)
    };
    let rects = strip_rects(atlas, strip);
    let text = strip_text(scrub, now);
    let left = pill.x + pill.width + 12.0;
    let right_width = layout::ui_text_width(atlas, &text.right);
    layout::push_ui_text_right(
        out,
        atlas,
        Viewport {
            x: left,
            width: (rects.close.x - 8.0 - left).max(0.0),
            ..pill
        },
        &text.right,
        theme.status_text,
    );
    // The facts stop short of the place; the summary gives way first.
    let room = (rects.close.x - 8.0 - right_width - 16.0 - left).max(0.0);
    let lead_width = layout::ui_text_width(atlas, &text.lead);
    let facts = if lead_width >= room {
        crate::platform::inspector::fit(atlas, text.lead.trim_end(), room)
    } else {
        // Measured widths stop at 120 characters: cut a long one first.
        let summary: String = text.summary.chars().take(100).collect();
        let summary = crate::platform::inspector::fit(atlas, &summary, room - lead_width);
        format!("{}{summary}", text.lead)
    };
    layout::strip_facts(out, atlas, strip, pill, &facts, theme);
    let (older, newer) = steps_open(scrub);
    Button::new(rects.older)
        .label(OLDER)
        .enabled(older)
        .tip("Older Version  \u{2190}")
        .draw(out, atlas, theme);
    Button::new(rects.newer)
        .label(NEWER)
        .enabled(newer)
        .tip("Newer Version  \u{2192}")
        .draw(out, atlas, theme);
    // Lit while it leaves some versions out.
    Button::new(rects.filter)
        .label(scrub.filter.label())
        .enabled(filter_open(scrub))
        .on(scrub.filter != Filter::All)
        .tip("Show All Versions, Commits or Local Versions")
        .draw(out, atlas, theme);
    Button::new(rects.compare)
        .label(COMPARE)
        .enabled(scrub.versions.is_some())
        .on(scrub.against_tree)
        .tip(if scrub.against_tree {
            "Marking Changes Against the Working Tree"
        } else {
            "Mark Changes Against the Working Tree, Not the Version Before"
        })
        .draw(out, atlas, theme);
    Button::new(rects.restore)
        .label(RESTORE)
        .tone(Tone::Primary)
        .enabled(scrub.restorable().is_ok())
        .tip("Restore This Version")
        .draw(out, atlas, theme);
    Button::new(rects.close)
        .icon(crate::project::icons::CLOSE)
        .tone(Tone::Ghost)
        .tip("Back to the File  \u{238b}")
        .draw(out, atlas, theme);
    draw_timeline(scrub, atlas, rects.track, theme, now, out);
    layout::strip_hairline(out, atlas, strip, theme);
}

/// The timeline: a line with a tick per commit, brighter where the text
/// is already read, a dot per local version, a ring for the working tree
/// at the right end, and the thumb on the version shown. Too many versions
/// for a tick each, and it is a band instead, brighter over the versions
/// read, with a mark over each run of local versions. While the versions
/// are listed, a bar runs along it.
fn draw_timeline(
    scrub: &Scrub,
    atlas: &mut Atlas,
    track: Viewport,
    theme: &Theme,
    now: i64,
    out: &mut Vec<GlyphInstance>,
) {
    if track.width <= 2.0 * layout::TIMELINE_INSET {
        return;
    }
    let m = atlas.metrics;
    let mid = track.y + (track.height * 0.5).round();
    let ink = theme.status_text;
    let Some(count) = scrub.versions.as_ref().map(Vec::len) else {
        let line = Viewport {
            x: track.x + layout::TIMELINE_INSET,
            y: mid - 1.0,
            width: track.width - 2.0 * layout::TIMELINE_INSET,
            height: 2.0,
        };
        layout::push_rounded_rect(out, line, 1.0, layout::faded(ink, 0.25));
        layout::push_progress(out, atlas, line, theme);
        return;
    };
    let slots = count + 1;
    let x_of = |slot: usize| layout::timeline_x(track, slots, slot);
    let first = x_of(0);
    let last = x_of(count);
    let dense = layout::timeline_dense(track, slots);
    if dense {
        let band = |out: &mut Vec<GlyphInstance>, from: f32, to: f32, colour| {
            layout::push_rounded_rect(
                out,
                Viewport {
                    x: from - 2.0,
                    y: mid - 2.0,
                    width: to - from + 4.0,
                    height: 4.0,
                },
                2.0,
                colour,
            )
        };
        band(out, first, last, layout::faded(ink, 0.22));
        // The versions read: a few runs, at most the window around the
        // one shown and the working tree.
        let mut read: Vec<usize> = scrub.texts.keys().copied().filter(|&s| s < count).collect();
        read.sort_unstable();
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for slot in read {
            match runs.last_mut() {
                Some((_, end)) if *end + 1 == slot => *end = slot,
                _ => runs.push((slot, slot)),
            }
        }
        for (from, to) in runs {
            band(out, x_of(from), x_of(to), layout::faded(ink, 0.5));
        }
        // Local versions: a mark a little taller than the band, in the
        // colour of their pill, one per run of them side by side.
        let versions = scrub.versions.as_deref().unwrap_or_default();
        let mut slot = 0;
        while slot < count {
            if !versions[slot].is_local() {
                slot += 1;
                continue;
            }
            let from = slot;
            while slot + 1 < count && versions[slot + 1].is_local() {
                slot += 1;
            }
            let left = m.snap(x_of(from) - 0.5);
            layout::push_rect(
                out,
                atlas,
                [left, mid - 3.0],
                [(m.snap(x_of(slot) + 0.5) - left).max(1.0), 6.0],
                theme.diff_modified,
            );
            slot += 1;
        }
    } else {
        layout::push_rect(
            out,
            atlas,
            [first, mid - 1.0],
            [last - first, 2.0],
            layout::faded(ink, 0.22),
        );
        // A local version is a dot on the line in the colour of its pill,
        // under the commits' ticks so a commit beside it keeps its height.
        let versions = scrub.versions.as_deref().unwrap_or_default();
        for slot in 0..count {
            if versions.get(slot).is_some_and(Version::is_local) {
                let centre = m.snap(x_of(slot) - 0.5) + 0.5;
                let dot = Viewport {
                    x: centre - 2.5,
                    y: mid - 2.5,
                    width: 5.0,
                    height: 5.0,
                };
                layout::push_rounded_rect(out, dot, 2.5, theme.diff_modified);
            }
        }
        for slot in 0..count {
            if versions.get(slot).is_some_and(Version::is_local) {
                continue;
            }
            let alpha = if scrub.texts.contains_key(&slot) {
                0.85
            } else {
                0.4
            };
            layout::push_rect(
                out,
                atlas,
                [m.snap(x_of(slot) - 0.5), mid - 5.0],
                [1.0, 10.0],
                layout::faded(ink, alpha),
            );
        }
    }
    // The working tree: a ring, not a tick, as it is no commit.
    let ground = theme.background.map(|c| c as f32);
    let ring = |size: f32| Viewport {
        x: m.snap(last - size * 0.5),
        y: mid - size * 0.5,
        width: size,
        height: size,
    };
    layout::push_rounded_rect(out, ring(10.0), 5.0, layout::faded(ink, 0.9));
    layout::push_rounded_rect(out, ring(6.0), 3.0, ground);
    // Under the pointer: that version brighter, and what it is said when
    // the pointer rests there.
    layout::hotspot(track, layout::Cursor::Pointing, None);
    if let Some((x, _)) = layout::pointer().filter(|_| layout::hovered(track)) {
        let slot = layout::timeline_slot_at(track, slots, x);
        if slot < count {
            layout::push_rect(
                out,
                atlas,
                [m.snap(x_of(slot) - 0.5), mid - 7.0],
                [1.0, 14.0],
                theme.text,
            );
        }
        layout::hotspot(
            layout::timeline_column(track, slots, slot),
            layout::Cursor::Pointing,
            Some(&slot_tip(scrub, slot, now)),
        );
    }
    // The thumb, dimmer while the version it is on is read.
    let thumb = Viewport {
        x: m.snap(x_of(scrub.center()) - 3.0),
        y: mid - 9.0,
        width: 6.0,
        height: 18.0,
    };
    let fill = if scrub.pending.is_some() {
        layout::faded(theme.accent, 0.55)
    } else {
        theme.accent
    };
    layout::push_rounded_rect(out, thumb, 3.0, fill);
}

/// What the self-test dump says of the strip: its facts, uncut, its
/// place, and which steps it offers.
pub(super) fn dump_strip(state: &State) -> String {
    let Some(scrub) = strip_scrub(state) else {
        return "none".into();
    };
    if chrome_of(state).response.is_none() {
        return "hidden".into();
    }
    let text = strip_text(scrub, crate::platform::unix_seconds() as i64);
    let (older, newer) = steps_open(scrub);
    format!(
        "{} | {} | older={older} newer={newer}",
        format!("{}{}", text.lead, text.summary).trim_end(),
        text.right
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
    // The first version comes from the working tree, unless the history
    // was opened at it with its lines already.
    let from = match (made, scrub.placed) {
        (true, _) => scrub.shown,
        (false, true) => target,
        (false, false) => count,
    };
    if !scrub.ready(from, target) {
        return None;
    }
    let text = scrub.texts.get(&target)?.clone();
    // What the step changes, faded out over the version it shows and
    // handed to the parser as an edit per change: none for the first,
    // which only opens the tab. A step during a fade starts its own;
    // nothing queues.
    // Nor any after a filter change took the version shown away: the tab's
    // text is no version here, and is replaced whole.
    let changes = if made && !scrub.refill {
        scrub.step_changes(from, target)
    } else {
        None
    };
    let fade = changes.as_deref().and_then(|c| scrub.fade_for(from, c));
    let label = scrub.label(target);
    let ((line, column), top) = match scrub.sibling.and_then(|id| buffer_by_id(docs, panes, id)) {
        Some(buffer) => (buffer.cursor_position(), buffer.scroll_line),
        None => ((scrub.start.0, scrub.start.1), scrub.start.2),
    };
    // Through the step's changes. Without them, which is only when the
    // worker stopped, through every diff between the two, and when one of
    // those is missing the caret and the view keep their line numbers,
    // clamped to the version's length.
    let map = |line| match &changes {
        Some(changes) => crate::ide::diff::map_line(changes, line),
        None => map_through(&scrub.diffs, from, target, line).unwrap_or(line),
    };
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
    buffer.show_text(&text, changes.as_deref(), line, column);
    let top = top.min(buffer.rope.len_lines().saturating_sub(1));
    if top != buffer.scroll_line {
        buffer.scroll_line = top;
        buffer.scroll_fraction = 0.0;
        buffer.scroll_row = 0;
    }
    let rope = buffer.rope.clone();
    scrub.shown = target;
    scrub.pending = None;
    scrub.refill = false;
    scrub.fade = fade;
    scrub.jump = None;
    // What this version changed, against its parent or the working tree.
    // No HEAD text: nothing here acts on the changes, and nothing reads
    // HEAD for a document without a path.
    let changes = scrub.shown_marks().unwrap_or_default();
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
    let said = match scrub.note.take() {
        Some(note) => format!("{note}: {}", scrub.describe(target)),
        None => scrub.describe(target),
    };
    Some((said, !made))
}

/// Restore This Version: the version shown replaces the text of the file's
/// own tab, as one edit that Undo takes back, and is not saved. What the
/// tab held is kept in local history. The caret and the view go where they
/// are in the history's tab, which holds the same text: the line looked at
/// is the line landed on. Then the history closes and the file's tab is in
/// front, as after Escape: the restore ends the look back, and a history
/// left open would show a working tree that is no longer the file's.
/// Returns what to say, or why nothing was restored.
fn restore(state: &mut State) -> Result<String, String> {
    let active = state.docs.active().id();
    let scrub = state
        .scrub
        .as_ref()
        .filter(|s| s.sibling == Some(active))
        .ok_or("File History is not open in this tab")?;
    scrub.restorable()?;
    let unread = || "this version could not be read, so it cannot be restored".to_string();
    let text = scrub.texts.get(&scrub.shown).cloned().ok_or_else(unread)?;
    let (label, name, live) = (scrub.label(scrub.shown), scrub.name.clone(), scrub.live);
    let ((line, column), top) = buffer_by_id(&state.docs, &state.panes, active)
        .map(|b| (b.cursor_position(), b.scroll_line))
        .ok_or_else(unread)?;
    let State {
        docs,
        panes,
        history,
        ..
    } = &mut *state;
    let buffer =
        buffer_by_id_mut(docs, panes, live).ok_or_else(|| format!("{name} is no longer open"))?;
    if buffer.is_read_only() {
        return Err(format!("{name} is open read-only, so nothing was restored"));
    }
    let before = buffer.rope.clone();
    if !buffer.restore_text(&text, line, column) {
        return Err(format!("{name} already has this version's text"));
    }
    history.before_restore(buffer, &before);
    buffer.scroll_line = top.min(buffer.rope.len_lines().saturating_sub(1));
    buffer.scroll_fraction = 0.0;
    buffer.scroll_row = 0;
    close(state, true);
    Ok(format!("restored {label}, not saved: Undo takes it back"))
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
        self.open_history_at(None);
    }

    /// File History, at the version `goto` names when there is one.
    pub(super) fn open_history_at(&self, goto: Option<Goto>) {
        let opened = self.start_history_at(goto);
        if let Some(mut state) = self.state_mut() {
            match opened {
                Err(why) => state.message = Some((why, Instant::now())),
                // The history's tab takes the keys, as any tab switched to
                // does: a list open in the file would keep the arrows.
                Ok(()) => state.completion = None,
            }
        }
        self.sync_title();
        self.reparse();
        self.resume_display_link();
        self.request_redraw();
        self.pump();
    }

    fn start_history_at(&self, goto: Option<Goto>) -> Result<(), String> {
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
            // Open already: to the tab as it is, unless asked for a commit,
            // when it is opened again there.
            if scrub.live == id && goto.is_none() {
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
        // A file outside any repository may still have local versions:
        // the worker finds out, off this thread, and says so if not.
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
        // Opened at a commit from blame: not among the local versions.
        let filter = match state.scrub_filter {
            Filter::Local if goto.is_some() => Filter::All,
            filter => filter,
        };
        std::thread::spawn(move || {
            let from = Lister {
                db: crate::history::store::default_path(),
                filter,
                #[cfg(test)]
                answers: None,
            };
            work(&path, text, &from, &worker_rx, &worker_tx, &halt)
        });
        state.scrub = Some(Scrub {
            live: id,
            sibling: None,
            name: name.clone(),
            ext,
            versions: None,
            all: Vec::new(),
            filter,
            capped: false,
            shown: 0,
            refill: false,
            pending: None,
            texts: HashMap::new(),
            diffs: HashMap::new(),
            worktree,
            start,
            placed: goto.is_some(),
            goto,
            note: None,
            fade: None,
            jump: None,
            unreadable: std::collections::HashSet::new(),
            against_tree: state.scrub_compare,
            tree_diffs: HashMap::new(),
            tree_generation: 0,
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
                        Step::To(slot) => Some(slot.min(count)),
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
                        // The timeline on the version already there.
                        Some(_) if matches!(step, Step::To(_)) => return,
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

    /// A press or a drag on the timeline at `x`: the version under it. A
    /// drag asks for every version it crosses; the worker drops what an
    /// older request still had queued, so the last one is read first.
    pub(super) fn scrub_track_at(&self, x: f32) {
        let slot = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(strip) = chrome_of(&state).response else {
                return;
            };
            let state = &mut *state;
            let active = state.docs.active().id();
            let Some(scrub) = state.scrub.as_ref().filter(|s| s.sibling == Some(active)) else {
                return;
            };
            let Some(count) = scrub.versions.as_ref().map(Vec::len) else {
                return;
            };
            let track = strip_rects(&mut state.renderer.atlas, strip).track;
            layout::timeline_slot_at(track, count + 1, x)
        };
        self.step_history(Step::To(slot));
    }

    /// The filter control: All, then Commits, then Local, and round again.
    /// The window keeps it for the next history it opens.
    pub(super) fn cycle_history_filter(&self) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let state = &mut *state;
            let Some(scrub) = state.scrub.as_mut() else {
                return;
            };
            let said = if scrub.versions.is_none() {
                format!("reading the history of {}", scrub.name)
            } else if !filter_open(scrub) {
                let kind = if scrub.all.iter().any(|v| v.is_local()) {
                    "commits"
                } else {
                    "local versions"
                };
                format!("{} has no {kind} to filter by", scrub.name)
            } else {
                scrub.set_filter(scrub.filter.next());
                state.scrub_filter = scrub.filter;
                // Its marks are against a version the filter may have
                // left out: they come again with the diff.
                if let Some(entry) = scrub.sibling.and_then(|id| state.gutter.docs.get_mut(&id)) {
                    entry.marks.clear();
                    entry.changes.clear();
                }
                format!(
                    "{}: {}",
                    scrub.filter.label(),
                    counted(scrub.versions.as_deref().unwrap_or_default(), "")
                )
            };
            state.message = Some((said, Instant::now()));
        }
        self.show_pending();
        self.resume_display_link();
    }

    /// The compare control: the gutter marks what the version shown changed
    /// against the working tree, or against the version before it, again.
    /// The window keeps it for the next history it opens.
    pub(super) fn toggle_history_compare(&self) {
        'toggle: {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let State {
                scrub,
                docs,
                panes,
                gutter,
                scrub_compare,
                message,
                ..
            } = &mut *state;
            let Some(scrub) = scrub.as_mut() else {
                return;
            };
            if scrub.versions.is_none() {
                let said = format!("reading the history of {}", scrub.name);
                *message = Some((said, Instant::now()));
                break 'toggle;
            }
            scrub.against_tree = !scrub.against_tree;
            *scrub_compare = scrub.against_tree;
            if scrub.against_tree
                && let Some(live) = buffer_by_id(docs, panes, scrub.live)
            {
                let live = live.rope.clone();
                scrub.refresh_worktree(&live);
            }
            // The marks there are already, or none until they come.
            let marks = scrub.shown_marks().unwrap_or_default();
            if let Some(entry) = scrub.sibling.and_then(|id| gutter.docs.get_mut(&id)) {
                entry.marks = crate::project::git::marks(&marks);
                entry.changes = marks;
            }
            scrub.request();
            let said = if scrub.against_tree {
                "marking changes against the working tree"
            } else {
                "marking changes against the version before"
            };
            *message = Some((said.to_string(), Instant::now()));
        }
        self.resume_display_link();
        self.request_redraw();
        self.pump();
    }

    /// Restore This Version, from the strip or the Git menu. No key: Return
    /// in the history's tab does nothing, so a stray one replaces nothing,
    /// and Cmd-Return is Send Request's when the text looks like a call.
    pub(super) fn restore_version(&self) {
        let restored = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            match restore(&mut state) {
                Ok(said) => {
                    state.message = Some((said, Instant::now()));
                    true
                }
                Err(why) => {
                    state.say(layout::Feedback::Failure, why);
                    false
                }
            }
        };
        if restored {
            self.after_edit();
        } else {
            self.request_redraw();
            self.pump();
        }
    }

    /// After every frame: marks against the working tree are worked out
    /// again when the file's text changed since (edited in its own tab, or
    /// reloaded). Two pointer compares when it did not.
    pub(super) fn scrub_after_frame(&self) {
        let asked = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let State {
                scrub, docs, panes, ..
            } = &mut *state;
            let active = docs.active().id();
            let Some(scrub) = scrub
                .as_mut()
                .filter(|s| s.against_tree && s.versions.is_some() && s.sibling == Some(active))
            else {
                return;
            };
            let Some(live) = buffer_by_id(docs, panes, scrub.live) else {
                return;
            };
            if scrub.worktree.same_as(&live.rope) {
                return;
            }
            let live = live.rope.clone();
            scrub.refresh_worktree(&live);
            scrub.request();
            true
        };
        if asked {
            self.resume_display_link();
        }
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
                    Ok(Reply::Versions(Ok((all, capped, unavailable)))) => {
                        if all.is_empty() {
                            said = Some(format!(
                                "{} has no history: Git does not track it{}",
                                scrub.name,
                                if unavailable { UNAVAILABLE_TAIL } else { "" }
                            ));
                            end = true;
                            break;
                        }
                        // As the worker picks them.
                        let (versions, filter) = visible(&all, scrub.filter);
                        scrub.all = all;
                        scrub.filter = filter;
                        let count = versions.len();
                        // Opened at a commit: that version first, the caret
                        // where the line was. One the list lacks stands in
                        // with the file's caret, clamped.
                        let first = match scrub.goto.take() {
                            Some(goto) => {
                                let (slot, note) = goto_slot(&versions, &goto.commit, capped);
                                if note.is_none() {
                                    scrub.start = (goto.line, 0, goto.top);
                                }
                                scrub.note = note;
                                slot
                            }
                            None => count - 1,
                        };
                        // Said once, with the first version: the commits
                        // are listed without the local versions.
                        if unavailable {
                            scrub.note = Some(match scrub.note.take() {
                                Some(note) => format!("{note}{UNAVAILABLE_TAIL}"),
                                None => UNAVAILABLE.to_string(),
                            });
                        }
                        scrub.versions = Some(versions);
                        scrub.capped = capped;
                        scrub.shown = count;
                        scrub
                            .texts
                            .insert(count, Arc::from(scrub.worktree.to_string()));
                        scrub.pending = Some(first);
                        if first != count - 1 {
                            scrub.request();
                        }
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
                            && !scrub.against_tree
                            && let Some(entry) =
                                scrub.sibling.and_then(|id| state.gutter.docs.get_mut(&id))
                        {
                            entry.marks = crate::project::git::marks(&changes);
                            entry.changes = changes.to_vec();
                            redraw = true;
                        }
                        scrub.diffs.insert(pair, changes);
                    }
                    // Only for the jump still waited for.
                    Ok(Reply::Jump(from, to, changes)) => {
                        if from == scrub.shown && scrub.pending == Some(to) {
                            scrub.jump = Some((from, to, changes));
                        }
                    }
                    // Only against the working tree as it is now.
                    Ok(Reply::Tree(slot, generation, changes)) => {
                        if !scrub.against_tree
                            || generation != scrub.tree_generation
                            || !(slot == scrub.shown || in_window(slot, scrub.center()))
                        {
                            continue;
                        }
                        if slot == scrub.shown
                            && let Some(entry) =
                                scrub.sibling.and_then(|id| state.gutter.docs.get_mut(&id))
                        {
                            entry.marks = crate::project::git::marks(&changes);
                            entry.changes = changes.to_vec();
                            redraw = true;
                        }
                        scrub.tree_diffs.insert(slot, changes);
                    }
                    Ok(Reply::Unreadable(slot)) => {
                        if slot == scrub.shown || in_window(slot, scrub.center()) {
                            scrub.unreadable.insert(slot);
                        }
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

/// Local versions a listing compares with the commits beside them, at
/// most: each is a read and a hash of up to 2 MB, and the list waits on
/// them.
const DUPLICATE_CHECKS: usize = 64;

/// Said when local history is there but cannot be read (locked by another
/// crc past its short wait, damaged, or not permitted): the commits alone.
const UNAVAILABLE: &str = "local history unavailable";
const UNAVAILABLE_TAIL: &str = "; local history unavailable";

/// Where the worker lists versions from besides Git: the local history
/// database, when there is one to read, and the filter the history opens
/// with.
pub(super) struct Lister {
    pub(super) db: Option<std::path::PathBuf>,
    pub(super) filter: Filter,
    /// How many reads of local history answer; each one after sleeps
    /// [`STALL`] first, as one that hangs would.
    #[cfg(test)]
    pub(super) answers: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

#[cfg(test)]
const STALL: Duration = Duration::from_secs(30);

/// How long the worker waits on one read of local history before it goes
/// on without it.
const LOCAL_DEADLINE: Duration = Duration::from_secs(2);

/// The history databases a helper is reading, one helper each at most: a
/// read that never returns holds one thread and one connection, however
/// often File History opens again.
static READING: std::sync::Mutex<Vec<std::path::PathBuf>> = std::sync::Mutex::new(Vec::new());

/// A database's place in [`READING`], given back when its helper is done,
/// or never started.
struct Reading(std::path::PathBuf);

impl Reading {
    fn claim(db: &Path) -> Option<Reading> {
        let mut reading = READING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if reading.iter().any(|held| held == db) {
            return None;
        }
        reading.push(db.to_path_buf());
        Some(Reading(db.to_path_buf()))
    }
}

impl Drop for Reading {
    fn drop(&mut self) {
        READING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|held| *held != self.0);
    }
}

/// Local history as the worker reads it: each read on a helper thread,
/// waited on for [`LOCAL_DEADLINE`] at most, so one that hangs costs the
/// local versions and never the commits.
struct Local {
    db: Option<std::path::PathBuf>,
    /// Read-only, and this worker's own: the history thread goes on
    /// writing through its connection. With a helper while it reads.
    store: Option<Store>,
    /// A read outlived its deadline, or the history closed during one:
    /// its helper may still have the store, and nothing more is asked.
    stuck: bool,
    #[cfg(test)]
    answers: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

impl Local {
    fn new(from: &Lister) -> Local {
        Local {
            db: from.db.clone(),
            store: None,
            stuck: false,
            #[cfg(test)]
            answers: from.answers.clone(),
        }
    }

    /// Opens the store when there is one. Whether one is there and could
    /// not be opened, or did not answer.
    fn open(&mut self, stop: &AtomicBool) -> bool {
        let Some(db) = self.db.clone() else {
            return false;
        };
        let opened = self.run(stop, move |store| {
            Store::open_to_read(&db).map(|opened| *store = opened)
        });
        !matches!(opened, Some(Ok(())))
    }

    /// Version `id`'s text, as [`Store::read`] gives it; `None` when the
    /// store is not open or did not answer.
    fn read(
        &mut self,
        id: crate::history::store::SnapshotId,
        stop: &AtomicBool,
    ) -> Option<crate::history::store::Result<Option<String>>> {
        self.store.as_ref()?;
        self.run(stop, move |store| {
            store.as_ref().map(|store| store.read(id))
        })
        .flatten()
    }

    /// Runs `job` on the store on a helper thread. `None` when it is not
    /// done within [`LOCAL_DEADLINE`] (waiting for another history's
    /// helper on the same database included) or the history closes first:
    /// the store stays with the helper, and from then on nothing is asked.
    fn run<T: Send + 'static>(
        &mut self,
        stop: &AtomicBool,
        job: impl FnOnce(&mut Option<Store>) -> T + Send + 'static,
    ) -> Option<T> {
        if self.stuck {
            return None;
        }
        let db = self.db.clone()?;
        let deadline = Instant::now() + LOCAL_DEADLINE;
        let slice = Duration::from_millis(20);
        let reading = loop {
            if let Some(reading) = Reading::claim(&db) {
                break reading;
            }
            if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
                self.stuck = true;
                self.store = None;
                return None;
            }
            std::thread::sleep(slice);
        };
        let mut store = self.store.take();
        #[cfg(test)]
        let answers = self.answers.clone();
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("crc-file-history-local".into())
            .spawn(move || {
                let _reading = reading;
                #[cfg(test)]
                if let Some(answers) = answers {
                    let left = answers
                        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
                    if left.is_err() {
                        std::thread::sleep(STALL);
                    }
                }
                let out = job(&mut store);
                let _ = tx.send((store, out));
            });
        if spawned.is_err() {
            // The job, the store and the claim went with it.
            return None;
        }
        loop {
            match rx.recv_timeout(slice) {
                Ok((store, out)) => {
                    self.store = store;
                    return Some(out);
                }
                // It panicked: the store went with it.
                Err(mpsc::RecvTimeoutError::Disconnected) => return None,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
                        self.stuck = true;
                        return None;
                    }
                }
            }
        }
    }
}

/// Every version of the file at `opened` (as its tab has the path) and
/// `path` (its folder resolved): the commits that changed it, cut short at
/// [`HISTORY_LIMIT`], and its local versions, in one timeline, with a local
/// one equal to a commit beside it left out; whether the commits were cut
/// short; and the repository's root when it is in one. Outside Git, or
/// untracked, the local versions alone; with none of those either, why
/// there is no history. A store that fails to list, or does not answer, is
/// taken away, so nothing else waits on it, and the commits are listed
/// alone.
fn list_versions(
    opened: &Path,
    path: &Path,
    local: &mut Local,
    stop: &AtomicBool,
) -> Result<(Vec<Version>, bool, Option<std::path::PathBuf>), String> {
    let git = path
        .parent()
        .ok_or_else(|| "no parent directory".to_string())
        .and_then(crate::project::git::toplevel)
        .map_err(|_| "this file is not in a Git repository".to_string())
        .map(|root| crate::platform::canonical(&root))
        .and_then(|root| {
            let relative = path
                .strip_prefix(&root)
                .map_err(|_| "this file is not in its repository".to_string())?
                .to_path_buf();
            let (versions, capped) =
                crate::project::history::file_history(&root, &relative, HISTORY_LIMIT)?;
            Ok((root, versions, capped))
        });
    let paths = [opened.to_path_buf(), path.to_path_buf()];
    let versions = if local.store.is_some() {
        let listed = local.run(stop, move |store| {
            let listed = store
                .as_ref()
                .map(|store| local_versions(store, paths.iter().map(|p| p.as_path())));
            match listed {
                Some(Ok(versions)) => versions,
                Some(Err(_)) => {
                    *store = None;
                    Vec::new()
                }
                None => Vec::new(),
            }
        });
        listed.unwrap_or_default()
    } else {
        Vec::new()
    };
    let (root, commits, capped) = match git {
        Ok((root, commits, capped)) => (Some(root), commits, capped),
        Err(_) if !versions.is_empty() => (None, Vec::new(), false),
        Err(why) => return Err(why),
    };
    let mut all = crate::project::history::merge(commits, versions);
    if local.store.is_some() && all.iter().any(Version::is_local) {
        // Checked on a copy: one that does not answer keeps them all.
        let checked = all.clone();
        let checked = local.run(stop, move |store| {
            let mut all = checked;
            if let Some(store) = store.as_ref() {
                // The first read that fails ends the checks: each would
                // wait on the same lock again.
                let failed = std::cell::Cell::new(false);
                crate::project::history::drop_duplicates(
                    &mut all,
                    DUPLICATE_CHECKS,
                    |local, commit| {
                        !failed.get()
                            && same_text(store, local, commit).unwrap_or_else(|_| {
                                failed.set(true);
                                false
                            })
                    },
                );
            }
            all
        });
        if let Some(checked) = checked {
            all = checked;
        }
    }
    Ok((all, capped, root))
}

/// The local versions kept under any of `paths`, each once. The history
/// thread keys a file by the path its tab has, which a linked folder makes
/// differ from the resolved one.
fn local_versions<'a>(
    store: &Store,
    paths: impl IntoIterator<Item = &'a Path>,
) -> crate::history::store::Result<Vec<Version>> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut listed: Vec<&Path> = Vec::new();
    for path in paths {
        if listed.contains(&path) {
            continue;
        }
        listed.push(path);
        for entry in store.list(path)? {
            if seen.insert(entry.id) {
                out.push(Version::local(&entry, path));
            }
        }
    }
    Ok(out)
}

/// Whether local version `local` holds the text `commit` left: its Git
/// object id is the commit's blob. A file Git keeps with other line breaks
/// or another encoding than the buffer's never matches, and is kept.
fn same_text(
    store: &Store,
    local: &Version,
    commit: &Version,
) -> crate::history::store::Result<bool> {
    let Source::Local { id, .. } = local.source else {
        return Ok(false);
    };
    let Some(text) = store.read(id)? else {
        return Ok(false);
    };
    Ok(crate::project::history::blob_id(text.as_bytes()).is_some_and(|blob| blob == commit.blob))
}

/// The worker: lists the versions of `path`, then loads what is asked for,
/// the newest version's neighbourhood until a request says otherwise.
fn work(
    path: &Path,
    worktree: Arc<str>,
    from: &Lister,
    rx: &mpsc::Receiver<Request>,
    tx: &mpsc::Sender<Reply>,
    stop: &AtomicBool,
) {
    let opened = path;
    // Its folder and the root as the file system resolves them, as Git
    // gives the root: a file opened through a linked folder is still
    // inside it, and so is one gone from the disk since it was opened.
    let path = &match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => crate::platform::canonical(dir).join(name),
        _ => path.to_path_buf(),
    };
    // A lock the history thread or another crc holds fails a read after
    // a moment, and a read that hangs is left behind after a deadline,
    // rather than holding the list up. One there that cannot be read
    // leaves the commits to stand alone.
    let mut local = Local::new(from);
    let mut unavailable = local.open(stop);
    let open = local.store.is_some();
    let listed = list_versions(opened, path, &mut local, stop);
    unavailable |= open && local.store.is_none();
    let (root, mut versions) = match listed {
        Ok((all, capped, root)) => {
            let empty = all.is_empty();
            let (versions, _) = visible(&all, from.filter);
            if tx
                .send(Reply::Versions(Ok((all, capped, unavailable))))
                .is_err()
                || empty
            {
                return;
            }
            (root, versions)
        }
        Err(why) if unavailable => {
            let _ = tx.send(Reply::Versions(Err(format!("{why}{UNAVAILABLE_TAIL}"))));
            return;
        }
        Err(why) => {
            let _ = tx.send(Reply::Versions(Err(why)));
            return;
        }
    };
    let mut tx = tx.clone();
    let mut worktree_slot = versions.len();
    let mut texts: HashMap<usize, Arc<str>> = HashMap::from([(worktree_slot, worktree)]);
    // The slots whose text is a line saying why there is none.
    let mut bad: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut generation = 0;
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
                Request::Want {
                    center,
                    shown,
                    jobs,
                } => {
                    let mut none: HashMap<usize, ()> = HashMap::new();
                    evict(&mut texts, &mut none, center, [worktree_slot, shown]);
                    bad.retain(|slot| texts.contains_key(slot));
                    queue = jobs.into();
                }
                Request::Worktree(text, n) => {
                    texts.insert(worktree_slot, text);
                    generation = n;
                }
                Request::Filter {
                    versions: kept,
                    tx: to,
                } => {
                    let worktree = texts.remove(&worktree_slot);
                    texts.clear();
                    bad.clear();
                    versions = kept;
                    worktree_slot = versions.len();
                    if let Some(worktree) = worktree {
                        texts.insert(worktree_slot, worktree);
                    }
                    queue.clear();
                    tx = to;
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
                .or_insert_with(|| {
                    read_version(
                        root.as_deref(),
                        &mut local,
                        stop,
                        versions.get(slot),
                        &mut reader,
                    )
                    .unwrap_or_else(|why| {
                        bad.insert(slot);
                        why
                    })
                })
                .clone()
        };
        let reply = match job {
            Job::Text(slot) if slot <= worktree_slot => {
                let text = load(slot);
                if bad.contains(&slot) {
                    let _ = tx.send(Reply::Unreadable(slot));
                }
                Reply::Text(slot, text)
            }
            Job::Diff(pair) if pair < worktree_slot => {
                let (old, new) = (load(pair), load(pair + 1));
                Reply::Diff(pair, Arc::new(crate::ide::diff::changes(&old, &new)))
            }
            Job::Jump(from, to) if from.max(to) <= worktree_slot => {
                let (old, new) = (load(from), load(to));
                Reply::Jump(from, to, Arc::new(crate::ide::diff::changes(&old, &new)))
            }
            Job::Tree(slot) if slot < worktree_slot => {
                let (old, new) = (load(worktree_slot), load(slot));
                let changes = crate::ide::diff::changes(&old, &new);
                Reply::Tree(slot, generation, Arc::new(changes))
            }
            _ => continue,
        };
        // Not an end: a filter change drops the receiver this reply was
        // for, and the request with the next one is on its way. A closed
        // history stops the loop through `stop` and its request channel.
        let _ = tx.send(reply);
    }
}

/// The text of `version`, or a line saying why there is none, so a step
/// onto it still shows something.
fn read_version(
    root: Option<&Path>,
    local: &mut Local,
    stop: &AtomicBool,
    version: Option<&Version>,
    reader: &mut Option<BlobReader>,
) -> Result<Arc<str>, Arc<str>> {
    let Some(version) = version else {
        return Err(Arc::from(""));
    };
    if let Source::Local { id, .. } = version.source {
        let text = match local.read(id, stop) {
            Some(Ok(Some(text))) => return Ok(Arc::from(text)),
            Some(Ok(None)) => "(this local version is gone from local history)\n".to_string(),
            Some(Err(why)) => format!("(could not read this version: {why})\n"),
            None if local.stuck => "(local history did not answer)\n".to_string(),
            None => "(local history is not open)\n".to_string(),
        };
        return Err(Arc::from(text));
    }
    let Some(root) = root else {
        return Err(Arc::from("(not in a repository)\n"));
    };
    if reader.is_none() {
        match BlobReader::open(root) {
            Ok(opened) => *reader = Some(opened),
            Err(why) => return Err(Arc::from(format!("(could not read this version: {why})\n"))),
        }
    }
    let Some(open) = reader.as_mut() else {
        return Err(Arc::from(""));
    };
    let text = match open.read(&version.blob, GUTTER_MAX_BYTES) {
        Ok(Blob::Text(text)) => return Ok(Arc::from(text)),
        Ok(Blob::Binary) => "(binary at this commit)\n".to_string(),
        Ok(Blob::TooLarge) => "(over 2 MB at this commit)\n".to_string(),
        Ok(Blob::Missing) => "(not in the repository)\n".to_string(),
        Err(why) => {
            // Out of step: the next read starts a new reader.
            *reader = None;
            format!("(could not read this version: {why})\n")
        }
    };
    Err(Arc::from(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Git alone: no test reads the local history in Application Support.
    const GIT_ONLY: Lister = Lister {
        db: None,
        filter: Filter::All,
        answers: None,
    };

    #[test]
    fn a_history_opened_at_a_commit_finds_it_by_any_prefix() {
        let version = |commit: &str| Version {
            commit: commit.repeat(40),
            author: "Robin".into(),
            time: 0,
            summary: "plant beans".into(),
            blob: "0".repeat(40),
            path: std::path::PathBuf::from("garden.txt"),
            source: Source::Commit,
        };
        let versions = [version("a"), version("b"), version("c")];
        let b = "b".repeat(40);
        assert_eq!(goto_slot(&versions, &b, false), (1, None));
        assert_eq!(goto_slot(&versions, "bbbbbbb", true), (1, None));
        assert_eq!(goto_slot(&versions, "c", false), (2, None));
        // Cut short before it: the oldest read, and why.
        assert_eq!(
            goto_slot(&versions, &"d".repeat(40), true),
            (
                0,
                Some("commit ddddddd is past the loaded history".to_string())
            )
        );
        // Not cut short: the file's history does not have it.
        assert_eq!(
            goto_slot(&versions, "dddd", false),
            (
                2,
                Some("commit dddd is not in this file's history".to_string())
            )
        );
        assert_eq!(
            goto_slot(&versions, "", false).0,
            2,
            "nothing matches nothing"
        );
    }

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
        evict(&mut texts, &mut diffs, 10, [60, 50]);
        let mut kept: Vec<usize> = texts.into_keys().collect();
        kept.sort_unstable();
        let mut expected: Vec<usize> = (0..=10 + RADIUS).collect();
        expected.extend([50, 60]);
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

    fn fade_between(old: &str, new: &str) -> Fade {
        Fade::new(&crate::ide::diff::changes(old, new), old, Instant::now())
    }

    #[test]
    fn a_fade_washes_what_was_added_or_changed_and_ghosts_what_went() {
        let fade = fade_between("a\nb\nc\nd\n", "a\nB\nc\nnew\none\n");
        assert_eq!(
            fade.tints,
            [(1..2, MarkKind::Modified), (3..5, MarkKind::Modified)]
        );
        assert!(fade.ghosts.is_empty());
        let fade = fade_between("a\nb\n", "a\nx\ny\nb\n");
        assert_eq!(fade.tints, [(1..3, MarkKind::Added)]);
        // Removed lines sit where they were in the new version, tabs
        // expanded and line breaks gone, CRLF ones included.
        let fade = fade_between("a\n\tgone\r\nb\n", "a\nb\n");
        assert!(fade.tints.is_empty());
        assert_eq!(
            fade.ghosts,
            [layout::Ghost {
                at: 1,
                lines: vec!["    gone".into()],
                more: 0,
            }]
        );
        // Removed from the end: above the line count.
        let fade = fade_between("a\nb\nc\n", "a\n");
        assert_eq!(fade.ghosts[0].at, 1);
        assert_eq!(fade.ghosts[0].lines, ["b", "c"]);
        assert!(fade_between("same\n", "same\n").tints.is_empty());
    }

    #[test]
    fn a_long_removal_shows_three_lines_and_how_many_more() {
        let fade = fade_between("1\n2\n3\n4\n5\n6\nkeep\n", "keep\n");
        assert_eq!(
            fade.ghosts,
            [layout::Ghost {
                at: 0,
                lines: vec!["1".into(), "2".into(), "3".into()],
                more: 3,
            }]
        );
        let fade = fade_between("1\n2\n3\nkeep\n", "keep\n");
        assert_eq!(fade.ghosts[0].lines.len(), 3);
        assert_eq!(fade.ghosts[0].more, 0, "exactly three: no more line");
    }

    #[test]
    fn a_step_back_swaps_what_was_added_and_removed() {
        // The worker's diff runs from the older version to the newer; a
        // step back reads it the other way, against the newer text.
        let (older, newer) = ("a\nb\nc\n", "a\nadded\nb\nC\n");
        let forward = crate::ide::diff::changes(older, newer);
        let back = Fade::new(&reversed(&forward), newer, Instant::now());
        assert_eq!(back.tints, [(2..3, MarkKind::Modified)]);
        assert_eq!(
            back.ghosts,
            [layout::Ghost {
                at: 1,
                lines: vec!["added".into()],
                more: 0,
            }]
        );
        // The same as diffing the other way round.
        assert_eq!(reversed(&forward), crate::ide::diff::changes(newer, older));
        assert_eq!(reversed(&reversed(&forward)), forward);
    }

    #[test]
    fn a_fade_eases_out_and_ends() {
        let start = Instant::now();
        let mut fade = Fade::new(&[], "", start);
        assert_eq!(fade.alpha(start), Some(1.0));
        let mid = fade.alpha(start + FADE / 2).unwrap();
        assert!((mid - 0.25).abs() < 1e-4, "{mid}");
        // Faster early than late: past half its strength before a third.
        assert!(fade.alpha(start + FADE / 3).unwrap() < 0.5);
        assert!(fade.alpha(start + FADE * 9 / 10).unwrap() < 0.02);
        assert_eq!(fade.alpha(start + FADE), None);
        assert_eq!(fade.alpha(start + FADE * 4), None);
        // A clock behind the start is the start.
        assert_eq!(fade_alpha(-1.0), 1.0);
        assert_eq!(fade_alpha(2.0), 0.0);
        // Held by a self-test, whatever the clock says.
        fade.frozen = Some(Duration::from_millis(75));
        let held = fade.alpha(start + FADE * 10).unwrap();
        assert!((held - 0.25).abs() < 1e-4, "{held}");
    }

    #[test]
    fn an_extension_is_leaked_once() {
        let a = static_ext("scrubtest");
        let b = static_ext(&String::from("scrubtest"));
        assert!(std::ptr::eq(a, b));
        assert_eq!(a, "scrubtest");
    }

    /// `git` in `dir`, which has to succeed.
    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repository whose `ledger.rs`, about 200 KB, changed in each of
    /// `commits` commits, made in one `git fast-import`; and the file's
    /// last text.
    fn long_history(name: &str, commits: usize) -> (crate::project::TempTree, String) {
        use std::io::Write;
        let tree = crate::project::TempTree::new(name, &[]);
        let dir = tree.0.as_path();
        git(dir, &["init", "--quiet", "--initial-branch=main"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
        let mut lines: Vec<String> = (0..4000)
            .map(|i| {
                format!(
                    "    let entry_{i} = ledger.post({i}, Amount::cents({}));",
                    i * 7
                )
            })
            .collect();
        let mut child = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["fast-import", "--quiet"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("git fast-import");
        let mut stream = std::io::BufWriter::new(child.stdin.take().expect("stdin"));
        let mut seed = 1u64;
        let mut text = String::new();
        for n in 0..commits {
            // A few lines changed a commit, and now and then one added.
            for _ in 0..3 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                let at = (seed >> 33) as usize % lines.len();
                lines[at] = format!("    let entry_{at} = ledger.post({at}, Amount::cents({n}));");
            }
            if n % 10 == 0 {
                lines.push(format!("    ledger.close({n});"));
            }
            text = lines.join("\n") + "\n";
            let message = format!("Post revision {n}");
            write!(
                stream,
                "commit refs/heads/main\n\
                 author Sam Example <sam@example.invalid> {time} +0000\n\
                 committer Sam Example <sam@example.invalid> {time} +0000\n\
                 data {}\n{message}\n\
                 M 100644 inline ledger.rs\ndata {}\n{text}\n",
                message.len(),
                text.len(),
                time = 1_700_000_000 + n * 60,
            )
            .expect("write");
        }
        drop(stream);
        assert!(child.wait().expect("fast-import").success());
        (tree, text)
    }

    /// A thousand versions of a 200 KB file through the real worker,
    /// stepped from the newest to the oldest as Left steps them: the texts
    /// held, by the window and the worker together, stay within the window
    /// and the working tree all the way, and the list and the first
    /// version come quickly.
    #[test]
    fn a_long_history_holds_no_more_than_the_window() {
        const COMMITS: usize = 1000;
        let made = Instant::now();
        let (tree, last) = long_history("scrub-long-history", COMMITS);
        let made = made.elapsed();

        let started = Instant::now();
        let (tx, worker_rx) = mpsc::channel();
        let (worker_tx, rx) = mpsc::channel();
        let halt = Arc::new(AtomicBool::new(false));
        let worktree: Arc<str> = Arc::from(last);
        // Through the temporary folder's link (`/var` to `/private/var`):
        // the worker resolves it.
        let path = tree.0.join("ledger.rs");
        let held = worktree.clone();
        let worker =
            std::thread::spawn(move || work(&path, held, &GIT_ONLY, &worker_rx, &worker_tx, &halt));
        let wait = Duration::from_secs(60);
        let (versions, capped) = match rx.recv_timeout(wait) {
            Ok(Reply::Versions(listed)) => {
                let (versions, capped, _) = listed.expect("versions");
                (versions, capped)
            }
            _ => panic!("no versions"),
        };
        let listed = started.elapsed();
        let count = versions.len();
        assert_eq!(count, COMMITS);
        assert!(capped, "the list stops at the limit");

        // The window's side, as `poll_scrub` keeps it, and every text that
        // came, to see which are still held anywhere.
        let mut texts: HashMap<usize, Arc<str>> = HashMap::from([(count, worktree)]);
        let mut diffs: HashMap<usize, Arc<Vec<Change>>> = HashMap::new();
        let mut seen: Vec<std::sync::Weak<str>> = Vec::new();
        let mut first = None;
        let mut most = 0;
        let mut shown = count;
        for target in (0..count).rev() {
            evict(&mut texts, &mut diffs, target, [count, shown]);
            let want = jobs(
                target,
                count + 1,
                |slot| texts.contains_key(&slot),
                |pair| diffs.contains_key(&pair),
            );
            if !want.is_empty() {
                tx.send(Request::Want {
                    center: target,
                    shown,
                    jobs: want,
                })
                .unwrap();
            }
            // Ready as `show` waits for it: the text, and the diff from the
            // version before, but for the first.
            let first_step = target + 1 == count;
            while !(texts.contains_key(&target) && (first_step || diffs.contains_key(&target))) {
                match rx.recv_timeout(wait).expect("the worker answers") {
                    Reply::Text(slot, text) => {
                        seen.push(Arc::downgrade(&text));
                        if in_window(slot, target) {
                            texts.insert(slot, text);
                        }
                    }
                    Reply::Diff(pair, changes) => {
                        if in_window(pair, target) && in_window(pair + 1, target) {
                            diffs.insert(pair, changes);
                        }
                    }
                    Reply::Versions(_) | Reply::Jump(..) | Reply::Tree(..) => {
                        panic!("not asked for")
                    }
                    Reply::Unreadable(slot) => panic!("version {slot} unreadable"),
                }
            }
            shown = target;
            first.get_or_insert_with(|| started.elapsed());
            assert!(texts.len() <= 2 * RADIUS + 2, "{} texts", texts.len());
            assert!(diffs.len() <= 2 * RADIUS, "{} diffs", diffs.len());
            // Window and worker together: the window, the working tree,
            // and the edge of the window the worker may not have moved yet.
            seen.retain(|text| text.strong_count() > 0);
            most = most.max(seen.len());
            assert!(seen.len() <= 2 * RADIUS + 3, "{} texts held", seen.len());
        }
        let stepped = started.elapsed();

        // A jump from the oldest to the middle, as the timeline asks for it:
        // its changes come from the worker, and lead from one text to the
        // other an edit each.
        let target = count / 2;
        evict(&mut texts, &mut diffs, target, [count, shown]);
        let mut want = jobs(target, count + 1, |s| texts.contains_key(&s), |_| true);
        want.insert(1, Job::Jump(shown, target));
        tx.send(Request::Want {
            center: target,
            shown,
            jobs: want,
        })
        .unwrap();
        let mut jump = None;
        while jump.is_none() || !texts.contains_key(&target) {
            match rx.recv_timeout(wait).expect("the worker answers") {
                Reply::Text(slot, text) => {
                    seen.push(Arc::downgrade(&text));
                    texts.insert(slot, text);
                }
                Reply::Jump(from, to, changes) => {
                    assert_eq!((from, to), (shown, target));
                    jump = Some(changes);
                }
                _ => {}
            }
        }
        let changes = jump.unwrap();
        assert!(!changes.is_empty());
        let mut buffer = crate::text::buffer::Buffer::generated("ledger.rs", "rs", "");
        buffer.show_text(&texts[&shown], None, 0, 0);
        buffer.drain_edits();
        buffer.show_text(&texts[&target], Some(&changes), 0, 0);
        assert!(buffer.rope.to_string() == *texts[&target]);
        let edits = buffer.drain_edits().expect("edits, not a full parse");
        assert_eq!(edits.len(), changes.len(), "an edit per change");
        seen.retain(|text| text.strong_count() > 0);
        assert!(seen.len() <= 2 * RADIUS + 4, "{} texts held", seen.len());

        drop(tx);
        worker.join().unwrap();
        eprintln!(
            "{COMMITS} versions of {} KB: made in {:.2} s, listed in {:.0} ms, \
             first version at {:.0} ms, all stepped in {:.2} s, at most {most} texts held",
            texts[&count].len() / 1024,
            made.as_secs_f64(),
            listed.as_secs_f64() * 1e3,
            first.unwrap_or_default().as_secs_f64() * 1e3,
            stepped.as_secs_f64(),
        );
        // Generous, so a slow or busy machine does not fail it.
        assert!(listed < Duration::from_secs(5), "listed in {listed:?}");
        assert!(
            first.unwrap_or_default() < Duration::from_secs(5),
            "first version at {first:?}"
        );
    }

    /// A file opened through a linked folder has its history: the worker
    /// resolves the path, as Git resolves the repository's.
    #[test]
    fn a_file_through_a_linked_folder_has_its_history() {
        let (tree, last) = long_history("scrub-linked-history", 3);
        let link = std::env::temp_dir().join(format!("crc-scrub-link-{}", std::process::id()));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&tree.0, &link).unwrap();
        let (tx, worker_rx) = mpsc::channel::<Request>();
        let (worker_tx, rx) = mpsc::channel();
        let halt = Arc::new(AtomicBool::new(false));
        let path = link.join("ledger.rs");
        let worker = std::thread::spawn(move || {
            work(
                &path,
                Arc::from(last),
                &GIT_ONLY,
                &worker_rx,
                &worker_tx,
                &halt,
            )
        });
        let listed = rx.recv_timeout(Duration::from_secs(60));
        drop(tx);
        worker.join().unwrap();
        let _ = std::fs::remove_file(&link);
        match listed {
            Ok(Reply::Versions(Ok((versions, capped, _)))) => {
                assert_eq!(versions.len(), 3);
                assert!(!capped);
                assert_eq!(versions[2].path, Path::new("ledger.rs"));
            }
            Ok(Reply::Versions(Err(why))) => panic!("no history: {why}"),
            _ => panic!("no versions"),
        }
    }

    fn commit_v(name: &str) -> Version {
        Version {
            commit: name.repeat(40),
            author: "Robin".into(),
            time: 0,
            summary: name.into(),
            blob: name.repeat(40),
            path: std::path::PathBuf::from("garden.txt"),
            source: Source::Commit,
        }
    }

    fn local_v(id: i64, reason: Reason) -> Version {
        let entry = crate::history::store::Entry {
            id: crate::history::store::SnapshotId(id),
            at: id * 1000,
            reason,
            size: 1,
        };
        Version::local(&entry, Path::new("/garden/garden.txt"))
    }

    #[test]
    fn the_filter_keeps_one_kind_and_never_nothing() {
        let all = [
            commit_v("a"),
            local_v(1, Reason::Saved),
            local_v(2, Reason::Idle),
            commit_v("b"),
        ];
        let tags = |v: &[Version]| v.iter().map(tag).collect::<Vec<_>>();
        assert_eq!(visible(&all, Filter::All), (all.to_vec(), Filter::All));
        let (commits, filter) = visible(&all, Filter::Commits);
        assert_eq!(
            (tags(&commits), filter),
            (
                vec!["aaaaaaa".to_string(), "bbbbbbb".into()],
                Filter::Commits
            )
        );
        let (local, filter) = visible(&all, Filter::Local);
        assert_eq!(
            (tags(&local), filter),
            (
                vec!["local:saved".to_string(), "local:idle".into()],
                Filter::Local
            )
        );
        // Nothing of the kind: every version, and the filter says so.
        let only = [commit_v("a"), commit_v("b")];
        assert_eq!(visible(&only, Filter::Local), (only.to_vec(), Filter::All));
        assert_eq!(Filter::All.next(), Filter::Commits);
        assert_eq!(Filter::Commits.next(), Filter::Local);
        assert_eq!(Filter::Local.next(), Filter::All);
        assert_eq!(counted(&all, ""), "4 versions");
        assert_eq!(counted(&commits, "+"), "2+ commits");
        assert_eq!(counted(&local[..1], ""), "1 local version");
    }

    #[test]
    fn a_filter_change_keeps_the_version_shown_or_the_next_newer() {
        let all = [
            commit_v("a"),
            local_v(1, Reason::Saved),
            commit_v("b"),
            local_v(2, Reason::Idle),
        ];
        let commits = visible(&all, Filter::Commits).0;
        let local = visible(&all, Filter::Local).0;
        // Kept: its slot among the commits.
        assert_eq!(reslot(&all, &all, &commits, 2), (1, true));
        // Left out: the next commit after it, or the working tree.
        assert_eq!(reslot(&all, &all, &commits, 1), (1, false));
        assert_eq!(reslot(&all, &all, &commits, 3), (2, false));
        assert_eq!(reslot(&all, &all, &local, 0), (0, false));
        // The working tree stays the working tree.
        assert_eq!(reslot(&all, &all, &local, 4), (2, true));
        assert_eq!(reslot(&all, &commits, &all, 1), (2, true));
    }

    #[test]
    fn local_versions_are_named_by_why_they_were_kept() {
        let words: Vec<&str> = Reason::ALL.into_iter().map(reason_words).collect();
        assert_eq!(
            words,
            [
                "opened",
                "saved",
                "idle",
                "before reload",
                "closed without saving",
                "at quit",
                "before restore",
                "edited on disk"
            ]
        );
        // From blame: a local version is no commit, whatever the prefix.
        let versions = [local_v(1, Reason::Saved), commit_v("b")];
        assert_eq!(goto_slot(&versions, "bbbb", false), (1, None));
        assert_eq!(goto_slot(&versions, "cccc", false).0, 1);
    }

    /// `git commit -am` in `dir` at a fixed date, with a test identity and
    /// no signing.
    fn commit_at(dir: &Path, message: &str, when: i64) {
        let date = format!("@{when} +0000");
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=Robin",
                "-c",
                "user.email=robin@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-am",
                message,
            ])
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// What [`start`] gives back: the list, the requests, the replies and
    /// the worker.
    type Started = (
        Result<(Vec<Version>, bool, bool), String>,
        mpsc::Sender<Request>,
        mpsc::Receiver<Reply>,
        std::thread::JoinHandle<()>,
    );

    /// A worker on `path` reading local history from `db`; its list, and
    /// the channels to ask it for more.
    fn start(path: &Path, db: &Path, worktree: &str, filter: Filter) -> Started {
        start_stalled(path, db, worktree, filter, None)
    }

    /// [`start`], with the reads of local history past the first
    /// `answers` hanging.
    fn start_stalled(
        path: &Path,
        db: &Path,
        worktree: &str,
        filter: Filter,
        answers: Option<usize>,
    ) -> Started {
        let (tx, worker_rx) = mpsc::channel();
        let (worker_tx, rx) = mpsc::channel();
        let halt = Arc::new(AtomicBool::new(false));
        let (path, worktree) = (path.to_path_buf(), Arc::<str>::from(worktree));
        let from = Lister {
            db: Some(db.to_path_buf()),
            filter,
            answers: answers.map(|n| Arc::new(std::sync::atomic::AtomicUsize::new(n))),
        };
        let worker =
            std::thread::spawn(move || work(&path, worktree, &from, &worker_rx, &worker_tx, &halt));
        let listed = match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(Reply::Versions(listed)) => listed,
            _ => panic!("no versions"),
        };
        (listed, tx, rx, worker)
    }

    /// Asks for version `slot`'s text and waits for it.
    fn text_of(tx: &mpsc::Sender<Request>, rx: &mpsc::Receiver<Reply>, slot: usize) -> String {
        tx.send(Request::Want {
            center: slot,
            shown: slot,
            jobs: vec![Job::Text(slot)],
        })
        .unwrap();
        loop {
            match rx
                .recv_timeout(Duration::from_secs(60))
                .expect("the worker answers")
            {
                Reply::Text(at, text) if at == slot => return text.to_string(),
                _ => {}
            }
        }
    }

    /// Two commits with local versions between and after them: one
    /// timeline by time, the save that was committed left out, each local
    /// version read from the store, and a filter change numbering them
    /// again on a channel of its own.
    #[test]
    fn local_versions_join_the_commits_in_one_timeline() {
        use crate::history::store::Store;
        let tree = crate::project::TempTree::new("scrub-local-history", &[("plan.txt", "one\n")]);
        let dir = tree.0.as_path();
        git(dir, &["init", "--quiet"]);
        git(dir, &["add", "plan.txt"]);
        commit_at(dir, "first", 1_700_000_000);
        std::fs::write(dir.join("plan.txt"), "one\ntwo\nthree\n").unwrap();
        commit_at(dir, "second", 1_700_000_100);
        let db = tree.0.join("history").join("history.db");
        let path = dir.join("plan.txt");
        {
            let store = Store::open(&db).unwrap();
            let at = |s: i64| s * 1000;
            for (text, reason, when) in [
                ("one\n", Reason::Opened, at(1_700_000_010)),
                ("one\ntwo\n", Reason::Saved, at(1_700_000_050)),
                ("one\ntwo\nthree\n", Reason::Saved, at(1_700_000_090)),
                ("one\ntwo\nthree\nfour\n", Reason::Idle, at(1_700_000_200)),
            ] {
                store.put_snapshot(&path, text, reason, when).unwrap();
            }
            // Another file's versions are not this one's.
            store
                .put_snapshot(
                    &dir.join("other.txt"),
                    "x",
                    Reason::Saved,
                    at(1_700_000_020),
                )
                .unwrap();
        }
        let worktree = "one\ntwo\nthree\nfour\nfive\n";
        let (listed, tx, rx, worker) = start(&path, &db, worktree, Filter::All);
        let (all, capped, unavailable) = listed.expect("versions");
        assert!(!capped && !unavailable);
        let tags: Vec<String> = all.iter().map(tag).collect();
        // Opened at the first commit and saved as the second: those two
        // add nothing to the commits beside them.
        assert_eq!(tags[1], "local:saved");
        assert_eq!(tags[3], "local:idle");
        assert_eq!(all.len(), 4, "{tags:?}");
        assert_eq!(all[0].summary, "first");
        assert_eq!(all[2].summary, "second");
        assert_eq!(text_of(&tx, &rx, 1), "one\ntwo\n");
        assert_eq!(text_of(&tx, &rx, 3), "one\ntwo\nthree\nfour\n");
        assert_eq!(text_of(&tx, &rx, 0), "one\n", "a commit through Git");
        // Local alone: slot 0 is the save now, and replies come on the new
        // channel.
        let (local, _) = visible(&all, Filter::Local);
        let (to, from) = mpsc::channel();
        tx.send(Request::Filter {
            versions: local,
            tx: to,
        })
        .unwrap();
        assert_eq!(text_of(&tx, &from, 0), "one\ntwo\n");
        assert_eq!(text_of(&tx, &from, 2), worktree, "the working tree kept");
        drop(tx);
        worker.join().unwrap();
    }

    /// Against the working tree, through the real worker: the changes from
    /// the working tree's text to a version, tagged with which text that
    /// was, and a version gone from local history said to be unreadable
    /// before the line that says why.
    #[test]
    fn the_worker_diffs_a_version_against_the_working_tree() {
        use crate::history::store::Store;
        let tree = crate::project::TempTree::new("scrub-against-tree", &[("menu.txt", "c\n")]);
        let db = tree.0.join("history").join("history.db");
        let path = tree.0.join("menu.txt");
        {
            let store = Store::open(&db).unwrap();
            store
                .put_snapshot(&path, "soup\nbread\n", Reason::Opened, 1_000)
                .unwrap();
            store
                .put_snapshot(&path, "soup\n", Reason::Saved, 2_000)
                .unwrap();
        }
        let (listed, tx, rx, worker) = start(&path, &db, "soup\nsalad\n", Filter::All);
        assert_eq!(listed.expect("local versions").0.len(), 2);
        let tree_of = |slot: usize| {
            tx.send(Request::Want {
                center: slot,
                shown: slot,
                jobs: vec![Job::Tree(slot)],
            })
            .unwrap();
            loop {
                match rx.recv_timeout(Duration::from_secs(60)).expect("an answer") {
                    Reply::Tree(at, generation, changes) if at == slot => {
                        return (generation, changes.to_vec());
                    }
                    _ => {}
                }
            }
        };
        assert_eq!(
            tree_of(0),
            (
                0,
                crate::ide::diff::changes("soup\nsalad\n", "soup\nbread\n")
            )
        );
        // The working tree changed: the next diff is against its new text.
        tx.send(Request::Worktree(Arc::from("soup\nsalad\nbread\n"), 1))
            .unwrap();
        let (generation, changes) = tree_of(0);
        assert_eq!(generation, 1);
        assert_eq!(
            changes,
            crate::ide::diff::changes("soup\nsalad\nbread\n", "soup\nbread\n")
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, MarkKind::Removed);
        // A version gone from local history: one the store never had.
        let (to, rx) = mpsc::channel();
        tx.send(Request::Filter {
            versions: vec![local_v(1_000_000, Reason::Saved)],
            tx: to,
        })
        .unwrap();
        tx.send(Request::Want {
            center: 0,
            shown: 0,
            jobs: vec![Job::Text(0)],
        })
        .unwrap();
        let mut said = Vec::new();
        while said.len() < 2 {
            match rx.recv_timeout(Duration::from_secs(60)).expect("an answer") {
                Reply::Unreadable(slot) => said.push(format!("unreadable {slot}")),
                Reply::Text(slot, text) => said.push(format!("{slot}: {}", text.trim_end())),
                _ => {}
            }
        }
        assert_eq!(
            said,
            [
                "unreadable 0",
                "0: (this local version is gone from local history)"
            ]
        );
        drop(tx);
        worker.join().unwrap();
    }

    /// A history over two versions and the working tree, with no worker:
    /// what is asked of it goes to the receiver given back.
    fn scrub_of(versions: Vec<Version>) -> (Scrub, mpsc::Receiver<Request>) {
        let (tx, requests) = mpsc::channel();
        let (_, rx) = mpsc::channel();
        let scrub = Scrub {
            live: 1,
            sibling: Some(2),
            name: "menu.txt".into(),
            ext: "txt",
            versions: Some(versions.clone()),
            all: versions,
            filter: Filter::All,
            capped: false,
            shown: 0,
            refill: false,
            pending: None,
            texts: HashMap::new(),
            diffs: HashMap::new(),
            worktree: crate::text::rope::Rope::from_text("w\n"),
            start: (0, 0, 0),
            goto: None,
            placed: false,
            note: None,
            fade: None,
            jump: None,
            unreadable: std::collections::HashSet::new(),
            against_tree: false,
            tree_diffs: HashMap::new(),
            tree_generation: 0,
            tx,
            rx,
            stop: Arc::new(AtomicBool::new(false)),
            gone: false,
        };
        (scrub, requests)
    }

    /// The marks come from the diff with the version before, or from the
    /// worker's diff with the working tree, which is asked for first and
    /// asked again when the working tree changes; and a version restores
    /// only once it is read and shown.
    #[test]
    fn marks_come_from_the_version_before_or_the_working_tree() {
        let (mut scrub, requests) = scrub_of(vec![commit_v("a"), local_v(1, Reason::Saved)]);
        let previous = Arc::new(crate::ide::diff::changes("a\n", "b\n"));
        let tree = Arc::new(crate::ide::diff::changes("w\n", "b\nw\n"));
        scrub.texts = HashMap::from([
            (0, Arc::from("a\n")),
            (1, Arc::from("b\n")),
            (2, Arc::from("w\n")),
        ]);
        scrub.diffs.insert(0, previous.clone());
        scrub.shown = 1;
        assert_eq!(scrub.shown_marks(), Some(previous.to_vec()));
        assert!(!scrub.wants_tree_diff());
        assert_eq!(scrub.restorable(), Ok(()));

        scrub.against_tree = true;
        assert_eq!(scrub.shown_marks(), None, "on its way");
        assert!(scrub.wants_tree_diff() && scrub.busy());
        scrub.request();
        let jobs = requests
            .try_iter()
            .find_map(|r| match r {
                Request::Want { jobs, .. } => Some(jobs),
                _ => None,
            })
            .expect("a request");
        assert_eq!(jobs[0], Job::Tree(1));
        scrub.tree_diffs.insert(1, tree.clone());
        assert_eq!(scrub.shown_marks(), Some(tree.to_vec()));
        assert!(!scrub.wants_tree_diff());

        // The file's text changed: its diffs go, and it is sent again.
        scrub.refresh_worktree(&crate::text::rope::Rope::from_text("w\nx\n"));
        assert!(scrub.tree_diffs.is_empty());
        let sent: Vec<u64> = requests
            .try_iter()
            .filter_map(|r| match r {
                Request::Worktree(text, n) => Some((text.len(), n)),
                _ => None,
            })
            .map(|(len, n)| {
                assert_eq!(len, 4);
                n
            })
            .collect();
        assert_eq!(sent, [1]);
        assert_eq!(scrub.tree_generation, 1);

        // The working tree has no marks against itself, and nothing to
        // restore; nor has a version on its way or one that was not read.
        scrub.shown = 2;
        assert_eq!(scrub.shown_marks(), Some(Vec::new()));
        assert!(scrub.restorable().unwrap_err().contains("working tree"));
        scrub.shown = 1;
        scrub.unreadable.insert(1);
        assert!(
            scrub
                .restorable()
                .unwrap_err()
                .contains("could not be read")
        );
        scrub.unreadable.clear();
        scrub.pending = Some(0);
        assert_eq!(
            scrub.restorable(),
            Err("version 1 of 2 is still loading".into())
        );
    }

    #[test]
    fn a_version_that_cannot_be_read_is_an_error_with_a_line_saying_why() {
        let mut reader = None;
        let mut closed = Local::new(&GIT_ONLY);
        let stop = AtomicBool::new(false);
        let local = local_v(1, Reason::Saved);
        assert_eq!(
            read_version(None, &mut closed, &stop, Some(&local), &mut reader),
            Err(Arc::from("(local history is not open)\n"))
        );
        assert_eq!(
            read_version(None, &mut closed, &stop, Some(&commit_v("a")), &mut reader),
            Err(Arc::from("(not in a repository)\n"))
        );
        assert!(read_version(None, &mut closed, &stop, None, &mut reader).is_err());
    }

    /// Outside Git a file has its local versions alone; with none, the
    /// history says why there is nothing.
    #[test]
    fn a_file_outside_git_has_its_local_versions() {
        use crate::history::store::Store;
        let tree = crate::project::TempTree::new("scrub-local-only", &[("loose.txt", "c\n")]);
        let db = tree.0.join("history").join("history.db");
        let path = tree.0.join("loose.txt");
        {
            let store = Store::open(&db).unwrap();
            store
                .put_snapshot(&path, "a\n", Reason::Opened, 1_000)
                .unwrap();
            store
                .put_snapshot(&path, "b\n", Reason::Saved, 2_000)
                .unwrap();
        }
        let (listed, tx, rx, worker) = start(&path, &db, "c\n", Filter::Commits);
        let (all, capped, unavailable) = listed.expect("local versions");
        assert!(!capped && !unavailable);
        let tags: Vec<String> = all.iter().map(tag).collect();
        assert_eq!(tags, ["local:opened", "local:saved"]);
        // No commits to keep: the worker steps through every version.
        assert_eq!(text_of(&tx, &rx, 0), "a\n");
        drop(tx);
        worker.join().unwrap();

        let none = tree.0.join("none.txt");
        std::fs::write(&none, "n\n").unwrap();
        let (listed, tx, _rx, worker) = start(&none, &db, "n\n", Filter::All);
        assert_eq!(
            listed,
            Err("this file is not in a Git repository".to_string())
        );
        drop(tx);
        worker.join().unwrap();
    }

    /// Local history that does not answer, on open or on a later read:
    /// the commits within the deadline, and they go on being read; a local
    /// version is unreadable; and with one read left behind on a database,
    /// the next is given up at once, with no second helper on it.
    #[test]
    fn a_local_history_that_hangs_costs_only_the_local_versions() {
        use crate::history::store::Store;
        let tree = crate::project::TempTree::new("scrub-local-hangs", &[("plan.txt", "one\n")]);
        let dir = tree.0.as_path();
        git(dir, &["init", "--quiet"]);
        git(dir, &["add", "plan.txt"]);
        commit_at(dir, "first", 1_700_000_000);
        let path = dir.join("plan.txt");
        // Two databases with the same two versions.
        let [db, other] = ["history", "other"].map(|folder| {
            let db = tree.0.join(folder).join("history.db");
            let store = Store::open(&db).unwrap();
            for (text, at) in [
                ("one\ntwo\n", 1_700_000_050_000),
                ("one\nthree\n", 1_700_000_060_000),
            ] {
                store.put_snapshot(&path, text, Reason::Saved, at).unwrap();
            }
            db
        });
        let commits_alone = |listed: Result<(Vec<Version>, bool, bool), String>| {
            let (all, _, unavailable) = listed.expect("the commits");
            assert!(all.len() == 1 && matches!(all[0].source, Source::Commit));
            assert!(unavailable, "local history said to be unavailable");
        };
        let within = |started: Instant, limit: u64| {
            let elapsed = started.elapsed();
            assert!(elapsed < Duration::from_millis(limit), "waited {elapsed:?}");
        };

        // On open: the commits alone, and they are read.
        let started = Instant::now();
        let (listed, tx, rx, _worker) = start_stalled(&path, &db, "w\n", Filter::All, Some(0));
        within(started, 2500);
        commits_alone(listed);
        let started = Instant::now();
        assert_eq!(text_of(&tx, &rx, 0), "one\n", "a commit through Git");
        within(started, 500);
        drop(tx);

        // Its helper still out: another history on the database starts
        // no second one, and gives up on it within the deadline.
        let started = Instant::now();
        let (listed, tx, _rx, _worker) = start(&path, &db, "w\n", Filter::All);
        within(started, 2500);
        commits_alone(listed);
        drop(tx);

        // Past the open, the list and the check for duplicates: a local
        // version unreadable within the deadline, whether the read that
        // hangs is the one asked for or the one the worker started on;
        // then the commits as before, and the other local version at once.
        let (listed, tx, rx, worker) = start_stalled(&path, &other, "w\n", Filter::All, Some(3));
        let (all, _, unavailable) = listed.expect("the versions");
        assert_eq!(all.len(), 3);
        assert!(!unavailable);
        let unreadable = |slot: usize| {
            tx.send(Request::Want {
                center: slot,
                shown: slot,
                jobs: vec![Job::Text(slot)],
            })
            .unwrap();
            let mut said = Vec::new();
            while said.len() < 2 {
                match rx.recv_timeout(Duration::from_secs(60)).expect("an answer") {
                    Reply::Unreadable(at) if at == slot => said.push("unreadable".to_string()),
                    Reply::Text(at, text) if at == slot => said.push(text.trim_end().to_string()),
                    _ => {}
                }
            }
            assert_eq!(said, ["unreadable", "(local history did not answer)"]);
        };
        let started = Instant::now();
        unreadable(1);
        within(started, 2500);
        let started = Instant::now();
        assert_eq!(text_of(&tx, &rx, 0), "one\n", "a commit through Git");
        unreadable(2);
        within(started, 500);
        let held = |db: &Path| {
            READING
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter(|held| *held == db)
                .count()
        };
        assert_eq!(
            (held(&db), held(&other)),
            (1, 1),
            "one helper each, still out"
        );
        drop(tx);
        worker.join().unwrap();
    }

    /// A local history another connection holds locked, or whose shared
    /// memory file cannot be opened: the commits alone, at once, and a
    /// word that local history could not be read.
    #[test]
    fn a_local_history_that_cannot_be_read_lists_the_commits_at_once() {
        use crate::history::store::Store;
        let tree = crate::project::TempTree::new("scrub-local-locked", &[("plan.txt", "one\n")]);
        let dir = tree.0.as_path();
        git(dir, &["init", "--quiet"]);
        git(dir, &["add", "plan.txt"]);
        commit_at(dir, "first", 1_700_000_000);
        let db = tree.0.join("history").join("history.db");
        let path = dir.join("plan.txt");
        {
            let store = Store::open(&db).unwrap();
            store
                .put_snapshot(&path, "one\ntwo\n", Reason::Saved, 1_700_000_050_000)
                .unwrap();
        }
        // Its shared memory file there and not readable, as another user's
        // or a damaged one would be. (One this process has open would be
        // shared with the reader, so the writer is closed first.)
        let shm = tree.0.join("history").join("history.db-shm");
        std::fs::write(&shm, []).unwrap();
        let mode = |bits: u32| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shm, std::fs::Permissions::from_mode(bits)).unwrap();
        };
        mode(0o000);
        let started = Instant::now();
        let (listed, tx, _rx, worker) = start(&path, &db, "one\ntwo\n", Filter::All);
        mode(0o600);
        let (all, _, unavailable) = listed.expect("the commits");
        assert!(all.len() == 1 && matches!(all[0].source, Source::Commit));
        assert!(unavailable, "an unreadable history is said");
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(tx);
        worker.join().unwrap();

        // Locked by another connection: no waiting out its busy timeout.
        let holder = crate::index::db::Db::open(&db).unwrap();
        holder
            .execute("PRAGMA locking_mode = EXCLUSIVE; BEGIN EXCLUSIVE;")
            .unwrap();
        let started = Instant::now();
        let (listed, tx, _rx, worker) = start(&path, &db, "one\ntwo\n", Filter::All);
        let elapsed = started.elapsed();
        let (all, _, unavailable) = listed.expect("the commits");
        assert!(all.len() == 1 && matches!(all[0].source, Source::Commit));
        assert!(unavailable, "a locked history is said");
        assert!(elapsed < Duration::from_secs(2), "waited {elapsed:?}");
        drop(tx);
        worker.join().unwrap();

        // Readable again: the local version is back, and nothing is said.
        drop(holder);
        let (listed, tx, _rx, worker) = start(&path, &db, "one\ntwo\n", Filter::All);
        let (all, _, unavailable) = listed.expect("the versions");
        assert_eq!(all.len(), 2);
        assert!(!unavailable);
        drop(tx);
        worker.join().unwrap();
    }
}
