//! Copying and moving project items on disk, for the Explorer.
//!
//! Nothing here writes over an entry that already exists. A copy is built in
//! a hidden folder beside its destination and moved into place with the
//! exclusive rename; a move is that rename, or a copy and a removal when the
//! destination is on another volume. A copy told to replace an item moves
//! that item to the Trash first. A symbolic link is the item itself
//! throughout: it is copied, moved and removed as the link, and never
//! followed. Every operation answers where the item ended up, so that it can
//! be reversed.
//!
//! Every change the Explorer makes to the files goes through [`perform`]:
//! a new file or folder, a rename, a move, a copy, a duplicate, a move to
//! the Trash. It answers a [`Record`] of what was done. What a paste or a
//! duplicate will do to each item is worked out first, by [`plan_paste`]
//! and [`plan_duplicate`], without touching anything. A record is what an
//! undo goes by: [`Steps`] keeps them, a step for each thing a person did,
//! and [`undo_step`] and [`redo_step`] take one back and do it again.

use std::collections::{HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::tree::move_without_replace;

/// What the hidden folder a copy is built in is named after, beside the
/// copy's destination. One left behind means a copy was cut short.
pub const STAGING_PREFIX: &str = ".crc-copy-";

/// Whether `name` is a staging folder's: never an item of the project, so
/// the tree, the finder, the search, the index and the watcher all leave
/// it out.
pub fn is_staging(name: &OsStr) -> bool {
    name.as_bytes().starts_with(STAGING_PREFIX.as_bytes())
}

/// The name a copy of `name` takes in the folder `dir`: `name` itself when
/// nothing there has it, otherwise the first free of `stem-copy.ext`,
/// `stem-copy-2.ext`, `stem-copy-3.ext` and so on.
///
/// The extension is what follows the last dot. A name that only starts with
/// one (`.env`) has none, and neither has a `folder`, whose dots are part
/// of its name. A name that is already a copy's continues its series
/// instead of nesting: `a-copy.ts` is followed by `a-copy-2.ts`.
pub fn free_name(dir: &Path, name: &OsStr, folder: bool) -> OsString {
    first_free(name, folder, |candidate| taken(&dir.join(candidate)))
}

/// [`free_name`], with `taken` answering whether a name is in use.
fn first_free(name: &OsStr, folder: bool, taken: impl Fn(&OsStr) -> bool) -> OsString {
    if !taken(name) {
        return name.to_owned();
    }
    let bytes = name.as_bytes();
    let (stem, extension) = match bytes.iter().rposition(|&b| b == b'.') {
        Some(dot) if dot > 0 && !folder => bytes.split_at(dot),
        _ => (bytes, &[][..]),
    };
    let (base, mut n) = series(stem);
    loop {
        let mut candidate = base.to_vec();
        candidate.extend_from_slice(b"-copy");
        if n > 1 {
            candidate.extend_from_slice(format!("-{n}").as_bytes());
        }
        candidate.extend_from_slice(extension);
        let candidate = OsString::from_vec(candidate);
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// What a stem's copies are named after, and the number of the first one
/// to try: `a` gives `a` and 1, `a-copy` gives `a` and 2, `a-copy-4` gives
/// `a` and 5.
fn series(stem: &[u8]) -> (&[u8], u64) {
    if let Some(base) = stem.strip_suffix(b"-copy") {
        return (base, 2);
    }
    if let Some(dash) = stem.iter().rposition(|&b| b == b'-')
        && let Some(base) = stem[..dash].strip_suffix(b"-copy")
        && let Some(next) = copy_number(&stem[dash + 1..]).and_then(|n| n.checked_add(1))
    {
        return (base, next);
    }
    (stem, 1)
}

/// The number a copy's name ends in, as this module writes it: from 2 up,
/// with no leading zero.
fn copy_number(digits: &[u8]) -> Option<u64> {
    if digits.first() == Some(&b'0') || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let n = std::str::from_utf8(digits).ok()?.parse().ok()?;
    (n >= 2).then_some(n)
}

/// Whether anything is at `path`, a dangling symbolic link included.
fn taken(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// `path` as the file system resolves it, symbolic links followed. What
/// does not exist yet is kept as given, under the nearest folder that does.
pub fn resolved(path: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(path) {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => resolved(parent).join(name),
        _ => path.to_path_buf(),
    }
}

/// Where the item at `path` is. As [`resolved`], except that a symbolic
/// link stays the link: its folder is resolved and its own name kept.
pub fn located(path: &Path) -> PathBuf {
    let link = fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if link => resolved(parent).join(name),
        _ => resolved(path),
    }
}

/// Whether the folder `dir` is the item at `item` or somewhere inside it,
/// by where the file system says both are: a path that reaches into `item`
/// through a symbolic link is inside it. A link to a folder contains
/// nothing, as it is the link that would be copied or moved.
pub fn is_inside(dir: &Path, item: &Path) -> bool {
    resolved(dir).starts_with(located(item))
}

/// Copies the file, folder or symbolic link at `source` to `destination`,
/// and answers `destination`.
///
/// Nothing is replaced: with anything already at `destination`, a dangling
/// link included, this fails with `AlreadyExists` and leaves it alone. On
/// any failure nothing of the copy is left behind. A link is copied as a
/// link, at the top or inside a folder, so a link that loops is not
/// followed. Files and folders keep their permissions. A folder is not
/// copied into itself or anything below it. Sockets, pipes and devices
/// inside a folder are left out: there is nothing in them to copy.
pub fn copy_item(source: &Path, destination: &Path) -> io::Result<PathBuf> {
    copy_item_until(source, destination, &|| false)
}

/// [`copy_item`], which can be stopped: `stop` is asked before each file
/// and folder is copied, and once it answers `true` the copy ends with
/// `Interrupted` and nothing of it is left. A copy already moved into
/// place is whole, and stays.
pub fn copy_item_until(
    source: &Path,
    destination: &Path,
    stop: &dyn Fn() -> bool,
) -> io::Result<PathBuf> {
    copy_into_place(source, destination, stop, None).map(|placed| placed.path)
}

/// What moves an item to the Trash, and answers where it is there when
/// the system says: [`Env::trash`].
pub type ToTrash<'a> = &'a dyn Fn(&Path) -> io::Result<Option<PathBuf>>;

/// A copy put where another item was.
#[derive(Debug)]
pub struct Replaced {
    /// Where the copy is.
    pub path: PathBuf,
    /// Something was there, and went to the Trash.
    pub replaced: bool,
    /// Where that is in the Trash, when the system says.
    pub trashed: Option<PathBuf>,
}

/// Copies the item at `source` to `destination` in place of what is there,
/// which is handed to `trash` and never written over. As [`copy_item_until`]
/// otherwise, and a plain copy when nothing is there.
///
/// The copy is built whole first, so one that fails or is stopped leaves
/// what was there where it was. Only then is that moved to the Trash and
/// the copy given its name. An item is not put in place of itself, or of a
/// folder it is in: the Trash would take the original along.
pub fn replace_item_until(
    source: &Path,
    destination: &Path,
    trash: ToTrash<'_>,
    stop: &dyn Fn() -> bool,
) -> io::Result<Replaced> {
    copy_into_place(source, destination, stop, Some(trash))
}

/// [`copy_item_until`], or with `trash` given [`replace_item_until`].
fn copy_into_place(
    source: &Path,
    destination: &Path,
    stop: &dyn Fn() -> bool,
    trash: Option<ToTrash<'_>>,
) -> io::Result<Replaced> {
    let (dir, name) = folder_and_name(destination)?;
    let kind = fs::symlink_metadata(source)?.file_type();
    if !(kind.is_file() || kind.is_dir() || kind.is_symlink()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only files, folders and links can be copied",
        ));
    }
    refuse_nesting(source, dir)?;
    if trash.is_none() {
        // Before the work of copying. The exclusive move below is what
        // keeps an entry made in the meantime.
        if taken(destination) {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
    } else if holds(destination, source) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an item cannot replace itself or the folder it is in",
        ));
    }
    let staging = Staging::new(dir)?;
    let staged = staging.0.join(name);
    let mut folders = Vec::new();
    copy_tree(source, &staged, Path::new(""), &mut folders, stop)?;
    // The copy is whole: only now does what is there give way to it.
    let mut replaced = false;
    let mut trashed = None;
    if let Some(trash) = trash
        && taken(destination)
    {
        trashed = trash(destination)?;
        replaced = true;
    }
    let placed = move_without_replace(&staged, destination).and_then(|()| {
        // Folders were made writable to be filled, and to be moved out of
        // the staging folder. Deepest first, as one that loses its search
        // bit hides the folders in it.
        for (relative, permissions) in folders.into_iter().rev() {
            if let Err(error) = fs::set_permissions(destination.join(relative), permissions) {
                let _ = remove_item(destination);
                return Err(error);
            }
        }
        Ok(())
    });
    match placed {
        Ok(()) => Ok(Replaced {
            path: destination.to_path_buf(),
            replaced,
            trashed,
        }),
        Err(error) if replaced => {
            // What gave way comes back, when the Trash said where it is.
            let back = trashed
                .as_deref()
                .is_some_and(|trashed| move_without_replace(trashed, destination).is_ok());
            if back {
                Err(error)
            } else {
                Err(io::Error::new(
                    error.kind(),
                    format!("{error}; the item that was there is in the Trash"),
                ))
            }
        }
        Err(error) => Err(error),
    }
}

/// Whether the item at `outer` is the one at `item`, or a folder that has
/// `item` somewhere in it, by where the file system says both are. A link
/// to a folder holds nothing: it is the link that would go to the Trash.
fn holds(outer: &Path, item: &Path) -> bool {
    located(item).starts_with(located(outer))
}

/// The folder a destination is in, and the name it has there.
fn folder_and_name(destination: &Path) -> io::Result<(&Path, &OsStr)> {
    match (destination.parent(), destination.file_name()) {
        (Some(dir), Some(name)) => Ok((dir, name)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the destination has no name",
        )),
    }
}

/// Refuses a folder headed for `dir` when that is the folder itself or
/// inside it.
fn refuse_nesting(source: &Path, dir: &Path) -> io::Result<()> {
    if fs::symlink_metadata(source)?.is_dir() && is_inside(dir, source) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a folder cannot go inside itself",
        ));
    }
    Ok(())
}

/// A hidden folder made for one copy, private to this user, and removed
/// with whatever is still in it when the copy ends either way.
struct Staging(PathBuf);

impl Staging {
    fn new(dir: &Path) -> io::Result<Staging> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // A name in use is one a process with this id left when it died
        // mid-copy: not ours to fill or to remove.
        for _ in 0..16 {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!("{STAGING_PREFIX}{}-{n}", std::process::id()));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Staging(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::ErrorKind::AlreadyExists.into())
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Copies what is at `from` to `to`, where nothing is yet. Each folder is
/// made writable by its owner whatever the original allows; what it should
/// have is added to `folders` under its path below the copy's root, to be
/// set when the copy is in place. `stop` is asked before each item.
fn copy_tree(
    from: &Path,
    to: &Path,
    relative: &Path,
    folders: &mut Vec<(PathBuf, fs::Permissions)>,
    stop: &dyn Fn() -> bool,
) -> io::Result<()> {
    if stop() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let metadata = fs::symlink_metadata(from)?;
    let kind = metadata.file_type();
    if kind.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(from)?, to)
    } else if kind.is_dir() {
        // Never more open to others than the original, even for a moment.
        let mode = (metadata.permissions().mode() & 0o777) | 0o700;
        fs::DirBuilder::new().mode(mode).create(to)?;
        folders.push((relative.to_path_buf(), metadata.permissions()));
        for entry in fs::read_dir(from)? {
            let name = entry?.file_name();
            copy_tree(
                &from.join(&name),
                &to.join(&name),
                &relative.join(&name),
                folders,
                stop,
            )?;
        }
        Ok(())
    } else if kind.is_file() {
        fs::copy(from, to).map(|_| ())
    } else {
        // A socket, a pipe or a device: reading one could wait forever.
        Ok(())
    }
}

/// Where a moved item is now.
#[derive(Debug)]
pub struct Moved {
    pub path: PathBuf,
    /// Set when the move went to another volume and the original could not
    /// be removed after it was copied: it is still at the source, and this
    /// is why.
    pub original_left: Option<io::Error>,
}

/// Moves the item at `source` to `destination` without replacing anything
/// there, deleting the original when the move has to be a copy. See
/// [`move_item_with`].
pub fn move_item(source: &Path, destination: &Path) -> io::Result<Moved> {
    move_item_with(source, destination, remove_item)
}

/// Moves the item at `source` to `destination` without replacing anything
/// there: with anything already at `destination` this fails with
/// `AlreadyExists` and nothing changes.
///
/// A destination on another volume cannot be renamed to. The item is
/// copied there as [`copy_item`] does, then `remove` is called with
/// `source` to take the original away, by deleting it or by moving it to
/// the Trash. When `remove` fails the copy stays and the failure is
/// answered in [`Moved::original_left`]: taking the copy back could lose
/// what a removal that stopped halfway already took.
pub fn move_item_with(
    source: &Path,
    destination: &Path,
    remove: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<Moved> {
    move_with(source, destination, move_without_replace, remove)
}

/// [`move_item_with`], with the rename given so a test can stand in for
/// another volume.
fn move_with(
    source: &Path,
    destination: &Path,
    rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
    remove: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<Moved> {
    let (dir, _) = folder_and_name(destination)?;
    // The rename refuses this too, as an invalid argument and no more.
    refuse_nesting(source, dir)?;
    match rename(source, destination) {
        Ok(()) => Ok(Moved {
            path: destination.to_path_buf(),
            original_left: None,
        }),
        Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
            let path = copy_item(source, destination)?;
            Ok(Moved {
                path,
                original_left: remove(source).err(),
            })
        }
        Err(error) => Err(error),
    }
}

/// Deletes the item at `path` for good, a folder with everything in it. A
/// symbolic link goes, not what it points at.
pub fn remove_item(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// What a file operation is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpKind {
    NewFile,
    NewFolder,
    Rename,
    Move,
    Copy,
    Duplicate,
    /// A copy put in place of an item, which went to the Trash.
    Replace,
    Trash,
}

/// One change to the files on disk, as it is asked for.
#[derive(Clone, Copy, Debug)]
pub enum Op<'a> {
    /// An empty file at this path, the folders above it made as needed.
    NewFile(&'a Path),
    /// A folder at this path, the folders above it made as needed.
    NewFolder(&'a Path),
    /// The item at `from` under another name, `to`, in the same folder.
    Rename { from: &'a Path, to: &'a Path },
    /// The item at `from` moved to `to`.
    Move { from: &'a Path, to: &'a Path },
    /// A copy of the item at `from`, at `to`.
    Copy { from: &'a Path, to: &'a Path },
    /// As `Copy`, asked for as a duplicate beside the original.
    Duplicate { from: &'a Path, to: &'a Path },
    /// A copy of the item at `from`, at `to`, in place of the item that is
    /// there: that one is moved to the Trash once the copy is whole.
    Replace { from: &'a Path, to: &'a Path },
    /// The item at this path moved to the Trash.
    Trash(&'a Path),
}

/// What an operation did, with what it takes to reverse it.
#[derive(Debug)]
pub struct Record {
    pub kind: OpKind,
    /// The item acted on, where it was. `None` for a new file or folder.
    pub source: Option<PathBuf>,
    /// Where the result is: the new item, the copy, the item once renamed
    /// or moved. `None` for an item moved to the Trash.
    pub result: Option<PathBuf>,
    /// Where the item is in the Trash, when the system says. For a copy
    /// that replaced an item, where the item that was at `result` is.
    pub trashed: Option<PathBuf>,
    /// A move to another volume whose original could not be removed after
    /// it was copied: it is still at `source`, and this is why.
    pub original_left: Option<io::Error>,
    /// What the item is: the one at `result`, or the one that went to the
    /// Trash. An undo touches only what is still that kind of thing.
    pub item: Option<ItemKind>,
    /// What the item a copy replaced is, the one `trashed` says where.
    pub replaced: Option<ItemKind>,
}

/// What an item is, as far as taking an operation back goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    File,
    Folder,
    /// A symbolic link, whatever it points at.
    Link,
}

/// What is at `path`: a link is the link.
fn item_kind(path: &Path) -> io::Result<ItemKind> {
    let kind = fs::symlink_metadata(path)?.file_type();
    Ok(if kind.is_symlink() {
        ItemKind::Link
    } else if kind.is_dir() {
        ItemKind::Folder
    } else {
        ItemKind::File
    })
}

/// What an operation is given besides its paths.
pub struct Env<'a> {
    /// Moves an item to the Trash, and answers where it is there when the
    /// system says.
    pub trash: ToTrash<'a>,
    /// Asked before each file and folder of a copy: `true` stops the copy,
    /// which then fails with `Interrupted` and leaves nothing behind.
    pub stop: &'a dyn Fn() -> bool,
}

/// Does `op`, and answers what was done. The one way a file or folder of
/// the project is made, renamed, moved, copied or trashed, so that every
/// one of them leaves a record.
///
/// Nothing is ever written over: an operation whose destination is taken,
/// a dangling link included, fails with `AlreadyExists` and changes
/// nothing. [`Op::Replace`] alone asks for the destination, and what is
/// there goes to the Trash, where the record says it is.
pub fn perform(op: Op<'_>, env: &Env<'_>) -> io::Result<Record> {
    let record = |kind: OpKind, source: Option<&Path>, result: Option<PathBuf>| Record {
        kind,
        source: source.map(Path::to_path_buf),
        item: result.as_deref().and_then(|path| item_kind(path).ok()),
        result,
        trashed: None,
        original_left: None,
        replaced: None,
    };
    match op {
        Op::NewFile(path) => {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            // Never truncates: a file already there is an error, not an
            // empty file.
            fs::File::create_new(path)?;
            Ok(record(OpKind::NewFile, None, Some(path.to_path_buf())))
        }
        Op::NewFolder(path) => {
            if taken(path) {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            fs::create_dir_all(path)?;
            Ok(record(OpKind::NewFolder, None, Some(path.to_path_buf())))
        }
        Op::Rename { from, to } | Op::Move { from, to } => {
            let kind = match op {
                Op::Rename { .. } => OpKind::Rename,
                _ => OpKind::Move,
            };
            let moved = move_item(from, to)?;
            Ok(Record {
                original_left: moved.original_left,
                ..record(kind, Some(from), Some(moved.path))
            })
        }
        Op::Copy { from, to } | Op::Duplicate { from, to } => {
            let kind = match op {
                Op::Copy { .. } => OpKind::Copy,
                _ => OpKind::Duplicate,
            };
            let copy = copy_item_until(from, to, env.stop)?;
            Ok(record(kind, Some(from), Some(copy)))
        }
        Op::Replace { from, to } => {
            let there = item_kind(to).ok();
            let placed = replace_item_until(from, to, env.trash, env.stop)?;
            // Nothing was there after all: a copy like any other.
            let kind = if placed.replaced {
                OpKind::Replace
            } else {
                OpKind::Copy
            };
            Ok(Record {
                trashed: placed.trashed,
                replaced: there.filter(|_| placed.replaced),
                ..record(kind, Some(from), Some(placed.path))
            })
        }
        Op::Trash(path) => {
            let item = item_kind(path).ok();
            let trashed = (env.trash)(path)?;
            Ok(Record {
                trashed,
                item,
                ..record(OpKind::Trash, Some(path), None)
            })
        }
    }
}

/// [`perform`] for a move, a copy or a duplicate whose destination's name
/// was free when it was chosen. If something has taken the name since, the
/// item goes under the next free one in the same folder instead of failing.
/// Any other operation is performed as it is.
pub fn perform_free(op: Op<'_>, env: &Env<'_>) -> io::Result<Record> {
    let (from, first) = match op {
        Op::Move { from, to } | Op::Copy { from, to } | Op::Duplicate { from, to } => (from, to),
        _ => return perform(op, env),
    };
    let mut to = first.to_path_buf();
    // More than a few names taken in a row is not a race any more.
    for _ in 0..8 {
        let attempt = match op {
            Op::Move { .. } => Op::Move { from, to: &to },
            Op::Copy { .. } => Op::Copy { from, to: &to },
            _ => Op::Duplicate { from, to: &to },
        };
        match perform(attempt, env) {
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let (dir, name) = folder_and_name(&to)?;
                let next = dir.join(free_name(dir, name, shown_as_folder(from)));
                if next == to {
                    return Err(error);
                }
                to = next;
            }
            done => return done,
        }
    }
    Err(io::ErrorKind::AlreadyExists.into())
}

/// Whether the item at `path` is a folder as the Explorer shows it: a
/// folder, or a link to one. Its copies are named as a folder's are.
fn shown_as_folder(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_dir())
}

/// What a paste, or a duplicate, will do with one item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Planned {
    /// Copied to `to`, a name nothing has.
    Copy { from: PathBuf, to: PathBuf },
    /// Copied to `to` in place of the item there, which goes to the Trash:
    /// only where a drop from outside was told to replace.
    Replace { from: PathBuf, to: PathBuf },
    /// Moved to `to`, a name nothing has.
    Move { from: PathBuf, to: PathBuf },
    /// Cut, and pasted into the folder it is in already: it stays.
    Stays(PathBuf),
    /// Left alone, and why.
    Refused { from: PathBuf, why: Refusal },
}

/// Why a paste leaves an item alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Nothing is at its path any more.
    Missing,
    /// A folder, headed into itself or into a folder inside it.
    IntoItself,
    /// Dragged to a folder where something already has its name: a move
    /// by drag replaces nothing and renames nothing.
    Taken,
    /// Told to replace the item of its name, which is the folder it is in:
    /// the Trash would take the original along.
    InsideReplaced,
}

impl Refusal {
    /// The refusal as the rest of a sentence that starts with the item's
    /// name.
    pub fn said(self) -> &'static str {
        match self {
            Refusal::Missing => "is no longer there",
            Refusal::IntoItself => "cannot be pasted into itself",
            Refusal::Taken => "would replace an item of the same name",
            Refusal::InsideReplaced => "is inside the item it would replace",
        }
    }
}

/// What a drop from outside does where a name it brings is in use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnTaken {
    /// The item there goes to the Trash and the copy takes its name.
    Replace,
    /// The copy takes the next free `-copy` name.
    KeepBoth,
}

/// What pasting `sources` into the folder `dir` will do with each of them,
/// in order, without touching anything. Copied (`cut` false) or moved
/// (`cut` true), an item takes its own name in `dir` when that is free and
/// the next free `-copy` name otherwise, so that nothing is replaced: a
/// copy pasted into the folder its original is in becomes `name-copy.ext`.
/// Names already given to an earlier item of the same paste count as
/// taken.
///
/// When `dir` is itself one of `sources`, the paste goes to the folder that
/// item is in instead, as a paste on a file's row does: a folder copied
/// and pasted at once lands beside the original as `folder-copy`, and one
/// cut and pasted on itself stays.
///
/// A cut item pasted into the folder it is in stays where it is. A folder
/// is refused when `dir` is inside it, and so is an item that is no longer
/// there. A symbolic link is the item itself: it is never a folder that
/// could hold `dir`.
pub fn plan_paste(sources: &[PathBuf], dir: &Path, cut: bool) -> Vec<Planned> {
    let onto = located(dir);
    let dir = sources
        .iter()
        .find(|from| located(from) == onto)
        .and_then(|from| from.parent())
        .unwrap_or(dir);
    let mut chosen = HashSet::new();
    sources
        .iter()
        .map(|from| {
            let refused = |why| Planned::Refused {
                from: from.clone(),
                why,
            };
            let (Some(name), Ok(metadata)) = (from.file_name(), fs::symlink_metadata(from)) else {
                return refused(Refusal::Missing);
            };
            if metadata.is_dir() && is_inside(dir, from) {
                return refused(Refusal::IntoItself);
            }
            if cut && located(from).parent() == Some(resolved(dir).as_path()) {
                return Planned::Stays(from.clone());
            }
            let to = free_destination(dir, name, shown_as_folder(from), &mut chosen);
            let from = from.clone();
            if cut {
                Planned::Move { from, to }
            } else {
                Planned::Copy { from, to }
            }
        })
        .collect()
}

/// What duplicating `sources` will do with each of them, in order, without
/// touching anything: a copy beside the original, under the next free
/// `-copy` name. An item that is no longer there is refused.
pub fn plan_duplicate(sources: &[PathBuf]) -> Vec<Planned> {
    let mut chosen = HashSet::new();
    sources
        .iter()
        .map(|from| {
            let (Some(dir), Some(name), true) = (from.parent(), from.file_name(), taken(from))
            else {
                return Planned::Refused {
                    from: from.clone(),
                    why: Refusal::Missing,
                };
            };
            Planned::Copy {
                from: from.clone(),
                to: free_destination(dir, name, shown_as_folder(from), &mut chosen),
            }
        })
        .collect()
}

/// Where an item named `name` goes in `dir`: under [`free_name`], with the
/// destinations in `chosen` taken as well. The answer is added to them.
/// They are compared without regard to case, as most volumes here compare
/// names.
fn free_destination(
    dir: &Path,
    name: &OsStr,
    folder: bool,
    chosen: &mut HashSet<String>,
) -> PathBuf {
    let key = |candidate: &OsStr| dir.join(candidate).to_string_lossy().to_lowercase();
    let name = first_free(name, folder, |candidate| {
        taken(&dir.join(candidate)) || chosen.contains(&key(candidate))
    });
    chosen.insert(key(&name));
    dir.join(name)
}

// ---- dragging -----------------------------------------------------------------
// A drag in the Explorer asks whether it may be dropped on every move of
// the pointer. Where the file system says the items and the folder are is
// read once each, and the answer is then a comparison of paths.

/// An item being dragged, with where the file system says it is: read
/// once, when the drag starts.
#[derive(Clone, Debug)]
struct Dragged {
    path: PathBuf,
    /// As [`located`] answers: a symbolic link is the link.
    at: PathBuf,
    /// A folder that could hold the place it is dropped, not a link to one.
    folder: bool,
}

impl Dragged {
    fn new(path: PathBuf) -> Dragged {
        let folder = fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
        let at = located(&path);
        Dragged { path, at, folder }
    }
}

/// The folder a drag is over, with where the file system says it is: read
/// once, when the drag comes to it.
#[derive(Clone, Debug)]
struct DropFolder {
    dir: PathBuf,
    /// As [`resolved`] answers.
    real: PathBuf,
}

/// What dropping one item on a folder comes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Landing {
    /// It is moved there, or copied.
    Goes,
    /// Dropped on itself, or moved to the folder it is in: nothing to do.
    Stays,
    /// A folder, dropped somewhere inside itself.
    IntoItself,
}

/// What dropping `item` on `onto` comes to, moved or with `copy` copied.
/// Reads nothing: both were looked up when they were made.
///
/// Judged by where the file system says the two are, not by how the paths
/// are written: a folder reached through a symbolic link is still the
/// folder, and dropping it there would nest it all the same. A link is an
/// item of its own, and can go into the folder it points at.
fn landing(item: &Dragged, onto: &DropFolder, copy: bool) -> Landing {
    if onto.real == item.at || onto.dir == item.path {
        Landing::Stays
    } else if item.folder && onto.real.starts_with(&item.at) {
        Landing::IntoItself
    } else if !copy && item.at.parent() == Some(onto.real.as_path()) {
        Landing::Stays
    } else {
        Landing::Goes
    }
}

/// What a drag in the Explorer carries, and the folder it is over.
#[derive(Clone, Debug)]
pub struct DropCheck {
    items: Vec<Dragged>,
    onto: Option<DropFolder>,
}

impl DropCheck {
    /// A drag of the items at `paths` starts: each is looked up, once.
    pub fn new(paths: Vec<PathBuf>) -> DropCheck {
        DropCheck {
            items: paths.into_iter().map(Dragged::new).collect(),
            onto: None,
        }
    }

    /// How many items are carried.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The items carried, as they were pressed.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.items.iter().map(|item| item.path.as_path())
    }

    /// The folder the drag is over, if it is over one.
    pub fn dir(&self) -> Option<&Path> {
        self.onto.as_ref().map(|onto| onto.dir.as_path())
    }

    /// The drag is over the folder `dir` now, or over none. The folder is
    /// looked up only when it is another one than the last: a pointer that
    /// moves about inside one folder's rows reads nothing.
    pub fn aim(&mut self, dir: Option<&Path>) {
        self.aim_with(dir, resolved);
    }

    /// [`DropCheck::aim`], with the lookup given so a test can count it.
    fn aim_with(&mut self, dir: Option<&Path>, resolve: impl FnOnce(&Path) -> PathBuf) {
        if self.dir() == dir {
            return;
        }
        self.onto = dir.map(|dir| DropFolder {
            dir: dir.to_path_buf(),
            real: resolve(dir),
        });
    }

    /// Whether a drop here would move anything, or with `copy` copy it:
    /// what the target lights up for. Reads nothing.
    pub fn allowed(&self, copy: bool) -> bool {
        self.onto.as_ref().is_some_and(|onto| {
            self.items
                .iter()
                .any(|item| landing(item, onto, copy) == Landing::Goes)
        })
    }

    /// What a drop here will do with each item, in order, without touching
    /// anything. A copy takes its own name when that is free and the next
    /// free `-copy` name otherwise, as a paste does. A move never takes
    /// another name: where the item's name is in use it is refused as
    /// [`Refusal::Taken`], and so is the second of two items of one name.
    /// The rest still go.
    pub fn plan(&self, copy: bool) -> Vec<Planned> {
        let Some(onto) = &self.onto else {
            return Vec::new();
        };
        let mut chosen = HashSet::new();
        self.items
            .iter()
            .map(|item| {
                let refused = |why| Planned::Refused {
                    from: item.path.clone(),
                    why,
                };
                match landing(item, onto, copy) {
                    Landing::Stays => return Planned::Stays(item.path.clone()),
                    Landing::IntoItself => return refused(Refusal::IntoItself),
                    Landing::Goes => {}
                }
                let Some(name) = item.path.file_name().filter(|_| taken(&item.path)) else {
                    return refused(Refusal::Missing);
                };
                let from = item.path.clone();
                if copy {
                    let folder = shown_as_folder(&from);
                    let to = free_destination(&onto.dir, name, folder, &mut chosen);
                    return Planned::Copy { from, to };
                }
                let to = onto.dir.join(name);
                if taken(&to) || !chosen.insert(to.to_string_lossy().to_lowercase()) {
                    return refused(Refusal::Taken);
                }
                Planned::Move { from, to }
            })
            .collect()
    }

    // A drag from outside the tree, from the Finder or another app, is
    // always a copy, and it goes where a move would: dropped on the folder
    // an item is in already it has nothing to do, so [`DropCheck::allowed`]
    // is asked as for a move.

    /// The items a drop from outside would bring to a name that is in use
    /// in the folder: what the one question of such a drop is about. Looks
    /// at the folder once for each item that would go.
    pub fn taken_there(&self) -> Vec<&Path> {
        let Some(onto) = &self.onto else {
            return Vec::new();
        };
        self.items
            .iter()
            .filter(|item| landing(item, onto, false) == Landing::Goes && taken(&item.path))
            .filter(|item| {
                item.path
                    .file_name()
                    .is_some_and(|name| taken(&onto.dir.join(name)))
            })
            .map(|item| item.path.as_path())
            .collect()
    }

    /// What a drop from outside will do with each item, in order, without
    /// touching anything. Every item is copied under its own name where
    /// that is free. Where it is in use, `on_taken` says: the item there
    /// is replaced, or the copy takes the next free `-copy` name. An item
    /// dropped on the folder it is in already stays, a folder is refused
    /// inside itself, and so is an item that is no longer there.
    ///
    /// Only what was in the folder before the drop is replaced. The second
    /// of two items of one name is kept beside the first, and an item is
    /// refused that would replace a folder it is in.
    pub fn plan_from_outside(&self, on_taken: OnTaken) -> Vec<Planned> {
        let Some(onto) = &self.onto else {
            return Vec::new();
        };
        let mut chosen = HashSet::new();
        self.items
            .iter()
            .map(|item| {
                let refused = |why| Planned::Refused {
                    from: item.path.clone(),
                    why,
                };
                match landing(item, onto, false) {
                    Landing::Stays => return Planned::Stays(item.path.clone()),
                    Landing::IntoItself => return refused(Refusal::IntoItself),
                    Landing::Goes => {}
                }
                let Some(name) = item.path.file_name().filter(|_| taken(&item.path)) else {
                    return refused(Refusal::Missing);
                };
                let from = item.path.clone();
                let to = onto.dir.join(name);
                let key = to.to_string_lossy().to_lowercase();
                if on_taken == OnTaken::Replace && taken(&to) && !chosen.contains(&key) {
                    if item.at.starts_with(located(&to)) {
                        return refused(Refusal::InsideReplaced);
                    }
                    chosen.insert(key);
                    return Planned::Replace { from, to };
                }
                let folder = shown_as_folder(&from);
                let to = free_destination(&onto.dir, name, folder, &mut chosen);
                Planned::Copy { from, to }
            })
            .collect()
    }
}

// ---- taking back --------------------------------------------------------------
// A record says enough to take its operation back, and to do it again after
// that. Nothing is deleted on the way: what an operation made goes to the
// Trash, and what it sent there comes back from where the Trash put it.
// Each item is looked at first, and left alone when the files are no longer
// as the operation left them. Nothing is ever written over.

/// What taking an operation back, or doing it again, does to the files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reversal {
    /// The item at `from` goes to `to`, where nothing is.
    Move { from: PathBuf, to: PathBuf },
    /// The item goes to the Trash.
    Trash(PathBuf),
    /// The item at `from`, in the Trash, comes back to `to`.
    PutBack { from: PathBuf, to: PathBuf },
    /// The item at `path` goes to the Trash, and the one at `from`, in the
    /// Trash, takes its place.
    Exchange { path: PathBuf, from: PathBuf },
}

/// Why an operation is not taken back, or not done again.
#[derive(Debug)]
pub enum Skip {
    /// The item is not where the operation left it.
    Gone,
    /// It is not in the Trash where it was put.
    GoneFromTrash,
    /// Something else has its name now: a folder where a file was.
    Changed,
    /// The place it would go to is taken, by the item here.
    Taken(PathBuf),
    /// It went to the Trash, and the Trash did not say where.
    Unplaced,
    /// It could not be brought back from the Trash, where it still is:
    /// where it belongs, and why it is not there.
    StillInTrash { belongs: PathBuf, error: io::Error },
    /// It could not be done, and the item is where it was.
    Failed(io::Error),
}

impl Skip {
    /// The reason, as a sentence about the item called `name`.
    pub fn said(&self, name: &str) -> String {
        match self {
            Skip::Gone => format!("{name} is no longer there"),
            Skip::GoneFromTrash => format!("{name} is no longer in the Trash"),
            Skip::Changed => format!("{name} is no longer the same kind of item"),
            Skip::Taken(by) => format!(
                "{name} was left alone: an item named {} is in the way",
                shown_name(by)
            ),
            Skip::Unplaced => {
                format!("{name} is in the Trash, and the Trash did not say where")
            }
            Skip::StillInTrash { belongs, error } => format!(
                "{name} is still in the Trash and belongs in {}: {error}",
                belongs.parent().unwrap_or(belongs).display()
            ),
            Skip::Failed(error) => format!("{name}: {error}"),
        }
    }
}

/// An item's name, as a sentence says it.
fn shown_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Whether the item at `path` is still the `kind` of thing an operation
/// left there. What is `in_trash` and cannot be looked at passes: the
/// Trash may keep itself from being read, and the move out of it says so.
fn still(path: &Path, kind: Option<ItemKind>, in_trash: bool) -> Result<(), Skip> {
    use io::ErrorKind::{NotADirectory, NotFound};
    match item_kind(path) {
        Ok(found) if kind.is_none_or(|kind| kind == found) => Ok(()),
        Ok(_) => Err(Skip::Changed),
        Err(error) if matches!(error.kind(), NotFound | NotADirectory) => Err(if in_trash {
            Skip::GoneFromTrash
        } else {
            Skip::Gone
        }),
        Err(_) if in_trash => Ok(()),
        Err(error) => Err(Skip::Failed(error)),
    }
}

/// Whether `item` can go to `to`: nothing is there, or only `item` itself
/// under another spelling of its name, on a volume that does not tell the
/// two apart.
fn free_for(to: &Path, item: &Path) -> Result<(), Skip> {
    use std::os::unix::fs::MetadataExt;
    let itself = || match (fs::symlink_metadata(to), fs::symlink_metadata(item)) {
        (Ok(to), Ok(item)) => to.dev() == item.dev() && to.ino() == item.ino(),
        _ => false,
    };
    if !taken(to) || itself() {
        Ok(())
    } else {
        Err(Skip::Taken(to.to_path_buf()))
    }
}

/// Moves the item at `from` to `to`, where nothing may be.
fn put(from: &Path, to: &Path) -> Result<(), io::Error> {
    move_item(from, to).map(|_| ())
}

impl Reversal {
    /// The item that leaves its place, moved away or to the Trash: a tab
    /// open on it, or under it, would be left on a file that is gone.
    pub fn leaves(&self) -> Option<&Path> {
        match self {
            Reversal::Move { from, .. } => Some(from),
            Reversal::Trash(path) | Reversal::Exchange { path, .. } => Some(path),
            Reversal::PutBack { .. } => None,
        }
    }

    /// Where an item is once this is done; `None` when it is in the Trash.
    pub fn lands(&self) -> Option<&Path> {
        match self {
            Reversal::Move { to, .. } | Reversal::PutBack { to, .. } => Some(to),
            Reversal::Exchange { path, .. } => Some(path),
            Reversal::Trash(_) => None,
        }
    }

    /// Does it, and answers where `trash` put what went to the Trash, when
    /// it says. A place taken since it was looked at is left as it is.
    pub fn apply(&self, trash: ToTrash<'_>) -> Result<Option<PathBuf>, Skip> {
        let taken_or = |to: &Path, error: io::Error, other: fn(io::Error) -> Skip| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                Skip::Taken(to.to_path_buf())
            } else {
                other(error)
            }
        };
        match self {
            Reversal::Move { from, to } => put(from, to)
                .map(|()| None)
                .map_err(|error| taken_or(to, error, Skip::Failed)),
            Reversal::Trash(path) => trash(path).map_err(Skip::Failed),
            Reversal::PutBack { from, to } => put(from, to).map(|()| None).map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    Skip::Taken(to.clone())
                } else {
                    Skip::StillInTrash {
                        belongs: to.clone(),
                        error,
                    }
                }
            }),
            Reversal::Exchange { path, from } => {
                let gone = trash(path).map_err(Skip::Failed)?;
                let Err(error) = put(from, path) else {
                    return Ok(gone);
                };
                // What gave way comes back, when the Trash said where it is.
                let back = gone.as_deref().is_some_and(|gone| put(gone, path).is_ok());
                Err(if back {
                    Skip::StillInTrash {
                        belongs: path.clone(),
                        error,
                    }
                } else {
                    Skip::Failed(io::Error::new(
                        error.kind(),
                        format!(
                            "{error}; it is in the Trash now, and so is the item that was to take its place"
                        ),
                    ))
                })
            }
        }
    }
}

impl Record {
    /// The item, by the path a person knows it by: where the operation
    /// left it, or where it was before it went to the Trash.
    pub fn shown(&self) -> &Path {
        self.result
            .as_deref()
            .or(self.source.as_deref())
            .unwrap_or(Path::new(""))
    }

    /// What taking the operation back would do, looked up now and without
    /// touching anything: a new item or a copy goes to the Trash, a
    /// renamed or moved one back to where it was, a trashed one out of the
    /// Trash again, and a copy that replaced an item gives the place back
    /// to it. Refused when what the operation left is gone or has become
    /// another kind of thing, and when the place something would go back
    /// to is taken.
    pub fn undo(&self) -> Result<Reversal, Skip> {
        let at = |path: &Option<PathBuf>| path.clone().ok_or(Skip::Gone);
        match self.kind {
            OpKind::NewFile | OpKind::NewFolder | OpKind::Copy | OpKind::Duplicate => {
                let made = at(&self.result)?;
                still(&made, self.item, false)?;
                Ok(Reversal::Trash(made))
            }
            OpKind::Rename | OpKind::Move => {
                let (from, to) = (at(&self.result)?, at(&self.source)?);
                still(&from, self.item, false)?;
                free_for(&to, &from)?;
                Ok(Reversal::Move { from, to })
            }
            OpKind::Trash => {
                let to = at(&self.source)?;
                let from = self.trashed.clone().ok_or(Skip::Unplaced)?;
                still(&from, self.item, true)?;
                free_for(&to, &from)?;
                Ok(Reversal::PutBack { from, to })
            }
            OpKind::Replace => {
                let path = at(&self.result)?;
                let from = self.trashed.clone().ok_or(Skip::Unplaced)?;
                still(&path, self.item, false)?;
                still(&from, self.replaced, true)?;
                Ok(Reversal::Exchange { path, from })
            }
        }
    }
}

/// An operation taken back, with what it takes to do it again.
#[derive(Debug)]
pub struct Undone {
    pub record: Record,
    /// Where taking it back put what the operation had made, in the
    /// Trash, when the Trash said.
    pub aside: Option<PathBuf>,
}

impl Undone {
    /// What doing the operation again would do, looked up now and without
    /// touching anything, under the checks of [`Record::undo`]. What went
    /// to the Trash when it was taken back comes out of it again, with
    /// whatever is in it, instead of being made or copied a second time.
    pub fn redo(&self) -> Result<Reversal, Skip> {
        let record = &self.record;
        let at = |path: &Option<PathBuf>| path.clone().ok_or(Skip::Gone);
        match record.kind {
            OpKind::NewFile | OpKind::NewFolder | OpKind::Copy | OpKind::Duplicate => {
                let to = at(&record.result)?;
                let from = self.aside.clone().ok_or(Skip::Unplaced)?;
                still(&from, record.item, true)?;
                free_for(&to, &from)?;
                Ok(Reversal::PutBack { from, to })
            }
            OpKind::Rename | OpKind::Move => {
                let (from, to) = (at(&record.source)?, at(&record.result)?);
                still(&from, record.item, false)?;
                free_for(&to, &from)?;
                Ok(Reversal::Move { from, to })
            }
            OpKind::Trash => {
                let path = at(&record.source)?;
                still(&path, record.item, false)?;
                Ok(Reversal::Trash(path))
            }
            OpKind::Replace => {
                let path = at(&record.result)?;
                let from = self.aside.clone().ok_or(Skip::Unplaced)?;
                still(&path, record.replaced, false)?;
                still(&from, record.item, true)?;
                Ok(Reversal::Exchange { path, from })
            }
        }
    }
}

/// An item a step left as it is, and why.
#[derive(Debug)]
pub struct Skipped {
    /// The item, by the path a person knows it by.
    pub item: PathBuf,
    pub why: Skip,
}

/// What carries a [`Reversal`] out: [`Reversal::apply`], and whatever else
/// has to follow an item that moves.
pub type Apply<'a> = &'a mut dyn FnMut(&Reversal) -> Result<Option<PathBuf>, Skip>;

/// Takes back the operations of one step, the last one first, each as
/// [`Record::undo`] says and `apply` carries out. An operation that cannot
/// be taken back is skipped, and the others still are. Answers what was
/// taken back, in the order it was, and what was skipped.
pub fn undo_step(records: Vec<Record>, apply: Apply<'_>) -> (Vec<Undone>, Vec<Skipped>) {
    let mut undone = Vec::new();
    let mut skipped = Vec::new();
    for record in records.into_iter().rev() {
        match record.undo().and_then(|reversal| apply(&reversal)) {
            Ok(aside) => undone.push(Undone { record, aside }),
            Err(why) => skipped.push(Skipped {
                item: record.shown().to_path_buf(),
                why,
            }),
        }
    }
    (undone, skipped)
}

/// Does the operations of a step that was taken back again, in the order
/// they were first done, each as [`Undone::redo`] says and `apply` carries
/// out. Answers the records of what was done again, as [`perform`] would
/// have, and what was skipped.
pub fn redo_step(undone: Vec<Undone>, apply: Apply<'_>) -> (Vec<Record>, Vec<Skipped>) {
    let mut records = Vec::new();
    let mut skipped = Vec::new();
    for taken_back in undone.into_iter().rev() {
        match taken_back.redo().and_then(|reversal| apply(&reversal)) {
            Ok(trashed) => {
                let mut record = taken_back.record;
                // Where the Trash put the item this time.
                if matches!(record.kind, OpKind::Trash | OpKind::Replace) {
                    record.trashed = trashed;
                }
                records.push(record);
            }
            Err(why) => skipped.push(Skipped {
                item: taken_back.record.shown().to_path_buf(),
                why,
            }),
        }
    }
    (records, skipped)
}

/// How many steps are kept to be taken back.
pub const STEPS_KEPT: usize = 100;

/// What was done to the files of a project, to be taken back and done
/// again. A step is one thing a person did, with every operation it came
/// to: a paste of five items is one step, and one undo.
#[derive(Debug, Default)]
pub struct Steps {
    /// The steps done, oldest first, each under the number it was begun
    /// with.
    done: VecDeque<(u64, Vec<Record>)>,
    /// The steps taken back, the last one taken back last.
    undone: Vec<Vec<Undone>>,
    next: u64,
}

impl Steps {
    /// A new step: the number its operations are kept under.
    pub fn begin(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    /// Keeps `record` as one of the operations of `step`, the oldest step
    /// making room for a new one. What was taken back can no longer be
    /// done again: the files have moved on.
    pub fn keep(&mut self, step: u64, record: Record) {
        self.undone.clear();
        if let Some((_, records)) = self.done.iter_mut().rev().find(|(id, _)| *id == step) {
            records.push(record);
            return;
        }
        self.push(step, vec![record]);
    }

    fn push(&mut self, step: u64, records: Vec<Record>) {
        if self.done.len() == STEPS_KEPT {
            self.done.pop_front();
        }
        self.done.push_back((step, records));
    }

    /// How many steps can be taken back.
    pub fn len(&self) -> usize {
        self.done.len()
    }

    pub fn is_empty(&self) -> bool {
        self.done.is_empty()
    }

    /// Every operation that can be taken back, oldest first.
    pub fn records(&self) -> impl Iterator<Item = &Record> {
        self.done.iter().flat_map(|(_, records)| records)
    }

    /// The step an undo would take back.
    pub fn last(&self) -> Option<&[Record]> {
        self.done.back().map(|(_, records)| records.as_slice())
    }

    /// The step a redo would do again.
    pub fn last_undone(&self) -> Option<&[Undone]> {
        self.undone.last().map(Vec::as_slice)
    }

    /// Takes the last step off, to be taken back.
    pub fn take_last(&mut self) -> Option<Vec<Record>> {
        self.done.pop_back().map(|(_, records)| records)
    }

    /// Takes the step last taken back off, to be done again.
    pub fn take_undone(&mut self) -> Option<Vec<Undone>> {
        self.undone.pop()
    }

    /// A step was taken back, and can be done again. One of which nothing
    /// could be taken back is not kept: it would only be in the way of
    /// the steps before it.
    pub fn took_back(&mut self, undone: Vec<Undone>) {
        if !undone.is_empty() {
            self.undone.push(undone);
        }
    }

    /// A step was done again, and can be taken back as it could the first
    /// time. The other steps taken back stay so: nothing new was done.
    pub fn did_again(&mut self, records: Vec<Record>) {
        if !records.is_empty() {
            let step = self.begin();
            self.push(step, records);
        }
    }

    /// Forgets every step: the project is another one.
    pub fn clear(&mut self) {
        self.done.clear();
        self.undone.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::TempTree;
    use std::collections::HashSet;
    use std::os::unix::fs::symlink;

    /// The names in `dir`, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Another volume, as the rename reports it.
    fn other_volume(_: &Path, _: &Path) -> io::Result<()> {
        const EXDEV: i32 = 18;
        Err(io::Error::from_raw_os_error(EXDEV))
    }

    #[test]
    fn a_copy_is_named_after_the_original_and_numbered_from_two() {
        let copy = |name: &str, folder: bool, taken: &[&str]| {
            let taken: HashSet<&OsStr> = taken.iter().map(OsStr::new).collect();
            first_free(OsStr::new(name), folder, |n| taken.contains(n))
                .into_string()
                .unwrap()
        };
        assert_eq!(copy("a.ts", false, &[]), "a.ts", "free as it is");
        assert_eq!(copy("a.ts", false, &["a.ts"]), "a-copy.ts");
        assert_eq!(copy("a.ts", false, &["a.ts", "a-copy.ts"]), "a-copy-2.ts");
        assert_eq!(
            copy("a.ts", false, &["a.ts", "a-copy.ts", "a-copy-2.ts"]),
            "a-copy-3.ts"
        );
        // Only what follows the last dot is the extension.
        assert_eq!(
            copy("notes.tar.gz", false, &["notes.tar.gz"]),
            "notes.tar-copy.gz"
        );
        assert_eq!(copy(".env", false, &[".env"]), ".env-copy");
        assert_eq!(
            copy(".env.local", false, &[".env.local"]),
            ".env-copy.local"
        );
        assert_eq!(copy("Makefile", false, &["Makefile"]), "Makefile-copy");
        assert_eq!(copy("src", true, &["src"]), "src-copy");
        assert_eq!(copy("v1.2", true, &["v1.2"]), "v1.2-copy", "a folder's dot");
        // A copy's copy continues the series.
        assert_eq!(copy("a-copy.ts", false, &["a-copy.ts"]), "a-copy-2.ts");
        assert_eq!(
            copy("a-copy.ts", false, &["a-copy.ts", "a-copy-2.ts"]),
            "a-copy-3.ts"
        );
        assert_eq!(copy("a-copy-2.ts", false, &["a-copy-2.ts"]), "a-copy-3.ts");
        assert_eq!(copy("a-copy-9", true, &["a-copy-9"]), "a-copy-10");
        // Not names this module writes: an ordinary stem.
        assert_eq!(copy("a-2.ts", false, &["a-2.ts"]), "a-2-copy.ts");
        assert_eq!(
            copy("a-copy-1.ts", false, &["a-copy-1.ts"]),
            "a-copy-1-copy.ts"
        );
        assert_eq!(
            copy("a-copy-02.ts", false, &["a-copy-02.ts"]),
            "a-copy-02-copy.ts"
        );
        assert_eq!(copy("copy.ts", false, &["copy.ts"]), "copy-copy.ts");
    }

    #[test]
    fn a_free_name_counts_dangling_links_as_taken() {
        let tree = TempTree::new("fileops-free-name", &[("a.ts", "a")]);
        let dir = tree.0.as_path();
        assert_eq!(free_name(dir, OsStr::new("b.ts"), false), "b.ts");
        assert_eq!(free_name(dir, OsStr::new("a.ts"), false), "a-copy.ts");
        symlink("missing-target", dir.join("a-copy.ts")).unwrap();
        assert!(!dir.join("a-copy.ts").exists(), "deliberately dangling");
        assert_eq!(free_name(dir, OsStr::new("a.ts"), false), "a-copy-2.ts");
    }

    #[test]
    fn a_file_is_copied_with_its_text_and_permissions() {
        let tree = TempTree::new(
            "fileops-copy-file",
            &[("run.sh", "echo hi\n"), ("out/", "")],
        );
        let dir = tree.0.as_path();
        let source = dir.join("run.sh");
        chmod(&source, 0o750);
        let copy = copy_item(&source, &dir.join("out/run.sh")).unwrap();
        assert_eq!(copy, dir.join("out/run.sh"));
        assert_eq!(fs::read_to_string(&copy).unwrap(), "echo hi\n");
        assert_eq!(mode(&copy), 0o750);
        assert_eq!(fs::read_to_string(&source).unwrap(), "echo hi\n");
        assert_eq!(names(&dir.join("out")), ["run.sh"], "no staging left");
    }

    #[test]
    fn a_copy_never_replaces_what_is_there() {
        let tree = TempTree::new(
            "fileops-copy-kept",
            &[
                ("a.txt", "new"),
                ("out/a.txt", "keep"),
                ("out/b/c.txt", "c"),
            ],
        );
        let dir = tree.0.as_path();
        let source = dir.join("a.txt");
        let occupied = dir.join("out/a.txt");
        assert_eq!(
            copy_item(&source, &occupied).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read_to_string(&occupied).unwrap(), "keep");

        fs::remove_file(&occupied).unwrap();
        symlink("missing-target", &occupied).unwrap();
        assert_eq!(
            copy_item(&source, &occupied).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            fs::read_link(&occupied).unwrap(),
            Path::new("missing-target")
        );

        // A folder over a folder: not merged into it either.
        fs::create_dir(dir.join("b")).unwrap();
        assert_eq!(
            copy_item(&dir.join("b"), &dir.join("out/b"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(names(&dir.join("out")), ["a.txt", "b"]);
        assert_eq!(names(&dir.join("out/b")), ["c.txt"]);
    }

    /// The exclusive move is what protects an entry made while the copy
    /// was being built, after the first look found the name free.
    #[test]
    fn a_staged_copy_does_not_replace_a_late_arrival() {
        let tree = TempTree::new("fileops-copy-late", &[("a.txt", "new"), ("out/", "")]);
        let dir = tree.0.as_path();
        let staging = Staging::new(&dir.join("out")).unwrap();
        let staged = staging.0.join("a.txt");
        copy_tree(
            &dir.join("a.txt"),
            &staged,
            Path::new(""),
            &mut Vec::new(),
            &|| false,
        )
        .unwrap();
        fs::write(dir.join("out/a.txt"), "late").unwrap();
        assert_eq!(
            move_without_replace(&staged, &dir.join("out/a.txt"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        drop(staging);
        assert_eq!(fs::read_to_string(dir.join("out/a.txt")).unwrap(), "late");
        assert_eq!(names(&dir.join("out")), ["a.txt"]);
    }

    #[test]
    fn a_folder_is_copied_whole_with_links_kept_as_links() {
        let tree = TempTree::new(
            "fileops-copy-folder",
            &[
                ("site/index.html", "<p>hi</p>"),
                ("site/css/main.css", "p {}"),
                ("site/empty/", ""),
                ("elsewhere/secret.txt", "s"),
                ("out/", ""),
            ],
        );
        let dir = tree.0.as_path();
        let site = dir.join("site");
        // A link back to the folder holding it, one out of the folder, and
        // one to nothing: following any of them is the bug.
        symlink("..", site.join("css/loop")).unwrap();
        symlink(dir.join("elsewhere"), site.join("outside")).unwrap();
        symlink("missing-target", site.join("dangling")).unwrap();
        chmod(&site.join("css"), 0o750);

        let copy = copy_item(&site, &dir.join("out/site-copy")).unwrap();
        assert_eq!(copy, dir.join("out/site-copy"));
        assert_eq!(
            names(&copy),
            ["css", "dangling", "empty", "index.html", "outside"]
        );
        assert_eq!(names(&copy.join("css")), ["loop", "main.css"]);
        assert_eq!(names(&copy.join("empty")), [] as [&str; 0]);
        assert_eq!(
            fs::read_to_string(copy.join("css/main.css")).unwrap(),
            "p {}"
        );
        assert_eq!(
            fs::read_link(copy.join("css/loop")).unwrap(),
            Path::new("..")
        );
        assert_eq!(
            fs::read_link(copy.join("outside")).unwrap(),
            dir.join("elsewhere")
        );
        assert_eq!(
            fs::read_link(copy.join("dangling")).unwrap(),
            Path::new("missing-target")
        );
        assert_eq!(mode(&copy.join("css")), 0o750);
        assert_eq!(names(&dir.join("out")), ["site-copy"], "no staging left");
        assert_eq!(names(&dir.join("elsewhere")), ["secret.txt"]);
    }

    /// A folder nobody may write to is still filled, moved into place and
    /// only then closed.
    #[test]
    fn a_read_only_folder_keeps_its_permissions() {
        let tree = TempTree::new("fileops-copy-readonly", &[("docs/deep/a.txt", "a")]);
        let dir = tree.0.as_path();
        let docs = dir.join("docs");
        chmod(&docs.join("deep"), 0o555);
        chmod(&docs, 0o555);
        let copied = copy_item(&docs, &dir.join("docs-copy"));
        let modes = copied
            .as_ref()
            .ok()
            .map(|copy| (mode(copy), mode(&copy.join("deep"))));
        // Writable again before anything is asserted, so the folder can be
        // removed whatever happens.
        for folder in [&docs, &dir.join("docs-copy")] {
            let _ = fs::set_permissions(folder, fs::Permissions::from_mode(0o755));
            let _ = fs::set_permissions(folder.join("deep"), fs::Permissions::from_mode(0o755));
        }
        assert_eq!(copied.unwrap(), dir.join("docs-copy"));
        assert_eq!(modes, Some((0o555, 0o555)));
        assert_eq!(
            fs::read_to_string(dir.join("docs-copy/deep/a.txt")).unwrap(),
            "a"
        );
    }

    #[test]
    fn a_copy_that_fails_leaves_nothing_behind() {
        let tree = TempTree::new(
            "fileops-copy-fails",
            &[("src/a.txt", "a"), ("src/b/locked.txt", "b"), ("out/", "")],
        );
        let dir = tree.0.as_path();
        // Unreadable: the copy stops partway through the folder.
        chmod(&dir.join("src/b/locked.txt"), 0o000);
        let error = copy_item(&dir.join("src"), &dir.join("out/src")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(names(&dir.join("out")), [] as [&str; 0]);
        assert_eq!(names(&dir.join("src")), ["a.txt", "b"], "source untouched");

        let missing = copy_item(&dir.join("absent.txt"), &dir.join("out/absent.txt"));
        assert_eq!(missing.unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(names(&dir.join("out")), [] as [&str; 0]);
    }

    #[test]
    fn a_folder_is_not_copied_or_moved_into_itself() {
        let tree = TempTree::new(
            "fileops-nesting",
            &[("src/render/font.rs", "f"), ("other/", "")],
        );
        let dir = tree.0.as_path();
        let src = dir.join("src");
        // The same folder by another path.
        symlink(&src, dir.join("other/alias")).unwrap();
        for inside in [
            src.join("src"),
            src.join("render/src"),
            dir.join("other/alias/src"),
            dir.join("other/alias/render/src"),
        ] {
            assert_eq!(
                copy_item(&src, &inside).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "copy to {}",
                inside.display()
            );
            assert_eq!(
                move_item(&src, &inside).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "move to {}",
                inside.display()
            );
        }
        assert_eq!(names(&src), ["render"]);
        assert_eq!(names(&src.join("render")), ["font.rs"]);
        // Beside itself is a copy like any other.
        copy_item(&src, &dir.join("src-copy")).unwrap();
        assert_eq!(names(&dir.join("src-copy/render")), ["font.rs"]);
    }

    #[test]
    fn a_link_is_copied_moved_and_removed_as_the_link() {
        let tree = TempTree::new(
            "fileops-links",
            &[("real/a.txt", "a"), ("out/", ""), ("bin/", "")],
        );
        let dir = tree.0.as_path();
        let link = dir.join("link");
        symlink("real", &link).unwrap();

        let copy = copy_item(&link, &dir.join("out/link")).unwrap();
        assert_eq!(fs::read_link(&copy).unwrap(), Path::new("real"));
        // Into the folder it points at: the link moves, nothing nests.
        let moved = move_item(&link, &dir.join("real/link")).unwrap();
        assert_eq!(moved.path, dir.join("real/link"));
        assert_eq!(fs::read_link(&moved.path).unwrap(), Path::new("real"));
        assert!(fs::symlink_metadata(&link).is_err());

        // Across volumes: copied as the link, and the link removed.
        let across = move_with(
            &moved.path,
            &dir.join("bin/link"),
            other_volume,
            remove_item,
        )
        .unwrap();
        assert!(across.original_left.is_none());
        assert_eq!(fs::read_link(&across.path).unwrap(), Path::new("real"));
        assert_eq!(names(&dir.join("real")), ["a.txt"], "the target is kept");
    }

    #[test]
    fn a_move_never_replaces_what_is_there() {
        let tree = TempTree::new(
            "fileops-move",
            &[("a.txt", "a"), ("b.txt", "b"), ("out/b.txt", "keep")],
        );
        let dir = tree.0.as_path();
        let moved = move_item(&dir.join("a.txt"), &dir.join("out/a.txt")).unwrap();
        assert_eq!(moved.path, dir.join("out/a.txt"));
        assert!(moved.original_left.is_none());
        assert!(!dir.join("a.txt").exists());
        assert_eq!(fs::read_to_string(&moved.path).unwrap(), "a");

        for rename in [move_without_replace, other_volume] {
            let error = move_with(
                &dir.join("b.txt"),
                &dir.join("out/b.txt"),
                rename,
                remove_item,
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(fs::read_to_string(dir.join("out/b.txt")).unwrap(), "keep");
            assert_eq!(fs::read_to_string(dir.join("b.txt")).unwrap(), "b");
        }
        assert_eq!(names(&dir.join("out")), ["a.txt", "b.txt"]);
    }

    #[test]
    fn a_move_to_another_volume_copies_then_removes_the_original() {
        let tree = TempTree::new(
            "fileops-move-across",
            &[
                ("site/index.html", "<p>hi</p>"),
                ("site/css/main.css", "p {}"),
                ("notes.txt", "n"),
                ("kept.txt", "k"),
                ("out/", ""),
                ("bin/", ""),
            ],
        );
        let dir = tree.0.as_path();

        // The default: the original is deleted.
        let moved = move_with(
            &dir.join("site"),
            &dir.join("out/site"),
            other_volume,
            remove_item,
        )
        .unwrap();
        assert_eq!(moved.path, dir.join("out/site"));
        assert!(moved.original_left.is_none());
        assert!(!dir.join("site").exists());
        assert_eq!(
            fs::read_to_string(dir.join("out/site/css/main.css")).unwrap(),
            "p {}"
        );

        // The caller's removal: here the original is put aside instead.
        let aside = dir.join("bin/notes.txt");
        let moved = move_with(
            &dir.join("notes.txt"),
            &dir.join("out/notes.txt"),
            other_volume,
            |original| fs::rename(original, &aside),
        )
        .unwrap();
        assert!(moved.original_left.is_none());
        assert_eq!(fs::read_to_string(&moved.path).unwrap(), "n");
        assert_eq!(fs::read_to_string(&aside).unwrap(), "n");
        assert!(!dir.join("notes.txt").exists());

        // A removal that fails: the copy stays, and so does the original.
        let moved = move_with(
            &dir.join("kept.txt"),
            &dir.join("out/kept.txt"),
            other_volume,
            |_| Err(io::ErrorKind::PermissionDenied.into()),
        )
        .unwrap();
        assert_eq!(
            moved.original_left.map(|error| error.kind()),
            Some(io::ErrorKind::PermissionDenied)
        );
        assert_eq!(fs::read_to_string(&moved.path).unwrap(), "k");
        assert_eq!(fs::read_to_string(dir.join("kept.txt")).unwrap(), "k");
        assert_eq!(names(&dir.join("out")), ["kept.txt", "notes.txt", "site"]);
    }

    #[test]
    fn inside_is_judged_by_where_the_paths_really_are() {
        let tree = TempTree::new("fileops-inside", &[("src/render/", ""), ("docs/", "")]);
        let dir = tree.0.as_path();
        let src = dir.join("src");
        symlink(&src, dir.join("alias")).unwrap();

        assert!(is_inside(&src, &src));
        assert!(is_inside(&src.join("render"), &src));
        assert!(is_inside(&dir.join("alias"), &src), "the folder, linked");
        assert!(is_inside(&dir.join("alias/render"), &src));
        assert!(is_inside(&src.join("not/made/yet"), &src));
        assert!(!is_inside(&dir.join("docs"), &src));
        assert!(!is_inside(dir, &src), "its parent");
        // The link itself is an item with nothing inside it.
        assert!(!is_inside(&src.join("render"), &dir.join("alias")));
        assert_eq!(
            located(&dir.join("alias")),
            resolved(dir).join("alias"),
            "the link, not its target"
        );
        assert_eq!(resolved(&dir.join("alias")), resolved(&src));
        // Nothing on disk: compared as written, by whole components.
        assert!(is_inside(Path::new("/p/src/render"), Path::new("/p/src")));
        assert!(!is_inside(
            Path::new("/p/src-generated"),
            Path::new("/p/src")
        ));
    }

    /// An environment for operations that neither trash nor stop.
    fn plain() -> Env<'static> {
        Env {
            trash: &|_| Err(io::ErrorKind::Unsupported.into()),
            stop: &|| false,
        }
    }

    fn copy_to(from: &Path, to: &Path) -> Planned {
        Planned::Copy {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        }
    }

    fn move_to(from: &Path, to: &Path) -> Planned {
        Planned::Move {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        }
    }

    fn refused(from: &Path, why: Refusal) -> Planned {
        Planned::Refused {
            from: from.to_path_buf(),
            why,
        }
    }

    #[test]
    fn a_paste_plan_names_each_item_and_replaces_nothing() {
        let tree = TempTree::new(
            "fileops-plan-paste",
            &[
                ("notes/a.txt", "n"),
                ("notes/b.txt", "b"),
                ("photos/a.txt", "p"),
                ("photos/v1.2/", ""),
                ("out/a.txt", "keep"),
                ("empty/", ""),
            ],
        );
        let dir = tree.0.as_path();
        let (notes, photos, out) = (dir.join("notes"), dir.join("photos"), dir.join("out"));
        let before = names(&out);

        // Copied: its own name when free, the next free one otherwise, and
        // a name given earlier in the same paste is taken too.
        let sources = [
            notes.join("a.txt"),
            photos.join("a.txt"),
            notes.join("b.txt"),
        ];
        assert_eq!(
            plan_paste(&sources, &out, false),
            [
                copy_to(&sources[0], &out.join("a-copy.txt")),
                copy_to(&sources[1], &out.join("a-copy-2.txt")),
                copy_to(&sources[2], &out.join("b.txt")),
            ]
        );
        assert_eq!(
            plan_paste(&sources[..2], &dir.join("empty"), false),
            [
                copy_to(&sources[0], &dir.join("empty/a.txt")),
                copy_to(&sources[1], &dir.join("empty/a-copy.txt")),
            ]
        );
        // Into the folder the original is in: both are kept.
        assert_eq!(
            plan_paste(&sources[..1], &notes, false),
            [copy_to(&sources[0], &notes.join("a-copy.txt"))]
        );
        // A folder's dots are part of its name.
        let folder = photos.join("v1.2");
        assert_eq!(
            plan_paste(std::slice::from_ref(&folder), &photos, false),
            [copy_to(&folder, &photos.join("v1.2-copy"))]
        );

        // Cut: moved, a clash resolved the same way; where it already is,
        // it stays.
        assert_eq!(
            plan_paste(&sources, &out, true),
            [
                move_to(&sources[0], &out.join("a-copy.txt")),
                move_to(&sources[1], &out.join("a-copy-2.txt")),
                move_to(&sources[2], &out.join("b.txt")),
            ]
        );
        assert_eq!(
            plan_paste(&sources, &notes, true),
            [
                Planned::Stays(sources[0].clone()),
                move_to(&sources[1], &notes.join("a-copy.txt")),
                Planned::Stays(sources[2].clone()),
            ]
        );

        // A plan touches nothing.
        assert_eq!(names(&out), before);
        assert_eq!(names(&notes), ["a.txt", "b.txt"]);
        assert_eq!(fs::read_to_string(out.join("a.txt")).unwrap(), "keep");
    }

    #[test]
    fn a_paste_plan_refuses_a_folder_into_itself_and_what_is_gone() {
        let tree = TempTree::new(
            "fileops-plan-refuse",
            &[("photos/trip/day.txt", "d"), ("out/", "")],
        );
        let dir = tree.0.as_path();
        let photos = dir.join("photos");
        let gone = dir.join("gone.txt");
        // The folder by another path, and a link that is an item itself.
        symlink(&photos, dir.join("out/alias")).unwrap();
        symlink("photos", dir.join("link")).unwrap();

        for cut in [false, true] {
            // Inside it, and the folder itself reached by another path: a
            // paste there is into it. On its own row it goes beside it.
            for inside in [
                photos.join("trip"),
                dir.join("out/alias"),
                dir.join("out/alias/trip"),
            ] {
                assert_eq!(
                    plan_paste(std::slice::from_ref(&photos), &inside, cut),
                    [refused(&photos, Refusal::IntoItself)],
                    "into {} (cut: {cut})",
                    inside.display()
                );
            }
            assert_eq!(
                plan_paste(std::slice::from_ref(&gone), &dir.join("out"), cut),
                [refused(&gone, Refusal::Missing)]
            );
        }
        // One refusal does not stop the rest of the paste.
        let day = photos.join("trip/day.txt");
        assert_eq!(
            plan_paste(&[photos.clone(), day.clone()], &photos.join("trip"), false),
            [
                refused(&photos, Refusal::IntoItself),
                copy_to(&day, &photos.join("trip/day-copy.txt")),
            ]
        );
        // Beside itself is a copy like any other, and a link goes into
        // the folder it points at: it is the link that is copied.
        assert_eq!(
            plan_paste(std::slice::from_ref(&photos), dir, false),
            [copy_to(&photos, &dir.join("photos-copy"))]
        );
        let link = dir.join("link");
        assert_eq!(
            plan_paste(std::slice::from_ref(&link), &photos, false),
            [copy_to(&link, &photos.join("link"))]
        );
        assert_eq!(Refusal::IntoItself.said(), "cannot be pasted into itself");
        assert_eq!(Refusal::Missing.said(), "is no longer there");
    }

    /// Select a folder, copy, paste: the cursor is still on the folder
    /// copied. That used to be refused as a paste into itself; it is a
    /// paste beside it, as the same keys on a file are.
    #[test]
    fn a_paste_onto_one_of_the_items_goes_beside_it() {
        let tree = TempTree::new(
            "fileops-plan-beside",
            &[
                ("photos/trip/day.txt", "d"),
                ("photos/list.txt", "l"),
                ("notes.txt", "n"),
            ],
        );
        let dir = tree.0.as_path();
        let photos = dir.join("photos");
        let one = std::slice::from_ref(&photos);

        // Copied and pasted at once, then again: `-copy`, then `-copy-2`.
        assert_eq!(
            plan_paste(one, &photos, false),
            [copy_to(&photos, &dir.join("photos-copy"))]
        );
        fs::create_dir(dir.join("photos-copy")).unwrap();
        assert_eq!(
            plan_paste(one, &photos, false),
            [copy_to(&photos, &dir.join("photos-copy-2"))]
        );
        // A folder inside it is still inside it.
        assert_eq!(
            plan_paste(one, &photos.join("trip"), false),
            [refused(&photos, Refusal::IntoItself)]
        );
        // Everything pasted with it goes to the same folder: what was in
        // the folder lands beside it too, and what is there already gets
        // a name of its own.
        let (list, notes) = (photos.join("list.txt"), dir.join("notes.txt"));
        assert_eq!(
            plan_paste(
                &[photos.clone(), list.clone(), notes.clone()],
                &photos,
                false
            ),
            [
                copy_to(&photos, &dir.join("photos-copy-2")),
                copy_to(&list, &dir.join("list.txt")),
                copy_to(&notes, &dir.join("notes-copy.txt")),
            ]
        );
        // A nested folder's row: beside it, in the folder it is in.
        let trip = photos.join("trip");
        assert_eq!(
            plan_paste(std::slice::from_ref(&trip), &trip, false),
            [copy_to(&trip, &photos.join("trip-copy"))]
        );

        // Cut and pasted on itself: it is where it would go, and stays;
        // what was cut with it from elsewhere moves beside it.
        assert_eq!(
            plan_paste(one, &photos, true),
            [Planned::Stays(photos.clone())]
        );
        assert_eq!(
            plan_paste(&[photos.clone(), list.clone()], &photos, true),
            [
                Planned::Stays(photos.clone()),
                move_to(&list, &dir.join("list.txt")),
            ]
        );

        // A link to a folder is a row that can be pasted on, too: the
        // link is copied beside itself, not into what it points at.
        let link = dir.join("link");
        symlink("photos", &link).unwrap();
        assert_eq!(
            plan_paste(std::slice::from_ref(&link), &link, false),
            [copy_to(&link, &dir.join("link-copy"))]
        );
        assert_eq!(
            names(dir),
            ["link", "notes.txt", "photos", "photos-copy"],
            "a plan touches nothing"
        );
    }

    #[test]
    fn a_duplicate_plan_puts_a_copy_beside_each_item() {
        let tree = TempTree::new(
            "fileops-plan-duplicate",
            &[
                ("a.txt", "a"),
                ("a-copy.txt", "taken"),
                ("docs/guide.md", "g"),
            ],
        );
        let dir = tree.0.as_path();
        let sources = [
            dir.join("a.txt"),
            dir.join("docs"),
            dir.join("docs/guide.md"),
            dir.join("gone.txt"),
            // The same item twice: each copy has a name of its own.
            dir.join("a.txt"),
        ];
        assert_eq!(
            plan_duplicate(&sources),
            [
                copy_to(&sources[0], &dir.join("a-copy-2.txt")),
                copy_to(&sources[1], &dir.join("docs-copy")),
                copy_to(&sources[2], &dir.join("docs/guide-copy.md")),
                refused(&sources[3], Refusal::Missing),
                copy_to(&sources[4], &dir.join("a-copy-3.txt")),
            ]
        );
        assert_eq!(names(dir), ["a-copy.txt", "a.txt", "docs"]);
    }

    #[test]
    fn every_operation_answers_a_record_of_what_it_did() {
        let tree = TempTree::new(
            "fileops-perform",
            &[("a.txt", "a"), ("docs/guide.md", "g"), ("bin/", "")],
        );
        let dir = tree.0.as_path();
        let env = plain();
        let at = |path: &str| dir.join(path);
        let summary = |record: &Record| {
            (
                record.kind,
                record.source.clone(),
                record.result.clone(),
                record.trashed.clone(),
            )
        };

        // New: the folders above are made, and nothing is truncated.
        let made = perform(Op::NewFile(&at("new/deep/b.txt")), &env).unwrap();
        assert_eq!(
            summary(&made),
            (OpKind::NewFile, None, Some(at("new/deep/b.txt")), None)
        );
        assert_eq!(fs::read_to_string(at("new/deep/b.txt")).unwrap(), "");
        let again = perform(Op::NewFile(&at("a.txt")), &env).unwrap_err();
        assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(at("a.txt")).unwrap(), "a");
        let made = perform(Op::NewFolder(&at("new/more")), &env).unwrap();
        assert_eq!(
            summary(&made),
            (OpKind::NewFolder, None, Some(at("new/more")), None)
        );
        assert!(at("new/more").is_dir());
        for taken in ["docs", "a.txt"] {
            let error = perform(Op::NewFolder(&at(taken)), &env).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{taken}");
        }

        // Renamed and moved: where it was, and where it is.
        let (from, to) = (at("a.txt"), at("first.txt"));
        let renamed = perform(
            Op::Rename {
                from: &from,
                to: &to,
            },
            &env,
        )
        .unwrap();
        assert_eq!(
            summary(&renamed),
            (OpKind::Rename, Some(from.clone()), Some(to.clone()), None)
        );
        assert!(renamed.original_left.is_none());
        let moved = perform(
            Op::Move {
                from: &to,
                to: &at("docs/first.txt"),
            },
            &env,
        )
        .unwrap();
        assert_eq!(
            summary(&moved),
            (
                OpKind::Move,
                Some(to.clone()),
                Some(at("docs/first.txt")),
                None
            )
        );
        assert!(!to.exists());

        // Copied and duplicated: both are there after.
        let copied = perform(
            Op::Copy {
                from: &at("docs"),
                to: &at("new/docs"),
            },
            &env,
        )
        .unwrap();
        assert_eq!(
            summary(&copied),
            (OpKind::Copy, Some(at("docs")), Some(at("new/docs")), None)
        );
        assert_eq!(names(&at("new/docs")), ["first.txt", "guide.md"]);
        let duplicate = perform(
            Op::Duplicate {
                from: &at("docs/guide.md"),
                to: &at("docs/guide-copy.md"),
            },
            &env,
        )
        .unwrap();
        assert_eq!(duplicate.kind, OpKind::Duplicate);
        assert_eq!(
            names(&at("docs")),
            ["first.txt", "guide-copy.md", "guide.md"]
        );
        // Never over what is there.
        let clash = perform(
            Op::Copy {
                from: &at("docs/guide.md"),
                to: &at("docs/first.txt"),
            },
            &env,
        )
        .unwrap_err();
        assert_eq!(clash.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(at("docs/first.txt")).unwrap(), "a");

        // Trashed: where the item went, as the Trash says. This one is a
        // folder of the fixture, standing in for the real one.
        let bin = at("bin");
        let env = Env {
            trash: &|path| {
                let landed = bin.join(path.file_name().unwrap());
                fs::rename(path, &landed)?;
                Ok(Some(landed))
            },
            stop: &|| false,
        };
        let trashed = perform(Op::Trash(&at("new/docs")), &env).unwrap();
        assert_eq!(
            summary(&trashed),
            (
                OpKind::Trash,
                Some(at("new/docs")),
                None,
                Some(at("bin/docs"))
            )
        );
        assert!(!at("new/docs").exists());
        assert_eq!(names(&at("bin/docs")), ["first.txt", "guide.md"]);
    }

    #[test]
    fn a_stopped_copy_leaves_nothing_and_a_finished_one_stays() {
        let tree = TempTree::new(
            "fileops-copy-stopped",
            &[
                ("site/a.txt", "a"),
                ("site/b.txt", "b"),
                ("site/css/main.css", "p {}"),
                ("out/", ""),
            ],
        );
        let dir = tree.0.as_path();
        let (site, out) = (dir.join("site"), dir.join("out"));
        // Stopped at every point a copy of this folder can be: before the
        // folder itself, and before each of the five items in it.
        for allowed in 0..5 {
            let asked = std::cell::Cell::new(0);
            let stop = || {
                asked.set(asked.get() + 1);
                asked.get() > allowed
            };
            let error = copy_item_until(&site, &out.join("site"), &stop).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Interrupted, "after {allowed}");
            assert_eq!(names(&out), [] as [&str; 0], "after {allowed}");
        }
        assert_eq!(names(&site), ["a.txt", "b.txt", "css"], "source untouched");

        // Through `perform`, and a staging folder is named as one while
        // the copy is being built.
        let seen = std::cell::RefCell::new(Vec::new());
        let env = Env {
            trash: &|_| Err(io::ErrorKind::Unsupported.into()),
            stop: &|| {
                seen.borrow_mut().extend(names(&out));
                false
            },
        };
        let copied = perform(
            Op::Copy {
                from: &site,
                to: &out.join("site"),
            },
            &env,
        )
        .unwrap();
        assert_eq!(copied.result, Some(out.join("site")));
        assert_eq!(names(&out), ["site"]);
        let seen = seen.into_inner();
        assert!(!seen.is_empty());
        for name in &seen {
            assert!(is_staging(OsStr::new(name)), "{name} while copying");
        }
        assert!(!is_staging(OsStr::new("site")));
        assert!(!is_staging(OsStr::new(".crc-notes.tmp")));
    }

    #[test]
    fn an_item_takes_the_next_free_name_when_its_own_was_taken_since() {
        let tree = TempTree::new(
            "fileops-perform-free",
            &[
                ("a.txt", "new"),
                ("b.txt", "moved"),
                ("out/a.txt", "late"),
                ("out/b.txt", "late"),
                ("out/b-copy.txt", "later"),
            ],
        );
        let dir = tree.0.as_path();
        let env = plain();
        let copied = perform_free(
            Op::Copy {
                from: &dir.join("a.txt"),
                to: &dir.join("out/a.txt"),
            },
            &env,
        )
        .unwrap();
        assert_eq!(copied.kind, OpKind::Copy);
        assert_eq!(copied.result, Some(dir.join("out/a-copy.txt")));
        assert_eq!(fs::read_to_string(dir.join("out/a.txt")).unwrap(), "late");
        assert_eq!(
            fs::read_to_string(dir.join("out/a-copy.txt")).unwrap(),
            "new"
        );

        let moved = perform_free(
            Op::Move {
                from: &dir.join("b.txt"),
                to: &dir.join("out/b.txt"),
            },
            &env,
        )
        .unwrap();
        assert_eq!(moved.kind, OpKind::Move);
        assert_eq!(moved.result, Some(dir.join("out/b-copy-2.txt")));
        assert!(!dir.join("b.txt").exists());
        assert_eq!(fs::read_to_string(dir.join("out/b.txt")).unwrap(), "late");
        assert_eq!(
            fs::read_to_string(dir.join("out/b-copy.txt")).unwrap(),
            "later"
        );
        // Anything but a taken name is the failure it is.
        let missing = perform_free(
            Op::Copy {
                from: &dir.join("absent.txt"),
                to: &dir.join("out/absent.txt"),
            },
            &env,
        )
        .unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    }

    /// A dragged item as the paths alone say it is, for rules that read
    /// nothing.
    fn dragged(path: &str, folder: bool) -> Dragged {
        Dragged {
            path: PathBuf::from(path),
            at: PathBuf::from(path),
            folder,
        }
    }

    fn onto(dir: &str) -> DropFolder {
        DropFolder {
            dir: PathBuf::from(dir),
            real: PathBuf::from(dir),
        }
    }

    #[test]
    fn a_drop_on_itself_or_where_it_is_stays_and_into_itself_is_refused() {
        let src = dragged("/p/src", true);
        let file = dragged("/p/src/main.rs", false);
        for copy in [false, true] {
            // Onto itself: nothing to do, moved or copied.
            assert_eq!(landing(&src, &onto("/p/src"), copy), Landing::Stays);
            // Into a folder inside itself, however deep.
            for inside in ["/p/src/render", "/p/src/render/font"] {
                assert_eq!(
                    landing(&src, &onto(inside), copy),
                    Landing::IntoItself,
                    "{inside} (copy: {copy})"
                );
            }
            // Somewhere else is a move, or a copy.
            assert_eq!(landing(&file, &onto("/p/docs"), copy), Landing::Goes);
            assert_eq!(landing(&src, &onto("/p/docs"), copy), Landing::Goes);
            // A sibling whose name starts with the folder's is not inside
            // it: paths compare whole components.
            assert_eq!(
                landing(&src, &onto("/p/src-generated"), copy),
                Landing::Goes
            );
        }
        // The folder it is in already: a move has nothing to do, and a
        // copy is made beside the original.
        assert_eq!(landing(&file, &onto("/p/src"), false), Landing::Stays);
        assert_eq!(landing(&src, &onto("/p"), false), Landing::Stays);
        assert_eq!(landing(&file, &onto("/p/src"), true), Landing::Goes);
        assert_eq!(landing(&src, &onto("/p"), true), Landing::Goes);
    }

    /// The same folder under another name: as written, `alias/render` is
    /// nowhere near `src`, and a move into it would still be a folder
    /// moved inside itself.
    #[test]
    fn a_drop_is_judged_through_symbolic_links() {
        let tree = TempTree::new(
            "fileops-drop-link",
            &[("src/render/font.rs", "f"), ("docs/", "")],
        );
        let root = tree.0.as_path();
        let src = root.join("src");
        let alias = root.join("alias");
        symlink(&src, &alias).unwrap();
        let allowed = |item: &Path, dir: &Path| {
            let mut check = DropCheck::new(vec![item.to_path_buf()]);
            check.aim(Some(dir));
            check.allowed(false)
        };

        assert!(!allowed(&src, &alias), "onto itself");
        assert!(!allowed(&src, &alias.join("render")), "into a child");
        assert!(!allowed(&src.join("render"), &alias), "where it already is");
        assert!(
            !allowed(&alias.join("render/font.rs"), &src.join("render")),
            "where it already is, the item through the link"
        );
        // The link is an item of its own: it can go into the folder it
        // points at, other things can be dropped through it, and on its
        // own row it stays.
        assert!(allowed(&alias, &src.join("render")));
        assert!(allowed(&root.join("docs"), &alias));
        assert!(!allowed(&alias, root), "the link is already there");
        assert!(!allowed(&alias, &alias), "the link on its own row");
        // Over nothing, nothing is allowed.
        let mut nowhere = DropCheck::new(vec![src.clone()]);
        nowhere.aim(None);
        assert!(!nowhere.allowed(false));
        assert_eq!(nowhere.plan(false), []);
    }

    /// A drag asks on every move of the pointer. The folder under it is
    /// looked up when it changes, and never again while it is the same.
    #[test]
    fn a_drop_folder_is_looked_up_once_for_as_long_as_the_drag_is_over_it() {
        let looked_up = std::cell::Cell::new(0);
        let resolve = |dir: &Path| {
            looked_up.set(looked_up.get() + 1);
            dir.to_path_buf()
        };
        let mut check = DropCheck {
            items: vec![dragged("/p/garden.txt", false), dragged("/p/trips", true)],
            onto: None,
        };
        assert!(!check.allowed(false), "over nothing yet");
        for _ in 0..50 {
            check.aim_with(Some(Path::new("/p/recipes")), resolve);
            assert!(check.allowed(false));
            assert!(check.allowed(true));
        }
        assert_eq!(looked_up.get(), 1);
        // Another folder, then off the tree, then back: once each time it
        // comes to a folder.
        for _ in 0..50 {
            check.aim_with(Some(Path::new("/p")), resolve);
            assert!(!check.allowed(false), "both are in it already");
            assert!(check.allowed(true), "a copy beside the originals");
        }
        assert_eq!(looked_up.get(), 2);
        check.aim_with(None, resolve);
        check.aim_with(None, resolve);
        assert_eq!(looked_up.get(), 2);
        assert_eq!(check.dir(), None);
        check.aim_with(Some(Path::new("/p/trips/lisbon")), resolve);
        assert_eq!(looked_up.get(), 3);
        // One item can go and one cannot: the drop is allowed for the one.
        assert!(check.allowed(false));
        assert_eq!(check.len(), 2);
    }

    #[test]
    fn a_drop_plan_moves_what_can_go_and_skips_a_taken_name() {
        let tree = TempTree::new(
            "fileops-plan-drop",
            &[
                ("recipes/bread.md", "flour"),
                ("recipes/soup.md", "leeks"),
                ("trips/lisbon.md", "tram 28"),
                ("trips/soup.md", "a bowl in alfama"),
                ("old/bread.md", "stale"),
                ("garden.txt", "tomatoes"),
                ("archive/", ""),
            ],
        );
        let dir = tree.0.as_path();
        let (recipes, trips) = (dir.join("recipes"), dir.join("trips"));
        let (bread, soup) = (recipes.join("bread.md"), recipes.join("soup.md"));
        let garden = dir.join("garden.txt");
        let plan = |paths: &[&Path], onto: &Path, copy: bool| {
            let mut check = DropCheck::new(paths.iter().map(|p| p.to_path_buf()).collect());
            check.aim(Some(onto));
            check.plan(copy)
        };

        // Several items go together; the one whose name is in use there is
        // skipped, and the rest still go.
        assert_eq!(
            plan(&[&bread, &soup, &garden], &trips, false),
            [
                move_to(&bread, &trips.join("bread.md")),
                refused(&soup, Refusal::Taken),
                move_to(&garden, &trips.join("garden.txt")),
            ]
        );
        // Two items of one name from two folders: the first goes, and the
        // second would replace it.
        let stale = dir.join("old/bread.md");
        assert_eq!(
            plan(&[&stale, &bread], &dir.join("archive"), false),
            [
                move_to(&stale, &dir.join("archive/bread.md")),
                refused(&bread, Refusal::Taken),
            ]
        );
        // A dangling link has the name as much as a file has.
        symlink("nowhere", dir.join("archive/garden.txt")).unwrap();
        assert_eq!(
            plan(&[&garden], &dir.join("archive"), false),
            [refused(&garden, Refusal::Taken)]
        );
        // Copied, nothing is skipped: a name in use gives way to `-copy`,
        // and a copy into the folder it is in lands beside the original.
        assert_eq!(
            plan(&[&bread, &soup], &trips, true),
            [
                copy_to(&bread, &trips.join("bread.md")),
                copy_to(&soup, &trips.join("soup-copy.md")),
            ]
        );
        assert_eq!(
            plan(&[&bread, &recipes], &recipes, true),
            [
                copy_to(&bread, &recipes.join("bread-copy.md")),
                Planned::Stays(recipes.clone()),
            ]
        );
        // Where it is, on itself, into itself, and gone meanwhile.
        assert_eq!(
            plan(&[&bread, &recipes, &trips], dir, false),
            [
                move_to(&bread, &dir.join("bread.md")),
                Planned::Stays(recipes.clone()),
                Planned::Stays(trips.clone()),
            ]
        );
        fs::create_dir(recipes.join("baking")).unwrap();
        assert_eq!(
            plan(&[&recipes, &garden], &recipes.join("baking"), false),
            [
                refused(&recipes, Refusal::IntoItself),
                move_to(&garden, &recipes.join("baking/garden.txt")),
            ]
        );
        let mut gone = DropCheck::new(vec![garden.clone()]);
        gone.aim(Some(&trips));
        fs::remove_file(&garden).unwrap();
        assert_eq!(gone.plan(false), [refused(&garden, Refusal::Missing)]);
        assert_eq!(
            Refusal::Taken.said(),
            "would replace an item of the same name"
        );

        // A plan touches nothing.
        assert_eq!(names(&trips), ["lisbon.md", "soup.md"]);
        assert_eq!(names(&recipes), ["baking", "bread.md", "soup.md"]);
    }

    fn replace_at(from: &Path, to: &Path) -> Planned {
        Planned::Replace {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        }
    }

    /// Files dragged in from another app, onto a folder of the project
    /// where some of their names are in use: what the one question is
    /// about, and what each answer to it does.
    #[test]
    fn a_drop_from_outside_is_planned_as_the_question_was_answered() {
        let tree = TempTree::new(
            "fileops-plan-outside",
            &[
                ("desk/soup.md", "pumpkin"),
                ("desk/plots.txt", "beans by the wall"),
                ("desk/lisbon.md", "a second visit"),
                ("desk/later/soup.md", "miso"),
                ("project/trips/lisbon.md", "tram 28"),
                ("project/trips/soup.md", "a bowl in alfama"),
                ("project/trips/soup-copy.md", "another bowl"),
                ("project/garden.txt", "tomatoes"),
            ],
        );
        let dir = tree.0.as_path();
        let (desk, project) = (dir.join("desk"), dir.join("project"));
        let trips = project.join("trips");
        let (soup, plots) = (desk.join("soup.md"), desk.join("plots.txt"));
        let lisbon = desk.join("lisbon.md");
        let check = |paths: &[&Path], onto: &Path| {
            let mut check = DropCheck::new(paths.iter().map(|p| p.to_path_buf()).collect());
            check.aim(Some(onto));
            check
        };

        // No name in use where it lands: nothing to ask, and either answer
        // plans the same copy.
        let free = check(&[&plots, &soup], &project);
        assert!(free.allowed(false));
        assert_eq!(free.taken_there(), [] as [&Path; 0]);
        for answer in [OnTaken::Replace, OnTaken::KeepBoth] {
            assert_eq!(
                free.plan_from_outside(answer),
                [
                    copy_to(&plots, &project.join("plots.txt")),
                    copy_to(&soup, &project.join("soup.md")),
                ]
            );
        }

        // Two of three names are in use: the question is about those two.
        let clash = check(&[&soup, &plots, &lisbon], &trips);
        assert_eq!(clash.taken_there(), [soup.as_path(), lisbon.as_path()]);
        // Replace: each takes the name, in place of what has it.
        assert_eq!(
            clash.plan_from_outside(OnTaken::Replace),
            [
                replace_at(&soup, &trips.join("soup.md")),
                copy_to(&plots, &trips.join("plots.txt")),
                replace_at(&lisbon, &trips.join("lisbon.md")),
            ]
        );
        // Keep Both: the next free `-copy` name, past one already there.
        assert_eq!(
            clash.plan_from_outside(OnTaken::KeepBoth),
            [
                copy_to(&soup, &trips.join("soup-copy-2.md")),
                copy_to(&plots, &trips.join("plots.txt")),
                copy_to(&lisbon, &trips.join("lisbon-copy.md")),
            ]
        );

        // Two items of one name in one drop: only what was there before is
        // replaced, and the second is kept beside the first.
        let later = desk.join("later/soup.md");
        let twice = check(&[&soup, &later], &trips);
        assert_eq!(
            twice.plan_from_outside(OnTaken::Replace),
            [
                replace_at(&soup, &trips.join("soup.md")),
                copy_to(&later, &trips.join("soup-copy-2.md")),
            ]
        );
        let twice_free = check(&[&soup, &later], &project);
        assert_eq!(twice_free.taken_there(), [] as [&Path; 0]);
        assert_eq!(
            twice_free.plan_from_outside(OnTaken::Replace),
            [
                copy_to(&soup, &project.join("soup.md")),
                copy_to(&later, &project.join("soup-copy.md")),
            ]
        );

        // Onto the folder it is in already: nothing lights up, nothing is
        // asked and nothing is planned, though its own name is in use
        // there. Beside one that can go, it stays.
        let garden = project.join("garden.txt");
        let own = check(&[&garden], &project);
        assert!(!own.allowed(false));
        assert_eq!(own.taken_there(), [] as [&Path; 0]);
        for answer in [OnTaken::Replace, OnTaken::KeepBoth] {
            assert_eq!(
                own.plan_from_outside(answer),
                [Planned::Stays(garden.clone())]
            );
        }
        let mixed = check(&[&garden, &plots], &project);
        assert!(mixed.allowed(false));
        assert_eq!(
            mixed.plan_from_outside(OnTaken::KeepBoth),
            [
                Planned::Stays(garden.clone()),
                copy_to(&plots, &project.join("plots.txt")),
            ]
        );

        // A folder into itself or its own subtree: refused, and what came
        // with it still goes.
        let into = check(&[&project, &plots], &trips);
        assert!(into.allowed(false));
        assert_eq!(into.taken_there(), [] as [&Path; 0]);
        assert_eq!(
            into.plan_from_outside(OnTaken::Replace),
            [
                refused(&project, Refusal::IntoItself),
                copy_to(&plots, &trips.join("plots.txt")),
            ]
        );
        let alone = check(&[&project], &trips);
        assert!(!alone.allowed(false));
        assert!(!check(&[&trips], &trips).allowed(false), "onto itself");

        // An item that would replace the folder it is in: the Trash would
        // take it along. Kept beside it, it is a copy like any other.
        fs::create_dir(trips.join("trips")).unwrap();
        let nested = trips.join("trips");
        let inside = check(&[&nested], &project);
        assert_eq!(inside.taken_there(), [nested.as_path()]);
        assert_eq!(
            inside.plan_from_outside(OnTaken::Replace),
            [refused(&nested, Refusal::InsideReplaced)]
        );
        assert_eq!(
            inside.plan_from_outside(OnTaken::KeepBoth),
            [copy_to(&nested, &project.join("trips-copy"))]
        );
        assert_eq!(
            Refusal::InsideReplaced.said(),
            "is inside the item it would replace"
        );

        // Gone since the drag began: not asked about, and skipped.
        let gone = check(&[&soup, &plots], &trips);
        fs::remove_file(&soup).unwrap();
        assert_eq!(gone.taken_there(), [] as [&Path; 0]);
        assert_eq!(
            gone.plan_from_outside(OnTaken::Replace),
            [
                refused(&soup, Refusal::Missing),
                copy_to(&plots, &trips.join("plots.txt")),
            ]
        );
        // Over nothing, nothing is planned.
        let mut nowhere = DropCheck::new(vec![plots.clone()]);
        nowhere.aim(None);
        assert_eq!(nowhere.taken_there(), [] as [&Path; 0]);
        assert_eq!(nowhere.plan_from_outside(OnTaken::Replace), []);

        // A plan touches nothing.
        assert_eq!(
            names(&trips),
            ["lisbon.md", "soup-copy.md", "soup.md", "trips"]
        );
        assert_eq!(
            fs::read_to_string(trips.join("soup.md")).unwrap(),
            "a bowl in alfama"
        );
    }

    /// Replace: the copy takes the name, and what had it is in the Trash,
    /// where the record says. Never written over, and not trashed before
    /// the copy is whole.
    #[test]
    fn a_copy_that_replaces_keeps_where_the_trash_put_what_was_there() {
        let tree = TempTree::new(
            "fileops-replace",
            &[
                ("desk/soup.md", "pumpkin"),
                ("desk/recipes/bread.md", "rye"),
                ("desk/recipes/baking/scones.md", "butter"),
                ("project/soup.md", "leeks"),
                ("project/recipes/bread.md", "white"),
                ("project/recipes/old.md", "kept until now"),
                ("bin/", ""),
            ],
        );
        let dir = tree.0.as_path();
        let at = |path: &str| dir.join(path);
        let bin = at("bin");
        let trashed = std::cell::Cell::new(0);
        // A folder of the fixture stands in for the Trash.
        let trash = |path: &Path| -> io::Result<Option<PathBuf>> {
            trashed.set(trashed.get() + 1);
            let landed = bin.join(path.file_name().unwrap());
            fs::rename(path, &landed)?;
            Ok(Some(landed))
        };
        let env = Env {
            trash: &trash,
            stop: &|| false,
        };

        // A file: the new text under the name, the old one in the Trash.
        let (from, to) = (at("desk/soup.md"), at("project/soup.md"));
        let record = perform(
            Op::Replace {
                from: &from,
                to: &to,
            },
            &env,
        )
        .unwrap();
        assert_eq!(record.kind, OpKind::Replace);
        assert_eq!(record.source, Some(from.clone()));
        assert_eq!(record.result, Some(to.clone()));
        assert_eq!(record.trashed, Some(at("bin/soup.md")));
        assert_eq!(fs::read_to_string(&to).unwrap(), "pumpkin");
        assert_eq!(fs::read_to_string(at("bin/soup.md")).unwrap(), "leeks");
        assert_eq!(fs::read_to_string(&from).unwrap(), "pumpkin", "a copy");

        // A folder: the whole of the old one goes, nothing is merged.
        let (from, to) = (at("desk/recipes"), at("project/recipes"));
        let record = perform_free(
            Op::Replace {
                from: &from,
                to: &to,
            },
            &env,
        )
        .unwrap();
        assert_eq!(record.kind, OpKind::Replace);
        assert_eq!(record.trashed, Some(at("bin/recipes")));
        assert_eq!(names(&to), ["baking", "bread.md"]);
        assert_eq!(fs::read_to_string(to.join("bread.md")).unwrap(), "rye");
        assert_eq!(names(&at("bin/recipes")), ["bread.md", "old.md"]);
        assert_eq!(names(&at("project")), ["recipes", "soup.md"]);

        // Nothing there any more: a copy like any other, and nothing trashed.
        let before = trashed.get();
        let (from, to) = (at("desk/soup.md"), at("project/stew.md"));
        let record = perform(
            Op::Replace {
                from: &from,
                to: &to,
            },
            &env,
        )
        .unwrap();
        assert_eq!(record.kind, OpKind::Copy);
        assert_eq!(record.trashed, None);
        assert_eq!(trashed.get(), before);

        // Stopped while the copy is built: what was there has not moved.
        let stopped = Env {
            trash: &trash,
            stop: &|| true,
        };
        let (from, to) = (at("desk/recipes"), at("project/recipes"));
        let error = perform(
            Op::Replace {
                from: &from,
                to: &to,
            },
            &stopped,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(trashed.get(), before);
        assert_eq!(names(&to), ["baking", "bread.md"]);
        assert_eq!(names(&at("project")), ["recipes", "soup.md", "stew.md"]);

        // The Trash refuses: the copy is given up, and what was there stays.
        let refusing = Env {
            trash: &|_| Err(io::ErrorKind::PermissionDenied.into()),
            stop: &|| false,
        };
        let (from, to) = (at("desk/soup.md"), at("project/soup.md"));
        let error = perform(
            Op::Replace {
                from: &from,
                to: &to,
            },
            &refusing,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read_to_string(&to).unwrap(), "pumpkin");
        assert_eq!(names(&at("project")), ["recipes", "soup.md", "stew.md"]);

        // An item in place of itself, or of a folder it is in: refused
        // before anything is copied or trashed.
        for (from, to) in [
            (at("project/soup.md"), at("project/soup.md")),
            (at("project/recipes/baking"), at("project/recipes")),
        ] {
            let error = perform(
                Op::Replace {
                    from: &from,
                    to: &to,
                },
                &env,
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{from:?}");
        }
        assert_eq!(trashed.get(), before);
        assert!(at("project/recipes/baking/scones.md").exists());

        // The name is taken again between the Trash and the copy landing:
        // nothing is written over, and the failure says where the old
        // item is when it could not be put back.
        let raced = Env {
            trash: &|path| {
                let landed = bin.join("raced.md");
                fs::rename(path, &landed)?;
                fs::write(path, "someone else's")?;
                Ok(Some(landed))
            },
            stop: &|| false,
        };
        let error = perform(
            Op::Replace {
                from: &at("desk/soup.md"),
                to: &at("project/stew.md"),
            },
            &raced,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(error.to_string().contains("is in the Trash"), "{error}");
        assert_eq!(
            fs::read_to_string(at("project/stew.md")).unwrap(),
            "someone else's"
        );
        assert_eq!(fs::read_to_string(at("bin/raced.md")).unwrap(), "pumpkin");
        // No staging folder is left by any of it.
        assert_eq!(names(&at("project")), ["recipes", "soup.md", "stew.md"]);
    }

    /// A Trash for a test: the folder `bin` of the fixture, where an item
    /// takes the next free name, as the Trash renames what would clash.
    fn trash_into(bin: &Path) -> impl Fn(&Path) -> io::Result<Option<PathBuf>> + '_ {
        move |path: &Path| {
            let folder = fs::symlink_metadata(path)?.is_dir();
            let landed = bin.join(free_name(bin, path.file_name().unwrap(), folder));
            fs::rename(path, &landed)?;
            Ok(Some(landed))
        }
    }

    /// Takes `record` back through `trash`, as a step of one.
    fn take_back(record: Record, trash: ToTrash<'_>) -> Result<Undone, Skip> {
        let (mut undone, mut skipped) = undo_step(vec![record], &mut |one| one.apply(trash));
        match skipped.pop() {
            Some(skipped) => Err(skipped.why),
            None => Ok(undone.pop().unwrap()),
        }
    }

    /// Does `undone` again through `trash`, as a step of one.
    fn do_again(undone: Undone, trash: ToTrash<'_>) -> Result<Record, Skip> {
        let (mut records, mut skipped) = redo_step(vec![undone], &mut |one| one.apply(trash));
        match skipped.pop() {
            Some(skipped) => Err(skipped.why),
            None => Ok(records.pop().unwrap()),
        }
    }

    /// Every operation has its inverse, and the inverse its own: taken
    /// back and done again the files are as the operation left them, and
    /// nothing was deleted or written over on the way. What an undo sends
    /// to the Trash comes back out of it with whatever was put in it.
    #[test]
    fn every_operation_is_taken_back_and_done_again() {
        let tree = TempTree::new(
            "fileops-undo",
            &[
                ("project/soup.md", "leeks"),
                ("project/bread.md", "flour"),
                ("project/recipes/", ""),
                ("desk/soup.md", "pumpkin"),
                ("bin/", ""),
            ],
        );
        let dir = tree.0.as_path();
        let at = |path: &str| dir.join(path);
        let bin = at("bin");
        let trash = trash_into(&bin);
        let env = Env {
            trash: &trash,
            stop: &|| false,
        };
        let read = |path: &str| fs::read_to_string(at(path)).unwrap();

        // A new file: to the Trash, never deleted, and back with what was
        // written in it meanwhile.
        let made = perform(Op::NewFile(&at("project/plots.md")), &env).unwrap();
        assert_eq!(made.item, Some(ItemKind::File));
        fs::write(at("project/plots.md"), "beans").unwrap();
        let undone = take_back(made, &trash).unwrap();
        assert_eq!(undone.aside, Some(at("bin/plots.md")));
        assert!(!at("project/plots.md").exists());
        assert_eq!(read("bin/plots.md"), "beans");
        let made = do_again(undone, &trash).unwrap();
        assert_eq!(made.kind, OpKind::NewFile);
        assert_eq!(read("project/plots.md"), "beans");
        assert_eq!(names(&bin), [] as [&str; 0]);

        // A new folder, with what was put in it since.
        let made = perform(Op::NewFolder(&at("project/trips")), &env).unwrap();
        assert_eq!(made.item, Some(ItemKind::Folder));
        fs::write(at("project/trips/lisbon.md"), "tram 28").unwrap();
        let undone = take_back(made, &trash).unwrap();
        assert!(!at("project/trips").exists());
        assert_eq!(read("bin/trips/lisbon.md"), "tram 28");
        do_again(undone, &trash).unwrap();
        assert_eq!(read("project/trips/lisbon.md"), "tram 28");
        assert_eq!(names(&bin), [] as [&str; 0]);

        // A rename and a move: back to where the item was, and there again.
        for (kind, from, to) in [
            (OpKind::Rename, "project/soup.md", "project/stew.md"),
            (OpKind::Move, "project/stew.md", "project/recipes/stew.md"),
        ] {
            let (source, result) = (at(from), at(to));
            let op = match kind {
                OpKind::Rename => Op::Rename {
                    from: &source,
                    to: &result,
                },
                _ => Op::Move {
                    from: &source,
                    to: &result,
                },
            };
            let moved = perform(op, &env).unwrap();
            let undone = take_back(moved, &trash).unwrap();
            assert_eq!(undone.aside, None);
            assert_eq!(read(from), "leeks");
            assert!(!result.exists(), "{to}");
            let moved = do_again(undone, &trash).unwrap();
            assert_eq!(moved.kind, kind);
            assert_eq!(read(to), "leeks");
            assert!(!source.exists(), "{from}");
        }

        // A copy and a duplicate: the copy goes to the Trash, the original
        // is never touched, and the same copy comes back.
        for (kind, from, to) in [
            (OpKind::Copy, "desk/soup.md", "project/soup.md"),
            (
                OpKind::Duplicate,
                "project/bread.md",
                "project/bread-copy.md",
            ),
        ] {
            let (source, result) = (at(from), at(to));
            let op = match kind {
                OpKind::Copy => Op::Copy {
                    from: &source,
                    to: &result,
                },
                _ => Op::Duplicate {
                    from: &source,
                    to: &result,
                },
            };
            let original = read(from);
            let copied = perform(op, &env).unwrap();
            fs::write(&result, "edited since").unwrap();
            let undone = take_back(copied, &trash).unwrap();
            assert!(!result.exists(), "{to}");
            assert_eq!(read(from), original);
            let copied = do_again(undone, &trash).unwrap();
            assert_eq!(copied.kind, kind);
            assert_eq!(read(to), "edited since");
            assert_eq!(read(from), original);
            assert_eq!(names(&bin), [] as [&str; 0]);
        }

        // Moved to the Trash: put back from where the Trash put it, and
        // moved there again, wherever the Trash puts it this time.
        let trashed = perform(Op::Trash(&at("project/bread.md")), &env).unwrap();
        assert_eq!(trashed.item, Some(ItemKind::File));
        assert_eq!(trashed.trashed, Some(at("bin/bread.md")));
        let undone = take_back(trashed, &trash).unwrap();
        assert_eq!(read("project/bread.md"), "flour");
        assert_eq!(names(&bin), [] as [&str; 0]);
        fs::write(at("bin/bread.md"), "someone else's").unwrap();
        let trashed = do_again(undone, &trash).unwrap();
        assert_eq!(trashed.trashed, Some(at("bin/bread-copy.md")));
        assert!(!at("project/bread.md").exists());
        assert_eq!(read("bin/bread-copy.md"), "flour");
        take_back(trashed, &trash).unwrap();
        assert_eq!(read("project/bread.md"), "flour");
        fs::remove_file(at("bin/bread.md")).unwrap();

        // A copy that replaced an item: the copy goes to the Trash and
        // the item comes back from it, then the other way round again.
        fs::write(at("project/soup.md"), "leeks").unwrap();
        let (from, to) = (at("desk/soup.md"), at("project/soup.md"));
        let replaced = perform(
            Op::Replace {
                from: &from,
                to: &to,
            },
            &env,
        )
        .unwrap();
        assert_eq!(replaced.kind, OpKind::Replace);
        assert_eq!(replaced.replaced, Some(ItemKind::File));
        assert_eq!(read("project/soup.md"), "pumpkin");
        assert_eq!(read("bin/soup.md"), "leeks");
        let undone = take_back(replaced, &trash).unwrap();
        assert_eq!(read("project/soup.md"), "leeks");
        let aside = undone.aside.clone().unwrap();
        assert_eq!(fs::read_to_string(&aside).unwrap(), "pumpkin");
        assert_eq!(names(&bin).len(), 1);
        let replaced = do_again(undone, &trash).unwrap();
        assert_eq!(read("project/soup.md"), "pumpkin");
        let old = replaced.trashed.clone().unwrap();
        assert_eq!(fs::read_to_string(&old).unwrap(), "leeks");
        assert_eq!(names(&bin).len(), 1);
        // And taken back once more, from where the Trash put it this time.
        take_back(replaced, &trash).unwrap();
        assert_eq!(read("project/soup.md"), "leeks");
        assert_eq!(read("desk/soup.md"), "pumpkin", "the original of the copy");
    }

    /// An undo looks before it acts: what the operation left must still
    /// be there and be the same kind of thing, and the place an item goes
    /// back to must be free. Otherwise nothing is touched, and the reason
    /// says which it was.
    #[test]
    fn what_no_longer_matches_is_left_alone_and_nothing_is_written_over() {
        let tree = TempTree::new(
            "fileops-undo-refused",
            &[
                ("soup.md", "leeks"),
                ("bread.md", "flour"),
                ("plots.md", "beans"),
                ("old.md", "last year"),
                ("recipes/", ""),
                ("bin/", ""),
            ],
        );
        let dir = tree.0.as_path();
        let at = |path: &str| dir.join(path);
        let bin = at("bin");
        let trash = trash_into(&bin);
        let env = Env {
            trash: &trash,
            stop: &|| false,
        };
        let read = |path: &str| fs::read_to_string(at(path)).unwrap();
        let rename = |from: &str, to: &str| {
            perform(
                Op::Rename {
                    from: &at(from),
                    to: &at(to),
                },
                &env,
            )
            .unwrap()
        };

        // The place is taken: neither file is touched.
        let renamed = rename("soup.md", "stew.md");
        fs::write(at("soup.md"), "another soup").unwrap();
        let why = renamed.undo().unwrap_err();
        assert!(
            matches!(&why, Skip::Taken(by) if *by == at("soup.md")),
            "{why:?}"
        );
        assert_eq!(
            why.said("stew.md"),
            "stew.md was left alone: an item named soup.md is in the way"
        );
        assert!(matches!(take_back(renamed, &trash), Err(Skip::Taken(_))));
        assert_eq!(read("soup.md"), "another soup");
        assert_eq!(read("stew.md"), "leeks");
        // By a dangling link as well.
        let renamed = rename("bread.md", "rye.md");
        symlink("missing-target", at("bread.md")).unwrap();
        assert!(matches!(renamed.undo(), Err(Skip::Taken(_))));
        fs::remove_file(at("bread.md")).unwrap();
        // Taken between the look and the move: the move itself refuses.
        let back = renamed.undo().unwrap();
        fs::write(at("bread.md"), "late").unwrap();
        assert!(matches!(back.apply(&trash), Err(Skip::Taken(_))));
        assert_eq!(read("bread.md"), "late");
        assert_eq!(read("rye.md"), "flour");
        fs::remove_file(at("bread.md")).unwrap();

        // The item is gone.
        fs::remove_file(at("rye.md")).unwrap();
        let why = renamed.undo().unwrap_err();
        assert!(matches!(why, Skip::Gone), "{why:?}");
        assert_eq!(why.said("rye.md"), "rye.md is no longer there");
        // Or has become another kind of thing: a folder where the file was.
        fs::create_dir(at("rye.md")).unwrap();
        let why = renamed.undo().unwrap_err();
        assert!(matches!(why, Skip::Changed), "{why:?}");
        assert_eq!(
            why.said("rye.md"),
            "rye.md is no longer the same kind of item"
        );
        assert!(matches!(take_back(renamed, &trash), Err(Skip::Changed)));
        assert!(at("rye.md").is_dir());
        // A link where a file was made is not that file either.
        let made = perform(Op::NewFile(&at("notes.md")), &env).unwrap();
        fs::remove_file(at("notes.md")).unwrap();
        symlink("plots.md", at("notes.md")).unwrap();
        assert!(matches!(made.undo(), Err(Skip::Changed)));
        fs::remove_file(at("notes.md")).unwrap();
        assert!(matches!(take_back(made, &trash), Err(Skip::Gone)));
        assert_eq!(names(&bin), [] as [&str; 0], "nothing went to the Trash");

        // Out of the Trash: gone from it, another kind of thing there, the
        // place taken, and a Trash that never said where the item went.
        let trashed = perform(Op::Trash(&at("plots.md")), &env).unwrap();
        fs::write(at("plots.md"), "peas").unwrap();
        assert!(matches!(trashed.undo(), Err(Skip::Taken(_))));
        fs::remove_file(at("plots.md")).unwrap();
        fs::rename(at("bin/plots.md"), at("bin/elsewhere.md")).unwrap();
        let why = trashed.undo().unwrap_err();
        assert!(matches!(why, Skip::GoneFromTrash), "{why:?}");
        assert_eq!(why.said("plots.md"), "plots.md is no longer in the Trash");
        fs::create_dir(at("bin/plots.md")).unwrap();
        assert!(matches!(trashed.undo(), Err(Skip::Changed)));
        assert!(!at("plots.md").exists());
        let silent = Env {
            trash: &|path| fs::remove_file(path).map(|()| None),
            stop: &|| false,
        };
        let trashed = perform(Op::Trash(&at("old.md")), &silent).unwrap();
        let why = trashed.undo().unwrap_err();
        assert!(matches!(why, Skip::Unplaced), "{why:?}");
        assert_eq!(
            why.said("old.md"),
            "old.md is in the Trash, and the Trash did not say where"
        );

        // Doing again is checked the same way: the place the item would
        // go to has been taken since it was taken back.
        let renamed = rename("stew.md", "broth.md");
        let undone = take_back(renamed, &trash).unwrap();
        fs::write(at("broth.md"), "bones").unwrap();
        assert!(matches!(undone.redo(), Err(Skip::Taken(_))));
        fs::remove_file(at("broth.md")).unwrap();
        fs::remove_file(at("stew.md")).unwrap();
        assert!(matches!(do_again(undone, &trash), Err(Skip::Gone)));
        // A copy taken back and then emptied out of the Trash cannot come
        // back, and is not copied a second time behind anyone's back.
        let copied = perform(
            Op::Copy {
                from: &at("soup.md"),
                to: &at("recipes/soup.md"),
            },
            &env,
        )
        .unwrap();
        let undone = take_back(copied, &trash).unwrap();
        fs::remove_file(undone.aside.as_deref().unwrap()).unwrap();
        assert!(matches!(undone.redo(), Err(Skip::GoneFromTrash)));
        assert!(!at("recipes/soup.md").exists());
    }

    /// The Trash can refuse to give an item back. Then it is still in the
    /// Trash, and the reason says so and where it belongs: nothing is
    /// called done that was not. A copy that replaced an item stays where
    /// it is when the item cannot come back to take its place.
    #[test]
    fn an_item_the_trash_does_not_give_back_is_said_to_be_still_there() {
        let tree = TempTree::new(
            "fileops-undo-stuck",
            &[
                ("recipes/soup.md", "pumpkin"),
                ("locked/soup.md", "leeks"),
                ("locked/old.md", "last year"),
                ("bin/", ""),
            ],
        );
        let dir = tree.0.as_path();
        let at = |path: &str| dir.join(path);
        let bin = at("bin");
        let trash = trash_into(&bin);
        // As the operations left them: old.md was moved to a Trash that
        // lets nothing out, and soup.md replaced an item now in it.
        let record = |kind, source: &str, result: Option<&str>, trashed: &str| Record {
            kind,
            source: Some(at(source)),
            result: result.map(at),
            trashed: Some(at(trashed)),
            original_left: None,
            item: Some(ItemKind::File),
            replaced: Some(ItemKind::File),
        };
        let trashed = record(OpKind::Trash, "recipes/old.md", None, "locked/old.md");
        let replaced = record(
            OpKind::Replace,
            "desk/soup.md",
            Some("recipes/soup.md"),
            "locked/soup.md",
        );
        chmod(&at("locked"), 0o555);
        let put_back = take_back(trashed, &trash);
        let exchanged = take_back(replaced, &trash);
        // Writable again before anything is asserted, so the folder can be
        // removed whatever happens.
        chmod(&at("locked"), 0o755);

        let why = put_back.unwrap_err();
        assert!(
            matches!(&why, Skip::StillInTrash { belongs, .. } if *belongs == at("recipes/old.md")),
            "{why:?}"
        );
        let said = why.said("old.md");
        assert!(
            said.starts_with(&format!(
                "old.md is still in the Trash and belongs in {}: ",
                at("recipes").display()
            )),
            "{said}"
        );
        assert_eq!(
            fs::read_to_string(at("locked/old.md")).unwrap(),
            "last year"
        );
        assert!(!at("recipes/old.md").exists());

        let why = exchanged.unwrap_err();
        assert!(
            matches!(&why, Skip::StillInTrash { belongs, .. } if *belongs == at("recipes/soup.md")),
            "{why:?}"
        );
        // The copy is back where it was, and nothing of it is left in the
        // Trash it went to for a moment.
        assert_eq!(
            fs::read_to_string(at("recipes/soup.md")).unwrap(),
            "pumpkin"
        );
        assert_eq!(fs::read_to_string(at("locked/soup.md")).unwrap(), "leeks");
        assert_eq!(names(&bin), [] as [&str; 0]);

        // A Trash that refuses to take an item: it stays where it is.
        let made = perform(Op::NewFile(&at("recipes/notes.md")), &plain()).unwrap();
        let why = take_back(made, &|_| Err(io::ErrorKind::PermissionDenied.into())).unwrap_err();
        assert!(matches!(why, Skip::Failed(_)), "{why:?}");
        assert!(at("recipes/notes.md").exists());
    }

    /// The operations of one step are taken back last first, so that what
    /// a later one did to the result of an earlier one is undone before
    /// it. One that cannot be is skipped with its reason, and the others
    /// still go. Done again they go first to last.
    #[test]
    fn a_step_is_taken_back_last_first_and_skips_what_it_cannot() {
        let tree = TempTree::new(
            "fileops-undo-step",
            &[
                ("bread.md", "flour"),
                ("soup.md", "leeks"),
                ("plots.md", "beans"),
                ("bin/", ""),
            ],
        );
        let dir = tree.0.as_path();
        let at = |path: &str| dir.join(path);
        let bin = at("bin");
        let trash = trash_into(&bin);
        let env = Env {
            trash: &trash,
            stop: &|| false,
        };
        // A folder made, and three files moved into it: the folder can
        // only go once they are out of it again.
        let mut records = vec![perform(Op::NewFolder(&at("recipes")), &env).unwrap()];
        for name in ["bread.md", "soup.md", "plots.md"] {
            let moved = perform(
                Op::Move {
                    from: &at(name),
                    to: &at("recipes").join(name),
                },
                &env,
            )
            .unwrap();
            records.push(moved);
        }
        // Something else has taken soup.md's place meanwhile.
        fs::write(at("soup.md"), "another soup").unwrap();

        let mut order = Vec::new();
        let (undone, skipped) = undo_step(records, &mut |reversal| {
            order.push(reversal.clone());
            reversal.apply(&trash)
        });
        assert_eq!(
            order,
            [
                Reversal::Move {
                    from: at("recipes/plots.md"),
                    to: at("plots.md"),
                },
                Reversal::Move {
                    from: at("recipes/bread.md"),
                    to: at("bread.md"),
                },
                Reversal::Trash(at("recipes")),
            ]
        );
        let kinds: Vec<OpKind> = undone.iter().map(|undone| undone.record.kind).collect();
        assert_eq!(kinds, [OpKind::Move, OpKind::Move, OpKind::NewFolder]);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].item, at("recipes/soup.md"));
        assert!(matches!(skipped[0].why, Skip::Taken(_)));
        assert_eq!(names(dir), ["bin", "bread.md", "plots.md", "soup.md"]);
        assert_eq!(fs::read_to_string(at("soup.md")).unwrap(), "another soup");
        // The file that could not go back went to the Trash inside its
        // folder, and was not deleted.
        assert_eq!(
            fs::read_to_string(at("bin/recipes/soup.md")).unwrap(),
            "leeks"
        );

        // Done again in the order they were done: the folder first, then
        // the files into it.
        let mut order = Vec::new();
        let (records, skipped) = redo_step(undone, &mut |reversal| {
            order.push(reversal.lands().map(Path::to_path_buf));
            reversal.apply(&trash)
        });
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(
            order,
            [
                Some(at("recipes")),
                Some(at("recipes/bread.md")),
                Some(at("recipes/plots.md")),
            ]
        );
        let kinds: Vec<OpKind> = records.iter().map(|record| record.kind).collect();
        assert_eq!(kinds, [OpKind::NewFolder, OpKind::Move, OpKind::Move]);
        assert_eq!(names(&at("recipes")), ["bread.md", "plots.md", "soup.md"]);
        assert_eq!(names(dir), ["bin", "recipes", "soup.md"]);
    }

    /// A rename that only changes the case of a name is taken back too:
    /// on a volume that does not tell the two spellings apart, the old
    /// name is not taken by anything but the item itself.
    #[test]
    fn a_change_of_case_is_taken_back() {
        let tree = TempTree::new("fileops-undo-case", &[("bread.md", "flour")]);
        let dir = tree.0.as_path();
        let (from, to) = (dir.join("bread.md"), dir.join("Bread.md"));
        let env = plain();
        let renamed = perform(
            Op::Rename {
                from: &from,
                to: &to,
            },
            &env,
        )
        .unwrap();
        assert_eq!(names(dir), ["Bread.md"]);
        let undone = take_back(renamed, env.trash).unwrap();
        assert_eq!(names(dir), ["bread.md"]);
        do_again(undone, env.trash).unwrap();
        assert_eq!(names(dir), ["Bread.md"]);
    }

    /// The steps kept: one for each thing done, whatever it came to, the
    /// oldest making room. Taking one back and doing it again are not
    /// steps themselves, and anything new that is done ends what could be
    /// done again.
    #[test]
    fn steps_are_kept_taken_back_and_done_again_until_something_new_is_done() {
        let record = |name: &str| Record {
            kind: OpKind::NewFile,
            source: None,
            result: Some(PathBuf::from(name)),
            trashed: None,
            original_left: None,
            item: Some(ItemKind::File),
            replaced: None,
        };
        let shown = |records: &[Record]| -> Vec<String> {
            records
                .iter()
                .map(|record| record.shown().display().to_string())
                .collect()
        };
        let mut steps = Steps::default();
        assert!(steps.is_empty());
        assert!(steps.last().is_none() && steps.last_undone().is_none());

        // Three operations of one step, kept as they end, with a step of
        // one operation done in between: still two steps.
        let (paste, rename) = (steps.begin(), steps.begin());
        steps.keep(paste, record("a"));
        steps.keep(paste, record("b"));
        steps.keep(rename, record("r"));
        steps.keep(paste, record("c"));
        assert_eq!(steps.len(), 2);
        assert_eq!(shown(steps.last().unwrap()), ["r"]);
        let all: Vec<&Record> = steps.records().collect();
        assert_eq!(all.len(), 4);

        // Taken back: off what was done, onto what can be done again.
        let taken = steps.take_last().unwrap();
        assert_eq!(shown(&taken), ["r"]);
        let undone = |records: Vec<Record>| -> Vec<Undone> {
            records
                .into_iter()
                .map(|record| Undone {
                    record,
                    aside: None,
                })
                .collect()
        };
        steps.took_back(undone(taken));
        assert_eq!(steps.len(), 1);
        assert_eq!(shown(steps.last().unwrap()), ["a", "b", "c"]);
        assert_eq!(steps.last_undone().map(<[Undone]>::len), Some(1));
        let taken = steps.take_last().unwrap();
        steps.took_back(undone(taken));
        assert!(steps.is_empty());

        // Done again, the last one taken back first, and the other one
        // can still be done again after it.
        let again = steps.take_undone().unwrap();
        assert_eq!(again.len(), 3);
        steps.did_again(again.into_iter().map(|undone| undone.record).collect());
        assert_eq!(steps.len(), 1);
        assert_eq!(steps.last_undone().map(<[Undone]>::len), Some(1));

        // Something new is done: nothing can be done again any more.
        let step = steps.begin();
        steps.keep(step, record("n"));
        assert!(steps.last_undone().is_none());
        assert!(steps.take_undone().is_none());
        assert_eq!(steps.len(), 2);

        // A step of which nothing was taken back is not kept to be done
        // again, and it is gone from what can be taken back.
        let _ = steps.take_last().unwrap();
        steps.took_back(Vec::new());
        assert!(steps.last_undone().is_none());
        assert_eq!(steps.len(), 1);

        // No more than a hundred steps: the oldest go.
        for n in 0..STEPS_KEPT + 5 {
            let step = steps.begin();
            steps.keep(step, record(&format!("f{n}")));
        }
        assert_eq!(steps.len(), STEPS_KEPT);
        assert_eq!(
            steps
                .records()
                .next()
                .map(|record| record.shown().to_path_buf()),
            Some(PathBuf::from("f5"))
        );

        steps.clear();
        assert!(steps.is_empty() && steps.last_undone().is_none());
    }
}
