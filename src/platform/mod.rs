//! Platform layer: the AppKit window, input, clipboard, files, session, and
//! timing.

pub mod claude;
pub mod clipboard;
pub mod commands;
pub mod conflicts;
pub mod dispatch;
pub mod extensions;
pub mod git_panel;
pub mod latency;
pub mod mcp_page;
pub mod mcp_panel;
pub mod recovery;
pub mod report;
pub mod review_page;
pub mod search;
pub mod selftest;
pub mod session;
pub mod settings;
pub mod settings_page;
pub mod symbols;
pub mod terminal;
pub mod update;
pub mod webview;
pub mod window;

/// crc's folder in Application Support: kept, and backed up, unlike
/// Caches, and not something a person browses, unlike Documents.
pub fn app_support() -> Option<std::path::PathBuf> {
    Some(home()?.join("Library/Application Support/crc"))
}

/// `~/Library/Logs/crc`, where Console.app looks for an app's logs.
pub fn logs() -> Option<std::path::PathBuf> {
    Some(home()?.join("Library/Logs/crc"))
}

/// crc's cache folder: anything in it can be rebuilt, and may be deleted.
pub fn caches() -> Option<std::path::PathBuf> {
    Some(home()?.join("Library/Caches/crc"))
}

/// A path as the file system resolves it (symbolic links followed, `..`
/// gone), or as given when it cannot be resolved (a file not written yet).
/// Open documents keep their paths this way, so comparing one key against
/// them needs no further lookups.
pub fn canonical(path: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A path from the raw bytes Git, the crash journal and the session store
/// keep. A `Vec` moves in without a copy.
pub fn path_from_bytes(bytes: impl Into<Vec<u8>>) -> std::path::PathBuf {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(bytes.into()).into()
}

/// Seconds since 1970, 0 on a clock set before it.
pub fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// FNV-1a: a stable 64-bit hash for names, no crate for it. build.rs keeps
/// its own copy, as a build script cannot use the crate it builds.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}

/// Sends `signal` to `pid`, or to the process group `-pid`. Whether it was
/// delivered.
pub fn send_signal(pid: i32, signal: i32) -> bool {
    // SAFETY: kill(2) takes two integers and touches no memory of ours.
    unsafe { kill(pid, signal) == 0 }
}

/// Starts `command` and reaps it from a thread, so a short-lived helper
/// (`open -R`, `open <url>`) does not stay a zombie until quit.
pub fn spawn_reaped(command: &mut std::process::Command) -> std::io::Result<()> {
    let mut child = command.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Whether a process exists. Signal 0 checks without sending anything;
/// `EPERM` means it exists but belongs to someone else.
pub fn process_alive(pid: u64) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    if send_signal(pid, 0) {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(1)
}

/// Writes `bytes` to a temporary file beside `path` and renames it over, so
/// a crash or a full disk leaves the old file rather than half of the new.
pub fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    write_atomically_with(path, 0o666, |file| file.write_all(bytes))
}

/// [`write_atomically`], with the file created as `mode` (before the umask)
/// and its contents written by `write`.
pub fn write_atomically_with(
    path: &std::path::Path,
    mode: u32,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        write(&mut file)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}
