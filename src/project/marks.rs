//! Git's status of the project's files, as the Explorer's rows show it:
//! one mark to a changed file, and one to every folder with a change
//! somewhere inside it.
//!
//! Built once from the status Source Control already has, when that status
//! is read again, and asked by path for every row drawn: a lookup, with no
//! Git process and nothing allocated.

use crate::project::git::{Change, Snapshot};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// What a row says about its file, or about what its folder holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Modified,
    /// New and staged.
    Added,
    Untracked,
    /// Gone for Git, and still listed.
    Deleted,
    Renamed,
    /// Unmerged: both sides changed it.
    Conflicted,
    /// A folder with changes inside.
    Contains,
    /// A folder with a conflict inside.
    ContainsConflict,
}

impl Mark {
    /// The letter at the row's right edge; a folder has a dot there.
    pub fn letter(self) -> Option<&'static str> {
        match self {
            Mark::Modified => Some("M"),
            Mark::Added => Some("A"),
            Mark::Untracked => Some("U"),
            Mark::Deleted => Some("D"),
            Mark::Renamed => Some("R"),
            Mark::Conflicted => Some("C"),
            Mark::Contains | Mark::ContainsConflict => None,
        }
    }

    /// Whether the mark is a folder's, for what is inside it.
    pub fn of_folder(self) -> bool {
        matches!(self, Mark::Contains | Mark::ContainsConflict)
    }

    /// What Git's two status letters for `change` come to.
    pub fn of(change: &Change) -> Mark {
        let either = |code: u8| change.index == code || change.worktree == code;
        if change.conflicted() {
            Mark::Conflicted
        } else if change.index == b'?' {
            Mark::Untracked
        } else if change.index == b'A' || change.index == b'C' {
            Mark::Added
        } else if either(b'R') {
            Mark::Renamed
        } else if either(b'D') {
            Mark::Deleted
        } else {
            Mark::Modified
        }
    }

    /// Which of two marks a path keeps: the heavier.
    fn weight(self) -> u8 {
        match self {
            Mark::Contains => 0,
            Mark::Deleted => 1,
            Mark::Modified => 2,
            Mark::Renamed => 3,
            Mark::Added => 4,
            Mark::Untracked => 5,
            Mark::ContainsConflict => 6,
            Mark::Conflicted => 7,
        }
    }
}

/// The marks of one status, by the path the tree has for the row.
#[derive(Debug, Default)]
pub struct Marks {
    /// Names the status these were built from; 0 for none.
    stamp: u64,
    by_path: HashMap<PathBuf, Mark>,
}

impl Marks {
    /// The marks of `snapshot`, the status of the repository `directory`
    /// is in, for the rows under `top`. `directory` is `top` or a folder
    /// in it, spelled as the tree spells it: Git names its top level by
    /// its resolved path, so what it reports is put back under
    /// `directory`. Changes outside `directory` have no row and no mark.
    ///
    /// A folder is marked for what it holds, up to but not including
    /// `top`.
    pub fn build(stamp: u64, snapshot: Option<&Snapshot>, directory: &Path, top: &Path) -> Marks {
        let mut marks = Marks {
            stamp,
            by_path: HashMap::new(),
        };
        let Some(snapshot) = snapshot else {
            return marks;
        };
        if snapshot.changes.is_empty() || !directory.starts_with(top) {
            return marks;
        }
        // Where `directory` is inside the repository. The disk is asked
        // only when the two are spelled differently.
        let below = match directory.strip_prefix(&snapshot.root) {
            Ok(below) => below.to_path_buf(),
            Err(_) => {
                let resolved = crate::platform::canonical(directory);
                match resolved.strip_prefix(&snapshot.root) {
                    Ok(below) => below.to_path_buf(),
                    Err(_) => return marks,
                }
            }
        };
        for change in &snapshot.changes {
            let Ok(inside) = change.path.strip_prefix(&below) else {
                continue;
            };
            let mark = Mark::of(change);
            let path = directory.join(inside);
            let folder = if mark == Mark::Conflicted {
                Mark::ContainsConflict
            } else {
                Mark::Contains
            };
            for dir in path.ancestors().skip(1) {
                if dir == top {
                    break;
                }
                match marks.by_path.get_mut(dir) {
                    // Marked this heavily already, and so is every folder
                    // above it.
                    Some(has) if has.of_folder() && has.weight() >= folder.weight() => break,
                    Some(has) if has.weight() >= folder.weight() => {}
                    Some(has) => *has = folder,
                    None => {
                        marks.by_path.insert(dir.to_path_buf(), folder);
                    }
                }
            }
            // A path Git lists twice (taken out of the index and still
            // on disk) keeps the heavier of its two marks.
            match marks.by_path.get_mut(&path) {
                Some(has) if has.weight() >= mark.weight() => {}
                Some(has) => *has = mark,
                None => {
                    marks.by_path.insert(path, mark);
                }
            }
        }
        marks
    }

    /// Which status these were built from.
    pub fn stamp(&self) -> u64 {
        self.stamp
    }

    /// The mark of the row at `path`, if it has one.
    pub fn get(&self, path: &Path) -> Option<Mark> {
        if self.by_path.is_empty() {
            return None;
        }
        self.by_path.get(path).copied()
    }

    pub fn len(&self) -> usize {
        self.by_path.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(path: &str, index: u8, worktree: u8) -> Change {
        Change {
            path: PathBuf::from(path),
            original: None,
            index,
            worktree,
        }
    }

    fn snapshot(root: &str, changes: Vec<Change>) -> Snapshot {
        Snapshot {
            root: PathBuf::from(root),
            branch: "main".into(),
            changes,
            in_progress: None,
            head_message: None,
            head_pushed: false,
        }
    }

    fn marks(root: &str, directory: &str, top: &str, changes: Vec<Change>) -> Marks {
        let snapshot = snapshot(root, changes);
        Marks::build(1, Some(&snapshot), Path::new(directory), Path::new(top))
    }

    fn at(marks: &Marks, path: &str) -> Option<Mark> {
        marks.get(Path::new(path))
    }

    #[test]
    fn each_state_of_a_file_has_its_mark_and_letter() {
        let cases: [(u8, u8, Mark, &str); 14] = [
            (b' ', b'M', Mark::Modified, "M"),
            (b'M', b' ', Mark::Modified, "M"),
            (b'M', b'M', Mark::Modified, "M"),
            (b' ', b'T', Mark::Modified, "M"),
            (b'A', b' ', Mark::Added, "A"),
            (b'A', b'M', Mark::Added, "A"),
            (b'?', b'?', Mark::Untracked, "U"),
            (b' ', b'D', Mark::Deleted, "D"),
            (b'D', b' ', Mark::Deleted, "D"),
            (b'R', b' ', Mark::Renamed, "R"),
            (b'R', b'M', Mark::Renamed, "R"),
            (b'U', b'U', Mark::Conflicted, "C"),
            (b'A', b'A', Mark::Conflicted, "C"),
            (b'D', b'U', Mark::Conflicted, "C"),
        ];
        for (index, worktree, mark, letter) in cases {
            let change = change("notes.md", index, worktree);
            assert_eq!(Mark::of(&change), mark, "{}", change.status());
            assert_eq!(mark.letter(), Some(letter));
            assert!(!mark.of_folder());
        }
        assert_eq!(Mark::Contains.letter(), None);
        assert_eq!(Mark::ContainsConflict.letter(), None);
    }

    #[test]
    fn files_are_marked_by_the_path_the_tree_has_for_them() {
        let marks = marks(
            "/p",
            "/p",
            "/p",
            vec![
                change("journal.md", b' ', b'M'),
                change("garden.txt", b'A', b' '),
                change("recipes/pancakes.md", b'?', b'?'),
                change("groceries.txt", b'R', b' '),
            ],
        );
        assert_eq!(at(&marks, "/p/journal.md"), Some(Mark::Modified));
        assert_eq!(at(&marks, "/p/garden.txt"), Some(Mark::Added));
        assert_eq!(at(&marks, "/p/recipes/pancakes.md"), Some(Mark::Untracked));
        assert_eq!(at(&marks, "/p/groceries.txt"), Some(Mark::Renamed));
        // What has not changed has no mark, and neither has an ignored
        // file: Git's status does not list it, and the tree dims it.
        assert_eq!(at(&marks, "/p/letters.txt"), None);
        assert_eq!(at(&marks, "/p/scratch.log"), None);
        assert_eq!(marks.stamp(), 1);
    }

    #[test]
    fn a_folder_is_marked_for_what_it_holds_and_a_conflict_wins() {
        let marks = marks(
            "/p",
            "/p",
            "/p",
            vec![
                change("recipes/baking/scones.md", b' ', b'M'),
                change("recipes/bread.md", b' ', b'M'),
                change("plots/beds/tomatoes.md", b'U', b'U'),
                change("plots/herbs.md", b' ', b'M'),
                change("trips/old/lisbon.md", b' ', b'D'),
            ],
        );
        assert_eq!(at(&marks, "/p/recipes"), Some(Mark::Contains));
        assert_eq!(at(&marks, "/p/recipes/baking"), Some(Mark::Contains));
        // A conflict anywhere inside wins over the changes beside it,
        // whichever was listed first.
        assert_eq!(at(&marks, "/p/plots"), Some(Mark::ContainsConflict));
        assert_eq!(at(&marks, "/p/plots/beds"), Some(Mark::ContainsConflict));
        assert_eq!(at(&marks, "/p/plots/herbs.md"), Some(Mark::Modified));
        // A deletion inside is a change inside.
        assert_eq!(at(&marks, "/p/trips"), Some(Mark::Contains));
        assert_eq!(at(&marks, "/p/trips/old"), Some(Mark::Contains));
        // The project root has no row, and nothing above it is marked.
        assert_eq!(at(&marks, "/p"), None);
        assert_eq!(at(&marks, "/"), None);
        assert_eq!(marks.len(), 5 + 6);
    }

    #[test]
    fn a_conflict_listed_after_a_change_still_wins_all_the_way_up() {
        let marks = marks(
            "/p",
            "/p",
            "/p",
            vec![
                change("plots/beds/herbs.md", b' ', b'M'),
                change("plots/beds/deep/tomatoes.md", b'U', b'U'),
                change("plots/beds/deep/beans.md", b'?', b'?'),
            ],
        );
        for folder in ["/p/plots", "/p/plots/beds", "/p/plots/beds/deep"] {
            assert_eq!(at(&marks, folder), Some(Mark::ContainsConflict), "{folder}");
        }
    }

    #[test]
    fn a_path_listed_twice_keeps_the_heavier_mark() {
        // Taken out of the index and still on disk: deleted, and untracked.
        let marks = marks(
            "/p",
            "/p",
            "/p",
            vec![
                change("letters.txt", b'D', b' '),
                change("letters.txt", b'?', b'?'),
            ],
        );
        assert_eq!(at(&marks, "/p/letters.txt"), Some(Mark::Untracked));
    }

    #[test]
    fn a_project_inside_a_repository_marks_only_what_is_under_it() {
        // The repository is above the folder that is open.
        let marks = marks(
            "/repo",
            "/repo/notebook",
            "/repo/notebook",
            vec![
                change("notebook/journal.md", b' ', b'M'),
                change("notebook/recipes/bread.md", b' ', b'M'),
                change("elsewhere/plan.md", b' ', b'M'),
                change("top.md", b'?', b'?'),
            ],
        );
        assert_eq!(
            at(&marks, "/repo/notebook/journal.md"),
            Some(Mark::Modified)
        );
        assert_eq!(at(&marks, "/repo/notebook/recipes"), Some(Mark::Contains));
        assert_eq!(at(&marks, "/repo/elsewhere/plan.md"), None);
        assert_eq!(at(&marks, "/repo/top.md"), None);
        assert_eq!(marks.len(), 3);
    }

    #[test]
    fn a_repository_inside_the_project_marks_the_folders_down_to_it() {
        // One of several repositories in the open folder: the one Source
        // Control shows. The folders between it and the project root hold
        // its changes too.
        let marks = marks(
            "/p/books/cooking",
            "/p/books/cooking",
            "/p",
            vec![change("recipes/bread.md", b' ', b'M')],
        );
        assert_eq!(
            at(&marks, "/p/books/cooking/recipes/bread.md"),
            Some(Mark::Modified)
        );
        for folder in ["/p/books/cooking/recipes", "/p/books/cooking", "/p/books"] {
            assert_eq!(at(&marks, folder), Some(Mark::Contains), "{folder}");
        }
        assert_eq!(at(&marks, "/p"), None);
        // Another repository beside it is not in this status: no marks.
        assert_eq!(at(&marks, "/p/books/travel/lisbon.md"), None);
    }

    #[test]
    fn a_project_opened_through_a_link_is_marked_under_the_spelling_it_has() {
        // Git reports the resolved top level; the tree's rows are under
        // the path the folder was opened by.
        let real = crate::project::TempTree::new("marks-real", &[("journal.md", "a day\n")]);
        let link = real
            .0
            .with_file_name(format!("crc-marks-link-{}", std::process::id()));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real.0, &link).unwrap();
        let root = crate::platform::canonical(&real.0);
        let snapshot = snapshot(
            &root.to_string_lossy(),
            vec![change("journal.md", b' ', b'M')],
        );
        let marks = Marks::build(3, Some(&snapshot), &link, &link);
        std::fs::remove_file(&link).unwrap();
        assert_eq!(marks.get(&link.join("journal.md")), Some(Mark::Modified));
        assert_eq!(marks.get(&root.join("journal.md")), None);
    }

    #[test]
    fn no_repository_and_no_changes_mark_nothing() {
        let none = Marks::build(4, None, Path::new("/p"), Path::new("/p"));
        assert!(none.is_empty());
        assert_eq!(none.stamp(), 4);
        assert_eq!(at(&none, "/p/journal.md"), None);
        let clean = marks("/p", "/p", "/p", Vec::new());
        assert!(clean.is_empty());
        assert_eq!(Marks::default().stamp(), 0);
    }

    /// The tree builds its marks when the status is a new one and not
    /// again: asked every frame with the same stamp, it keeps the very
    /// map it has.
    #[test]
    fn the_tree_builds_its_marks_once_for_each_status() {
        use crate::project::tree::Tree;
        let files = crate::project::TempTree::new(
            "marks-sync",
            &[("journal.md", "a day\n"), ("letters.txt", "dear anna\n")],
        );
        let root = files.0.as_path();
        let at = root.to_string_lossy();
        let mut tree = Tree::new();
        tree.open(root);
        let journal = root.join("journal.md");
        let letters = root.join("letters.txt");
        // No status yet: nothing to build, nothing marked.
        assert!(!tree.sync_git(0, None, root));
        assert_eq!(tree.git_mark(&journal), None);

        let first = snapshot(&at, vec![change("journal.md", b' ', b'M')]);
        assert!(tree.sync_git(7, Some(&first), root), "a new status");
        assert_eq!(tree.git_mark(&journal), Some(Mark::Modified));
        let built = std::sync::Arc::as_ptr(tree.git_marks());
        for _ in 0..3 {
            assert!(!tree.sync_git(7, Some(&first), root), "the same status");
        }
        assert_eq!(
            std::sync::Arc::as_ptr(tree.git_marks()),
            built,
            "not built again"
        );
        // A copy handed to a worker and back carries the marks with it.
        let mut copy = tree.clone();
        assert!(!copy.sync_git(7, Some(&first), root));
        assert_eq!(std::sync::Arc::as_ptr(copy.git_marks()), built);

        let second = snapshot(&at, vec![change("letters.txt", b'?', b'?')]);
        assert!(tree.sync_git(8, Some(&second), root), "the next status");
        assert_eq!(tree.git_mark(&journal), None);
        assert_eq!(tree.git_mark(&letters), Some(Mark::Untracked));

        // Another folder opened: the marks go with the old one, and the
        // new panel has no status yet.
        tree.set_root(root.join("elsewhere"));
        assert_eq!(tree.git_mark(&letters), None);
        assert!(!tree.sync_git(0, None, root));
    }
}
