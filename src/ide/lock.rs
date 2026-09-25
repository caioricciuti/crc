//! The lock file that lets `claude` find the editor.
//!
//! `~/.claude/ide/<port>.lock` (under `$CLAUDE_CONFIG_DIR` when that is set),
//! JSON naming the process, the workspace folders and the token. `claude`
//! offers the editor when its working directory is inside one of the
//! folders, so they are written canonical: a symlinked or differently cased
//! path does not match. The file holds the token, so it is created readable
//! by the user alone, written whole to a temporary name and renamed into
//! place so a reader never sees half of it.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::json::{self, Value};

/// What a lock file names this editor as. Also how stale locks of our own
/// are told from another editor's.
pub const IDE_NAME: &str = "crc";

/// Where lock files live, or `None` without a home directory.
pub fn dir() -> Option<PathBuf> {
    if let Some(config) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(config).join("ide"));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".claude/ide"))
}

/// A written lock file, removed on drop.
pub struct Lock {
    path: PathBuf,
    port: u16,
    token: String,
}

impl Lock {
    pub fn write(dir: &Path, port: u16, token: &str, folders: &[PathBuf]) -> std::io::Result<Lock> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let lock = Lock {
            path: dir.join(format!("{port}.lock")),
            port,
            token: token.to_owned(),
        };
        lock.update(folders)?;
        Ok(lock)
    }

    /// Rewrites the folder list, as when another project is opened.
    pub fn update(&self, folders: &[PathBuf]) -> std::io::Result<()> {
        let body = json::compact(&contents(std::process::id(), folders, &self.token));
        let temp = self
            .path
            .with_extension(format!("lock.{}.tmp", std::process::id()));
        let _ = std::fs::remove_file(&temp);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        let written = file
            .write_all(body.as_bytes())
            .and_then(|()| file.sync_all());
        drop(file);
        if let Err(e) = written.and_then(|()| std::fs::rename(&temp, &self.path)) {
            let _ = std::fs::remove_file(&temp);
            return Err(e);
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn contents(pid: u32, folders: &[PathBuf], token: &str) -> Value {
    json::object([
        ("pid", json::number(pid)),
        (
            "workspaceFolders",
            Value::Array(
                folders
                    .iter()
                    .map(|f| json::string(&f.to_string_lossy()))
                    .collect(),
            ),
        ),
        ("ideName", json::string(IDE_NAME)),
        ("transport", json::string("ws")),
        ("runningInWindows", Value::Bool(false)),
        ("authToken", json::string(token)),
    ])
}

/// Removes lock files this editor left behind when it did not quit
/// cleanly: ours by name, and naming a process that no longer exists.
/// Other editors' files are never touched.
pub fn remove_stale(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "lock") {
            continue;
        }
        let Some(value) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| json::parse(&text).ok())
        else {
            continue;
        };
        let ours = value.get("ideName").and_then(Value::as_str) == Some(IDE_NAME);
        let pid = value.get("pid").and_then(Value::as_u64);
        if ours && pid.is_some_and(|pid| !alive(pid)) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Whether a process exists. Signal 0 checks without sending anything;
/// `EPERM` means it exists but belongs to someone else.
fn alive(pid: u64) -> bool {
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

/// Reads a lock file back. For tests and the self-test.
pub fn read(path: &Path) -> std::io::Result<Value> {
    let text = std::io::read_to_string(File::open(path)?)?;
    json::parse(&text).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("crc-lock-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn folders(value: &Value) -> Vec<&str> {
        value
            .get("workspaceFolders")
            .and_then(Value::as_array)
            .unwrap_or_default()
            .iter()
            .filter_map(Value::as_str)
            .collect()
    }

    #[test]
    fn written_private_readable_and_removed_on_drop() {
        let dir = temp("write").join("ide");
        let lock = Lock::write(&dir, 45678, "abc123", &[PathBuf::from("/p/one")]).unwrap();
        assert_eq!(lock.path(), dir.join("45678.lock"));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(lock.path()), 0o600);

        let value = read(lock.path()).unwrap();
        assert_eq!(value.get("ideName").and_then(Value::as_str), Some("crc"));
        assert_eq!(value.get("transport").and_then(Value::as_str), Some("ws"));
        assert_eq!(
            value.get("authToken").and_then(Value::as_str),
            Some("abc123")
        );
        assert_eq!(
            value.get("pid").and_then(Value::as_u64),
            Some(std::process::id() as u64)
        );
        assert_eq!(folders(&value), ["/p/one"]);

        lock.update(&[PathBuf::from("/p/two"), PathBuf::from("/p/three")])
            .unwrap();
        let value = read(lock.path()).unwrap();
        assert_eq!(folders(&value), ["/p/two", "/p/three"]);
        assert_eq!(mode(lock.path()), 0o600);
        // No temporary file left beside it.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        let path = lock.path().to_path_buf();
        drop(lock);
        assert!(!path.exists());
    }

    #[test]
    fn only_our_own_dead_locks_are_removed() {
        let dir = temp("stale");
        std::fs::create_dir_all(&dir).unwrap();
        // No process has pid 2^31 - 1.
        let dead = i32::MAX as u32;
        let write = |name: &str, pid: u32, ide: &str| {
            let mut value = contents(pid, &[], "t");
            if let Value::Object(members) = &mut value {
                members.iter_mut().find(|(k, _)| k == "ideName").unwrap().1 = json::string(ide);
            }
            std::fs::write(dir.join(name), json::compact(&value)).unwrap();
        };
        write("1.lock", dead, "crc");
        write("2.lock", std::process::id(), "crc");
        write("3.lock", dead, "Visual Studio Code");
        std::fs::write(dir.join("4.lock"), "not json").unwrap();
        remove_stale(&dir);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["2.lock", "3.lock", "4.lock"]);
    }
}
