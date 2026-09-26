//! Watches the project folder with FSEvents, so the sidebar, the finder and
//! the Git panel follow what other tools do to the tree: a checkout, an
//! install, a generator, a save from another editor.
//!
//! FSEvents is a C API in CoreServices, declared here by hand the way SQLite
//! and tree-sitter are; no crate. Events arrive on a private dispatch queue,
//! are filtered there, and are handed to the main thread with
//! `dispatch_async_f`, a plain function pointer, so the editor's state is
//! only ever touched from the thread that owns it.
//!
//! What the watcher reports is coarse on purpose: "the tree changed" or
//! "the repository changed". The consumers already know how to rebuild
//! themselves from disk; they only needed to be told when.

use std::ffi::{CStr, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use objc2_core_foundation::{CFArray, CFRetained, CFString};

/// What a batch of file events amounts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Files or folders under the project changed.
    Tree,
    /// Only `.git` bookkeeping changed: a commit, a checkout, staging.
    Git,
}

type StreamRef = *mut c_void;
type Queue = *mut c_void;

#[repr(C)]
struct StreamContext {
    version: isize,
    info: *mut c_void,
    retain: Option<unsafe extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<unsafe extern "C" fn(*const c_void)>,
    copy_description: Option<unsafe extern "C" fn(*const c_void) -> *const c_void>,
}

type Callback = unsafe extern "C" fn(
    stream: *const c_void,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    ids: *const u64,
);

unsafe extern "C" {
    fn FSEventStreamCreate(
        allocator: *const c_void,
        callback: Callback,
        context: *const StreamContext,
        paths: *const c_void,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> StreamRef;
    fn FSEventStreamSetDispatchQueue(stream: StreamRef, queue: Queue);
    fn FSEventStreamStart(stream: StreamRef) -> u8;
    fn FSEventStreamStop(stream: StreamRef);
    fn FSEventStreamInvalidate(stream: StreamRef);
    fn FSEventStreamRelease(stream: StreamRef);

    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> Queue;
    fn dispatch_release(object: *mut c_void);
}

const SINCE_NOW: u64 = 0xFFFF_FFFF_FFFF_FFFF;
const FLAG_NO_DEFER: u32 = 0x02;
/// Events caused by this process are not reported. The editor refreshes
/// after its own saves and creations already; hearing about them again
/// would only rebuild the tree twice.
const FLAG_IGNORE_SELF: u32 = 0x08;
const FLAG_FILE_EVENTS: u32 = 0x10;

/// Event flags that mean "rescan": the kernel or FSEvents dropped events
/// (a burst larger than its queue), or the watched root itself changed.
/// The path is then the root, and no finer detail is coming.
const RESCAN: u32 = 0x01 | 0x02 | 0x04 | 0x20;

/// How long FSEvents coalesces before calling back, in seconds. A checkout
/// touching a thousand files becomes one callback.
const LATENCY: f64 = 0.25;

/// Runs on the main thread with what changed since the last call.
pub type OnChange = Box<dyn Fn(Change)>;

/// Shared between the queue thread and the main thread. Never freed: a
/// callback already queued for the main thread may still point at it after
/// the watcher is dropped, so it is marked closed instead. A root changes
/// a few times per session; the bytes are not worth the race.
struct Shared {
    root: PathBuf,
    /// The repository's Git directories: its own, and the common one of a
    /// linked worktree. Outside `root` when the project is a subfolder of
    /// the repository or a linked worktree, and then watched as well.
    git_dirs: Vec<PathBuf>,
    closed: AtomicBool,
    /// Tree and Git changes since the last delivery.
    pending: Mutex<(bool, bool)>,
    on_change: MainThreadOnly<OnChange>,
}

/// A value stored where another thread can see it but only the main thread
/// touches. The queue thread only ever reads `closed` and `pending`.
struct MainThreadOnly<T>(T);
unsafe impl<T> Send for MainThreadOnly<T> {}
unsafe impl<T> Sync for MainThreadOnly<T> {}

pub struct Watcher {
    stream: StreamRef,
    queue: Queue,
    shared: *const Shared,
}

impl Watcher {
    /// Starts watching `root`. `on_change` runs on the main thread, at most
    /// once per batch of events, with the coarsest change in the batch.
    pub fn new(root: &Path, on_change: OnChange) -> Option<Self> {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let git_dirs: Vec<PathBuf> = crate::project::git::git_dirs(&root)
            .into_iter()
            .map(|d| std::fs::canonicalize(&d).unwrap_or(d))
            .collect();
        let mut watched = vec![root.clone()];
        for dir in &git_dirs {
            if !dir.starts_with(&root) && !watched.contains(dir) {
                watched.push(dir.clone());
            }
        }
        let shared: *const Shared = Box::into_raw(Box::new(Shared {
            root: root.clone(),
            git_dirs,
            closed: AtomicBool::new(false),
            pending: Mutex::new((false, false)),
            on_change: MainThreadOnly(on_change),
        }));
        let context = StreamContext {
            version: 0,
            info: shared as *mut c_void,
            retain: None,
            release: None,
            copy_description: None,
        };
        let names: Vec<CFRetained<CFString>> = watched
            .iter()
            .map(|p| CFString::from_str(&p.to_string_lossy()))
            .collect();
        let paths: CFRetained<CFArray<CFString>> = CFArray::from_retained_objects(&names);
        let queue = unsafe { dispatch_queue_create(c"crc.watch".as_ptr(), std::ptr::null()) };
        let stream = unsafe {
            FSEventStreamCreate(
                std::ptr::null(),
                on_events,
                &context,
                CFRetained::as_ptr(&paths).as_ptr() as *const c_void,
                SINCE_NOW,
                LATENCY,
                FLAG_NO_DEFER | FLAG_IGNORE_SELF | FLAG_FILE_EVENTS,
            )
        };
        if stream.is_null() {
            unsafe { dispatch_release(queue) };
            return None;
        }
        unsafe {
            FSEventStreamSetDispatchQueue(stream, queue);
            if FSEventStreamStart(stream) == 0 {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                dispatch_release(queue);
                return None;
            }
        }
        Some(Watcher {
            stream,
            queue,
            shared,
        })
    }

    pub fn root(&self) -> &Path {
        // The shared block is only freed never, see `Shared`.
        unsafe { &(*self.shared).root }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        unsafe {
            (*self.shared).closed.store(true, Ordering::SeqCst);
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream);
            dispatch_release(self.queue);
        }
    }
}

/// On the watcher's queue: decide what the batch means and hand it on.
unsafe extern "C" fn on_events(
    _stream: *const c_void,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    _ids: *const u64,
) {
    let shared = unsafe { &*(info as *const Shared) };
    if shared.closed.load(Ordering::SeqCst) {
        return;
    }
    let paths = paths as *const *const c_char;
    let (mut tree, mut git) = (false, false);
    for index in 0..count {
        if !flags.is_null() && unsafe { *flags.add(index) } & RESCAN != 0 {
            (tree, git) = (true, true);
            break;
        }
        let raw = unsafe { *paths.add(index) };
        if raw.is_null() {
            continue;
        }
        let path = unsafe { CStr::from_ptr(raw) }.to_string_lossy();
        match classify(&shared.root, &shared.git_dirs, Path::new(path.as_ref())) {
            Some(Change::Tree) => tree = true,
            Some(Change::Git) => git = true,
            None => {}
        }
        if tree && git {
            break;
        }
    }
    if !tree && !git {
        return;
    }
    let mut pending = shared.pending.lock().unwrap_or_else(|e| e.into_inner());
    let already_queued = pending.0 || pending.1;
    *pending = (pending.0 || tree, pending.1 || git);
    drop(pending);
    if !already_queued {
        // `info` is the never-freed shared block.
        unsafe { crate::platform::dispatch::on_main(info, deliver) };
    }
}

/// On the main thread: run the handler with the coalesced change.
unsafe extern "C" fn deliver(info: *mut c_void) {
    let shared = unsafe { &*(info as *const Shared) };
    if shared.closed.load(Ordering::SeqCst) {
        return;
    }
    let (tree, git) =
        std::mem::take(&mut *shared.pending.lock().unwrap_or_else(|e| e.into_inner()));
    if tree {
        (shared.on_change.0)(Change::Tree);
    }
    if git {
        (shared.on_change.0)(Change::Git);
    }
}

/// What an event at `path` means for the editor, or `None` when it is in a
/// directory the sidebar never shows anyway.
pub fn classify(root: &Path, git_dirs: &[PathBuf], path: &Path) -> Option<Change> {
    // Only what changes the panel: the branch, the index, refs. Object
    // writes and lock files churn constantly during any operation. A
    // linked worktree keeps its HEAD and index under `worktrees/<name>`.
    let bookkeeping = |inside: Vec<String>| {
        let inside = match inside.first().map(String::as_str) {
            Some("worktrees") if inside.len() > 2 => inside[2..].to_vec(),
            _ => inside,
        };
        (matches!(
            inside.first().map(String::as_str),
            Some("HEAD" | "index" | "refs" | "packed-refs" | "ORIG_HEAD" | "MERGE_HEAD")
        ) && !inside.last().is_some_and(|last| last.ends_with(".lock")))
        .then_some(Change::Git)
    };
    let names = |p: &Path| -> Vec<String> {
        p.components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect()
    };
    if let Some(inside) = git_dirs.iter().find_map(|d| path.strip_prefix(d).ok()) {
        return bookkeeping(names(inside));
    }
    let relative = path.strip_prefix(root).ok()?;
    let mut components = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy());
    let first = components.next()?;
    if first == ".git" {
        return bookkeeping(components.map(|c| c.into_owned()).collect());
    }
    let noisy = ["target", "node_modules", "vendor", ".DS_Store"];
    if noisy.contains(&first.as_ref()) {
        return None;
    }
    let name = relative
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    // The editor's own atomic-save temporaries and recovery dumps.
    if name.starts_with(".crc-") && name.ends_with(".tmp") {
        return None;
    }
    Some(Change::Tree)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_tree_git_and_noise() {
        let root = Path::new("/p");
        assert_eq!(
            classify(root, &[], Path::new("/p/src/main.rs")),
            Some(Change::Tree)
        );
        assert_eq!(
            classify(root, &[], Path::new("/p/.env")),
            Some(Change::Tree)
        );
        assert_eq!(
            classify(root, &[], Path::new("/p/.git/HEAD")),
            Some(Change::Git)
        );
        assert_eq!(
            classify(root, &[], Path::new("/p/.git/index")),
            Some(Change::Git)
        );
        assert_eq!(
            classify(root, &[], Path::new("/p/.git/refs/heads/main")),
            Some(Change::Git)
        );
        assert_eq!(classify(root, &[], Path::new("/p/.git/index.lock")), None);
        assert_eq!(
            classify(root, &[], Path::new("/p/.git/objects/ab/cd")),
            None
        );
        assert_eq!(
            classify(root, &[], Path::new("/p/node_modules/x/index.js")),
            None
        );
        assert_eq!(classify(root, &[], Path::new("/p/target/debug/app")), None);
        assert_eq!(
            classify(root, &[], Path::new("/p/src/.crc-1-2-3.tmp")),
            None
        );
        assert_eq!(
            classify(root, &[], Path::new("/p")),
            None,
            "the root itself is nothing"
        );
    }

    #[test]
    fn git_directories_outside_the_project_are_bookkeeping_too() {
        // A subfolder of a repository, and a linked worktree.
        let root = Path::new("/repo/app");
        let dirs = [PathBuf::from("/repo/.git")];
        assert_eq!(
            classify(root, &dirs, Path::new("/repo/.git/HEAD")),
            Some(Change::Git)
        );
        assert_eq!(
            classify(root, &dirs, Path::new("/repo/.git/objects/ab")),
            None
        );
        assert_eq!(
            classify(root, &dirs, Path::new("/repo/app/x.rs")),
            Some(Change::Tree)
        );
        assert_eq!(classify(root, &dirs, Path::new("/repo/other/y.rs")), None);
        let dirs = [
            PathBuf::from("/main/.git/worktrees/wt"),
            PathBuf::from("/main/.git"),
        ];
        let root = Path::new("/wt");
        assert_eq!(
            classify(root, &dirs, Path::new("/main/.git/worktrees/wt/index")),
            Some(Change::Git)
        );
        assert_eq!(
            classify(root, &dirs, Path::new("/main/.git/refs/heads/topic")),
            Some(Change::Git)
        );
    }

    #[test]
    fn a_watcher_starts_and_stops_without_a_run_loop() {
        let dir = std::env::temp_dir().join(format!("caio-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let watcher = Watcher::new(&dir, Box::new(|_| {})).expect("FSEvents stream");
        assert_eq!(watcher.root(), std::fs::canonicalize(&dir).unwrap());
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(watcher);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
