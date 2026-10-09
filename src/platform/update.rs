//! Tells you when there is a newer crc, and installs it when you ask.
//!
//! At most once a day on launch, and whenever Help > Check for Updates is
//! chosen, `/usr/bin/curl` reads the public list of releases from GitHub on
//! a worker thread. The list, not `/releases/latest`: that one skips
//! prereleases, and every alpha is one. The launch check only says a newer
//! version exists. `update_check = false` in the settings stops it.
//!
//! Asking (Check for Updates) downloads the release's DMG and its
//! `SHA256SUMS`, checks the hash, copies `crc.app` out of the DMG, and
//! accepts it only when it is signed by crc's developer with crc's bundle
//! id, notarized, and newer than this build. Restart to Update then quits
//! the way Cmd-Q does, puts the new app where this one is, and opens it.
//! Nothing is installed without that click, and nothing but macOS's own
//! tools does the work.

use std::cmp::Ordering;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

pub const RELEASES_API: &str = "https://api.github.com/repos/caioricciuti/crc/releases?per_page=20";

/// How long a launch check counts for.
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// A release newer than this build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub url: String,
    /// Its `crc.dmg` and `SHA256SUMS` downloads, when it has both.
    pub dmg: Option<String>,
    pub sums: Option<String>,
}

/// Where a release's files are downloaded from.
const DOWNLOADS: &str = "https://github.com/caioricciuti/crc/releases/download/";
/// The bundle id and Apple team every release is signed with.
pub const BUNDLE_ID: &str = "dev.ricciuti.crc";
pub const TEAM: &str = "ZDW7RUL9X4";

/// Whether a download address is one of this repository's release files;
/// a local file too in tests and self-test runs.
fn release_file(url: &str) -> bool {
    url.starts_with(DOWNLOADS)
        || ((cfg!(test) || std::env::var_os("CRC_SELFTEST").is_some())
            && url.starts_with("file://"))
}

/// The address of the asset called `name` in a release, when it is one of
/// this repository's release files.
fn asset(release: &crate::json::Value, name: &str) -> Option<String> {
    release
        .get("assets")?
        .as_array()?
        .iter()
        .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(name))?
        .get("browser_download_url")?
        .as_str()
        .filter(|url| release_file(url))
        .map(str::to_owned)
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
        // It is handed to `open`: a page of this repository only, never
        // another scheme or site.
        if !url.starts_with("https://github.com/caioricciuti/crc/") {
            continue;
        }
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
                dmg: asset(release, "crc.dmg"),
                sums: asset(release, "SHA256SUMS"),
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
    Some(crate::platform::app_support()?.join("update-checked"))
}

/// Whether a launch check is due, and if so records that one is starting.
/// A failed check still counts: offline all day is not a reason to try on
/// every launch.
pub fn launch_check_due() -> bool {
    let Some(path) = stamp_path() else {
        return false;
    };
    let now = crate::platform::unix_seconds();
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
    // A release listing is a small JSON file.
    let output = crate::http::curl::strict_get(4 << 20, 10)
        .arg("--header")
        .arg("Accept: application/vnd.github+json")
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

/// A downloaded, checked release, waiting for Restart to Update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ready {
    pub version: String,
    /// The new `crc.app`, outside the DMG.
    pub app: PathBuf,
}

/// The app bundle this binary runs from: `crc.app` for
/// `crc.app/Contents/MacOS/crc`, or `None` for a bare binary.
pub fn running_app() -> Option<PathBuf> {
    // A self-test names one, so the download and its checks run from a
    // bare binary.
    if std::env::var_os("CRC_SELFTEST").is_some() {
        return std::env::var_os("CRC_UPDATE_APP").map(PathBuf::from);
    }
    let exe = std::env::current_exe().ok()?;
    let app = exe.parent()?.parent()?.parent()?;
    (app.extension()? == "app").then(|| crate::platform::canonical(app))
}

/// Where downloads are unpacked; emptied before each one.
fn staging() -> Option<PathBuf> {
    Some(crate::platform::caches()?.join("update"))
}

/// Downloads and checks `release` on a worker; the answer arrives on the
/// receiver.
pub fn spawn_install(release: Release) -> mpsc::Receiver<Result<Ready, String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = match staging() {
            Some(dir) => download(&release, &dir),
            None => Err("no caches folder".to_owned()),
        };
        let _ = tx.send(result);
    });
    rx
}

/// Runs a macOS tool to the end; its first line of complaint when it fails.
fn run(command: &mut std::process::Command, what: &str) -> Result<Vec<u8>, String> {
    let output = command
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{what}: {e}"))?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    let said = String::from_utf8_lossy(&output.stderr);
    let first = said
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("failed");
    Err(format!(
        "{what}: {}",
        first.trim().trim_start_matches("curl: ")
    ))
}

fn download(release: &Release, dir: &std::path::Path) -> Result<Ready, String> {
    let (Some(dmg_url), Some(sums_url)) = (&release.dmg, &release.sums) else {
        return Err("the release has no crc.dmg and SHA256SUMS to install from".into());
    };
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let dmg = dir.join("crc.dmg");
    let sums = dir.join("SHA256SUMS");
    run(
        crate::http::curl::strict_get(512 << 20, 900)
            .arg("--output")
            .arg(&dmg)
            .arg(dmg_url),
        "download",
    )?;
    run(
        crate::http::curl::strict_get(1 << 16, 30)
            .arg("--output")
            .arg(&sums)
            .arg(sums_url),
        "download",
    )?;
    let listed = std::fs::read_to_string(&sums).map_err(|e| format!("SHA256SUMS: {e}"))?;
    let expected = expected_hash(&listed, "crc.dmg").ok_or("SHA256SUMS does not list crc.dmg")?;
    let hashed = run(
        std::process::Command::new("/usr/bin/shasum")
            .args(["-a", "256"])
            .arg(&dmg),
        "shasum",
    )?;
    let actual = String::from_utf8_lossy(&hashed);
    if actual.split_whitespace().next() != Some(expected.as_str()) {
        return Err("the download does not match its SHA256SUMS".into());
    }
    let mount = dir.join("mount");
    std::fs::create_dir_all(&mount).map_err(|e| format!("{}: {e}", mount.display()))?;
    run(
        std::process::Command::new("/usr/bin/hdiutil")
            .args([
                "attach",
                "-nobrowse",
                "-readonly",
                "-noautoopen",
                "-mountpoint",
            ])
            .arg(&mount)
            .arg(&dmg),
        "hdiutil",
    )?;
    let app = dir.join("crc.app");
    let copied = run(
        std::process::Command::new("/usr/bin/ditto")
            .arg(mount.join("crc.app"))
            .arg(&app),
        "copy",
    );
    let _ = run(
        std::process::Command::new("/usr/bin/hdiutil")
            .args(["detach", "-quiet"])
            .arg(&mount),
        "hdiutil",
    );
    copied?;
    let version = verify(&app, env!("CARGO_PKG_VERSION"))?;
    Ok(Ready { version, app })
}

/// The hash `SHA256SUMS` gives for `name`, in lower case.
fn expected_hash(listed: &str, name: &str) -> Option<String> {
    listed.lines().find_map(|line| {
        let (hash, file) = line.split_once(char::is_whitespace)?;
        let file = file.trim_start().trim_start_matches('*');
        (file == name && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

/// Whether `app` may replace this build: signed by crc's developer with
/// crc's bundle id, accepted by Gatekeeper as notarized, and newer than
/// `current`. Its version when it is.
pub fn verify(app: &std::path::Path, current: &str) -> Result<String, String> {
    let requirement = format!(
        "=anchor apple generic and identifier \"{BUNDLE_ID}\" and certificate leaf[subject.OU] = \"{TEAM}\""
    );
    run(
        std::process::Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(format!("-R{requirement}"))
            .arg(app),
        "not signed by crc's developer",
    )?;
    run(
        std::process::Command::new("/usr/sbin/spctl")
            .args(["--assess", "--type", "execute"])
            .arg(app),
        "not accepted by Gatekeeper",
    )?;
    let raw = run(
        std::process::Command::new("/usr/bin/plutil")
            .args(["-extract", "CFBundleShortVersionString", "raw", "-o", "-"])
            .arg(app.join("Contents/Info.plist")),
        "no version",
    )?;
    let version = String::from_utf8_lossy(&raw).trim().to_owned();
    if compare(&version, current) != Ordering::Greater {
        return Err(format!("{version} is not newer than {current}"));
    }
    Ok(version)
}

/// Puts `ready` where `app` is. The old app moves aside first and comes
/// back if the new one cannot take its place. A running crc keeps running
/// from the files it already has open.
pub fn apply(ready: &Ready, app: &std::path::Path) -> Result<(), String> {
    let old = ready.app.with_file_name("previous.app");
    let _ = std::fs::remove_dir_all(&old);
    std::fs::rename(app, &old).map_err(|e| format!("{}: {e}", app.display()))?;
    if let Err(e) = std::fs::rename(&ready.app, app) {
        let _ = std::fs::rename(&old, app);
        return Err(format!("{}: {e}", app.display()));
    }
    Ok(())
}

/// Opens `app` once this process has exited.
pub fn relaunch_after_exit(app: &std::path::Path) -> std::io::Result<()> {
    // A test instance must not open a real crc on someone's screen.
    if std::env::var_os("CRC_SELFTEST").is_some() {
        return Ok(());
    }
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("while /bin/kill -0 \"$1\" 2>/dev/null; do /bin/sleep 0.2; done; /usr/bin/open \"$2\"")
        .arg("sh")
        .arg(std::process::id().to_string())
        .arg(app)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_carries_its_own_downloads_only() {
        let json = r#"[{"tag_name": "v9.0.0", "html_url": "https://github.com/caioricciuti/crc/releases/tag/v9.0.0",
            "assets": [
              {"name": "crc.dmg", "browser_download_url": "https://github.com/caioricciuti/crc/releases/download/v9.0.0/crc.dmg"},
              {"name": "SHA256SUMS", "browser_download_url": "https://example.com/SHA256SUMS"}
            ]}]"#;
        let release = newer_release(json, "0.2.0").unwrap().unwrap();
        assert_eq!(
            release.dmg.as_deref(),
            Some("https://github.com/caioricciuti/crc/releases/download/v9.0.0/crc.dmg")
        );
        assert_eq!(release.sums, None, "a file elsewhere is not this release's");
    }

    #[test]
    fn sums_are_read_for_the_named_file() {
        let hash = "a".repeat(64);
        let listed = format!("{hash}  crc.dmg\n{}  other\n", "b".repeat(64));
        assert_eq!(expected_hash(&listed, "crc.dmg"), Some(hash));
        assert_eq!(expected_hash("short  crc.dmg\n", "crc.dmg"), None);
        assert_eq!(expected_hash(&listed, "missing"), None);
    }

    #[test]
    fn an_unsigned_app_is_refused() {
        let dir = std::env::temp_dir().join(format!("crc-verify-{}", std::process::id()));
        let app = dir.join("crc.app");
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::write(app.join("Contents/MacOS/crc"), "#!/bin/sh\n").unwrap();
        let refused = verify(&app, "0.0.1").unwrap_err();
        assert!(
            refused.starts_with("not signed by crc's developer"),
            "{refused}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn apply_puts_the_old_app_back_when_it_cannot_finish() {
        let dir = std::env::temp_dir().join(format!("crc-apply-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let installed = dir.join("Applications/crc.app");
        let staged = dir.join("update/crc.app");
        std::fs::create_dir_all(&installed).unwrap();
        std::fs::write(installed.join("v"), "old").unwrap();
        // Nothing staged: the second move fails, the first is undone.
        std::fs::create_dir_all(dir.join("update")).unwrap();
        let ready = Ready {
            version: "9.0.0".into(),
            app: staged.clone(),
        };
        assert!(apply(&ready, &installed).is_err());
        assert_eq!(std::fs::read_to_string(installed.join("v")).unwrap(), "old");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join("v"), "new").unwrap();
        apply(&ready, &installed).unwrap();
        assert_eq!(std::fs::read_to_string(installed.join("v")).unwrap(), "new");
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
            {"tag_name": "v0.3.0", "html_url": "https://github.com/caioricciuti/crc/releases/0.3.0", "draft": true},
            {"tag_name": "v0.2.0-alpha.3", "html_url": "https://github.com/caioricciuti/crc/releases/a3", "draft": false},
            {"tag_name": "v0.2.0-alpha.2", "html_url": "https://github.com/caioricciuti/crc/releases/a2", "draft": false},
            {"tag_name": "v0.2.0-alpha.1", "html_url": "https://github.com/caioricciuti/crc/releases/a1", "draft": false},
            {"tag_name": "not-a-version", "html_url": "https://github.com/caioricciuti/crc/releases/n"}
        ]"#;
        assert_eq!(
            newer_release(json, "0.2.0-alpha.1"),
            Ok(Some(Release {
                version: "0.2.0-alpha.3".into(),
                url: "https://github.com/caioricciuti/crc/releases/a3".into(),
                dmg: None,
                sums: None,
            }))
        );
        assert_eq!(newer_release(json, "0.2.0-alpha.3"), Ok(None));
        // A page anywhere else is not a release of this editor.
        let elsewhere = r#"[{"tag_name": "v9.0.0", "html_url": "file:///etc/passwd"}]"#;
        assert_eq!(newer_release(elsewhere, "0.2.0"), Ok(None));
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
            r#"[{"tag_name": "v999.0.0", "html_url": "https://github.com/caioricciuti/crc/releases/999"}]"#,
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
