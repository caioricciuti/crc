//! Files and images dropped on the window, and images pasted into a
//! terminal. A drop on the terminal types the files' quoted paths, as
//! Terminal and iTerm do: an agent given a screenshot's path reads the
//! image. A drop on the editor opens the files. Image data without a file
//! (dragged or copied from a browser, a screenshot on the clipboard) is
//! saved as PNG first, under Application Support, so it has a path.

use std::path::{Path, PathBuf};

use objc2_app_kit::NSPasteboard;

use super::*;

impl EditorView {
    /// What a drag over the window may do: copy, for files and images;
    /// nothing for anything else, so the pointer says so.
    pub(super) fn drag_operation(&self, pasteboard: &NSPasteboard) -> NSDragOperation {
        if !file_urls(pasteboard).is_empty() || clipboard::has_image_on(pasteboard) {
            NSDragOperation::Copy
        } else {
            NSDragOperation::None
        }
    }

    /// Takes a drop at `(x, y)` in view coordinates; `true` when something
    /// was done with it.
    pub(super) fn drop_pasteboard(&self, pasteboard: &NSPasteboard, x: f32, y: f32) -> bool {
        let mut paths = file_urls(pasteboard);
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

    /// Types `paths` into the active session, quoted for the shell and
    /// a space apart, as one paste.
    fn type_paths(&self, paths: &[PathBuf]) {
        let text = paths
            .iter()
            .map(|p| shell_quote(&p.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(" ");
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

    /// Opens a dropped file in a tab, or says why not.
    fn open_dropped_file(&self, path: &Path) {
        if path.is_dir() {
            if let Some(mut state) = self.state_mut() {
                state.say(
                    layout::Feedback::Info,
                    format!("{}: a folder; File > Open Folder opens one", path.display()),
                );
            }
            return;
        }
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            match Buffer::open(path) {
                Ok(buffer) => {
                    state.docs.add(buffer);
                    reveal_active_tab(&mut state);
                }
                Err(e) => {
                    state.say(
                        layout::Feedback::Failure,
                        format!("{}: {e}", path.display()),
                    );
                    return;
                }
            }
        }
        self.sync_title();
        self.reparse();
        self.lsp_sync_open();
        self.request_redraw();
        self.pump();
    }
}

/// The files on the pasteboard, as paths.
fn file_urls(pasteboard: &NSPasteboard) -> Vec<PathBuf> {
    let Some(items) = pasteboard.pasteboardItems() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let text = item.stringForType(unsafe { NSPasteboardTypeFileURL })?;
            let url = NSURL::URLWithString(&text)?;
            let path = url.path()?;
            Some(PathBuf::from(path.to_string()))
        })
        .collect()
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
