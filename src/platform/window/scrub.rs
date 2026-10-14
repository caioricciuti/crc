//! Git > File History: the file at each commit that changed it, in a
//! read-only tab beside the file's own. Left and Right step to the older
//! and the newer version, Home and End to the oldest and to the working
//! tree, Escape goes back to the file. The file's own tab is never touched.
//!
//! One worker per open history lists the versions, then reads them through
//! one `git cat-file --batch`, nearest the version shown first, with the
//! diff between each pair of neighbours: it moves the caret from one
//! version to the next and draws the gutter marks.
//!
//! A step marks what it changed for a moment: a wash behind the lines it
//! added or changed, and the lines it removed drawn where they were, all
//! fading out over [`FADE`].

use std::collections::VecDeque;

use super::*;
use crate::ide::diff::Change;
use crate::project::git::MarkKind;
use crate::project::history::{Blob, BlobReader, HISTORY_LIMIT, Version};

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
    /// The file's own text, the last version, changed.
    Worktree(Arc<str>),
}

pub(super) enum Reply {
    /// The versions, oldest first, and whether the list was cut short; or
    /// why there are none.
    Versions(Result<(Vec<Version>, bool), String>),
    Text(usize, Arc<str>),
    Diff(usize, Arc<Vec<Change>>),
    /// The changes from one version to another, as [`Job::Jump`] asked.
    Jump(usize, usize, Arc<Vec<Change>>),
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
        !commit.is_empty() && (v.commit.starts_with(commit) || commit.starts_with(&v.commit))
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
// Above the history's tab: which version it is, Older and Newer, and a
// timeline of every version with the working tree at its right end, to
// click or drag along.

const OLDER: &str = "Older";
const NEWER: &str = "Newer";

/// The history whose strip is above the active tab.
pub(super) fn strip_scrub(state: &State) -> Option<&Scrub> {
    let active = state.docs.active().id();
    state.scrub.as_ref().filter(|s| s.strip_over(active))
}

/// Where the strip's controls are.
pub(super) struct StripRects {
    pub(super) older: Viewport,
    pub(super) newer: Viewport,
    pub(super) close: Viewport,
    /// The rest of the second row, right of Newer.
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
    // On the first row, level with the pill.
    let close = Viewport {
        x: strip.x + strip.width - 16.0 - layout::UI_CONTROL,
        y: strip.y + 6.0,
        width: layout::UI_CONTROL,
        height: layout::UI_CONTROL,
    };
    let left = newer.x + newer.width + layout::UI_INSET;
    let track = Viewport {
        x: left,
        width: (strip.x + strip.width - 16.0 - left).max(0.0),
        ..older
    };
    StripRects {
        older,
        newer,
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
                "{} as open in its tab, after {count}{more} commits",
                scrub.name
            ),
            summary: String::new(),
            right: "working tree".into(),
        },
    }
}

/// What the pointer resting on slot `slot` of the timeline is told.
fn slot_tip(scrub: &Scrub, slot: usize, now: i64) -> String {
    match scrub.versions.as_ref().and_then(|v| v.get(slot)) {
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
    let pill = layout::strip_pill(out, atlas, strip, "History", theme.accent);
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
    Button::new(rects.close)
        .icon(crate::project::icons::CLOSE)
        .tone(Tone::Ghost)
        .tip("Back to the File  \u{238b}")
        .draw(out, atlas, theme);
    draw_timeline(scrub, atlas, rects.track, theme, now, out);
    layout::strip_hairline(out, atlas, strip, theme);
}

/// The timeline: a line with a tick per version, brighter where the text
/// is already read, a ring for the working tree at the right end, and the
/// thumb on the version shown. Too many versions for a tick each, and it
/// is a band instead, brighter over the versions read. While the versions
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
    } else {
        layout::push_rect(
            out,
            atlas,
            [first, mid - 1.0],
            [last - first, 2.0],
            layout::faded(ink, 0.22),
        );
        for slot in 0..count {
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
    let changes = if made {
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
    scrub.fade = fade;
    scrub.jump = None;
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
    let said = match scrub.note.take() {
        Some(note) => format!("{note}: {}", scrub.describe(target)),
        None => scrub.describe(target),
    };
    Some((said, !made))
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
            placed: goto.is_some(),
            goto,
            note: None,
            fade: None,
            jump: None,
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
    // Its folder and the root as the file system resolves them, as Git
    // gives the root: a file opened through a linked folder is still
    // inside it, and so is one gone from the disk since it was opened.
    let path = &match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => crate::platform::canonical(dir).join(name),
        _ => path.to_path_buf(),
    };
    let listed = path
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
                Request::Want {
                    center,
                    shown,
                    jobs,
                } => {
                    let mut none: HashMap<usize, ()> = HashMap::new();
                    evict(&mut texts, &mut none, center, [worktree_slot, shown]);
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
            Job::Jump(from, to) if from.max(to) <= worktree_slot => {
                let (old, new) = (load(from), load(to));
                Reply::Jump(from, to, Arc::new(crate::ide::diff::changes(&old, &new)))
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
    fn a_history_opened_at_a_commit_finds_it_by_any_prefix() {
        let version = |commit: &str| Version {
            commit: commit.repeat(40),
            author: "Robin".into(),
            time: 0,
            summary: "plant beans".into(),
            blob: "0".repeat(40),
            path: std::path::PathBuf::from("garden.txt"),
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
        let worker = std::thread::spawn(move || work(&path, held, &worker_rx, &worker_tx, &halt));
        let wait = Duration::from_secs(60);
        let (versions, capped) = match rx.recv_timeout(wait) {
            Ok(Reply::Versions(listed)) => listed.expect("versions"),
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
                    Reply::Versions(_) | Reply::Jump(..) => panic!("not asked for"),
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
        let worker =
            std::thread::spawn(move || work(&path, Arc::from(last), &worker_rx, &worker_tx, &halt));
        let listed = rx.recv_timeout(Duration::from_secs(60));
        drop(tx);
        worker.join().unwrap();
        let _ = std::fs::remove_file(&link);
        match listed {
            Ok(Reply::Versions(Ok((versions, capped)))) => {
                assert_eq!(versions.len(), 3);
                assert!(!capped);
                assert_eq!(versions[2].path, Path::new("ledger.rs"));
            }
            Ok(Reply::Versions(Err(why))) => panic!("no history: {why}"),
            _ => panic!("no versions"),
        }
    }
}
