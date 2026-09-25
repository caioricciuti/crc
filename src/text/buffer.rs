//! An editable buffer: a rope, a selection, undo history, and a scroll
//! position.
//!
//! Positions are byte offsets into the rope, always on character boundaries.
//! Byte offsets rather than (line, column) pairs because that is what the
//! rope indexes natively and what a syntax tree will want later; line and
//! column are derived when something needs to display them.
//!
//! Undo is a stack of whole-buffer snapshots. That sounds extravagant until
//! you remember [`Rope::clone`] is O(1) and shares structure, so a snapshot
//! costs one pointer bump and a refcount. It is the payoff for having written
//! the rope as a persistent tree.

use std::io::{Read, Seek};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, Instant};

use crate::text::file_format::{self, DiskFormat};
use crate::text::rope::Rope;
use crate::text::wrap;

/// Fold All reads every line; past this many it is refused.
pub const FOLD_ALL_MAX_LINES: usize = 100_000;

/// Consecutive edits closer together than this fold into one undo step, so
/// undo steps back over a word rather than a character.
const COALESCE_WINDOW: Duration = Duration::from_millis(600);

/// True for a UTF-8 continuation byte, the trailing bytes of a multi-byte
/// character.
fn is_continuation(b: u8) -> bool {
    b & 0xC0 == 0x80
}

/// Copies mode, owner, ACLs and extended attributes onto a replacement file.
fn copy_metadata(source: &std::path::Path, destination: &std::fs::File) -> std::io::Result<()> {
    unsafe extern "C" {
        fn fcopyfile(
            from: std::os::raw::c_int,
            to: std::os::raw::c_int,
            state: *mut std::ffi::c_void,
            flags: u32,
        ) -> std::os::raw::c_int;
    }
    let original = std::fs::File::open(source)?;
    // COPYFILE_METADATA = COPYFILE_STAT | COPYFILE_ACL | COPYFILE_XATTR.
    let rc = unsafe {
        fcopyfile(
            original.as_raw_fd(),
            destination.as_raw_fd(),
            std::ptr::null_mut(),
            0b111,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn matches_disk(
    path: &std::path::Path,
    expected: &Rope,
    format: &DiskFormat,
) -> std::io::Result<bool> {
    let mut file = std::fs::File::open(path)?;
    let mut encoded = Vec::new();
    file_format::write(&mut encoded, expected, format)?;
    let mut bytes = Vec::new();
    for chunk in encoded.chunks(64 * 1024) {
        bytes.resize(chunk.len(), 0);
        match file.read_exact(&mut bytes) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
            Err(e) => return Err(e),
        }
        if bytes != chunk {
            return Ok(false);
        }
    }
    Ok(file.read(&mut [0u8; 1])? == 0)
}

/// What `stat` said about the file the last time the buffer read or wrote it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DiskStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

impl DiskStamp {
    fn of(path: &std::path::Path) -> std::io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        Ok(DiskStamp {
            len: meta.len(),
            modified: meta.modified().ok(),
        })
    }
}

/// Files past this open read-only, with the status line saying why.
pub const READ_ONLY_BYTES: u64 = 512 * 1024 * 1024;
/// Files past this are not opened at all.
pub const MAX_OPEN_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// `512 MB`, `2.0 GB`: sizes as the status line and alerts say them.
pub fn human_size(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    if bytes >= 1024 * MB {
        format!("{:.1} GB", bytes as f64 / (1024 * MB) as f64)
    } else {
        format!("{} MB", bytes.div_ceil(MB))
    }
}

/// The file behind a buffer, compared with what the buffer last saw of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskState {
    /// Same size and modification time as at the last read or write, or the
    /// buffer has no file to compare against.
    Unchanged,
    /// Something else wrote the file since the buffer last read or wrote it.
    Changed,
    /// The file is gone, or is no longer a regular file.
    Missing,
}

/// How a character counts for word motions.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Word,
    Punctuation,
    Whitespace,
}

/// Underscores and digits count as word characters, so `foo_bar2` is one
/// word rather than three. Identifiers are what people navigate in code.
fn class_of(c: char) -> CharClass {
    if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else if c.is_whitespace() {
        CharClass::Whitespace
    } else {
        CharClass::Punctuation
    }
}

/// Whether a movement collapses the selection or extends it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    /// Plain movement: the selection collapses to the new position.
    Move,
    /// Shift-movement: the anchor stays put and the selection grows.
    Extend,
}

/// What produced the most recent edit, for undo coalescing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditKind {
    Insert,
    Delete,
}

/// A single change to the text, in the shape an incremental parser needs.
///
/// Byte offsets alone are not enough: tree-sitter also wants row/column for
/// each end, and the *old* end has to be measured before the edit lands while
/// the *new* end can only be measured after. Recording them here, at the
/// point of the edit, is the only place both are available.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edit {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_end_byte: usize,
    /// (row, byte column within the row), matching tree-sitter's TSPoint.
    pub start_point: (usize, usize),
    pub old_end_point: (usize, usize),
    pub new_end_point: (usize, usize),
}

/// A restorable point in the buffer's history.
#[derive(Clone)]
struct Snapshot {
    rope: Rope,
    cursor: usize,
    anchor: usize,
    /// The extra cursors belong to the text they were placed in. Restoring a
    /// rope without them leaves offsets that point past the end of it.
    extra: Vec<(usize, usize)>,
}

/// What a bare cursor consumes around itself in a multi-cursor edit. A cursor
/// with a selection always consumes exactly its selection.
#[derive(Clone, Copy)]
enum Reach {
    Nothing,
    /// The character before it: backspace.
    Back,
    /// The characters either side of it: backspace inside an empty pair.
    Both,
}

pub struct Buffer {
    pub rope: Rope,
    /// The moving end of the selection, and where text is inserted.
    cursor: usize,
    /// The fixed end. Equal to `cursor` when there is no selection.
    anchor: usize,
    /// Additional cursors as (anchor, head) pairs, beyond the primary one.
    ///
    /// Kept as a side list rather than making the primary one element of a
    /// `Vec` so that the overwhelmingly common single-cursor path stays
    /// exactly as it was, with no allocation and no sorting. Multi-cursor
    /// editing is a branch taken only when this is non-empty.
    extra: Vec<(usize, usize)>,
    /// Opened too big to edit: every edit is refused, as for a preview.
    read_only: bool,
    /// First visible line.
    pub scroll_line: usize,
    /// How much of `scroll_line` is scrolled off the top, from 0 up to but
    /// not including 1. A trackpad moves the view in points, not lines.
    pub scroll_fraction: f32,
    /// First visible column, in characters. Long lines otherwise vanish off
    /// the right edge with no way to reach them. Always 0 while wrapping.
    pub scroll_column: usize,
    /// Soft wrap width in columns, or `None` to scroll long lines sideways.
    /// The window sets it from the text area's width before each frame.
    pub wrap: Option<usize>,
    /// Rows of `scroll_line` above the view, while wrapping.
    pub scroll_row: usize,
    /// View > Word Wrap for this document: `None` follows the setting.
    pub wrap_choice: Option<bool>,
    /// Folded lines: inclusive ranges of hidden lines, sorted, apart.
    pub folds: Vec<(usize, usize)>,
    /// How this document indents, from `.editorconfig` or its own text.
    /// `None` when neither says: the surrounding line decides.
    pub indent_style: Option<crate::text::indent::Style>,
    /// Column the cursor tries to return to during vertical movement,
    /// measured in characters. Without this, moving down through a short line
    /// would permanently lose the original column.
    goal_column: Option<usize>,

    undo_stack: Vec<Snapshot>,
    redo_stack: Vec<Snapshot>,
    last_edit: Option<(EditKind, Instant)>,

    /// Edits since the syntax layer last drained them. Kept so a re-parse
    /// can be incremental instead of starting from scratch.
    pending_edits: Vec<Edit>,
    /// Set when history is replaced wholesale (undo, redo, reload), where
    /// replaying edits is not possible and a full re-parse is required.
    edits_invalidated: bool,

    /// Where this buffer came from, if anywhere.
    pub path: Option<std::path::PathBuf>,
    /// A tab title for a document with no path: a request's response.
    /// Documents with a path are titled by their file name.
    pub label: Option<String>,
    /// The extension a pathless document should be treated as having, for
    /// highlighting and preview. `md` makes a response tab open rendered.
    pub display_ext: Option<&'static str>,
    /// Read-only file shown by the native Quick Look view.
    preview_file: bool,
    /// Whether there are unsaved changes.
    dirty: bool,
    /// Last contents read from or written to disk, shared with the rope.
    saved: Option<Rope>,
    /// Size and modification time of the file as of the last read or write,
    /// so "did anyone else touch it" is one `stat` and not a full compare.
    stamp: Option<DiskStamp>,
    /// The window has already told the user this tab disagrees with disk,
    /// so the next watcher batch does not say it again.
    pub conflict_noticed: bool,
    format: DiskFormat,
    /// Identity of this document for as long as it is open. Anything kept
    /// beside a buffer rather than inside it, a parse tree above all, is
    /// keyed by this: a tab index moves when a tab closes, and a path changes
    /// on Save As and is absent for an untitled buffer.
    id: u64,
}

/// Source of [`Buffer::id`]. Never reused within a run.
static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Formats macOS Quick Look can display without treating their bytes as text.
fn is_preview_path(path: &std::path::Path) -> bool {
    let Some(extension) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "pdf"
            | "svg"
            | "svgz"
            | "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "heic"
            | "heif"
            | "tif"
            | "tiff"
            | "bmp"
            | "ico"
            | "icns"
            | "avif"
            | "psd"
            | "mp3"
            | "m4a"
            | "wav"
            | "aiff"
            | "flac"
            | "mp4"
            | "m4v"
            | "mov"
            | "avi"
            | "webm"
            | "doc"
            | "docx"
            | "pages"
            | "xls"
            | "xlsx"
            | "numbers"
            | "ppt"
            | "pptx"
            | "key"
    )
}

impl Buffer {
    pub fn new() -> Self {
        Buffer::from_rope(Rope::new())
    }

    pub fn from_text(text: &str) -> Self {
        Buffer::from_rope(Rope::from_text(text))
    }

    /// A document that exists only in the editor: generated text under a
    /// title, treated as `ext` for highlighting. Clean until it is typed
    /// in, so closing it never asks about unsaved changes.
    pub fn generated(label: &str, ext: &'static str, text: &str) -> Self {
        let mut buffer = Buffer::from_text(text);
        buffer.label = Some(label.to_owned());
        buffer.display_ext = Some(ext);
        buffer
    }

    /// Replaces the whole text of a generated document with a new version,
    /// keeping the tab, its identity and the caret near where it was.
    pub fn regenerate(&mut self, text: &str) {
        self.rope = Rope::from_text(text);
        self.extra.clear();
        self.clamp_positions();
        // The whole rope was replaced: queued edits no longer describe the
        // way from the old text to the new, so the parser starts over.
        self.invalidate_edits();
        self.pending_edits.clear();
        self.last_edit = None;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.dirty = false;
    }

    /// The lowercase file extension, from the path or the display hint.
    pub fn extension(&self) -> Option<String> {
        self.path
            .as_deref()
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .or_else(|| self.display_ext.map(str::to_owned))
    }

    /// Loads a file, remembering the path so it can be saved back.
    ///
    /// Past [`READ_ONLY_BYTES`] it opens read-only; past [`MAX_OPEN_BYTES`]
    /// it is refused with `FileTooLarge` before a byte is read. The file is
    /// held about three times over while it loads (bytes, decoded text,
    /// rope), so the cap is what keeps a stray disk image from taking the
    /// machine's memory. `CRC_READ_ONLY_BYTES` lowers the first limit for
    /// the GUI self-test, which cannot write half a gigabyte per run.
    pub fn open(path: impl Into<std::path::PathBuf>) -> std::io::Result<Self> {
        let read_only_at = std::env::var("CRC_READ_ONLY_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(READ_ONLY_BYTES);
        Self::open_with_limits(path.into(), read_only_at, MAX_OPEN_BYTES)
    }

    fn open_with_limits(
        path: std::path::PathBuf,
        read_only_at: u64,
        max: u64,
    ) -> std::io::Result<Self> {
        if is_preview_path(&path) {
            std::fs::metadata(&path)?;
            return Ok(Self::preview_file(path));
        }
        let len = std::fs::metadata(&path)?.len();
        if len > max {
            let name = path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                format!(
                    "{name} is {}; crc opens files up to {}",
                    human_size(len),
                    human_size(max)
                ),
            ));
        }
        let raw = std::fs::read(&path)?;
        let (text, format) = match file_format::decode(&raw) {
            Ok(decoded) => decoded,
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                return Ok(Self::preview_file(path));
            }
            Err(error) => return Err(error),
        };
        let mut buffer = Buffer::from_text(&text);
        buffer.format = format;
        // Canonicalise, so the same file reached by two different routes is
        // the same document. Without it a symlink, a `..`, or a path from
        // the sidebar versus one from Cmd-P are different keys and open a
        // second tab onto identical text, which can then diverge.
        buffer.stamp = DiskStamp::of(&path).ok();
        buffer.path = Some(std::fs::canonicalize(&path).unwrap_or(path));
        buffer.saved = Some(buffer.rope.clone());
        buffer.read_only = len > read_only_at;
        if let Some(path) = &buffer.path {
            buffer.indent_style = crate::text::indent::for_file(path, &buffer.rope);
        }
        Ok(buffer)
    }

    /// Opened past [`READ_ONLY_BYTES`], so it cannot be edited.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Whether edits are refused: a preview has no text to edit, and a
    /// read-only file is too big to.
    fn is_locked(&self) -> bool {
        self.preview_file || self.read_only
    }

    /// Whether the file behind this buffer still matches the last read or
    /// write. One `stat`, so it is cheap enough to ask on every watcher batch
    /// and every time the window comes to the front.
    pub fn disk_state(&self) -> DiskState {
        let (Some(path), Some(stamp)) = (&self.path, self.stamp) else {
            return DiskState::Unchanged;
        };
        if self.preview_file {
            return DiskState::Unchanged;
        }
        match std::fs::metadata(path) {
            Ok(meta) if !meta.is_file() => DiskState::Missing,
            Ok(meta) => {
                let now = DiskStamp {
                    len: meta.len(),
                    modified: meta.modified().ok(),
                };
                if now == stamp {
                    DiskState::Unchanged
                } else {
                    DiskState::Changed
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => DiskState::Missing,
            Err(_) => DiskState::Unchanged,
        }
    }

    /// Re-reads the file behind the buffer, replacing the text.
    ///
    /// The previous text goes on the undo stack, so a reload that took
    /// something away is one Cmd-Z from coming back (as an unsaved change).
    /// The caret and scroll stay where they were, clamped to the new text.
    pub fn reload(&mut self) -> std::io::Result<()> {
        let Some(path) = self.path.clone() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "buffer has no path to reload from",
            ));
        };
        if self.preview_file {
            return Ok(());
        }
        let raw = std::fs::read(&path)?;
        let (text, format) = file_format::decode(&raw)?;
        let stamp = DiskStamp::of(&path).ok();
        let before = self.snapshot();
        self.rope = Rope::from_text(&text);
        self.format = format;
        self.extra.clear();
        self.clamp_positions();
        self.undo_stack.push(before);
        self.redo_stack.clear();
        self.invalidate_edits();
        self.pending_edits.clear();
        self.last_edit = None;
        self.goal_column = None;
        self.saved = Some(self.rope.clone());
        self.stamp = stamp;
        self.conflict_noticed = false;
        self.dirty = false;
        Ok(())
    }

    /// The file behind the buffer was deleted by something else. The text
    /// is now the only copy, which is what "unsaved" means: the tab gets its
    /// dot and closing asks first.
    pub fn note_missing_on_disk(&mut self) {
        if self.path.is_some() && !self.preview_file {
            self.dirty = true;
        }
    }

    fn preview_file(path: std::path::PathBuf) -> Self {
        let mut buffer = Buffer::new();
        buffer.path = Some(std::fs::canonicalize(&path).unwrap_or(path));
        buffer.preview_file = true;
        buffer.saved = Some(buffer.rope.clone());
        buffer
    }

    pub fn is_preview_file(&self) -> bool {
        self.preview_file
    }

    /// Rebuilds a document from text that outlived a crash.
    ///
    /// Dirty from the start, and the file at `path` is not read: what is on
    /// disk is the last save, and this is what came after it.
    pub fn recovered(path: Option<std::path::PathBuf>, text: &str) -> Self {
        let mut buffer = Buffer::from_text(text);
        if let Some((saved, format)) = path
            .as_deref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|raw| file_format::decode(&raw).ok())
        {
            buffer.saved = Some(Rope::from_text(&saved));
            buffer.format = format;
        }
        buffer.path = path;
        buffer.dirty = true;
        buffer
    }

    /// Writes back to `path`, or to the path it was opened from.
    pub fn save(&mut self, path: Option<&std::path::Path>) -> std::io::Result<()> {
        self.save_with(path, false)
    }

    /// Saves over whatever is on disk, for a user who has seen the conflict
    /// and chosen their copy. Everything else about the write is the same.
    pub fn save_overwriting(&mut self, path: Option<&std::path::Path>) -> std::io::Result<()> {
        self.save_with(path, true)
    }

    fn save_with(&mut self, path: Option<&std::path::Path>, force: bool) -> std::io::Result<()> {
        if self.preview_file {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "preview files are read-only",
            ));
        }
        let target = match (path, &self.path) {
            (Some(p), _) => p.to_path_buf(),
            (None, Some(p)) => p.clone(),
            (None, None) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "buffer has no path; save-as is required",
                ));
            }
        };
        // Resolve Save As symlinks too: replacing the link would leave its
        // destination untouched and turn this tab into a different file.
        let target = match std::fs::canonicalize(&target) {
            Ok(path) => path,
            Err(e)
                if std::fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) =>
            {
                return Err(e);
            }
            Err(_) => target,
        };
        let existing = match std::fs::metadata(&target) {
            Ok(meta) => Some(meta),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        if let Some(meta) = &existing {
            if !meta.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "target is not a regular file",
                ));
            }
            if !force && self.path.as_deref() == Some(target.as_path()) {
                let expected = self.saved.as_ref().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "cannot verify the previous disk contents",
                    )
                })?;
                if !matches_disk(&target, expected, &self.format)? {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "file changed on disk since it was opened",
                    ));
                }
            }
        } else if !force && self.saved.is_some() && self.path.as_deref() == Some(target.as_path()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "file was removed from disk",
            ));
        }

        file_format::validate(&self.rope, &self.format)?;
        if existing.as_ref().is_some_and(|m| m.nlink() > 1) {
            // Rename would silently detach the other names from this file.
            // Writing the same inode preserves its links, ACLs and xattrs.
            let mut file = std::fs::OpenOptions::new().write(true).open(&target)?;
            file.seek(std::io::SeekFrom::Start(0))?;
            file.set_len(0)?;
            file_format::write(&mut file, &self.rope, &self.format)?;
            file.sync_all()?;
        } else {
            let parent = target.parent().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "target has no parent directory",
                )
            })?;
            let mut tmp = None;
            for nonce in 0..1000 {
                let candidate = parent.join(format!(
                    ".crc-{}-{}-{nonce}.tmp",
                    std::process::id(),
                    self.id
                ));
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&candidate)
                {
                    Ok(file) => {
                        tmp = Some((candidate, file));
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                }
            }
            let (temp_path, mut file) = tmp.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "could not create a unique temporary file",
                )
            })?;
            let result: std::io::Result<()> = (|| {
                file_format::write(&mut file, &self.rope, &self.format)?;
                if existing.is_some() {
                    copy_metadata(&target, &file)?;
                    // COPYFILE_STAT also copies the old modification time.
                    file.set_times(
                        std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()),
                    )?;
                }
                file.sync_all()?;
                std::fs::rename(&temp_path, &target)?;
                std::fs::File::open(parent)?.sync_all()?;
                Ok(())
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&temp_path);
            }
            result?;
        }
        self.stamp = DiskStamp::of(&target).ok();
        self.conflict_noticed = false;
        self.path = Some(std::fs::canonicalize(&target).unwrap_or(target));
        self.saved = Some(self.rope.clone());
        self.dirty = false;
        Ok(())
    }

    fn from_rope(rope: Rope) -> Self {
        Buffer {
            rope,
            cursor: 0,
            anchor: 0,
            extra: Vec::new(),
            read_only: false,
            scroll_line: 0,
            scroll_fraction: 0.0,
            scroll_column: 0,
            wrap: None,
            scroll_row: 0,
            wrap_choice: None,
            folds: Vec::new(),
            indent_style: None,
            goal_column: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            last_edit: None,
            pending_edits: Vec::new(),
            edits_invalidated: false,
            path: None,
            label: None,
            display_ext: None,
            preview_file: false,
            dirty: false,
            saved: None,
            stamp: None,
            conflict_noticed: false,
            format: DiskFormat::default(),
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn disk_format(&self) -> &DiskFormat {
        &self.format
    }

    /// The selected byte range, or `None` when the selection is empty.
    pub fn selection(&self) -> Option<std::ops::Range<usize>> {
        if self.anchor == self.cursor {
            None
        } else {
            Some(self.anchor.min(self.cursor)..self.anchor.max(self.cursor))
        }
    }

    pub fn selected_text(&self) -> Option<String> {
        self.selection().map(|r| self.rope.slice_to_string(r))
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.cursor = self.rope.len_bytes();
        self.goal_column = None;
    }

    /// Cursor position as (line, column in characters).
    pub fn cursor_position(&self) -> (usize, usize) {
        self.position_of(self.cursor)
    }

    pub fn position_of(&self, byte: usize) -> (usize, usize) {
        let line = self.rope.byte_to_line(byte);
        let line_start = self.rope.line_to_byte(line);
        let column = self.rope.byte_to_char(byte) - self.rope.byte_to_char(line_start);
        (line, column)
    }

    /// Byte offset for a (line, column) pair, clamped into the buffer. Used
    /// by mouse hit-testing.
    pub fn offset_at(&self, line: usize, column: usize) -> usize {
        let line = line.min(self.rope.len_lines().saturating_sub(1));
        self.byte_at(line, column)
    }

    /// Moves the cursor, optionally dragging the selection with it.
    pub fn place_cursor(&mut self, byte: usize, motion: Motion) {
        self.cursor = byte.min(self.rope.len_bytes());
        self.goal_column = None;
        // A plain click is a plain movement: back to one cursor, and the end
        // of the current undo run.
        self.after_move(motion);
    }

    /// Sets the selection outright, as one cursor. For the mouse, which
    /// knows both ends at once.
    pub fn select_range(&mut self, anchor: usize, cursor: usize) {
        let len = self.rope.len_bytes();
        self.anchor = anchor.min(len);
        self.cursor = cursor.min(len);
        self.clamp_positions();
        self.after_move(Motion::Extend);
        self.extra.clear();
    }

    /// The selection as the system's text input machinery wants it: UTF-16
    /// units, as (location, length).
    ///
    /// Measured from the start of the caret's line, not of the document. An
    /// input method only ever looks at the text around the caret, and a
    /// document-wide UTF-16 offset would mean counting through the whole
    /// file, a hundred megabytes of it, on every keystroke.
    pub fn input_selection(&self) -> (usize, usize) {
        let range = self.selection().unwrap_or(self.cursor..self.cursor);
        let line_start = self.rope.line_to_byte(self.rope.byte_to_line(range.start));
        let units = |from: usize, to: usize| -> usize {
            self.rope
                .chunks_in(from..to)
                .flat_map(str::chars)
                .map(char::len_utf16)
                .sum()
        };
        (
            units(line_start, range.start),
            units(range.start, range.end),
        )
    }

    /// Selects a range given in the coordinates of [`Buffer::input_selection`],
    /// clamped to the caret's line. This is how the press-and-hold accent
    /// menu says "replace the `e` you just typed".
    pub fn select_input_range(&mut self, location: usize, length: usize) {
        let line = self.rope.byte_to_line(self.cursor);
        let line_start = self.rope.line_to_byte(line);
        let line_end = self.line_end(line);
        let text = self.rope.slice_to_string(line_start..line_end);

        let mut units = 0;
        let (mut start, mut end) = (None, None);
        for (byte, ch) in text.char_indices() {
            if units >= location && start.is_none() {
                start = Some(byte);
            }
            if units >= location + length && end.is_none() {
                end = Some(byte);
            }
            units += ch.len_utf16();
        }
        let start = start.unwrap_or(text.len());
        let end = end.unwrap_or(text.len()).max(start);
        self.select_range(line_start + start, line_start + end);
    }

    /// The run of like characters around `at`: a word, a run of punctuation,
    /// or a run of spaces, whichever `at` is in. This is what a double click
    /// selects. It never crosses a line end.
    pub fn word_range_at(&self, at: usize) -> std::ops::Range<usize> {
        let at = at.min(self.rope.len_bytes());
        // At the end of a line the character to the left is the one meant.
        let probe = match self.char_at(at) {
            Some(c) if c != '\n' && c != '\r' => Some(c),
            _ => self.char_before(at).filter(|c| *c != '\n' && *c != '\r'),
        };
        let Some(class) = probe.map(class_of) else {
            return at..at;
        };
        let same = |c: char| c != '\n' && c != '\r' && class_of(c) == class;

        let mut start = at;
        while let Some(c) = self.char_before(start).filter(|c| same(*c)) {
            start -= c.len_utf8();
        }
        let mut end = at;
        while let Some(c) = self.char_at(end).filter(|c| same(*c)) {
            end += c.len_utf8();
        }
        start..end
    }

    /// The whole line containing `at`, with its line ending, which is what a
    /// triple click selects: deleting it then takes the line away, not just
    /// its text.
    pub fn line_range_at(&self, at: usize) -> std::ops::Range<usize> {
        let line = self.rope.byte_to_line(at.min(self.rope.len_bytes()));
        let start = self.rope.line_to_byte(line);
        let end = if line + 1 < self.rope.len_lines() {
            self.rope.line_to_byte(line + 1)
        } else {
            self.rope.len_bytes()
        };
        start..end
    }

    // ---- editing ---------------------------------------------------------

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            rope: self.rope.clone(),
            cursor: self.cursor,
            anchor: self.anchor,
            extra: self.extra.clone(),
        }
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.folds.clear();
        self.rope = snapshot.rope;
        self.cursor = snapshot.cursor;
        self.anchor = snapshot.anchor;
        self.extra = snapshot.extra;
    }

    /// Checkpoint for an edit made at the primary cursor only, which is every
    /// edit except plain typing and backspace.
    ///
    /// Such an edit shifts the text under the other cursors without moving
    /// them, so they are dropped here, after the snapshot has recorded them.
    /// Losing them is a limitation; keeping them was a crash on the next
    /// keystroke, or an edit in the middle of somebody else's character.
    fn checkpoint(&mut self, kind: EditKind) {
        self.checkpoint_keeping_cursors(kind);
        self.extra.clear();
    }

    /// Captures a snapshot unless this edit should fold into the previous
    /// one. Must be called before mutating the rope.
    fn checkpoint_keeping_cursors(&mut self, kind: EditKind) {
        let now = Instant::now();
        let coalesce = matches!(
            self.last_edit,
            Some((prev, at)) if prev == kind && now.duration_since(at) < COALESCE_WINDOW
        );
        if !coalesce {
            self.undo_stack.push(self.snapshot());
        }
        self.last_edit = Some((kind, now));
        self.redo_stack.clear();
        self.dirty = true;
    }

    pub fn insert(&mut self, text: &str) {
        if self.is_locked() {
            return;
        }
        let normalized;
        let text = if text.contains('\r') {
            normalized = text.replace("\r\n", "\n").replace('\r', "\n");
            normalized.as_str()
        } else {
            text
        };
        if !self.extra.is_empty() {
            self.checkpoint_keeping_cursors(EditKind::Insert);
            // Multi-cursor edits are not describable as a single tree-sitter
            // edit, so the syntax layer re-parses in full.
            self.invalidate_edits();
            self.edit_at_all_cursors(text, Reach::Nothing);
            return;
        }
        self.checkpoint(EditKind::Insert);
        // A selection being replaced is one edit from its start to its end,
        // not a delete followed by an insert.
        let selection = self.selection();
        let start = selection.as_ref().map_or(self.cursor, |r| r.start);
        let old_end = selection.as_ref().map_or(self.cursor, |r| r.end);
        let old_end_point = self.point_of(old_end);

        self.delete_selection_inner();
        self.rope.insert(self.cursor, text);
        self.cursor += text.len();
        self.anchor = self.cursor;
        self.goal_column = None;

        self.record_edit(start, old_end, old_end_point, self.cursor);
    }

    /// Deletes the selection if there is one, otherwise the character before
    /// the cursor.
    pub fn backspace(&mut self) {
        if self.is_locked() {
            return;
        }
        if !self.extra.is_empty() {
            self.checkpoint_keeping_cursors(EditKind::Delete);
            self.invalidate_edits();
            // One character back at each bare cursor; a selection consumes
            // itself instead.
            self.edit_at_all_cursors("", Reach::Back);
            return;
        }
        if let Some(range) = self.selection() {
            self.checkpoint(EditKind::Delete);
            self.delete_range_recorded(range);
            return;
        }
        if self.cursor == 0 {
            return;
        }
        self.checkpoint(EditKind::Delete);
        let prev = self.prev_boundary(self.cursor);
        let old_end_point = self.point_of(self.cursor);
        let old_end = self.cursor;
        self.rope.delete(prev..self.cursor);
        self.cursor = prev;
        self.anchor = prev;
        self.goal_column = None;
        self.record_edit(prev, old_end, old_end_point, prev);
    }

    /// Deletes the selection if there is one, otherwise the character at the
    /// cursor.
    pub fn delete_forward(&mut self) {
        if self.is_locked() {
            return;
        }
        if let Some(range) = self.selection() {
            self.checkpoint(EditKind::Delete);
            self.delete_range_recorded(range);
            return;
        }
        if self.cursor >= self.rope.len_bytes() {
            return;
        }
        self.checkpoint(EditKind::Delete);
        let next = self.next_boundary(self.cursor);
        let old_end_point = self.point_of(next);
        self.rope.delete(self.cursor..next);
        self.anchor = self.cursor;
        self.goal_column = None;
        self.record_edit(self.cursor, next, old_end_point, self.cursor);
    }

    /// Removes the selected range. Assumes a checkpoint has been taken.
    fn delete_selection_inner(&mut self) {
        if let Some(range) = self.selection() {
            self.rope.delete(range.clone());
            self.cursor = range.start;
            self.anchor = range.start;
        }
    }

    /// Deletes `range`, recording the edit. Used by the word and line
    /// deletions, which all have the same shape.
    fn delete_range_recorded(&mut self, range: std::ops::Range<usize>) {
        let (start, old_end) = (range.start, range.end);
        let old_end_point = self.point_of(old_end);
        self.rope.delete(range);
        self.cursor = start;
        self.anchor = start;
        self.goal_column = None;
        self.record_edit(start, old_end, old_end_point, start);
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    pub fn undo(&mut self) -> bool {
        if self.is_locked() {
            return false;
        }
        let Some(snapshot) = self.undo_stack.pop() else {
            return false;
        };
        self.redo_stack.push(self.snapshot());
        self.restore(snapshot);
        // The whole rope was replaced, so queued edits no longer describe
        // how to get from the old text to the new. Force a full re-parse.
        self.invalidate_edits();
        self.pending_edits.clear();
        // Break coalescing, so the next keystroke starts a fresh undo step
        // instead of folding into the one we just reverted.
        self.last_edit = None;
        self.goal_column = None;
        self.dirty = true;
        true
    }

    pub fn redo(&mut self) -> bool {
        if self.is_locked() {
            return false;
        }
        let Some(snapshot) = self.redo_stack.pop() else {
            return false;
        };
        self.undo_stack.push(self.snapshot());
        self.restore(snapshot);
        self.invalidate_edits();
        self.pending_edits.clear();
        self.last_edit = None;
        self.goal_column = None;
        self.dirty = true;
        true
    }

    // ---- multiple cursors ------------------------------------------------

    /// How many cursors are active, including the primary.
    pub fn cursor_count(&self) -> usize {
        self.extra.len() + 1
    }

    /// Every selection as a normalised `(start, end)`, ascending, with
    /// overlaps merged.
    ///
    /// Merging matters: two cursors that have grown into each other must
    /// become one, or an edit would be applied twice to the same text.
    /// Selections that merely touch stay apart, since `ab` selected twice in
    /// `abab` is two selections, but a bare caret on the edge of a selection
    /// has nothing of its own to edit and folds into it.
    fn all_selections(&self) -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = std::iter::once((self.anchor, self.cursor))
            .chain(self.extra.iter().copied())
            .map(|(a, h)| (a.min(h), a.max(h)))
            .collect();
        out.sort_by_key(|r| r.0);

        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(out.len());
        for range in out {
            match merged.last_mut() {
                Some(last)
                    if range.0 < last.1
                        || (range.0 == last.1 && (range.0 == range.1 || last.0 == last.1)) =>
                {
                    last.1 = last.1.max(range.1)
                }
                _ => merged.push(range),
            }
        }
        merged
    }

    /// Every selection as `(start, end)`, ascending and merged. Public so
    /// the renderer can draw all of them.
    pub fn selections(&self) -> Vec<(usize, usize)> {
        self.all_selections()
    }

    /// Every caret position, ascending. Empty selections included.
    pub fn caret_positions(&self) -> Vec<usize> {
        let mut out: Vec<usize> = std::iter::once(self.cursor)
            .chain(self.extra.iter().map(|&(_, h)| h))
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Adds a cursor, ignoring one that duplicates an existing position.
    pub fn add_cursor(&mut self, anchor: usize, head: usize) {
        let len = self.rope.len_bytes();
        let (anchor, head) = (anchor.min(len), head.min(len));
        if (self.anchor, self.cursor) == (anchor, head) || self.extra.contains(&(anchor, head)) {
            return;
        }
        self.extra.push((anchor, head));
    }

    /// Drops every cursor but the primary.
    pub fn collapse_cursors(&mut self) -> bool {
        if self.extra.is_empty() {
            return false;
        }
        self.extra.clear();
        true
    }

    /// Cmd-D: selects the word at the cursor, or adds the next occurrence of
    /// the current selection as another cursor.
    pub fn select_next_occurrence(&mut self) -> bool {
        let Some(range) = self.selection() else {
            // Nothing selected yet: select the word under the caret, which is
            // what makes the first press useful. A caret at the end of a
            // word still means that word, not the delimiter after it.
            let is_word = |c: Option<char>| c.is_some_and(|c| class_of(c) == CharClass::Word);
            let start =
                if !is_word(self.char_at(self.cursor)) && is_word(self.char_before(self.cursor)) {
                    self.prev_word_boundary(self.cursor)
                } else {
                    self.prev_word_boundary(self.next_boundary(self.cursor))
                };
            let end = self.next_word_boundary(start);
            if end <= start {
                return false;
            }
            self.anchor = start;
            self.cursor = end;
            return true;
        };

        let needle = self.rope.slice_to_string(range.clone());
        if needle.is_empty() {
            return false;
        }

        // Search forward from the furthest selection, wrapping once, and
        // skipping occurrences that already have a cursor: after the wrap the
        // first hit is usually one of those, and stopping at it would leave
        // everything between it and the starting point unreachable.
        let selected = self.all_selections();
        let from = selected.last().map_or(range.end, |r| r.1);
        let mut at = from;
        let mut wrapped = false;
        loop {
            match self.rope.find_from(&needle, at) {
                Some(found) if wrapped && found >= from => return false,
                Some(found) if selected.iter().any(|r| r.0 == found) => {
                    at = found + needle.len();
                }
                Some(found) => {
                    self.add_cursor(found, found + needle.len());
                    return true;
                }
                None if wrapped => return false,
                None => {
                    wrapped = true;
                    at = 0;
                }
            }
        }
    }

    /// Replaces every selection with `replacement`, back to front.
    ///
    /// Back to front so that an earlier edit cannot invalidate a later
    /// offset. Positions are then recomputed front to back, accumulating the
    /// net length change as it goes.
    fn edit_at_all_cursors(&mut self, replacement: &str, reach: Reach) {
        // Every range is measured here, against the text as it is, before any
        // of them is applied. How far a caret reaches depends on the
        // character next to *that* caret: one width for all of them deleted
        // half of an accent under one cursor because another sat after an
        // ASCII letter.
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for (start, end) in self.all_selections() {
            let range = match reach {
                _ if end > start => (start, end),
                Reach::Nothing => (start, end),
                Reach::Back => (self.prev_boundary(start), end),
                Reach::Both => (self.prev_boundary(start), self.next_boundary(end)),
            };
            // Reaching can run into the neighbour, and two edits over the
            // same bytes would delete past what the first one left.
            match ranges.last_mut() {
                Some(last) if range.0 < last.1 => last.1 = last.1.max(range.1),
                _ => ranges.push(range),
            }
        }

        for &(from, end) in ranges.iter().rev() {
            if end > from {
                self.rope.delete(from..end);
            }
            if !replacement.is_empty() {
                self.rope.insert(from, replacement);
            }
        }

        // Recompute every cursor's landing position.
        let mut delta: isize = 0;
        let mut positions = Vec::with_capacity(ranges.len());
        for &(from, end) in &ranges {
            let landing = (from as isize + delta) as usize + replacement.len();
            positions.push(landing);
            delta += replacement.len() as isize - (end as isize - from as isize);
        }

        self.cursor = *positions.first().expect("at least one cursor");
        self.anchor = self.cursor;
        self.extra = positions[1..].iter().map(|&p| (p, p)).collect();
        self.clamp_positions();
    }

    /// Whether every cursor is a bare caret for which `test` holds.
    fn every_caret(&self, test: impl Fn(&Self, usize) -> bool) -> bool {
        self.all_selections()
            .iter()
            .all(|&(start, end)| start == end && test(self, start))
    }

    /// Moves every caret by `by` bytes, collapsing each to a bare caret.
    fn shift_carets(&mut self, by: isize) {
        let shift = |p: usize| (p as isize + by).max(0) as usize;
        self.cursor = shift(self.cursor);
        self.anchor = self.cursor;
        for cursor in &mut self.extra {
            cursor.1 = shift(cursor.1);
            cursor.0 = cursor.1;
        }
        self.clamp_positions();
    }

    // ---- bracket pairing -------------------------------------------------

    /// Types `ch`, auto-closing brackets and quotes where it helps.
    ///
    /// Three behaviours, all of which exist because their absence is
    /// immediately irritating:
    ///   - typing an opener inserts the pair and sits between them
    ///   - typing a closer that is already there steps over it instead of
    ///     inserting a second one
    ///   - with text selected, an opener wraps the selection
    ///
    /// Auto-close is suppressed when the next character is a letter or digit,
    /// because typing `(` before an existing word almost always means calling
    /// it, not wrapping it.
    pub fn insert_char_paired(&mut self, ch: char) {
        if self.is_locked() {
            return;
        }
        let closer = match ch {
            '(' => Some(')'),
            '[' => Some(']'),
            '{' => Some('}'),
            '"' => Some('"'),
            '\'' => Some('\''),
            '`' => Some('`'),
            _ => None,
        };

        // Wrap a selection rather than replacing it.
        if let (Some(closer), Some(range)) = (closer, self.selection()) {
            // The wrapped text is the primary selection's. Other cursors may
            // hold something else, or nothing, so this is a one-cursor edit.
            self.extra.clear();
            let text = self.rope.slice_to_string(range.clone());
            self.insert(&format!("{ch}{text}{closer}"));
            // Leave the wrapped text selected, which is what makes wrapping
            // twice work.
            self.anchor = range.start + ch.len_utf8();
            self.cursor = self.anchor + text.len();
            return;
        }

        // Step over a closer that is already there.
        if matches!(ch, ')' | ']' | '}' | '"' | '\'' | '`')
            && self.every_caret(|b, at| b.char_at(at) == Some(ch))
        {
            // At every caret or at none: stepping over at some and typing at
            // others would leave the cursors disagreeing about what happened.
            self.shift_carets(ch.len_utf8() as isize);
            self.last_edit = None;
            self.goal_column = None;
            return;
        }

        let Some(closer) = closer else {
            self.insert(&ch.to_string());
            return;
        };

        let next_is_word = self
            .char_at(self.cursor)
            .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if next_is_word {
            self.insert(&ch.to_string());
            return;
        }

        // A quote immediately after a word is a closing quote or an
        // apostrophe, not the start of a new string.
        if matches!(ch, '"' | '\'' | '`')
            && self
                .char_before(self.cursor)
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            self.insert(&ch.to_string());
            return;
        }

        let mut pair = String::with_capacity(2);
        pair.push(ch);
        pair.push(closer);
        self.insert(&pair);
        // Every cursor typed the pair, so every cursor sits inside its own.
        self.shift_carets(-(closer.len_utf8() as isize));
    }

    /// Backspace that removes both halves of an empty pair.
    pub fn backspace_paired(&mut self) {
        if self.is_locked() {
            return;
        }
        const PAIRS: [(char, char); 6] = [
            ('(', ')'),
            ('[', ']'),
            ('{', '}'),
            ('"', '"'),
            ('\'', '\''),
            ('`', '`'),
        ];
        if !self.extra.is_empty() {
            let in_empty_pair = |b: &Self, at: usize| matches!((b.char_before(at), b.char_at(at)), (Some(o), Some(c)) if PAIRS.contains(&(o, c)));
            if self.every_caret(in_empty_pair) {
                self.checkpoint_keeping_cursors(EditKind::Delete);
                self.invalidate_edits();
                self.edit_at_all_cursors("", Reach::Both);
            } else {
                self.backspace();
            }
            return;
        }
        if self.selection().is_none() {
            let before = self.char_before(self.cursor);
            let after = self.char_at(self.cursor);
            let empty_pair = matches!(
                (before, after),
                (Some('('), Some(')'))
                    | (Some('['), Some(']'))
                    | (Some('{'), Some('}'))
                    | (Some('"'), Some('"'))
                    | (Some('\''), Some('\''))
                    | (Some('`'), Some('`'))
            );
            if empty_pair {
                let start = self.prev_boundary(self.cursor);
                let end = self.next_boundary(self.cursor);
                self.checkpoint(EditKind::Delete);
                self.delete_range_recorded(start..end);
                return;
            }
        }
        self.backspace();
    }

    // ---- search and replace ----------------------------------------------

    /// Replaces the selection with `text` if it matches `needle`, then finds
    /// the next occurrence. Returns whether anything was replaced.
    pub fn replace_current(&mut self, needle: &str, text: &str) -> bool {
        if self.is_locked() {
            return false;
        }
        let Some(range) = self.selection() else {
            return false;
        };
        if self.rope.slice_to_string(range.clone()) != needle {
            return false;
        }
        self.insert(text);
        true
    }

    /// Replaces every occurrence of `needle`, returning how many.
    ///
    /// One undo step for the whole operation, which is the only thing that
    /// makes a mistaken replace-all recoverable.
    pub fn replace_all(&mut self, needle: &str, text: &str) -> usize {
        if self.is_locked() {
            return 0;
        }
        if needle.is_empty() {
            return 0;
        }
        let mut offsets = Vec::new();
        let mut at = 0;
        while let Some(found) = self.rope.find_from(needle, at) {
            offsets.push(found);
            at = found + needle.len();
        }
        if offsets.is_empty() {
            return 0;
        }

        self.checkpoint(EditKind::Insert);
        // Back to front, so earlier replacements do not shift later offsets.
        for &offset in offsets.iter().rev() {
            let end = offset + needle.len();
            let old_end_point = self.point_of(end);
            self.rope.delete(offset..end);
            self.rope.insert(offset, text);
            self.record_edit(offset, end, old_end_point, offset + text.len());
        }

        self.cursor = self.cursor.min(self.rope.len_bytes());
        self.anchor = self.cursor;
        self.clamp_positions();
        offsets.len()
    }

    /// Applies already-resolved ranges as one undoable edit: regex captures,
    /// case-insensitive matches, a language server's edits. Ranges are in
    /// order and do not overlap; an empty one is an insertion. The caret
    /// keeps its place in the text around the edits.
    pub fn replace_ranges(&mut self, replacements: &[(std::ops::Range<usize>, String)]) -> usize {
        if self.is_locked() {
            return 0;
        }
        if replacements.is_empty() {
            return 0;
        }
        let mut previous_end = 0;
        for (range, _) in replacements {
            if range.start < previous_end
                || range.start > range.end
                || range.end > self.rope.len_bytes()
            {
                return 0;
            }
            previous_end = range.end;
        }
        let mut caret = self.cursor as isize;
        for (range, replacement) in replacements {
            if range.end <= self.cursor {
                caret += replacement.len() as isize - range.len() as isize;
            } else if range.start < self.cursor {
                caret = caret - (self.cursor - range.start) as isize + replacement.len() as isize;
            }
        }
        self.checkpoint(EditKind::Insert);
        for (range, replacement) in replacements.iter().rev() {
            let old_end_point = self.point_of(range.end);
            self.rope.delete(range.clone());
            self.rope.insert(range.start, replacement);
            self.record_edit(
                range.start,
                range.end,
                old_end_point,
                range.start + replacement.len(),
            );
        }
        self.cursor = (caret.max(0) as usize).min(self.rope.len_bytes());
        self.anchor = self.cursor;
        self.clamp_positions();
        replacements.len()
    }

    // ---- line and block operations ---------------------------------------

    /// The leading whitespace of `line`, as a string.
    fn indent_of(&self, line: usize) -> String {
        let start = self.rope.line_to_byte(line);
        let end = self.line_end(line);
        let text = self.rope.slice_to_string(start..end);
        text.chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect()
    }

    /// One indent level, matching whatever the surrounding line already uses.
    ///
    /// Guessing from context rather than from a setting: a file that is
    /// indented with tabs should stay indented with tabs, and there is no
    /// preferences system yet to say otherwise.
    fn indent_unit(&self, line: usize) -> String {
        if let Some(style) = self.indent_style {
            return style.unit();
        }
        let indent = self.indent_of(line);
        if indent.contains('\t') {
            "\t".to_string()
        } else {
            "    ".to_string()
        }
    }

    /// Tab with nothing selected: a tab character, or spaces to the next
    /// stop when the document indents with spaces.
    pub fn insert_tab(&mut self) {
        match self.indent_style {
            Some(style) if !style.tabs => {
                let (_, column) = self.cursor_position();
                let width = style.width.max(1);
                self.insert(&" ".repeat(width - column % width));
            }
            _ => self.insert("\t"),
        }
    }

    /// Inserts a newline, carrying the current indentation, and adding a
    /// level when the line being left ends in an opening bracket.
    pub fn insert_newline_indented(&mut self) {
        if self.is_locked() {
            return;
        }
        let (line, _) = self.cursor_position();
        let indent = self.indent_of(line);

        // Look at the text before the cursor, not the whole line: pressing
        // Enter in the middle of `{ foo }` should not indent.
        let line_start = self.rope.line_to_byte(line);
        let before = self.rope.slice_to_string(line_start..self.cursor);
        let opens = before.trim_end().ends_with(['{', '[', '(']);

        // And whether a closing bracket sits immediately after, in which case
        // it gets a line of its own at the outer level.
        let after_is_close = self
            .char_at(self.cursor)
            .is_some_and(|c| matches!(c, '}' | ']' | ')'));

        let unit = self.indent_unit(line);
        let mut text = String::with_capacity(indent.len() + unit.len() + 2);
        text.push('\n');
        text.push_str(&indent);
        if opens {
            text.push_str(&unit);
        }
        self.insert(&text);

        if opens && after_is_close {
            // Put the closer on its own line, then step back onto the blank
            // line between them.
            let landing = self.cursor;
            let mut tail = String::with_capacity(indent.len() + 1);
            tail.push('\n');
            tail.push_str(&indent);
            self.insert(&tail);
            self.cursor = landing;
            self.anchor = landing;
        }
    }

    /// Lines touched by the selection, or the cursor's line.
    fn selected_lines(&self) -> std::ops::RangeInclusive<usize> {
        match self.selection() {
            Some(range) => {
                let first = self.rope.byte_to_line(range.start);
                // A selection ending exactly at a line start does not include
                // that line; otherwise selecting a whole line by dragging to
                // the next one would indent two.
                let mut last = self.rope.byte_to_line(range.end);
                if last > first && range.end == self.rope.line_to_byte(last) {
                    last -= 1;
                }
                first..=last
            }
            None => {
                let (line, _) = self.cursor_position();
                line..=line
            }
        }
    }

    /// Adds one indent level to every selected line.
    pub fn indent(&mut self) {
        if self.is_locked() {
            return;
        }
        let lines = self.selected_lines();
        let unit = self.indent_unit(*lines.start());
        self.checkpoint(EditKind::Insert);

        // Back to front, so earlier insertions do not shift later offsets.
        // Cursor and anchor each move by the indents inserted *before* them,
        // which is not the same number for both when only part of a line is
        // selected.
        let mut before_cursor = 0usize;
        let mut before_anchor = 0usize;
        for line in lines.rev() {
            let at = self.rope.line_to_byte(line);
            let old_end_point = self.point_of(at);
            self.rope.insert(at, &unit);
            self.record_edit(at, at, old_end_point, at + unit.len());
            if at <= self.cursor {
                before_cursor += unit.len();
            }
            if at <= self.anchor {
                before_anchor += unit.len();
            }
        }
        self.cursor += before_cursor;
        self.anchor += before_anchor;
        self.clamp_positions();
    }

    /// Removes one indent level from every selected line that has one.
    pub fn outdent(&mut self) {
        if self.is_locked() {
            return;
        }
        let lines = self.selected_lines();
        self.checkpoint(EditKind::Delete);

        let mut removed_before_cursor = 0usize;
        let mut removed_before_anchor = 0usize;
        for line in lines.rev() {
            let at = self.rope.line_to_byte(line);
            let indent = self.indent_of(line);
            // Take a tab, or up to one level of spaces: a line indented by
            // three spaces should still outdent rather than refusing.
            let level = self.indent_style.map_or(4, |s| s.width.max(1));
            let take = if indent.starts_with('\t') {
                1
            } else {
                indent.chars().take_while(|c| *c == ' ').count().min(level)
            };
            if take == 0 {
                continue;
            }
            let old_end_point = self.point_of(at + take);
            self.rope.delete(at..at + take);
            self.record_edit(at, at + take, old_end_point, at);
            if at < self.cursor {
                removed_before_cursor += take;
            }
            if at < self.anchor {
                removed_before_anchor += take;
            }
        }
        self.cursor = self.cursor.saturating_sub(removed_before_cursor);
        self.anchor = self.anchor.saturating_sub(removed_before_anchor);
        self.clamp_positions();
    }

    /// Comments or uncomments the selected lines with `token`.
    ///
    /// Toggling on the whole block rather than per line: if any selected line
    /// is uncommented, the whole block gets commented, which is what makes a
    /// second press restore exactly what you started with.
    pub fn toggle_comment(&mut self, token: &str) {
        if self.is_locked() {
            return;
        }
        let lines = self.selected_lines();
        let non_empty: Vec<usize> = lines
            .clone()
            .filter(|&l| {
                let start = self.rope.line_to_byte(l);
                let end = self.line_end(l);
                !self.rope.slice_to_string(start..end).trim().is_empty()
            })
            .collect();
        if non_empty.is_empty() {
            return;
        }

        let all_commented = non_empty.iter().all(|&l| {
            let start = self.rope.line_to_byte(l);
            let end = self.line_end(l);
            self.rope
                .slice_to_string(start..end)
                .trim_start()
                .starts_with(token)
        });

        self.checkpoint(if all_commented {
            EditKind::Delete
        } else {
            EditKind::Insert
        });

        for &line in non_empty.iter().rev() {
            let start = self.rope.line_to_byte(line);
            let end = self.line_end(line);
            let text = self.rope.slice_to_string(start..end);
            let indent_len = text.len() - text.trim_start().len();
            let at = start + indent_len;

            if all_commented {
                // Remove the token and one following space if it is there.
                let rest = &text[indent_len..];
                let mut take = token.len();
                if rest[take..].starts_with(' ') {
                    take += 1;
                }
                let old_end_point = self.point_of(at + take);
                self.rope.delete(at..at + take);
                self.record_edit(at, at + take, old_end_point, at);
            } else {
                let insert = format!("{token} ");
                let old_end_point = self.point_of(at);
                self.rope.insert(at, &insert);
                self.record_edit(at, at, old_end_point, at + insert.len());
            }
        }
        self.clamp_positions();
    }

    /// Duplicates the selected lines below themselves.
    pub fn duplicate_lines(&mut self) {
        if self.is_locked() {
            return;
        }
        let lines = self.selected_lines();
        let start = self.rope.line_to_byte(*lines.start());
        let last = *lines.end();
        let end = if last + 1 < self.rope.len_lines() {
            self.rope.line_to_byte(last + 1)
        } else {
            self.rope.len_bytes()
        };

        // On a last line with no trailing newline the separator has to go
        // *before* the copy, or the two lines run together.
        let source = self.rope.slice_to_string(start..end);
        let text = if source.ends_with('\n') {
            source
        } else {
            format!("\n{source}")
        };

        self.checkpoint(EditKind::Insert);
        let old_end_point = self.point_of(end);
        self.rope.insert(end, &text);
        self.record_edit(end, end, old_end_point, end + text.len());

        // Move onto the copy, which is what makes repeated presses stack.
        self.cursor += text.len();
        self.anchor += text.len();
        self.clamp_positions();
    }

    /// Moves the selected lines up or down by one.
    ///
    /// Implemented as a swap of two adjacent spans rather than a delete and
    /// a re-insert elsewhere, so it stays one edit for undo and for the
    /// incremental parser.
    pub fn move_lines(&mut self, down: bool) {
        if self.is_locked() {
            return;
        }
        let lines = self.selected_lines();
        let (first, last) = (*lines.start(), *lines.end());
        let total = self.rope.len_lines();
        if (down && last + 1 >= total) || (!down && first == 0) {
            return;
        }

        // The span covering the block plus the line it swaps with, and the
        // offset where one ends and the other begins.
        let (span_first, span_last) = if down {
            (first, last + 1)
        } else {
            (first - 1, last)
        };
        let start = self.rope.line_to_byte(span_first);
        let end = if span_last + 1 < total {
            self.rope.line_to_byte(span_last + 1)
        } else {
            self.rope.len_bytes()
        };
        let split = self.rope.line_to_byte(if down { last + 1 } else { first });

        let head = self.rope.slice_to_string(start..split);
        let tail = self.rope.slice_to_string(split..end);

        // After the swap `tail` comes first and must end in a newline, while
        // `head` comes last and must only keep one if the span originally did.
        // The last line of a file with no trailing newline is what makes this
        // fiddly rather than a plain concatenation.
        let mut leading = tail;
        if !leading.ends_with('\n') {
            leading.push('\n');
        }
        let mut trailing = head;
        if !self.rope.slice_to_string(start..end).ends_with('\n') {
            trailing = trailing.strip_suffix('\n').unwrap_or(&trailing).to_string();
        }
        let swapped = format!("{leading}{trailing}");

        self.checkpoint(EditKind::Insert);
        let old_end_point = self.point_of(end);
        self.rope.delete(start..end);
        self.rope.insert(start, &swapped);
        self.record_edit(start, end, old_end_point, start + swapped.len());

        // Follow the block: moving down it now begins after `leading`,
        // moving up it begins where the span does.
        let delta = if down {
            leading.len() as isize
        } else {
            start as isize - split as isize
        };
        self.cursor = (self.cursor as isize + delta).max(0) as usize;
        self.anchor = (self.anchor as isize + delta).max(0) as usize;
        self.clamp_positions();
    }

    /// Places the cursor at the start of `line`, 0-based and clamped.
    pub fn goto_line(&mut self, line: usize) {
        let line = line.min(self.rope.len_lines().saturating_sub(1));
        self.cursor = self.rope.line_to_byte(line);
        self.goal_column = None;
        self.after_move(Motion::Move);
    }

    /// Clamps cursor and anchor into the buffer and onto char boundaries.
    fn clamp_positions(&mut self) {
        let rope = &self.rope;
        let clamp = |at: usize| {
            let mut at = at.min(rope.len_bytes());
            while at > 0 && rope.byte_at(at).is_some_and(is_continuation) {
                at -= 1;
            }
            at
        };
        self.cursor = clamp(self.cursor);
        self.anchor = clamp(self.anchor);
        // The extra cursors too: they are dereferenced by the renderer on
        // the very next frame, and an offset past the end asserts there.
        for cursor in &mut self.extra {
            *cursor = (clamp(cursor.0), clamp(cursor.1));
        }
        self.goal_column = None;
    }

    // ---- change tracking -------------------------------------------------

    /// tree-sitter's point for a byte offset: row, plus the byte column
    /// within that row.
    fn point_of(&self, byte: usize) -> (usize, usize) {
        let row = self.rope.byte_to_line(byte);
        (row, byte - self.rope.line_to_byte(row))
    }

    /// Records a replacement of `start..old_end` by `new_len` bytes.
    ///
    /// Must be called with `old_end_point` captured before the rope changed
    /// and `new_end` measured after, which is why each edit site calls this
    /// rather than deriving edits afterwards.
    fn record_edit(
        &mut self,
        start: usize,
        old_end: usize,
        old_end_point: (usize, usize),
        new_end: usize,
    ) {
        let start_point = self.point_of(start);
        let new_end_point = self.point_of(new_end);
        if !self.folds.is_empty() {
            // Folds below the edit move with it; one the edit reaches into
            // opens, since what it hid has changed.
            let (first, old_last, new_last) = (start_point.0, old_end_point.0, new_end_point.0);
            let delta = new_last as isize - old_last as isize;
            self.folds.retain_mut(|(a, b)| {
                if *b < first {
                    true
                } else if *a > old_last {
                    *a = (*a as isize + delta) as usize;
                    *b = (*b as isize + delta) as usize;
                    true
                } else {
                    false
                }
            });
        }
        self.pending_edits.push(Edit {
            start_byte: start,
            old_end_byte: old_end,
            new_end_byte: new_end,
            start_point,
            old_end_point,
            new_end_point,
        });
    }

    /// Hands over the edits since the last drain.
    ///
    /// Returns `None` when the text changed in a way edits cannot describe
    /// (undo, redo, a reload), meaning the caller must re-parse in full.
    /// Whether the text changed since the last [`Buffer::drain_edits`].
    /// The text changed in a way edits cannot describe. Folds cannot follow
    /// such a change either, so they open.
    fn invalidate_edits(&mut self) {
        self.edits_invalidated = true;
        self.folds.clear();
    }

    pub fn has_pending_edits(&self) -> bool {
        self.edits_invalidated || !self.pending_edits.is_empty()
    }

    pub fn drain_edits(&mut self) -> Option<Vec<Edit>> {
        if self.edits_invalidated {
            self.edits_invalidated = false;
            self.pending_edits.clear();
            return None;
        }
        Some(std::mem::take(&mut self.pending_edits))
    }

    // ---- words -----------------------------------------------------------

    /// The character immediately before `at`, if any.
    fn char_before(&self, at: usize) -> Option<char> {
        if at == 0 {
            return None;
        }
        let start = self.prev_boundary(at);
        self.rope.slice_to_string(start..at).chars().next()
    }

    /// The character starting at `at`, if any.
    fn char_at(&self, at: usize) -> Option<char> {
        if at >= self.rope.len_bytes() {
            return None;
        }
        let end = self.next_boundary(at);
        self.rope.slice_to_string(at..end).chars().next()
    }

    /// Start of the word before `at`.
    ///
    /// Follows the macOS convention: skip any whitespace first, then consume
    /// one run of like characters. So from `"foo bar|"` you land before
    /// `bar`, and from `"foo bar |"` you also land before `bar` rather than
    /// stopping on the space.
    pub fn prev_word_boundary(&self, at: usize) -> usize {
        let mut i = at;
        while i > 0 {
            match self.char_before(i) {
                Some(c) if c.is_whitespace() && c != '\n' => i = self.prev_boundary(i),
                _ => break,
            }
        }
        let Some(first) = self.char_before(i) else {
            return i;
        };
        // A newline is its own boundary; stepping over it would merge lines
        // in a way nobody expects from a word motion.
        if first == '\n' {
            return self.prev_boundary(i);
        }
        let class = class_of(first);
        while i > 0 {
            match self.char_before(i) {
                Some(c) if c != '\n' && class_of(c) == class => i = self.prev_boundary(i),
                _ => break,
            }
        }
        i
    }

    /// End of the word after `at`.
    pub fn next_word_boundary(&self, at: usize) -> usize {
        let len = self.rope.len_bytes();
        let mut i = at;
        while i < len {
            match self.char_at(i) {
                Some(c) if c.is_whitespace() && c != '\n' => i = self.next_boundary(i),
                _ => break,
            }
        }
        let Some(first) = self.char_at(i) else {
            return i;
        };
        if first == '\n' {
            return self.next_boundary(i);
        }
        let class = class_of(first);
        while i < len {
            match self.char_at(i) {
                Some(c) if c != '\n' && class_of(c) == class => i = self.next_boundary(i),
                _ => break,
            }
        }
        i
    }

    pub fn move_word_left(&mut self, motion: Motion) {
        self.cursor = self.prev_word_boundary(self.cursor);
        self.goal_column = None;
        self.after_move(motion);
    }

    pub fn move_word_right(&mut self, motion: Motion) {
        self.cursor = self.next_word_boundary(self.cursor);
        self.goal_column = None;
        self.after_move(motion);
    }

    /// Option-Backspace: delete the word before the cursor.
    pub fn delete_word_backward(&mut self) {
        if self.is_locked() {
            return;
        }
        if self.selection().is_some() {
            self.backspace();
            return;
        }
        let to = self.prev_word_boundary(self.cursor);
        if to == self.cursor {
            return;
        }
        self.checkpoint(EditKind::Delete);
        self.delete_range_recorded(to..self.cursor);
    }

    /// Option-Delete: delete the word after the cursor.
    pub fn delete_word_forward(&mut self) {
        if self.is_locked() {
            return;
        }
        if self.selection().is_some() {
            self.delete_forward();
            return;
        }
        let to = self.next_word_boundary(self.cursor);
        if to == self.cursor {
            return;
        }
        self.checkpoint(EditKind::Delete);
        self.delete_range_recorded(self.cursor..to);
    }

    /// Cmd-Backspace: delete from the cursor back to the start of the line.
    pub fn delete_to_line_start(&mut self) {
        if self.is_locked() {
            return;
        }
        if self.selection().is_some() {
            self.backspace();
            return;
        }
        let (line, _) = self.cursor_position();
        let start = self.rope.line_to_byte(line);
        if start == self.cursor {
            return;
        }
        self.checkpoint(EditKind::Delete);
        self.delete_range_recorded(start..self.cursor);
    }

    /// Cmd-Delete: delete from the cursor to the end of the line.
    pub fn delete_to_line_end(&mut self) {
        if self.is_locked() {
            return;
        }
        if self.selection().is_some() {
            self.delete_forward();
            return;
        }
        let (line, _) = self.cursor_position();
        let end = self.line_end(line);
        if end == self.cursor {
            return;
        }
        self.checkpoint(EditKind::Delete);
        self.delete_range_recorded(self.cursor..end);
    }

    // ---- movement --------------------------------------------------------

    fn after_move(&mut self, motion: Motion) {
        if motion == Motion::Move {
            self.anchor = self.cursor;
            // Plain movement is how you get back to one cursor.
            self.extra.clear();
        }
        // Any movement ends an edit run for undo purposes.
        self.last_edit = None;
    }

    pub fn move_left(&mut self, motion: Motion) {
        // Plain left with a selection collapses to its start, which is what
        // every other editor does.
        if motion == Motion::Move
            && let Some(range) = self.selection()
        {
            self.cursor = range.start;
            self.goal_column = None;
            self.after_move(motion);
            return;
        }
        self.cursor = self.prev_boundary(self.cursor);
        self.goal_column = None;
        self.after_move(motion);
    }

    pub fn move_right(&mut self, motion: Motion) {
        if motion == Motion::Move
            && let Some(range) = self.selection()
        {
            self.cursor = range.end;
            self.goal_column = None;
            self.after_move(motion);
            return;
        }
        self.cursor = self.next_boundary(self.cursor);
        self.goal_column = None;
        self.after_move(motion);
    }

    pub fn move_up(&mut self, motion: Motion) {
        if self.row_mode() {
            return self.move_row(-1, motion);
        }
        let (line, column) = self.cursor_position();
        if line == 0 {
            self.cursor = 0;
        } else {
            let goal = self.goal_column.unwrap_or(column);
            self.cursor = self.byte_at(line - 1, goal);
            self.goal_column = Some(goal);
        }
        self.after_move(motion);
    }

    pub fn move_down(&mut self, motion: Motion) {
        if self.row_mode() {
            return self.move_row(1, motion);
        }
        let (line, column) = self.cursor_position();
        if line + 1 >= self.rope.len_lines() {
            self.cursor = self.rope.len_bytes();
        } else {
            let goal = self.goal_column.unwrap_or(column);
            self.cursor = self.byte_at(line + 1, goal);
            self.goal_column = Some(goal);
        }
        self.after_move(motion);
    }

    /// Whether screen rows differ from lines: wrapping, or lines folded away.
    pub fn row_mode(&self) -> bool {
        self.wrap.is_some() || !self.folds.is_empty()
    }

    /// Whether `line` is folded away.
    pub fn is_hidden(&self, line: usize) -> bool {
        let at = self.folds.partition_point(|(_, b)| *b < line);
        self.folds.get(at).is_some_and(|(a, _)| *a <= line)
    }

    /// The last line of the fold hiding `line`, if one does.
    pub fn hidden_until(&self, line: usize) -> Option<usize> {
        let at = self.folds.partition_point(|(_, b)| *b < line);
        self.folds
            .get(at)
            .filter(|(a, _)| *a <= line)
            .map(|(_, b)| *b)
    }

    /// Leading whitespace of `line` in columns, or `None` for a blank line.
    fn indent_columns(&self, line: usize) -> Option<usize> {
        let start = self.rope.line_to_byte(line);
        let end = self.line_end(line).min(start + 1024);
        let mut column = 0;
        for chunk in self.rope.chunks_in(start..end) {
            for ch in chunk.chars() {
                match ch {
                    ' ' => column += 1,
                    '\t' => column = column / 4 * 4 + 4,
                    '\r' => {}
                    _ => return Some(column),
                }
            }
        }
        None
    }

    /// Whether the next non-blank line after `line` is indented deeper:
    /// the cheap test for a fold, run for every visible line.
    pub fn can_fold(&self, line: usize) -> bool {
        let Some(base) = self.indent_columns(line) else {
            return false;
        };
        let total = self.rope.len_lines();
        (line + 1..total.min(line + 64))
            .find_map(|l| self.indent_columns(l))
            .is_some_and(|next| next > base)
    }

    /// The lines folding `line` hides: everything below it indented deeper,
    /// down to the last such non-blank line. A closing bracket at the
    /// block's own indent stays visible, as do blank lines after the block.
    pub fn fold_range(&self, line: usize) -> Option<(usize, usize)> {
        if !self.can_fold(line) {
            return None;
        }
        let base = self.indent_columns(line)?;
        let mut last = line;
        for l in line + 1..self.rope.len_lines() {
            match self.indent_columns(l) {
                Some(indent) if indent <= base => break,
                Some(_) => last = l,
                None => {}
            }
        }
        (last > line).then_some((line + 1, last))
    }

    /// Folds the block that starts at `line`. The caret, if it was inside,
    /// moves to the end of `line`.
    pub fn fold(&mut self, line: usize) -> bool {
        let Some((a, b)) = self.fold_range(line) else {
            return false;
        };
        // A fold swallows any inside it.
        self.folds.retain(|(x, y)| !(*x >= a && *y <= b));
        let at = self.folds.partition_point(|(x, _)| *x < a);
        self.folds.insert(at, (a, b));
        let caret = self.rope.byte_to_line(self.cursor);
        if (a..=b).contains(&caret) {
            self.cursor = self.line_end(line);
            self.anchor = self.cursor;
            self.extra.clear();
        }
        true
    }

    /// Opens the fold that `line` heads, or the one hiding `line`.
    pub fn unfold(&mut self, line: usize) -> bool {
        let before = self.folds.len();
        self.folds
            .retain(|(a, b)| *a != line + 1 && !(*a <= line && line <= *b));
        before != self.folds.len()
    }

    /// Whether `line` heads a fold that is closed.
    pub fn is_folded_at(&self, line: usize) -> bool {
        self.folds
            .binary_search_by_key(&(line + 1), |(a, _)| *a)
            .is_ok()
    }

    /// Folds every top-level block whose lines are indented deeper. Refused
    /// past [`FOLD_ALL_MAX_LINES`]: reading every line's indent on the main
    /// thread would stall the window for seconds.
    pub fn fold_all(&mut self) -> bool {
        if self.rope.len_lines() > FOLD_ALL_MAX_LINES {
            return false;
        }
        let caret = self.rope.byte_to_line(self.cursor);
        let mut line = 0;
        let total = self.rope.len_lines();
        let mut folds = Vec::new();
        while line < total {
            match self.fold_range(line) {
                Some((a, b)) => {
                    folds.push((a, b));
                    line = b + 1;
                }
                None => line += 1,
            }
        }
        self.folds = folds;
        if self.is_hidden(caret) {
            let at = self.folds.partition_point(|(_, b)| *b < caret);
            let head = self.folds[at].0 - 1;
            self.cursor = self.line_end(head);
            self.anchor = self.cursor;
        }
        true
    }

    /// Where each screen row of `line` starts: one row unless wrapping,
    /// none when the line is folded away.
    pub fn row_starts(&self, line: usize) -> Vec<usize> {
        if !self.folds.is_empty() && self.is_hidden(line) {
            return Vec::new();
        }
        match self.wrap {
            Some(columns) => wrap::row_starts(&self.rope, line, columns),
            None => vec![self.rope.line_to_byte(line)],
        }
    }

    /// The caret's line and its row within that line.
    pub fn cursor_row(&self) -> (usize, usize) {
        let line = self.rope.byte_to_line(self.cursor);
        (line, wrap::row_of(&self.row_starts(line), self.cursor))
    }

    /// Up or down one screen row while wrapping, keeping the goal column
    /// measured from the start of the row.
    fn move_row(&mut self, delta: isize, motion: Motion) {
        let line = self.rope.byte_to_line(self.cursor);
        let starts = self.row_starts(line);
        let row = wrap::row_of(&starts, self.cursor);
        let column = wrap::column_in_row(&self.rope, starts[row], self.cursor);
        let goal = self.goal_column.unwrap_or(column);
        let (target_line, target_row) = self.step_rows((line, row), delta);
        if (target_line, target_row) == (line, row) {
            self.cursor = if delta < 0 { 0 } else { self.rope.len_bytes() };
        } else {
            let starts = if target_line == line {
                starts
            } else {
                self.row_starts(target_line)
            };
            let start = starts[target_row];
            let end = starts
                .get(target_row + 1)
                .copied()
                .unwrap_or_else(|| wrap::line_end(&self.rope, target_line));
            let mut at = wrap::byte_at_column(&self.rope, start, end, goal);
            // The end of a row that continues is the next row's start:
            // stop before it, or the caret would jump down a row.
            if target_row + 1 < starts.len() && at == end {
                at = self.prev_boundary(end).max(start);
            }
            self.cursor = at;
            self.goal_column = Some(goal);
        }
        self.after_move(motion);
    }

    /// `delta` screen rows from `(line, row)`, stopping at either end of the
    /// document.
    pub fn step_rows(
        &self,
        (mut line, mut row): (usize, usize),
        mut delta: isize,
    ) -> (usize, usize) {
        let total = self.rope.len_lines();
        if !self.row_mode() {
            let line = (line as isize + delta).clamp(0, total.saturating_sub(1) as isize);
            return (line as usize, 0);
        }
        // The next line with rows, from `line` in `step` direction.
        let shown = |mut l: isize, step: isize| -> Option<(usize, usize)> {
            while l >= 0 && (l as usize) < total {
                let count = self.row_starts(l as usize).len();
                if count > 0 {
                    return Some((l as usize, count));
                }
                l += step;
            }
            None
        };
        let Some((start, count)) = shown(line as isize, -1).or_else(|| shown(line as isize, 1))
        else {
            return (0, 0);
        };
        if start != line {
            (line, row) = (start, count - 1);
        }
        while delta > 0 {
            let count = self.row_starts(line).len().max(1);
            let left = (count - 1 - row.min(count - 1)) as isize;
            if delta <= left {
                row += delta as usize;
                delta = 0;
            } else if let Some((next, _)) = shown(line as isize + 1, 1) {
                delta -= left + 1;
                line = next;
                row = 0;
            } else {
                row = count - 1;
                delta = 0;
            }
        }
        while delta < 0 {
            if (-delta) as usize <= row {
                row -= (-delta) as usize;
                delta = 0;
            } else if let Some((previous, count)) =
                shown(line as isize - 1, -1).filter(|_| line > 0)
            {
                delta += row as isize + 1;
                line = previous;
                row = count - 1;
            } else {
                row = 0;
                delta = 0;
            }
        }
        (line, row)
    }

    /// The furthest scroll position, with the last row at the bottom of a
    /// view `rows` tall.
    fn max_scroll_row(&self, rows: usize) -> (usize, usize) {
        let last = self.rope.len_lines().saturating_sub(1);
        let end = (last, self.row_starts(last).len().saturating_sub(1));
        // step_rows first finds the last line with rows, if `last` has none.
        let end = self.step_rows(end, 0);
        self.step_rows(end, -(rows.max(1) as isize - 1))
    }

    pub fn move_line_start(&mut self, motion: Motion) {
        let (line, _) = self.cursor_position();
        self.cursor = self.rope.line_to_byte(line);
        self.goal_column = None;
        self.after_move(motion);
    }

    pub fn move_line_end(&mut self, motion: Motion) {
        let (line, _) = self.cursor_position();
        self.cursor = self.line_end(line);
        self.goal_column = None;
        self.after_move(motion);
    }

    pub fn move_buffer_start(&mut self, motion: Motion) {
        self.cursor = 0;
        self.goal_column = None;
        self.after_move(motion);
    }

    pub fn move_buffer_end(&mut self, motion: Motion) {
        self.cursor = self.rope.len_bytes();
        self.goal_column = None;
        self.after_move(motion);
    }

    // ---- scrolling -------------------------------------------------------

    /// Scrolls so the cursor is visible, given a viewport of `rows` lines and
    /// `cols` columns.
    ///
    /// `cols` of zero means "do not track horizontally", which is what the
    /// callers that only know the row count pass.
    pub fn scroll_to_cursor(&mut self, rows: usize, cols: usize) {
        if self.row_mode() {
            if self.wrap.is_some() {
                self.scroll_column = 0;
            }
            let caret = self.cursor_row();
            let top = (self.scroll_line, self.scroll_row);
            if caret < top || (caret == top && self.scroll_fraction > 0.0) {
                (self.scroll_line, self.scroll_row) = caret;
                self.scroll_fraction = 0.0;
            } else if rows > 0 && self.step_rows(top, rows as isize - 1) < caret {
                (self.scroll_line, self.scroll_row) = self.step_rows(caret, -(rows as isize - 1));
                self.scroll_fraction = 0.0;
            }
            return;
        }
        let (line, column) = self.cursor_position();
        // The top line is only partly in view while a fraction is scrolled
        // off, so a caret on it counts as above the view.
        if line < self.scroll_line || (line == self.scroll_line && self.scroll_fraction > 0.0) {
            self.scroll_line = line;
            self.scroll_fraction = 0.0;
        } else if rows > 0 && line >= self.scroll_line + rows {
            self.scroll_line = line + 1 - rows;
            self.scroll_fraction = 0.0;
        }

        if cols == 0 {
            return;
        }
        // A few columns of lead, so the caret is not pinned to the very edge
        // while you type toward it.
        const MARGIN: usize = 4;
        if column < self.scroll_column + MARGIN {
            self.scroll_column = column.saturating_sub(MARGIN);
        } else if column >= self.scroll_column + cols {
            self.scroll_column = column + 1 + MARGIN - cols;
        }
    }

    /// Scrolls horizontally, clamped at the left edge.
    /// Scrolls horizontally, clamped so you cannot drift off into empty
    /// space to the right of the longest visible line.
    ///
    /// The bound is the longest line *in view*, not in the document: finding
    /// the longest line of a 100MB file is a full scan, and the answer would
    /// only be used to stop a gesture.
    pub fn scroll_columns_by(&mut self, columns: isize, rows: usize) {
        if columns == 0 {
            return;
        }
        let longest = self.longest_visible_line(rows);
        let next = self.scroll_column as isize + columns;
        self.scroll_column = next.clamp(0, longest as isize) as usize;
    }

    /// Pulls the scroll position back inside what there is to show.
    ///
    /// Scrolling clamps itself, but the limits move without any scrolling:
    /// the view gets taller, lines are deleted, a vertical scroll leaves the
    /// long line that a horizontal one was measured against. Called every
    /// frame with that frame's size.
    pub fn clamp_scroll(&mut self, rows: usize, cols: usize) {
        if self.row_mode() {
            if self.wrap.is_some() {
                self.scroll_column = 0;
            }
            // A top line folded away: the view starts at the fold's head.
            let mut top = self
                .scroll_line
                .min(self.rope.len_lines().saturating_sub(1));
            while top > 0 && self.row_starts(top).is_empty() {
                top -= 1;
                self.scroll_row = usize::MAX;
            }
            self.scroll_line = top;
            let count = self.row_starts(top).len().max(1);
            self.scroll_row = self.scroll_row.min(count - 1);
            self.scroll_by(0, rows);
            return;
        }
        self.scroll_row = 0;
        self.scroll_by(0, rows);
        if self.scroll_column == 0 {
            return;
        }
        // Keep the end of the longest visible line reachable, and no more:
        // further right than this the whole view is blank.
        let longest = self.longest_visible_line(rows);
        let reach = longest.saturating_sub(cols.saturating_sub(1).min(longest));
        // The caret is allowed to hold the view out there, since typing past
        // the right edge is how it got there.
        let caret = self.cursor_position().1;
        self.scroll_column = self
            .scroll_column
            .min(reach.max(caret.saturating_sub(cols.saturating_sub(1))));
    }

    /// Characters in the longest line of the current viewport.
    fn longest_visible_line(&self, rows: usize) -> usize {
        let total = self.rope.len_lines();
        let first = self.scroll_line.min(total.saturating_sub(1));
        let last = (first + rows.max(1)).min(total);
        (first..last)
            .map(|line| {
                let start = self.rope.line_to_byte(line);
                let end = self.line_end(line);
                self.rope.byte_to_char(end) - self.rope.byte_to_char(start)
            })
            .max()
            .unwrap_or(0)
    }

    /// Scrolls by whole lines. A fraction left by a trackpad stays put, so a
    /// Page Down in the middle of a gesture does not snap the view; at the
    /// last position there is none, since nothing is below to reveal.
    pub fn scroll_by(&mut self, lines: isize, rows: usize) {
        if self.row_mode() {
            let max = self.max_scroll_row(rows);
            let at = self
                .step_rows((self.scroll_line, self.scroll_row), lines)
                .min(max);
            (self.scroll_line, self.scroll_row) = at;
            if at >= max {
                self.scroll_fraction = 0.0;
            }
            return;
        }
        let max = self.rope.len_lines().saturating_sub(rows.max(1));
        let next = self.scroll_line as isize + lines;
        self.scroll_line = next.clamp(0, max as isize) as usize;
        if self.scroll_line >= max {
            self.scroll_fraction = 0.0;
        }
    }

    /// Scrolls by a part of a line, for a trackpad: `lines` is the gesture's
    /// points over the line height. Clamped to the same range as
    /// [`Buffer::scroll_by`].
    pub fn scroll_smooth_by(&mut self, lines: f32, rows: usize) {
        if !lines.is_finite() {
            return;
        }
        if self.row_mode() {
            // Rows here, not lines: a wrapped paragraph scrolls row by row.
            let total = self.scroll_fraction as f64 + lines as f64;
            let whole = total.floor();
            let max = self.max_scroll_row(rows);
            let from = (self.scroll_line, self.scroll_row);
            let to = self.step_rows(from, whole as isize).min(max);
            (self.scroll_line, self.scroll_row) = to;
            self.scroll_fraction = (total - whole) as f32;
            // Stopped short at either end: nothing more to reveal.
            let short_of_top =
                whole < 0.0 && to == (0, 0) && self.step_rows(to, -whole as isize) != from;
            if to >= max || short_of_top {
                self.scroll_fraction = 0.0;
            }
            return;
        }
        let max = self.rope.len_lines().saturating_sub(rows.max(1)) as f64;
        let at =
            (self.scroll_line as f64 + self.scroll_fraction as f64 + lines as f64).clamp(0.0, max);
        self.scroll_line = at.floor() as usize;
        self.scroll_fraction = (at - at.floor()) as f32;
    }

    /// Puts `line` at the top of a view `rows` tall, as far as the text allows.
    pub fn scroll_to(&mut self, line: usize, rows: usize) {
        self.scroll_row = 0;
        if self.row_mode() {
            let max = self.max_scroll_row(rows);
            (self.scroll_line, self.scroll_row) = (line, 0).min(max);
            self.scroll_fraction = 0.0;
            return;
        }
        let max = self.rope.len_lines().saturating_sub(rows.max(1));
        self.scroll_line = line.min(max);
        self.scroll_fraction = 0.0;
    }

    // ---- internals -------------------------------------------------------

    /// Byte offset of `column` characters into `line`, clamped to the line's
    /// end so a short line cannot push the cursor onto the next one.
    fn byte_at(&self, line: usize, column: usize) -> usize {
        let start = self.rope.line_to_byte(line);
        let end = self.line_end(line);
        self.rope
            .char_to_byte(self.rope.byte_to_char(start).saturating_add(column))
            .min(end)
    }

    /// Byte offset of the end of `line`, excluding its newline.
    fn line_end(&self, line: usize) -> usize {
        if line + 1 < self.rope.len_lines() {
            // line_to_byte points past the newline, so step back over it.
            self.rope.line_to_byte(line + 1) - 1
        } else {
            self.rope.len_bytes()
        }
    }

    /// Previous user-perceived character boundary, preserving native grapheme rules.
    fn prev_boundary(&self, at: usize) -> usize {
        if at == 0 {
            return 0;
        }
        // Raw CRLF inserted through the buffer API stays one editing unit.
        if at >= 2
            && self.rope.byte_at(at - 2) == Some(b'\r')
            && self.rope.byte_at(at - 1) == Some(b'\n')
        {
            return at - 2;
        }
        if self.rope.byte_at(at - 1).is_some_and(|b| b.is_ascii())
            && (at == 1 || self.rope.byte_at(at - 2).is_some_and(|b| b.is_ascii()))
        {
            return at - 1;
        }
        let units = self.rope.byte_to_utf16(at);
        let range = super::grapheme::range(&self.rope, units - 1);
        self.rope.utf16_to_byte(range.start)
    }

    fn next_boundary(&self, at: usize) -> usize {
        let len = self.rope.len_bytes();
        if at >= len {
            return len;
        }
        if self.rope.byte_at(at) == Some(b'\r') && self.rope.byte_at(at + 1) == Some(b'\n') {
            return at + 2;
        }
        if self.rope.byte_at(at).is_some_and(|b| b.is_ascii())
            && self.rope.byte_at(at + 1).is_none_or(|b| b.is_ascii())
        {
            return at + 1;
        }
        let range = super::grapheme::range(&self.rope, self.rope.byte_to_utf16(at));
        self.rope.utf16_to_byte(range.end)
    }
}

impl Default for Buffer {
    fn default() -> Self {
        Buffer::new()
    }
}

#[cfg(test)]
mod tests {
    use super::Motion::{Extend, Move};
    use super::*;

    #[test]
    fn motion_and_deletion_match_native_graphemes_at_every_scalar_boundary() {
        use objc2_foundation::NSString;
        let text = format!("{}\u{600}a\r\ne\u{301}🇪🇸👩‍💻क्‍ष\r\n", "é漢 ".repeat(300));
        let mut b = Buffer::from_text(&text);
        let native = NSString::from_str(&text);
        for byte in text.char_indices().map(|(i, _)| i).chain([text.len()]) {
            let utf16 = text[..byte].encode_utf16().count();
            if byte > 0 {
                let range = native.rangeOfComposedCharacterSequenceAtIndex(utf16 - 1);
                let expected = if text[..byte].ends_with("\r\n") {
                    byte - 2
                } else {
                    b.rope.utf16_to_byte(range.location)
                };
                assert_eq!(b.prev_boundary(byte), expected, "prev at {byte}");
            }
            if byte < text.len() {
                let range = native.rangeOfComposedCharacterSequenceAtIndex(utf16);
                let expected = if text[byte..].starts_with("\r\n") {
                    byte + 2
                } else {
                    b.rope.utf16_to_byte(range.location + range.length)
                };
                assert_eq!(b.next_boundary(byte), expected, "next at {byte}");
            }
        }
        b.place_cursor(text.len(), Move);
        b.backspace();
        assert_eq!(b.rope.to_string(), text.trim_end_matches("\r\n"));
        b.undo();
        assert_eq!(b.rope.to_string(), text);
        b.move_left(Move);
        assert_eq!(b.cursor(), text.len() - 2);
        b.delete_forward();
        assert_eq!(b.rope.to_string(), text.trim_end_matches("\r\n"));
    }

    #[test]
    fn long_line_positions_cross_rope_chunks_without_copying_the_line() {
        let text = format!("{}é\t界", "a".repeat(100_000));
        let mut buffer = Buffer::from_text(&text);
        let accent = 100_000;
        assert_eq!(buffer.position_of(accent + "é".len()), (0, 100_001));
        assert_eq!(buffer.offset_at(0, 100_001), accent + "é".len());
        buffer.place_cursor(accent + "é".len(), Move);
        assert_eq!(buffer.input_selection(), (100_001, 0));
        buffer.scroll_columns_by(100_000, 1);
        assert_eq!(buffer.scroll_column, 100_000);
    }

    #[test]
    fn media_files_open_as_read_only_previews() {
        let root = std::env::temp_dir().join(format!("caio-preview-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for name in ["picture.PNG", "scan.jpeg", "drawing.svg", "invoice.pdf"] {
            let path = root.join(name);
            std::fs::write(&path, b"preview bytes").unwrap();
            let mut buffer = Buffer::open(&path).unwrap();
            assert!(buffer.is_preview_file(), "{name}");
            assert!(!buffer.is_dirty());
            buffer.insert("cannot edit");
            assert_eq!(buffer.rope.len_bytes(), 0);
            assert!(!buffer.is_dirty());
            assert!(buffer.save(None).is_err());
        }
        let binary = root.join("unknown.bin");
        std::fs::write(&binary, [0, 0xff]).unwrap();
        assert!(Buffer::open(&binary).unwrap().is_preview_file());
        let text = root.join("notes.txt");
        std::fs::write(&text, b"editable").unwrap();
        assert!(!Buffer::open(&text).unwrap().is_preview_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn typing_advances_the_cursor() {
        let mut b = Buffer::new();
        b.insert("hello");
        assert_eq!(b.cursor(), 5);
        assert_eq!(b.rope.to_string(), "hello");
        assert_eq!(b.cursor_position(), (0, 5));
    }

    #[test]
    fn backspace_removes_whole_characters() {
        let mut b = Buffer::new();
        b.insert("café");
        assert_eq!(b.rope.len_bytes(), 5, "é is two bytes");
        b.backspace();
        assert_eq!(b.rope.to_string(), "caf", "backspace split a character");
        assert_eq!(b.cursor(), 3);
    }

    #[test]
    fn movement_and_delete_use_composed_characters() {
        for cluster in ["e\u{301}", "👍🏽", "👩‍👩‍👧‍👦", "🇪🇸"] {
            let mut b = Buffer::from_text(&format!("a{cluster}b"));
            b.move_right(Move);
            b.move_right(Move);
            assert_eq!(b.cursor(), 1 + cluster.len(), "{cluster}");
            b.backspace();
            assert_eq!(b.rope.to_string(), "ab", "{cluster}");
            assert_eq!(b.cursor(), 1);
        }
    }

    #[test]
    fn horizontal_movement_steps_by_character_not_byte() {
        let mut b = Buffer::from_text("a🌍b");
        b.move_right(Move);
        assert_eq!(b.cursor(), 1);
        b.move_right(Move);
        assert_eq!(
            b.cursor(),
            5,
            "should step over all four bytes of the emoji"
        );
        b.move_left(Move);
        assert_eq!(b.cursor(), 1);
    }

    #[test]
    fn vertical_movement_remembers_the_goal_column() {
        let mut b = Buffer::from_text("long line here\nshort\nlong line again");
        for _ in 0..12 {
            b.move_right(Move);
        }
        assert_eq!(b.cursor_position(), (0, 12));
        b.move_down(Move);
        assert_eq!(
            b.cursor_position(),
            (1, 5),
            "should clamp to the short line"
        );
        b.move_down(Move);
        assert_eq!(b.cursor_position(), (2, 12), "goal column was lost");
    }

    /// Regression: walking boundaries by slicing a fixed 4-byte window panics
    /// whenever the window edge lands inside a character. Forward movement
    /// broke on "a🌍b" and backward movement on "é€".
    #[test]
    fn boundary_walking_survives_adjacent_multibyte_characters() {
        let mut b = Buffer::from_text("a🌍b");
        b.move_right(Move);
        b.move_right(Move);
        b.move_right(Move);
        assert_eq!(b.cursor(), 6);

        let mut b = Buffer::from_text("é€");
        b.move_right(Move);
        b.move_right(Move);
        assert_eq!(b.cursor(), 5);
        b.move_left(Move);
        assert_eq!(b.cursor(), 2, "backward walk landed mid-character");
    }

    #[test]
    fn cursor_cannot_leave_the_buffer() {
        let mut b = Buffer::from_text("ab");
        b.move_left(Move);
        assert_eq!(b.cursor(), 0);
        b.backspace();
        assert_eq!(b.rope.to_string(), "ab");
        for _ in 0..10 {
            b.move_right(Move);
        }
        assert_eq!(b.cursor(), 2);
        b.delete_forward();
        assert_eq!(b.rope.to_string(), "ab");
    }

    // ---- selection -------------------------------------------------------

    #[test]
    fn plain_movement_has_no_selection() {
        let mut b = Buffer::from_text("hello");
        b.move_right(Move);
        b.move_right(Move);
        assert_eq!(b.selection(), None);
        assert_eq!(b.selected_text(), None);
    }

    #[test]
    fn shift_movement_extends_from_the_anchor() {
        let mut b = Buffer::from_text("hello world");
        for _ in 0..5 {
            b.move_right(Extend);
        }
        assert_eq!(b.selection(), Some(0..5));
        assert_eq!(b.selected_text().as_deref(), Some("hello"));
    }

    #[test]
    fn extending_back_to_the_anchor_empties_the_selection() {
        let mut b = Buffer::from_text("hello world");
        for _ in 0..5 {
            b.move_right(Extend);
        }
        // Walking back to (and past, since it clamps at 0) the anchor leaves
        // nothing selected rather than a reversed range.
        for _ in 0..8 {
            b.move_left(Extend);
        }
        assert_eq!(b.cursor(), 0);
        assert_eq!(b.selection(), None);
    }

    #[test]
    fn extending_past_the_anchor_flips_the_range() {
        //            h0 e1 l2 l3 o4 _5 w6 o7 r8 l9 d10
        let mut b = Buffer::from_text("hello world");
        for _ in 0..5 {
            b.move_right(Move);
        }
        assert_eq!(
            b.selection(),
            None,
            "plain movement leaves the anchor with the cursor"
        );

        for _ in 0..3 {
            b.move_right(Extend);
        }
        assert_eq!(b.selected_text().as_deref(), Some(" wo"));

        // Cross back over the anchor: the range should invert, not collapse.
        for _ in 0..6 {
            b.move_left(Extend);
        }
        assert_eq!(b.selection(), Some(2..5));
        assert_eq!(b.selected_text().as_deref(), Some("llo"));
    }

    #[test]
    fn select_all_covers_the_buffer() {
        let mut b = Buffer::from_text("one\ntwo\nthree");
        b.select_all();
        assert_eq!(b.selection(), Some(0..13));
        assert_eq!(b.selected_text().as_deref(), Some("one\ntwo\nthree"));
    }

    #[test]
    fn typing_replaces_the_selection() {
        let mut b = Buffer::from_text("hello world");
        for _ in 0..5 {
            b.move_right(Extend);
        }
        b.insert("goodbye");
        assert_eq!(b.rope.to_string(), "goodbye world");
        assert_eq!(
            b.selection(),
            None,
            "selection should collapse after typing"
        );
    }

    #[test]
    fn backspace_deletes_the_selection_not_one_character() {
        let mut b = Buffer::from_text("hello world");
        for _ in 0..5 {
            b.move_right(Extend);
        }
        b.backspace();
        assert_eq!(b.rope.to_string(), " world");
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn plain_arrow_collapses_a_selection_to_its_edge() {
        let mut b = Buffer::from_text("hello world");
        for _ in 0..5 {
            b.move_right(Extend);
        }
        b.move_left(Move);
        assert_eq!(b.cursor(), 0, "left should collapse to the selection start");

        b.select_all();
        b.move_right(Move);
        assert_eq!(b.cursor(), 11, "right should collapse to the selection end");
    }

    // ---- word motions ----------------------------------------------------

    #[test]
    fn option_backspace_deletes_a_word() {
        let mut b = Buffer::from_text("let counts = HashMap::new");
        b.move_buffer_end(Move);
        b.delete_word_backward();
        assert_eq!(b.rope.to_string(), "let counts = HashMap::");
        b.delete_word_backward();
        assert_eq!(b.rope.to_string(), "let counts = HashMap");
        b.delete_word_backward();
        assert_eq!(b.rope.to_string(), "let counts = ");
    }

    #[test]
    fn option_backspace_skips_trailing_whitespace() {
        let mut b = Buffer::from_text("foo bar   ");
        b.move_buffer_end(Move);
        b.delete_word_backward();
        assert_eq!(
            b.rope.to_string(),
            "foo ",
            "should eat the spaces and bar together"
        );
    }

    #[test]
    fn word_motions_treat_identifiers_as_one_word() {
        let b = Buffer::from_text("foo_bar2 baz");
        // From the end of the identifier, back one word reaches its start.
        assert_eq!(b.prev_word_boundary(8), 0);
        assert_eq!(b.next_word_boundary(0), 8);
    }

    #[test]
    fn word_motions_stop_at_line_ends() {
        let mut b = Buffer::from_text("first line\nsecond");
        b.move_buffer_end(Move);
        b.delete_word_backward();
        assert_eq!(b.rope.to_string(), "first line\n");
        // The next one consumes the newline itself rather than jumping lines.
        b.delete_word_backward();
        assert_eq!(b.rope.to_string(), "first line");
    }

    #[test]
    fn cmd_backspace_clears_to_the_line_start() {
        let mut b = Buffer::from_text("keep\n    indented code here");
        b.move_buffer_end(Move);
        b.delete_to_line_start();
        assert_eq!(b.rope.to_string(), "keep\n");
    }

    #[test]
    fn cmd_delete_clears_to_the_line_end() {
        let mut b = Buffer::from_text("abcdef\nnext");
        for _ in 0..3 {
            b.move_right(Move);
        }
        b.delete_to_line_end();
        assert_eq!(
            b.rope.to_string(),
            "abc\nnext",
            "should not eat the newline"
        );
    }

    #[test]
    fn word_deletions_remove_a_selection_when_there_is_one() {
        let mut b = Buffer::from_text("hello world");
        for _ in 0..5 {
            b.move_right(Extend);
        }
        b.delete_word_backward();
        assert_eq!(b.rope.to_string(), " world");
    }

    #[test]
    fn word_deletions_at_the_edges_do_nothing() {
        let mut b = Buffer::from_text("word");
        b.delete_word_backward();
        assert_eq!(b.rope.to_string(), "word");
        b.move_buffer_end(Move);
        b.delete_word_forward();
        assert_eq!(b.rope.to_string(), "word");
    }

    #[test]
    fn word_motion_handles_multibyte() {
        let mut b = Buffer::from_text("café naïve");
        b.move_buffer_end(Move);
        b.delete_word_backward();
        assert_eq!(b.rope.to_string(), "café ");
    }

    // ---- multiple cursors ------------------------------------------------

    #[test]
    fn a_fresh_buffer_has_one_cursor() {
        let b = Buffer::from_text("x");
        assert_eq!(b.cursor_count(), 1);
    }

    #[test]
    fn typing_inserts_at_every_cursor() {
        let mut b = Buffer::from_text("aa\nbb\ncc\n");
        b.add_cursor(3, 3);
        b.add_cursor(6, 6);
        assert_eq!(b.cursor_count(), 3);
        b.insert("X");
        assert_eq!(b.rope.to_string(), "Xaa\nXbb\nXcc\n");
    }

    #[test]
    fn cursors_stay_correct_as_earlier_edits_shift_later_ones() {
        let mut b = Buffer::from_text("a\na\na\n");
        b.add_cursor(2, 2);
        b.add_cursor(4, 4);
        b.insert("long");
        assert_eq!(b.rope.to_string(), "longa\nlonga\nlonga\n");
        // And each cursor must have landed after its own insertion.
        b.insert("!");
        assert_eq!(b.rope.to_string(), "long!a\nlong!a\nlong!a\n");
    }

    #[test]
    fn backspace_applies_at_every_cursor() {
        let mut b = Buffer::from_text("xa\nxb\nxc\n");
        b.place_cursor(1, Move);
        b.add_cursor(4, 4);
        b.add_cursor(7, 7);
        b.backspace();
        assert_eq!(b.rope.to_string(), "a\nb\nc\n");
    }

    #[test]
    fn overlapping_cursors_are_merged_not_applied_twice() {
        let mut b = Buffer::from_text("abcdef");
        // Two selections that overlap.
        b.anchor = 0;
        b.cursor = 4;
        b.add_cursor(2, 6);
        b.insert("X");
        assert_eq!(
            b.rope.to_string(),
            "X",
            "overlapping selections must collapse into one edit"
        );
    }

    #[test]
    fn a_duplicate_cursor_is_ignored() {
        let mut b = Buffer::from_text("abc");
        b.add_cursor(1, 1);
        b.add_cursor(1, 1);
        assert_eq!(b.cursor_count(), 2);
    }

    #[test]
    fn plain_movement_collapses_to_one_cursor() {
        let mut b = Buffer::from_text("aa\nbb\n");
        b.add_cursor(3, 3);
        assert_eq!(b.cursor_count(), 2);
        b.move_right(Move);
        assert_eq!(b.cursor_count(), 1, "plain movement is how you get back");
    }

    #[test]
    fn escape_collapses_cursors() {
        let mut b = Buffer::from_text("aa\nbb\n");
        b.add_cursor(3, 3);
        assert!(b.collapse_cursors());
        assert_eq!(b.cursor_count(), 1);
        assert!(!b.collapse_cursors(), "nothing left to collapse");
    }

    #[test]
    fn select_next_occurrence_selects_the_word_first() {
        let mut b = Buffer::from_text("total + total");
        b.place_cursor(2, Move);
        assert!(b.select_next_occurrence());
        assert_eq!(b.selected_text().as_deref(), Some("total"));
        assert_eq!(b.cursor_count(), 1, "the first press only selects");
    }

    #[test]
    fn select_next_occurrence_then_adds_cursors() {
        let mut b = Buffer::from_text("total + total + total");
        b.place_cursor(2, Move);
        b.select_next_occurrence(); // selects the first "total"
        assert!(b.select_next_occurrence());
        assert_eq!(b.cursor_count(), 2);
        assert!(b.select_next_occurrence());
        assert_eq!(b.cursor_count(), 3);
        // A fourth press has nowhere left to go.
        assert!(!b.select_next_occurrence());

        b.insert("sum");
        assert_eq!(b.rope.to_string(), "sum + sum + sum");
    }

    // Each of these was a way to leave an extra cursor pointing at text that
    // had moved or gone. The renderer dereferences every cursor on the next
    // frame and the next keystroke edits at it, so each was an abort.

    #[test]
    fn undo_brings_back_the_cursors_that_made_the_edit() {
        let mut b = Buffer::from_text("a\nb");
        b.place_cursor(1, Move);
        b.add_cursor(3, 3);
        b.insert("XXXX");
        assert_eq!(b.rope.to_string(), "aXXXX\nbXXXX");

        b.undo();
        assert_eq!(b.rope.to_string(), "a\nb");
        assert_eq!(b.caret_positions(), vec![1, 3]);
        b.insert("y");
        assert_eq!(b.rope.to_string(), "ay\nby");

        b.undo();
        b.redo();
        assert_eq!(b.rope.to_string(), "ay\nby");
        assert!(b.caret_positions().iter().all(|&c| c <= b.rope.len_bytes()));
    }

    #[test]
    fn backspace_measures_the_character_at_each_cursor() {
        // One byte before the first cursor, two before the second.
        let mut b = Buffer::from_text("a \u{e9}");
        b.place_cursor(1, Move);
        b.add_cursor(4, 4);
        b.backspace();
        assert_eq!(b.rope.to_string(), " ");

        // A primary at offset 0 has nothing to delete. The others still do.
        let mut b = Buffer::from_text("ab");
        b.place_cursor(0, Move);
        b.add_cursor(2, 2);
        b.backspace();
        assert_eq!(b.rope.to_string(), "a");
    }

    #[test]
    fn backspace_deletes_selections_and_characters_together() {
        let mut b = Buffer::from_text("one two");
        b.place_cursor(0, Move);
        b.place_cursor(3, Extend);
        b.add_cursor(7, 7);
        b.backspace();
        assert_eq!(b.rope.to_string(), " tw");
    }

    #[test]
    fn an_edit_at_one_cursor_drops_the_others() {
        type Op = fn(&mut Buffer);
        let ops: [(&str, Op); 9] = [
            ("delete_forward", |b| b.delete_forward()),
            ("delete_word_backward", |b| b.delete_word_backward()),
            ("delete_word_forward", |b| b.delete_word_forward()),
            ("delete_to_line_start", |b| b.delete_to_line_start()),
            ("delete_to_line_end", |b| b.delete_to_line_end()),
            ("indent", |b| b.indent()),
            ("toggle_comment", |b| b.toggle_comment("//")),
            ("duplicate_lines", |b| b.duplicate_lines()),
            ("replace_all", |b| {
                b.replace_all("ab", "a");
            }),
        ];
        for (name, op) in ops {
            let mut b = Buffer::from_text("ab cd\nab cd");
            b.place_cursor(2, Move);
            b.add_cursor(11, 11);
            op(&mut b);
            assert_eq!(b.cursor_count(), 1, "{name} left a cursor behind");
            // The keystroke that used to abort.
            b.insert("z");
            b.undo();
            b.undo();
            assert_eq!(b.rope.to_string(), "ab cd\nab cd", "{name}");
            assert_eq!(b.cursor_count(), 2, "{name}: undo restores both cursors");
        }
    }

    #[test]
    fn a_pair_opens_and_closes_at_every_cursor() {
        let mut b = Buffer::from_text("\n");
        b.place_cursor(0, Move);
        b.add_cursor(1, 1);
        b.insert_char_paired('(');
        b.insert("x");
        assert_eq!(b.rope.to_string(), "(x)\n(x)");

        // Typing the closer steps over it everywhere, keeping both cursors.
        b.insert_char_paired(')');
        assert_eq!(b.rope.to_string(), "(x)\n(x)");
        assert_eq!(b.caret_positions(), vec![3, 7]);

        // And backspace inside empty pairs removes both halves everywhere.
        let mut b = Buffer::from_text("\n");
        b.place_cursor(0, Move);
        b.add_cursor(1, 1);
        b.insert_char_paired('[');
        b.backspace_paired();
        assert_eq!(b.rope.to_string(), "\n");
        b.backspace_paired();
        b.insert("q");
    }

    #[test]
    fn touching_selections_stay_separate() {
        let mut b = Buffer::from_text("abab");
        b.place_cursor(0, Move);
        b.place_cursor(2, Extend);
        assert!(b.select_next_occurrence());
        assert_eq!(b.selections(), vec![(0, 2), (2, 4)]);
        b.insert("x");
        assert_eq!(b.rope.to_string(), "xx");
    }

    #[test]
    fn select_next_occurrence_reaches_everything_after_wrapping() {
        let mut b = Buffer::from_text("x x x");
        b.place_cursor(4, Move);
        b.place_cursor(5, Extend);
        assert!(b.select_next_occurrence(), "wraps to the first");
        assert!(b.select_next_occurrence(), "then takes the middle one");
        assert_eq!(b.selections(), vec![(0, 1), (2, 3), (4, 5)]);
        assert!(!b.select_next_occurrence(), "and then there are none left");
    }

    #[test]
    fn select_next_occurrence_at_the_end_of_a_word_takes_the_word() {
        for text in ["foo\nfoo", "foo(bar)", "foo"] {
            let mut b = Buffer::from_text(text);
            b.place_cursor(3, Move);
            assert!(b.select_next_occurrence());
            assert_eq!(b.selected_text().as_deref(), Some("foo"), "{text:?}");
        }
    }

    #[test]
    fn collapsing_a_selection_or_clicking_returns_to_one_cursor() {
        let mut b = Buffer::from_text("foo foo");
        b.place_cursor(0, Move);
        b.place_cursor(3, Extend);
        b.select_next_occurrence();
        b.move_left(Move);
        assert_eq!(b.cursor_count(), 1);

        let mut b = Buffer::from_text("abc abc");
        b.add_cursor(4, 4);
        b.place_cursor(2, Move);
        assert_eq!(b.cursor_count(), 1);
    }

    #[test]
    fn multi_cursor_edits_force_a_full_reparse() {
        let mut b = Buffer::from_text("a\na\n");
        b.add_cursor(2, 2);
        b.insert("x");
        assert!(
            b.drain_edits().is_none(),
            "a multi-cursor edit is not one tree-sitter edit"
        );
    }

    #[test]
    fn multi_cursor_edits_undo_as_one_step() {
        let mut b = Buffer::from_text("a\na\na\n");
        b.add_cursor(2, 2);
        b.add_cursor(4, 4);
        b.insert("X");
        assert_eq!(b.rope.to_string(), "Xa\nXa\nXa\n");
        b.undo();
        assert_eq!(b.rope.to_string(), "a\na\na\n");
    }

    // ---- brackets and replace --------------------------------------------

    #[test]
    fn typing_an_opener_inserts_the_pair() {
        let mut b = Buffer::new();
        b.insert_char_paired('(');
        assert_eq!(b.rope.to_string(), "()");
        assert_eq!(b.cursor(), 1, "caret should sit between them");
    }

    #[test]
    fn typing_a_closer_steps_over_it() {
        let mut b = Buffer::new();
        b.insert_char_paired('(');
        b.insert_char_paired(')');
        assert_eq!(
            b.rope.to_string(),
            "()",
            "should not insert a second closer"
        );
        assert_eq!(b.cursor(), 2);
    }

    #[test]
    fn an_opener_wraps_a_selection() {
        let mut b = Buffer::from_text("wrapped");
        b.select_all();
        b.insert_char_paired('(');
        assert_eq!(b.rope.to_string(), "(wrapped)");
        assert_eq!(
            b.selected_text().as_deref(),
            Some("wrapped"),
            "the text should stay selected so it can be wrapped again"
        );
    }

    #[test]
    fn no_auto_close_before_a_word() {
        let mut b = Buffer::from_text("value");
        b.move_buffer_start(Move);
        b.insert_char_paired('(');
        assert_eq!(
            b.rope.to_string(),
            "(value",
            "typing ( before a word usually means calling it"
        );
    }

    #[test]
    fn no_auto_close_for_an_apostrophe_after_a_word() {
        let mut b = Buffer::from_text("dont");
        b.move_buffer_end(Move);
        b.insert_char_paired('\'');
        assert_eq!(
            b.rope.to_string(),
            "dont'",
            "that is an apostrophe, not a string"
        );
    }

    #[test]
    fn backspace_removes_an_empty_pair() {
        let mut b = Buffer::new();
        b.insert_char_paired('{');
        assert_eq!(b.rope.to_string(), "{}");
        b.backspace_paired();
        assert_eq!(b.rope.to_string(), "", "both halves should go");
    }

    #[test]
    fn backspace_leaves_a_non_empty_pair_alone() {
        let mut b = Buffer::from_text("{x}");
        for _ in 0..2 {
            b.move_right(Move);
        }
        b.backspace_paired();
        assert_eq!(b.rope.to_string(), "{}", "only the character should go");
    }

    #[test]
    fn replace_all_counts_and_replaces() {
        let mut b = Buffer::from_text("a b a b a");
        assert_eq!(b.replace_all("a", "X"), 3);
        assert_eq!(b.rope.to_string(), "X b X b X");
    }

    #[test]
    fn replace_all_handles_different_lengths() {
        let mut b = Buffer::from_text("aa aa");
        assert_eq!(b.replace_all("aa", "bbbb"), 2);
        assert_eq!(b.rope.to_string(), "bbbb bbbb");
    }

    fn wrapped(text: &str) -> Buffer {
        let mut b = Buffer::from_text(text);
        b.wrap = Some(20);
        b
    }

    #[test]
    fn wrapped_up_and_down_move_by_screen_row() {
        // Rows: "the quick brown fox " | "jumps over the lazy " | "dog", then "end".
        let mut b = wrapped("the quick brown fox jumps over the lazy dog\nend");
        b.place_cursor(4, Move); // "q"
        b.move_down(Move);
        assert_eq!(b.cursor(), 24, "column 4 of the second row");
        b.move_down(Move);
        assert_eq!(b.cursor(), 43, "the short last row clamps to its end");
        b.move_down(Move);
        assert_eq!(
            b.cursor(),
            47,
            "then the next line, still aiming at column 4"
        );
        b.move_up(Move);
        b.move_up(Move);
        assert_eq!(b.cursor(), 24);
        b.move_up(Move);
        assert_eq!(b.cursor(), 4);
    }

    #[test]
    fn wrapped_caret_at_a_row_end_stays_on_its_row() {
        let mut b = wrapped("the quick brown fox jumps over the lazy dog");
        b.place_cursor(40, Move); // "dog" row
        b.move_up(Move);
        // Column 0 of "jumps over the lazy ", not the break byte 40.
        assert_eq!(b.cursor(), 20);
        b.place_cursor(39, Move); // end of row two, just before the break
        b.move_up(Move);
        assert!(
            b.cursor() < 20,
            "stays on the first row, got {}",
            b.cursor()
        );
    }

    #[test]
    fn wrapped_scrolling_counts_rows_not_lines() {
        let text = "the quick brown fox jumps over the lazy dog\n".repeat(10);
        let mut b = wrapped(&text);
        b.scroll_by(4, 5);
        assert_eq!((b.scroll_line, b.scroll_row), (1, 1));
        b.scroll_smooth_by(1.5, 5);
        assert_eq!((b.scroll_line, b.scroll_row), (1, 2));
        assert!((b.scroll_fraction - 0.5).abs() < 1e-6);
        b.scroll_smooth_by(-9.0, 5);
        assert_eq!(
            (b.scroll_line, b.scroll_row, b.scroll_fraction),
            (0, 0, 0.0)
        );
        // 31 rows in all (the last line is empty): the top can go to row 26.
        b.scroll_by(1000, 5);
        assert_eq!((b.scroll_line, b.scroll_row), (8, 2));
    }

    #[test]
    fn wrapped_scroll_to_cursor_brings_the_row_into_view() {
        let text = "the quick brown fox jumps over the lazy dog\n".repeat(10);
        let mut b = wrapped(&text);
        let at = text.len() - 3; // "dog" on the last text line
        b.place_cursor(at, Move);
        b.scroll_to_cursor(4, 80);
        assert_eq!(b.cursor_row(), (9, 2));
        // Rows 8.2, 9.0, 9.1, 9.2: the caret's row is the bottom one.
        assert_eq!((b.scroll_line, b.scroll_row), (8, 2));
        b.place_cursor(0, Move);
        b.scroll_to_cursor(4, 80);
        assert_eq!((b.scroll_line, b.scroll_row), (0, 0));
    }

    const BLOCKS: &str =
        "fn a() {\n    one;\n    two;\n}\n\ndef b():\n    x = 1\n\n    return x\nend\n";

    #[test]
    fn fold_ranges_follow_indentation() {
        let b = Buffer::from_text(BLOCKS);
        // Braces: the closing line stays visible.
        assert_eq!(b.fold_range(0), Some((1, 2)));
        // Indentation: a blank line inside the block is part of it.
        assert_eq!(b.fold_range(5), Some((6, 8)));
        assert_eq!(b.fold_range(1), None, "nothing deeper follows");
        assert_eq!(b.fold_range(4), None, "blank lines do not fold");
    }

    #[test]
    fn folded_lines_are_skipped_by_motion_and_rows() {
        let mut b = Buffer::from_text(BLOCKS);
        b.place_cursor(0, Move);
        assert!(b.fold(0));
        assert!(b.is_folded_at(0) && b.is_hidden(1) && b.is_hidden(2) && !b.is_hidden(3));
        assert!(b.row_starts(1).is_empty());
        b.move_down(Move);
        assert_eq!(
            b.cursor_position().0,
            3,
            "down from the fold's head lands after it"
        );
        b.move_up(Move);
        assert_eq!(b.cursor_position().0, 0);
        assert!(b.unfold(0));
        assert!(b.folds.is_empty());
    }

    #[test]
    fn folding_around_the_caret_moves_it_to_the_head() {
        let mut b = Buffer::from_text(BLOCKS);
        b.place_cursor(b.rope.line_to_byte(2) + 4, Move);
        assert!(b.fold(0));
        assert_eq!(b.cursor_position(), (0, 8));
    }

    #[test]
    fn folds_move_with_edits_above_and_open_when_edited() {
        let mut b = Buffer::from_text(BLOCKS);
        b.fold(5);
        assert_eq!(b.folds, [(6, 8)]);
        b.place_cursor(0, Move);
        b.insert("// top\n");
        assert_eq!(b.folds, [(7, 9)], "a line inserted above shifts the fold");
        b.place_cursor(b.rope.line_to_byte(8), Move);
        b.insert("x");
        assert!(b.folds.is_empty(), "an edit inside the fold opens it");
        b.fold(6);
        assert!(!b.folds.is_empty());
        b.undo();
        assert!(b.folds.is_empty(), "undo opens folds");
    }

    #[test]
    fn fold_all_folds_top_level_blocks() {
        let mut b = Buffer::from_text(BLOCKS);
        b.fold_all();
        assert_eq!(b.folds, [(1, 2), (6, 8)]);
    }

    #[test]
    fn replace_ranges_inserts_and_keeps_the_caret_in_place() {
        let mut b = Buffer::from_text("fn a(){x}");
        b.place_cursor(8, Motion::Move); // before `}`
        let n = b.replace_ranges(&[
            (4..4, "".into()),
            (6..6, " ".into()),
            (7..8, "\n    x\n".into()),
        ]);
        assert_eq!(n, 3);
        assert_eq!(b.rope.to_string(), "fn a() {\n    x\n}");
        assert_eq!(&b.rope.to_string()[b.cursor()..], "}");
        assert!(b.undo());
        assert_eq!(b.rope.to_string(), "fn a(){x}");
    }

    #[test]
    fn replace_all_is_one_undo_step() {
        let mut b = Buffer::from_text("x x x");
        b.replace_all("x", "y");
        assert_eq!(b.rope.to_string(), "y y y");
        b.undo();
        assert_eq!(
            b.rope.to_string(),
            "x x x",
            "a bad replace-all must be recoverable"
        );
    }

    #[test]
    fn ranged_replacements_with_different_lengths_undo_together() {
        let mut b = Buffer::from_text("ab12 cd34");
        assert_eq!(
            b.replace_ranges(&[(0..4, "12-ab".into()), (5..9, "34-cd".into())]),
            2
        );
        assert_eq!(b.rope.to_string(), "12-ab 34-cd");
        b.undo();
        assert_eq!(b.rope.to_string(), "ab12 cd34");
    }

    #[test]
    fn replace_all_on_no_matches_changes_nothing() {
        let mut b = Buffer::from_text("abc");
        assert_eq!(b.replace_all("zzz", "y"), 0);
        assert_eq!(b.rope.to_string(), "abc");
    }

    #[test]
    fn replace_current_only_fires_on_a_matching_selection() {
        let mut b = Buffer::from_text("find me");
        b.move_buffer_start(Move);
        for _ in 0..4 {
            b.move_right(Extend);
        }
        assert!(b.replace_current("find", "got"));
        assert_eq!(b.rope.to_string(), "got me");
        assert!(
            !b.replace_current("find", "x"),
            "selection no longer matches"
        );
    }

    // ---- line operations -------------------------------------------------

    #[test]
    fn enter_carries_the_current_indentation() {
        let mut b = Buffer::from_text("    let x = 1;");
        b.move_buffer_end(Move);
        b.insert_newline_indented();
        assert_eq!(b.rope.to_string(), "    let x = 1;\n    ");
    }

    #[test]
    fn enter_after_an_opening_brace_adds_a_level() {
        let mut b = Buffer::from_text("fn main() {");
        b.move_buffer_end(Move);
        b.insert_newline_indented();
        assert_eq!(b.rope.to_string(), "fn main() {\n    ");
    }

    #[test]
    fn enter_between_braces_puts_the_closer_on_its_own_line() {
        let mut b = Buffer::from_text("fn main() {}");
        // Between the braces.
        for _ in 0..11 {
            b.move_right(Move);
        }
        b.insert_newline_indented();
        assert_eq!(b.rope.to_string(), "fn main() {\n    \n}");
        // And the caret lands on the blank middle line, not after the closer.
        assert_eq!(b.cursor_position(), (1, 4));
    }

    #[test]
    fn enter_respects_tab_indentation() {
        let mut b = Buffer::from_text("\tif x {");
        b.move_buffer_end(Move);
        b.insert_newline_indented();
        assert_eq!(
            b.rope.to_string(),
            "\tif x {\n\t\t",
            "should indent with tabs"
        );
    }

    #[test]
    fn enter_mid_line_does_not_add_a_level() {
        let mut b = Buffer::from_text("    { done }");
        b.move_line_end(Move);
        b.insert_newline_indented();
        assert_eq!(b.rope.to_string(), "    { done }\n    ");
    }

    #[test]
    fn indent_and_outdent_a_selection() {
        let mut b = Buffer::from_text("one\ntwo\nthree\n");
        b.select_all();
        b.indent();
        assert_eq!(b.rope.to_string(), "    one\n    two\n    three\n");
        b.select_all();
        b.outdent();
        assert_eq!(b.rope.to_string(), "one\ntwo\nthree\n");
    }

    #[test]
    fn outdent_handles_partial_indentation() {
        let mut b = Buffer::from_text("  two spaces\n");
        b.outdent();
        assert_eq!(
            b.rope.to_string(),
            "two spaces\n",
            "should not refuse a short indent"
        );
        b.outdent();
        assert_eq!(
            b.rope.to_string(),
            "two spaces\n",
            "and should stop at zero"
        );
    }

    #[test]
    fn a_selection_ending_at_a_line_start_does_not_include_that_line() {
        let mut b = Buffer::from_text("one\ntwo\nthree\n");
        // Select "one\n" exactly.
        for _ in 0..4 {
            b.move_right(Extend);
        }
        b.indent();
        assert_eq!(
            b.rope.to_string(),
            "    one\ntwo\nthree\n",
            "dragging to the next line start should indent one line, not two"
        );
    }

    #[test]
    fn comment_toggles_as_a_block() {
        let mut b = Buffer::from_text("let a = 1;\nlet b = 2;\n");
        b.select_all();
        b.toggle_comment("//");
        assert_eq!(b.rope.to_string(), "// let a = 1;\n// let b = 2;\n");
        b.select_all();
        b.toggle_comment("//");
        assert_eq!(b.rope.to_string(), "let a = 1;\nlet b = 2;\n");
    }

    #[test]
    fn a_partly_commented_block_becomes_fully_commented() {
        let mut b = Buffer::from_text("// already\nnot yet\n");
        b.select_all();
        b.toggle_comment("//");
        assert_eq!(
            b.rope.to_string(),
            "// // already\n// not yet\n",
            "any uncommented line means the whole block gets commented"
        );
    }

    #[test]
    fn comment_preserves_indentation() {
        let mut b = Buffer::from_text("    indented\n");
        b.toggle_comment("//");
        assert_eq!(b.rope.to_string(), "    // indented\n");
    }

    #[test]
    fn comment_skips_blank_lines() {
        let mut b = Buffer::from_text("a\n\nb\n");
        b.select_all();
        b.toggle_comment("//");
        assert_eq!(b.rope.to_string(), "// a\n\n// b\n");
    }

    #[test]
    fn duplicate_copies_the_line_below() {
        let mut b = Buffer::from_text("first\nsecond\n");
        b.duplicate_lines();
        assert_eq!(b.rope.to_string(), "first\nfirst\nsecond\n");
        // Repeating stacks, because the caret follows the copy.
        b.duplicate_lines();
        assert_eq!(b.rope.to_string(), "first\nfirst\nfirst\nsecond\n");
    }

    #[test]
    fn duplicate_works_on_a_last_line_without_a_newline() {
        let mut b = Buffer::from_text("only");
        b.duplicate_lines();
        // The file had no trailing newline, and duplicating a line is not a
        // reason to silently give it one.
        assert_eq!(b.rope.to_string(), "only\nonly");
    }

    #[test]
    fn move_line_down_and_back_is_identity() {
        let mut b = Buffer::from_text("a\nb\nc\n");
        b.move_lines(true);
        assert_eq!(b.rope.to_string(), "b\na\nc\n");
        b.move_lines(false);
        assert_eq!(b.rope.to_string(), "a\nb\nc\n");
    }

    #[test]
    fn move_line_carries_the_cursor() {
        let mut b = Buffer::from_text("a\nb\nc\n");
        b.goto_line(1); // on "b"
        b.move_lines(true);
        assert_eq!(b.rope.to_string(), "a\nc\nb\n");
        assert_eq!(
            b.cursor_position().0,
            2,
            "cursor should follow the moved line"
        );
    }

    #[test]
    fn move_line_refuses_past_the_edges() {
        let mut b = Buffer::from_text("a\nb\n");
        b.goto_line(0);
        b.move_lines(false);
        assert_eq!(
            b.rope.to_string(),
            "a\nb\n",
            "cannot move the first line up"
        );
        b.goto_line(1);
        b.move_lines(true);
        // Line 2 is the empty line after the trailing newline.
        assert!(b.rope.to_string().starts_with('a') || b.rope.to_string().starts_with('b'));
    }

    #[test]
    fn move_line_survives_a_missing_trailing_newline() {
        let mut b = Buffer::from_text("a\nb");
        b.goto_line(0);
        b.move_lines(true);
        assert_eq!(
            b.rope.to_string(),
            "b\na",
            "must not gain or lose a newline"
        );
    }

    #[test]
    fn goto_line_clamps() {
        let mut b = Buffer::from_text("a\nb\nc\n");
        b.goto_line(1);
        assert_eq!(b.cursor_position(), (1, 0));
        b.goto_line(9999);
        assert_eq!(b.cursor_position().0, b.rope.len_lines() - 1);
    }

    #[test]
    fn line_operations_are_undoable_in_one_step() {
        let mut b = Buffer::from_text("one\ntwo\n");
        b.select_all();
        b.indent();
        assert_eq!(b.rope.to_string(), "    one\n    two\n");
        b.undo();
        assert_eq!(
            b.rope.to_string(),
            "one\ntwo\n",
            "indent should undo as one step"
        );
    }

    // ---- undo ------------------------------------------------------------

    #[test]
    fn undo_restores_the_previous_state() {
        let mut b = Buffer::from_text("start");
        b.move_buffer_end(Move);
        b.insert(" more");
        assert_eq!(b.rope.to_string(), "start more");

        assert!(b.undo());
        assert_eq!(b.rope.to_string(), "start");
        assert_eq!(b.cursor(), 5, "cursor should come back too");

        assert!(b.redo());
        assert_eq!(b.rope.to_string(), "start more");
    }

    #[test]
    fn undo_on_an_empty_history_is_a_no_op() {
        let mut b = Buffer::from_text("x");
        assert!(!b.undo());
        assert!(!b.redo());
        assert_eq!(b.rope.to_string(), "x");
    }

    #[test]
    fn a_new_edit_clears_the_redo_stack() {
        let mut b = Buffer::from_text("");
        b.insert("a");
        b.undo();
        b.insert("b");
        assert!(!b.redo(), "redo should be gone after a divergent edit");
        assert_eq!(b.rope.to_string(), "b");
    }

    #[test]
    fn rapid_typing_coalesces_into_one_undo_step() {
        let mut b = Buffer::from_text("");
        for c in "hello".chars() {
            b.insert(&c.to_string());
        }
        assert_eq!(b.rope.to_string(), "hello");
        // All five inserts happened well inside the coalesce window, so one
        // undo should take the whole run.
        assert!(b.undo());
        assert_eq!(
            b.rope.to_string(),
            "",
            "undo stepped one character at a time"
        );
    }

    #[test]
    fn inserts_and_deletes_are_separate_undo_steps() {
        let mut b = Buffer::from_text("");
        b.insert("abc");
        b.backspace();
        assert_eq!(b.rope.to_string(), "ab");
        b.undo();
        assert_eq!(b.rope.to_string(), "abc", "delete should undo on its own");
        b.undo();
        assert_eq!(b.rope.to_string(), "");
    }

    #[test]
    fn movement_breaks_a_coalescing_run() {
        let mut b = Buffer::from_text("");
        b.insert("ab");
        b.move_left(Move);
        b.insert("X");
        assert_eq!(b.rope.to_string(), "aXb");
        b.undo();
        assert_eq!(
            b.rope.to_string(),
            "ab",
            "the insert after a move should be its own step"
        );
    }

    // ---- files -----------------------------------------------------------

    #[test]
    fn dirty_tracks_unsaved_edits() {
        let mut b = Buffer::from_text("x");
        assert!(!b.is_dirty());
        b.insert("y");
        assert!(b.is_dirty());
    }

    #[test]
    fn save_without_a_path_is_an_error_not_a_panic() {
        let mut b = Buffer::from_text("x");
        assert!(b.save(None).is_err());
    }

    #[test]
    fn open_canonicalises_the_path() {
        let dir = std::env::temp_dir().join("caio-canon-tests");
        std::fs::create_dir_all(dir.join("sub")).expect("mkdir");
        let real = dir.join("sub/file.txt");
        std::fs::write(&real, "x").expect("write");

        // The same file by an awkward route.
        let messy = dir.join("sub/../sub/./file.txt");
        let a = Buffer::open(&real).expect("open");
        let b = Buffer::open(&messy).expect("open");
        assert_eq!(
            a.path, b.path,
            "two routes to one file must produce one identity, or they open two tabs"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn round_trips_through_a_file() {
        let dir = std::env::temp_dir().join("crc-tests");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("roundtrip.txt");

        let mut b = Buffer::from_text("hello\nfile\n");
        b.save(Some(&path)).expect("save");
        assert!(!b.is_dirty(), "saving should clear the dirty flag");

        let reopened = Buffer::open(&path).expect("open");
        assert_eq!(reopened.rope.to_string(), "hello\nfile\n");
        // Canonical, not as-given: on macOS /var is a symlink to /private/var.
        let canonical = std::fs::canonicalize(&path).expect("canonicalize");
        assert_eq!(reopened.path.as_deref(), Some(canonical.as_path()));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn opened_crlf_file_edits_with_lf_and_saves_crlf() {
        let dir = std::env::temp_dir().join(format!(
            "caio-crlf-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("notes.txt");
        std::fs::write(&path, b"one\r\ntwo\r\n").unwrap();
        let mut buffer = Buffer::open(&path).unwrap();
        assert_eq!(buffer.rope.to_string(), "one\ntwo\n");
        buffer.place_cursor(3, Move);
        buffer.insert_newline_indented();
        buffer.insert("new");
        buffer.save(None).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"one\r\nnew\r\ntwo\r\n");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn utf16_and_legacy_save_keep_their_encoding() {
        let dir = std::env::temp_dir().join(format!(
            "caio-enc-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let utf16 = dir.join("utf16.txt");
        std::fs::write(&utf16, b"\xff\xfeA\0\r\0\n\0").unwrap();
        let mut buffer = Buffer::open(&utf16).unwrap();
        assert_eq!(buffer.rope.to_string(), "A\n");
        buffer.insert("B");
        buffer.save(None).unwrap();
        assert_eq!(std::fs::read(&utf16).unwrap(), b"\xff\xfeB\0A\0\r\0\n\0");
        let legacy = dir.join("legacy.txt");
        std::fs::write(&legacy, b"caf\xe9\r\n").unwrap();
        let mut buffer = Buffer::open(&legacy).unwrap();
        assert_eq!(buffer.rope.to_string(), "café\n");
        buffer.insert("X");
        buffer.save(None).unwrap();
        assert_eq!(std::fs::read(&legacy).unwrap(), b"Xcaf\xe9\r\n");
        buffer.insert("😀");
        assert_eq!(
            buffer.save(None).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        assert_eq!(std::fs::read(&legacy).unwrap(), b"Xcaf\xe9\r\n");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_preserves_mode_links_and_symlink_destination() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = std::env::temp_dir().join(format!(
            "caio-save-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).expect("mkdir");
        let path = dir.join("script.sh");
        std::fs::write(&path, "old").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let link = dir.join("shortcut");
        symlink(&path, &link).expect("symlink");
        let mut buffer = Buffer::open(&link).expect("open link");
        buffer.insert("new");
        buffer.save(None).expect("save");
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("link")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "newold");

        let sibling = dir.join("second-name");
        std::fs::hard_link(&path, &sibling).expect("hard link");
        buffer.insert("!");
        buffer.save(None).expect("save linked file");
        assert_eq!(
            std::fs::read_to_string(&sibling).expect("read sibling"),
            "new!old"
        );
        assert_eq!(
            std::fs::metadata(&path).expect("stat").ino(),
            std::fs::metadata(&sibling).expect("stat").ino()
        );
        let mut other = Buffer::from_text("saved as a link");
        other.save(Some(&link)).expect("save as symlink");
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("link")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(&sibling).expect("read sibling"),
            "saved as a link"
        );
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn save_rejects_external_changes_and_keeps_the_buffer_dirty() {
        let dir = std::env::temp_dir().join(format!(
            "caio-conflict-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).expect("mkdir");
        let path = dir.join("notes.txt");
        std::fs::write(&path, "old").expect("write");
        let mut buffer = Buffer::open(&path).expect("open");
        buffer.insert("mine");
        std::fs::write(&path, "theirs").expect("external write");
        assert_eq!(
            buffer.save(None).expect_err("conflict").kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert!(buffer.is_dirty());
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "theirs");
        // The user saw the conflict and chose their copy.
        buffer.save_overwriting(None).expect("overwrite");
        assert!(!buffer.is_dirty());
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "mineold");
        assert_eq!(buffer.disk_state(), DiskState::Unchanged);
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "caio-{tag}-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).expect("mkdir");
        dir
    }

    /// A file written by something else makes the buffer say so, and the
    /// buffer's own save does not.
    #[test]
    fn disk_state_sees_other_writers_but_not_its_own() {
        let dir = scratch_dir("stamp");
        let path = dir.join("notes.txt");
        std::fs::write(&path, "one\n").expect("write");
        let mut buffer = Buffer::open(&path).expect("open");
        assert_eq!(buffer.disk_state(), DiskState::Unchanged);

        buffer.insert("x");
        buffer.save(None).expect("save");
        assert_eq!(
            buffer.disk_state(),
            DiskState::Unchanged,
            "own save is not a change"
        );

        // Same length, so only the modification time can tell. Push it into
        // the future rather than sleeping past the timestamp resolution.
        std::fs::write(&path, "yone\n").expect("external write");
        let file = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open");
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(2)),
        )
        .expect("touch");
        assert_eq!(buffer.disk_state(), DiskState::Changed);

        std::fs::remove_file(&path).expect("rm");
        assert_eq!(buffer.disk_state(), DiskState::Missing);
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn reload_takes_the_disk_text_and_keeps_the_old_one_under_undo() {
        let dir = scratch_dir("reload");
        let path = dir.join("notes.txt");
        std::fs::write(&path, "alpha\nbeta\ngamma\n").expect("write");
        let mut buffer = Buffer::open(&path).expect("open");
        buffer.place_cursor(17, Motion::Move);
        assert_eq!(buffer.cursor(), 17);

        std::fs::write(&path, "alpha\n").expect("external write");
        buffer.reload().expect("reload");
        assert_eq!(buffer.rope.to_string(), "alpha\n");
        assert!(!buffer.is_dirty());
        assert_eq!(buffer.disk_state(), DiskState::Unchanged);
        assert!(
            buffer.cursor() <= buffer.rope.len_bytes(),
            "caret clamped to the shorter text"
        );

        assert!(buffer.undo(), "the reload is one undo step");
        assert_eq!(buffer.rope.to_string(), "alpha\nbeta\ngamma\n");
        assert!(buffer.is_dirty(), "the old text is now an unsaved change");
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn a_deleted_file_makes_the_buffer_dirty_and_save_recreates_it() {
        let dir = scratch_dir("missing");
        let path = dir.join("notes.txt");
        std::fs::write(&path, "keep me").expect("write");
        let mut buffer = Buffer::open(&path).expect("open");
        std::fs::remove_file(&path).expect("rm");
        assert_eq!(buffer.disk_state(), DiskState::Missing);
        buffer.note_missing_on_disk();
        assert!(buffer.is_dirty());
        assert_eq!(
            buffer.save(None).expect_err("gone").kind(),
            std::io::ErrorKind::NotFound
        );
        buffer.save_overwriting(None).expect("recreate");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "keep me");
        assert!(!buffer.is_dirty());
        assert_eq!(buffer.disk_state(), DiskState::Unchanged);
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn horizontal_scroll_stops_at_the_longest_visible_line() {
        let mut b = Buffer::from_text("short\nalso short\n");
        b.scroll_columns_by(500, 10);
        assert_eq!(
            b.scroll_column, 10,
            "should stop at the longest line in view, not drift into nothing"
        );
        b.scroll_columns_by(-500, 10);
        assert_eq!(b.scroll_column, 0);
    }

    /// The "scrolled past the end" that would not reproduce. The clamp was
    /// correct at every size it was probed at, because it only ran while
    /// scrolling: what it missed was the limit moving with no scroll at all.
    #[test]
    fn the_view_cannot_be_left_past_the_end_by_growing_or_by_deleting() {
        let text: String = (0..100).map(|n| format!("line {n}\n")).collect();
        let mut b = Buffer::from_text(&text);
        let lines = b.rope.len_lines();

        // Scroll to the bottom of a 20-row view, then make it 50 rows tall.
        b.scroll_by(1000, 20);
        assert_eq!(b.scroll_line, lines - 20);
        b.clamp_scroll(50, 80);
        assert_eq!(
            b.scroll_line,
            lines - 50,
            "no blank rows under the last line"
        );

        // At the bottom again, delete most of the file from under the view.
        b.scroll_by(1000, 20);
        b.select_all();
        b.insert("one\ntwo\n");
        b.clamp_scroll(20, 80);
        assert_eq!(b.scroll_line, 0);
    }

    #[test]
    fn a_file_past_the_limit_opens_read_only_and_refuses_every_edit() {
        let dir = std::env::temp_dir().join(format!("crc-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.txt");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();

        let mut small = Buffer::open_with_limits(path.clone(), 100, 1000).unwrap();
        assert!(!small.is_read_only());
        small.insert("x");
        assert!(small.is_dirty());

        let mut big = Buffer::open_with_limits(path.clone(), 4, 1000).unwrap();
        assert!(big.is_read_only());
        let before = big.rope.to_string();
        big.insert("x");
        big.backspace();
        big.select_all();
        big.delete_forward();
        big.indent();
        big.toggle_comment("//");
        big.duplicate_lines();
        assert_eq!(big.replace_all("one", "1"), 0);
        assert!(!big.undo());
        assert_eq!(big.rope.to_string(), before);
        assert!(!big.is_dirty());

        let refused = Buffer::open_with_limits(path.clone(), 4, 8)
            .err()
            .expect("over the cap");
        assert_eq!(refused.kind(), std::io::ErrorKind::FileTooLarge);
        assert!(refused.to_string().contains("big.txt"), "{refused}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The real cap, on a sparse file: refused from its size alone, so the
    /// test reads nothing and costs nothing.
    #[test]
    fn a_file_past_two_gigabytes_is_refused_before_it_is_read() {
        let dir = std::env::temp_dir().join(format!("crc-huge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("huge.bin.txt");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_OPEN_BYTES + 1)
            .unwrap();
        let refused = Buffer::open(&path).err().expect("refused");
        assert_eq!(refused.kind(), std::io::ErrorKind::FileTooLarge);
        assert!(refused.to_string().contains("2.0 GB"), "{refused}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sizes_read_the_way_people_say_them() {
        assert_eq!(human_size(READ_ONLY_BYTES), "512 MB");
        assert_eq!(human_size(MAX_OPEN_BYTES), "2.0 GB");
        assert_eq!(human_size(1), "1 MB");
    }

    #[test]
    fn a_trackpad_scrolls_by_parts_of_a_line_and_stops_at_both_ends() {
        let text: String = (0..100).map(|n| format!("line {n}\n")).collect();
        let mut b = Buffer::from_text(&text);
        let max = b.rope.len_lines() - 20;

        b.scroll_smooth_by(0.25, 20);
        b.scroll_smooth_by(0.5, 20);
        assert_eq!((b.scroll_line, b.scroll_fraction), (0, 0.75));
        b.scroll_smooth_by(0.5, 20);
        assert_eq!(b.scroll_line, 1);
        assert!((b.scroll_fraction - 0.25).abs() < 1e-6);

        b.scroll_smooth_by(-5.0, 20);
        assert_eq!((b.scroll_line, b.scroll_fraction), (0, 0.0));
        b.scroll_smooth_by(1000.5, 20);
        assert_eq!(
            (b.scroll_line, b.scroll_fraction),
            (max, 0.0),
            "nothing below the end"
        );

        // Whole-line scrolling keeps the fraction, except at the end.
        b.scroll_to(10, 20);
        b.scroll_smooth_by(0.5, 20);
        b.scroll_by(3, 20);
        assert_eq!((b.scroll_line, b.scroll_fraction), (13, 0.5));
        b.scroll_by(1000, 20);
        assert_eq!((b.scroll_line, b.scroll_fraction), (max, 0.0));
    }

    #[test]
    fn a_caret_on_a_partly_hidden_top_line_brings_it_fully_into_view() {
        let text: String = (0..100).map(|n| format!("line {n}\n")).collect();
        let mut b = Buffer::from_text(&text);
        b.scroll_smooth_by(10.5, 20);
        b.goto_line(12);
        b.scroll_to_cursor(20, 80);
        assert_eq!(
            (b.scroll_line, b.scroll_fraction),
            (10, 0.5),
            "caret already in view"
        );
        b.goto_line(10);
        b.scroll_to_cursor(20, 80);
        assert_eq!((b.scroll_line, b.scroll_fraction), (10, 0.0));
    }

    #[test]
    fn scrolling_down_off_a_long_line_does_not_leave_a_blank_view() {
        let mut text = format!("{}\n", "x".repeat(300));
        text.push_str(&"short\n".repeat(50));
        let mut b = Buffer::from_text(&text);

        b.scroll_columns_by(250, 10);
        assert_eq!(b.scroll_column, 250);
        // Down past the long line: every visible line is now 5 characters.
        b.scroll_by(20, 10);
        b.clamp_scroll(10, 80);
        assert_eq!(
            b.scroll_column, 0,
            "250 columns right of `short` is nothing at all"
        );

        // On the long line itself the limit is its end at the right edge,
        // not its end at the left edge with an empty view after it.
        b.scroll_by(-1000, 10);
        b.scroll_columns_by(1000, 10);
        b.clamp_scroll(10, 80);
        assert_eq!(b.scroll_column, 300 - 79);
    }

    #[test]
    fn input_ranges_are_utf16_from_the_start_of_the_line() {
        // An astral character is two UTF-16 units and four bytes.
        let mut b = Buffer::from_text("first\n\u{1f600}caf\u{e9} x\n");
        let line = "first\n".len();
        b.place_cursor(line + "\u{1f600}caf\u{e9}".len(), Move);
        assert_eq!(b.input_selection(), (2 + 4, 0));

        // Press and hold on the `e` that was just typed: replace one unit
        // before the caret.
        b.select_input_range(5, 1);
        assert_eq!(b.selected_text().as_deref(), Some("\u{e9}"));
        assert_eq!(b.input_selection(), (5, 1));
        b.insert("\u{ea}");
        assert_eq!(b.rope.line(1), "\u{1f600}caf\u{ea} x\n");

        // The emoji itself, and a range past the end of the line.
        b.select_input_range(0, 2);
        assert_eq!(b.selected_text().as_deref(), Some("\u{1f600}"));
        b.select_input_range(100, 5);
        assert_eq!(b.selection(), None);
        assert_eq!(b.cursor_position().0, 1, "clamped to the caret's own line");
    }

    #[test]
    fn a_double_click_takes_the_run_it_lands_in() {
        let b = Buffer::from_text("let caf\u{e9}_x = foo(bar);\nnext");
        let text = b.rope.to_string();
        let word = |at: usize| text[b.word_range_at(at)].to_string();
        assert_eq!(word(5), "caf\u{e9}_x", "inside a word, accents and all");
        assert_eq!(word(4), "caf\u{e9}_x", "at its first character");
        assert_eq!(word(3), " ", "on a space, the run of spaces");
        let paren = text.find('(').expect("paren");
        assert_eq!(word(paren), "(", "punctuation is its own run");
        let end = text.find('\n').expect("newline");
        assert_eq!(word(end), ");", "at the end of a line, what is to the left");
        assert_eq!(b.word_range_at(usize::MAX), text.len() - 4..text.len());
        assert_eq!(Buffer::new().word_range_at(0), 0..0);
    }

    #[test]
    fn a_triple_click_takes_the_line_and_its_ending() {
        let mut b = Buffer::from_text("one\ntwo\nthree");
        assert_eq!(b.line_range_at(5), 4..8);
        assert_eq!(
            b.line_range_at(10),
            8..13,
            "the last line has no ending to take"
        );

        let range = b.line_range_at(5);
        b.add_cursor(0, 0);
        b.select_range(range.start, range.end);
        assert_eq!(b.cursor_count(), 1, "a mouse selection is one cursor");
        b.backspace();
        assert_eq!(b.rope.to_string(), "one\nthree");
    }

    #[test]
    fn horizontal_scroll_does_nothing_when_everything_fits() {
        let mut b = Buffer::from_text("a\nb\n");
        b.scroll_columns_by(50, 10);
        assert_eq!(b.scroll_column, 1, "one character is the whole width here");
    }

    #[test]
    fn scrolling_follows_the_cursor() {
        let text: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let mut b = Buffer::from_text(&text);
        for _ in 0..50 {
            b.move_down(Move);
        }
        b.scroll_to_cursor(20, 0);
        let (line, _) = b.cursor_position();
        assert!((b.scroll_line..b.scroll_line + 20).contains(&line));
    }
}
