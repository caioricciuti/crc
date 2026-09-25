//! Crash logs, and Help > Report a Problem.
//!
//! A panic writes a plain text log under `~/Library/Logs/crc/`: version,
//! commit, macOS version, the panic message and where it happened, and a
//! backtrace. Nothing leaves the machine on its own. Report a Problem opens
//! a new GitHub issue in the browser with those facts filled in, and the
//! person reads it and presses the button, or does not.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Where issues are filed.
pub const ISSUES: &str = "https://github.com/caioricciuti/crc/issues/new";

/// A crash older than this is not offered in a new report: it is probably
/// not what the report is about.
const RECENT: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// `~/Library/Logs/crc`, where Console.app looks for an app's logs.
pub fn logs_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Logs/crc"))
}

/// `15.6.1`, from the file the system itself reads it from.
pub fn macos_version() -> String {
    std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist")
        .ok()
        .and_then(|plist| plist_string(&plist, "ProductVersion"))
        .unwrap_or_else(|| "unknown".into())
}

/// The `<string>` after `<key>{key}</key>` in an XML plist.
fn plist_string(plist: &str, key: &str) -> Option<String> {
    let after = &plist[plist.find(&format!("<key>{key}</key>"))?..];
    let start = after.find("<string>")? + "<string>".len();
    let end = after[start..].find("</string>")? + start;
    Some(after[start..end].trim().to_owned())
}

/// Writes one crash log and returns its path. Called from the panic hook,
/// so it takes everything it needs as arguments and allocates little.
pub fn write_crash_log(
    dir: &Path,
    message: &str,
    location: &str,
    backtrace: &str,
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let path = dir.join(format!("crash-{stamp}-{}.log", std::process::id()));
    let mut file = std::fs::File::create(&path)?;
    write!(
        file,
        "crc {}\nmacOS {}\n\npanic: {message}\nat: {location}\n\n{backtrace}\n",
        crate::build_label(),
        macos_version(),
    )?;
    file.sync_all()?;
    Ok(path)
}

/// The newest crash log, if it is recent: its path and its panic line.
pub fn latest_crash(dir: &Path) -> Option<(PathBuf, String)> {
    let newest = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("crash-") && name.ends_with(".log")
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .filter(|(at, _)| at.elapsed().is_ok_and(|age| age < RECENT))
        .max()?
        .1;
    let text = std::fs::read_to_string(&newest).ok()?;
    let panic = text.lines().find(|l| l.starts_with("panic: "))?;
    let at = text.lines().find(|l| l.starts_with("at: ")).unwrap_or("");
    Some((
        newest.clone(),
        format!("{panic} {at}").trim_end().to_owned(),
    ))
}

/// The body of a new issue: three questions, then the facts.
pub fn issue_body(build: &str, macos: &str, crash: Option<&(PathBuf, String)>) -> String {
    let mut body = format!(
        "What did you do?\n\n\nWhat did you expect?\n\n\nWhat happened?\n\n\n---\ncrc {build}\nmacOS {macos}, Apple Silicon\n"
    );
    if let Some((path, line)) = crash {
        let name = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        body.push_str(&format!(
            "\nLast crash: {line}\nFull log: ~/Library/Logs/crc/{name} (drag it into this box to attach it)\n"
        ));
    }
    body
}

/// The new-issue URL with `body` filled in.
pub fn issue_url(body: &str) -> String {
    format!("{ISSUES}?body={}", percent_encode(body))
}

/// Everything but the unreserved characters, as UTF-8 bytes.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("crc-report-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_crash_log_holds_the_facts_and_is_found_again() {
        let dir = scratch("log");
        assert!(latest_crash(&dir).is_none());
        let path = write_crash_log(&dir, "index out of range", "src/a.rs:3:9", "0: frame").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with(&format!("crc {}\nmacOS ", crate::build_label())),
            "{text}"
        );
        assert!(text.contains("panic: index out of range\nat: src/a.rs:3:9\n\n0: frame"));
        let (found, line) = latest_crash(&dir).unwrap();
        assert_eq!(found, path);
        assert_eq!(line, "panic: index out of range at: src/a.rs:3:9");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_issue_body_says_the_version_and_points_at_the_log() {
        let crash = (
            PathBuf::from("/x/crash-1-2.log"),
            "panic: boom at: a.rs:1:1".to_owned(),
        );
        let body = issue_body("0.2.0 (abc)", "15.6", Some(&crash));
        assert!(body.starts_with("What did you do?"));
        assert!(body.contains("crc 0.2.0 (abc)\nmacOS 15.6, Apple Silicon\n"));
        assert!(body.contains("Last crash: panic: boom at: a.rs:1:1\n"));
        assert!(body.contains("~/Library/Logs/crc/crash-1-2.log"));
        assert!(!issue_body("v", "m", None).contains("Last crash"));
    }

    #[test]
    fn the_url_is_encoded_whole() {
        let url = issue_url("a b\né&?#");
        assert_eq!(url, format!("{ISSUES}?body=a%20b%0A%C3%A9%26%3F%23"));
    }

    #[test]
    fn reads_the_version_out_of_a_plist() {
        let plist = "<dict>\n\t<key>ProductName</key>\n\t<string>macOS</string>\n\t<key>ProductVersion</key>\n\t<string>15.6.1</string>\n</dict>";
        assert_eq!(
            plist_string(plist, "ProductVersion").as_deref(),
            Some("15.6.1")
        );
        assert_eq!(plist_string(plist, "Missing"), None);
        assert_ne!(macos_version(), "unknown", "this Mac has the file");
    }
}
