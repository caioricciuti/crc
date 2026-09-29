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
    outline: Option<Vec<Definition>>,
    /// The outline being parsed on a worker, from the moment the palette
    /// opens: a few megabytes of tree-sitter is not a main-thread job.
    parsing: Option<std::sync::mpsc::Receiver<Vec<Definition>>>,
    project: RefCell<Option<(String, Vec<Hit>)>>,
    /// The index file, when it is not the root's usual one (tests).
    index: Option<PathBuf>,
    /// A `#` query being answered on a worker: a substring match over
    /// every symbol in the index cannot use its index, so it scans.
    project_rx: RefCell<Option<(String, std::sync::mpsc::Receiver<Vec<Hit>>)>>,
}

impl Symbols {
    /// Forgets the last palette's lists and remembers what this one opens on.
    pub fn reset(&mut self, document: Option<(Language, Rope)>, root: Option<PathBuf>) {
        self.document = document;
        self.root = root;
        self.outline = None;
        *self.project_rx.get_mut() = None;
        self.parsing = None;
        if let Some((language, rope)) = &self.document {
            if rope.len_bytes() <= MAX_DOCUMENT_BYTES {
                let (language, rope) = (*language, rope.clone());
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(defs::definitions(language, &rope.to_string()));
                });
                self.parsing = Some(rx);
            } else {
                self.outline = Some(Vec::new());
            }
        }
        *self.project.get_mut() = None;
    }

    /// The rows for `needle` in `scope`, best first.
    pub fn search(&self, scope: Scope, needle: &str) -> Vec<Hit> {
        match scope {
            Scope::Document => self.document_hits(needle),
            Scope::Project => self.project_hits(needle),
        }
    }

    /// Takes the outline once the worker has it. `true` when it just
    /// arrived, so the palette is drawn again with it.
    pub fn poll(&mut self) -> bool {
        let mut arrived = false;
        let answered =
            self.project_rx
                .get_mut()
                .as_ref()
                .and_then(|(needle, rx)| match rx.try_recv() {
                    Ok(hits) => Some(Some((needle.clone(), hits))),
                    Err(std::sync::mpsc::TryRecvError::Empty) => None,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(None),
                });
        if let Some(answer) = answered {
            *self.project_rx.get_mut() = None;
            if let Some(answer) = answer {
                *self.project.get_mut() = Some(answer);
            }
            arrived = true;
        }
        let Some(rx) = &self.parsing else {
            return arrived;
        };
        match rx.try_recv() {
            Ok(definitions) => {
                self.outline = Some(definitions);
                self.parsing = None;
                true
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => arrived,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.outline = Some(Vec::new());
                self.parsing = None;
                true
            }
        }
    }

    /// Whether the outline is still being parsed.
    pub fn pending(&self) -> bool {
        self.parsing.is_some() || self.project_rx.borrow().is_some()
    }

    fn document_hits(&self, needle: &str) -> Vec<Hit> {
        // Still parsing: nothing yet, and the palette fills in when it lands.
        let Some(definitions) = self.outline.as_ref() else {
            return Vec::new();
        };
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
        let shown = || {
            self.project
                .borrow()
                .as_ref()
                .map(|(_, hits)| hits.clone())
                .unwrap_or_default()
        };
        if let Some((cached, hits)) = &*self.project.borrow()
            && cached == needle
        {
            return hits.clone();
        }
        // Asked already: the last answer stands in until this one lands.
        if self
            .project_rx
            .borrow()
            .as_ref()
            .is_some_and(|(asked, _)| asked == needle)
        {
            return shown();
        }
        let (Some(root), false) = (self.root.clone(), needle.is_empty()) else {
            *self.project.borrow_mut() = Some((needle.to_string(), Vec::new()));
            return Vec::new();
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let asked = needle.to_string();
        let index = self.index.clone().or_else(|| store::db_path(&root));
        std::thread::spawn(move || {
            let _ = tx.send(read_project(&root, index.as_deref(), &asked));
        });
        *self.project_rx.borrow_mut() = Some((needle.to_string(), rx));
        shown()
    }
}

/// `#` rows for `needle`, from the index on disk. On a worker.
fn read_project(root: &Path, index: Option<&Path>, needle: &str) -> Vec<Hit> {
    let Some(reader) = index.and_then(Reader::open) else {
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

    /// Waits for the outline's worker, as the display link would.
    fn settle(symbols: &mut Symbols) {
        let started = std::time::Instant::now();
        while symbols.pending() && started.elapsed().as_secs() < 10 {
            symbols.poll();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
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
        assert!(symbols.search(Scope::Document, "").is_empty() || !symbols.pending());
        settle(&mut symbols);
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
        settle(&mut symbols);
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

        let mut symbols = Symbols {
            root: Some(dir.clone()),
            index: Some(db.clone()),
            ..Symbols::default()
        };
        symbols.search(Scope::Project, "revpa");
        settle(&mut symbols);
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
