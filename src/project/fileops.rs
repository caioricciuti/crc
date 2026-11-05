//! Copying and moving project items on disk, for the Explorer.
//!
//! Nothing here replaces an entry that already exists. A copy is built in a
//! hidden folder beside its destination and moved into place with the
//! exclusive rename; a move is that rename, or a copy and a removal when the
//! destination is on another volume. A symbolic link is the item itself
//! throughout: it is copied, moved and removed as the link, and never
//! followed. Every operation answers where the item ended up, so that it can
//! be reversed.

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
    copy_tree(source, &staged, Path::new(""), &mut folders)?;
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
/// set when the copy is in place.
fn copy_tree(
    from: &Path,
    to: &Path,
    relative: &Path,
    folders: &mut Vec<(PathBuf, fs::Permissions)>,
) -> io::Result<()> {
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
        copy_tree(&dir.join("a.txt"), &staged, Path::new(""), &mut Vec::new()).unwrap();
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
}
