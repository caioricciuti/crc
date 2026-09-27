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
