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
pub mod recovery;
pub mod report;
pub mod search;
pub mod selftest;
pub mod session;
pub mod settings;
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

fn home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// Whether a process exists. Signal 0 checks without sending anything;
/// `EPERM` means it exists but belongs to someone else.
pub fn process_alive(pid: u64) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    if unsafe { kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(1)
}

/// Writes `bytes` to a temporary file beside `path` and renames it over, so
/// a crash or a full disk leaves the old file rather than half of the new.
pub fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}
