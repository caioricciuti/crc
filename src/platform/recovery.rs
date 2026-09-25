//! Keeps unsaved work alive through a crash.
//!
//! Release builds abort on panic, and every AppKit entry point is an
//! `extern "C"` frame that could not unwind anyway. A panic anywhere is the
//! end of the process, and used to be the end of every unsaved buffer.
//!
//! The panic hook still runs before the abort. What it cannot do is read the
//! editor's state: that sits in a `RefCell` which is, more often than not,
//! mutably borrowed by the very code that panicked. So the documents are
//! published to a side table as they change, and the hook reads that instead.
//! Publishing is cheap enough to do on every frame because a rope clone is
//! one `Arc` increment, and nothing is copied until a crash actually happens.
//!
//! On disk, one directory per crash under
//! `~/Library/Application Support/crc/recovery/`, holding for each dirty
//! document `<n>.txt` with its text and `<n>.path` with where it came from
//! (empty for an untitled one). Plain files on purpose: if the editor cannot
//! start at all, the work is still there to be read with anything.
//!
//! This covers panics. It does not cover a fault inside the C we link, an
//! Objective-C exception, or the power going: those need periodic snapshots,
//! which is the local-history step in the roadmap.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::text::buffer::Buffer;
use crate::text::rope::Rope;

/// One dirty document, as of the last frame.
struct Entry {
    id: u64,
    path: Option<PathBuf>,
    rope: Rope,
}

static TABLE: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

/// A document found on disk after a crash.
#[derive(Debug, PartialEq, Eq)]
pub struct Recovered {
    pub path: Option<PathBuf>,
    pub text: String,
}

/// Where crash directories are written.
pub fn default_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/crc/recovery"))
}

/// Records the dirty documents among `buffers`, and forgets the rest.
///
/// Called once per frame, so it allocates only when something changed: a
/// document already in the table whose text is the same rope is left alone.
pub fn publish<'a>(buffers: impl Iterator<Item = &'a Buffer>) {
    let Ok(mut table) = TABLE.lock() else {
        return;
    };
    let mut seen = 0;
    for buffer in buffers.filter(|b| b.is_dirty()) {
        let at = table.iter().position(|e| e.id == buffer.id());
        match at {
            Some(at) => {
                let entry = &mut table[at];
                if !entry.rope.same_as(&buffer.rope) {
                    entry.rope = buffer.rope.clone();
                }
                if entry.path != buffer.path {
                    entry.path = buffer.path.clone();
                }
                table.swap(seen, at);
            }
            None => {
                table.push(Entry {
                    id: buffer.id(),
                    path: buffer.path.clone(),
                    rope: buffer.rope.clone(),
                });
                let last = table.len() - 1;
                table.swap(seen, last);
            }
        }
        seen += 1;
    }
    // Everything past `seen` was saved or closed since the last frame.
    table.truncate(seen);
}

/// Installs the panic hook. The previous hook, which prints the message and
/// location, still runs first.
pub fn install(dir: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        match dump(&dir) {
            Ok(Some(written)) => {
                eprintln!("crc: unsaved work written to {}", written.display())
            }
            Ok(None) => {}
            Err(e) => eprintln!("crc: could not write unsaved work: {e}"),
        }
        // Then the log, for Help > Report a Problem. Unsaved work first:
        // it is the thing that cannot be reproduced.
        if let Some(logs) = crate::platform::report::logs_dir() {
            let payload = info.payload();
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "(no message)".into());
            let location = info.location().map_or_else(String::new, |l| l.to_string());
            let backtrace = std::backtrace::Backtrace::force_capture().to_string();
            match crate::platform::report::write_crash_log(&logs, &message, &location, &backtrace) {
                Ok(path) => eprintln!("crc: crash log written to {}", path.display()),
                Err(e) => eprintln!("crc: could not write the crash log: {e}"),
            }
        }
        // A self-test crashes on purpose. Aborting would hand macOS a crash
        // to report, and put a Problem Report dialog on the user's screen
        // for every run; the work above is the part under test, so leave
        // with abort's exit status instead of its signal.
        if std::env::var_os("CRC_SELFTEST").is_some() {
            std::process::exit(134);
        }
    }));
}

/// Writes every published document under a fresh directory in `dir`.
/// Returns that directory, or `None` when there was nothing unsaved.
fn dump(dir: &Path) -> std::io::Result<Option<PathBuf>> {
    // A poisoned lock means the panic happened inside `publish`. The table
    // is a list of whole entries either way, so it is still worth writing.
    // A lock held by this very thread cannot be waited for, hence `try`.
    let table = match TABLE.try_lock() {
        Ok(table) => table,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
    };
    if table.is_empty() {
        return Ok(None);
    }

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let crash = dir.join(format!("{stamp}-{}", std::process::id()));
    std::fs::create_dir_all(&crash)?;
    // Written before the entries so a failed final write remains detectable.
    std::fs::write(crash.join("count"), table.len().to_string())?;

    for (n, entry) in table.iter().enumerate() {
        // One document failing to write must not stop the others.
        if let Err(e) = write_entry(&crash, n, entry) {
            eprintln!("crc: document {n} not recovered: {e}");
        }
    }
    Ok(Some(crash))
}

fn write_entry(crash: &Path, n: usize, entry: &Entry) -> std::io::Result<()> {
    let mut text = std::fs::File::create(crash.join(format!("{n}.txt")))?;
    for chunk in entry.rope.chunks_in(0..entry.rope.len_bytes()) {
        text.write_all(chunk.as_bytes())?;
    }
    text.sync_all()?;

    use std::os::unix::ffi::OsStrExt;
    let path = entry
        .path
        .as_deref()
        .map_or(&b""[..], |p| p.as_os_str().as_bytes());
    std::fs::write(crash.join(format!("{n}.path")), path)
}

/// What earlier crashes left behind.
pub struct Pending {
    /// Oldest crash first.
    pub documents: Vec<Recovered>,
    /// The directories those were read from, and the only ones
    /// [`Pending::clear`] will remove.
    crashes: Vec<PathBuf>,
}

/// Reads everything left behind by earlier crashes.
pub fn pending(dir: &Path) -> Pending {
    let mut crashes: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    crashes.sort();

    let mut documents = Vec::new();
    let mut complete = Vec::new();
    for crash in &crashes {
        let Ok(entries) = std::fs::read_dir(crash) else {
            continue;
        };
        let mut numbers = Vec::new();
        let mut readable = true;
        for entry in entries {
            match entry {
                Ok(entry) => {
                    if let Some(n) = entry
                        .file_name()
                        .to_str()
                        .and_then(|s| s.strip_suffix(".txt"))
                        .and_then(|s| s.parse::<usize>().ok())
                    {
                        numbers.push(n);
                    }
                }
                Err(_) => readable = false,
            }
        }
        numbers.sort_unstable();
        numbers.dedup();
        if numbers.is_empty() {
            readable = false;
        }
        let count_path = crash.join("count");
        if count_path.exists() {
            match std::fs::read_to_string(&count_path)
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
            {
                Some(count) if count == numbers.len() => {}
                _ => readable = false,
            }
        }
        if numbers
            .iter()
            .enumerate()
            .any(|(position, number)| position != *number)
        {
            readable = false;
        }
        for n in numbers {
            let Ok(bytes) = std::fs::read(crash.join(format!("{n}.txt"))) else {
                readable = false;
                continue;
            };
            let Ok(text) = String::from_utf8(bytes) else {
                readable = false;
                continue;
            };
            use std::os::unix::ffi::OsStringExt;
            let path = std::fs::read(crash.join(format!("{n}.path")))
                .ok()
                .filter(|p| !p.is_empty())
                .map(|p| PathBuf::from(std::ffi::OsString::from_vec(p)));
            documents.push(Recovered { path, text });
        }
        if readable {
            complete.push(crash.clone());
        }
    }
    Pending {
        documents,
        crashes: complete,
    }
}

impl Pending {
    /// Deletes the crash directories this was read from, once their contents
    /// are safely in buffers again or the user has said they do not want
    /// them. Only those: a crash written by another instance in the meantime
    /// has not been offered to anyone yet.
    pub fn clear(self) {
        for crash in self.crashes {
            let _ = std::fs::remove_dir_all(crash);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table is process-wide, so tests that touch it take turns.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("caio-recovery-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn dirty(text: &str, path: Option<&str>) -> Buffer {
        let mut buffer = Buffer::new();
        buffer.insert(text);
        buffer.path = path.map(PathBuf::from);
        buffer
    }

    #[test]
    fn only_dirty_documents_are_written_and_come_back() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("roundtrip");
        let clean = Buffer::from_text("on disk already");
        let named = dirty(
            "caf\u{e9} \u{65e5}\u{672c}\n",
            Some("/tmp/some dir/notes.md"),
        );
        let untitled = dirty("scratch", None);

        publish([&clean, &named, &untitled].into_iter());
        let crash = dump(&dir).expect("dump").expect("something to write");
        assert!(crash.starts_with(&dir));

        let found = pending(&dir);
        assert_eq!(
            found.documents,
            vec![
                Recovered {
                    path: Some(PathBuf::from("/tmp/some dir/notes.md")),
                    text: "caf\u{e9} \u{65e5}\u{672c}\n".into(),
                },
                Recovered {
                    path: None,
                    text: "scratch".into()
                },
            ]
        );

        // A crash that lands after the read is not this one's to delete.
        let later = dir.join("9999999999-1");
        std::fs::create_dir_all(&later).expect("mkdir");
        std::fs::write(later.join("0.txt"), "from another instance").expect("write");
        found.clear();
        let left = pending(&dir);
        assert_eq!(left.documents.len(), 1, "only what was read is removed");
        assert_eq!(left.documents[0].text, "from another instance");
        publish(std::iter::empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn saving_or_closing_a_document_withdraws_it() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("withdraw");
        let mut a = dirty("a", None);
        let b = dirty("b", None);
        publish([&a, &b].into_iter());

        // More typing replaces the snapshot rather than adding one.
        a.insert("a");
        publish([&a, &b].into_iter());
        // `b` is closed. Nothing of it may be written after that.
        publish([&a].into_iter());
        dump(&dir).expect("dump");
        assert_eq!(
            pending(&dir).documents,
            vec![Recovered {
                path: None,
                text: "aa".into()
            }]
        );

        publish(std::iter::empty());
        pending(&dir).clear();
        assert_eq!(
            dump(&dir).expect("dump"),
            None,
            "nothing dirty, nothing written"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn incomplete_recovery_keeps_every_file_on_disk() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("gap");
        let crash = dir.join("123-1");
        std::fs::create_dir_all(&crash).expect("mkdir");
        std::fs::write(crash.join("0.txt"), "first").expect("first");
        std::fs::write(crash.join("2.txt"), "third").expect("third");
        let found = pending(&dir);
        assert_eq!(
            found
                .documents
                .iter()
                .map(|d| d.text.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "third"]
        );
        found.clear();
        assert!(
            crash.join("2.txt").exists(),
            "a missing recovery entry must not erase later ones"
        );
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn missing_final_recovery_entry_keeps_the_crash_directory() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("missing-last");
        let crash = dir.join("123-1");
        std::fs::create_dir_all(&crash).expect("mkdir");
        std::fs::write(crash.join("count"), "2").expect("count");
        std::fs::write(crash.join("0.txt"), "first").expect("first");
        let found = pending(&dir);
        assert_eq!(found.documents.len(), 1);
        found.clear();
        assert!(crash.join("0.txt").exists());
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    /// The real path: a process that panics with the hook installed. This
    /// test runs itself as a child, which takes the panicking branch, and
    /// then looks at what the child left on disk.
    #[test]
    fn a_panic_leaves_the_unsaved_text_on_disk() {
        const CHILD: &str = "CRC_RECOVERY_TEST_DIR";
        if let Some(dir) = std::env::var_os(CHILD) {
            install(PathBuf::from(dir));
            let buffer = dirty("typed just before the crash", Some("/tmp/victim.rs"));
            publish(std::iter::once(&buffer));
            // What actually happens in the app: the state is borrowed when
            // the panic starts, so the hook cannot rely on reading it.
            let state = std::cell::RefCell::new(buffer);
            let _held = state.borrow_mut();
            panic!("deliberate, to exercise the hook");
        }

        let dir = scratch("panic");
        let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "platform::recovery::tests::a_panic_leaves_the_unsaved_text_on_disk",
            ])
            .env(CHILD, &dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("spawn");
        assert!(!status.success(), "the child was supposed to panic");

        assert_eq!(
            pending(&dir).documents,
            vec![Recovered {
                path: Some(PathBuf::from("/tmp/victim.rs")),
                text: "typed just before the crash".into(),
            }]
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
