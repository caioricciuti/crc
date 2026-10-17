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
//!
//! Undo too: a clean document's undo and redo stacks, each time they or
//! its file move, as a journal against the saved text. A document dirty
//! at quit keeps its quit snapshot and no journal. A file opened clean
//! and untouched asks the thread for its journal; the answer is taken
//! only if the document is still untouched and the text on disk is the
//! one the journal was written against. Opening never waits for it.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::store::{Caps, Error, Reason, Store};
use crate::complete::glob::{self, Pattern};
use crate::platform::dispatch::Wake;
use crate::text::buffer::{Buffer, DiskEvent};
use crate::text::rope::Rope;
use crate::text::undo_journal::{self, Journal};

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
    /// A clean document's undo and redo stacks, against its text `base`.
    Undo {
        path: PathBuf,
        base: Rope,
        undo: Vec<undo_journal::Step>,
        redo: Vec<undo_journal::Step>,
        at: i64,
    },
    /// The journal kept for `path`, wanted by document `id` while its text
    /// is `base` and its disk event count `event`.
    LoadUndo {
        id: u64,
        path: PathBuf,
        base: Rope,
        event: u64,
    },
    /// Everything kept for `path`: its versions and its journal.
    Forget {
        path: PathBuf,
    },
    /// Everything kept, for every file.
    ForgetAll,
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
    /// A clear was asked of a history from a newer crc, which is
    /// read-only here: nothing went.
    NotCleared,
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
            Notice::NotCleared => "local history was written by a newer crc and is \
                                   read-only here: nothing was cleared"
                .to_owned(),
        }
    }
}

/// A journal found for a document that asked, as [`Job::LoadUndo`] asked.
pub struct Restored {
    pub id: u64,
    pub path: PathBuf,
    pub event: u64,
    pub base: Rope,
    pub journal: Journal,
}

struct Worker {
    jobs: mpsc::Sender<Job>,
    notices: mpsc::Receiver<Notice>,
    restored: mpsc::Receiver<Restored>,
}

/// One document as the recorder last saw it.
struct Track {
    id: u64,
    path: Option<PathBuf>,
    /// The path may be kept, by [`Filter::allows`].
    allowed: bool,
    policy: Policy,
    /// The undo count and disk event count as of the last journal sent,
    /// or as first seen.
    journaled: (u64, u64),
}

/// Whether `buffer`'s journal is to be sent: it is clean, its stacks or its
/// file moved since `journaled`, which it updates, and the stacks hold
/// something.
fn undo_due(journaled: &mut (u64, u64), buffer: &Buffer) -> bool {
    let now = (buffer.undo_rev(), buffer.disk_event().0);
    if buffer.is_dirty() || now == *journaled {
        return false;
    }
    *journaled = now;
    buffer.undo_len() > 0 || buffer.redo_len() > 0
}

/// `buffer`'s journal, as of now.
fn undo_job(buffer: &Buffer, path: &Path) -> Job {
    let (undo, redo) = buffer.undo_steps();
    Job::Undo {
        path: path.to_owned(),
        base: buffer.rope.clone(),
        undo,
        redo,
        at: now_ms(),
    }
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
    /// Whether `db` was there when a journal was first asked for, before
    /// anything started the history thread: without it there is none to
    /// load, and asking would create it.
    db_exists: Option<bool>,
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
            db_exists: None,
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
            let mut fresh = false;
            let at = match self.tracks[seen..].iter().position(|t| t.id == buffer.id()) {
                Some(at) => seen + at,
                None => {
                    fresh = true;
                    self.tracks.push(Track {
                        id: buffer.id(),
                        path: buffer.path.clone(),
                        allowed: buffer
                            .path
                            .as_deref()
                            .is_some_and(|p| self.filter.allows(p)),
                        policy: Policy::new(&view, now),
                        journaled: (buffer.undo_rev(), buffer.disk_event().0),
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
            if fresh
                && !buffer.is_dirty()
                && buffer.undo_len() == 0
                && buffer.redo_len() == 0
                && let Some(saved) = buffer.saved_rope()
            {
                self.jobs.push(Job::LoadUndo {
                    id: buffer.id(),
                    path: path.clone(),
                    base: saved.clone(),
                    event: buffer.disk_event().0,
                });
            }
            if !self.out.is_empty() {
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
            // After the save's snapshot: the journal and the version share
            // one blob for the text.
            if undo_due(&mut track.journaled, buffer) {
                self.jobs.push(undo_job(buffer, path));
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

    /// `buffer`'s tab closed: its journal, when it moved since the last
    /// frame looked.
    pub fn closed(&mut self, buffer: &Buffer) {
        // Its path now, not as of the last frame: a Save As may be newer.
        let Some(path) = buffer.path.as_deref() else {
            return;
        };
        if !self.on || self.paused || !keepable(buffer) || !self.filter.allows(path) {
            return;
        }
        let Some(track) = self.tracks.iter_mut().find(|t| t.id == buffer.id()) else {
            return;
        };
        if undo_due(&mut track.journaled, buffer) {
            let job = undo_job(buffer, path);
            self.send(job);
        }
    }

    /// Takes the journals the history thread found, for the documents still
    /// as they were when they asked: same file, same text on disk, nothing
    /// unsaved, nothing to undo or redo. Returns whether any was taken.
    pub fn adopt<'a>(&mut self, buffers: impl Iterator<Item = &'a mut Buffer>) -> bool {
        let Some(worker) = &self.worker else {
            return false;
        };
        let mut found: Vec<Restored> = worker.restored.try_iter().collect();
        if found.is_empty() {
            return false;
        }
        let mut adopted = false;
        for buffer in buffers {
            let Some(at) = found.iter().position(|r| r.id == buffer.id()) else {
                continue;
            };
            let restored = found.swap_remove(at);
            let untouched = buffer.path.as_deref() == Some(restored.path.as_path())
                && !buffer.is_dirty()
                && buffer.undo_len() == 0
                && buffer.redo_len() == 0
                && buffer.disk_event().0 == restored.event
                && buffer
                    .saved_rope()
                    .is_some_and(|saved| saved.same_as(&restored.base));
            if !untouched || !buffer.adopt_undo(restored.journal, &restored.base) {
                continue;
            }
            adopted = true;
            // Taken, not changed: nothing new to write.
            if let Some(track) = self.tracks.iter_mut().find(|t| t.id == buffer.id()) {
                track.journaled = (buffer.undo_rev(), restored.event);
            }
            if found.is_empty() {
                break;
            }
        }
        adopted
    }

    /// `buffer`'s text was just replaced by a version from its history:
    /// `before`, what it held, is kept.
    pub fn before_restore(&mut self, buffer: &Buffer, before: &Rope) {
        let Some(path) = buffer.path.as_deref() else {
            return;
        };
        if keepable(buffer) && before.len_bytes() <= MAX_BYTES && self.filter.allows(path) {
            self.send(Job::Snapshot {
                path: path.to_owned(),
                rope: before.clone(),
                reason: Reason::BeforeRestore,
                at: now_ms(),
            });
        }
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

    /// Clear History for This File: what is kept for `path` goes, versions
    /// and journal. Asked for even with history off or paused; with no
    /// history file yet there is nothing to clear, and none is made.
    pub fn forget(&mut self, path: &Path) {
        for track in &mut self.tracks {
            if track.path.as_deref() == Some(path) {
                track.policy.opened = None;
            }
        }
        self.clear(Job::Forget {
            path: path.to_owned(),
        });
    }

    /// Clear All Local History, as [`Recorder::forget`] for every file.
    pub fn forget_all(&mut self) {
        for track in &mut self.tracks {
            track.policy.opened = None;
        }
        self.clear(Job::ForgetAll);
    }

    /// Sends a clear, starting the history thread for it only when there
    /// is a history file to clear.
    fn clear(&mut self, job: Job) {
        if self.worker.is_none() {
            let Some(db) = self.db.clone() else {
                return;
            };
            if !db.exists() {
                return;
            }
            self.worker = start(db, self.wake.take());
        }
        let Some(worker) = &self.worker else {
            return;
        };
        if worker.jobs.send(job).is_err() {
            self.worker = None;
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
        for buffer in buffers {
            if buffer.is_dirty() {
                self.keep(buffer, Reason::Quit);
            } else {
                self.closed(buffer);
            }
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
            if matches!(job, Job::LoadUndo { .. })
                && !*self.db_exists.get_or_insert_with(|| db.exists())
            {
                return;
            }
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
    let (found, restored) = mpsc::channel();
    std::thread::Builder::new()
        .name("crc-history".into())
        .spawn(move || {
            let wake = || {
                if let Some(wake) = &wake {
                    wake();
                }
            };
            run(
                &db,
                &queue,
                &|notice| {
                    let _ = say.send(notice);
                    wake();
                },
                &|journal| {
                    let _ = found.send(journal);
                    wake();
                },
            )
        })
        .ok()?;
    Some(Worker {
        jobs,
        notices,
        restored,
    })
}

/// The history thread: opens the store, then writes what it is sent until
/// the recorder goes. Journals it finds go to `found`.
pub fn run(db: &Path, queue: &mpsc::Receiver<Job>, say: &dyn Fn(Notice), found: &dyn Fn(Restored)) {
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
            writer.handle(job, say, found);
        }
    }
}

/// `batch` without the idle snapshots a later idle snapshot of the same
/// file replaces, and the journals a later journal of it replaces.
/// Nothing is dropped across a rename or a clear.
fn coalesce(batch: Vec<Job>) -> Vec<Job> {
    let keep: Vec<bool> = (0..batch.len())
        .map(|i| {
            let (path, journal) = match &batch[i] {
                Job::Snapshot {
                    path,
                    reason: Reason::Idle,
                    ..
                } => (path, false),
                Job::Undo { path, .. } => (path, true),
                _ => return true,
            };
            for later in &batch[i + 1..] {
                match later {
                    Job::Rename { .. }
                    | Job::RenameFolder { .. }
                    | Job::Forget { .. }
                    | Job::ForgetAll => return true,
                    Job::Snapshot {
                        path: other,
                        reason: Reason::Idle,
                        ..
                    } if !journal && other == path => return false,
                    Job::Undo { path: other, .. } if journal && other == path => return false,
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

    fn handle(&mut self, job: Job, say: &dyn Fn(Notice), found: &dyn Fn(Restored)) {
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
            Job::Undo {
                path,
                base,
                undo,
                redo,
                at,
            } => {
                if let Some(store) = self.writable() {
                    let journal = undo_journal::encode(&base, &undo, &redo);
                    let result = store.put_undo(&path, &base.to_string(), &journal, at);
                    self.check(result, say);
                }
            }
            Job::LoadUndo {
                id,
                path,
                base,
                event,
            } => {
                if let Some(journal) = self.load_undo(&path, &base) {
                    found(Restored {
                        id,
                        path,
                        event,
                        base,
                        journal,
                    });
                }
            }
            Job::Forget { path } => {
                if let Some(store) = self.clearable(say) {
                    let result = store.forget(&path);
                    self.check(result, say);
                }
            }
            Job::ForgetAll => {
                if let Some(store) = self.clearable(say) {
                    let result = store.forget_all();
                    self.check(result, say);
                }
            }
            Job::Flush(done) => {
                let _ = done.send(());
            }
        }
    }

    /// `path`'s journal, when one was written against `base`'s text and
    /// reads back whole.
    fn load_undo(&self, path: &Path, base: &Rope) -> Option<Journal> {
        let undo = self.store.as_ref()?.get_undo(path).ok()??;
        let text = base.to_string();
        if crate::ext::verify::sha256(text.as_bytes())? != undo.base {
            return None;
        }
        undo_journal::decode(&undo.journal, base)
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

    /// A store a clear can go to: open and not from a newer crc, which is
    /// said, as the window has already said "cleared". A full disk does
    /// not stop it, as clearing is how room is made.
    fn clearable(&self, say: &dyn Fn(Notice)) -> Option<&Store> {
        let store = self.store.as_ref()?;
        if store.read_only() {
            say(Notice::NotCleared);
            return None;
        }
        Some(store)
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

    /// A restore keeps the text it replaced, typed and unsaved; the text
    /// the file was opened with, when that is the one, comes after it the
    /// same, and is not stored twice.
    #[test]
    fn a_restore_keeps_the_text_it_replaces() {
        let dir = temp("restore");
        let db = dir.join("history.db");
        let file = dir.join("a.txt");
        std::fs::write(&file, "a\n").unwrap();
        let file = crate::platform::canonical(&file);
        let mut buffer = open(&file);
        let mut recorder = recorder(&db, "", Timing::default());
        let t0 = Instant::now();
        recorder.observe(std::iter::once(&buffer), t0);
        buffer.insert("typed ");
        recorder.observe(std::iter::once(&buffer), t0);
        let before = buffer.rope.clone();
        assert!(buffer.restore_text("then\n", 0, 0));
        recorder.before_restore(&buffer, &before);
        recorder.observe(std::iter::once(&buffer), t0 + secs(1));
        // Untouched since it was opened: what it was opened with is the
        // text before the restore.
        let mut other = open(&{
            let other = dir.join("b.txt");
            std::fs::write(&other, "b\n").unwrap();
            crate::platform::canonical(&other)
        });
        recorder.observe(std::iter::once(&other), t0);
        let before = other.rope.clone();
        assert!(other.restore_text("older\n", 0, 0));
        recorder.before_restore(&other, &before);
        recorder.observe(std::iter::once(&other), t0 + secs(1));
        // Not one that is not kept.
        let mut frozen = Buffer::generated("a.txt @ abc", "txt", "x\n");
        frozen.set_frozen(true);
        recorder.before_restore(&frozen, &Rope::from_text("y\n"));
        recorder.finish(std::iter::empty(), secs(10));
        assert_eq!(reasons(&db, &file), [Reason::Opened, Reason::BeforeRestore]);
        let store = Store::open(&db).unwrap();
        let latest = store.list(&file).unwrap()[0].id;
        assert_eq!(store.read(latest).unwrap().as_deref(), Some("typed a\n"));
        let other = other.path.clone().unwrap();
        assert_eq!(reasons(&db, &other), [Reason::BeforeRestore]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// `path`'s journal as kept, decoded against `base`: (undo, redo).
    fn journal(db: &Path, path: &Path, base: &str) -> Option<(usize, usize)> {
        let undo = Store::open(db).unwrap().get_undo(path).unwrap()?;
        assert_eq!(
            undo.base,
            crate::ext::verify::sha256(base.as_bytes()).unwrap()
        );
        let journal = undo_journal::decode(&undo.journal, &Rope::from_text(base))?;
        Some((journal.undo.len(), journal.redo.len()))
    }

    #[test]
    fn a_clean_document_keeps_its_undo_and_a_dirty_one_does_not() {
        let dir = temp("undo-kept");
        let db = dir.join("history.db");
        let file = dir.join("a.txt");
        std::fs::write(&file, "a\n").unwrap();
        let file = crate::platform::canonical(&file);
        let mut buffer = open(&file);
        let mut recorder = recorder(&db, "", Timing::default());
        let t0 = Instant::now();
        recorder.observe(std::iter::once(&buffer), t0);
        buffer.insert("one ");
        recorder.observe(std::iter::once(&buffer), t0);
        buffer.save(None).unwrap();
        recorder.observe(std::iter::once(&buffer), t0 + secs(1));
        recorder.finish(std::iter::empty(), secs(10));
        assert_eq!(journal(&db, &file, "one a\n"), Some((1, 0)), "after a save");

        // Undone: unsaved, nothing written; redone, clean again with the
        // stacks moved, written again.
        buffer.undo();
        recorder.observe(std::iter::once(&buffer), t0 + secs(2));
        buffer.redo();
        buffer.place_cursor(0, crate::text::buffer::Motion::Move);
        buffer.insert("two ");
        recorder.observe(std::iter::once(&buffer), t0 + secs(3));
        assert!(buffer.is_dirty());
        recorder.finish(std::iter::once(&buffer), secs(10));
        assert_eq!(
            journal(&db, &file, "one a\n"),
            Some((1, 0)),
            "dirty at quit: the journal of the last clean text stays"
        );
        assert_eq!(*reasons(&db, &file).last().unwrap(), Reason::Quit);

        buffer.undo();
        assert!(!buffer.is_dirty());
        recorder.observe(std::iter::once(&buffer), t0 + secs(4));
        recorder.finish(std::iter::empty(), secs(10));
        assert_eq!(journal(&db, &file, "one a\n"), Some((1, 1)));

        // Closing a clean tab writes what moved since the last frame.
        buffer.undo();
        buffer.redo();
        recorder.closed(&buffer);
        recorder.finish(std::iter::empty(), secs(10));
        assert_eq!(journal(&db, &file, "one a\n"), Some((1, 1)));

        // A secret keeps no journal.
        let env = dir.join(".env");
        std::fs::write(&env, "A=1\n").unwrap();
        let env = crate::platform::canonical(&env);
        let mut secret = open(&env);
        recorder.observe(std::iter::once(&secret), t0);
        secret.insert("B=2\n");
        secret.save(None).unwrap();
        recorder.observe(std::iter::once(&secret), t0 + secs(1));
        recorder.finish(std::iter::once(&secret), secs(10));
        assert_eq!(journal(&db, &env, "B=2\nA=1\n"), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_untouched_document_takes_its_journal_back() {
        let dir = temp("undo-back");
        let db = dir.join("history.db");
        let file = dir.join("a.txt");
        std::fs::write(&file, "a\n").unwrap();
        let file = crate::platform::canonical(&file);
        let mut buffer = open(&file);
        let mut recorder = recorder(&db, "", Timing::default());
        let t0 = Instant::now();
        recorder.observe(std::iter::once(&buffer), t0);
        buffer.insert("one ");
        buffer.save(None).unwrap();
        recorder.finish(std::iter::once(&buffer), secs(10));
        drop(recorder);

        // Opened again, twice: one tab left alone, one typed in before the
        // answer came.
        let mut recorder = self::recorder(&db, "", Timing::default());
        let mut docs = [open(&file), open(&file)];
        recorder.observe(docs.iter(), t0);
        docs[1].insert("x");
        recorder.finish(std::iter::empty(), secs(10));
        assert!(recorder.adopt(docs.iter_mut()));
        assert_eq!((docs[0].undo_len(), docs[0].redo_len()), (1, 0));
        assert!(!docs[0].is_dirty());
        assert_eq!(docs[1].undo_len(), 1, "its own typing, not the journal");
        assert!(!recorder.adopt(docs.iter_mut()), "once");
        // Taken, nothing to write back.
        recorder.observe(docs.iter(), t0 + secs(1));
        assert!(docs[0].undo());
        assert_eq!(docs[0].rope.to_string(), "a\n");
        assert!(docs[0].is_dirty());

        // Changed on disk since: no journal for that text.
        std::fs::write(&file, "two\n").unwrap();
        let mut changed = [open(&file)];
        recorder.observe(changed.iter(), t0);
        recorder.finish(std::iter::empty(), secs(10));
        assert!(!recorder.adopt(changed.iter_mut()));
        assert_eq!(changed[0].undo_len(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_journal_is_found_only_for_the_text_it_was_written_against() {
        let dir = temp("undo-load");
        let db = dir.join("history.db");
        let (old, new) = (dir.join("old.txt"), dir.join("new.txt"));
        let base = Rope::from_text("same\n");
        let step = undo_journal::Step {
            rope: Rope::from_text("before\n"),
            cursors: undo_journal::Cursors::default(),
        };
        let bytes = undo_journal::encode(&base, &[step], &[]);
        Store::open(&db)
            .unwrap()
            .put_undo(&old, "same\n", &bytes, 0)
            .unwrap();
        let (jobs, queue) = mpsc::channel();
        let load = |id, path: &Path, text: &str| Job::LoadUndo {
            id,
            path: path.to_owned(),
            base: Rope::from_text(text),
            event: 0,
        };
        jobs.send(load(1, &old, "same\n")).unwrap();
        // The same length, so only the hash tells them apart.
        jobs.send(load(2, &old, "sane\n")).unwrap();
        jobs.send(Job::Rename {
            old: old.clone(),
            new: new.clone(),
        })
        .unwrap();
        jobs.send(load(3, &old, "same\n")).unwrap();
        jobs.send(load(4, &new, "same\n")).unwrap();
        drop(jobs);
        let found = std::sync::Mutex::new(Vec::new());
        run(&db, &queue, &|_| {}, &|restored| {
            found.lock().unwrap().push((
                restored.id,
                restored.journal.undo.len(),
                restored.journal.undo[0].rope.to_string(),
            ))
        });
        assert_eq!(
            found.into_inner().unwrap(),
            [(1, 1, "before\n".into()), (4, 1, "before\n".into())]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn opening_with_no_history_starts_nothing() {
        let dir = temp("undo-none");
        let db = dir.join("history.db");
        let file = dir.join("a.txt");
        std::fs::write(&file, "a\n").unwrap();
        let buffer = open(&file);
        let mut recorder = recorder(&db, "", Timing::default());
        recorder.observe(std::iter::once(&buffer), Instant::now());
        assert!(recorder.worker.is_none());
        assert!(!db.exists());
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
    fn queued_idle_snapshots_and_journals_of_a_file_are_coalesced() {
        let snap = |path: &str, reason| Job::Snapshot {
            path: PathBuf::from(path),
            rope: Rope::new(),
            reason,
            at: 0,
        };
        let undo = |path: &str| Job::Undo {
            path: PathBuf::from(path),
            base: Rope::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            at: 0,
        };
        let batch = vec![
            snap("/a", Reason::Idle),
            undo("/a"),
            snap("/b", Reason::Idle),
            undo("/b"),
            snap("/a", Reason::Idle),
            undo("/a"),
            snap("/a", Reason::Saved),
            snap("/b", Reason::Idle),
            Job::Rename {
                old: "/b".into(),
                new: "/c".into(),
            },
            snap("/b", Reason::Idle),
            undo("/b"),
        ];
        let kept: Vec<(String, i64)> = coalesce(batch)
            .into_iter()
            .map(|job| match job {
                Job::Snapshot { path, reason, .. } => (path.display().to_string(), reason.id()),
                Job::Rename { .. } | Job::RenameFolder { .. } => ("rename".into(), 0),
                Job::Undo { path, .. } => (format!("undo {}", path.display()), 0),
                Job::Forget { .. } | Job::ForgetAll => ("forget".into(), 0),
                Job::LoadUndo { .. } | Job::Flush(_) => ("other".into(), 0),
            })
            .collect();
        let idle = Reason::Idle.id();
        assert_eq!(
            kept,
            [
                ("undo /b".into(), 0),
                ("/a".into(), idle),
                ("undo /a".into(), 0),
                ("/a".into(), Reason::Saved.id()),
                ("/b".into(), idle),
                ("rename".into(), 0),
                ("/b".into(), idle),
                ("undo /b".into(), 0),
            ]
        );

        // Nor across a clear: what came before it is written, then cleared.
        for forget in [
            Job::Forget {
                path: PathBuf::from("/a"),
            },
            Job::ForgetAll,
        ] {
            let batch = vec![
                snap("/a", Reason::Idle),
                undo("/a"),
                forget,
                snap("/a", Reason::Idle),
                undo("/a"),
            ];
            assert_eq!(coalesce(batch).len(), 5);
        }
    }

    /// Versions and a journal queued for a file, then a clear: none of it
    /// is left, and what is kept after it is kept again.
    #[test]
    fn a_clear_drops_what_was_queued_before_it() {
        let dir = temp("forget");
        let db = dir.join("history.db");
        let file = dir.join("a.txt");
        let other = dir.join("b.txt");
        std::fs::write(&file, "a\n").unwrap();
        std::fs::write(&other, "b\n").unwrap();
        let (file, other) = (
            crate::platform::canonical(&file),
            crate::platform::canonical(&other),
        );
        let mut docs = [open(&file), open(&other)];
        let mut recorder = recorder(&db, "", Timing::default());
        let t0 = Instant::now();
        recorder.observe(docs.iter(), t0);
        for doc in &mut docs {
            doc.insert("one ");
            doc.save(None).unwrap();
        }
        // Queued, not yet written: the opened text, the save, the journal.
        recorder.observe(docs.iter(), t0 + secs(1));
        recorder.forget(&file);
        recorder.finish(std::iter::empty(), secs(10));
        assert_eq!(reasons(&db, &file), []);
        assert_eq!(Store::open(&db).unwrap().get_undo(&file).unwrap(), None);
        assert_eq!(reasons(&db, &other), [Reason::Opened, Reason::Saved]);
        assert!(journal(&db, &other, "one b\n").is_some());

        docs[0].insert("two ");
        docs[0].save(None).unwrap();
        recorder.observe(docs.iter(), t0 + secs(2));
        recorder.finish(std::iter::empty(), secs(10));
        assert_eq!(
            reasons(&db, &file),
            [Reason::Saved],
            "kept again, without the text it was opened with"
        );
        assert_eq!(journal(&db, &file, "one two a\n"), Some((2, 0)));

        recorder.forget_all();
        recorder.finish(std::iter::empty(), secs(10));
        let store = Store::open(&db).unwrap();
        for path in [&file, &other] {
            assert!(store.list(path).unwrap().is_empty());
            assert_eq!(store.get_undo(path).unwrap(), None);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A history from a newer crc is read-only here: a clear leaves it as
    /// it was and says so, rather than let "cleared" stand.
    #[test]
    fn a_clear_of_a_read_only_history_says_nothing_was_cleared() {
        let dir = temp("forget-read-only");
        let db = dir.join("history.db");
        let file = dir.join("a.txt");
        Store::open(&db)
            .unwrap()
            .put_snapshot(&file, "kept\n", Reason::Saved, 0)
            .unwrap();
        crate::index::db::Db::open(&db)
            .unwrap()
            .execute("PRAGMA user_version = 99")
            .unwrap();
        let (jobs, queue) = mpsc::channel();
        jobs.send(Job::Forget { path: file.clone() }).unwrap();
        jobs.send(Job::ForgetAll).unwrap();
        drop(jobs);
        let said = std::sync::Mutex::new(Vec::new());
        run(
            &db,
            &queue,
            &|notice| said.lock().unwrap().push(notice),
            &|_| {},
        );
        assert_eq!(
            said.into_inner().unwrap(),
            [Notice::NotCleared, Notice::NotCleared]
        );
        assert_eq!(
            Notice::NotCleared.describe(),
            "local history was written by a newer crc and is read-only here: nothing was cleared"
        );
        assert_eq!(reasons(&db, &file), [Reason::Saved]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// History off, or paused, still clears what an earlier session kept;
    /// with no history file, a clear makes none.
    #[test]
    fn a_clear_works_with_history_off_and_creates_nothing() {
        let dir = temp("forget-off");
        let db = dir.join("history.db");
        let file = dir.join("a.txt");
        let mut off = Recorder::new(Some(db.clone()), false, "", Timing::default());
        off.forget(&file);
        off.forget_all();
        off.finish(std::iter::empty(), secs(10));
        assert!(off.worker.is_none());
        assert!(!db.exists());

        let mut on = recorder(&db, "", Timing::default());
        on.edited_on_disk(&file, &Rope::from_text("before\n"));
        on.finish(std::iter::empty(), secs(10));
        drop(on);
        assert_eq!(reasons(&db, &file), [Reason::EditedOnDisk]);
        off.forget(&file);
        off.finish(std::iter::empty(), secs(10));
        assert_eq!(reasons(&db, &file), []);

        let mut paused = recorder(&db, "", Timing::default());
        paused.edited_on_disk(&file, &Rope::from_text("again\n"));
        paused.finish(std::iter::empty(), secs(10));
        paused.paused = true;
        paused.forget_all();
        paused.finish(std::iter::empty(), secs(10));
        assert_eq!(reasons(&db, &file), []);
        std::fs::remove_dir_all(dir).unwrap();
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
        run(
            &db,
            &queue,
            &|notice| said.lock().unwrap().push(notice),
            &|_| {},
        );
        let said = said.into_inner().unwrap();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(matches!(said[0], Notice::Failed(_)));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
