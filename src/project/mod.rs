//! Project-level state: the file tree and the fuzzy file finder.

pub mod conflict;
pub mod finder;
pub mod git;
pub mod icons;
pub mod tree;
pub mod watch;

/// Directories never worth showing in the sidebar: enormous, and nobody
/// opens files in them by name. Hidden entries are filtered separately; the
/// finder and the watcher add a few of their own.
pub const SKIP_DIRS: &[&str] = &["target", "node_modules", "vendor", ".git"];

/// A throwaway folder for tests, removed on drop, so a failing test leaves
/// nothing behind in the temporary directory.
#[cfg(test)]
pub(crate) struct TempTree(pub std::path::PathBuf);

#[cfg(test)]
impl TempTree {
    /// `entries` are paths under the folder with their text; a path ending
    /// in `/` is an empty folder.
    pub(crate) fn new(name: &str, entries: &[(&str, &str)]) -> TempTree {
        let root = std::env::temp_dir().join(format!("crc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mkdir");
        for (path, text) in entries {
            let at = root.join(path);
            if path.ends_with('/') {
                std::fs::create_dir_all(&at).expect("mkdir");
            } else {
                std::fs::create_dir_all(at.parent().expect("parent")).expect("mkdir");
                std::fs::write(&at, text).expect("write");
            }
        }
        TempTree(root)
    }
}

#[cfg(test)]
impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
