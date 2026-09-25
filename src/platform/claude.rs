//! Claude Code, connected to the window.
//!
//! The protocol is in [`crate::ide`]; this is the editor's side of it: the
//! server and lock file for the open project, the diffs waiting for the
//! user's decision, and the selection last told to Claude. The window owns
//! one of these while a project is open and answers the tools through it.
//!
//! A proposal is a tab. Its text is Claude's version of the file, so the tab
//! behaves like any other document, and it draws as a diff against the file
//! on disk with Accept and Reject above it. Answering never writes the file:
//! Claude does that once it hears `FILE_SAVED`, and the watcher brings the
//! change into any open copy. The tab stays until Claude sends `close_tab`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::ide::lock::{self, Lock};
use crate::ide::mcp::{self, Selection};
use crate::ide::ws::{self, Event};
use crate::json::Value;
use crate::platform::git_panel::{DIFF_LINE, draw_diff_lines};
use crate::project::git::Diff;
use crate::render::font::Atlas;
use crate::render::layout::{self, Theme, Viewport};
use crate::render::metal::GlyphInstance;

/// A change Claude proposed, shown in its own tab.
pub struct Review {
    /// The `tools/call` to answer.
    pub request: Value,
    pub tab_name: String,
    pub path: PathBuf,
    pub diff: Diff,
    pub proposed: String,
    /// `Some(true)` once accepted, `Some(false)` once rejected.
    pub decided: Option<bool>,
    pub scroll: usize,
}

impl Review {
    /// Scrolls by `delta` lines, keeping the last line reachable.
    pub fn scroll_by(&mut self, delta: isize, rect: Viewport) {
        let visible = (rect.height / DIFF_LINE).max(1.0) as usize;
        let last = self.diff.lines.len().saturating_sub(visible);
        self.scroll = self.scroll.saturating_add_signed(delta).min(last);
    }
}

pub struct Bridge {
    server: ws::Server,
    lock: Lock,
    root: PathBuf,
    /// The connection `claude` is on. Replies to an older one are dropped.
    client: Option<u64>,
    /// Open reviews, by the id of their tab's buffer.
    pub reviews: HashMap<u64, Review>,
    /// The selection as last seen after a frame: buffer, anchor, caret.
    pub selection_seen: Option<(u64, usize, usize)>,
    /// When it last changed. The notification goes out once it settles.
    pub selection_changed_at: Option<Instant>,
    pub selection_sent: Option<Selection>,
}

impl Bridge {
    /// Listens, and writes the lock file that lets `claude` in `root`
    /// find the editor.
    pub fn start(root: &Path, wake: ws::Wake) -> std::io::Result<Bridge> {
        let token = ws::new_token()?;
        let server = ws::Server::start(token.clone(), wake)?;
        let dir = lock::dir().ok_or_else(|| std::io::Error::other("no home directory"))?;
        lock::remove_stale(&dir);
        let root = canonical(root);
        let lock = Lock::write(&dir, server.port(), &token, std::slice::from_ref(&root))?;
        Ok(Bridge {
            server,
            lock,
            root,
            client: None,
            reviews: HashMap::new(),
            selection_seen: None,
            selection_changed_at: None,
            selection_sent: None,
        })
    }

    /// Whether this bridge was started for `root`.
    pub fn serves(&self, root: &Path) -> bool {
        self.root == canonical(root)
    }

    pub fn set_root(&mut self, root: &Path) -> std::io::Result<()> {
        self.root = canonical(root);
        self.lock.update(std::slice::from_ref(&self.root))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn is_connected(&self) -> bool {
        self.client.is_some()
    }

    pub fn send(&self, text: &str) -> bool {
        self.server.send(text)
    }

    /// The next event from the server, with connection changes applied:
    /// reviews of a client that is gone can no longer be answered, so they
    /// count as rejected and their tabs are the caller's to close.
    pub fn next_event(&mut self) -> Option<Event> {
        let event = self.server.try_recv()?;
        match event {
            Event::Connected(id) => {
                self.abandon_reviews();
                self.client = Some(id);
                // Tell the new client where we are.
                self.selection_sent = None;
                self.selection_changed_at = Some(Instant::now());
            }
            Event::Closed(id) if self.client == Some(id) => {
                self.abandon_reviews();
                self.client = None;
            }
            _ => {}
        }
        Some(event)
    }

    /// Whether a message is from the current client.
    pub fn is_current(&self, id: u64) -> bool {
        self.client == Some(id)
    }

    fn abandon_reviews(&mut self) {
        for review in self.reviews.values_mut() {
            review.decided.get_or_insert(false);
        }
    }

    /// Answers the review in tab `buffer`. `false` when there is none or it
    /// was already answered.
    pub fn decide(&mut self, buffer: u64, accept: bool) -> bool {
        let Some(review) = self.reviews.get_mut(&buffer) else {
            return false;
        };
        if review.decided.is_some() {
            return false;
        }
        review.decided = Some(accept);
        let reply = if accept {
            mcp::tool_reply(&review.request, &["FILE_SAVED", &review.proposed])
        } else {
            mcp::tool_reply(&review.request, &["DIFF_REJECTED", &review.tab_name])
        };
        self.server.send(&reply);
        true
    }

    /// The tab showing the review named `tab_name`.
    pub fn buffer_for_tab(&self, tab_name: &str) -> Option<u64> {
        self.reviews
            .iter()
            .find(|(_, review)| review.tab_name == tab_name)
            .map(|(id, _)| *id)
    }

    /// Answers every open review as rejected, as on quit.
    pub fn reject_all(&mut self) {
        let open: Vec<u64> = self.reviews.keys().copied().collect();
        for id in open {
            self.decide(id, false);
        }
    }

    /// The port `claude` connects to.
    pub fn port(&self) -> u16 {
        self.lock.port()
    }

    pub fn lock_path(&self) -> &Path {
        self.lock.path()
    }
}

/// The folder as `claude` will compare it: canonical, since a symlink or a
/// different case does not match its working directory.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The Accept and Reject buttons in the strip above a review.
pub fn review_buttons(atlas: &mut Atlas, strip: Viewport) -> [Viewport; 2] {
    let mut x = strip.x + 16.0;
    let y = strip.y + 38.0;
    let mut rects = [strip; 2];
    for (rect, label) in rects.iter_mut().zip(BUTTONS) {
        let width = layout::ui_text_width(atlas, label) + 24.0;
        *rect = Viewport {
            x,
            y,
            width,
            height: 26.0,
        };
        x += width + 6.0;
    }
    rects
}

const BUTTONS: [&str; 2] = ["Accept  ⌘↩", "Reject  Esc"];

/// What the review is, how big, and the two answers.
pub fn draw_review_strip(
    review: &Review,
    root: Option<&Path>,
    atlas: &mut Atlas,
    strip: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    let pill = Viewport {
        x: strip.x + 16.0,
        y: strip.y + 6.0,
        width: layout::ui_text_width(atlas, "Claude") + 20.0,
        height: 26.0,
    };
    let tone = theme.accent;
    layout::push_rounded_rect(out, pill, 6.0, [tone[0], tone[1], tone[2], 0.18]);
    layout::push_ui_text_centered(out, atlas, pill, "Claude", tone);
    let shown = root
        .and_then(|root| review.path.strip_prefix(root).ok())
        .unwrap_or(&review.path);
    let facts = match review.decided {
        None => format!(
            "Proposed change to {}   +{} −{}",
            shown.display(),
            review.diff.added,
            review.diff.removed
        ),
        Some(true) => format!("Accepted. Claude writes {} next.", shown.display()),
        Some(false) => format!("Rejected. {} is unchanged.", shown.display()),
    };
    layout::push_ui_text(
        out,
        atlas,
        Viewport {
            x: pill.x + pill.width + 12.0,
            y: pill.y,
            width: (strip.x + strip.width - pill.x - pill.width - 28.0).max(0.0),
            height: pill.height,
        },
        &facts,
        theme.status_text,
    );
    if review.decided.is_none() {
        let [accept, reject] = review_buttons(atlas, strip);
        layout::push_rounded_rect(out, accept, 6.0, [tone[0], tone[1], tone[2], 0.22]);
        layout::push_ui_text_centered(out, atlas, accept, BUTTONS[0], theme.text);
        layout::push_rounded_rect(out, reject, 6.0, theme.tab_hover);
        layout::push_ui_text_centered(out, atlas, reject, BUTTONS[1], theme.status_text);
    }
    layout::push_rect(
        out,
        atlas,
        [strip.x, strip.y + strip.height - 1.0],
        [strip.width, 1.0],
        theme.hairline,
    );
}

/// The proposal as a diff against the file.
pub fn draw_review(
    review: &Review,
    atlas: &mut Atlas,
    rect: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    draw_diff_lines(
        &review.diff.lines,
        review.scroll,
        "No change: the proposal matches the file",
        &|_| None,
        atlas,
        rect,
        theme,
        out,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrolling_stops_with_the_last_line_in_view() {
        let old: String = (0..100).map(|n| format!("{n}\n")).collect();
        let new: String = (0..100).map(|n| format!("x{n}\n")).collect();
        let mut review = Review {
            request: Value::Null,
            tab_name: "t".into(),
            path: "/p/a".into(),
            diff: crate::ide::diff::diff(&old, &new),
            proposed: new,
            decided: None,
            scroll: 0,
        };
        let rect = Viewport {
            x: 0.0,
            y: 0.0,
            width: 400.0,
            height: DIFF_LINE * 10.0,
        };
        review.scroll_by(-5, rect);
        assert_eq!(review.scroll, 0);
        review.scroll_by(1000, rect);
        assert_eq!(review.scroll, review.diff.lines.len() - 10);
    }
}
