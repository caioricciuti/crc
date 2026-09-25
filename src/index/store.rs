//! The project index on disk: which files, what each defines, which words
//! each uses. Built and kept current by one writer thread ([`Indexer`]);
//! read by the completion worker through its own connection ([`Reader`]).
//!
//! It lives in `~/Library/Caches/crc/index/`, one file per project, never in
//! the project: nothing to ignore, nothing to commit, and deleting it only
//! costs a rebuild. It is plain SQLite, so `sqlite3` can query it:
//!
//! ```sql
//! SELECT name, kind, path, line FROM symbols JOIN files ON files.id = file
//! WHERE name LIKE 'parse%';
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, UNIX_EPOCH};

use super::db::{Db, Result, Value};

/// Files past this are not read: generated bundles and data, mostly.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Files past this many are not indexed, in a project too big to be one.
const MAX_FILES: usize = 30_000;
/// A file's most used words, at most this many.
const WORDS_PER_FILE: usize = 2_000;
/// Files written per transaction, so a reader never waits long.
const BATCH: usize = 50;

const SCHEMA: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    CREATE TABLE IF NOT EXISTS files (
        id    INTEGER PRIMARY KEY,
        path  TEXT NOT NULL UNIQUE,
        mtime INTEGER NOT NULL,
        size  INTEGER NOT NULL
    ) STRICT;
    CREATE TABLE IF NOT EXISTS symbols (
        file INTEGER NOT NULL,
        name TEXT NOT NULL COLLATE NOCASE,
        kind TEXT NOT NULL,
        line INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX IF NOT EXISTS symbols_name ON symbols (name);
    CREATE INDEX IF NOT EXISTS symbols_file ON symbols (file);
    CREATE TABLE IF NOT EXISTS words (
        file  INTEGER NOT NULL,
        word  TEXT NOT NULL COLLATE NOCASE,
        count INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX IF NOT EXISTS words_word ON words (word);
    CREATE INDEX IF NOT EXISTS words_file ON words (file);
";

/// Where the index for `root` lives.
pub fn db_path(root: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    // FNV-1a over the path: stable, and no crate for a file name.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in root.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let name = root
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    Some(
        PathBuf::from(home)
            .join("Library/Caches/crc/index")
            .join(format!("{name}-{hash:016x}.db")),
    )
}

/// What one pass over the project did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Synced {
    pub read: usize,
    pub removed: usize,
    pub unchanged: usize,
}

/// The writer's connection.
pub struct Store {
    db: Db,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let db = Db::open(path)?;
        db.execute(SCHEMA)?;
        Ok(Store { db })
    }

    /// Brings the index in line with `files` under `root`: reads what is new
    /// or changed since the last pass, forgets what is gone.
    pub fn sync(&self, root: &Path, files: &[PathBuf]) -> Result<Synced> {
        let mut known: HashMap<String, (i64, i64, i64)> = HashMap::new();
        {
            let mut statement = self.db.prepare("SELECT id, path, mtime, size FROM files")?;
            statement.bind(&[])?;
            while statement.step()? {
                known.insert(
                    statement.text(1),
                    (statement.int(0), statement.int(2), statement.int(3)),
                );
            }
        }
        let mut synced = Synced::default();
        let mut pending: Vec<(String, i64, i64, Option<i64>, String)> = Vec::new();
        for file in files.iter().take(MAX_FILES) {
            let Ok(relative) = file.strip_prefix(root) else {
                continue;
            };
            let relative = relative.to_string_lossy().into_owned();
            let Ok(meta) = std::fs::metadata(file) else {
                continue;
            };
            if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
                continue;
            }
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos() as i64);
            let size = meta.len() as i64;
            let old = known.remove(&relative);
            if old.is_some_and(|(_, m, s)| m == mtime && s == size) {
                synced.unchanged += 1;
                continue;
            }
            // Not text: indexed as present, with nothing in it, so it is not
            // read again until it changes.
            let text = std::fs::read(file)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .unwrap_or_default();
            pending.push((relative, mtime, size, old.map(|(id, _, _)| id), text));
            if pending.len() >= BATCH {
                synced.read += pending.len();
                self.write(std::mem::take(&mut pending))?;
            }
        }
        synced.read += pending.len();
        self.write(pending)?;
        let gone: Vec<i64> = known.into_values().map(|(id, _, _)| id).collect();
        synced.removed = gone.len();
        self.db.transaction(|| {
            for id in &gone {
                self.forget_file(*id)?;
                let mut statement = self.db.prepare("DELETE FROM files WHERE id = ?1")?;
                statement.run(&[Value::Int(*id)])?;
            }
            Ok(())
        })?;
        Ok(synced)
    }

    fn forget_file(&self, id: i64) -> Result<()> {
        for sql in [
            "DELETE FROM symbols WHERE file = ?1",
            "DELETE FROM words WHERE file = ?1",
        ] {
            let mut statement = self.db.prepare(sql)?;
            statement.run(&[Value::Int(id)])?;
        }
        Ok(())
    }

    fn write(&self, batch: Vec<(String, i64, i64, Option<i64>, String)>) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        self.db.transaction(|| {
            for (relative, mtime, size, old, text) in &batch {
                let id = match old {
                    Some(id) => {
                        self.forget_file(*id)?;
                        let mut statement = self
                            .db
                            .prepare("UPDATE files SET mtime = ?2, size = ?3 WHERE id = ?1")?;
                        statement.run(&[Value::Int(*id), Value::Int(*mtime), Value::Int(*size)])?;
                        *id
                    }
                    None => {
                        let mut statement = self
                            .db
                            .prepare("INSERT INTO files (path, mtime, size) VALUES (?1, ?2, ?3)")?;
                        statement.run(&[
                            Value::Text(relative),
                            Value::Int(*mtime),
                            Value::Int(*size),
                        ])?;
                        self.db.last_insert_rowid()
                    }
                };
                if text.is_empty() {
                    continue;
                }
                if let Some(language) = crate::syntax::Language::from_path(Path::new(relative)) {
                    let mut statement = self.db.prepare(
                        "INSERT INTO symbols (file, name, kind, line) VALUES (?1, ?2, ?3, ?4)",
                    )?;
                    for definition in crate::syntax::defs::definitions(language, text) {
                        statement.run(&[
                            Value::Int(id),
                            Value::Text(&definition.name),
                            Value::Text(definition.kind),
                            Value::Int(i64::from(definition.line)),
                        ])?;
                    }
                }
                let mut statement = self
                    .db
                    .prepare("INSERT INTO words (file, word, count) VALUES (?1, ?2, ?3)")?;
                for (word, count) in crate::complete::words(text, "", None)
                    .into_iter()
                    .take(WORDS_PER_FILE)
                {
                    statement.run(&[
                        Value::Int(id),
                        Value::Text(&word),
                        Value::Int(count as i64),
                    ])?;
                }
            }
            Ok(())
        })
    }
}

/// A symbol found in the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub line: u32,
}

/// A reader's connection: the completion worker's.
pub struct Reader {
    db: Db,
}

impl Reader {
    /// `None` until the indexer has created the file.
    pub fn open(path: &Path) -> Option<Reader> {
        if !path.exists() {
            return None;
        }
        Db::open(path).ok().map(|db| Reader { db })
    }

    /// Definitions whose name starts with `prefix`, shortest names first.
    pub fn symbols(&self, prefix: &str, limit: usize) -> Result<Vec<Symbol>> {
        let like = format!("{}%", crate::complete::history::escape_like(prefix));
        let mut statement = self.db.prepare(
            "SELECT name, kind, path, line FROM symbols JOIN files ON files.id = symbols.file
             WHERE name LIKE ?1 ESCAPE '\\' ORDER BY length(name), name LIMIT ?2",
        )?;
        statement.bind(&[Value::Text(&like), Value::Int(limit as i64)])?;
        let mut out = Vec::new();
        while statement.step()? {
            out.push(Symbol {
                name: statement.text(0),
                kind: statement.text(1),
                path: statement.text(2),
                line: statement.int(3) as u32,
            });
        }
        Ok(out)
    }

    /// Definitions whose name holds the characters of `needle` in order, for
    /// go to symbol. SQLite narrows by a `LIKE 'a%b%c%'` pattern; the caller
    /// ranks what comes back.
    pub fn containing(&self, needle: &str, limit: usize) -> Result<Vec<Symbol>> {
        let mut like = String::from("%");
        for c in needle.chars() {
            like.push_str(&crate::complete::history::escape_like(&c.to_string()));
            like.push('%');
        }
        let mut statement = self.db.prepare(
            "SELECT name, kind, path, line FROM symbols JOIN files ON files.id = symbols.file
             WHERE name LIKE ?1 ESCAPE '\\' ORDER BY length(name), name LIMIT ?2",
        )?;
        statement.bind(&[Value::Text(&like), Value::Int(limit as i64)])?;
        let mut out = Vec::new();
        while statement.step()? {
            out.push(Symbol {
                name: statement.text(0),
                kind: statement.text(1),
                path: statement.text(2),
                line: statement.int(3) as u32,
            });
        }
        Ok(out)
    }

    /// Words starting with `prefix`: the word, how often it is used across
    /// the project, and in how many files.
    pub fn words(&self, prefix: &str, limit: usize) -> Result<Vec<(String, u32, u32)>> {
        let like = format!("{}%", crate::complete::history::escape_like(prefix));
        let mut statement = self.db.prepare(
            "SELECT word, SUM(count), COUNT(*) FROM words WHERE word LIKE ?1 ESCAPE '\\'
             GROUP BY word ORDER BY SUM(count) DESC LIMIT ?2",
        )?;
        statement.bind(&[Value::Text(&like), Value::Int(limit as i64)])?;
        let mut out = Vec::new();
        while statement.step()? {
            out.push((
                statement.text(0),
                statement.int(1) as u32,
                statement.int(2) as u32,
            ));
        }
        Ok(out)
    }
}

/// The writer thread for one project. Asked to look again with [`Indexer::poke`];
/// stops when dropped.
pub struct Indexer {
    tx: mpsc::Sender<()>,
    pub root: PathBuf,
}

impl Indexer {
    pub fn start(root: PathBuf) -> Option<Indexer> {
        let path = db_path(&root)?;
        let (tx, rx) = mpsc::channel::<()>();
        let thread_root = root.clone();
        std::thread::Builder::new()
            .name("crc-index".into())
            .spawn(move || {
                let Ok(store) = Store::open(&path) else {
                    return;
                };
                loop {
                    let files = project_files(&thread_root);
                    let _ = store.sync(&thread_root, &files);
                    // Wait for a poke; then let a burst of them settle.
                    if rx.recv().is_err() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(300));
                    while rx.try_recv().is_ok() {}
                }
            })
            .ok()?;
        Some(Indexer { tx, root })
    }

    /// Something in the project changed; index again soon.
    pub fn poke(&self) {
        let _ = self.tx.send(());
    }
}

/// The files to index: what Git tracks or would, or every file under the
/// root the Explorer's rules allow when it is not a repository.
fn project_files(root: &Path) -> Vec<PathBuf> {
    if let Ok(files) = crate::project::git::project_files(root) {
        return files;
    }
    let mut finder = crate::project::finder::Finder::new();
    finder.scan(root);
    (0..finder.len())
        .filter_map(|i| finder.entry(i).map(|e| e.path.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_changes_and_forgets_deleted_files() {
        let root = std::env::temp_dir().join(format!("crc-index-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/tree.rs"),
            "pub struct Tree;\nfn reveal() { reveal(); reveal(); }\n",
        )
        .unwrap();
        std::fs::write(root.join("notes.txt"), "revealing things\n").unwrap();
        std::fs::write(root.join("blob.bin"), [0xff, 0xfe, 0x00]).unwrap();
        let store = Store::open(&root.join(".index.db")).unwrap();
        let files = |names: &[&str]| names.iter().map(|n| root.join(n)).collect::<Vec<_>>();

        let first = store
            .sync(&root, &files(&["src/tree.rs", "notes.txt", "blob.bin"]))
            .unwrap();
        assert_eq!(
            first,
            Synced {
                read: 3,
                removed: 0,
                unchanged: 0
            }
        );
        let reader = Reader::open(&root.join(".index.db")).unwrap();
        assert_eq!(
            reader.symbols("rev", 10).unwrap(),
            [Symbol {
                name: "reveal".into(),
                kind: "function".into(),
                path: "src/tree.rs".into(),
                line: 1
            }]
        );
        assert_eq!(
            reader.symbols("TR", 10).unwrap()[0].name,
            "Tree",
            "case-insensitive"
        );
        assert_eq!(
            reader.words("reveal", 10).unwrap(),
            [("reveal".into(), 3, 1), ("revealing".into(), 1, 1)]
        );

        let again = store
            .sync(&root, &files(&["src/tree.rs", "notes.txt", "blob.bin"]))
            .unwrap();
        assert_eq!(
            again,
            Synced {
                read: 0,
                removed: 0,
                unchanged: 3
            },
            "nothing re-read"
        );

        std::thread::sleep(Duration::from_millis(10));
        std::fs::write(root.join("src/tree.rs"), "fn hide() {}\n").unwrap();
        let changed = store.sync(&root, &files(&["src/tree.rs"])).unwrap();
        assert_eq!(
            changed,
            Synced {
                read: 1,
                removed: 2,
                unchanged: 0
            }
        );
        assert!(reader.symbols("rev", 10).unwrap().is_empty());
        assert_eq!(reader.symbols("hi", 10).unwrap()[0].name, "hide");
        assert!(
            reader.words("revealing", 10).unwrap().is_empty(),
            "notes.txt is gone"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn each_project_has_its_own_file_outside_it() {
        let a = db_path(Path::new("/work/a")).unwrap();
        assert_ne!(a, db_path(Path::new("/work/b")).unwrap());
        assert!(a.to_string_lossy().contains("Library/Caches/crc/index/a-"));
    }
}
