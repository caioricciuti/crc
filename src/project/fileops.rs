//! Copying and moving project items on disk, for the Explorer.
//!
//! Nothing here replaces an entry that already exists. A copy is built in a
//! hidden folder beside its destination and moved into place with the
//! exclusive rename; a move is that rename, or a copy and a removal when the
//! destination is on another volume. A symbolic link is the item itself
//! throughout: it is copied, moved and removed as the link, and never
//! followed. Every operation answers where the item ended up, so that it can
//! be reversed.
//!
//! Every change the Explorer makes to the files goes through [`perform`]:
//! a new file or folder, a rename, a move, a copy, a duplicate, a move to
//! the Trash. It answers a [`Record`] of what was done. What a paste or a
//! duplicate will do to each item is worked out first, by [`plan_paste`]
//! and [`plan_duplicate`], without touching anything.

use std::collections::HashSet;
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
    let (dir, name) = folder_and_name(destination)?;
    let kind = fs::symlink_metadata(source)?.file_type();
    if !(kind.is_file() || kind.is_dir() || kind.is_symlink()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only files, folders and links can be copied",
        ));
    }
    refuse_nesting(source, dir)?;
    // Before the work of copying. The exclusive move below is what keeps
    // an entry made in the meantime.
    if taken(destination) {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    let staging = Staging::new(dir)?;
    let staged = staging.0.join(name);
    let mut folders = Vec::new();
    copy_tree(source, &staged, Path::new(""), &mut folders, stop)?;
    move_without_replace(&staged, destination)?;
    // Folders were made writable to be filled, and to be moved out of the
    // staging folder. Deepest first, as one that loses its search bit hides
    // the folders in it.
    for (relative, permissions) in folders.into_iter().rev() {
        if let Err(error) = fs::set_permissions(destination.join(relative), permissions) {
            let _ = remove_item(destination);
            return Err(error);
        }
    }
    Ok(destination.to_path_buf())
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
    /// Where the item is in the Trash, when the system says.
    pub trashed: Option<PathBuf>,
    /// A move to another volume whose original could not be removed after
    /// it was copied: it is still at `source`, and this is why.
    pub original_left: Option<io::Error>,
}

/// What an operation is given besides its paths.
pub struct Env<'a> {
    /// Moves an item to the Trash, and answers where it is there when the
    /// system says.
    pub trash: &'a dyn Fn(&Path) -> io::Result<Option<PathBuf>>,
    /// Asked before each file and folder of a copy: `true` stops the copy,
    /// which then fails with `Interrupted` and leaves nothing behind.
    pub stop: &'a dyn Fn() -> bool,
}

/// Does `op`, and answers what was done. The one way a file or folder of
/// the project is made, renamed, moved, copied or trashed, so that every
/// one of them leaves a record.
///
/// Nothing is ever replaced: an operation whose destination is taken, a
/// dangling link included, fails with `AlreadyExists` and changes nothing.
pub fn perform(op: Op<'_>, env: &Env<'_>) -> io::Result<Record> {
    let record = |kind: OpKind, source: Option<&Path>, result: Option<PathBuf>| Record {
        kind,
        source: source.map(Path::to_path_buf),
        result,
        trashed: None,
        original_left: None,
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
        Op::Trash(path) => {
            let trashed = (env.trash)(path)?;
            Ok(Record {
                trashed,
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
}

impl Refusal {
    /// The refusal as the rest of a sentence that starts with the item's
    /// name.
    pub fn said(self) -> &'static str {
        match self {
            Refusal::Missing => "is no longer there",
            Refusal::IntoItself => "cannot be pasted into itself",
            Refusal::Taken => "would replace an item of the same name",
        }
    }
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
}
