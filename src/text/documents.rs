//! The set of open buffers and which one is showing.
//!
//! Deliberately a plain `Vec` with an index rather than anything cleverer.
//! Tab order is the order they were opened, an editor is unlikely to hold
//! enough documents for a linear scan to matter, and the ordering has to be
//! stable because it is what the tab bar draws and what Cmd-1..9 address.

use std::path::Path;

use crate::text::buffer::Buffer;

pub struct Documents {
    buffers: Vec<Buffer>,
    active: usize,
}

impl Documents {
    /// Starts with a single buffer, which is always present: closing the last
    /// document leaves an empty untitled one rather than no document at all,
    /// so the rest of the editor never has to handle "nothing is open".
    pub fn new(initial: Buffer) -> Self {
        Documents {
            buffers: vec![initial],
            active: 0,
        }
    }

    pub fn active(&self) -> &Buffer {
        &self.buffers[self.active]
    }

    pub fn active_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[self.active]
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn len(&self) -> usize {
        self.buffers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffers.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Buffer> {
        self.buffers.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Buffer> {
        self.buffers.iter_mut()
    }

    /// Re-keys open documents after a file or directory is renamed in the
    /// project tree. Descendants follow a renamed directory as one operation.
    pub fn rename_path(&mut self, old: &Path, new: &Path) {
        for buffer in &mut self.buffers {
            let Some(path) = buffer.path.as_deref() else {
                continue;
            };
            let replacement = if path == old {
                Some(new.to_path_buf())
            } else {
                path.strip_prefix(old).ok().map(|suffix| new.join(suffix))
            };
            if let Some(path) = replacement {
                buffer.path = Some(std::fs::canonicalize(&path).unwrap_or(path));
            }
        }
    }

    /// Whether moving `path` away would strand edits that only exist in RAM.
    pub fn has_dirty_under(&self, path: &Path) -> bool {
        self.buffers.iter().any(|buffer| {
            buffer.is_dirty()
                && buffer
                    .path
                    .as_deref()
                    .is_some_and(|open| open == path || open.starts_with(path))
        })
    }

    /// Closes clean documents whose files were moved out of the project.
    pub fn close_under(&mut self, path: &Path) {
        let mut indices: Vec<usize> = self
            .buffers
            .iter()
            .enumerate()
            .filter_map(|(index, buffer)| {
                buffer
                    .path
                    .as_deref()
                    .is_some_and(|open| open == path || open.starts_with(path))
                    .then_some(index)
            })
            .collect();
        indices.reverse();
        for index in indices {
            self.close(index);
        }
    }

    /// Whether nothing is open: the one document is the untouched, untitled,
    /// empty buffer that is always there so the rest of the editor never has
    /// to handle "no document".
    ///
    /// The window shows its home screen for this instead of an empty editor
    /// called "Untitled". It is not a mode: type, and the buffer is no longer
    /// untouched, so it becomes an ordinary untitled document; open a file,
    /// and [`Documents::push`] replaces it.
    pub fn is_home(&self) -> bool {
        self.buffers.len() == 1 && is_untouched(&self.buffers[0])
    }

    /// Whether any document has unsaved changes.
    pub fn any_dirty(&self) -> bool {
        self.buffers.iter().any(|b| b.is_dirty())
    }

    /// Switches to `index` if it exists.
    pub fn switch(&mut self, index: usize) -> bool {
        if index >= self.buffers.len() {
            return false;
        }
        self.active = index;
        true
    }

    /// Moves `delta` tabs, wrapping at both ends.
    pub fn cycle(&mut self, delta: isize) {
        if self.buffers.is_empty() {
            return;
        }
        let len = self.buffers.len() as isize;
        self.active = ((self.active as isize + delta).rem_euclid(len)) as usize;
    }

    /// Moves a tab while keeping the same document active.
    pub fn move_tab(&mut self, from: usize, to: usize) -> bool {
        if from >= self.buffers.len() || to >= self.buffers.len() || from == to {
            return false;
        }
        let buffer = self.buffers.remove(from);
        self.buffers.insert(to, buffer);
        if self.active == from {
            self.active = to;
        } else if from < self.active && self.active <= to {
            self.active -= 1;
        } else if to <= self.active && self.active < from {
            self.active += 1;
        }
        true
    }

    /// Index of an already-open document for `path`.
    ///
    /// Canonicalises the key first. Buffers store canonical paths, and on
    /// macOS that is not cosmetic: `/var` is a symlink to `/private/var`, so
    /// a raw path from a panel or the sidebar never equals the stored one and
    /// every reopen would make a fresh tab onto the same file.
    pub fn index_of(&self, path: &Path) -> Option<usize> {
        let canonical = std::fs::canonicalize(path);
        let key = canonical.as_deref().unwrap_or(path);
        self.buffers
            .iter()
            .position(|b| b.path.as_deref() == Some(key))
    }

    /// Opens a file, or switches to it when it is already open.
    ///
    /// Re-opening never makes a second tab for the same file: two views of one
    /// document that can diverge is a whole class of bug not worth having.
    pub fn open(&mut self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref();
        if let Some(index) = self.index_of(path) {
            self.active = index;
            return Ok(());
        }
        let buffer = Buffer::open(path)?;
        self.push(buffer);
        Ok(())
    }

    /// Adds a document, switching to an existing tab for the same path.
    ///
    /// The dedup lives here rather than only in `open` because every route
    /// that can produce a tab has to share it: creating a file that happens
    /// to be open already would otherwise make a second one.
    pub fn add(&mut self, buffer: Buffer) {
        if let Some(path) = buffer.path.as_deref()
            && let Some(index) = self.index_of(path)
        {
            self.active = index;
            return;
        }
        self.push(buffer);
    }

    /// Puts back a document recovered after a crash.
    ///
    /// When its file is already open, which it will be if the session
    /// restored it a moment ago, the recovered text takes that tab: the one
    /// there is a clean copy of the last save, and this is what came after.
    /// A tab with changes of its own is never replaced.
    pub fn restore(&mut self, buffer: Buffer) {
        let existing = buffer.path.as_deref().and_then(|p| self.index_of(p));
        match existing {
            Some(index) if !self.buffers[index].is_dirty() => {
                self.buffers[index] = buffer;
                self.active = index;
            }
            _ => self.push(buffer),
        }
    }

    /// Adds a document and makes it active.
    ///
    /// An untouched, untitled, empty buffer is replaced rather than kept: the
    /// blank document you get at launch should not linger as a stray tab once
    /// you open something real.
    pub fn push(&mut self, buffer: Buffer) {
        if self.is_home() {
            self.buffers[0] = buffer;
            self.active = 0;
            return;
        }
        self.buffers.push(buffer);
        self.active = self.buffers.len() - 1;
    }

    /// Closes `index`, returning the buffer that was removed.
    ///
    /// Closing the last document leaves a fresh empty one behind. The caller
    /// is responsible for asking about unsaved changes first; this does not
    /// prompt, because prompting needs AppKit and this does not.
    pub fn close(&mut self, index: usize) -> Option<Buffer> {
        if index >= self.buffers.len() {
            return None;
        }
        let removed = self.buffers.remove(index);
        if self.buffers.is_empty() {
            self.buffers.push(Buffer::new());
            self.active = 0;
        } else if self.active >= self.buffers.len() {
            self.active = self.buffers.len() - 1;
        } else if index < self.active {
            self.active -= 1;
        }
        Some(removed)
    }

    /// Display name for a tab: the file name, "Untitled", or "Home" when
    /// nothing is open.
    pub fn title(&self, index: usize) -> String {
        if self.is_home() {
            return "Home".to_string();
        }
        let Some(buffer) = self.buffers.get(index) else {
            return "Untitled".to_string();
        };
        buffer
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .or_else(|| buffer.label.clone())
            .unwrap_or_else(|| "Untitled".to_string())
    }

    /// The tab of the generated document titled `label`, if one is open.
    pub fn index_of_label(&self, label: &str) -> Option<usize> {
        self.buffers
            .iter()
            .position(|b| b.path.is_none() && b.label.as_deref() == Some(label))
    }
}

fn is_untouched(buffer: &Buffer) -> bool {
    buffer.path.is_none() && !buffer.is_dirty() && buffer.rope.len_bytes() == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> Documents {
        Documents::new(Buffer::new())
    }

    #[test]
    fn moving_tabs_preserves_the_active_document() {
        let mut docs = scratch();
        for name in ["a", "b", "c"] {
            let mut buffer = Buffer::from_text(name);
            buffer.insert("!");
            docs.push(buffer);
        }
        docs.switch(1);
        assert!(docs.move_tab(0, 2));
        assert_eq!(docs.active_index(), 0);
        assert_eq!(docs.active().rope.to_string(), "!b");
        assert!(docs.move_tab(0, 2));
        assert_eq!(docs.active_index(), 2);
        assert_eq!(docs.active().rope.to_string(), "!b");
    }

    #[test]
    fn project_rename_rekeys_open_descendants() {
        let root = std::env::temp_dir().join(format!("caio-docs-rename-{}", std::process::id()));
        let old = root.join("old");
        let new = root.join("new");
        std::fs::create_dir_all(&old).unwrap();
        let file = old.join("note.txt");
        std::fs::write(&file, "note").unwrap();
        let mut docs = Documents::new(Buffer::open(&file).unwrap());
        let old_key = std::fs::canonicalize(&old).unwrap();
        std::fs::rename(&old, &new).unwrap();

        docs.rename_path(&old_key, &new);
        let expected = std::fs::canonicalize(new.join("note.txt")).unwrap();
        assert_eq!(docs.active().path.as_deref(), Some(expected.as_path()));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn project_trash_guard_finds_dirty_descendants_and_closes_clean_ones() {
        let root = std::env::temp_dir().join(format!("caio-docs-trash-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("note.txt");
        std::fs::write(&file, "note").unwrap();
        let root_key = std::fs::canonicalize(&root).unwrap();
        let mut docs = Documents::new(Buffer::open(&file).unwrap());
        docs.active_mut().insert(" changed");
        assert!(docs.has_dirty_under(&root_key));

        let mut clean = Documents::new(Buffer::open(&file).unwrap());
        assert!(!clean.has_dirty_under(&root_key));
        clean.close_under(&root_key);
        assert!(clean.is_home());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn starts_with_one_document() {
        let d = scratch();
        assert_eq!(d.len(), 1);
        assert_eq!(d.active_index(), 0);
        assert_eq!(d.title(0), "Home", "nothing open is Home, not a document");
        assert!(d.is_home());
    }

    #[test]
    fn home_ends_when_something_is_typed_or_opened_and_returns_when_all_is_closed() {
        let mut d = scratch();
        d.active_mut().insert("x");
        assert!(!d.is_home());
        assert_eq!(d.title(0), "Untitled");

        let mut d = scratch();
        d.push(Buffer::from_text("a file"));
        assert!(!d.is_home());
        assert_eq!(d.len(), 1, "Home is replaced, not kept as a stray tab");

        d.close(0);
        assert!(d.is_home(), "closing the last document comes back to Home");
    }

    #[test]
    fn pushing_replaces_an_untouched_scratch_buffer() {
        let mut d = scratch();
        d.push(Buffer::from_text("real content"));
        assert_eq!(d.len(), 1, "the empty scratch tab should not linger");
        assert_eq!(d.active().rope.to_string(), "real content");
    }

    #[test]
    fn pushing_keeps_a_scratch_buffer_that_has_been_typed_in() {
        let mut d = scratch();
        d.active_mut().insert("unsaved work");
        d.push(Buffer::from_text("second"));
        assert_eq!(d.len(), 2, "must not discard a dirty scratch buffer");
        assert_eq!(d.active_index(), 1);
    }

    #[test]
    fn switching_and_cycling_wrap() {
        let mut d = scratch();
        d.active_mut().insert("a");
        d.push(Buffer::from_text("b"));
        d.push(Buffer::from_text("c"));
        assert_eq!(d.len(), 3);
        assert_eq!(d.active_index(), 2);

        d.cycle(1);
        assert_eq!(d.active_index(), 0, "should wrap past the end");
        d.cycle(-1);
        assert_eq!(d.active_index(), 2, "should wrap past the start");

        assert!(d.switch(1));
        assert_eq!(d.active_index(), 1);
        assert!(!d.switch(9), "out of range switch should be refused");
        assert_eq!(d.active_index(), 1);
    }

    #[test]
    fn closing_before_the_active_tab_keeps_the_same_document_showing() {
        let mut d = scratch();
        d.active_mut().insert("first");
        d.push(Buffer::from_text("second"));
        d.push(Buffer::from_text("third"));
        d.switch(2);

        d.close(0);
        assert_eq!(d.len(), 2);
        assert_eq!(
            d.active().rope.to_string(),
            "third",
            "closing an earlier tab must not change which document is visible"
        );
    }

    #[test]
    fn closing_the_active_tab_falls_back_in_range() {
        let mut d = scratch();
        d.active_mut().insert("first");
        d.push(Buffer::from_text("second"));
        d.switch(1);
        d.close(1);
        assert_eq!(d.len(), 1);
        assert_eq!(d.active_index(), 0);
    }

    #[test]
    fn closing_the_last_document_leaves_an_empty_one() {
        let mut d = scratch();
        d.active_mut().insert("only");
        d.close(0);
        assert_eq!(d.len(), 1, "there is always a document");
        assert!(d.active().rope.is_empty());
        assert!(!d.active().is_dirty());
    }

    #[test]
    fn reopening_a_path_switches_rather_than_duplicating() {
        let dir = std::env::temp_dir().join("caio-docs-tests");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("one.txt");
        std::fs::write(&path, "content").expect("write");

        let mut d = scratch();
        d.open(&path).expect("open");
        d.push(Buffer::from_text("other"));
        assert_eq!(d.len(), 2);

        d.open(&path).expect("reopen");
        assert_eq!(d.len(), 2, "must not open a second tab for the same file");
        assert_eq!(d.active_index(), 0, "should switch to the existing tab");

        std::fs::remove_file(&path).ok();
    }

    /// The window used to open files by assigning over the active buffer,
    /// which threw away whatever was being edited. `open` is the only route
    /// now, so this is the property every open path inherits.
    #[test]
    fn opening_a_file_leaves_unsaved_work_in_other_tabs_alone() {
        let dir = std::env::temp_dir().join("caio-docs-open-keeps");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&a, "a on disk").expect("write");
        std::fs::write(&b, "b on disk").expect("write");

        let mut d = scratch();
        d.open(&a).expect("open a");
        d.active_mut().insert("unsaved ");
        d.open(&b).expect("open b");

        assert_eq!(d.len(), 2, "b gets its own tab");
        assert_eq!(d.active().rope.to_string(), "b on disk");
        assert!(d.any_dirty(), "a is still open and still dirty");
        d.switch(0);
        assert_eq!(d.active().rope.to_string(), "unsaved a on disk");

        // A failed open changes nothing either.
        assert!(d.open(dir.join("missing.txt")).is_err());
        assert_eq!(d.len(), 2);
        assert_eq!(d.active_index(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_switches_rather_than_duplicating() {
        let dir = std::env::temp_dir().join("caio-docs-add");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("one.txt");
        std::fs::write(&path, "content").expect("write");

        let mut d = scratch();
        d.add(Buffer::open(&path).expect("open"));
        d.push(Buffer::from_text("other"));
        assert_eq!(d.len(), 2);

        // A second buffer for the same file, as `new_file` would produce.
        d.add(Buffer::open(&path).expect("open"));
        assert_eq!(d.len(), 2, "must not open a second tab for one file");
        assert_eq!(d.active_index(), 0);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn any_dirty_sees_background_tabs() {
        let mut d = scratch();
        d.active_mut().insert("dirty");
        d.push(Buffer::from_text("clean"));
        assert!(!d.active().is_dirty());
        assert!(d.any_dirty(), "a dirty tab in the background still counts");
    }

    #[test]
    fn a_recovered_document_takes_over_the_clean_tab_for_its_file() {
        let dir = std::env::temp_dir().join("caio-docs-restore");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("notes.md");
        std::fs::write(&path, "last save").expect("write");
        let canonical = std::fs::canonicalize(&path).expect("canonical");

        let mut d = scratch();
        d.open(&path).expect("session restore opens it");
        d.push(Buffer::from_text("other"));

        d.restore(Buffer::recovered(
            Some(canonical.clone()),
            "last save, and more",
        ));
        assert_eq!(d.len(), 2, "no second tab for the same file");
        assert_eq!(d.active_index(), 0);
        assert_eq!(d.active().rope.to_string(), "last save, and more");
        assert!(
            d.active().is_dirty(),
            "recovered text is unsaved by definition"
        );

        // Untitled work comes back as a tab of its own.
        d.restore(Buffer::recovered(None, "never had a name"));
        assert_eq!(d.len(), 3);

        // And a tab that has changes of its own is not overwritten.
        d.restore(Buffer::recovered(Some(canonical), "an older crash"));
        assert_eq!(d.len(), 4);
        d.switch(0);
        assert_eq!(d.active().rope.to_string(), "last save, and more");

        std::fs::remove_dir_all(&dir).ok();
    }
}
