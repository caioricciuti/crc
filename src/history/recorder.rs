//! The recorder: which versions of the open files go into local history,
//! and when, decided frame by frame and written on a thread of its own.
//!
//! The main thread only looks. Per document and frame that is an id
//! lookup, a few rope pointer compares and a clock compare; a version it
//! wants is sent as a rope clone, one `Arc` increment. Turning it into
//! text, hashing, compressing and SQLite all happen on `crc-history`, so
//! typing never waits on history, and a save never depends on it.
//!
//! When: the text a file was opened with, lazily, before its first change
//! or reload; every save; the text a reload replaced; 30 seconds after the
//! last change while unsaved, and at least every 5 minutes of changes; a
//! tab closed without saving; quitting with changes unsaved. A version
//! equal to the file's latest is not stored twice: the store checks.
//!
//! Never: untitled and generated documents, previews, versions from File
//! History, files too big to edit or over [`MAX_BYTES`], and secrets by
//! name (`.env`, keys, `~/.ssh` and the like) or by the user's own globs.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::store::{Caps, Error, Reason, Store};
use crate::complete::glob::{self, Pattern};
use crate::platform::dispatch::Wake;
use crate::text::buffer::{Buffer, DiskEvent};
use crate::text::rope::Rope;

/// Larger documents are not kept.
pub const MAX_BYTES: usize = 2 * 1024 * 1024;

/// The history thread's first prune waits this long after it starts, so
/// it never competes with opening a window.
const PRUNE_AFTER_START: Duration = Duration::from_secs(10);
/// And then it prunes again after this many new versions.
const PRUNE_EVERY: usize = 50;

/// How long changes go unrecorded.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// From the last change to an idle snapshot.
    pub idle: Duration,
    /// The longest a run of changes goes without one.
    pub every: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            idle: Duration::from_secs(30),
            every: Duration::from_secs(5 * 60),
        }
    }
}

/// What the policy reads of a document in one frame.
pub struct Seen<'a> {
    pub rope: &'a Rope,
    pub disk: (u64, DiskEvent),
    pub dirty: bool,
    /// The text last read from or written to the file.
    pub saved: Option<&'a Rope>,
}

impl<'a> Seen<'a> {
    pub fn of(buffer: &'a Buffer) -> Self {
        Seen {
            rope: &buffer.rope,
            disk: buffer.disk_event(),
            dirty: buffer.is_dirty(),
            saved: buffer.saved_rope(),
        }
    }
}

/// One document's snapshot policy, apart from any clock or thread so it
/// can be tested with made-up instants.
pub struct Policy {
    /// The text as of the last frame.
    rope: Rope,
    /// The disk event count as of the last frame.
    disk: u64,
    /// The text it was opened or reloaded with, until something sends it.
    opened: Option<Rope>,
    /// The text last sent, or last known to be on disk.
    snapped: Rope,
    dirty: bool,
    /// The last change.
    changed_at: Option<Instant>,
    /// The first change since the last snapshot.
    run_since: Option<Instant>,
}

impl Policy {
    pub fn new(seen: &Seen, now: Instant) -> Policy {
        let snapped = seen.saved.unwrap_or(seen.rope).clone();
        // Unsaved from the start, as a document restored after a crash is:
        // its text is a change like any other.
        let changed = seen.dirty && !seen.rope.same_as(&snapped);
        Policy {
            rope: seen.rope.clone(),
            disk: seen.disk.0,
            opened: seen.saved.cloned(),
            snapped,
            dirty: seen.dirty,
            changed_at: changed.then_some(now),
            run_since: changed.then_some(now),
        }
    }

    /// Looks at the document once more, as of `now`, and adds to `out`
    /// whatever it says to keep.
    pub fn step(
        &mut self,
        seen: &Seen,
        now: Instant,
        timing: &Timing,
        out: &mut Vec<(Reason, Rope)>,
    ) {
        let (count, event) = seen.disk;
        if count != self.disk {
            self.disk = count;
            match event {
                DiskEvent::Saved => {
                    self.send_opened(out);
                    out.push((Reason::Saved, seen.rope.clone()));
                }
                DiskEvent::Reloaded => {
                    // Unchanged since it was opened, the reload says the
                    // same, and says why.
                    if self.opened.as_ref().is_some_and(|o| o.same_as(&self.rope)) {
                        self.opened = None;
                    }
                    self.send_opened(out);
                    // What the reload replaced. What it brought is kept
                    // the way an opened file's text is, before it changes.
                    out.push((Reason::Reloaded, self.rope.clone()));
                    self.opened = Some(seen.rope.clone());
                }
                DiskEvent::Opened => {}
            }
            self.snapped = seen.rope.clone();
            self.changed_at = None;
            self.run_since = None;
        } else if !seen.rope.same_as(&self.rope) {
            self.send_opened(out);
            self.changed_at = Some(now);
            self.run_since.get_or_insert(now);
        }
        if !seen.rope.same_as(&self.rope) {
            self.rope = seen.rope.clone();
        }
        self.dirty = seen.dirty;
        if self.due(timing).is_some_and(|due| due <= now) {
            out.push((Reason::Idle, seen.rope.clone()));
            self.snapped = seen.rope.clone();
            self.changed_at = None;
            self.run_since = None;
        }
    }

    /// When an idle snapshot falls due, if one is waiting: unsaved changes
    /// not yet sent.
    pub fn due(&self, timing: &Timing) -> Option<Instant> {
        if !self.dirty || self.rope.same_as(&self.snapped) {
            return None;
        }
        let idle = self.changed_at.map(|at| at + timing.idle);
        let every = self.run_since.map(|at| at + timing.every);
        match (idle, every) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn send_opened(&mut self, out: &mut Vec<(Reason, Rope)>) {
        if let Some(opened) = self.opened.take() {
            out.push((Reason::Opened, opened));
        }
    }
}

/// Which paths may be kept: not a secret by name or place, and not left out
/// by the user's globs.
pub struct Filter {
    home: Option<PathBuf>,
    globs: Vec<Pattern>,
}

impl Filter {
    /// `exclude` is `local_history_exclude`: globs as in `.gitignore`,
    /// separated by commas, `~/` for `home`.
    pub fn new(home: Option<PathBuf>, exclude: &str) -> Filter {
        let globs = exclude
            .split(',')
            .map(str::trim)
            .filter_map(|glob| {
                let expanded = match (glob.strip_prefix("~/"), &home) {
                    (Some(rest), Some(home)) => format!("{}/{rest}", home.display()),
                    _ => glob.to_owned(),
                };
                glob::parse(&expanded)
            })
            .collect();
        Filter { home, globs }
    }

    pub fn allows(&self, path: &Path) -> bool {
        !secret(path, self.home.as_deref()) && !self.excluded(path)
    }

    /// Whether a glob matches the path or a folder it is in.
    fn excluded(&self, path: &Path) -> bool {
        if self.globs.is_empty() {
            return false;
        }
        let full = path.to_string_lossy();
        let relative = full.trim_start_matches('/');
        let mut ends: Vec<usize> = relative.match_indices('/').map(|(i, _)| i).collect();
        ends.push(relative.len());
        ends.iter().enumerate().any(|(n, &end)| {
            let folder = n + 1 < ends.len();
            self.globs
                .iter()
                .any(|glob| glob.matches(&relative[..end], folder))
        })
    }
}

/// Files that hold credentials more often than not, kept out of history
/// whatever the settings say.
fn secret(path: &Path, home: Option<&Path>) -> bool {
    if let Some(home) = home
        && [".ssh", ".aws", ".gnupg"]
            .iter()
            .any(|dir| path.starts_with(home.join(dir)))
    {
        return true;
    }
    let Some(name) = path.file_name() else {
        return false;
    };
    let name = name.to_string_lossy().to_lowercase();
    let env = name == ".env"
        || (name.starts_with(".env.")
            && ![".example", ".sample", ".template"]
                .iter()
                .any(|kept| name.ends_with(kept)));
    env || [".pem", ".key", ".p12", ".pfx"]
        .iter()
        .any(|ext| name.ends_with(ext))
        || ["id_rsa", "id_ed25519", "credentials"]
            .iter()
            .any(|start| name.starts_with(start))
        || name == ".netrc"
        || name == ".pgpass"
}

/// A document whose versions may be kept, path aside.
fn keepable(buffer: &Buffer) -> bool {
    buffer.path.is_some()
        && !buffer.is_read_only()
        && !buffer.is_frozen()
        && !buffer.is_view_only()
        && !buffer.is_preview_file()
        && buffer.rope.len_bytes() <= MAX_BYTES
}

/// Milliseconds since 1970, as the store keeps them.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Work for the history thread, in the order it was asked for.
pub enum Job {
    Snapshot {
        path: PathBuf,
        rope: Rope,
        reason: Reason,
        at: i64,
    },
    Rename {
        old: PathBuf,
        new: PathBuf,
    },
    /// Every file under the folder `old`, moved under `new`.
    RenameFolder {
        old: PathBuf,
        new: PathBuf,
    },
    /// Answered once everything before it is written.
    Flush(mpsc::Sender<()>),
}

/// What the history thread has to say, once each.
#[derive(Debug, PartialEq, Eq)]
pub enum Notice {
    /// The disk is full even after pruning: nothing more is recorded until
    /// crc starts again.
    Paused,
    /// The file was damaged and set aside here; a new one was started.
    Quarantined(PathBuf),
    /// It could not be opened or written, for this reason.
    Failed(String),
}

impl Notice {
    /// The status line's sentence.
    pub fn describe(&self) -> String {
        match self {
            Notice::Paused => {
                "local history is paused until crc restarts: the disk is full".to_owned()
            }
            Notice::Quarantined(aside) => format!(
                "local history was damaged; it was kept as {} and a new one started",
                aside.file_name().map_or_else(
                    || aside.display().to_string(),
                    |n| n.to_string_lossy().into_owned()
                )
            ),
            Notice::Failed(why) => format!("local history is not being kept: {why}"),
        }
    }
}

struct Worker {
    jobs: mpsc::Sender<Job>,
    notices: mpsc::Receiver<Notice>,
}

/// One document as the recorder last saw it.
struct Track {
    id: u64,
    path: Option<PathBuf>,
    /// The path may be kept, by [`Filter::allows`].
    allowed: bool,
    policy: Policy,
}

pub struct Recorder {
    db: Option<PathBuf>,
    /// Given to the history thread when it starts, to wake the window when
    /// it has something to say.
    wake: Option<Wake>,
    worker: Option<Worker>,
    on: bool,
    /// The disk filled: nothing more this session.
    paused: bool,
    filter: Filter,
    timing: Timing,
    tracks: Vec<Track>,
    /// When the next idle snapshot falls due, with no frame to notice it.
    due: Option<Instant>,
    /// The timer the window has set for `due`, so it only sets an earlier
    /// one.
    pub armed: Option<Instant>,
    /// Scratch for one document's snapshots, and for a frame's jobs.
    out: Vec<(Reason, Rope)>,
    jobs: Vec<Job>,
}

impl Recorder {
    /// A recorder writing to `db`, or to nothing when `None`. The history
    /// thread starts with the first version there is to keep.
    pub fn new(db: Option<PathBuf>, on: bool, exclude: &str, timing: Timing) -> Recorder {
        Recorder {
            db,
            wake: None,
            worker: None,
            on,
            paused: false,
            filter: Filter::new(home(), exclude),
            timing,
            tracks: Vec::new(),
            due: None,
            armed: None,
            out: Vec::new(),
            jobs: Vec::new(),
        }
    }

    /// How the history thread wakes the window.
    pub fn set_wake(&mut self, wake: Wake) {
        self.wake = Some(wake);
    }

    /// `local_history` and `local_history_exclude`, as the settings file
    /// says now.
    pub fn configure(&mut self, on: bool, exclude: &str) {
        self.on = on;
        self.filter = Filter::new(home(), exclude);
        for track in &mut self.tracks {
            track.allowed = track.path.as_deref().is_some_and(|p| self.filter.allows(p));
        }
    }

    /// Looks at every open document, once per frame. See the module notes
    /// for what that costs.
    pub fn observe<'a>(&mut self, buffers: impl Iterator<Item = &'a Buffer>, now: Instant) {
        if !self.on || self.paused {
            self.tracks.clear();
            self.due = None;
            return;
        }
        let mut seen = 0;
        let mut due: Option<Instant> = None;
        for buffer in buffers {
            let view = Seen::of(buffer);
            let at = match self.tracks[seen..].iter().position(|t| t.id == buffer.id()) {
                Some(at) => seen + at,
                None => {
                    self.tracks.push(Track {
                        id: buffer.id(),
                        path: buffer.path.clone(),
                        allowed: buffer
                            .path
                            .as_deref()
                            .is_some_and(|p| self.filter.allows(p)),
                        policy: Policy::new(&view, now),
                    });
                    self.tracks.len() - 1
                }
            };
            self.tracks.swap(seen, at);
            let track = &mut self.tracks[seen];
            seen += 1;
            if track.path != buffer.path {
                track.path = buffer.path.clone();
                track.allowed = track.path.as_deref().is_some_and(|p| self.filter.allows(p));
            }
            self.out.clear();
            track.policy.step(&view, now, &self.timing, &mut self.out);
            let Some(path) = track.path.as_ref().filter(|_| track.allowed) else {
                continue;
            };
            if !keepable(buffer) {
                continue;
            }
            if let Some(at) = track.policy.due(&self.timing) {
                due = Some(due.map_or(at, |d| d.min(at)));
            }
            if self.out.is_empty() {
                continue;
            }
            let at = now_ms();
            for (reason, rope) in self.out.drain(..) {
                self.jobs.push(Job::Snapshot {
                    path: path.clone(),
                    rope,
                    reason,
                    at,
                });
            }
        }
        // Everything past `seen` was closed since the last frame.
        self.tracks.truncate(seen);
        self.due = due;
        let mut jobs = std::mem::take(&mut self.jobs);
        for job in jobs.drain(..) {
            self.send(job);
        }
        self.jobs = jobs;
    }

    /// When an idle snapshot next falls due, as of the last look.
    pub fn due(&self) -> Option<Instant> {
        self.due
    }

    /// `buffer` is closing with changes the user chose not to save.
    pub fn closed_unsaved(&mut self, buffer: &Buffer) {
        self.keep(buffer, Reason::ClosedUnsaved);
    }

    /// `path` is about to be written by an edit made without opening it
    /// (a project-wide replace, a rename symbol): `before` is its text.
    pub fn edited_on_disk(&mut self, path: &Path, before: &Rope) {
        if before.len_bytes() <= MAX_BYTES && self.filter.allows(path) {
            self.send(Job::Snapshot {
                path: path.to_owned(),
                rope: before.clone(),
                reason: Reason::EditedOnDisk,
                at: now_ms(),
            });
        }
    }

    /// A file moved inside crc: its history goes with it.
    pub fn rename(&mut self, old: &Path, new: &Path) {
        self.send(Job::Rename {
            old: old.to_owned(),
            new: new.to_owned(),
        });
    }

    /// A folder moved inside crc: so does the history of every file in it.
    pub fn rename_folder(&mut self, old: &Path, new: &Path) {
        self.send(Job::RenameFolder {
            old: old.to_owned(),
            new: new.to_owned(),
        });
    }

    /// Quitting: what is still unsaved is kept, then everything queued is
    /// written, waiting at most `wait` for it.
    pub fn finish<'a>(&mut self, buffers: impl Iterator<Item = &'a Buffer>, wait: Duration) {
        for buffer in buffers.filter(|b| b.is_dirty()) {
            self.keep(buffer, Reason::Quit);
        }
        let Some(worker) = &self.worker else {
            return;
        };
        let (done, written) = mpsc::channel();
        if worker.jobs.send(Job::Flush(done)).is_ok() {
            let _ = written.recv_timeout(wait);
        }
    }

    /// What the history thread said since the last poll, as status-line
    /// sentences. Cheap when it said nothing.
    pub fn poll(&mut self) -> Option<String> {
        let notice = self.worker.as_ref()?.notices.try_recv().ok()?;
        if notice == Notice::Paused {
            self.paused = true;
        }
        Some(notice.describe())
    }

    fn keep(&mut self, buffer: &Buffer, reason: Reason) {
        let Some(path) = buffer.path.as_deref() else {
            return;
        };
        if keepable(buffer) && self.filter.allows(path) {
            self.send(Job::Snapshot {
                path: path.to_owned(),
                rope: buffer.rope.clone(),
                reason,
                at: now_ms(),
            });
        }
    }

    fn send(&mut self, job: Job) {
        if !self.on || self.paused {
            return;
        }
        if self.worker.is_none() {
            let Some(db) = self.db.clone() else {
                return;
            };
            self.worker = start(db, self.wake.take());
        }
        let Some(worker) = &self.worker else {
            // The thread could not start; there is nothing to send to.
            self.on = false;
            return;
        };
        if worker.jobs.send(job).is_err() {
            self.worker = None;
            self.on = false;
        }
    }
}

/// `$HOME`, resolved the way open documents' paths are.
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| crate::platform::canonical(Path::new(&home)))
}

fn start(db: PathBuf, wake: Option<Wake>) -> Option<Worker> {
    let (jobs, queue) = mpsc::channel();
    let (say, notices) = mpsc::channel();
    std::thread::Builder::new()
        .name("crc-history".into())
        .spawn(move || {
            run(&db, &queue, &|notice| {
                let _ = say.send(notice);
                if let Some(wake) = &wake {
                    wake();
                }
            })
        })
        .ok()?;
    Some(Worker { jobs, notices })
}

/// The history thread: opens the store, then writes what it is sent until
/// the recorder goes.
pub fn run(db: &Path, queue: &mpsc::Receiver<Job>, say: &dyn Fn(Notice)) {
    let mut writer = Writer {
        store: match Store::open(db) {
            Ok(store) => Some(store),
            Err(e) => {
                say(Notice::Failed(e.to_string()));
                None
            }
        },
        paused: false,
        failed: false,
        added: 0,
    };
    if let Some(aside) = writer.store.as_mut().and_then(Store::take_quarantined) {
        say(Notice::Quarantined(aside));
    }
    let prune_at = Instant::now() + PRUNE_AFTER_START;
    let mut pruned = false;
    loop {
        let job = if pruned {
            queue.recv().ok()
        } else {
            match queue.recv_timeout(prune_at.saturating_duration_since(Instant::now())) {
                Ok(job) => Some(job),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    writer.prune();
                    pruned = true;
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => None,
            }
        };
        let Some(job) = job else {
            return;
        };
        let mut batch = vec![job];
        batch.extend(queue.try_iter());
        for job in coalesce(batch) {
            writer.handle(job, say);
        }
    }
}

/// `batch` without the idle snapshots a later idle snapshot of the same
/// file replaces. Nothing is dropped across a rename.
fn coalesce(batch: Vec<Job>) -> Vec<Job> {
    let keep: Vec<bool> = (0..batch.len())
        .map(|i| {
            let Job::Snapshot {
                path,
                reason: Reason::Idle,
                ..
            } = &batch[i]
            else {
                return true;
            };
            for later in &batch[i + 1..] {
                match later {
                    Job::Rename { .. } | Job::RenameFolder { .. } => return true,
                    Job::Snapshot {
                        path: other,
                        reason: Reason::Idle,
                        ..
                    } if other == path => return false,
                    _ => {}
                }
            }
            true
        })
        .collect();
    batch
        .into_iter()
        .zip(keep)
        .filter_map(|(job, keep)| keep.then_some(job))
        .collect()
}

struct Writer {
    /// `None` when it could not be opened.
    store: Option<Store>,
    paused: bool,
    /// A failure was reported; the next ones are not.
    failed: bool,
    /// Versions added since the last prune.
    added: usize,
}

impl Writer {
    /// A store that takes writes: open, not from a newer crc, not paused.
    fn writable(&self) -> Option<&Store> {
        self.store
            .as_ref()
            .filter(|store| !store.read_only() && !self.paused)
    }

    fn handle(&mut self, job: Job, say: &dyn Fn(Notice)) {
        match job {
            Job::Snapshot {
                path,
                rope,
                reason,
                at,
            } => self.put(&path, &rope, reason, at, say),
            Job::Rename { old, new } => {
                if let Some(store) = self.writable() {
                    let result = store.rename(&old, &new);
                    self.check(result, say);
                }
            }
            Job::RenameFolder { old, new } => {
                if let Some(store) = self.writable() {
                    let result = store.rename_folder(&old, &new);
                    self.check(result, say);
                }
            }
            Job::Flush(done) => {
                let _ = done.send(());
            }
        }
    }

    fn put(&mut self, path: &Path, rope: &Rope, reason: Reason, at: i64, say: &dyn Fn(Notice)) {
        let Some(store) = self.writable() else {
            return;
        };
        let text = rope.to_string();
        let mut result = store.put_snapshot(path, &text, reason, at);
        if matches!(result, Err(Error::Full(_))) {
            // Once more with what retention would drop gone.
            let _ = store.prune(now_ms(), &Caps::default());
            result = store.put_snapshot(path, &text, reason, at);
            if matches!(result, Err(Error::Full(_))) {
                self.paused = true;
                say(Notice::Paused);
                return;
            }
        }
        if let Ok(Some(_)) = result {
            self.added += 1;
            if self.added >= PRUNE_EVERY {
                self.prune();
            }
        }
        self.check(result.map(|_| ()), say);
    }

    fn prune(&mut self) {
        self.added = 0;
        if let Some(store) = self.writable() {
            let _ = store.prune(now_ms(), &Caps::default());
        }
    }

    /// Says the first failure; a read-only store is not one.
    fn check(&mut self, result: Result<(), Error>, say: &dyn Fn(Notice)) {
        match result {
            Ok(()) | Err(Error::ReadOnly) => {}
            Err(e) if !self.failed => {
                self.failed = true;
                say(Notice::Failed(e.to_string()));
            }
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A document as the policy sees it, with a clock to move.
    struct Doc {
        rope: Rope,
        saved: Option<Rope>,
        disk: (u64, DiskEvent),
        dirty: bool,
    }

    impl Doc {
        fn opened(text: &str) -> Doc {
            let rope = Rope::from_text(text);
            Doc {
                saved: Some(rope.clone()),
                rope,
                disk: (0, DiskEvent::Opened),
                dirty: false,
            }
        }
        fn seen(&self) -> Seen<'_> {
            Seen {
                rope: &self.rope,
                disk: self.disk,
                dirty: self.dirty,
                saved: self.saved.as_ref(),
            }
        }
        fn type_text(&mut self, text: &str) {
            let at = self.rope.len_bytes();
            self.rope.insert(at, text);
            self.dirty = true;
        }
        fn save(&mut self) {
            self.saved = Some(self.rope.clone());
            self.disk = (self.disk.0 + 1, DiskEvent::Saved);
            self.dirty = false;
        }
        fn reload(&mut self, text: &str) {
            self.rope = Rope::from_text(text);
            self.saved = Some(self.rope.clone());
            self.disk = (self.disk.0 + 1, DiskEvent::Reloaded);
            self.dirty = false;
        }
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// Steps `policy` at `at`, returning what it kept as (reason, text).
    fn step(policy: &mut Policy, doc: &Doc, at: Instant) -> Vec<(Reason, String)> {
        let mut out = Vec::new();
        policy.step(&doc.seen(), at, &Timing::default(), &mut out);
        out.into_iter()
            .map(|(r, rope)| (r, rope.to_string()))
            .collect()
    }

    #[test]
    fn the_opened_text_is_kept_before_the_first_change() {
        let t0 = Instant::now();
        let mut doc = Doc::opened("one\n");
        let mut policy = Policy::new(&doc.seen(), t0);
        assert!(
            step(&mut policy, &doc, t0 + secs(1)).is_empty(),
            "looking keeps nothing"
        );
        assert!(step(&mut policy, &doc, t0 + secs(600)).is_empty());
        doc.type_text("two\n");
        assert_eq!(
            step(&mut policy, &doc, t0 + secs(601)),
            [(Reason::Opened, "one\n".into())]
        );
        doc.type_text("three\n");
        assert!(step(&mut policy, &doc, t0 + secs(602)).is_empty(), "once");
    }

    #[test]
    fn every_save_is_kept() {
        let t0 = Instant::now();
        let mut doc = Doc::opened("a\n");
        let mut policy = Policy::new(&doc.seen(), t0);
        doc.type_text("b\n");
        step(&mut policy, &doc, t0);
        doc.save();
        assert_eq!(
            step(&mut policy, &doc, t0 + secs(1)),
            [(Reason::Saved, "a\nb\n".into())]
        );
        doc.save();
        assert_eq!(
            step(&mut policy, &doc, t0 + secs(2)),
            [(Reason::Saved, "a\nb\n".into())],
            "a save with nothing new is still a save; the store drops the repeat"
        );
        // Saved, nothing unsaved: no idle snapshot later.
        assert!(step(&mut policy, &doc, t0 + secs(100)).is_empty());
        assert_eq!(policy.due(&Timing::default()), None);
    }

    #[test]
    fn a_save_without_a_change_keeps_the_opened_text_first() {
        let t0 = Instant::now();
        let mut doc = Doc::opened("a\n");
        let mut policy = Policy::new(&doc.seen(), t0);
        doc.save();
        let kept: Vec<Reason> = step(&mut policy, &doc, t0)
            .into_iter()
            .map(|k| k.0)
            .collect();
        assert_eq!(kept, [Reason::Opened, Reason::Saved]);
    }

    #[test]
    fn idle_is_kept_30_seconds_after_the_last_change() {
        let t0 = Instant::now();
        let mut doc = Doc::opened("a\n");
        let mut policy = Policy::new(&doc.seen(), t0);
        doc.type_text("b");
        step(&mut policy, &doc, t0);
        doc.type_text("c");
        assert!(step(&mut policy, &doc, t0 + secs(10)).is_empty());
        assert_eq!(policy.due(&Timing::default()), Some(t0 + secs(40)));
        assert!(step(&mut policy, &doc, t0 + secs(39)).is_empty());
        assert_eq!(
            step(&mut policy, &doc, t0 + secs(40)),
            [(Reason::Idle, "a\nbc".into())]
        );
        assert_eq!(policy.due(&Timing::default()), None, "nothing new since");
        assert!(step(&mut policy, &doc, t0 + secs(200)).is_empty());
    }

    #[test]
    fn continuous_changes_are_kept_every_5_minutes() {
        let t0 = Instant::now();
        let mut doc = Doc::opened("");
        let mut policy = Policy::new(&doc.seen(), t0);
        let mut idle = Vec::new();
        // A change every 10 seconds for 12 minutes: never 30 seconds idle.
        for n in 0..=72 {
            doc.type_text("x");
            let at = t0 + secs(n * 10);
            idle.extend(
                step(&mut policy, &doc, at)
                    .into_iter()
                    .filter(|k| k.0 == Reason::Idle)
                    .map(|_| n * 10),
            );
        }
        assert_eq!(idle, [300, 610]);
    }

    #[test]
    fn a_reload_keeps_the_text_it_replaced() {
        let t0 = Instant::now();
        let mut doc = Doc::opened("mine\n");
        let mut policy = Policy::new(&doc.seen(), t0);
        doc.reload("theirs\n");
        assert_eq!(
            step(&mut policy, &doc, t0 + secs(1)),
            [(Reason::Reloaded, "mine\n".into())]
        );
        // What the reload brought is kept before it is changed.
        doc.type_text("more\n");
        assert_eq!(
            step(&mut policy, &doc, t0 + secs(2)),
            [(Reason::Opened, "theirs\n".into())]
        );
    }

    #[test]
    fn a_reload_over_unsaved_changes_keeps_them() {
        let t0 = Instant::now();
        let mut doc = Doc::opened("a\n");
        let mut policy = Policy::new(&doc.seen(), t0);
        doc.type_text("unsaved\n");
        step(&mut policy, &doc, t0);
        doc.reload("disk\n");
        assert_eq!(
            step(&mut policy, &doc, t0 + secs(1)),
            [(Reason::Reloaded, "a\nunsaved\n".into())]
        );
        assert_eq!(policy.due(&Timing::default()), None);
    }

    #[test]
    fn a_document_unsaved_from_the_start_is_kept_when_idle() {
        let t0 = Instant::now();
        let saved = Rope::from_text("disk\n");
        let rope = Rope::from_text("recovered\n");
        let doc = Doc {
            rope,
            saved: Some(saved),
            disk: (0, DiskEvent::Opened),
            dirty: true,
        };
        let mut policy = Policy::new(&doc.seen(), t0);
        assert!(step(&mut policy, &doc, t0 + secs(29)).is_empty());
        assert_eq!(
            step(&mut policy, &doc, t0 + secs(30)),
            [(Reason::Idle, "recovered\n".into())]
        );
    }

    #[test]
    fn undone_to_saved_is_not_idle() {
        let t0 = Instant::now();
        let mut doc = Doc::opened("a\n");
        let mut policy = Policy::new(&doc.seen(), t0);
        doc.type_text("b");
        step(&mut policy, &doc, t0);
        doc.rope = doc.saved.clone().unwrap();
        doc.dirty = false;
        step(&mut policy, &doc, t0 + secs(1));
        assert!(step(&mut policy, &doc, t0 + secs(60)).is_empty());
    }

    #[test]
    fn secrets_are_never_kept() {
        let home = Path::new("/home/someone");
        let filter = Filter::new(Some(home.to_owned()), "");
        for kept in [
            "/p/.env.example",
            "/p/.env.sample",
            "/p/.env.template",
            "/p/src/main.rs",
            "/p/environment.md",
            "/p/keys.md",
            "/home/someone/notes/ssh.txt",
        ] {
            assert!(filter.allows(Path::new(kept)), "{kept}");
        }
        for secret in [
            "/p/.env",
            "/p/.env.local",
            "/p/.ENV.production",
            "/p/cert.pem",
            "/p/server.key",
            "/p/store.p12",
            "/p/store.pfx",
            "/p/id_rsa",
            "/p/id_ed25519.pub",
            "/p/.netrc",
            "/p/.pgpass",
            "/p/credentials.json",
            "/home/someone/.ssh/config",
            "/home/someone/.aws/config",
            "/home/someone/.gnupg/gpg.conf",
        ] {
            assert!(!filter.allows(Path::new(secret)), "{secret}");
        }
    }

    #[test]
    fn the_users_globs_leave_files_out() {
        let filter = Filter::new(
            Some(PathBuf::from("/home/someone")),
            "*.log, private/, ~/journal/**, /srv/app/data.txt,",
        );
        for out in [
            "/p/build.log",
            "/p/private/plan.md",
            "/p/a/private/b/c.txt",
            "/home/someone/journal/2026/10.md",
            "/srv/app/data.txt",
        ] {
            assert!(!filter.allows(Path::new(out)), "{out}");
        }
        for kept in [
            "/p/log.md",
            "/p/private.md",
            "/home/someone/notes/journal.md",
            "/srv/other/data.txt",
        ] {
            assert!(filter.allows(Path::new(kept)), "{kept}");
        }
        assert!(Filter::new(None, "").allows(Path::new("/p/a.rs")));
    }

    /// A fresh folder for one test, its database not created yet.
    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("crc-recorder-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open(path: &Path) -> Buffer {
        Buffer::open(path.to_owned()).unwrap()
    }

    /// The recorded reasons for `path`, oldest first.
    fn reasons(db: &Path, path: &Path) -> Vec<Reason> {
        let store = Store::open(db).unwrap();
        let mut list: Vec<Reason> = store.list(path).unwrap().iter().map(|e| e.reason).collect();
        list.reverse();
        list
    }

    fn recorder(db: &Path, exclude: &str, timing: Timing) -> Recorder {
        Recorder::new(Some(db.to_owned()), true, exclude, timing)
    }

    #[test]
    fn the_recorder_keeps_what_the_policy_says_on_its_thread() {
        let dir = temp("keeps");
        let db = dir.join("history.db");
        let file = dir.join("notes.txt");
        std::fs::write(&file, "one\n").unwrap();
        let file = crate::platform::canonical(&file);
        let mut buffer = open(&file);
        let mut recorder = recorder(&db, "", Timing::default());
        let t0 = Instant::now();
        recorder.observe(std::iter::once(&buffer), t0);
        assert!(
            recorder.worker.is_none(),
            "nothing to keep, nothing started"
        );
        buffer.insert("two ");
        recorder.observe(std::iter::once(&buffer), t0);
        assert_eq!(recorder.due(), Some(t0 + secs(30)));
        recorder.observe(std::iter::once(&buffer), t0 + secs(30));
        buffer.save(None).unwrap();
        recorder.observe(std::iter::once(&buffer), t0 + secs(31));
        buffer.insert("three ");
        recorder.observe(std::iter::once(&buffer), t0 + secs(32));
        recorder.closed_unsaved(&buffer);
        recorder.finish(std::iter::empty(), secs(10));
        assert_eq!(
            reasons(&db, &file),
            [
                Reason::Opened,
                Reason::Idle,
                // The save is the idle text: not stored twice.
                Reason::ClosedUnsaved
            ]
        );
        let store = Store::open(&db).unwrap();
        let latest = store.list(&file).unwrap()[0].id;
        assert_eq!(
            store.read(latest).unwrap().as_deref(),
            Some("two three one\n")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn documents_that_are_not_kept_send_nothing() {
        let dir = temp("skipped");
        let db = dir.join("history.db");
        let env = dir.join(".env");
        let log = dir.join("run.log");
        let big = dir.join("big.txt");
        let frozen = dir.join("old.txt");
        std::fs::write(&env, "TOKEN=1\n").unwrap();
        std::fs::write(&log, "started\n").unwrap();
        std::fs::write(&big, "x".repeat(MAX_BYTES)).unwrap();
        std::fs::write(&frozen, "then\n").unwrap();
        let mut docs = vec![open(&env), open(&log), open(&big), open(&frozen)];
        docs[3].set_frozen(true);
        docs.push(Buffer::from_text("untitled\n"));
        docs.push(Buffer::generated("notes @ abc", "txt", "generated\n"));
        let mut recorder = recorder(&db, "*.log", Timing::default());
        let t0 = Instant::now();
        recorder.observe(docs.iter(), t0);
        for doc in &mut docs {
            doc.insert("more ");
        }
        // Past the limit now, with the edit.
        assert!(docs[2].rope.len_bytes() > MAX_BYTES);
        recorder.observe(docs.iter(), t0 + secs(1));
        recorder.observe(docs.iter(), t0 + secs(60));
        for doc in &docs {
            recorder.closed_unsaved(doc);
        }
        assert_eq!(recorder.due(), None, "no timer for what is not kept");
        assert!(recorder.worker.is_none(), "nothing was sent");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_rename_takes_the_history_along() {
        let dir = temp("rename");
        let db = dir.join("history.db");
        let file = dir.join("a.txt");
        std::fs::write(&file, "a\n").unwrap();
        let file = crate::platform::canonical(&file);
        let mut buffer = open(&file);
        let mut recorder = recorder(&db, "", Timing::default());
        let t0 = Instant::now();
        recorder.observe(std::iter::once(&buffer), t0);
        buffer.insert("b");
        recorder.observe(std::iter::once(&buffer), t0);
        let moved = dir.join("b.txt");
        recorder.rename(&file, &moved);
        buffer.path = Some(moved.clone());
        recorder.finish(std::iter::once(&buffer), secs(10));
        assert_eq!(reasons(&db, &file), []);
        assert_eq!(reasons(&db, &moved), [Reason::Opened, Reason::Quit]);

        // A folder: every file under it, queued idle snapshots of it
        // written before the move rather than merged across it.
        let folder = dir.join("folder");
        let inside = folder.join("c.txt");
        recorder.edited_on_disk(&inside, &Rope::from_text("c\n"));
        let renamed = dir.join("renamed");
        recorder.rename_folder(&folder, &renamed);
        recorder.finish(std::iter::empty(), secs(10));
        assert_eq!(reasons(&db, &inside), []);
        assert_eq!(reasons(&db, &renamed.join("c.txt")), [Reason::EditedOnDisk]);
        assert_eq!(
            reasons(&db, &moved),
            [Reason::Opened, Reason::Quit],
            "not inside"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn edits_on_disk_keep_the_text_before() {
        let dir = temp("on-disk");
        let db = dir.join("history.db");
        let mut recorder = recorder(&db, "", Timing::default());
        let file = dir.join("a.txt");
        recorder.edited_on_disk(&file, &Rope::from_text("before\n"));
        recorder.edited_on_disk(&dir.join(".env"), &Rope::from_text("SECRET=1\n"));
        recorder.finish(std::iter::empty(), secs(10));
        assert_eq!(reasons(&db, &file), [Reason::EditedOnDisk]);
        assert_eq!(reasons(&db, &dir.join(".env")), []);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn off_records_nothing() {
        let dir = temp("off");
        let db = dir.join("history.db");
        let file = dir.join("a.txt");
        std::fs::write(&file, "a\n").unwrap();
        let mut buffer = open(&file);
        let mut recorder = Recorder::new(Some(db.clone()), false, "", Timing::default());
        let t0 = Instant::now();
        recorder.observe(std::iter::once(&buffer), t0);
        buffer.insert("b");
        recorder.observe(std::iter::once(&buffer), t0);
        recorder.closed_unsaved(&buffer);
        assert!(recorder.worker.is_none());
        assert!(!db.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn queued_idle_snapshots_of_a_file_are_coalesced() {
        let snap = |path: &str, reason| Job::Snapshot {
            path: PathBuf::from(path),
            rope: Rope::new(),
            reason,
            at: 0,
        };
        let batch = vec![
            snap("/a", Reason::Idle),
            snap("/b", Reason::Idle),
            snap("/a", Reason::Idle),
            snap("/a", Reason::Saved),
            snap("/b", Reason::Idle),
            Job::Rename {
                old: "/b".into(),
                new: "/c".into(),
            },
            snap("/b", Reason::Idle),
        ];
        let kept: Vec<(String, i64)> = coalesce(batch)
            .into_iter()
            .map(|job| match job {
                Job::Snapshot { path, reason, .. } => (path.display().to_string(), reason.id()),
                Job::Rename { .. } | Job::RenameFolder { .. } => ("rename".into(), 0),
                Job::Flush(_) => ("flush".into(), 0),
            })
            .collect();
        let idle = Reason::Idle.id();
        assert_eq!(
            kept,
            [
                ("/a".into(), idle),
                ("/a".into(), Reason::Saved.id()),
                ("/b".into(), idle),
                ("rename".into(), 0),
                ("/b".into(), idle),
            ]
        );
    }

    #[test]
    fn a_store_that_cannot_open_is_said_once() {
        let dir = temp("cannot-open");
        // A folder where the file should be.
        let db = dir.join("history.db");
        std::fs::create_dir_all(&db).unwrap();
        let (jobs, queue) = mpsc::channel();
        let said = std::sync::Mutex::new(Vec::new());
        let file = dir.join("a.txt");
        for _ in 0..2 {
            jobs.send(Job::Snapshot {
                path: file.clone(),
                rope: Rope::from_text("a"),
                reason: Reason::Saved,
                at: 0,
            })
            .unwrap();
        }
        drop(jobs);
        run(&db, &queue, &|notice| said.lock().unwrap().push(notice));
        let said = said.into_inner().unwrap();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(matches!(said[0], Notice::Failed(_)));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
