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

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long typed letters are kept as one prefix: after a pause this long
/// the next letter starts a new one.
const TYPE_AHEAD_RESET: Duration = Duration::from_millis(1000);

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
    /// The selected items, by path: rows come and go above them (a refresh,
    /// a folder opening or closing), and an index would then point at
    /// something else. Every one of them has a row; see [`Tree::settle`].
    selection: HashSet<PathBuf>,
    /// Where a Shift-click or Shift-arrow range is measured from.
    anchor: Option<PathBuf>,
    /// The keyboard's item, and the one a single-item command acts on.
    /// While anything is selected it is one of the selected items.
    cursor: Option<PathBuf>,
    /// The row showing `cursor`, kept in step by [`Tree::settle`], so
    /// neither drawing nor a key press has to search for it.
    cursor_row: Option<usize>,
    /// Whether the tree has the keyboard: the selection is drawn at full
    /// strength and the cursor row is outlined. Set by the window.
    pub focused: bool,
    /// The letters typed to jump to a name, lower-cased, and when the last
    /// one was typed.
    typed: Option<(String, Instant)>,
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
        self.clear_selection();
        self.scroll = 0;
    }

    /// Records a project immediately while its directory rows are read on a worker.
    pub fn set_root(&mut self, root: impl Into<PathBuf>) {
        self.root = Some(root.into());
        self.rows.clear();
        self.clear_selection();
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

        self.rows = read_dir(&root, 0);
        // Re-expand outermost-first, so nested paths exist by the time they
        // are reached.
        let mut wanted = expanded;
        wanted.sort_by_key(|p| p.components().count());
        for path in wanted {
            if let Some(i) = self.rows.iter().position(|e| e.path == path) {
                self.splice_children(i);
            }
        }

        self.settle(true);
    }

    /// Closes every expanded directory, leaving the top level showing.
    ///
    /// Unlike `refresh`, which deliberately re-expands what was open.
    pub fn collapse_all(&mut self) {
        if self.root.is_none() {
            return;
        }
        // The entries are already in memory. Collapse must not rescan a large
        // project on the AppKit event thread or silently act as Refresh.
        self.rows.retain(|entry| entry.depth == 0);
        for entry in &mut self.rows {
            entry.expanded = false;
        }
        self.scroll = 0;
        // A selection inside a directory that is now closed has no row to
        // point at; keep only what still has one.
        self.settle(false);
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
        self.rows[index].expanded = true;
        self.rows.splice(index + 1..index + 1, children);
        self.settle(true);
        true
    }

    fn expand(&mut self, index: usize) {
        if self.splice_children(index) {
            self.settle(true);
        }
    }

    /// Reads the directory at `index` and puts its children below it.
    /// Whether it did; the caller settles the selection.
    fn splice_children(&mut self, index: usize) -> bool {
        let Some(entry) = self.rows.get(index) else {
            return false;
        };
        if !entry.is_dir || entry.expanded {
            return false;
        }
        let children = read_dir(&entry.path, entry.depth + 1);
        self.rows[index].expanded = true;
        self.rows.splice(index + 1..index + 1, children);
        true
    }

    /// The row showing `path`, if one does.
    fn row_of(&self, path: &Path) -> Option<usize> {
        self.rows.iter().position(|e| e.path == path)
    }

    /// Brings the selection back in step with the rows after they changed.
    /// What has no row any more is no longer selected: a command must never
    /// act on an item that is not showing. A cursor whose row went (inside
    /// a folder now closed, or deleted) is handed to the nearest folder
    /// above it that still shows when `hand_over`, and dropped otherwise.
    fn settle(&mut self, hand_over: bool) {
        let was_selected = self
            .cursor
            .as_ref()
            .is_some_and(|cursor| self.selection.contains(cursor));
        if !self.selection.is_empty() {
            let mut before = std::mem::take(&mut self.selection);
            for entry in &self.rows {
                if let Some(path) = before.take(&entry.path) {
                    self.selection.insert(path);
                }
            }
        }
        self.cursor_row = None;
        if let Some(cursor) = self.cursor.take() {
            let root = self.root.as_deref();
            let mut at = Some(cursor.as_path());
            while let Some(path) = at.filter(|path| Some(*path) != root) {
                if let Some(index) = self.row_of(path) {
                    self.cursor_row = Some(index);
                    break;
                }
                if !hand_over {
                    break;
                }
                at = path.parent();
            }
            if let Some(index) = self.cursor_row {
                let path = self.rows[index].path.clone();
                if was_selected {
                    self.selection.insert(path.clone());
                }
                self.cursor = Some(path);
            }
        }
        // The cursor went and other items are still selected: the first of
        // them has it now.
        if self.cursor_row.is_none()
            && let Some(index) = (0..self.rows.len()).find(|&i| self.is_selected_path(i))
        {
            self.cursor = Some(self.rows[index].path.clone());
            self.cursor_row = Some(index);
        }
        if !self
            .anchor
            .as_deref()
            .is_some_and(|anchor| self.row_of(anchor).is_some())
        {
            self.anchor = self.cursor.clone();
        }
    }

    /// The cursor's row: where the keyboard is in the tree.
    pub fn cursor_row(&self) -> Option<usize> {
        self.cursor_row
    }

    /// Whether the row at `index` is selected. Asked for every row drawn,
    /// so the usual case, one item selected, does not hash a path.
    pub fn is_selected(&self, index: usize) -> bool {
        match self.selection.len() {
            0 => false,
            1 => self.cursor_row == Some(index),
            _ => self
                .rows
                .get(index)
                .is_some_and(|entry| self.selection.contains(&entry.path)),
        }
    }

    /// How many items are selected.
    pub fn selection_len(&self) -> usize {
        self.selection.len()
    }

    /// The cursor's entry while it is selected: the one item a command for
    /// one item (Rename, New File) acts on.
    fn primary(&self) -> Option<&Entry> {
        if self.selection.is_empty() {
            return None;
        }
        self.cursor_row.and_then(|index| self.rows.get(index))
    }

    /// The one item a single-item command acts on: the cursor's.
    pub fn selected_path(&self) -> Option<PathBuf> {
        self.primary().map(|entry| entry.path.clone())
    }

    /// Every selected item, top to bottom as the rows show them.
    pub fn selected_paths(&self) -> Vec<PathBuf> {
        (0..self.rows.len())
            .filter(|&index| self.is_selected(index))
            .map(|index| self.rows[index].path.clone())
            .collect()
    }

    /// What a command on the selection acts on: the selected items, less
    /// those inside a folder that is selected too, since moving or trashing
    /// the folder takes them with it. Top to bottom.
    pub fn selected_roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = Vec::new();
        for path in self.selected_paths() {
            // Rows list a folder before what is in it, so anything nested
            // comes straight after the folder kept last.
            if roots.last().is_none_or(|kept| !path.starts_with(kept)) {
                roots.push(path);
            }
        }
        roots
    }

    /// Selects nothing and leaves the keyboard nowhere.
    pub fn clear_selection(&mut self) {
        self.selection.clear();
        self.anchor = None;
        self.cursor = None;
        self.cursor_row = None;
        self.typed = None;
    }

    /// Cmd-click: adds the row at `index` to the selection, or takes it
    /// out. The next Shift range starts from it either way.
    pub fn toggle_selected(&mut self, index: usize) {
        let Some(entry) = self.rows.get(index) else {
            return;
        };
        let path = entry.path.clone();
        self.anchor = Some(path.clone());
        if self.selection.remove(&path) {
            // The cursor stays among what is selected: the next selected
            // row below, or else above. With nothing left it rests here.
            let near = (index + 1..self.rows.len())
                .chain((0..index).rev())
                .find(|&row| self.is_selected_path(row));
            let row = near.unwrap_or(index);
            self.cursor = Some(self.rows[row].path.clone());
            self.cursor_row = Some(row);
        } else {
            self.selection.insert(path.clone());
            self.cursor = Some(path);
            self.cursor_row = Some(index);
        }
    }

    /// [`Tree::is_selected`] by the set alone, for while the cursor is
    /// being moved and cannot stand in for a single selection.
    fn is_selected_path(&self, index: usize) -> bool {
        self.rows
            .get(index)
            .is_some_and(|entry| self.selection.contains(&entry.path))
    }

    /// A right-click on the row at `index`: one of the selected rows keeps
    /// the selection, so a command from the menu acts on all of it, and
    /// only the cursor moves there; any other row is selected alone.
    pub fn select_for_menu(&mut self, index: usize) {
        if self.is_selected(index) {
            self.cursor = Some(self.rows[index].path.clone());
            self.cursor_row = Some(index);
        } else {
            self.select(index);
        }
    }

    /// Shift-click and Shift-arrows: selects the rows from the anchor to
    /// `index`, which the cursor moves to. The anchor stays.
    pub fn select_to(&mut self, index: usize) {
        if index >= self.rows.len() {
            return;
        }
        let anchor = self
            .anchor
            .as_deref()
            .and_then(|anchor| self.row_of(anchor))
            .or(self.cursor_row)
            .unwrap_or(index);
        self.selection.clear();
        for entry in &self.rows[anchor.min(index)..=anchor.max(index)] {
            self.selection.insert(entry.path.clone());
        }
        self.anchor = Some(self.rows[anchor].path.clone());
        self.cursor = Some(self.rows[index].path.clone());
        self.cursor_row = Some(index);
    }

    /// Selects every row showing. The cursor stays where it is.
    pub fn select_all(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.selection = self.rows.iter().map(|e| e.path.clone()).collect();
        let row = self.cursor_row.unwrap_or(0);
        self.cursor = Some(self.rows[row].path.clone());
        self.cursor_row = Some(row);
        self.anchor = self.cursor.clone();
    }

    /// Escape: several items selected become the cursor's alone. Whether
    /// there was anything to reduce.
    pub fn reduce_selection(&mut self) -> bool {
        if self.selection.len() < 2 {
            return false;
        }
        match self.cursor_row {
            Some(row) => {
                self.select(row);
            }
            None => self.clear_selection(),
        }
        true
    }

    /// Moves the cursor to `index`: selecting that row alone, or with
    /// `extend` (Shift) the rows from the anchor to it.
    pub fn move_cursor(&mut self, index: usize, extend: bool) {
        if extend {
            self.select_to(index);
        } else {
            self.select(index);
        }
    }

    /// Moves the cursor `delta` rows, stopping at the first and the last.
    /// With no cursor yet, down starts at the first row and up at the last.
    pub fn step_cursor(&mut self, delta: isize, extend: bool) {
        let Some(last) = self.rows.len().checked_sub(1) else {
            return;
        };
        let to = match self.cursor_row {
            Some(row) => (row as isize).saturating_add(delta).clamp(0, last as isize) as usize,
            None if delta < 0 => last,
            None => 0,
        };
        self.move_cursor(to, extend);
    }

    /// The row of the folder holding the row at `index`, for Left.
    pub fn parent_row(&self, index: usize) -> Option<usize> {
        let depth = self.rows.get(index)?.depth;
        self.rows[..index].iter().rposition(|e| e.depth < depth)
    }

    /// The first row inside the open folder at `index`, for Right.
    pub fn first_child_row(&self, index: usize) -> Option<usize> {
        let entry = self.rows.get(index)?;
        let child = self.rows.get(index + 1)?;
        (entry.is_dir && entry.expanded && child.depth > entry.depth).then_some(index + 1)
    }

    /// A letter typed while the tree has the keyboard: the cursor jumps to
    /// the next row whose name starts with what has been typed since the
    /// last pause, whatever its case. Whether a row was found.
    pub fn type_ahead(&mut self, letter: char, now: Instant) -> bool {
        let mut prefix = match self.typed.take() {
            Some((typed, at)) if now.saturating_duration_since(at) < TYPE_AHEAD_RESET => typed,
            _ => String::new(),
        };
        prefix.extend(letter.to_lowercase());
        let found = self.jump_to_prefix(&prefix);
        self.typed = Some((prefix, now));
        found
    }

    /// Whether letters typed before `now` are still being added to: a
    /// space then belongs to the name being typed.
    pub fn typing_ahead(&self, now: Instant) -> bool {
        self.typed
            .as_ref()
            .is_some_and(|(_, at)| now.saturating_duration_since(*at) < TYPE_AHEAD_RESET)
    }

    fn jump_to_prefix(&mut self, prefix: &str) -> bool {
        let len = self.rows.len();
        let Some(first) = prefix.chars().next() else {
            return false;
        };
        // One letter, or the same one again, steps on to the next name that
        // starts with it. A longer prefix stays on a row it already fits.
        let stepping = prefix.chars().all(|c| c == first);
        let prefix = if stepping {
            &prefix[..first.len_utf8()]
        } else {
            prefix
        };
        let start = match self.cursor_row {
            Some(row) if stepping => row + 1,
            Some(row) => row,
            None => 0,
        };
        let hit = (0..len)
            .map(|offset| (start + offset) % len)
            .find(|&row| starts_with_lowered(&self.rows[row].name, prefix));
        match hit {
            Some(row) => {
                self.select(row);
                true
            }
            None => false,
        }
    }

    /// An item was renamed or moved, from `from` to `to`: what was selected
    /// at the old path, or under it, is selected at the new one, so the
    /// selection is still on the item when the tree is read again.
    pub fn follow_move(&mut self, from: &Path, to: &Path) {
        let moved = |path: &Path| path.strip_prefix(from).ok().map(|rest| to.join(rest));
        self.selection = std::mem::take(&mut self.selection)
            .into_iter()
            .map(|path| moved(&path).unwrap_or(path))
            .collect();
        for path in [&mut self.anchor, &mut self.cursor].into_iter().flatten() {
            if let Some(now) = moved(path) {
                *path = now;
            }
        }
    }

    /// Takes the selection, the keyboard and the scroll position of
    /// `live`, the tree that was showing while this one was read on a
    /// worker: what was clicked or typed meanwhile is not lost.
    pub fn adopt_selection(&mut self, live: &Tree) {
        self.selection = live.selection.clone();
        self.anchor = live.anchor.clone();
        self.cursor = live.cursor.clone();
        self.typed = live.typed.clone();
        self.focused = live.focused;
        self.scroll = live.scroll;
        self.settle(true);
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
        // A cursor that was inside is on the folder now.
        self.settle(true);
    }

    /// The directory new files should land in: the selected directory, the
    /// parent of the selected file, or the root. With several selected, the
    /// cursor's.
    pub fn target_dir(&self) -> Option<PathBuf> {
        match self.primary() {
            Some(e) if e.is_dir => Some(e.path.clone()),
            Some(e) => e.path.parent().map(Path::to_path_buf),
            None => self.root.clone(),
        }
    }

    /// Selects the row at `index` alone, if it exists: the cursor and the
    /// anchor are there too.
    pub fn select(&mut self, index: usize) -> Option<&Entry> {
        let path = self.rows.get(index)?.path.clone();
        self.selection.clear();
        self.selection.insert(path.clone());
        self.anchor = Some(path.clone());
        self.cursor = Some(path);
        self.cursor_row = Some(index);
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
                self.select(index);
            }
        }
    }

    /// Keeps the cursor's row inside a viewport of `rows` rows.
    pub fn scroll_to_selection(&mut self, rows: usize) {
        let Some(selected) = self.cursor_row else {
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

/// Whether `name` starts with `prefix`, which is already lower-case,
/// whatever the case of `name`. Compared letter by letter: this runs for
/// every row on a key press and must not build a string for each.
fn starts_with_lowered(name: &str, prefix: &str) -> bool {
    let mut letters = name.chars().flat_map(char::to_lowercase);
    prefix.chars().all(|c| letters.next() == Some(c))
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
            // Through a symlink, what it points at: a linked folder is a
            // folder to expand, not a file to open as text.
            let is_dir = entry.file_type().is_ok_and(|t| {
                t.is_dir()
                    || (t.is_symlink() && std::fs::metadata(entry.path()).is_ok_and(|m| m.is_dir()))
            });
            if is_dir && super::SKIP_DIRS.contains(&name.as_str()) {
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

    /// A small project: `src` with a nested folder, `target` and `.git`
    /// (which the tree hides), and a README.
    struct Fixture;

    impl Fixture {
        #[allow(clippy::new_ret_no_self)]
        fn new(name: &str) -> crate::project::TempTree {
            crate::project::TempTree::new(
                &format!("tree-{name}"),
                &[
                    ("target/", ""),
                    (".git/", ""),
                    ("README.md", "hi"),
                    ("src/main.rs", "fn main() {}"),
                    ("src/nested/deep.rs", "//"),
                ],
            )
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

        let selected = t
            .cursor_row()
            .and_then(|i| t.rows().get(i))
            .expect("selected");
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
        assert_eq!(t.cursor_row(), None, "hidden selection must be cleared");
        assert_eq!(t.selection_len(), 0, "hidden selection must be cleared");

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

    /// Two folders and four files: `docs`, `src`, `alpha.txt`, `beta.txt`,
    /// `Brief.md`, `notes.txt`, in that order, the folders closed.
    fn picking(name: &str) -> (crate::project::TempTree, Tree) {
        let f = crate::project::TempTree::new(
            &format!("tree-pick-{name}"),
            &[
                ("docs/guide.md", ""),
                ("docs/api/index.md", ""),
                ("src/main.rs", ""),
                ("src/lib.rs", ""),
                ("alpha.txt", ""),
                ("beta.txt", ""),
                ("Brief.md", ""),
                ("notes.txt", ""),
            ],
        );
        let mut t = Tree::new();
        t.open(&f.0);
        assert_eq!(
            names(&t),
            [
                "docs",
                "src",
                "alpha.txt",
                "beta.txt",
                "Brief.md",
                "notes.txt"
            ]
        );
        (f, t)
    }

    fn names(t: &Tree) -> Vec<&str> {
        t.rows().iter().map(|e| e.name.as_str()).collect()
    }

    /// The selected rows' names, top to bottom, the cursor's in brackets.
    fn picked(t: &Tree) -> Vec<String> {
        (0..t.len())
            .filter(|&i| t.is_selected(i))
            .map(|i| {
                let name = &t.rows()[i].name;
                if t.cursor_row() == Some(i) {
                    format!("[{name}]")
                } else {
                    name.clone()
                }
            })
            .collect()
    }

    fn row(t: &Tree, name: &str) -> usize {
        t.rows().iter().position(|e| e.name == name).expect(name)
    }

    #[test]
    fn a_shift_range_runs_from_the_anchor_either_way() {
        let (_f, mut t) = picking("range");
        t.select(2);
        assert_eq!(picked(&t), ["[alpha.txt]"]);
        t.select_to(4);
        assert_eq!(picked(&t), ["alpha.txt", "beta.txt", "[Brief.md]"]);
        // Back over the anchor: the range turns round it, nothing is left
        // behind from the first one.
        t.select_to(0);
        assert_eq!(picked(&t), ["[docs]", "src", "alpha.txt"]);
        // A plain select starts over, and moves the anchor.
        t.select(5);
        t.select_to(4);
        assert_eq!(picked(&t), ["[Brief.md]", "notes.txt"]);
        // With nothing picked yet, the range is the row itself.
        t.clear_selection();
        t.select_to(3);
        assert_eq!(picked(&t), ["[beta.txt]"]);
        t.select_to(99);
        assert_eq!(picked(&t), ["[beta.txt]"], "no such row");
    }

    #[test]
    fn a_toggle_adds_and_removes_and_keeps_the_cursor_selected() {
        let (f, mut t) = picking("toggle");
        t.select(2);
        t.toggle_selected(4);
        assert_eq!(picked(&t), ["alpha.txt", "[Brief.md]"]);
        assert_eq!(t.selection_len(), 2);
        assert_eq!(t.selected_path(), Some(f.0.join("Brief.md")));
        // The next range starts from the row toggled last.
        t.select_to(5);
        assert_eq!(picked(&t), ["Brief.md", "[notes.txt]"]);

        t.select(2);
        t.toggle_selected(4);
        t.toggle_selected(0);
        // Off again: the cursor goes to what is still selected, below first.
        t.toggle_selected(0);
        assert_eq!(picked(&t), ["[alpha.txt]", "Brief.md"]);
        t.toggle_selected(4);
        assert_eq!(picked(&t), ["[alpha.txt]"]);
        // The last one off: nothing is selected, so nothing is acted on,
        // and the keyboard stays where it was.
        t.toggle_selected(2);
        assert_eq!(picked(&t), [] as [&str; 0]);
        assert_eq!(t.selection_len(), 0);
        assert_eq!(t.selected_path(), None);
        assert_eq!(t.cursor_row(), Some(2));
        assert_eq!(t.target_dir().as_deref(), Some(f.0.as_path()));

        // A right-click on one of several selected rows keeps them and
        // moves the cursor there; on any other row it selects that one.
        t.select(2);
        t.select_to(4);
        t.select_for_menu(3);
        assert_eq!(picked(&t), ["alpha.txt", "[beta.txt]", "Brief.md"]);
        t.select_for_menu(5);
        assert_eq!(picked(&t), ["[notes.txt]"]);
    }

    #[test]
    fn the_selection_follows_its_items_through_refresh_expand_and_collapse() {
        let (f, mut t) = picking("survive");
        t.select(row(&t, "beta.txt"));
        t.toggle_selected(row(&t, "notes.txt"));
        assert_eq!(picked(&t), ["beta.txt", "[notes.txt]"]);

        // Rows arrive above: a folder opens.
        t.toggle(row(&t, "docs"));
        assert_eq!(t.cursor_row(), Some(row(&t, "notes.txt")));
        assert_eq!(picked(&t), ["beta.txt", "[notes.txt]"]);
        // A worker's rows arrive.
        let src = f.0.join("src");
        assert!(t.install_children(&src, Tree::children(&src, 1)));
        assert_eq!(picked(&t), ["beta.txt", "[notes.txt]"]);
        // A file appears before them, and one of them is deleted.
        std::fs::write(f.0.join("aaa.txt"), "").unwrap();
        t.refresh();
        assert_eq!(picked(&t), ["beta.txt", "[notes.txt]"]);
        assert_eq!(t.cursor_row(), Some(row(&t, "notes.txt")));
        std::fs::remove_file(f.0.join("notes.txt")).unwrap();
        t.refresh();
        assert_eq!(picked(&t), ["[beta.txt]"], "the cursor's item went");
        // Rows leave above.
        t.toggle(row(&t, "docs"));
        assert_eq!(picked(&t), ["[beta.txt]"]);
        assert_eq!(t.selected_path(), Some(f.0.join("beta.txt")));

        // What a closing folder hides is no longer selected, and a cursor
        // that was inside is on the folder.
        t.toggle_selected(row(&t, "lib.rs"));
        t.toggle_selected(row(&t, "main.rs"));
        assert_eq!(picked(&t), ["lib.rs", "[main.rs]", "beta.txt"]);
        t.toggle(row(&t, "src"));
        assert_eq!(picked(&t), ["[src]", "beta.txt"]);
        assert_eq!(t.selected_paths(), [f.0.join("src"), f.0.join("beta.txt")]);
        // Opening it again does not bring them back.
        t.toggle(row(&t, "src"));
        assert_eq!(picked(&t), ["[src]", "beta.txt"]);
    }

    #[test]
    fn a_command_acts_on_a_selected_folder_and_not_on_what_is_in_it() {
        let (f, mut t) = picking("nested");
        t.toggle(row(&t, "docs"));
        t.toggle(row(&t, "api"));
        t.toggle(row(&t, "src"));
        t.select(row(&t, "docs"));
        t.toggle_selected(row(&t, "index.md"));
        t.toggle_selected(row(&t, "guide.md"));
        t.toggle_selected(row(&t, "lib.rs"));
        t.toggle_selected(row(&t, "alpha.txt"));
        assert_eq!(t.selection_len(), 5);
        assert_eq!(t.selected_paths().len(), 5);
        assert_eq!(
            t.selected_roots(),
            [
                f.0.join("docs"),
                f.0.join("src/lib.rs"),
                f.0.join("alpha.txt")
            ]
        );
        // A folder's name as a prefix of a neighbour's is not nesting.
        std::fs::write(f.0.join("docs-old.txt"), "").unwrap();
        t.refresh();
        t.select(row(&t, "docs"));
        t.toggle_selected(row(&t, "docs-old.txt"));
        assert_eq!(
            t.selected_roots(),
            [f.0.join("docs"), f.0.join("docs-old.txt")]
        );
        t.select_all();
        assert_eq!(t.selection_len(), t.len());
        assert_eq!(t.selected_roots().len(), 7, "the top level");
    }

    #[test]
    fn the_cursor_steps_over_closed_folders_and_into_open_ones() {
        let (f, mut t) = picking("cursor");
        // No cursor yet: down starts at the top, up at the bottom.
        t.step_cursor(-1, false);
        assert_eq!(picked(&t), ["[notes.txt]"]);
        t.clear_selection();
        t.step_cursor(1, false);
        assert_eq!(picked(&t), ["[docs]"]);
        // A closed folder is one row: its files are not stepped through.
        t.step_cursor(1, false);
        assert_eq!(picked(&t), ["[src]"]);
        assert_eq!(t.first_child_row(1), None, "closed");
        assert_eq!(t.parent_row(1), None, "top level");

        t.toggle(row(&t, "docs"));
        t.select(row(&t, "docs"));
        assert_eq!(t.first_child_row(0), Some(1));
        t.step_cursor(1, false);
        assert_eq!(t.selected_path(), Some(f.0.join("docs/api")));
        t.step_cursor(1, false);
        assert_eq!(t.selected_path(), Some(f.0.join("docs/guide.md")));
        assert_eq!(t.parent_row(row(&t, "guide.md")), Some(0));
        assert_eq!(t.first_child_row(row(&t, "guide.md")), None, "a file");

        // Shift extends from where the cursor was, and comes back.
        t.step_cursor(2, true);
        assert_eq!(picked(&t), ["guide.md", "src", "[alpha.txt]"]);
        t.step_cursor(-1, true);
        assert_eq!(picked(&t), ["guide.md", "[src]"]);
        // The ends hold.
        t.step_cursor(100, false);
        assert_eq!(picked(&t), ["[notes.txt]"]);
        t.step_cursor(-100, true);
        assert_eq!(t.selection_len(), t.len());
        assert_eq!(t.cursor_row(), Some(0));
        // Escape leaves the cursor's row alone.
        assert!(t.reduce_selection());
        assert_eq!(picked(&t), ["[docs]"]);
        assert!(!t.reduce_selection(), "nothing more to reduce");

        // The cursor stays in a view of three rows.
        t.step_cursor(100, false);
        t.scroll_to_selection(3);
        assert_eq!(t.scroll, t.len() - 3);
        t.step_cursor(-100, false);
        t.scroll_to_selection(3);
        assert_eq!(t.scroll, 0);
    }

    #[test]
    fn typed_letters_jump_to_the_next_name_that_starts_with_them() {
        let (_f, mut t) = picking("typeahead");
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        t.select(0);
        // One letter, whatever its case, and the same again steps on.
        assert!(t.type_ahead('b', at(0)));
        assert_eq!(picked(&t), ["[beta.txt]"]);
        assert!(t.type_ahead('B', at(100)));
        assert_eq!(picked(&t), ["[Brief.md]"]);
        assert!(t.type_ahead('b', at(200)));
        assert_eq!(picked(&t), ["[beta.txt]"], "round to the first again");
        assert!(t.typing_ahead(at(900)));
        assert!(!t.typing_ahead(at(1300)));

        // After a pause a letter starts a new prefix; more letters narrow
        // it, staying on a row that still fits.
        t.select(0);
        assert!(t.type_ahead('b', at(2000)));
        assert_eq!(picked(&t), ["[beta.txt]"]);
        assert!(t.type_ahead('r', at(2100)));
        assert_eq!(picked(&t), ["[Brief.md]"]);
        assert!(t.type_ahead('i', at(2200)));
        assert_eq!(picked(&t), ["[Brief.md]"]);
        // Nothing starts with it: the cursor stays.
        assert!(!t.type_ahead('z', at(2300)));
        assert_eq!(picked(&t), ["[Brief.md]"]);
        // The pause is counted from the last letter.
        assert!(t.type_ahead('n', at(3400)));
        assert_eq!(picked(&t), ["[notes.txt]"]);
        // Only rows that show are searched.
        assert!(
            !t.type_ahead('g', at(5000)),
            "guide.md is in a closed folder"
        );
        t.toggle(row(&t, "docs"));
        assert!(t.type_ahead('g', at(7000)));
        assert_eq!(picked(&t), ["[guide.md]"]);
    }

    #[test]
    fn a_tree_read_on_a_worker_takes_the_selection_made_meanwhile() {
        let (f, mut live) = picking("adopt");
        live.select(row(&live, "alpha.txt"));
        // The worker's copy is taken, then the person goes on working.
        let mut read = live.clone();
        live.select(row(&live, "beta.txt"));
        live.toggle_selected(row(&live, "notes.txt"));
        live.scroll = 2;
        live.focused = true;
        std::fs::write(f.0.join("aaa.txt"), "").unwrap();
        std::fs::remove_file(f.0.join("notes.txt")).unwrap();
        read.refresh();
        assert_eq!(picked(&read), ["[alpha.txt]"]);

        read.adopt_selection(&live);
        assert!(names(&read).contains(&"aaa.txt"));
        assert_eq!(picked(&read), ["[beta.txt]"], "less the file that went");
        assert_eq!(read.cursor_row(), Some(row(&read, "beta.txt")));
        assert_eq!(read.scroll, 2);
        assert!(read.focused);
    }

    #[test]
    fn the_selection_follows_an_item_that_is_renamed_or_moved() {
        let (f, mut t) = picking("follow");
        t.toggle(row(&t, "docs"));
        t.select(row(&t, "guide.md"));
        t.toggle_selected(row(&t, "alpha.txt"));
        // A rename in place: still selected under its new name.
        std::fs::rename(f.0.join("alpha.txt"), f.0.join("omega.txt")).unwrap();
        t.follow_move(&f.0.join("alpha.txt"), &f.0.join("omega.txt"));
        t.refresh();
        assert_eq!(picked(&t), ["guide.md", "[omega.txt]"]);
        // A folder renamed: it is read again closed, so what was selected
        // inside it no longer shows and is no longer selected.
        std::fs::rename(f.0.join("docs"), f.0.join("manual")).unwrap();
        t.follow_move(&f.0.join("docs"), &f.0.join("manual"));
        t.refresh();
        assert_eq!(picked(&t), ["[omega.txt]"]);
        // Moved into a folder that is closed: the folder has the cursor.
        std::fs::rename(f.0.join("omega.txt"), f.0.join("src/omega.txt")).unwrap();
        t.follow_move(&f.0.join("omega.txt"), &f.0.join("src/omega.txt"));
        t.refresh();
        assert_eq!(picked(&t), ["[src]"]);
        assert_eq!(t.selected_path(), Some(f.0.join("src")));
    }
}
