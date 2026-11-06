//! Files and images dropped on the window, and images pasted into a
//! terminal. A drop on the terminal types the files' quoted paths, each
//! with a space after it, as Terminal and iTerm do: an agent given a
//! screenshot's path reads the image. A drop on the editor opens the files. A drop on the Explorer's
//! tree copies them into the folder under the pointer: see `tree_drag.rs`
//! for the drag over it and `explorer.rs` for the copy. Rows of that tree
//! dragged onto the terminal or the editor are typed and opened through
//! here as well: see `tree_drag/out.rs`. Image data without
//! a file (dragged or copied from a browser, a screenshot on the clipboard)
//! is saved as PNG first, under Application Support, so it has a path.

use std::path::{Path, PathBuf};

use objc2_app_kit::NSPasteboard;

use super::*;

impl EditorView {
    /// What a drag at `(x, y)` over the window may do: copy, for files and
    /// images; nothing for anything else, so the pointer says so. Over the
    /// Explorer's tree, copy where letting go would copy a file into the
    /// folder under the pointer, and nothing where it would do nothing.
    pub(super) fn drag_operation(
        &self,
        pasteboard: &NSPasteboard,
        x: f32,
        y: f32,
    ) -> NSDragOperation {
        // Rows of the Explorer taken out of the window and brought back:
        // ours still, wherever in the window they are.
        if let Some(operation) = self.returned_drag_over(pasteboard, x, y) {
            return operation;
        }
        if let Some(would) = self.outside_drag_over(pasteboard, x, y) {
            return if would {
                NSDragOperation::Copy
            } else {
                NSDragOperation::None
            };
        }
        if !clipboard::file_urls(pasteboard).is_empty() || clipboard::has_image_on(pasteboard) {
            NSDragOperation::Copy
        } else {
            NSDragOperation::None
        }
    }

    /// Takes a drop at `(x, y)` in view coordinates; `true` when something
    /// was done with it.
    pub(super) fn drop_pasteboard(&self, pasteboard: &NSPasteboard, x: f32, y: f32) -> bool {
        // Rows of the Explorer brought back: dropped as a drag that never
        // left the window is.
        if let Some(taken) = self.returned_drag_dropped(pasteboard, x, y) {
            return taken;
        }
        // On the Explorer's tree: copied into the project, not opened.
        if let Some(taken) = self.outside_drag_dropped(pasteboard, x, y) {
            return taken;
        }
        let mut paths = clipboard::file_urls(pasteboard);
        if paths.is_empty()
            && let Some(png) = clipboard::read_image_png_on(pasteboard)
            && let Some(saved) = save_image(&png)
        {
            paths.push(saved);
        }
        if paths.is_empty() {
            return false;
        }
        let chrome = self.chrome();
        let terminal_has_tabs = self
            .state()
            .is_some_and(|state| !state.terminal.tabs.is_empty());
        if chrome.terminal.is_some_and(|rect| rect.contains(x, y)) && terminal_has_tabs {
            self.type_paths(&paths);
            return true;
        }
        for path in &paths {
            self.open_dropped_file(path);
        }
        true
    }

    /// Pastes an image from the clipboard into the terminal: a Claude tab
    /// takes it from the clipboard itself (`Ctrl-V` is its key for that),
    /// anything else gets the path of a PNG saved from it. `false` when
    /// the clipboard holds no image.
    pub(super) fn paste_image_into_terminal(&self) -> bool {
        if !clipboard::has_image() {
            return false;
        }
        let claude = self
            .state()
            .and_then(|state| state.terminal.active_tab().map(|tab| tab.claude))
            .unwrap_or(false);
        if claude {
            self.terminal_write(b"\x16");
            return true;
        }
        let Some(saved) = clipboard::read_image_png().and_then(|png| save_image(&png)) else {
            return false;
        };
        self.type_paths(&[saved]);
        true
    }

    /// Types `paths` into the session `tab`, which gets the keyboard: in
    /// its pane, when it is another than the one that had it.
    pub(super) fn type_paths_in(&self, tab: usize, paths: &[PathBuf]) {
        if let Some(mut state) = self.state_mut()
            && tab != state.terminal.active
            && tab < state.terminal.tabs.len()
        {
            state.terminal.show(tab);
            note_focus(&mut state);
        }
        self.type_paths(paths);
    }

    /// Types `paths` into the active session, each quoted for the shell
    /// with a space after it, as one paste.
    fn type_paths(&self, paths: &[PathBuf]) {
        let text = typed_paths(paths);
        let bracketed = self.state().is_some_and(|state| {
            state.terminal.active_tab().is_some_and(|tab| {
                tab.session
                    .term
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .modes
                    .bracketed_paste
            })
        });
        if let Some(mut state) = self.state_mut() {
            state.terminal.focus = true;
            state.sidebar_keys = false;
        }
        self.terminal_write(&crate::term::keys::paste(&text, bracketed));
        self.request_redraw();
        self.pump();
    }

    /// Opens a dropped file in a tab, or says why not. Through `load_path`,
    /// as every other route: a file already open in another pane comes to
    /// the front there instead of opening a second copy that could diverge.
    pub(super) fn open_dropped_file(&self, path: &Path) {
        if path.is_dir() {
            if let Some(mut state) = self.state_mut() {
                state.say(
                    layout::Feedback::Info,
                    format!("{}: a folder; File > Open Folder opens one", path.display()),
                );
            }
            return;
        }
        self.load_path(&path.to_string_lossy());
        self.sync_title();
        self.reparse();
        self.request_redraw();
        self.pump();
    }
}

/// Where dropped and pasted images go: `drops/` under Application Support,
/// kept for a week.
fn drops_dir() -> Option<PathBuf> {
    let dir = crate::platform::app_support()?.join("drops");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Writes `png` to a file of its own and returns the path, pruning what
/// is a week old on the way.
fn save_image(png: &[u8]) -> Option<PathBuf> {
    let dir = drops_dir()?;
    prune(&dir, std::time::Duration::from_secs(7 * 24 * 60 * 60));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis();
    let path = dir.join(format!("image-{stamp}.png"));
    std::fs::write(&path, png).ok()?;
    Some(path)
}

fn prune(dir: &Path, max_age: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|at| now.duration_since(at).ok())
            .is_some_and(|age| age >= max_age);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// What is typed for `paths`: each quoted for the shell and followed by
/// a space, the last as well, as Terminal types a dropped file. What is
/// dropped or typed after it is then another word, not the end of this
/// one.
fn typed_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| format!("{} ", shell_quote(&p.to_string_lossy())))
        .collect()
}

/// Quoted for `sh`: the whole string, literally.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_quoted_for_the_shell() {
        assert_eq!(shell_quote("/a b/it's.png"), "'/a b/it'\\''s.png'");
    }

    /// What files dropped on a terminal type, rows of the Explorer and
    /// files from the Finder alike: a name with spaces is one word to the
    /// shell, a quote in one cannot end it, and each is followed by a
    /// space, the last one too, so that a second drop is other words and
    /// not the end of this one.
    #[test]
    fn dropped_paths_are_typed_as_one_word_each() {
        let paths = |names: &[&str]| -> Vec<PathBuf> { names.iter().map(PathBuf::from).collect() };
        assert_eq!(
            typed_paths(&paths(&["/p/notes/a walk in spring.md"])),
            "'/p/notes/a walk in spring.md' "
        );
        assert_eq!(
            typed_paths(&paths(&["/p/anna's \"best\" soup.md"])),
            "'/p/anna'\\''s \"best\" soup.md' "
        );
        let several = typed_paths(&paths(&[
            "/p/garden.txt",
            "/p/old recipes",
            "/p/caf\u{e9} $HOME `id`.md",
        ]));
        assert_eq!(
            several,
            "'/p/garden.txt' '/p/old recipes' '/p/caf\u{e9} $HOME `id`.md' "
        );
        // Two drops, one after the other, are as many words as one drop
        // of all of it.
        let twice = [
            typed_paths(&paths(&["/p/garden.txt"])),
            typed_paths(&paths(&["/p/old recipes", "/p/caf\u{e9} $HOME `id`.md"])),
        ]
        .concat();
        assert_eq!(twice, several);
        // Nothing dropped types nothing, a space included.
        assert_eq!(typed_paths(&[]), "");
    }

    #[test]
    fn saved_images_are_pruned_by_age() {
        let dir = std::env::temp_dir().join(format!("crc-drops-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("old.png"), b"x").unwrap();
        prune(&dir, std::time::Duration::ZERO);
        assert!(!dir.join("old.png").exists());
        std::fs::write(dir.join("new.png"), b"x").unwrap();
        prune(&dir, std::time::Duration::from_secs(60));
        assert!(dir.join("new.png").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
