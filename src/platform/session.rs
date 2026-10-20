//! Remembering what was open between launches.
//!
//! A plain `key=value` text file rather than JSON. Session state is a handful
//! of scalars and a list of paths; reaching for `serde` and `serde_json`
//! would add five crates, a build script and a proc macro to store it, which
//! is a bad trade for something a dozen lines of parsing covers. It is also
//! readable and hand-editable when something goes wrong, which JSON in an
//! application support directory is not.
//!
//! Nothing here fails loudly. A missing, truncated or hand-mangled session
//! file means starting fresh, because losing your window position is not
//! worth refusing to launch over.

use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq)]
pub struct Session {
    /// Window frame in screen coordinates: x, y, width, height.
    pub frame: Option<(f64, f64, f64, f64)>,
    /// The project folder, if one was open.
    pub folder: Option<PathBuf>,
    /// Open documents, in tab order. Untitled buffers are not represented,
    /// since there is nothing to reopen.
    pub files: Vec<PathBuf>,
    /// Index into `files` that was showing.
    pub active: usize,
    pub sidebar: bool,
    /// Sidebar width in logical points.
    pub sidebar_width: f32,
    /// Project folders opened before, most recent first, for the home screen.
    pub recent: Vec<PathBuf>,
}

/// How many recent projects the session keeps.
pub const RECENT_LIMIT: usize = 8;

/// Hand-written rather than derived: `#[derive(Default)]` would make
/// `sidebar` false, which is not the default anyone wants and quietly
/// disagreed with what parsing an empty file produced.
impl Default for Session {
    fn default() -> Self {
        Session {
            frame: None,
            folder: None,
            files: Vec::new(),
            active: 0,
            sidebar: true,
            sidebar_width: 240.0,
            recent: Vec::new(),
        }
    }
}

impl Session {
    /// Where the session file lives.
    ///
    /// `~/Library/Application Support` is the documented place for this on
    /// macOS: it is backed up, unlike Caches, and not user-facing, unlike
    /// Documents.
    pub fn path() -> Option<PathBuf> {
        Some(crate::platform::app_support()?.join("session"))
    }

    /// Reads the session, or a default one if anything is missing or wrong.
    pub fn load() -> Session {
        let Some(path) = Session::path() else {
            return Session::default();
        };
        // Bytes, not text: a path that is not UTF-8 is still a path, and one
        // such line used to lose the whole session.
        let Ok(bytes) = std::fs::read(&path) else {
            return Session::default();
        };
        Session::parse_bytes(&bytes)
    }

    #[cfg(test)]
    fn parse(text: &str) -> Session {
        Session::parse_bytes(text.as_bytes())
    }

    fn parse_bytes(bytes: &[u8]) -> Session {
        use std::os::unix::ffi::OsStrExt;
        let mut session = Session::default();

        for line in bytes.split(|&b| b == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let Some(eq) = line.iter().position(|&b| b == b'=') else {
                continue;
            };
            let (key, raw) = (&line[..eq], &line[eq + 1..]);
            let key = String::from_utf8_lossy(key);
            // A path is taken as written: trimming lost a file named with a
            // trailing space.
            let path = || PathBuf::from(std::ffi::OsStr::from_bytes(raw));
            let text = String::from_utf8_lossy(raw);
            let value = text.trim();
            match key.trim() {
                "frame" => {
                    // Four comma-separated floats, or the whole entry is
                    // ignored: a half-parsed frame is worse than none.
                    let parts: Vec<f64> = value
                        .split(',')
                        .filter_map(|p| p.trim().parse().ok())
                        .collect();
                    if parts.len() == 4 && parts[2] > 0.0 && parts[3] > 0.0 {
                        session.frame = Some((parts[0], parts[1], parts[2], parts[3]));
                    }
                }
                "folder" if !raw.is_empty() => session.folder = Some(path()),
                "file" if !raw.is_empty() => session.files.push(path()),
                "recent" if !raw.is_empty() && session.recent.len() < RECENT_LIMIT => {
                    session.recent.push(path())
                }
                "active" => session.active = value.parse().unwrap_or(0),
                "sidebar" => session.sidebar = value != "0",
                "sidebar_width" => {
                    if let Ok(w) = value.parse::<f32>()
                        && w.is_finite()
                        && w > 0.0
                    {
                        session.sidebar_width = w;
                    }
                }
                _ => {}
            }
        }

        session.active = session.active.min(session.files.len().saturating_sub(1));
        session
    }

    /// Writes the session, creating the directory if needed.
    ///
    /// Errors are swallowed on purpose: failing to record where a window was
    /// is not something to interrupt a quit over.
    pub fn save(&self) {
        let Some(path) = Session::path() else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = crate::platform::write_atomically(&path, &self.serialize());
    }

    /// The session file. Paths are written as their bytes (`display` would
    /// replace what is not UTF-8); one with a line break cannot be a line
    /// and is left out.
    fn serialize(&self) -> Vec<u8> {
        use std::os::unix::ffi::OsStrExt;
        let mut out = Vec::with_capacity(256);
        let path_line = |key: &str, path: &Path, out: &mut Vec<u8>| {
            let bytes = path.as_os_str().as_bytes();
            if !bytes.contains(&b'\n') && !bytes.contains(&b'\r') {
                out.extend_from_slice(key.as_bytes());
                out.push(b'=');
                out.extend_from_slice(bytes);
                out.push(b'\n');
            }
        };
        out.extend_from_slice(b"# crc session. Safe to delete.\n");
        if let Some((x, y, w, h)) = self.frame {
            out.extend_from_slice(format!("frame={x:.0},{y:.0},{w:.0},{h:.0}\n").as_bytes());
        }
        if let Some(folder) = &self.folder {
            path_line("folder", folder, &mut out);
        }
        for file in &self.files {
            path_line("file", file, &mut out);
        }
        for folder in self.recent.iter().take(RECENT_LIMIT) {
            path_line("recent", folder, &mut out);
        }
        out.extend_from_slice(format!("active={}\n", self.active).as_bytes());
        out.extend_from_slice(format!("sidebar={}\n", if self.sidebar { 1 } else { 0 }).as_bytes());
        out.extend_from_slice(format!("sidebar_width={:.0}\n", self.sidebar_width).as_bytes());
        out
    }

    /// A checkout has its own branch sessions, including worktrees. The
    /// contents carry the key too, so a hash collision cannot load another
    /// checkout's tabs. No state is written into the repository.
    fn branch_path(dir: &Path, root: &Path, branch: &str) -> PathBuf {
        use std::os::unix::ffi::OsStrExt;
        let mut key = root.as_os_str().as_bytes().to_vec();
        key.push(0);
        key.extend_from_slice(branch.as_bytes());
        dir.join(format!("{:016x}", crate::platform::fnv1a(&key)))
    }

    pub fn load_branch(root: &Path, branch: &str) -> Option<Session> {
        Self::read_branch(
            &crate::platform::app_support()?.join("branch-sessions"),
            root,
            branch,
        )
    }

    fn read_branch(dir: &Path, root: &Path, branch: &str) -> Option<Session> {
        let path = Self::branch_path(dir, root, branch);
        if std::fs::metadata(&path).ok()?.len() > 1024 * 1024 {
            return None;
        }
        let bytes = std::fs::read(path).ok()?;
        let (header, body) = bytes.split_at(bytes.iter().position(|&b| b == b'\n')? + 1);
        if header != format!("branch={branch}\n").as_bytes() {
            return None;
        }
        let mut session = Self::parse_bytes(body);
        if session.folder.as_deref() != Some(root) {
            return None;
        }
        session.files.retain(|p| p.starts_with(root));
        session.prune();
        Some(session)
    }

    /// Saves only this checkout's named documents. Untitled and dirty
    /// buffers are kept alive by the window, never serialized as text here.
    pub fn save_branch(&self, root: &Path, branch: &str) {
        if let Some(dir) = crate::platform::app_support() {
            let _ = self.write_branch(&dir.join("branch-sessions"), root, branch);
        }
    }

    fn write_branch(&self, dir: &Path, root: &Path, branch: &str) -> std::io::Result<()> {
        use std::os::unix::ffi::OsStrExt;
        if branch.contains(['\n', '\r'])
            || root.as_os_str().as_bytes().contains(&b'\n')
            || root.as_os_str().as_bytes().contains(&b'\r')
        {
            return Ok(());
        }
        let files: Vec<_> = self
            .files
            .iter()
            .filter(|p| p.starts_with(root))
            .cloned()
            .collect();
        let active = self
            .files
            .get(self.active)
            .and_then(|p| files.iter().position(|f| f == p))
            .unwrap_or(0);
        let session = Session {
            folder: Some(root.to_path_buf()),
            files,
            active,
            ..Session::default()
        };
        let mut bytes = format!("branch={branch}\n").into_bytes();
        bytes.extend(session.serialize());
        std::fs::create_dir_all(dir)?;
        crate::platform::write_atomically(&Self::branch_path(dir, root, branch), &bytes)
    }

    /// Drops files that no longer exist, so a deleted file does not resurrect
    /// as an error tab on every launch.
    pub fn prune(&mut self) {
        // The active tab stays the same file: every missing one before it
        // moves it up by one.
        let removed_before = self
            .files
            .iter()
            .take(self.active)
            .filter(|p| !p.is_file())
            .count();
        self.active -= removed_before;
        self.files.retain(|p| p.is_file());
        self.recent.retain(|p| p.is_dir());
        if self.folder.as_deref().is_some_and(|p| !p.is_dir()) {
            self.folder = None;
        }
        self.active = self.active.min(self.files.len().saturating_sub(1));
    }
}

/// True when `path` is somewhere sensible to reopen from.
pub fn is_reopenable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_sessions_are_isolated_and_validate_their_key() {
        let dir = std::env::temp_dir().join(format!("crc-branches-{}", std::process::id()));
        let root = dir.join("checkout");
        let other = dir.join("worktree");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let a = root.join("a.txt");
        let b = root.join("b.txt");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        let session = Session {
            files: vec![other.join("outside"), a.clone(), b.clone()],
            active: 2,
            ..Session::default()
        };
        let store = dir.join("sessions");
        session.write_branch(&store, &root, "feature/a").unwrap();
        let saved = Session::read_branch(&store, &root, "feature/a").unwrap();
        assert_eq!(saved.files, [a.clone(), b.clone()]);
        assert_eq!(saved.active, 1);
        assert!(Session::read_branch(&store, &root, "main").is_none());
        assert!(Session::read_branch(&store, &other, "feature/a").is_none());
        // Missing files are pruned without moving focus to the wrong file.
        std::fs::remove_file(&a).unwrap();
        let saved = Session::read_branch(&store, &root, "feature/a").unwrap();
        assert_eq!(saved.files, [b]);
        assert_eq!(saved.active, 0);
        let path = Session::branch_path(&store, &root, "feature/a");
        std::fs::write(&path, b"branch=wrong\nfile=/tmp/unrelated\n").unwrap();
        assert!(Session::read_branch(&store, &root, "feature/a").is_none());
        Session::default()
            .write_branch(&store, &root, "main")
            .unwrap();
        assert!(
            Session::read_branch(&store, &root, "main")
                .unwrap()
                .files
                .is_empty()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn odd_paths_survive_and_pruning_keeps_the_active_file() {
        use std::os::unix::ffi::OsStrExt;
        let odd = PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/caf\xe9/notes "));
        let session = Session {
            files: vec![odd.clone()],
            ..Session::default()
        };
        assert_eq!(Session::parse_bytes(&session.serialize()).files, [odd]);

        let dir = std::env::temp_dir().join(format!("caio-session-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (b, c) = (dir.join("b"), dir.join("c"));
        std::fs::write(&b, "").unwrap();
        std::fs::write(&c, "").unwrap();
        let mut session = Session {
            files: vec![dir.join("gone"), b.clone(), c],
            active: 1,
            ..Session::default()
        };
        session.prune();
        assert_eq!(session.files[session.active], b, "still b, not c");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn round_trips() {
        let session = Session {
            frame: Some((100.0, 200.0, 1200.0, 800.0)),
            folder: Some(PathBuf::from("/tmp/project")),
            files: vec![PathBuf::from("/tmp/a.rs"), PathBuf::from("/tmp/b.rs")],
            active: 1,
            sidebar: false,
            sidebar_width: 310.0,
            recent: vec![PathBuf::from("/tmp/project"), PathBuf::from("/tmp/older")],
        };
        let parsed = Session::parse_bytes(&session.serialize());
        assert_eq!(parsed, session);
    }

    /// Regression: `load()` fell back to a derived Default where `sidebar`
    /// was false, while `parse("")` set it true. Two defaults for one thing
    /// is how a fresh launch ended up hiding the sidebar.
    #[test]
    fn a_missing_file_and_an_empty_one_agree() {
        assert_eq!(Session::default(), Session::parse(""));
        assert!(Session::default().sidebar);
    }

    #[test]
    fn a_missing_file_yields_defaults() {
        let s = Session::parse("");
        assert!(s.frame.is_none());
        assert!(s.files.is_empty());
        assert!(s.sidebar, "the sidebar should default to showing");
    }

    #[test]
    fn garbage_is_ignored_rather_than_fatal() {
        let s = Session::parse("this is not a session\nframe=nonsense\n!!!\nactive=abc\n");
        assert!(
            s.frame.is_none(),
            "a frame that does not parse is dropped whole"
        );
        assert_eq!(s.active, 0);
    }

    #[test]
    fn a_partial_frame_is_rejected() {
        let s = Session::parse("frame=1,2,3\n");
        assert!(s.frame.is_none(), "three numbers is not a rectangle");
    }

    #[test]
    fn a_zero_sized_frame_is_rejected() {
        let s = Session::parse("frame=0,0,0,0\n");
        assert!(
            s.frame.is_none(),
            "restoring a zero-sized window is useless"
        );
    }

    #[test]
    fn active_is_clamped_to_the_file_list() {
        let s = Session::parse("file=/tmp/a\nfile=/tmp/b\nactive=99\n");
        assert_eq!(s.active, 1);
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let s = Session::parse("# a comment\n\nfile=/tmp/x\n");
        assert_eq!(s.files.len(), 1);
    }

    #[test]
    fn prune_drops_files_that_are_gone() {
        let dir = std::env::temp_dir().join("caio-session-tests");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let real = dir.join("real.txt");
        std::fs::write(&real, "x").expect("write");

        let mut s = Session {
            files: vec![real.clone(), dir.join("missing.txt")],
            folder: Some(dir.join("no-such-folder")),
            active: 1,
            ..Session::default()
        };
        s.prune();

        assert_eq!(s.files, vec![real.clone()]);
        assert!(s.folder.is_none());
        assert_eq!(s.active, 0, "active must stay in range after pruning");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn paths_with_spaces_survive() {
        let s = Session::parse("file=/tmp/a file with spaces.rs\n");
        assert_eq!(s.files[0], PathBuf::from("/tmp/a file with spaces.rs"));
    }
}
