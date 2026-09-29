//! A buffer and its file: opening, reading, reloading, saving, and
//! whether the file changed behind it.

use super::*;

/// `text` with CRLF and lone CR as LF, the only line break the rope holds.
pub(crate) fn normalize_newlines(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains('\r') {
        text.replace("\r\n", "\n").replace('\r', "\n").into()
    } else {
        text.into()
    }
}
/// Copies mode, owner, ACLs and extended attributes onto a replacement file.
pub(super) fn copy_metadata(
    source: &std::path::Path,
    destination: &std::fs::File,
) -> std::io::Result<()> {
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
pub(super) fn matches_disk(
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
pub struct DiskStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
}
impl DiskStamp {
    /// `len secs nanos`, or `len -` without a modification time: how a
    /// recovery file keeps it.
    pub fn encode(&self) -> String {
        match self
            .modified
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        {
            Some(d) => format!("{} {} {}", self.len, d.as_secs(), d.subsec_nanos()),
            None => format!("{} -", self.len),
        }
    }

    pub fn decode(text: &str) -> Option<Self> {
        let mut parts = text.split_whitespace();
        let len = parts.next()?.parse().ok()?;
        let modified = match parts.next()? {
            "-" => None,
            secs => Some(
                std::time::UNIX_EPOCH
                    + std::time::Duration::new(secs.parse().ok()?, parts.next()?.parse().ok()?),
            ),
        };
        Some(DiskStamp { len, modified })
    }

    pub fn of(path: &std::path::Path) -> std::io::Result<Self> {
        Ok(Self::from(&std::fs::metadata(path)?))
    }
}
impl From<&std::fs::Metadata> for DiskStamp {
    fn from(meta: &std::fs::Metadata) -> Self {
        DiskStamp {
            len: meta.len(),
            modified: meta.modified().ok(),
        }
    }
}
/// A file read for a reload, not yet taken by the buffer.
pub struct DiskRead {
    rope: Rope,
    format: DiskFormat,
    stamp: Option<DiskStamp>,
}
impl DiskRead {
    /// What `stat` said when it was read: still the file's, or it changed
    /// again while the read was on its way.
    pub fn stamp(&self) -> Option<DiskStamp> {
        self.stamp
    }
}
/// Files past this open read-only, with the status line saying why.
pub const READ_ONLY_BYTES: u64 = 512 * 1024 * 1024;
/// Files past this are not opened at all.
pub const MAX_OPEN_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// `512 MB`, `2.0 GB`: sizes as the status line and alerts say them.
pub fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    if bytes >= 1024 * MB {
        format!("{:.1} GB", bytes as f64 / (1024 * MB) as f64)
    } else if bytes >= MB {
        format!("{} MB", bytes.div_ceil(MB))
    } else if bytes >= KB {
        format!("{} KB", bytes.div_ceil(KB))
    } else if bytes == 1 {
        "1 byte".to_string()
    } else {
        format!("{bytes} bytes")
    }
}
/// The size past which a file opens read-only: [`READ_ONLY_BYTES`], or less
/// when `CRC_READ_ONLY_BYTES` says so (the GUI self-test, which cannot write
/// half a gigabyte per run). What the messages say is this, not the constant.
pub fn read_only_limit() -> u64 {
    static LIMIT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("CRC_READ_ONLY_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(READ_ONLY_BYTES)
    })
}
/// Formats macOS Quick Look can display without treating their bytes as text.
pub(super) fn is_preview_path(path: &std::path::Path) -> bool {
    let Some(extension) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "pdf"
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
    /// Loads a file, remembering the path so it can be saved back.
    ///
    /// Past [`READ_ONLY_BYTES`] it opens read-only; past [`MAX_OPEN_BYTES`]
    /// it is refused with `FileTooLarge` before a byte is read. The file is
    /// held about three times over while it loads (bytes, decoded text,
    /// rope), so the cap is what keeps a stray disk image from taking the
    /// machine's memory. `CRC_READ_ONLY_BYTES` lowers the first limit for
    /// the GUI self-test, which cannot write half a gigabyte per run.
    pub fn open(path: impl Into<std::path::PathBuf>) -> std::io::Result<Self> {
        Self::open_with_limits(path.into(), read_only_limit(), MAX_OPEN_BYTES)
    }
    pub(super) fn open_with_limits(
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
        buffer.path = Some(crate::platform::canonical(&path));
        buffer.saved = Some(buffer.rope.clone());
        buffer.read_only = len > read_only_at;
        if let Some(path) = &buffer.path {
            buffer.indent_style = crate::text::indent::for_file(path, &buffer.rope);
        }
        Ok(buffer)
    }
    /// What `stat` said the last time this buffer read or wrote its file.
    pub fn disk_stamp(&self) -> Option<DiskStamp> {
        self.stamp
    }
    /// Opened past [`READ_ONLY_BYTES`], so it cannot be edited.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }
    /// Whether edits are refused: a preview has no text to edit, and a
    /// read-only file is too big to.
    pub(super) fn is_locked(&self) -> bool {
        self.preview_file || self.read_only || self.home_page || self.view_only
    }
    pub fn set_view_only(&mut self, view_only: bool) {
        self.view_only = view_only;
    }
    /// A tab with no text of its own: no caret, encoding or save state.
    pub fn is_view_only(&self) -> bool {
        self.view_only
    }
    /// Marks this as the document behind the Home page, which refuses edits.
    pub fn set_home_page(&mut self, home: bool) {
        self.home_page = home;
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
                let now = DiskStamp::from(&meta);
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
        let read = Buffer::read_disk(&path)?;
        self.apply_disk(read);
        Ok(())
    }
    /// The file at `path` as a reload would take it. Touches nothing of a
    /// buffer, so a large file can be read on another thread.
    pub fn read_disk(path: &std::path::Path) -> std::io::Result<DiskRead> {
        let raw = std::fs::read(path)?;
        let (text, format) = file_format::decode(&raw)?;
        Ok(DiskRead {
            rope: Rope::from_text(&text),
            format,
            stamp: DiskStamp::of(path).ok(),
        })
    }
    /// Takes the text [`Buffer::read_disk`] read, keeping the old text under
    /// undo.
    pub fn apply_disk(&mut self, read: DiskRead) {
        let DiskRead {
            rope,
            format,
            stamp,
        } = read;
        let before = self.snapshot();
        self.rope = rope;
        self.format = format;
        self.extra.clear();
        self.clamp_positions();
        self.undo_stack.push(before);
        self.redo_stack.clear();
        self.invalidate_edits();
        self.goal_column = None;
        self.saved = Some(self.rope.clone());
        self.stamp = stamp;
        self.conflict_noticed = false;
        self.dirty = false;
    }
    /// The file behind the buffer was deleted by something else. The text
    /// is now the only copy, which is what "unsaved" means: the tab gets its
    /// dot and closing asks first.
    pub fn note_missing_on_disk(&mut self) {
        if self.path.is_some() && !self.preview_file {
            self.dirty = true;
        }
    }
    pub(super) fn preview_file(path: std::path::PathBuf) -> Self {
        let mut buffer = Buffer::new();
        buffer.path = Some(crate::platform::canonical(&path));
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
    ///
    /// `stamp` is what the crashed instance knew of the file. When the file
    /// has changed since, the disk text is not what the edits were made
    /// against: the first save asks, as for any change on disk.
    pub fn recovered(
        path: Option<std::path::PathBuf>,
        text: &str,
        stamp: Option<DiskStamp>,
    ) -> Self {
        let path = path.map(|p| crate::platform::canonical(&p));
        let mut buffer = Buffer::from_text(text);
        if let Some((saved, format)) = path
            .as_deref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|raw| file_format::decode(&raw).ok())
        {
            buffer.saved = Some(Rope::from_text(&saved));
            buffer.format = format;
        }
        let now = path.as_deref().and_then(|p| DiskStamp::of(p).ok());
        buffer.stamp = stamp.or(now);
        if stamp.is_some() && stamp != now {
            buffer.saved = None;
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
    pub(super) fn save_with(
        &mut self,
        path: Option<&std::path::Path>,
        force: bool,
    ) -> std::io::Result<()> {
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
                // Our own stamp still on the file means nobody wrote it since;
                // only a changed stamp costs reading the whole file.
                let unchanged = self.stamp.is_some() && DiskStamp::of(&target).ok() == self.stamp;
                if !unchanged && !matches_disk(&target, expected, &self.format)? {
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
        // The format this text is written with. It replaces `self.format`
        // only once the write landed: until then the endings stay numbered
        // by the saved text's lines, which a rollback and the next save need.
        let format = self.format_for_save();
        let in_place = existing.as_ref().is_some_and(|m| m.nlink() > 1);
        let atomic = if in_place {
            None
        } else {
            Some(self.write_replacing(&target, existing.is_some(), &format))
        };
        match atomic {
            Some(Ok(())) => {}
            // A writable file in a directory we may not create in: write
            // the file itself, as for a hard link.
            Some(Err(e))
                if e.kind() == std::io::ErrorKind::PermissionDenied && existing.is_some() =>
            {
                self.write_in_place(&target, &format)?;
            }
            Some(Err(e)) => return Err(e),
            // Rename would silently detach the other names from this file.
            // Writing the same inode preserves its links, ACLs and xattrs.
            None => self.write_in_place(&target, &format)?,
        }
        self.format = format;
        self.stamp = DiskStamp::of(&target).ok();
        self.conflict_noticed = false;
        self.path = Some(crate::platform::canonical(&target));
        self.saved = Some(self.rope.clone());
        self.dirty = false;
        Ok(())
    }
    /// The format to write the text with. A file with mixed line endings
    /// keeps one per newline, by position. Lines added or removed since the
    /// last save shift those positions, so each line that is still there
    /// takes the ending it had, and each new one the file's usual ending.
    pub(super) fn format_for_save(&self) -> DiskFormat {
        let mut format = self.format.clone();
        let preferred = self.format.preferred;
        if self.format.endings.iter().all(|e| *e == preferred) {
            return format;
        }
        let Some(saved) = &self.saved else {
            return format;
        };
        if saved.same_as(&self.rope) {
            return format;
        }
        let (old, new) = (saved.to_string(), self.rope.to_string());
        let mut endings = vec![preferred; new.bytes().filter(|b| *b == b'\n').count()];
        for (was, is) in crate::ide::diff::unchanged_lines(&old, &new) {
            if let (Some(slot), Some(ending)) = (endings.get_mut(is), self.format.endings.get(was))
            {
                *slot = *ending;
            }
        }
        format.endings = endings;
        format
    }
    /// Writes over the file's own bytes. Nothing is truncated before the new
    /// text is all written, and a write that fails half-way puts the old
    /// text back, when it is known.
    pub(super) fn write_in_place(
        &self,
        target: &std::path::Path,
        format: &DiskFormat,
    ) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        let mut new = Vec::new();
        file_format::write(&mut new, &self.rope, format)?;
        let mut old = None;
        if let Some(saved) = &self.saved
            && self.path.as_deref() == Some(target)
        {
            let mut bytes = Vec::new();
            file_format::write(&mut bytes, saved, &self.format)?;
            old = Some(bytes);
        }
        let file = std::fs::OpenOptions::new().write(true).open(target)?;
        let result = file
            .write_all_at(&new, 0)
            .and_then(|()| file.set_len(new.len() as u64))
            .and_then(|()| file.sync_all());
        if let (Err(_), Some(old)) = (&result, old) {
            let _ = file
                .write_all_at(&old, 0)
                .and_then(|()| file.set_len(old.len() as u64))
                .and_then(|()| file.sync_all());
        }
        result
    }
    /// Writes a temporary file next to `target` and renames it over.
    pub(super) fn write_replacing(
        &self,
        target: &std::path::Path,
        existing: bool,
        format: &DiskFormat,
    ) -> std::io::Result<()> {
        {
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
                file_format::write(&mut file, &self.rope, format)?;
                if existing {
                    copy_metadata(target, &file)?;
                    // COPYFILE_STAT also copies the old modification time.
                    file.set_times(
                        std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()),
                    )?;
                }
                file.sync_all()?;
                std::fs::rename(&temp_path, target)?;
                std::fs::File::open(parent)?.sync_all()?;
                Ok(())
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&temp_path);
            }
            result
        }
    }
    pub(super) fn from_rope(rope: Rope) -> Self {
        Buffer {
            rope,
            cursor: 0,
            anchor: 0,
            extra: Vec::new(),
            read_only: false,
            home_page: false,
            view_only: false,
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
            syntax_sync: SyntaxSync::Edits(Vec::new()),
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
}
