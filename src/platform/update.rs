//! Tells you when there is a newer crc. Never downloads or installs it.
//!
//! At most once a day on launch, and whenever Help > Check for Updates is
//! chosen, `/usr/bin/curl` reads the public list of releases from GitHub on
//! a worker thread. The list, not `/releases/latest`: that one skips
//! prereleases, and every alpha is one. A newer version is a status line
//! message, or, when asked for, its release page in the browser.
//! `update_check = false` in the settings stops the launch check.

use std::cmp::Ordering;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

pub const RELEASES_API: &str = "https://api.github.com/repos/caioricciuti/crc/releases?per_page=20";

/// How long a launch check counts for.
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// A release newer than this build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub url: String,
}

/// What a check found: a newer release, none, or why it could not tell.
pub type Outcome = Result<Option<Release>, String>;

/// The newest release in a GitHub releases listing that is newer than
/// `current`. Drafts are skipped (an anonymous reader never sees them
/// anyway).
pub fn newer_release(json: &str, current: &str) -> Outcome {
    let value = crate::json::parse(json).map_err(|e| format!("unreadable reply: {e}"))?;
    let releases = value.as_array().ok_or("unexpected reply: not a list")?;
    let mut best: Option<Release> = None;
    for release in releases {
        if release.get("draft").and_then(|d| d.as_bool()) == Some(true) {
            continue;
        }
        let (Some(tag), Some(url)) = (
            release.get("tag_name").and_then(|t| t.as_str()),
            release.get("html_url").and_then(|u| u.as_str()),
        ) else {
            continue;
        };
        let version = tag.trim_start_matches('v');
        if compare(version, current) != Ordering::Greater {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|b| compare(version, &b.version) == Ordering::Greater)
        {
            best = Some(Release {
                version: version.to_owned(),
                url: url.to_owned(),
            });
        }
    }
    Ok(best)
}

/// Semantic version order, prereleases included: `0.2.0-alpha.2` is after
/// `0.2.0-alpha.1` and before `0.2.0`. Anything unparsable sorts first, so
/// a strange tag is never announced as an update.
pub fn compare(a: &str, b: &str) -> Ordering {
    fn split(v: &str) -> Option<([u64; 3], Option<&str>)> {
        let v = v.split('+').next()?;
        let (core, pre) = match v.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (v, None),
        };
        let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
        let numbers = [parts.next()??, parts.next()??, parts.next()??];
        parts.next().is_none().then_some((numbers, pre))
    }
    fn identifiers(a: &str, b: &str) -> Ordering {
        let (mut a, mut b) = (a.split('.'), b.split('.'));
        loop {
            match (a.next(), b.next()) {
                (None, None) => return Ordering::Equal,
                (None, Some(_)) => return Ordering::Less,
                (Some(_), None) => return Ordering::Greater,
                (Some(x), Some(y)) => {
                    let order = match (x.parse::<u64>(), y.parse::<u64>()) {
                        (Ok(x), Ok(y)) => x.cmp(&y),
                        (Ok(_), Err(_)) => Ordering::Less,
                        (Err(_), Ok(_)) => Ordering::Greater,
                        (Err(_), Err(_)) => x.cmp(y),
                    };
                    if order != Ordering::Equal {
                        return order;
                    }
                }
            }
        }
    }
    match (split(a), split(b)) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some((x, xp)), Some((y, yp))) => x.cmp(&y).then_with(|| match (xp, yp) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(xp), Some(yp)) => identifiers(xp, yp),
        }),
    }
}

/// Where the time of the last launch check is kept.
fn stamp_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/crc/update-checked"))
}

/// Whether a launch check is due, and if so records that one is starting.
/// A failed check still counts: offline all day is not a reason to try on
/// every launch.
pub fn launch_check_due() -> bool {
    let Some(path) = stamp_path() else {
        return false;
    };
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let last = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok());
    if last.is_some_and(|last| now.saturating_sub(last) < DAY.as_secs()) {
        return false;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, now.to_string());
    true
}

/// Starts a check on a worker; the answer arrives on the receiver.
/// `CRC_UPDATE_URL` replaces the API address, for the GUI self-test.
pub fn spawn() -> mpsc::Receiver<Outcome> {
    let (tx, rx) = mpsc::channel();
    let url = std::env::var("CRC_UPDATE_URL").unwrap_or_else(|_| RELEASES_API.to_owned());
    std::thread::spawn(move || {
        let _ = tx.send(fetch(&url));
    });
    rx
}

fn fetch(url: &str) -> Outcome {
    let output = std::process::Command::new("/usr/bin/curl")
        .args([
            "--silent",
            "--show-error",
            "--fail",
            "--location",
            "--max-time",
            "10",
        ])
        .args(["--proto", "=https,file"])
        .arg("--header")
        .arg("Accept: application/vnd.github+json")
        .arg("--user-agent")
        .arg(format!("crc/{}", env!("CARGO_PKG_VERSION")))
        .arg(url)
        .output()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        // What an anonymous reader gets while the repository is private.
        if error.contains("error: 404") {
            return Err("no public releases to check against yet".to_owned());
        }
        return Err(error.trim().trim_start_matches("curl: ").to_owned());
    }
    newer_release(
        &String::from_utf8_lossy(&output.stdout),
        env!("CARGO_PKG_VERSION"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_order_the_way_semver_says() {
        let order = [
            "0.1.9",
            "0.2.0-alpha.1",
            "0.2.0-alpha.2",
            "0.2.0-alpha.10",
            "0.2.0-beta",
            "0.2.0-rc.1",
            "0.2.0",
            "0.10.0",
            "1.0.0",
        ];
        for pair in order.windows(2) {
            assert_eq!(
                compare(pair[0], pair[1]),
                Ordering::Less,
                "{} < {}",
                pair[0],
                pair[1]
            );
            assert_eq!(compare(pair[1], pair[0]), Ordering::Greater);
        }
        assert_eq!(compare("0.2.0+build.5", "0.2.0"), Ordering::Equal);
        assert_eq!(compare("nonsense", "0.0.1"), Ordering::Less);
        assert_eq!(
            compare("1.2", "0.0.1"),
            Ordering::Less,
            "two parts is not a version"
        );
    }

    #[test]
    fn picks_the_newest_published_release_past_this_one() {
        let json = r#"[
            {"tag_name": "v0.3.0", "html_url": "https://x/0.3.0", "draft": true},
            {"tag_name": "v0.2.0-alpha.3", "html_url": "https://x/a3", "draft": false},
            {"tag_name": "v0.2.0-alpha.2", "html_url": "https://x/a2", "draft": false},
            {"tag_name": "v0.2.0-alpha.1", "html_url": "https://x/a1", "draft": false},
            {"tag_name": "not-a-version", "html_url": "https://x/n"}
        ]"#;
        assert_eq!(
            newer_release(json, "0.2.0-alpha.1"),
            Ok(Some(Release {
                version: "0.2.0-alpha.3".into(),
                url: "https://x/a3".into()
            }))
        );
        assert_eq!(newer_release(json, "0.2.0-alpha.3"), Ok(None));
        assert_eq!(newer_release("[]", "0.2.0"), Ok(None));
        assert!(newer_release("{\"message\": \"Not Found\"}", "0.2.0").is_err());
    }

    #[test]
    fn reads_a_listing_through_curl() {
        let dir = std::env::temp_dir().join(format!("crc-update-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("releases.json");
        std::fs::write(
            &file,
            r#"[{"tag_name": "v999.0.0", "html_url": "https://x/999"}]"#,
        )
        .unwrap();
        let found = fetch(&format!("file://{}", file.display()));
        assert_eq!(
            found.unwrap().map(|r| r.version).as_deref(),
            Some("999.0.0")
        );
        assert!(fetch(&format!("file://{}/missing.json", dir.display())).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
