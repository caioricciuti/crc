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
