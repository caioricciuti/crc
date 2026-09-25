//! Go to symbol, the palette's `@` and `#` modes.
//!
//! `@` lists what the active document defines, in document order when the
//! query is empty, so it doubles as the outline. `#` asks the project index
//! (`index::store`) for definitions across the project. Both read the same
//! tree-sitter queries (`syntax::defs`) the index is built from.
//!
//! The palette asks for its rows several times a frame (drawing, the dump,
//! hit testing), so both lists are cached here: the document's definitions
//! for as long as the palette stays open, the project's per query.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use crate::index::store::{self, Reader};
use crate::project::finder;
use crate::syntax::Language;
use crate::syntax::defs::{self, Definition};
use crate::text::rope::Rope;

/// Rows shown at most, the palette's own limit.
const LIMIT: usize = 100;
/// Candidates read from SQLite before ranking.
const CANDIDATES: usize = 2_000;
/// A document past this is not parsed for its outline on the main thread.
const MAX_DOCUMENT_BYTES: usize = 4 * 1024 * 1024;

/// Which symbols a query asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// `@`: the active document.
    Document,
    /// `#`: the whole project.
    Project,
}

/// The scope and the rest of the query, when the palette's query asks for
/// symbols.
pub fn query(text: &str) -> Option<(Scope, &str)> {
    if let Some(rest) = text.strip_prefix('@') {
        Some((Scope::Document, rest))
    } else {
        text.strip_prefix('#').map(|rest| (Scope::Project, rest))
    }
}

/// One row: what it is called and where it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub name: String,
    pub kind: String,
    /// Absolute. `None` for the active document.
    pub path: Option<PathBuf>,
    /// Zero-based.
    pub line: u32,
}

/// What the palette was opened on, and what has been worked out from it.
#[derive(Default)]
pub struct Symbols {
    /// The active document when the palette opened: an O(1) rope clone.
    document: Option<(Language, Rope)>,
    root: Option<PathBuf>,
    outline: RefCell<Option<Vec<Definition>>>,
    reader: RefCell<Option<Reader>>,
    project: RefCell<Option<(String, Vec<Hit>)>>,
}

impl Symbols {
    /// Forgets the last palette's lists and remembers what this one opens on.
    pub fn reset(&mut self, document: Option<(Language, Rope)>, root: Option<PathBuf>) {
        let root_changed = self.root != root;
        self.document = document;
        self.root = root;
        *self.outline.get_mut() = None;
        *self.project.get_mut() = None;
        if root_changed {
            *self.reader.get_mut() = None;
        }
    }

    /// The rows for `needle` in `scope`, best first.
    pub fn search(&self, scope: Scope, needle: &str) -> Vec<Hit> {
        match scope {
            Scope::Document => self.document_hits(needle),
            Scope::Project => self.project_hits(needle),
        }
    }

    fn document_hits(&self, needle: &str) -> Vec<Hit> {
        let mut outline = self.outline.borrow_mut();
        let definitions = outline.get_or_insert_with(|| match &self.document {
            Some((language, rope)) if rope.len_bytes() <= MAX_DOCUMENT_BYTES => {
                defs::definitions(*language, &rope.to_string())
            }
            _ => Vec::new(),
        });
        let hits = rank(definitions.iter(), needle, |d| &d.name);
        hits.into_iter()
            .map(|d| Hit {
                name: d.name.clone(),
                kind: d.kind.to_string(),
                path: None,
                line: d.line,
            })
            .collect()
    }

    fn project_hits(&self, needle: &str) -> Vec<Hit> {
        let needle = needle.trim();
        if let Some((cached, hits)) = &*self.project.borrow()
            && cached == needle
        {
            return hits.clone();
        }
        let hits = self.read_project(needle);
        *self.project.borrow_mut() = Some((needle.to_string(), hits.clone()));
        hits
    }

    fn read_project(&self, needle: &str) -> Vec<Hit> {
        if needle.is_empty() {
            return Vec::new();
        }
        let Some(root) = &self.root else {
            return Vec::new();
        };
        let mut reader = self.reader.borrow_mut();
        if reader.is_none() {
            *reader = store::db_path(root).and_then(|path| Reader::open(&path));
        }
        let Some(reader) = reader.as_ref() else {
            return Vec::new();
        };
        let found = reader
            .containing(&needle.to_lowercase(), CANDIDATES)
            .unwrap_or_default();
        rank(found.iter(), needle, |s| &s.name)
            .into_iter()
            .map(|s| Hit {
                name: s.name.clone(),
                kind: s.kind.clone(),
                path: Some(resolve(root, &s.path)),
                line: s.line,
            })
            .collect()
    }
}

/// The index stores paths relative to the root; an absolute one is kept.
fn resolve(root: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

/// Items whose name matches `needle` as a subsequence, best first; every
/// item in its own order when `needle` is empty. Ties go to the shorter
/// name, then to the earlier item.
fn rank<'a, T>(
    items: impl Iterator<Item = &'a T>,
    needle: &str,
    name: impl Fn(&T) -> &str,
) -> Vec<&'a T>
where
    T: 'a,
{
    let needle: Vec<char> = needle.trim().to_lowercase().chars().collect();
    if needle.is_empty() {
        return items.take(LIMIT).collect();
    }
    let mut scored: Vec<(i32, usize, usize, &T)> = items
        .enumerate()
        .filter_map(|(at, item)| {
            let name = name(item);
            finder::score(&name.to_lowercase(), &needle)
                .map(|(points, _)| (points, name.len(), at, item))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    scored
        .into_iter()
        .take(LIMIT)
        .map(|(.., item)| item)
        .collect()
}

/// The palette's detail text for a hit: the kind, and for another file its
/// path relative to the project and the line.
pub fn detail(hit: &Hit, root: Option<&Path>) -> String {
    match &hit.path {
        None => format!("{}  ·  line {}", hit.kind, hit.line + 1),
        Some(path) => {
            let shown = root
                .and_then(|root| path.strip_prefix(root).ok())
                .unwrap_or(path);
            format!("{}  ·  {}:{}", hit.kind, shown.display(), hit.line + 1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_pick_the_scope() {
        assert_eq!(query("@parse"), Some((Scope::Document, "parse")));
        assert_eq!(query("#Tree"), Some((Scope::Project, "Tree")));
        assert_eq!(query("main.rs"), None);
        assert_eq!(query(">commit"), None);
    }

    #[test]
    fn empty_document_query_is_the_outline_in_order() {
        let mut symbols = Symbols::default();
        symbols.reset(
            Some((
                Language::Rust,
                Rope::from_text("struct Zebra;\nfn apple() {}\nfn mango() {}\n"),
            )),
            None,
        );
        let names: Vec<String> = symbols
            .search(Scope::Document, "")
            .into_iter()
            .map(|h| h.name)
            .collect();
        assert_eq!(names, ["Zebra", "apple", "mango"]);
    }

    #[test]
    fn document_query_ranks_word_starts_first() {
        let mut symbols = Symbols::default();
        symbols.reset(
            Some((
                Language::Rust,
                Rope::from_text("fn reparse_all() {}\nfn parse() {}\nfn spare() {}\n"),
            )),
            None,
        );
        let hits = symbols.search(Scope::Document, "parse");
        assert_eq!(hits[0].name, "parse");
        assert_eq!(hits[0].line, 1);
        assert!(hits.iter().all(|h| h.path.is_none()));
    }

    #[test]
    fn project_query_reads_the_index() {
        let dir = std::env::temp_dir().join(format!("crc-symbols-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/tree.rs"),
            "pub struct Tree;\nfn reveal_path() {}\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/other.rs"), "fn unrelated() {}\n").unwrap();
        let db = dir.join("index.db");
        let store = store::Store::open(&db).unwrap();
        store
            .sync(&dir, &[dir.join("src/tree.rs"), dir.join("src/other.rs")])
            .unwrap();
        drop(store);

        let symbols = Symbols {
            root: Some(dir.clone()),
            reader: RefCell::new(Reader::open(&db)),
            ..Symbols::default()
        };
        let hits = symbols.search(Scope::Project, "revpa");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "reveal_path");
        assert_eq!(hits[0].line, 1);
        assert_eq!(
            hits[0].path.as_deref(),
            Some(dir.join("src/tree.rs").as_path())
        );
        assert_eq!(detail(&hits[0], Some(&dir)), "function  ·  src/tree.rs:2");
        assert!(symbols.search(Scope::Project, "").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
