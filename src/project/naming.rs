//! What is wrong with a name typed into the Explorer's field, found while
//! it is typed rather than after Return.
//!
//! A verdict costs at most one look at the disk for the name, and one more
//! for a rename that lands on something: nothing is read for a name that
//! is wrong on its face, or for a rename that changes nothing.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// The longest one name can be on the volumes the Explorer works on, in
/// bytes.
const NAME_MAX: usize = 255;

/// Why a name will not do, or what is odd about one that will.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    /// Nothing typed, or only spaces.
    Empty,
    /// A path with nothing between two slashes, or after the last.
    EmptyPart,
    /// A path that starts at the root of the disk.
    LeadingSlash,
    /// A slash where one name is asked for.
    Slash,
    /// A line break, a tab or another control character.
    Control,
    /// `.` or `..`.
    Reserved,
    TooLong,
    /// Something else has the name.
    Taken,
    /// A file stands where the path needs a folder.
    Blocked,
    /// Spaces before or after a name: allowed, and worth a second look.
    Spaces,
}

impl Problem {
    /// The problem in a line, as the field says it.
    pub fn said(self) -> &'static str {
        match self {
            Problem::Empty => "A name is required",
            Problem::EmptyPart => "Every part of the path needs a name",
            Problem::LeadingSlash => "The path starts in this folder, not at /",
            Problem::Slash => "A name cannot contain /",
            Problem::Control => "A name cannot contain control characters",
            Problem::Reserved => ". and .. are not names",
            Problem::TooLong => "A name can be 255 bytes at most",
            Problem::Taken => "This name is already taken",
            Problem::Blocked => "A file is in the way of this path",
            Problem::Spaces => "Starts or ends with a space",
        }
    }

    /// Whether Return is refused. A warning lets the name through.
    pub fn blocks(self) -> bool {
        self != Problem::Spaces
    }
}

/// What the name is for.
#[derive(Clone, Copy, Debug)]
pub struct Ask<'a> {
    /// The folder the name is in.
    pub parent: &'a Path,
    /// A new file or folder takes a path, `plots/2027/beds.md`, and the
    /// folders on the way are made. A rename takes one name.
    pub nested: bool,
    /// The item being renamed: it is not in its own way, under the name it
    /// has or under one the volume reads as the same (`Notes.md` for
    /// `notes.md`).
    pub renaming: Option<&'a Path>,
}

/// What is at a path. An item is told from another by its volume and its
/// number there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Found {
    Free,
    Taken((u64, u64)),
    /// Something on the way to the path is not a folder.
    Blocked,
}

/// One look at `path` itself, a dangling link included.
fn look(path: &Path) -> Found {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => Found::Taken((meta.dev(), meta.ino())),
        Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => Found::Blocked,
        Err(_) => Found::Free,
    }
}

/// What is wrong with `text` as the name `ask` describes, the worst thing
/// first; `None` for a name that will do.
pub fn check(text: &str, ask: &Ask<'_>) -> Option<Problem> {
    check_with(text, ask, look)
}

/// What is wrong with `text` before the disk is asked anything.
fn shape(text: &str, nested: bool) -> Option<Problem> {
    if text.trim().is_empty() {
        return Some(Problem::Empty);
    }
    if text.chars().any(char::is_control) {
        return Some(Problem::Control);
    }
    // A colon is a name's to have: the disk takes it, and files with one
    // (a time in a name) are renamed like any other.
    if !nested && text.contains('/') {
        return Some(Problem::Slash);
    }
    if text.starts_with('/') {
        return Some(Problem::LeadingSlash);
    }
    let mut spaces = false;
    for part in text.split('/') {
        if part.trim().is_empty() {
            return Some(Problem::EmptyPart);
        }
        if part == "." || part == ".." {
            return Some(Problem::Reserved);
        }
        if part.len() > NAME_MAX {
            return Some(Problem::TooLong);
        }
        spaces |= part != part.trim();
    }
    spaces.then_some(Problem::Spaces)
}

/// [`check`], with the look at the disk given so a test can count the
/// looks and stand in for a volume that ignores case.
fn check_with(text: &str, ask: &Ask<'_>, mut look: impl FnMut(&Path) -> Found) -> Option<Problem> {
    let odd = shape(text, ask.nested);
    if odd.is_some_and(Problem::blocks) {
        return odd;
    }
    // The name it has already: nothing would change, so nothing is asked.
    if ask
        .renaming
        .and_then(Path::file_name)
        .is_some_and(|current| current == text)
    {
        return odd;
    }
    let target = ask.parent.join(text);
    match look(&target) {
        Found::Free => odd,
        Found::Blocked => Some(Problem::Blocked),
        Found::Taken(there) => {
            // Only the item itself may be under the name: a change of
            // case on a volume that reads both spellings as one.
            let itself = ask
                .renaming
                .is_some_and(|current| look(current) == Found::Taken(there));
            if itself { odd } else { Some(Problem::Taken) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::TempTree;
    use std::path::PathBuf;

    /// A new file or folder in `/p`.
    fn new_item() -> Ask<'static> {
        Ask {
            parent: Path::new("/p"),
            nested: true,
            renaming: None,
        }
    }

    fn renaming(path: &'static str) -> Ask<'static> {
        Ask {
            parent: Path::new("/p"),
            nested: false,
            renaming: Some(Path::new(path)),
        }
    }

    /// A disk with these items on it, by path and number, and a count of
    /// the looks taken.
    struct Disk {
        items: Vec<(PathBuf, u64)>,
        looks: usize,
    }

    impl Disk {
        fn new(items: &[(&str, u64)]) -> Disk {
            Disk {
                items: items
                    .iter()
                    .map(|(path, id)| (PathBuf::from(path), *id))
                    .collect(),
                looks: 0,
            }
        }

        fn check(&mut self, text: &str, ask: &Ask<'_>) -> Option<Problem> {
            check_with(text, ask, |path| {
                self.looks += 1;
                self.items
                    .iter()
                    .find(|(at, _)| at == path)
                    .map_or(Found::Free, |(_, id)| Found::Taken((1, *id)))
            })
        }
    }

    #[test]
    fn an_empty_name_and_one_of_spaces_are_refused() {
        let mut disk = Disk::new(&[]);
        assert_eq!(disk.check("", &new_item()), Some(Problem::Empty));
        assert_eq!(disk.check("   ", &new_item()), Some(Problem::Empty));
        assert_eq!(
            disk.check("", &renaming("/p/bread.md")),
            Some(Problem::Empty)
        );
        assert_eq!(disk.looks, 0, "nothing on disk is asked about no name");
        assert!(Problem::Empty.blocks());
    }

    #[test]
    fn a_name_something_else_has_is_taken() {
        let mut disk = Disk::new(&[("/p/soup.md", 2), ("/p/bread.md", 3)]);
        assert_eq!(disk.check("soup.md", &new_item()), Some(Problem::Taken));
        assert_eq!(
            disk.check("soup.md", &renaming("/p/bread.md")),
            Some(Problem::Taken)
        );
        assert_eq!(disk.check("stew.md", &new_item()), None);
        assert_eq!(disk.check("stew.md", &renaming("/p/bread.md")), None);
    }

    #[test]
    fn an_item_is_not_in_its_own_way() {
        // The name it has: nothing is asked at all.
        let mut disk = Disk::new(&[("/p/bread.md", 3)]);
        assert_eq!(disk.check("bread.md", &renaming("/p/bread.md")), None);
        assert_eq!(disk.looks, 0);
        // A change of case alone, on a volume that reads both spellings
        // as the one item.
        let mut disk = Disk::new(&[("/p/bread.md", 3), ("/p/Bread.md", 3)]);
        assert_eq!(disk.check("Bread.md", &renaming("/p/bread.md")), None);
        // On a volume that tells them apart, another item has the name.
        let mut disk = Disk::new(&[("/p/bread.md", 3), ("/p/Bread.md", 4)]);
        assert_eq!(
            disk.check("Bread.md", &renaming("/p/bread.md")),
            Some(Problem::Taken)
        );
        // And nothing has it: free.
        let mut disk = Disk::new(&[("/p/bread.md", 3)]);
        assert_eq!(disk.check("Bread.md", &renaming("/p/bread.md")), None);
    }

    #[test]
    fn dot_and_dot_dot_are_not_names() {
        let mut disk = Disk::new(&[]);
        for ask in [new_item(), renaming("/p/bread.md")] {
            assert_eq!(disk.check(".", &ask), Some(Problem::Reserved));
            assert_eq!(disk.check("..", &ask), Some(Problem::Reserved));
        }
        assert_eq!(
            disk.check("plots/../beds.md", &new_item()),
            Some(Problem::Reserved)
        );
        assert_eq!(
            disk.check("./beds.md", &new_item()),
            Some(Problem::Reserved)
        );
        // A name that only starts with dots is one.
        assert_eq!(disk.check(".env", &new_item()), None);
        assert_eq!(disk.check("...", &new_item()), None);
    }

    #[test]
    fn a_slash_makes_a_path_for_a_new_item_and_is_refused_in_a_rename() {
        let mut disk = Disk::new(&[("/p/plots", 5), ("/p/plots/beds.md", 6)]);
        assert_eq!(disk.check("plots/2027/herbs.md", &new_item()), None);
        assert_eq!(
            disk.check("plots/beds.md", &new_item()),
            Some(Problem::Taken)
        );
        assert_eq!(
            disk.check("plots/herbs.md", &renaming("/p/bread.md")),
            Some(Problem::Slash)
        );
        // Every part of a path is a name.
        assert_eq!(
            disk.check("/herbs.md", &new_item()),
            Some(Problem::LeadingSlash)
        );
        assert_eq!(
            disk.check("plots//herbs.md", &new_item()),
            Some(Problem::EmptyPart)
        );
        assert_eq!(disk.check("plots/", &new_item()), Some(Problem::EmptyPart));
        assert_eq!(
            disk.check("plots/ /herbs.md", &new_item()),
            Some(Problem::EmptyPart)
        );
    }

    #[test]
    fn a_colon_is_allowed_and_a_control_character_or_a_name_too_long_refused() {
        let mut disk = Disk::new(&[]);
        // A colon is in many a name already, a time above all: no error
        // and no warning, for a new item or a rename.
        assert_eq!(disk.check("notes:today.md", &new_item()), None);
        assert_eq!(disk.check("10:30 notes.md", &new_item()), None);
        assert_eq!(disk.check("a:b", &renaming("/p/bread.md")), None);
        assert_eq!(disk.looks, 3);
        assert_eq!(
            disk.check("two\nlines.md", &new_item()),
            Some(Problem::Control)
        );
        assert_eq!(
            disk.check("tab\there.md", &new_item()),
            Some(Problem::Control)
        );
        let long = "n".repeat(256);
        assert_eq!(disk.check(&long, &new_item()), Some(Problem::TooLong));
        assert_eq!(disk.check(&long[..255], &new_item()), None);
        // Each part of a path has the whole length to itself.
        let path = format!("{}/{}", &long[..200], &long[..200]);
        assert_eq!(disk.check(&path, &new_item()), None);
        assert_eq!(
            disk.looks, 5,
            "only the names that would do were looked for"
        );
    }

    #[test]
    fn spaces_at_either_end_are_a_warning_and_the_name_is_allowed() {
        let mut disk = Disk::new(&[("/p/soup.md", 2), ("/p/ soup.md", 7)]);
        for name in [" stew.md", "stew.md ", "plots /herbs.md", "plots/ herbs.md"] {
            assert_eq!(
                disk.check(name, &new_item()),
                Some(Problem::Spaces),
                "{name:?}"
            );
        }
        assert!(!Problem::Spaces.blocks());
        // Spaces inside a name are nothing.
        assert_eq!(disk.check("rye bread.md", &new_item()), None);
        // What blocks is said before the warning.
        assert_eq!(disk.check(" soup.md", &new_item()), Some(Problem::Taken));
        assert_eq!(
            disk.check(" plots/../b", &new_item()),
            Some(Problem::Reserved)
        );
        // A rename that keeps the spaces the name has still says so.
        assert_eq!(
            disk.check(" soup.md", &renaming("/p/ soup.md")),
            Some(Problem::Spaces)
        );
    }

    #[test]
    fn a_verdict_looks_at_the_disk_once() {
        let mut disk = Disk::new(&[("/p/soup.md", 2), ("/p/bread.md", 3)]);
        disk.check("stew.md", &new_item());
        assert_eq!(disk.looks, 1, "a free name");
        disk.check("soup.md", &new_item());
        assert_eq!(disk.looks, 2, "a taken one");
        disk.check("stew.md", &renaming("/p/bread.md"));
        assert_eq!(disk.looks, 3, "a rename to a free name");
        // A rename that lands on something asks which item it is.
        disk.check("soup.md", &renaming("/p/bread.md"));
        assert_eq!(disk.looks, 5);
        for wrong in ["", ".", "..", "/a", "a//b", "a/"] {
            disk.check(wrong, &new_item());
        }
        assert_eq!(disk.looks, 5, "a name wrong on its face asks nothing");
    }

    #[test]
    fn every_problem_says_one_short_line() {
        let all = [
            Problem::Empty,
            Problem::EmptyPart,
            Problem::LeadingSlash,
            Problem::Slash,
            Problem::Control,
            Problem::Reserved,
            Problem::TooLong,
            Problem::Taken,
            Problem::Blocked,
            Problem::Spaces,
        ];
        for problem in all {
            let said = problem.said();
            assert!(!said.is_empty() && said.len() <= 40, "{said:?}");
            assert!(!said.contains('\n') && !said.ends_with('.'), "{said:?}");
            assert_eq!(problem.blocks(), problem != Problem::Spaces);
        }
    }

    /// The real disk: a taken name, a dangling link, a file in the way of
    /// a path, and a change of case, which is the item itself wherever the
    /// volume says both spellings are one.
    #[test]
    fn the_disk_is_read_for_what_is_there() {
        let tree = TempTree::new(
            "naming",
            &[
                ("soup.md", "leeks\n"),
                ("bread.md", "flour\n"),
                ("plots/", ""),
            ],
        );
        let root = tree.0.as_path();
        std::os::unix::fs::symlink(root.join("nowhere"), root.join("link.md")).unwrap();
        let new = Ask {
            parent: root,
            nested: true,
            renaming: None,
        };
        assert_eq!(check("soup.md", &new), Some(Problem::Taken));
        assert_eq!(check("plots", &new), Some(Problem::Taken));
        assert_eq!(check("link.md", &new), Some(Problem::Taken));
        assert_eq!(check("stew.md", &new), None);
        assert_eq!(check("plots/herbs.md", &new), None);
        assert_eq!(check("soup.md/herbs.md", &new), Some(Problem::Blocked));
        let bread = root.join("bread.md");
        let rename = Ask {
            parent: root,
            nested: false,
            renaming: Some(&bread),
        };
        assert_eq!(check("soup.md", &rename), Some(Problem::Taken));
        assert_eq!(check("bread.md", &rename), None);
        // `Bread.md` is this very file where case is ignored and nothing
        // where it is not: free either way.
        assert_eq!(check("Bread.md", &rename), None);
        // `SOUP.md` is the other file where case is ignored.
        let ignores_case = root.join("SOUP.md").exists();
        assert_eq!(
            check("SOUP.md", &rename),
            ignores_case.then_some(Problem::Taken)
        );
    }
}
