//! The Review page: what agents in crc's terminals changed, by session, with
//! each file's changes against the copy kept before the agent's first edit.
//! Keep moves a change into that copy, so it leaves the review; Undo takes
//! it back out of the file. Opened from Home, the palette, or the count on
//! a terminal tab; it has the editor column the way Settings does.
//!
//! It also shows a worktree's branch against the default branch, read
//! only: what the branch changes since it left, uncommitted work included.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use crate::platform::git_panel::{DIFF_LINE, draw_diff_lines};
use crate::platform::mcp_page::{button, link, text};
use crate::project::git::{self, Changed, Diff, DiffKind};
use crate::project::review;
use crate::render::{
    font::Atlas,
    layout::{self, Theme, Viewport},
    metal::GlyphInstance,
};

const PAD: f32 = 28.0;
/// The session and file list on the left.
const LIST: f32 = 300.0;
const ROW: f32 = 44.0;
const SESSION_ROW: f32 = 54.0;
/// A branch review shows this many files at most.
const BRANCH_FILES: usize = 500;

/// What a click on the page does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Close,
    /// Show this session's file.
    Select(usize, usize),
    /// Keep or undo one change in the file shown.
    KeepHunk(usize),
    UndoHunk(usize),
    /// Every change in the file shown.
    KeepFile,
    UndoFile,
    /// Every file in a session.
    KeepSession(usize),
    UndoSession(usize),
    /// The file shown, in the editor.
    OpenFile,
}

/// A file under review, with its diff as last read.
pub struct Entry {
    pub file: review::File,
    pub diff: Diff,
    /// The fingerprint of the text the diff was made from.
    pub seen: u64,
    /// Why the file can only be kept or undone whole, or, on a branch,
    /// why its changes are not shown.
    pub whole_only: Option<String>,
    /// On a branch, what Git says changed; `None` for an agent's file.
    pub change: Option<Changed>,
    /// What the list says instead of the line counts: `new`, `deleted`,
    /// `binary` and the like.
    pub tag: Option<&'static str>,
}

/// What a session's files are compared with.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// The copies kept before an agent's edits: Keep and Undo work.
    #[default]
    Checkpoints,
    /// Where the branch left `base`: read only. `left_out` files past
    /// the most the page shows.
    Branch { base: String, left_out: usize },
}

pub struct Session {
    pub dir: PathBuf,
    pub title: String,
    /// Where the terminal ran; paths under it are shown relative to it.
    pub folder: PathBuf,
    pub files: Vec<Entry>,
    pub source: Source,
}

impl Session {
    /// A branch under review: nothing to keep or undo.
    pub fn read_only(&self) -> bool {
        matches!(self.source, Source::Branch { .. })
    }
}

/// A branch being read on a worker: which, and the answer to come.
pub struct Loading {
    pub title: String,
    pub base: String,
    pub rx: mpsc::Receiver<Result<Session, String>>,
}

#[derive(Default)]
pub struct Page {
    pub open: bool,
    pub sessions: Vec<Session>,
    /// Session and file shown.
    pub selected: (usize, usize),
    /// The session folders the window's terminals are writing to now.
    pub running: Vec<PathBuf>,
    /// Each session with something to review, its title and file count,
    /// for Home; kept current while the page is closed.
    pub summary: Vec<(PathBuf, String, usize)>,
    /// The summary being read again on a worker, and whether more changed
    /// since that read started, so one more follows it.
    pub counting: Option<mpsc::Receiver<Vec<(PathBuf, String, usize)>>>,
    pub recount: bool,
    /// A destructive action clicked once, waiting for its second click.
    pub confirm: Option<Action>,
    pub hits: Vec<(Viewport, Action)>,
    /// The diff's first line on screen, and where it was drawn.
    pub diff_scroll: usize,
    pub diff_rect: Option<Viewport>,
    /// The list's scroll, in points, and where it was drawn.
    pub list_scroll: f32,
    pub list_rect: Option<Viewport>,
    pub list_content: f32,
    /// A branch being read for the page.
    pub loading: Option<Loading>,
}

impl Page {
    /// Reads every session folder again, keeping the file shown when it is
    /// still there. A branch review, or one being read, stays as it is:
    /// it is read once, on a worker, when it is asked for.
    pub fn reload(&mut self) {
        if self.loading.is_some() || self.sessions.first().is_some_and(Session::read_only) {
            let files = self.sessions.first().map_or(0, |s| s.files.len());
            if self.selected.1 >= files {
                self.selected = (0, 0);
            }
            let lines = self.selected_entry().map_or(0, |e| e.diff.lines.len());
            self.diff_scroll = self.diff_scroll.min(lines.saturating_sub(1));
            return;
        }
        let shown = self.selected_entry().map(|e| e.file.path.clone());
        self.sessions = load();
        self.selected = shown
            .and_then(|path| {
                self.sessions.iter().enumerate().find_map(|(s, session)| {
                    session
                        .files
                        .iter()
                        .position(|e| e.file.path == path)
                        .map(|f| (s, f))
                })
            })
            .unwrap_or((0, 0));
        let lines = self.selected_entry().map_or(0, |e| e.diff.lines.len());
        self.diff_scroll = self.diff_scroll.min(lines.saturating_sub(1));
    }

    pub fn selected_entry(&self) -> Option<&Entry> {
        self.sessions
            .get(self.selected.0)
            .and_then(|s| s.files.get(self.selected.1))
    }

    pub fn file_count(&self) -> usize {
        self.sessions.iter().map(|s| s.files.len()).sum()
    }

    /// Wheel lines over the list or the diff.
    pub fn scroll_at(&mut self, x: f32, y: f32, lines: isize) {
        if self.diff_rect.is_some_and(|r| r.contains(x, y)) {
            let max = self
                .selected_entry()
                .map_or(0, |e| e.diff.lines.len().saturating_sub(1));
            self.diff_scroll = self.diff_scroll.saturating_add_signed(lines).min(max);
        } else if self.list_rect.is_some_and(|r| r.contains(x, y)) {
            let visible = self.list_rect.map_or(0.0, |r| r.height);
            let max = (self.list_content - visible).max(0.0);
            self.list_scroll = (self.list_scroll + lines as f32 * 26.0).clamp(0.0, max);
        }
    }

    pub fn over(&self, x: f32, y: f32) -> bool {
        self.diff_rect.is_some_and(|r| r.contains(x, y))
            || self.list_rect.is_some_and(|r| r.contains(x, y))
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<Action> {
        self.hits
            .iter()
            .filter(|_| self.open)
            .find(|(rect, _)| rect.contains(x, y))
            .map(|(_, action)| *action)
    }

    /// A region for scripts: `review_page.close`, `review_page.select.S.F`,
    /// `review_page.keep.N`, `review_page.undo.N`, `review_page.keep_file`,
    /// `review_page.undo_file`, `review_page.keep_session.S`,
    /// `review_page.undo_session.S`.
    pub fn named(&self, name: &str) -> Option<Viewport> {
        self.hits
            .iter()
            .filter(|_| self.open)
            .find(|(_, action)| name_of(*action) == name)
            .map(|(rect, _)| *rect)
    }

    /// For the self-test: whether it has the column, what it holds, the
    /// file shown, and how many targets it drew; for a branch, its base,
    /// and whether it is still being read.
    pub fn report(&self) -> String {
        let shown = self
            .selected_entry()
            .and_then(|e| e.file.path.file_name())
            .map_or_else(|| "none".into(), |n| n.to_string_lossy().into_owned());
        format!(
            "{} sessions={} files={} hunks={} shown={shown} confirm={} targets={}",
            if self.open { "open" } else { "closed" },
            self.sessions.len(),
            self.file_count(),
            self.selected_entry().map_or(0, |e| e.diff.hunks.len()),
            self.confirm.is_some(),
            self.hits.len()
        ) + &self
            .sessions
            .first()
            .and_then(|s| match &s.source {
                Source::Branch { base, .. } => Some(format!(" base={base}")),
                Source::Checkpoints => None,
            })
            .unwrap_or_default()
            + if self.loading.is_some() {
                " loading"
            } else {
                ""
            }
    }
}

fn name_of(action: Action) -> String {
    match action {
        Action::Close => "review_page.close".into(),
        Action::Select(s, f) => format!("review_page.select.{s}.{f}"),
        Action::KeepHunk(n) => format!("review_page.keep.{n}"),
        Action::UndoHunk(n) => format!("review_page.undo.{n}"),
        Action::KeepFile => "review_page.keep_file".into(),
        Action::UndoFile => "review_page.undo_file".into(),
        Action::KeepSession(s) => format!("review_page.keep_session.{s}"),
        Action::UndoSession(s) => format!("review_page.undo_session.{s}"),
        Action::OpenFile => "review_page.open_file".into(),
    }
}

/// Every session with something to review, newest first.
fn load() -> Vec<Session> {
    let Some(root) = review::root() else {
        return Vec::new();
    };
    let mut sessions: Vec<(std::time::SystemTime, Session)> = review::sessions(&root)
        .into_iter()
        .filter_map(|dir| {
            let files: Vec<Entry> = review::files(&dir).into_iter().map(entry).collect();
            if files.is_empty() {
                return None;
            }
            let (title, folder) = review::about(&dir);
            let started = std::fs::metadata(&dir)
                .and_then(|m| m.created())
                .unwrap_or(std::time::UNIX_EPOCH);
            Some((
                started,
                Session {
                    dir,
                    title,
                    folder,
                    files,
                    source: Source::Checkpoints,
                },
            ))
        })
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.0));
    sessions.into_iter().map(|(_, s)| s).collect()
}

fn entry(file: review::File) -> Entry {
    match review::texts(&file) {
        Ok((old, new)) => Entry {
            diff: crate::ide::diff::diff(&old, &new),
            seen: review::fingerprint(&new),
            whole_only: None,
            change: None,
            tag: None,
            file,
        },
        Err(why) => Entry {
            diff: Diff::default(),
            seen: 0,
            whole_only: Some(why),
            change: None,
            tag: None,
            file,
        },
    }
}

/// What the branch checked out in `tree` changes since it left `base`,
/// uncommitted work and new files included, as a read-only session
/// titled `title`. Asks Git about every file: run it on a worker.
pub fn load_branch(tree: &Path, base: &str, title: &str) -> Result<Session, String> {
    let since = git::merge_base(tree, base)?;
    let changed = git::changed_since(tree, &since)?;
    let left_out = changed.len().saturating_sub(BRANCH_FILES);
    let files = changed
        .into_iter()
        .take(BRANCH_FILES)
        .map(|change| branch_entry(tree, &since, change))
        .collect();
    Ok(Session {
        dir: tree.to_path_buf(),
        title: title.to_owned(),
        folder: tree.to_path_buf(),
        files,
        source: Source::Branch {
            base: base.to_owned(),
            left_out,
        },
    })
}

/// One changed file on a branch: its text at `since` against the file in
/// the worktree now.
fn branch_entry(tree: &Path, since: &str, change: Changed) -> Entry {
    let path = tree.join(&change.path);
    let read = || -> Result<(String, String), (&'static str, String)> {
        if change.kind != 'D' && path.is_dir() {
            return Err((
                "submodule",
                "A submodule: its changes are its own repository's.".into(),
            ));
        }
        let large = || {
            (
                "large",
                "Too large to show by change. Open the file to see it.".to_owned(),
            )
        };
        let old = match change.kind {
            'A' | '?' => Vec::new(),
            _ => {
                let from = change.from.as_deref().unwrap_or(&change.path);
                git::rev_bytes(tree, since, from)
                    .map_err(|e| ("error", e))?
                    .unwrap_or_default()
            }
        };
        let new = if change.kind == 'D' {
            Vec::new()
        } else {
            if std::fs::metadata(&path).is_ok_and(|m| m.len() > review::MAX_CHECKPOINT) {
                return Err(large());
            }
            std::fs::read(&path).map_err(|e| ("error", e.to_string()))?
        };
        if old.len() as u64 > review::MAX_CHECKPOINT {
            return Err(large());
        }
        let text = |bytes: Vec<u8>| {
            if bytes.contains(&0) {
                return None;
            }
            String::from_utf8(bytes).ok()
        };
        match (text(old), text(new)) {
            (Some(old), Some(new)) => Ok((old, new)),
            _ => Err(("binary", "Binary, so not shown.".into())),
        }
    };
    let read = read();
    let file = review::File {
        path,
        // Never acted on: a branch review is read only.
        checkpoint: review::Checkpoint::Unhooked(None),
    };
    match read {
        Ok((old, new)) => Entry {
            diff: crate::ide::diff::diff(&old, &new),
            seen: 0,
            whole_only: None,
            tag: match change.kind {
                'A' | '?' => Some("new"),
                'D' => Some("deleted"),
                _ => None,
            },
            change: Some(change),
            file,
        },
        Err((tag, why)) => Entry {
            diff: Diff::default(),
            seen: 0,
            whole_only: Some(why),
            tag: Some(tag),
            change: Some(change),
            file,
        },
    }
}

/// A path as the list shows it: the name, and the folder it is in.
fn split(path: &Path) -> (String, String) {
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let folder = path
        .parent()
        .and_then(|p| p.file_name())
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    (name, folder)
}

/// The editor column: header, then the list of sessions and files on the
/// left and the shown file's changes on the right.
pub fn draw(
    page: &mut Page,
    atlas: &mut Atlas,
    rect: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    let mut hits = Vec::new();
    layout::push_rect(
        out,
        atlas,
        [rect.x, rect.y],
        [rect.width, rect.height],
        theme.tab_active,
    );
    page.diff_rect = None;
    page.list_rect = None;
    if rect.width < 480.0 || rect.height < 200.0 {
        page.hits = hits;
        return;
    }
    let dim = theme.status_text;
    let x = rect.x + PAD;
    let right = rect.x + rect.width - PAD;
    let bottom = rect.y + rect.height;
    let mut y = rect.y + 22.0;

    let title_w = layout::push_page_title(out, atlas, x, y - 4.0, "Review", theme.text, 200.0);
    let close_w = layout::ui_text_width(atlas, "Close") + 28.0;
    if right - close_w > x + title_w + 24.0 {
        let r = button(out, atlas, theme, right - close_w, y - 4.0, "Close", false);
        hits.push((r, Action::Close));
    }
    y += 26.0;
    let files = page.file_count();
    let branch = page.sessions.first().and_then(|s| match &s.source {
        Source::Branch { base, left_out } => Some((s.title.as_str(), base.as_str(), *left_out)),
        Source::Checkpoints => None,
    });
    let status = if let Some(loading) = &page.loading {
        format!("Reading {} against {}\u{2026}", loading.title, loading.base)
    } else if let Some((title, base, left_out)) = branch {
        let mut said = match files {
            0 => format!("Nothing on {title} differs from {base}"),
            1 => format!("1 file changed on {title} since {base}"),
            n => format!("{n} files changed on {title} since {base}"),
        };
        if left_out > 0 {
            said.push_str(&format!("; {left_out} more not shown"));
        }
        said
    } else {
        match (page.sessions.len(), files) {
            (_, 0) => {
                "Nothing to review. Changes an agent makes in a crc terminal show here.".to_owned()
            }
            (1, 1) => "1 file from 1 session".to_owned(),
            (1, n) => format!("{n} files from 1 session"),
            (s, n) => format!("{n} files from {s} sessions"),
        }
    };
    text(out, atlas, x, y, right - x, &status, dim);
    y += 28.0;
    layout::push_rect(out, atlas, [x, y], [right - x, 1.0], theme.hairline);
    y += 8.0;
    let top = y;
    if page.loading.is_some() {
        // The list and the diff as they will stand, breathing.
        let mut ly = top + 4.0;
        for (n, width) in [0.7, 0.55, 0.62, 0.48].into_iter().enumerate() {
            let height = if n == 0 {
                SESSION_ROW - 10.0
            } else {
                ROW - 4.0
            };
            layout::push_skeleton_row(
                out,
                Viewport {
                    x: x + 8.0,
                    y: ly,
                    width: LIST - 16.0,
                    height,
                },
                0.0,
                (width, width * 0.6),
                true,
                theme,
            );
            ly += height + 6.0;
        }
        layout::push_rect(
            out,
            atlas,
            [x + LIST + 12.0, top],
            [1.0, bottom - top],
            theme.hairline,
        );
        let dx = x + LIST + 24.0;
        for (n, width) in [0.62, 0.4, 0.75, 0.3, 0.55, 0.68, 0.36, 0.5]
            .into_iter()
            .enumerate()
        {
            layout::push_skeleton_row(
                out,
                Viewport {
                    x: dx,
                    y: top + 6.0 + n as f32 * DIFF_LINE,
                    width: (right - dx).max(0.0),
                    height: DIFF_LINE,
                },
                0.0,
                (width, 0.0),
                false,
                theme,
            );
        }
        page.hits = hits;
        return;
    }
    if files == 0 {
        page.hits = hits;
        return;
    }

    // The list, scrolled and clipped to its own column.
    let list = Viewport {
        x,
        y: top,
        width: LIST,
        height: (bottom - top).max(0.0),
    };
    page.list_rect = Some(list);
    let list_start = out.len();
    let mut list_hits = Vec::new();
    let mut ly = top - page.list_scroll;
    for (s, session) in page.sessions.iter().enumerate() {
        let running = page.running.contains(&session.dir);
        let label = if running {
            format!("{} \u{b7} running", session.title)
        } else {
            session.title.clone()
        };
        text(
            out,
            atlas,
            x + 4.0,
            ly + 4.0,
            LIST - 8.0,
            &label,
            theme.text,
        );
        let place = match &session.source {
            Source::Branch { base, .. } => format!("since {base}"),
            Source::Checkpoints => session
                .folder
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
        };
        let counts = format!(
            "{place} \u{b7} {} file{}",
            session.files.len(),
            if session.files.len() == 1 { "" } else { "s" }
        );
        if session.read_only() {
            // Nothing to keep or undo: the line has the whole width.
            text(out, atlas, x + 4.0, ly + 24.0, LIST - 8.0, &counts, dim);
        } else {
            text(out, atlas, x + 4.0, ly + 24.0, LIST * 0.5, &counts, dim);
            let undo_label = if page.confirm == Some(Action::UndoSession(s)) {
                "Confirm undo all"
            } else {
                "Undo all"
            };
            let undo_w = layout::ui_text_width(atlas, undo_label) + 12.0;
            let undo = link(out, atlas, theme, x + LIST - undo_w, ly + 20.0, undo_label);
            let keep_w = layout::ui_text_width(atlas, "Keep all") + 12.0;
            let keep = link(out, atlas, theme, undo.x - keep_w, ly + 20.0, "Keep all");
            list_hits.push((keep, Action::KeepSession(s)));
            list_hits.push((undo, Action::UndoSession(s)));
        }
        ly += SESSION_ROW;
        for (f, e) in session.files.iter().enumerate() {
            let row = Viewport {
                x,
                y: ly,
                width: LIST,
                height: ROW - 4.0,
            };
            if page.selected == (s, f) {
                layout::push_rounded_rect(out, row, layout::UI_RADIUS, theme.sidebar_selected);
            } else if layout::hovered(row) {
                layout::push_rounded_rect(out, row, layout::UI_RADIUS, theme.row_hover);
            }
            let (name, folder) = split(&e.file.path);
            text(
                out,
                atlas,
                x + 12.0,
                ly + 2.0,
                LIST - 90.0,
                &name,
                theme.text,
            );
            let folder = match &e.change {
                Some(Changed {
                    from: Some(from), ..
                }) => format!("renamed from {}", split(from).0),
                // What Git says of it, and the folder under the worktree.
                Some(change) => {
                    let what = match change.kind {
                        'A' => "added",
                        'D' => "deleted",
                        '?' => "not committed",
                        'T' => "type changed",
                        _ => "modified",
                    };
                    match change.path.parent().filter(|p| !p.as_os_str().is_empty()) {
                        Some(dir) => format!("{what} \u{b7} {}", dir.display()),
                        None => what.to_owned(),
                    }
                }
                None if e.file.checkpoint.undoable() => folder,
                None => format!("by a command \u{b7} {folder}"),
            };
            text(out, atlas, x + 12.0, ly + 20.0, LIST - 24.0, &folder, dim);
            let counts = match (&e.tag, &e.file.checkpoint) {
                (Some(tag), _) => (*tag).to_owned(),
                (None, review::Checkpoint::Absent) => "new".to_owned(),
                _ if e.whole_only.is_some() => "whole".to_owned(),
                _ => format!("+{} \u{2212}{}", e.diff.added, e.diff.removed),
            };
            layout::push_ui_text_right(
                out,
                atlas,
                Viewport {
                    x: x + LIST - 84.0,
                    y: ly + 2.0,
                    width: 74.0,
                    height: 20.0,
                },
                &counts,
                dim,
            );
            list_hits.push((row, Action::Select(s, f)));
            ly += ROW;
        }
        ly += 10.0;
    }
    page.list_content = ly + page.list_scroll - top;
    for quad in &mut out[list_start..] {
        layout::clip_vertical(quad, top, bottom);
    }
    hits.extend(
        list_hits
            .into_iter()
            .filter(|(r, _)| r.y >= top && r.y + r.height <= bottom),
    );
    layout::push_rect(
        out,
        atlas,
        [x + LIST + 12.0, top],
        [1.0, bottom - top],
        theme.hairline,
    );

    // The file shown: its path and whole-file actions, then the diff.
    let dx = x + LIST + 24.0;
    let dw = right - dx;
    let Some(e) = page.selected_entry() else {
        page.hits = hits;
        return;
    };
    let mut fy = top + 4.0;
    let folder = &page.sessions[page.selected.0].folder;
    let path = e
        .file
        .path
        .strip_prefix(folder)
        .ok()
        .filter(|_| !folder.as_os_str().is_empty())
        .unwrap_or(&e.file.path)
        .to_string_lossy();
    let read_only = page.sessions[page.selected.0].read_only();
    let undoable = e.file.checkpoint.undoable() && !read_only;
    let buttons_x = if read_only {
        // Read only: the file in the editor, unless the branch deleted it.
        if e.change.as_ref().is_some_and(|c| c.kind != 'D') {
            let open_w = layout::ui_text_width(atlas, "Open file") + 28.0;
            let open = button(out, atlas, theme, right - open_w, fy, "Open file", true);
            hits.push((open, Action::OpenFile));
            open.x
        } else {
            right
        }
    } else {
        let undo_label = if page.confirm == Some(Action::UndoFile) {
            "Confirm undo"
        } else {
            "Undo file"
        };
        let keep_w = layout::ui_text_width(atlas, "Keep file") + 28.0;
        let keep_x = if undoable {
            let undo_w = layout::ui_text_width(atlas, undo_label) + 28.0;
            let undo = button(out, atlas, theme, right - undo_w, fy, undo_label, false);
            hits.push((undo, Action::UndoFile));
            undo.x - 8.0 - keep_w
        } else {
            right - keep_w
        };
        let keep = button(out, atlas, theme, keep_x, fy, "Keep file", true);
        hits.push((keep, Action::KeepFile));
        keep.x
    };
    text(
        out,
        atlas,
        dx,
        fy + 2.0,
        buttons_x - dx - 12.0,
        &path,
        theme.text,
    );
    fy += 34.0;
    if !undoable && !read_only && e.whole_only.is_none() {
        // Said once, above the diff: what this file is, and why only Keep.
        let note = match &e.file.checkpoint {
            review::Checkpoint::Unhooked(Some(_)) => {
                "Changed by a command while this session was working, not through the agent's file tools. No copy from before was kept, so it can only be kept. Compared with Git's staged copy."
            }
            _ => {
                "Created by a command while this session was working, not through the agent's file tools. No copy from before was kept, so it can only be kept."
            }
        };
        for line in layout::wrap_words(atlas, note, dw) {
            text(out, atlas, dx, fy, dw, &line, dim);
            fy += 20.0;
        }
        fy += 8.0;
    }
    if let Some(why) = &e.whole_only {
        text(out, atlas, dx, fy, dw, why, dim);
        page.hits = hits;
        return;
    }
    let diff_rect = Viewport {
        x: dx,
        y: fy,
        width: dw,
        height: (bottom - fy - 8.0).max(0.0),
    };
    draw_diff_lines(
        &e.diff.lines,
        page.diff_scroll,
        if read_only {
            "No changes to its text."
        } else {
            "No changes left in this file."
        },
        &|_| None,
        None,
        atlas,
        diff_rect,
        theme,
        out,
    );
    // Keep and Undo on each hunk's rule; a file with no copy is kept whole.
    let visible = if undoable {
        (diff_rect.height / DIFF_LINE) as usize
    } else {
        0
    };
    for (row, line) in e
        .diff
        .lines
        .iter()
        .skip(page.diff_scroll)
        .take(visible)
        .enumerate()
    {
        let (DiffKind::Hunk, Some(n)) = (line.kind, line.hunk) else {
            continue;
        };
        let ry = diff_rect.y + row as f32 * DIFF_LINE + (DIFF_LINE - layout::UI_CONTROL) * 0.5;
        let undo_w = layout::ui_text_width(atlas, "Undo") + 12.0;
        let keep_w = layout::ui_text_width(atlas, "Keep") + 12.0;
        let ux = right - undo_w;
        let kx = ux - 8.0 - keep_w;
        layout::push_rect(
            out,
            atlas,
            [kx - 6.0, diff_rect.y + row as f32 * DIFF_LINE],
            [right - kx + 6.0, DIFF_LINE],
            theme.tab_active,
        );
        let keep = link(out, atlas, theme, kx + 6.0, ry, "Keep");
        let undo = link(out, atlas, theme, ux + 6.0, ry, "Undo");
        hits.push((keep, Action::KeepHunk(n)));
        hits.push((undo, Action::UndoHunk(n)));
    }
    page.diff_rect = Some(diff_rect);
    page.hits = hits;
}
