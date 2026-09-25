//! The project file tree behind the sidebar.
//!
//! Kept as a flat `Vec` of visible rows rather than a nested structure. The
//! sidebar draws rows and hit-tests rows, so the representation that matters
//! is the flattened one; nesting is expressed by each row's `depth`. Expanding
//! a directory splices its children in, collapsing removes them.
//!
//! Directory contents are read once on expand and cached. Nothing here
//! watches the filesystem yet, so [`Tree::refresh`] exists for the cases
//! where we know something changed.

use std::path::{Path, PathBuf};

/// Moves a project item without replacing any entry already at `destination`.
///
/// `Path::exists` misses dangling symlinks, and checking before `rename` also
/// leaves a race with another process creating the destination. macOS provides
/// an atomic exclusive rename for both files and directories.
pub fn move_without_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn renamex_np(
            from: *const std::ffi::c_char,
            to: *const std::ffi::c_char,
            flags: u32,
        ) -> std::ffi::c_int;
    }
    const RENAME_EXCL: u32 = 0x0000_0004;

    let from = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let to = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: both pointers remain valid NUL-terminated paths for the call.
    if unsafe { renamex_np(from.as_ptr(), to.as_ptr(), RENAME_EXCL) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// One visible row in the sidebar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    /// File name only; the sidebar has no room for full paths.
    pub name: String,
    /// Nesting level, 0 for the root's direct children.
    pub depth: usize,
    pub is_dir: bool,
    /// Directories only: whether children are currently spliced in below.
    pub expanded: bool,
}

#[derive(Clone, Default)]
pub struct Tree {
    root: Option<PathBuf>,
    rows: Vec<Entry>,
    /// Which row is highlighted, as an index into `rows`.
    pub selected: Option<usize>,
    /// First visible row.
    pub scroll: usize,
    /// What Git ignores under the root, from [`crate::project::git::ignored`].
    /// Shared, so cloning the tree for a worker stays cheap.
    ignored: std::sync::Arc<std::collections::HashSet<PathBuf>>,
    /// What the `.gitignore` line being typed would ignore, before it is
    /// saved: the Explorer shows it while you type.
    preview: std::collections::HashSet<PathBuf>,
}

/// Whether Git ignores a path, and where the rule applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ignored {
    No,
    /// This path itself is ignored: `dist/`, `.env`.
    Here,
    /// Inside an ignored directory.
    Inside,
    /// Would be ignored by the line being typed into a `.gitignore`.
    Preview,
}

/// Directories that are never worth showing in an editor sidebar. Hidden
/// entries are filtered separately, so this is only for the noisy ones that
/// do not start with a dot.
const SKIP_DIRS: &[&str] = &["target", "node_modules", "vendor", ".git"];

impl Tree {
    pub fn new() -> Self {
        Tree::default()
    }

    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    pub fn rows(&self) -> &[Entry] {
        &self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Opens a directory as the project root, replacing whatever was there.
    pub fn open(&mut self, root: impl Into<PathBuf>) {
        let root = root.into();
        self.rows = read_dir(&root, 0);
        self.root = Some(root);
        self.selected = None;
        self.scroll = 0;
    }

    /// Records a project immediately while its directory rows are read on a worker.
    pub fn set_root(&mut self, root: impl Into<PathBuf>) {
        self.root = Some(root.into());
        self.rows.clear();
        self.selected = None;
        self.scroll = 0;
        self.ignored = Default::default();
        self.preview.clear();
    }

    pub fn set_ignored(&mut self, ignored: std::sync::Arc<std::collections::HashSet<PathBuf>>) {
        self.ignored = ignored;
    }

    pub fn ignored_set(&self) -> std::sync::Arc<std::collections::HashSet<PathBuf>> {
        self.ignored.clone()
    }

    pub fn set_preview(&mut self, preview: std::collections::HashSet<PathBuf>) {
        self.preview = preview;
    }

    /// Whether Git ignores `path`: one lookup for the path, then one per
    /// directory above it up to the root.
    pub fn ignored(&self, path: &Path) -> Ignored {
        if self.ignored.is_empty() && self.preview.is_empty() {
            return Ignored::No;
        }
        if self.ignored.contains(path) {
            return Ignored::Here;
        }
        let root = self.root.as_deref();
        let mut above = path
            .ancestors()
            .skip(1)
            .take_while(|dir| Some(*dir) != root);
        if above.clone().any(|dir| self.ignored.contains(dir)) {
            return Ignored::Inside;
        }
        if self.preview.contains(path) || above.any(|dir| self.preview.contains(dir)) {
            return Ignored::Preview;
        }
        Ignored::No
    }

    /// Re-reads every currently expanded directory, preserving what was open.
    ///
    /// Called after creating a file, since nothing watches the filesystem.
    pub fn refresh(&mut self) {
        let Some(root) = self.root.clone() else {
            return;
        };
        let expanded: Vec<PathBuf> = self
            .rows
            .iter()
            .filter(|e| e.is_dir && e.expanded)
            .map(|e| e.path.clone())
            .collect();
        let selected_path = self
            .selected
            .and_then(|i| self.rows.get(i))
            .map(|e| e.path.clone());

        self.rows = read_dir(&root, 0);
        // Re-expand outermost-first, so nested paths exist by the time they
        // are reached.
        let mut wanted = expanded;
        wanted.sort_by_key(|p| p.components().count());
        for path in wanted {
            if let Some(i) = self.rows.iter().position(|e| e.path == path) {
                self.expand(i);
            }
        }

        self.selected = selected_path.and_then(|p| self.rows.iter().position(|e| e.path == p));
    }

    /// Closes every expanded directory, leaving the top level showing.
    ///
    /// Unlike `refresh`, which deliberately re-expands what was open.
    pub fn collapse_all(&mut self) {
        if self.root.is_none() {
            return;
        }
        let selected_path = self
            .selected
            .and_then(|i| self.rows.get(i))
            .map(|e| e.path.clone());
        // The entries are already in memory. Collapse must not rescan a large
        // project on the AppKit event thread or silently act as Refresh.
        self.rows.retain(|entry| entry.depth == 0);
        for entry in &mut self.rows {
            entry.expanded = false;
        }
        self.scroll = 0;
        // A selection inside a directory that is now closed has no row to
        // point at; keep it only if its row survived.
        self.selected = selected_path.and_then(|p| self.rows.iter().position(|e| e.path == p));
    }

    /// Expands or collapses the directory at `index`.
    pub fn toggle(&mut self, index: usize) {
        let Some(entry) = self.rows.get(index) else {
            return;
        };
        if !entry.is_dir {
            return;
        }
        if entry.expanded {
            self.collapse(index);
        } else {
            self.expand(index);
        }
    }

    /// Reads children without touching the visible tree, for a worker thread.
    pub fn children(path: &Path, depth: usize) -> Vec<Entry> {
        read_dir(path, depth)
    }

    /// Installs a worker's result if the directory is still visible and closed.
    pub fn install_children(&mut self, path: &Path, children: Vec<Entry>) -> bool {
        let Some(index) = self
            .rows
            .iter()
            .position(|row| row.path == path && row.is_dir && !row.expanded)
        else {
            return false;
        };
        let selected_path = self
            .selected
            .and_then(|i| self.rows.get(i))
            .map(|e| e.path.clone());
        self.rows[index].expanded = true;
        self.rows.splice(index + 1..index + 1, children);
        self.selected =
            selected_path.and_then(|path| self.rows.iter().position(|e| e.path == path));
        true
    }

    fn expand(&mut self, index: usize) {
        let Some(entry) = self.rows.get(index) else {
            return;
        };
        if !entry.is_dir || entry.expanded {
            return;
        }
        let children = read_dir(&entry.path, entry.depth + 1);
        self.rows[index].expanded = true;
        self.rows.splice(index + 1..index + 1, children);
    }

    fn collapse(&mut self, index: usize) {
        let Some(entry) = self.rows.get(index) else {
            return;
        };
        if !entry.is_dir || !entry.expanded {
            return;
        }
        let depth = entry.depth;
        // Everything deeper than this row, until the next sibling, is a
        // descendant and goes away with it.
        let end = self.rows[index + 1..]
            .iter()
            .position(|e| e.depth <= depth)
            .map(|offset| index + 1 + offset)
            .unwrap_or(self.rows.len());
        self.rows.drain(index + 1..end);
        self.rows[index].expanded = false;
    }

    /// The directory new files should land in: the selected directory, the
    /// parent of the selected file, or the root.
    pub fn target_dir(&self) -> Option<PathBuf> {
        match self.selected.and_then(|i| self.rows.get(i)) {
            Some(e) if e.is_dir => Some(e.path.clone()),
            Some(e) => e.path.parent().map(Path::to_path_buf),
            None => self.root.clone(),
        }
    }

    /// Selects the row at `index`, if it exists.
    pub fn select(&mut self, index: usize) -> Option<&Entry> {
        if index >= self.rows.len() {
            return None;
        }
        self.selected = Some(index);
        self.rows.get(index)
    }

    /// Selects the row holding `path`, expanding ancestors as needed.
    pub fn reveal(&mut self, path: &Path) {
        let Some(root) = self.root.clone() else {
            return;
        };
        let Ok(relative) = path.strip_prefix(&root) else {
            return;
        };

        // Walk down the components, expanding each directory on the way.
        let mut current = root;
        let components: Vec<_> = relative.components().collect();
        for (i, component) in components.iter().enumerate() {
            current = current.join(component);
            let Some(index) = self.rows.iter().position(|e| e.path == current) else {
                return;
            };
            if i + 1 < components.len() {
                self.expand(index);
            } else {
                self.selected = Some(index);
            }
        }
    }

    /// Keeps the selected row inside a viewport of `rows` rows.
    pub fn scroll_to_selection(&mut self, rows: usize) {
        let Some(selected) = self.selected else {
            return;
        };
        if selected < self.scroll {
            self.scroll = selected;
        } else if rows > 0 && selected >= self.scroll + rows {
            self.scroll = selected + 1 - rows;
        }
    }

    pub fn scroll_by(&mut self, delta: isize, rows: usize) {
        let max = self.rows.len().saturating_sub(rows.max(1));
        let next = self.scroll as isize + delta;
        self.scroll = next.clamp(0, max as isize) as usize;
    }
}

/// Reads one directory into rows, sorted directories-first then by name.
///
/// Unreadable directories yield nothing rather than propagating an error: a
/// permission-denied folder should render as empty, not take down the
/// sidebar.
fn read_dir(path: &Path, depth: usize) -> Vec<Entry> {
    let Ok(entries) = std::fs::read_dir(path) else {
        return Vec::new();
    };

    let mut out: Vec<Entry> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir && SKIP_DIRS.contains(&name.as_str()) {
                return None;
            }
            Some(Entry {
                path: entry.path(),
                name,
                depth,
                is_dir,
                expanded: false,
            })
        })
        .collect();

    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

#[cfg(test)]
mod tests {

    #[test]
    fn ignored_is_here_for_the_rule_and_inside_below_it() {
        let mut tree = Tree::new();
        tree.set_root("/p");
        assert_eq!(
            tree.ignored(Path::new("/p/dist")),
            Ignored::No,
            "nothing known yet"
        );
        tree.set_ignored(std::sync::Arc::new(
            [PathBuf::from("/p/dist"), PathBuf::from("/p/.env")]
                .into_iter()
                .collect(),
        ));
        assert_eq!(tree.ignored(Path::new("/p/dist")), Ignored::Here);
        assert_eq!(tree.ignored(Path::new("/p/.env")), Ignored::Here);
        assert_eq!(tree.ignored(Path::new("/p/dist/a/b.js")), Ignored::Inside);
        assert_eq!(tree.ignored(Path::new("/p/src/main.rs")), Ignored::No);
        assert_eq!(
            tree.ignored(Path::new("/p/distant")),
            Ignored::No,
            "a prefix is not a parent"
        );
        tree.set_root("/q");
        assert_eq!(
            tree.ignored(Path::new("/p/dist")),
            Ignored::No,
            "a new root forgets"
        );
    }

    use super::*;

    /// Builds a throwaway directory tree and removes it on drop, so a failing
    /// test cannot leave litter in /tmp.
    struct Fixture(PathBuf);

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("caio-tree-{name}"));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("src")).expect("mkdir src");
            std::fs::create_dir_all(root.join("src/nested")).expect("mkdir nested");
            std::fs::create_dir_all(root.join("target")).expect("mkdir target");
            std::fs::create_dir_all(root.join(".git")).expect("mkdir .git");
            std::fs::write(root.join("README.md"), "hi").expect("write");
            std::fs::write(root.join("src/main.rs"), "fn main() {}").expect("write");
            std::fs::write(root.join("src/nested/deep.rs"), "//").expect("write");
            Fixture(root)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn lists_directories_first_then_files() {
        let f = Fixture::new("order");
        let mut t = Tree::new();
        t.open(&f.0);
        let names: Vec<&str> = t.rows().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["src", "README.md"]);
    }

    #[test]
    fn shows_dotfiles_and_dot_directories_but_skips_git_and_build_output() {
        let f = Fixture::new("hidden");
        std::fs::write(f.0.join(".env"), "EXAMPLE=value").unwrap();
        std::fs::write(f.0.join(".gitignore"), "target/").unwrap();
        std::fs::create_dir(f.0.join(".github")).unwrap();
        std::fs::write(f.0.join("src/.env"), "NESTED=value").unwrap();
        let mut t = Tree::new();
        t.open(&f.0);
        let names: Vec<&str> = t.rows().iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&".env"));
        assert!(names.contains(&".gitignore"));
        assert!(names.contains(&".github"));
        assert!(!names.contains(&".git"), "Git internals stay hidden");
        assert!(!names.contains(&"target"), "build output should be hidden");
        let children = Tree::children(&f.0.join("src"), 1);
        assert!(children.iter().any(|e| e.name == ".env" && !e.is_dir));
    }

    #[test]
    fn expanding_splices_children_in_below() {
        let f = Fixture::new("expand");
        let mut t = Tree::new();
        t.open(&f.0);
        t.toggle(0); // src
        let names: Vec<&str> = t.rows().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["src", "nested", "main.rs", "README.md"]);
        assert_eq!(t.rows()[1].depth, 1, "children sit one level deeper");
    }

    #[test]
    fn worker_children_install_only_into_a_visible_closed_directory() {
        let f = Fixture::new("worker-expand");
        let mut t = Tree::new();
        t.open(&f.0);
        let path = f.0.join("src");
        let children = Tree::children(&path, 1);
        assert!(t.install_children(&path, children.clone()));
        assert_eq!(t.rows()[1].name, "nested");
        assert!(!t.install_children(&path, children));
        assert!(!t.install_children(&f.0.join("missing"), Vec::new()));
    }

    #[test]
    fn collapsing_removes_the_whole_subtree() {
        let f = Fixture::new("collapse");
        let mut t = Tree::new();
        t.open(&f.0);
        t.toggle(0); // src
        t.toggle(1); // src/nested
        assert_eq!(t.len(), 5, "src, nested, deep.rs, main.rs, README.md");

        t.toggle(0); // collapse src
        let names: Vec<&str> = t.rows().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["src", "README.md"],
            "collapsing src must take its grandchildren too"
        );
    }

    #[test]
    fn reveal_expands_ancestors_and_selects() {
        let f = Fixture::new("reveal");
        let mut t = Tree::new();
        t.open(&f.0);
        t.reveal(&f.0.join("src/nested/deep.rs"));

        let selected = t.selected.and_then(|i| t.rows().get(i)).expect("selected");
        assert_eq!(selected.name, "deep.rs");
        assert!(t.rows().iter().any(|e| e.name == "nested" && e.expanded));
    }

    #[test]
    fn refresh_picks_up_new_files_and_keeps_expansion() {
        let f = Fixture::new("refresh");
        let mut t = Tree::new();
        t.open(&f.0);
        t.toggle(0); // src expanded
        assert!(!t.rows().iter().any(|e| e.name == "added.rs"));

        std::fs::write(f.0.join("src/added.rs"), "//").expect("write");
        t.refresh();

        assert!(
            t.rows().iter().any(|e| e.name == "added.rs"),
            "new file missing"
        );
        assert!(
            t.rows().iter().any(|e| e.name == "src" && e.expanded),
            "refresh collapsed the tree"
        );
    }

    #[test]
    fn collapse_all_uses_visible_rows_without_refreshing_disk() {
        let f = Fixture::new("collapse-all");
        let mut t = Tree::new();
        t.open(&f.0);
        t.toggle(0);
        t.select(2); // src/main.rs
        std::fs::write(f.0.join("later.txt"), "new").unwrap();

        t.collapse_all();
        assert_eq!(
            t.rows().iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["src", "README.md"]
        );
        assert!(!t.rows()[0].expanded);
        assert_eq!(t.selected, None, "hidden selection must be cleared");

        t.refresh();
        assert!(t.rows().iter().any(|e| e.name == "later.txt"));
    }

    #[test]
    fn exclusive_move_preserves_existing_files_and_dangling_symlinks() {
        use std::os::unix::fs::symlink;

        let f = Fixture::new("exclusive-move");
        let source = f.0.join("README.md");
        let occupied = f.0.join("src/README.md");
        std::fs::write(&occupied, "keep").unwrap();
        assert_eq!(
            move_without_replace(&source, &occupied).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read_to_string(&occupied).unwrap(), "keep");
        assert!(source.exists());

        std::fs::remove_file(&occupied).unwrap();
        symlink("missing-target", &occupied).unwrap();
        assert!(!occupied.exists(), "the link is deliberately dangling");
        assert_eq!(
            move_without_replace(&source, &occupied).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            std::fs::read_link(&occupied).unwrap(),
            Path::new("missing-target")
        );
        assert!(source.exists());

        std::fs::remove_file(&occupied).unwrap();
        move_without_replace(&source, &occupied).unwrap();
        assert!(!source.exists());
        assert_eq!(std::fs::read_to_string(&occupied).unwrap(), "hi");
    }

    #[test]
    fn target_dir_follows_the_selection() {
        let f = Fixture::new("target");
        let mut t = Tree::new();
        t.open(&f.0);
        assert_eq!(t.target_dir().as_deref(), Some(f.0.as_path()));

        t.toggle(0);
        t.select(0); // src, a directory
        assert_eq!(t.target_dir(), Some(f.0.join("src")));

        t.select(2); // main.rs, a file
        assert_eq!(
            t.target_dir(),
            Some(f.0.join("src")),
            "should use the parent"
        );
    }

    #[test]
    fn an_unreadable_root_yields_an_empty_tree_not_a_panic() {
        let mut t = Tree::new();
        t.open("/definitely/not/a/real/path");
        assert!(t.is_empty());
        assert_eq!(
            t.target_dir().as_deref(),
            Some(Path::new("/definitely/not/a/real/path"))
        );
    }
}
