//! Fuzzy file matching for the Cmd-P palette.
//!
//! Subsequence matching with a score, which is what people mean by "fuzzy
//! open": typing `rlay` should find `src/render/layout.rs`. Scoring, not just
//! filtering, is the whole feature — a list of forty files that all contain
//! the letters in order is no more useful than no list at all.
//!
//! The file list is walked once when a folder opens and kept in memory.
//! Nothing watches the filesystem, so [`Finder::rescan`] exists for after a
//! file is created.

use std::path::{Path, PathBuf};

/// Directories never worth indexing. Same list the sidebar hides, for the
/// same reason: they are enormous and nobody opens files in them by name.
const SKIP_DIRS: &[&str] = &["target", "node_modules", "vendor", "third_party", ".git"];

/// How deep to walk. A guard against a symlink loop or a pathological tree
/// rather than a real limit.
const MAX_DEPTH: usize = 12;

/// Cap on indexed files, so opening `/` does not hang the editor.
const MAX_FILES: usize = 20_000;

/// One candidate, with its display form precomputed.
#[derive(Clone, Debug)]
pub struct Entry {
    pub path: PathBuf,
    /// Path relative to the project root, which is what gets matched and
    /// shown. Absolute paths are mostly shared prefix and waste the width.
    pub relative: String,
    /// Lowercased once, because matching is case-insensitive and doing it
    /// per keystroke per file is the whole cost of the feature.
    lower: String,
}

/// A scored match, with the positions that matched so they can be shown.
#[derive(Clone, Debug)]
pub struct Match {
    pub index: usize,
    pub score: i32,
    /// Byte offsets in `relative` that the query matched.
    pub positions: Vec<usize>,
}

#[derive(Default)]
pub struct Finder {
    entries: Vec<Entry>,
    root: Option<PathBuf>,
}

impl Finder {
    pub fn new() -> Self {
        Finder::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entry(&self, index: usize) -> Option<&Entry> {
        self.entries.get(index)
    }

    /// Indexes every file under `root`.
    pub fn scan(&mut self, root: impl Into<PathBuf>) {
        let root = root.into();
        self.entries.clear();
        walk(&root, &root, 0, &mut self.entries);
        // Shortest path first, so an exact-ish query surfaces the top-level
        // file rather than a deeply nested one that happens to score alike.
        self.entries.sort_by(|a, b| {
            a.relative
                .len()
                .cmp(&b.relative.len())
                .then_with(|| a.relative.cmp(&b.relative))
        });
        self.root = Some(root);
    }

    /// Re-walks the current root, if there is one.
    pub fn rescan(&mut self) {
        if let Some(root) = self.root.clone() {
            self.scan(root);
        }
    }

    /// Best matches for `query`, highest score first.
    ///
    /// An empty query returns the first `limit` files rather than nothing, so
    /// the palette is useful the instant it opens.
    pub fn search(&self, query: &str, limit: usize) -> Vec<Match> {
        if query.is_empty() {
            return self
                .entries
                .iter()
                .enumerate()
                .take(limit)
                .map(|(index, _)| Match {
                    index,
                    score: 0,
                    positions: Vec::new(),
                })
                .collect();
        }

        let needle: Vec<char> = query.to_lowercase().chars().collect();
        let mut hits: Vec<Match> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                score_entry(entry, &needle).map(|(score, positions)| Match {
                    index,
                    score,
                    positions,
                })
            })
            .collect();

        // Highest score first; ties broken by the shorter path, which is
        // almost always the one meant.
        hits.sort_by(|a, b| {
            b.score.cmp(&a.score).then_with(|| {
                self.entries[a.index]
                    .relative
                    .len()
                    .cmp(&self.entries[b.index].relative.len())
            })
        });
        hits.truncate(limit);
        hits
    }
}

/// How much better a match in the file name is than one in the directories.
///
/// Larger than any score a path match can reach, so the ordering is
/// categorical: every name match ranks above every path-only match.
const NAME_MATCH_BONUS: i32 = 10_000;

/// Scores an entry, preferring the file name over the directories it sits in.
///
/// Matching the whole relative path as one string made every file in a
/// matching directory score alike: typing `app-server-` in a project with an
/// `app-server-ng` folder returned each of its files with an identical score,
/// so the order fell back to path length and `.DS_Store` outranked the file
/// that was wanted. The name is what is being looked for; the directory is
/// context.
fn score_entry(entry: &Entry, needle: &[char]) -> Option<(i32, Vec<usize>)> {
    let name_start = entry.lower.rfind('/').map(|slash| slash + 1).unwrap_or(0);
    if let Some((points, positions)) = score(&entry.lower[name_start..], needle) {
        return Some((
            points + NAME_MATCH_BONUS,
            positions.into_iter().map(|at| at + name_start).collect(),
        ));
    }
    score(&entry.lower, needle)
}

/// Scores `needle` against `haystack`, or `None` if it is not a subsequence.
///
/// The scoring is what separates this from a filter. Consecutive matches and
/// matches at a word boundary score far higher, which is why `rlay` puts
/// `render/layout.rs` above some file that merely contains r, l, a, y in
/// that order.
pub fn score(haystack: &str, needle: &[char]) -> Option<(i32, Vec<usize>)> {
    if needle.is_empty() {
        return Some((0, Vec::new()));
    }

    let mut positions = Vec::with_capacity(needle.len());
    let mut total = 0i32;
    let mut needle_at = 0usize;
    let mut previous_match: Option<usize> = None;

    for (offset, ch) in haystack.char_indices() {
        if needle_at >= needle.len() {
            break;
        }
        if ch != needle[needle_at] {
            continue;
        }

        let mut points = 10;

        // Adjacent to the previous match: strongly preferred, because it
        // means the query is a real substring of the name.
        if previous_match.is_some_and(|p| haystack[p..offset].chars().count() == 1) {
            points += 15;
        }

        // At a word boundary, or the very start.
        let preceding = haystack[..offset].chars().next_back();
        match preceding {
            None => points += 20,
            Some('/') => points += 18,
            Some('_') | Some('-') | Some('.') | Some(' ') => points += 12,
            _ => {}
        }

        total += points;
        positions.push(offset);
        previous_match = Some(offset);
        needle_at += 1;
    }

    if needle_at < needle.len() {
        return None;
    }

    // Prefer shorter paths: the same score spread over less text is a
    // tighter match.
    total -= (haystack.len() as i32) / 8;
    Some((total, positions))
}

/// Recursively collects files, including dotfiles and configuration directories.
/// Git internals and known dependency/build directories remain excluded.
fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<Entry>) {
    if depth > MAX_DEPTH || out.len() >= MAX_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        if out.len() >= MAX_FILES {
            return;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };

        // Not followed: a symlinked directory can point at its own ancestor,
        // and the depth cap alone would still let it index the same tree many
        // times over.
        if kind.is_symlink() {
            continue;
        }

        if kind.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk(root, &path, depth + 1, out);
        } else if kind.is_file() {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let lower = relative.to_lowercase();
            out.push(Entry {
                path,
                relative,
                lower,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("caio-finder-{name}"));
            let _ = std::fs::remove_dir_all(&root);
            for dir in ["src/render", "src/text", "target/debug", ".git"] {
                std::fs::create_dir_all(root.join(dir)).expect("mkdir");
            }
            for file in [
                "README.md",
                "Cargo.toml",
                "src/main.rs",
                "src/render/layout.rs",
                "src/render/metal.rs",
                "src/text/rope.rs",
                "target/debug/junk.o",
                ".git/config",
            ] {
                std::fs::write(root.join(file), "x").expect("write");
            }
            Fixture(root)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn finder(f: &Fixture) -> Finder {
        let mut finder = Finder::new();
        finder.scan(&f.0);
        finder
    }

    #[test]
    fn indexes_files_and_skips_noise() {
        let f = Fixture::new("index");
        let finder = finder(&f);
        let names: Vec<&str> = finder.entries.iter().map(|e| e.relative.as_str()).collect();

        assert!(names.contains(&"README.md"));
        assert!(names.iter().any(|n| n.ends_with("layout.rs")));
        assert!(
            !names.iter().any(|n| n.contains("target")),
            "build output should not be indexed"
        );
        assert!(
            !names.iter().any(|n| n.contains(".git")),
            "Git internals should not be indexed"
        );
    }

    #[test]
    fn finds_dotfiles_in_nested_and_hidden_directories() {
        let f = Fixture::new("dotfiles");
        std::fs::create_dir_all(f.0.join(".github/workflows")).unwrap();
        for name in [
            ".env",
            ".gitignore",
            "src/.env",
            ".github/workflows/check.yml",
        ] {
            std::fs::write(f.0.join(name), "example").unwrap();
        }
        let finder = finder(&f);
        for name in [
            ".env",
            ".gitignore",
            "src/.env",
            ".github/workflows/check.yml",
        ] {
            assert!(finder.entries.iter().any(|e| e.relative == name));
        }
        assert!(!finder.search(".env", 10).is_empty());
        assert!(
            !finder
                .entries
                .iter()
                .any(|e| e.relative.starts_with(".git/"))
        );
    }

    #[test]
    fn an_empty_query_lists_files_rather_than_nothing() {
        let f = Fixture::new("empty");
        let finder = finder(&f);
        let hits = finder.search("", 3);
        assert_eq!(
            hits.len(),
            3,
            "the palette should be useful before you type"
        );
    }

    #[test]
    fn matches_a_scattered_subsequence() {
        let f = Fixture::new("subseq");
        let finder = finder(&f);
        let hits = finder.search("rlay", 10);
        let best = finder.entry(hits[0].index).expect("a hit");
        assert!(
            best.relative.ends_with("render/layout.rs"),
            "expected render/layout.rs, got {}",
            best.relative
        );
    }

    #[test]
    fn consecutive_and_boundary_matches_outrank_scattered_ones() {
        let f = Fixture::new("rank");
        let finder = finder(&f);
        // "rope" is a contiguous filename match; it must beat anything that
        // merely contains r, o, p, e in order.
        let hits = finder.search("rope", 10);
        let best = finder.entry(hits[0].index).expect("a hit");
        assert!(best.relative.ends_with("rope.rs"), "got {}", best.relative);
    }

    #[test]
    fn matching_is_case_insensitive() {
        let f = Fixture::new("case");
        let finder = finder(&f);
        assert!(!finder.search("README", 5).is_empty());
        assert!(!finder.search("readme", 5).is_empty());
        assert!(!finder.search("ReAdMe", 5).is_empty());
    }

    #[test]
    fn a_query_that_is_not_a_subsequence_finds_nothing() {
        let f = Fixture::new("miss");
        let finder = finder(&f);
        assert!(finder.search("zzzzqqq", 10).is_empty());
    }

    #[test]
    fn reports_which_characters_matched() {
        let f = Fixture::new("positions");
        let finder = finder(&f);
        let hits = finder.search("rope", 5);
        let hit = &hits[0];
        let entry = finder.entry(hit.index).expect("a hit");
        assert_eq!(hit.positions.len(), 4, "one position per query character");
        // Every reported position must actually be in the string.
        assert!(hit.positions.iter().all(|&p| p < entry.relative.len()));
        assert!(
            hit.positions.windows(2).all(|w| w[0] < w[1]),
            "positions must be ascending"
        );
    }

    #[test]
    fn rescan_picks_up_new_files() {
        let f = Fixture::new("rescan");
        let mut finder = finder(&f);
        let before = finder.len();
        std::fs::write(f.0.join("src/added.rs"), "x").expect("write");
        finder.rescan();
        assert_eq!(finder.len(), before + 1);
    }

    #[test]
    fn scanning_a_missing_root_is_empty_not_a_panic() {
        let mut finder = Finder::new();
        finder.scan("/definitely/not/a/real/path");
        assert!(finder.is_empty());
        assert!(finder.search("x", 5).is_empty());
    }
}

#[cfg(test)]
mod ranking_tests {
    use super::*;

    fn entry(relative: &str) -> Entry {
        Entry {
            path: PathBuf::from(relative),
            relative: relative.to_string(),
            lower: relative.to_lowercase(),
        }
    }

    fn rank(entries: &[Entry], query: &str) -> Vec<String> {
        let needle: Vec<char> = query.to_lowercase().chars().collect();
        let mut hits: Vec<(i32, &Entry)> = entries
            .iter()
            .filter_map(|e| score_entry(e, &needle).map(|(s, _)| (s, e)))
            .collect();
        hits.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.relative.len().cmp(&b.1.relative.len()))
        });
        hits.into_iter().map(|(_, e)| e.relative.clone()).collect()
    }

    /// The reported case: a query that names a directory gave every file in
    /// it the same score, so the order collapsed to "shortest path" and
    /// `.DS_Store` came out above the file that was wanted. A name match now
    /// outranks a directory match categorically.
    #[test]
    fn a_name_match_outranks_a_directory_match() {
        let entries = [
            entry("app-server-ng/go.mod"),
            entry("app-server-ng/.DS_Store"),
            entry("app-server-ng/Makefile"),
            entry("tools/app-server-notes.md"),
        ];
        let order = rank(&entries, "app-server-");
        assert_eq!(
            order.first().map(String::as_str),
            Some("tools/app-server-notes.md"),
            "the file actually named app-server- should lead: {order:?}"
        );
    }

    #[test]
    fn a_name_match_still_beats_a_shorter_path() {
        let entries = [entry("a/b.rs"), entry("some/deep/nested/layout.rs")];
        let order = rank(&entries, "layout");
        assert_eq!(
            order.first().map(String::as_str),
            Some("some/deep/nested/layout.rs")
        );
    }

    /// Positions are reported against the whole relative path, since that is
    /// what the palette draws; a name match must offset them past the
    /// directories or the highlight lands on the wrong characters.
    #[test]
    fn matched_positions_index_the_relative_path() {
        let e = entry("src/render/layout.rs");
        let needle: Vec<char> = "layout".chars().collect();
        let (_, positions) = score_entry(&e, &needle).expect("matches");
        let matched: String = positions
            .iter()
            .map(|&at| e.relative[at..].chars().next().unwrap())
            .collect();
        assert_eq!(matched, "layout");
        assert!(
            positions[0] >= "src/render/".len(),
            "highlight landed in the directory"
        );
    }

    /// A query that only the directories contain still matches, so files stay
    /// findable by where they live.
    #[test]
    fn a_directory_only_match_is_still_a_match() {
        let entries = [entry("src/render/metal.rs")];
        assert_eq!(rank(&entries, "render"), vec!["src/render/metal.rs"]);
    }
}
