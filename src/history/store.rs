//! The local history database: every kept version of every file, one
//! SQLite file for the whole Mac.
//!
//! It lives in `~/Library/Application Support/crc/history.db`, not in
//! Caches with the project index: the index can be rebuilt from the
//! project, this cannot, and it holds every past version of a file,
//! secrets included. So the folder is 0700, the file 0600, a corrupt file
//! is set aside rather than deleted, and a file from a newer crc is opened
//! read-only rather than migrated backwards.
//!
//! Versions are content-addressed: a blob is stored once per SHA-256 of
//! its text, LZFSE-compressed, and snapshots point at blobs. Reading a
//! version checks the hash again, so a damaged blob reads as missing
//! instead of as some other text.
//!
//! Written by one history thread per crc, but several crc instances may
//! share the file: WAL, a busy timeout, and every write in one
//! `BEGIN IMMEDIATE` transaction.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::compress::{self, Codec};
use crate::index::db::{self, Db, Value};

/// The layout `MIGRATIONS` builds, kept in `PRAGMA user_version`.
/// Migrations only ever add: a file from an older crc is brought forward,
/// one from a newer crc is opened read-only. A new `Reason` raises it too,
/// so an older crc never reads a reason it cannot name.
const SCHEMA_VERSION: i64 = 1;

/// `MIGRATIONS[n]` takes a file from version `n` to `n + 1`.
const MIGRATIONS: [&str; SCHEMA_VERSION as usize] = ["
    CREATE TABLE files (
        id   INTEGER PRIMARY KEY,
        path TEXT NOT NULL UNIQUE
    ) STRICT;
    CREATE TABLE blobs (
        id    INTEGER PRIMARY KEY,
        hash  BLOB NOT NULL UNIQUE,
        codec INTEGER NOT NULL,
        size  INTEGER NOT NULL,
        data  BLOB NOT NULL
    ) STRICT;
    CREATE TABLE snapshots (
        id     INTEGER PRIMARY KEY,
        file   INTEGER NOT NULL,
        blob   INTEGER NOT NULL,
        at     INTEGER NOT NULL,
        reason INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX snapshots_file ON snapshots (file, id);
    CREATE INDEX snapshots_blob ON snapshots (blob);
    CREATE TABLE undo (
        file  INTEGER PRIMARY KEY,
        base  INTEGER NOT NULL,
        at    INTEGER NOT NULL,
        codec INTEGER NOT NULL,
        size  INTEGER NOT NULL,
        data  BLOB NOT NULL
    ) STRICT;
"];

/// Pages returned to the file system per incremental vacuum step, so one
/// step never holds the write lock for long.
const VACUUM_SLICE: i64 = 512;

/// How long a reader's query waits on another connection's lock, in
/// milliseconds; a writer waits the default five seconds.
const READ_BUSY_MS: i32 = 200;

const HOUR_MS: i64 = 60 * 60 * 1000;
const DAY_MS: i64 = 24 * HOUR_MS;

// Primary result codes, read off the extended code an error carries.
const SQLITE_CORRUPT: i32 = 11;
const SQLITE_FULL: i32 = 13;
const SQLITE_TOOBIG: i32 = 18;
const SQLITE_NOTADB: i32 = 26;

#[derive(Debug)]
pub enum Error {
    /// The disk, or the database's page limit, is full. The caller prunes
    /// once, retries once, then stops recording for the session.
    Full(db::Error),
    /// The file was written by a newer crc and is open read-only.
    ReadOnly,
    /// The file failed `PRAGMA quick_check`.
    Corrupt(String),
    /// A path that is not UTF-8, which macOS file systems do not produce.
    NotUtf8,
    Io(std::io::Error),
    Sqlite(db::Error),
}

impl Error {
    fn is_corrupt(&self) -> bool {
        match self {
            Error::Corrupt(_) => true,
            Error::Sqlite(e) => matches!(e.code & 0xff, SQLITE_CORRUPT | SQLITE_NOTADB),
            _ => false,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Full(e) => write!(f, "local history is full: {e}"),
            Error::ReadOnly => {
                write!(
                    f,
                    "local history was written by a newer crc and is read-only"
                )
            }
            Error::Corrupt(detail) => write!(f, "local history is damaged: {detail}"),
            Error::NotUtf8 => write!(f, "the path is not UTF-8"),
            Error::Io(e) => write!(f, "local history: {e}"),
            Error::Sqlite(e) => write!(f, "local history: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<db::Error> for Error {
    fn from(e: db::Error) -> Self {
        if e.code & 0xff == SQLITE_FULL {
            Error::Full(e)
        } else {
            Error::Sqlite(e)
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Why a snapshot was taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Opened,
    Saved,
    Idle,
    Reloaded,
    ClosedUnsaved,
    Quit,
    BeforeRestore,
    EditedOnDisk,
}

impl Reason {
    pub const ALL: [Reason; 8] = [
        Reason::Opened,
        Reason::Saved,
        Reason::Idle,
        Reason::Reloaded,
        Reason::ClosedUnsaved,
        Reason::Quit,
        Reason::BeforeRestore,
        Reason::EditedOnDisk,
    ];

    /// The value written to the database. Never renumber.
    pub fn id(self) -> i64 {
        match self {
            Reason::Opened => 1,
            Reason::Saved => 2,
            Reason::Idle => 3,
            Reason::Reloaded => 4,
            Reason::ClosedUnsaved => 5,
            Reason::Quit => 6,
            Reason::BeforeRestore => 7,
            Reason::EditedOnDisk => 8,
        }
    }

    pub fn from_id(id: i64) -> Option<Reason> {
        Reason::ALL.into_iter().find(|r| r.id() == id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SnapshotId(pub i64);

/// One version in a file's timeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: SnapshotId,
    /// Milliseconds since 1970.
    pub at: i64,
    pub reason: Reason,
    /// The text's length in bytes, uncompressed.
    pub size: usize,
}

/// A file's saved undo journal and the text it applies to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Undo {
    /// SHA-256 of the text the journal was recorded against.
    pub base: [u8; 32],
    pub at: i64,
    pub journal: Vec<u8>,
}

/// How much history `prune` keeps. Ages in milliseconds.
#[derive(Clone, Copy, Debug)]
pub struct Caps {
    /// Everything younger than this is kept.
    pub all: i64,
    /// Then one per hour up to this age,
    pub hourly: i64,
    /// then one per day up to this age, and nothing older.
    pub daily: i64,
    pub per_file: usize,
    /// Stored (compressed) bytes across every file.
    pub total_bytes: u64,
}

impl Default for Caps {
    fn default() -> Self {
        Caps {
            all: DAY_MS,
            hourly: 7 * DAY_MS,
            daily: 30 * DAY_MS,
            per_file: 300,
            total_bytes: 256 << 20,
        }
    }
}

/// What one `prune` removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    pub snapshots: usize,
    pub blobs: usize,
}

/// Where the history lives: `CRC_HISTORY_DB` when set, for tests and
/// self-tests, else Application Support.
pub fn default_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CRC_HISTORY_DB") {
        return Some(PathBuf::from(path));
    }
    Some(crate::platform::app_support()?.join("history.db"))
}

pub struct Store {
    db: Db,
    read_only: bool,
    quarantined: Option<PathBuf>,
}

impl Store {
    /// Opens or creates the history at `path`. A file that is not a
    /// database or fails its check is renamed aside and a new one started;
    /// `take_quarantined` says where it went.
    pub fn open(path: &Path) -> Result<Store> {
        match Store::open_once(path) {
            Err(e) if e.is_corrupt() => {
                let aside = quarantine(path)?;
                let mut store = Store::open_once(path)?;
                store.quarantined = Some(aside);
                Ok(store)
            }
            other => other,
        }
    }

    fn open_once(path: &Path) -> Result<Store> {
        private_files(path)?;
        let db = Db::open(path)?;
        let version = query_int(&db, "PRAGMA user_version", &[])?.unwrap_or(0);
        if version > SCHEMA_VERSION {
            drop(db);
            return Ok(Store {
                db: Db::open_readonly(path)?,
                read_only: true,
                quarantined: None,
            });
        }
        let check = {
            let mut statement = db.prepare("PRAGMA quick_check")?;
            if statement.step()? {
                statement.text(0)
            } else {
                String::new()
            }
        };
        if check != "ok" {
            return Err(Error::Corrupt(check));
        }
        // auto_vacuum only takes on a file with no tables yet, so it goes
        // first; on an existing file it is a no-op.
        db.execute(
            "PRAGMA auto_vacuum = INCREMENTAL;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA journal_size_limit = 8388608;",
        )?;
        if version < SCHEMA_VERSION {
            db.transaction(|| {
                // Read again under the write lock: another crc may have
                // migrated it since.
                let version = query_int(&db, "PRAGMA user_version", &[])?.unwrap_or(0);
                for migration in &MIGRATIONS[version.clamp(0, SCHEMA_VERSION) as usize..] {
                    db.execute(migration)?;
                }
                db.execute(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
            })?;
        }
        Ok(Store {
            db,
            read_only: false,
            quarantined: None,
        })
    }

    /// Opens the history at `path` only to read it, as File History does on
    /// its own thread while the history thread writes: no check, no
    /// migration, nothing created. `None` when there is no file yet. A
    /// lock another crc holds fails a read after [`READ_BUSY_MS`], so a
    /// reader is never kept waiting on a writer.
    pub fn open_to_read(path: &Path) -> Result<Option<Store>> {
        if !path.exists() {
            return Ok(None);
        }
        let db = Db::open_readonly(path)?;
        db.set_busy_timeout(READ_BUSY_MS);
        Ok(Some(Store {
            db,
            read_only: true,
            quarantined: None,
        }))
    }

    /// Where a damaged file was moved on open, once.
    pub fn take_quarantined(&mut self) -> Option<PathBuf> {
        self.quarantined.take()
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    fn writable(&self) -> Result<()> {
        off_main_thread();
        if self.read_only {
            Err(Error::ReadOnly)
        } else {
            Ok(())
        }
    }

    /// Records `text` (the buffer's text, LF line endings) as a version of
    /// `path`. `None` when it equals the file's latest version.
    pub fn put_snapshot(
        &self,
        path: &Path,
        text: &str,
        reason: Reason,
        at: i64,
    ) -> Result<Option<SnapshotId>> {
        self.writable()?;
        let key = key(path)?;
        let hash = digest(text.as_bytes())?;
        let db = &self.db;
        Ok(db.transaction(|| {
            let file = file_id(db, key)?;
            let blob = query_int(
                db,
                "SELECT id FROM blobs WHERE hash = ?1",
                &[hash.as_slice().into()],
            )?;
            if let (Some(file), Some(blob)) = (file, blob) {
                let latest = query_int(
                    db,
                    "SELECT blob FROM snapshots WHERE file = ?1 ORDER BY id DESC LIMIT 1",
                    &[file.into()],
                )?;
                if latest == Some(blob) {
                    return Ok(None);
                }
            }
            let file = match file {
                Some(file) => file,
                None => insert_file(db, key)?,
            };
            let blob = match blob {
                Some(blob) => blob,
                None => insert_blob(db, &hash, text.as_bytes())?,
            };
            db.prepare("INSERT INTO snapshots (file, blob, at, reason) VALUES (?1, ?2, ?3, ?4)")?
                .run(&[file.into(), blob.into(), at.into(), reason.id().into()])?;
            Ok(Some(SnapshotId(db.last_insert_rowid())))
        })?)
    }

    /// `path`'s versions, newest first.
    pub fn list(&self, path: &Path) -> Result<Vec<Entry>> {
        let key = key(path)?;
        let mut statement = self.db.prepare(
            "SELECT s.id, s.at, s.reason, b.size FROM snapshots s
             JOIN files f ON f.id = s.file
             JOIN blobs b ON b.id = s.blob
             WHERE f.path = ?1 ORDER BY s.id DESC",
        )?;
        statement.bind(&[key.into()])?;
        let mut entries = Vec::new();
        while statement.step()? {
            let Some(reason) = Reason::from_id(statement.int(2)) else {
                continue;
            };
            entries.push(Entry {
                id: SnapshotId(statement.int(0)),
                at: statement.int(1),
                reason,
                size: usize::try_from(statement.int(3)).unwrap_or(0),
            });
        }
        Ok(entries)
    }

    /// A version's text. `None` when it is gone, or its blob no longer
    /// decodes to the text it was stored as.
    pub fn read(&self, id: SnapshotId) -> Result<Option<String>> {
        let mut statement = self.db.prepare(
            "SELECT b.hash, b.codec, b.size, b.data FROM snapshots s
             JOIN blobs b ON b.id = s.blob WHERE s.id = ?1",
        )?;
        statement.bind(&[id.0.into()])?;
        if !statement.step()? {
            return Ok(None);
        }
        let hash = statement.blob(0);
        let bytes = unpack(statement.int(1), statement.int(2), &statement.blob(3));
        Ok(bytes
            .filter(|bytes| crate::ext::verify::sha256(bytes).is_some_and(|h| h[..] == hash[..]))
            .and_then(|bytes| String::from_utf8(bytes).ok()))
    }

    /// Moves `old`'s timeline to `new`, as when the file is renamed. When
    /// `new` has versions of its own, both are kept, in the order they were
    /// taken; `old`'s undo journal wins, as it belongs to the text now at
    /// `new`.
    pub fn rename(&self, old: &Path, new: &Path) -> Result<()> {
        self.writable()?;
        let (old, new) = (key(old)?, key(new)?);
        if old == new {
            return Ok(());
        }
        let db = &self.db;
        Ok(db.transaction(|| {
            move_file(db, old, new)?;
            // `new`'s replaced journal may have held the last reference to
            // its base.
            collect_garbage(db).map(|_| ())
        })?)
    }

    /// Moves the timeline of every file under the folder `old` to the same
    /// place under `new`, as when the folder is renamed, each as `rename`
    /// would. Only what is inside the folder: `/a/b` does not take
    /// `/a/bc` along.
    pub fn rename_folder(&self, old: &Path, new: &Path) -> Result<()> {
        self.writable()?;
        let (old, new) = (key(old)?, key(new)?);
        let (old, new) = (old.trim_end_matches('/'), new.trim_end_matches('/'));
        if old == new {
            return Ok(());
        }
        let prefix = format!("{old}/");
        let db = &self.db;
        Ok(db.transaction(|| {
            let mut inside = Vec::new();
            {
                // A byte compare, not LIKE: no wildcards to escape, and
                // no case folding.
                let mut statement =
                    db.prepare("SELECT path FROM files WHERE substr(path, 1, length(?1)) = ?1")?;
                statement.bind(&[prefix.as_str().into()])?;
                while statement.step()? {
                    inside.push(statement.text(0));
                }
            }
            for path in &inside {
                let moved = format!("{new}/{}", &path[prefix.len()..]);
                move_file(db, path, &moved)?;
            }
            collect_garbage(db).map(|_| ())
        })?)
    }

    /// Drops everything kept for `path`.
    pub fn forget(&self, path: &Path) -> Result<()> {
        self.writable()?;
        let key = key(path)?;
        let db = &self.db;
        db.transaction(|| {
            let Some(file) = file_id(db, key)? else {
                return Ok(());
            };
            for sql in [
                "DELETE FROM snapshots WHERE file = ?1",
                "DELETE FROM undo WHERE file = ?1",
                "DELETE FROM files WHERE id = ?1",
            ] {
                db.prepare(sql)?.run(&[file.into()])?;
            }
            collect_garbage(db).map(|_| ())
        })?;
        self.vacuum()
    }

    /// Drops all history, for every file.
    pub fn forget_all(&self) -> Result<()> {
        self.writable()?;
        let db = &self.db;
        db.transaction(|| {
            db.execute(
                "DELETE FROM snapshots; DELETE FROM undo; DELETE FROM files; DELETE FROM blobs;",
            )
        })?;
        self.vacuum()
    }

    /// Paths with history that `exists` says are gone, most recently
    /// recorded first.
    pub fn deleted_paths(&self, exists: impl Fn(&Path) -> bool) -> Result<Vec<PathBuf>> {
        let mut statement = self.db.prepare(
            "SELECT f.path FROM files f JOIN snapshots s ON s.file = f.id
             GROUP BY f.id ORDER BY max(s.id) DESC",
        )?;
        statement.bind(&[])?;
        let mut paths = Vec::new();
        while statement.step()? {
            let path = PathBuf::from(statement.text(0));
            if !exists(&path) {
                paths.push(path);
            }
        }
        Ok(paths)
    }

    /// Keeps `journal`, opaque here, as `path`'s undo history against the
    /// text `base`, replacing any it had.
    pub fn put_undo(&self, path: &Path, base: &str, journal: &[u8], at: i64) -> Result<()> {
        self.writable()?;
        let key = key(path)?;
        let hash = digest(base.as_bytes())?;
        let db = &self.db;
        Ok(db.transaction(|| {
            let file = match file_id(db, key)? {
                Some(file) => file,
                None => insert_file(db, key)?,
            };
            let base = match query_int(
                db,
                "SELECT id FROM blobs WHERE hash = ?1",
                &[hash.as_slice().into()],
            )? {
                Some(blob) => blob,
                None => insert_blob(db, &hash, base.as_bytes())?,
            };
            let (codec, data) = compress::encode(journal);
            db.prepare(
                "INSERT OR REPLACE INTO undo (file, base, at, codec, size, data)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .run(&[
                file.into(),
                base.into(),
                at.into(),
                codec.id().into(),
                journal.len().into(),
                data.as_slice().into(),
            ])?;
            // The base blob it replaced may now be unreferenced.
            collect_garbage(db).map(|_| ())
        })?)
    }

    /// `path`'s undo journal, `None` when there is none or it is damaged.
    pub fn get_undo(&self, path: &Path) -> Result<Option<Undo>> {
        let key = key(path)?;
        let mut statement = self.db.prepare(
            "SELECT b.hash, u.at, u.codec, u.size, u.data FROM undo u
             JOIN files f ON f.id = u.file
             JOIN blobs b ON b.id = u.base
             WHERE f.path = ?1",
        )?;
        statement.bind(&[key.into()])?;
        if !statement.step()? {
            return Ok(None);
        }
        let Ok(base) = <[u8; 32]>::try_from(statement.blob(0).as_slice()) else {
            return Ok(None);
        };
        let at = statement.int(1);
        Ok(
            unpack(statement.int(2), statement.int(3), &statement.blob(4)).map(|journal| Undo {
                base,
                at,
                journal,
            }),
        )
    }

    /// Thins the history to `caps` as of `now` (milliseconds), drops blobs
    /// nothing points at any more, then gives the freed pages back to the
    /// file system.
    pub fn prune(&self, now: i64, caps: &Caps) -> Result<Pruned> {
        self.writable()?;
        let db = &self.db;
        let pruned = db.transaction(|| {
            let mut rows = Vec::new();
            {
                let mut statement = db.prepare("SELECT id, file, blob, at FROM snapshots")?;
                statement.bind(&[])?;
                while statement.step()? {
                    rows.push(Row {
                        id: statement.int(0),
                        file: statement.int(1),
                        blob: statement.int(2),
                        at: statement.int(3),
                    });
                }
            }
            let mut blob_bytes = HashMap::new();
            {
                let mut statement = db.prepare("SELECT id, length(data) FROM blobs")?;
                statement.bind(&[])?;
                while statement.step()? {
                    blob_bytes.insert(statement.int(0), statement.int(1).max(0) as u64);
                }
            }
            let mut undo = Vec::new();
            {
                let mut statement = db.prepare("SELECT base, length(data) FROM undo")?;
                statement.bind(&[])?;
                while statement.step()? {
                    undo.push((statement.int(0), statement.int(1).max(0) as u64));
                }
            }
            let doomed = doomed(&rows, &blob_bytes, &undo, now, caps);
            let mut delete = db.prepare("DELETE FROM snapshots WHERE id = ?1")?;
            for id in &doomed {
                delete.run(&[(*id).into()])?;
            }
            Ok(Pruned {
                snapshots: doomed.len(),
                blobs: collect_garbage(db)?,
            })
        })?;
        self.vacuum()?;
        Ok(pruned)
    }

    /// Hands free pages back in slices, each its own short transaction.
    fn vacuum(&self) -> Result<()> {
        let mut last = i64::MAX;
        loop {
            let free = query_int(&self.db, "PRAGMA freelist_count", &[])?.unwrap_or(0);
            if free == 0 || free >= last {
                return Ok(());
            }
            last = free;
            self.db
                .execute(&format!("PRAGMA incremental_vacuum({VACUUM_SLICE})"))?;
        }
    }
}

struct Row {
    id: i64,
    file: i64,
    blob: i64,
    at: i64,
}

/// The snapshots `caps` drops: by age tier and per-file count first, then
/// the oldest left until the stored bytes fit.
fn doomed(
    rows: &[Row],
    blob_bytes: &HashMap<i64, u64>,
    undo: &[(i64, u64)],
    now: i64,
    caps: &Caps,
) -> Vec<i64> {
    let mut by_file: HashMap<i64, Vec<&Row>> = HashMap::new();
    for row in rows {
        by_file.entry(row.file).or_default().push(row);
    }
    let mut doomed = HashSet::new();
    let mut kept = Vec::new();
    for timeline in by_file.values_mut() {
        timeline.sort_by_key(|row| std::cmp::Reverse((row.at, row.id)));
        // Buckets are absolute hours and days, not ages, so a version kept
        // by one prune is the one kept by the next.
        let mut seen = HashSet::new();
        let mut count = 0;
        for row in timeline.iter() {
            let age = now - row.at;
            let keep = if age < caps.all {
                true
            } else if age < caps.hourly {
                seen.insert((0, row.at.div_euclid(HOUR_MS)))
            } else if age < caps.daily {
                seen.insert((1, row.at.div_euclid(DAY_MS)))
            } else {
                false
            };
            if keep && count < caps.per_file {
                count += 1;
                kept.push(*row);
            } else {
                doomed.insert(row.id);
            }
        }
    }

    // What the file will hold once unreferenced blobs are gone. A blob an
    // undo journal is based on is never freed here.
    let mut refs: HashMap<i64, usize> = HashMap::new();
    for row in &kept {
        *refs.entry(row.blob).or_default() += 1;
    }
    let mut total: u64 = undo.iter().map(|(_, bytes)| bytes).sum();
    for (base, _) in undo {
        *refs.entry(*base).or_default() += 1;
    }
    total += refs
        .keys()
        .map(|blob| blob_bytes.get(blob).copied().unwrap_or(0))
        .sum::<u64>();
    kept.sort_by_key(|row| (row.at, row.id));
    for row in kept {
        if total <= caps.total_bytes {
            break;
        }
        doomed.insert(row.id);
        let left = refs.entry(row.blob).or_default();
        *left = left.saturating_sub(1);
        if *left == 0 {
            total = total.saturating_sub(blob_bytes.get(&row.blob).copied().unwrap_or(0));
        }
    }
    let mut doomed: Vec<i64> = doomed.into_iter().collect();
    doomed.sort_unstable();
    doomed
}

/// Deletes blobs no snapshot or undo journal points at, and files left
/// with neither. Returns how many blobs went.
fn collect_garbage(db: &Db) -> db::Result<usize> {
    db.execute(
        "DELETE FROM blobs WHERE NOT EXISTS (SELECT 1 FROM snapshots WHERE blob = blobs.id)
         AND NOT EXISTS (SELECT 1 FROM undo WHERE base = blobs.id)",
    )?;
    let blobs = db.changes().max(0) as usize;
    db.execute(
        "DELETE FROM files WHERE NOT EXISTS (SELECT 1 FROM snapshots WHERE file = files.id)
         AND NOT EXISTS (SELECT 1 FROM undo WHERE file = files.id)",
    )?;
    Ok(blobs)
}

fn query_int(db: &Db, sql: &str, params: &[Value<'_>]) -> db::Result<Option<i64>> {
    let mut statement = db.prepare(sql)?;
    statement.bind(params)?;
    Ok(if statement.step()? {
        Some(statement.int(0))
    } else {
        None
    })
}

/// `old`'s timeline and undo journal to `new`, merged with what `new`
/// has; inside a transaction, garbage left for the caller.
fn move_file(db: &Db, old: &str, new: &str) -> db::Result<()> {
    let Some(from) = file_id(db, old)? else {
        return Ok(());
    };
    let Some(to) = file_id(db, new)? else {
        return db
            .prepare("UPDATE files SET path = ?2 WHERE id = ?1")?
            .run(&[from.into(), new.into()]);
    };
    db.prepare("UPDATE snapshots SET file = ?2 WHERE file = ?1")?
        .run(&[from.into(), to.into()])?;
    if query_int(db, "SELECT 1 FROM undo WHERE file = ?1", &[from.into()])?.is_some() {
        db.prepare("DELETE FROM undo WHERE file = ?1")?
            .run(&[to.into()])?;
        db.prepare("UPDATE undo SET file = ?2 WHERE file = ?1")?
            .run(&[from.into(), to.into()])?;
    }
    db.prepare("DELETE FROM files WHERE id = ?1")?
        .run(&[from.into()])?;
    Ok(())
}

fn file_id(db: &Db, key: &str) -> db::Result<Option<i64>> {
    query_int(db, "SELECT id FROM files WHERE path = ?1", &[key.into()])
}

fn insert_file(db: &Db, key: &str) -> db::Result<i64> {
    db.prepare("INSERT INTO files (path) VALUES (?1)")?
        .run(&[key.into()])?;
    Ok(db.last_insert_rowid())
}

fn insert_blob(db: &Db, hash: &[u8; 32], bytes: &[u8]) -> db::Result<i64> {
    let (codec, data) = compress::encode(bytes);
    db.prepare("INSERT INTO blobs (hash, codec, size, data) VALUES (?1, ?2, ?3, ?4)")?
        .run(&[
            hash.as_slice().into(),
            codec.id().into(),
            bytes.len().into(),
            data.as_slice().into(),
        ])?;
    Ok(db.last_insert_rowid())
}

/// The bytes a stored row holds, `None` when they do not decode.
fn unpack(codec: i64, size: i64, data: &[u8]) -> Option<Vec<u8>> {
    compress::decode(Codec::from_id(codec)?, data, usize::try_from(size).ok()?)
}

fn key(path: &Path) -> Result<&str> {
    path.to_str().ok_or(Error::NotUtf8)
}

fn digest(bytes: &[u8]) -> Result<[u8; 32]> {
    crate::ext::verify::sha256(bytes).ok_or_else(|| {
        Error::Sqlite(db::Error {
            code: SQLITE_TOOBIG,
            message: format!("{} bytes is too big to hash", bytes.len()),
        })
    })
}

/// Creates the folder 0700 when missing, and the file 0600. Application
/// Support's crc folder predates this file, so it is tightened too; any
/// other folder a test points at is left as it was.
fn private_files(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        if !dir.exists() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        } else if crate::platform::app_support().as_deref() == Some(dir) {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    // SQLite gives the -wal and -shm files the main file's mode.
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Renames `path` and its -wal and -shm to `<path>.corrupt-<unix ms>`.
/// Never deletes: it may be the only copy of something.
fn quarantine(path: &Path) -> Result<PathBuf> {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let mut aside = path.as_os_str().to_owned();
    aside.push(format!(".corrupt-{ms}"));
    for suffix in ["", "-wal", "-shm"] {
        let mut from = path.as_os_str().to_owned();
        from.push(suffix);
        let mut to = aside.clone();
        to.push(suffix);
        match std::fs::rename(&from, &to) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    Ok(PathBuf::from(aside))
}

unsafe extern "C" {
    fn pthread_main_np() -> std::ffi::c_int;
}

/// Writes belong on the history thread: hashing, compressing and SQLite
/// on the main thread is exactly what typing must never wait on.
fn off_main_thread() {
    // SAFETY: no arguments, no preconditions.
    debug_assert!(
        unsafe { pthread_main_np() } == 0,
        "the history store was written from the main thread"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// 08:40 UTC on some day: 40 minutes into an hour, 8 hours into a day.
    const NOW: i64 = 1_800_000_000_000 + 40 * 60_000;

    /// A fresh folder for one test; the database inside it is not created.
    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("crc-history-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("history.db")
    }

    fn cleanup(path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    fn count(store: &Store, sql: &str) -> i64 {
        query_int(&store.db, sql, &[]).unwrap().unwrap()
    }

    /// Printable ASCII no compressor shrinks much, from a fixed seed.
    fn noise(len: usize, seed: u64) -> String {
        let mut state = seed * 2 + 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (b'!' + (state >> 32) as u8 % 94) as char
            })
            .collect()
    }

    fn ids(entries: &[Entry]) -> Vec<i64> {
        entries.iter().map(|e| e.id.0).collect()
    }

    fn put(store: &Store, path: &str, text: &str, at: i64) -> SnapshotId {
        store
            .put_snapshot(Path::new(path), text, Reason::Idle, at)
            .unwrap()
            .expect("a new version")
    }

    #[test]
    fn reason_ids_are_stable() {
        let ids: Vec<i64> = Reason::ALL.iter().map(|r| r.id()).collect();
        assert_eq!(ids, [1, 2, 3, 4, 5, 6, 7, 8]);
        for reason in Reason::ALL {
            assert_eq!(Reason::from_id(reason.id()), Some(reason));
        }
        assert_eq!(Reason::from_id(0), None);
        assert_eq!(Reason::from_id(9), None);
    }

    #[test]
    fn opens_with_the_pragmas_and_schema() {
        let path = temp("pragmas");
        let store = Store::open(&path).unwrap();
        assert_eq!(count(&store, "PRAGMA auto_vacuum"), 2, "incremental");
        assert_eq!(count(&store, "PRAGMA user_version"), SCHEMA_VERSION);
        assert_eq!(count(&store, "PRAGMA synchronous"), 1, "normal");
        assert_eq!(count(&store, "PRAGMA journal_size_limit"), 8 << 20);
        let mut mode = store.db.prepare("PRAGMA journal_mode").unwrap();
        assert!(mode.step().unwrap());
        assert_eq!(mode.text(0), "wal");
        drop(mode);
        drop(store);
        // Reopening an up-to-date file migrates nothing and still works.
        let store = Store::open(&path).unwrap();
        assert!(!store.read_only());
        cleanup(&path);
    }

    #[test]
    fn the_folder_and_files_are_private() {
        let path = temp("permissions");
        let store = Store::open(&path).unwrap();
        put(&store, "/notes/plan.md", "first", NOW);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&path), 0o600);
        let wal = PathBuf::from(format!("{}-wal", path.display()));
        assert!(wal.exists(), "WAL should be in use");
        assert_eq!(mode(&wal), 0o600);
        drop(store);
        // A file left readable by others is tightened on open.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let _store = Store::open(&path).unwrap();
        assert_eq!(mode(&path), 0o600);
        cleanup(&path);
    }

    #[test]
    fn equal_texts_share_a_blob_and_repeats_are_skipped() {
        let path = temp("dedup");
        let store = Store::open(&path).unwrap();
        let file = Path::new("/notes/plan.md");
        let text = "one\ntwo\n".repeat(100);
        assert!(
            store
                .put_snapshot(file, &text, Reason::Opened, NOW)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            store
                .put_snapshot(file, &text, Reason::Saved, NOW + 1)
                .unwrap(),
            None,
            "the same text twice in a row is one version"
        );
        put(&store, "/notes/plan.md", "changed", NOW + 2);
        put(&store, "/notes/plan.md", &text, NOW + 3);
        put(&store, "/notes/copy.md", &text, NOW + 4);
        assert_eq!(count(&store, "SELECT count(*) FROM snapshots"), 4);
        assert_eq!(count(&store, "SELECT count(*) FROM blobs"), 2);
        assert_eq!(
            count(&store, "SELECT codec FROM blobs WHERE size > 100"),
            Codec::Lzfse.id()
        );
        cleanup(&path);
    }

    #[test]
    fn lists_newest_first_and_reads_back() {
        let path = temp("list");
        let store = Store::open(&path).unwrap();
        let file = Path::new("/notes/plan.md");
        let a = store
            .put_snapshot(file, "alpha", Reason::Opened, NOW)
            .unwrap()
            .unwrap();
        let b = store
            .put_snapshot(file, "beta\u{e9}", Reason::Saved, NOW + 5)
            .unwrap()
            .unwrap();
        put(&store, "/notes/other.md", "unrelated", NOW + 6);
        let entries = store.list(file).unwrap();
        assert_eq!(
            entries,
            [
                Entry {
                    id: b,
                    at: NOW + 5,
                    reason: Reason::Saved,
                    size: 6
                },
                Entry {
                    id: a,
                    at: NOW,
                    reason: Reason::Opened,
                    size: 5
                },
            ]
        );
        assert_eq!(store.read(a).unwrap().as_deref(), Some("alpha"));
        assert_eq!(store.read(b).unwrap().as_deref(), Some("beta\u{e9}"));
        assert_eq!(store.read(SnapshotId(9999)).unwrap(), None);
        assert!(store.list(Path::new("/notes/none.md")).unwrap().is_empty());
        cleanup(&path);
    }

    #[test]
    fn a_reader_sees_the_writers_versions_and_creates_nothing() {
        let path = temp("reader");
        assert!(Store::open_to_read(&path).unwrap().is_none());
        assert!(!path.exists(), "a reader creates no file");
        let store = Store::open(&path).unwrap();
        let a = put(&store, "/notes/plan.md", "alpha", NOW);
        // While the writer has it open, and what it writes after.
        let reader = Store::open_to_read(&path).unwrap().unwrap();
        assert!(reader.read_only());
        assert_eq!(
            ids(&reader.list(Path::new("/notes/plan.md")).unwrap()),
            [a.0]
        );
        let b = put(&store, "/notes/plan.md", "beta", NOW + 1);
        assert_eq!(reader.read(b).unwrap().as_deref(), Some("beta"));
        assert!(
            reader
                .put_snapshot(Path::new("/x"), "x", Reason::Saved, NOW)
                .is_err()
        );
        drop(reader);
        // And once the writer has closed it.
        drop(store);
        let reader = Store::open_to_read(&path).unwrap().unwrap();
        assert_eq!(
            ids(&reader.list(Path::new("/notes/plan.md")).unwrap()),
            [b.0, a.0]
        );
        assert_eq!(reader.read(a).unwrap().as_deref(), Some("alpha"));
        cleanup(&path);
    }

    /// A reader held off by another connection's lock gives up at once,
    /// rather than after the writer's five seconds a query.
    #[test]
    fn a_reader_held_off_by_a_lock_gives_up_quickly() {
        let path = temp("reader-locked");
        let store = Store::open(&path).unwrap();
        put(&store, "/notes/plan.md", "alpha", NOW);
        drop(store);
        let holder = Db::open(&path).unwrap();
        holder
            .execute("PRAGMA locking_mode = EXCLUSIVE; BEGIN EXCLUSIVE;")
            .unwrap();
        let started = std::time::Instant::now();
        let listed = Store::open_to_read(&path)
            .and_then(|reader| reader.unwrap().list(Path::new("/notes/plan.md")));
        assert!(listed.is_err(), "{listed:?}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "waited {:?}",
            started.elapsed()
        );
        drop(holder);
        cleanup(&path);
    }

    /// What the history thread does to the history File History reads, at
    /// `path`: one file forgotten, then the other pruned to its newest
    /// version. Each writer closes after its part, but the first with
    /// `keep`, which is handed back.
    fn forget_and_prune(path: &Path, keep: bool) -> Option<Store> {
        let writer = Store::open(path).unwrap();
        writer.forget(Path::new("/notes/plan.md")).unwrap();
        let kept = keep.then_some(writer);
        let pruner = Store::open(path).unwrap();
        let tiny = Caps {
            per_file: 1,
            ..Caps::default()
        };
        pruner.prune(NOW + DAY_MS, &tiny).unwrap();
        drop(pruner);
        kept
    }

    /// `reader` lists and reads on a thread of its own; what it got, or a
    /// failure after a second rather than a hung test.
    fn read_within_a_second(reader: Store, id: SnapshotId) -> (Vec<Entry>, Option<String>) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let listed = reader.list(Path::new("/notes/list.md"));
            let read = reader.read(id);
            let _ = tx.send((listed, read));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok((listed, read)) => (listed.unwrap(), read.unwrap()),
            Err(e) => panic!("the reader did not answer: {e:?}"),
        }
    }

    /// A reader File History keeps open while the history thread forgets
    /// a file, prunes and closes, in this process or another: what it
    /// reads after comes back at once.
    #[test]
    fn a_reader_survives_the_writer_forgetting_and_closing() {
        if let Some(path) = std::env::var_os("CRC_HISTORY_TEST_WRITER") {
            // The writer in another process, below.
            forget_and_prune(Path::new(&path), false);
            return;
        }
        let path = temp("reader-forget");
        for round in 0..50 {
            for how in ["closed", "kept open", "other process"] {
                cleanup(&path);
                let writer = Store::open(&path).unwrap();
                for n in 0..4 {
                    put(
                        &writer,
                        "/notes/plan.md",
                        &noise(40_000, round * 8 + n),
                        NOW + n as i64,
                    );
                }
                let mut last = None;
                for n in 0..4 {
                    let text = noise(40_000, 1000 + round * 8 + n);
                    last = Some(put(&writer, "/notes/list.md", &text, NOW + n as i64));
                }
                let last = last.unwrap();
                let reader = Store::open_to_read(&path).unwrap().unwrap();
                assert_eq!(reader.list(Path::new("/notes/list.md")).unwrap().len(), 4);
                drop(writer);
                let kept = if how == "other process" {
                    let out = std::process::Command::new(std::env::current_exe().unwrap())
                        .args([
                            "history::store::tests::a_reader_survives_the_writer_forgetting_and_closing",
                            "--exact",
                            "--test-threads=1",
                        ])
                        .env("CRC_HISTORY_TEST_WRITER", &path)
                        .output()
                        .unwrap();
                    assert!(out.status.success(), "{out:?}");
                    None
                } else {
                    forget_and_prune(&path, how == "kept open")
                };
                let (listed, read) = read_within_a_second(reader, last);
                assert_eq!(ids(&listed), [last.0], "round {round}, writer {how}");
                assert!(read.is_some(), "round {round}, writer {how}");
                drop(kept);
            }
        }
        cleanup(&path);
    }

    /// Clear All Local History while File History has a reader open, the
    /// writer closed after, kept open, or in another process: the reader
    /// answers at once, with nothing.
    #[test]
    fn a_reader_survives_the_writer_forgetting_everything() {
        if let Some(path) = std::env::var_os("CRC_HISTORY_TEST_WRITER") {
            Store::open(Path::new(&path)).unwrap().forget_all().unwrap();
            return;
        }
        let path = temp("reader-forget-all");
        for round in 0..20 {
            for how in ["closed", "kept open", "other process"] {
                cleanup(&path);
                let writer = Store::open(&path).unwrap();
                let mut last = None;
                for n in 0..4 {
                    let text = noise(40_000, round * 8 + n);
                    last = Some(put(&writer, "/notes/list.md", &text, NOW + n as i64));
                }
                let last = last.unwrap();
                let reader = Store::open_to_read(&path).unwrap().unwrap();
                assert_eq!(reader.list(Path::new("/notes/list.md")).unwrap().len(), 4);
                let kept = match how {
                    "other process" => {
                        drop(writer);
                        let out = std::process::Command::new(std::env::current_exe().unwrap())
                            .args([
                                "history::store::tests::a_reader_survives_the_writer_forgetting_everything",
                                "--exact",
                                "--test-threads=1",
                            ])
                            .env("CRC_HISTORY_TEST_WRITER", &path)
                            .output()
                            .unwrap();
                        assert!(out.status.success(), "{out:?}");
                        None
                    }
                    _ => {
                        writer.forget_all().unwrap();
                        (how == "kept open").then_some(writer)
                    }
                };
                let (listed, read) = read_within_a_second(reader, last);
                assert_eq!(ids(&listed), [], "round {round}, writer {how}");
                assert_eq!(read, None, "round {round}, writer {how}");
                drop(kept);
            }
        }
        cleanup(&path);
    }

    #[test]
    fn a_tampered_blob_reads_as_missing() {
        let path = temp("tamper");
        let store = Store::open(&path).unwrap();
        // Short enough to stay raw, so the bytes decode fine and only the
        // hash can tell.
        let id = put(&store, "/notes/plan.md", "hello", NOW);
        assert_eq!(count(&store, "SELECT codec FROM blobs"), Codec::Raw.id());
        store
            .db
            .execute("UPDATE blobs SET data = CAST('jello' AS BLOB)")
            .unwrap();
        assert_eq!(store.read(id).unwrap(), None);
        // Truncated compressed data is refused too.
        let long = put(&store, "/notes/long.md", &"line\n".repeat(1000), NOW);
        store
            .db
            .execute("UPDATE blobs SET data = substr(data, 1, 10) WHERE codec = 1")
            .unwrap();
        assert_eq!(store.read(long).unwrap(), None);
        cleanup(&path);
    }

    fn timeline(store: &Store, file: &str, ats: &[i64]) -> Vec<SnapshotId> {
        ats.iter()
            .enumerate()
            .map(|(i, at)| put(store, file, &format!("{file} {i}"), *at))
            .collect()
    }

    fn kept(store: &Store, file: &str) -> Vec<i64> {
        let mut ids = ids(&store.list(Path::new(file)).unwrap());
        ids.sort_unstable();
        ids
    }

    #[test]
    fn retention_keeps_everything_for_a_day() {
        let path = temp("tier-day");
        let store = Store::open(&path).unwrap();
        // NOW is 40 minutes past an hour, so these four share an hour.
        assert_eq!(NOW.rem_euclid(HOUR_MS), 40 * 60_000);
        assert_eq!(NOW.rem_euclid(DAY_MS), 8 * HOUR_MS + 40 * 60_000);
        let ids = timeline(
            &store,
            "/a",
            &[
                NOW - DAY_MS - 1,
                NOW - DAY_MS,
                NOW - DAY_MS + 1,
                NOW - DAY_MS + 2,
            ],
        );
        let pruned = store.prune(NOW, &Caps::default()).unwrap();
        assert_eq!(pruned.snapshots, 1);
        assert_eq!(pruned.blobs, 1);
        // Exactly a day old is hourly, and shares its hour with the one
        // before it; younger is kept whatever its hour.
        assert_eq!(kept(&store, "/a"), [ids[1].0, ids[2].0, ids[3].0]);
        cleanup(&path);
    }

    #[test]
    fn retention_keeps_one_an_hour_to_a_week() {
        let path = temp("tier-hour");
        let store = Store::open(&path).unwrap();
        let hour = (NOW - 2 * DAY_MS).div_euclid(HOUR_MS) * HOUR_MS;
        let week = NOW - 7 * DAY_MS;
        let ids = timeline(
            &store,
            "/a",
            &[
                hour + 10 * 60_000,
                hour + 20 * 60_000,
                hour + HOUR_MS + 1,
                // Same UTC day, either side of the week boundary.
                week - 1,
                week,
                week + 1,
            ],
        );
        store.prune(NOW, &Caps::default()).unwrap();
        // Of two in an hour the newer stays. Exactly a week is daily and
        // shares a day with the one before it; just under is hourly.
        assert_eq!(kept(&store, "/a"), [ids[1].0, ids[2].0, ids[4].0, ids[5].0]);
        cleanup(&path);
    }

    #[test]
    fn retention_keeps_one_a_day_to_a_month() {
        let path = temp("tier-month");
        let store = Store::open(&path).unwrap();
        let day = (NOW - 10 * DAY_MS).div_euclid(DAY_MS) * DAY_MS;
        let month = NOW - 30 * DAY_MS;
        let ids = timeline(
            &store,
            "/a",
            &[
                day + HOUR_MS,
                day + 5 * HOUR_MS,
                day + DAY_MS + 1,
                month - 1,
                month,
                month + 1,
            ],
        );
        store.prune(NOW, &Caps::default()).unwrap();
        assert_eq!(kept(&store, "/a"), [ids[1].0, ids[2].0, ids[5].0]);
        // A second prune at the same time changes nothing.
        assert_eq!(
            store.prune(NOW, &Caps::default()).unwrap(),
            Pruned::default()
        );
        cleanup(&path);
    }

    #[test]
    fn retention_caps_each_file_and_the_total() {
        let path = temp("caps");
        let store = Store::open(&path).unwrap();
        let a = timeline(&store, "/a", &[NOW - 5, NOW - 4, NOW - 3, NOW - 2, NOW - 1]);
        let b = timeline(&store, "/b", &[NOW - 50, NOW - 40]);
        let caps = Caps {
            per_file: 3,
            ..Caps::default()
        };
        store.prune(NOW, &caps).unwrap();
        assert_eq!(kept(&store, "/a"), [a[2].0, a[3].0, a[4].0]);
        assert_eq!(kept(&store, "/b"), [b[0].0, b[1].0]);

        // Big unique texts, so bytes are dominated by them and each
        // snapshot frees its own blob.
        let big: Vec<SnapshotId> = (0..4)
            .map(|i| put(&store, "/big", &noise(20_000, i + 1), NOW - 30 + i as i64))
            .collect();
        let total = count(&store, "SELECT sum(length(data)) FROM blobs") as u64;
        let caps = Caps {
            total_bytes: total - 10_000,
            ..Caps::default()
        };
        // The oldest anywhere goes first: /b's two, which do not free
        // enough on their own, then the oldest big one.
        let pruned = store.prune(NOW, &caps).unwrap();
        assert_eq!(pruned.snapshots, 3);
        assert_eq!(kept(&store, "/b"), Vec::<i64>::new());
        assert_eq!(kept(&store, "/big"), [big[1].0, big[2].0, big[3].0]);
        let after = count(&store, "SELECT sum(length(data)) FROM blobs") as u64;
        assert!(after <= caps.total_bytes);
        cleanup(&path);
    }

    #[test]
    fn gc_keeps_blobs_still_referenced() {
        let path = temp("gc");
        let store = Store::open(&path).unwrap();
        let shared = "shared text\n".repeat(50);
        put(&store, "/old", &shared, NOW - 40 * DAY_MS);
        let young = put(&store, "/young", &shared, NOW);
        // An undo journal based on a text whose only snapshot is pruned.
        put(&store, "/undo", "base text", NOW - 40 * DAY_MS);
        store
            .put_undo(Path::new("/undo"), "base text", b"journal bytes", NOW)
            .unwrap();
        put(&store, "/gone", "only here", NOW - 40 * DAY_MS);

        let pruned = store.prune(NOW, &Caps::default()).unwrap();
        assert_eq!(pruned.snapshots, 3);
        assert_eq!(pruned.blobs, 1, "only the text nothing else holds");
        assert_eq!(store.read(young).unwrap().as_deref(), Some(&*shared));
        let undo = store.get_undo(Path::new("/undo")).unwrap().unwrap();
        assert_eq!(undo.journal, b"journal bytes");
        assert_eq!(undo.base, crate::ext::verify::sha256(b"base text").unwrap());
        assert_eq!(count(&store, "SELECT count(*) FROM blobs"), 2);
        // /old and /gone have nothing left, so their rows go too.
        assert_eq!(count(&store, "SELECT count(*) FROM files"), 2);
        assert_eq!(count(&store, "PRAGMA freelist_count"), 0, "vacuumed");

        store.forget(Path::new("/undo")).unwrap();
        assert_eq!(store.get_undo(Path::new("/undo")).unwrap(), None);
        assert_eq!(count(&store, "SELECT count(*) FROM blobs"), 1);
        assert_eq!(store.read(young).unwrap().as_deref(), Some(&*shared));
        cleanup(&path);
    }

    #[test]
    fn undo_journals_round_trip_and_replace() {
        let path = temp("undo");
        let store = Store::open(&path).unwrap();
        let file = Path::new("/notes/plan.md");
        assert_eq!(store.get_undo(file).unwrap(), None);
        let journal: Vec<u8> = (0..20_000u32).map(|i| (i % 7) as u8).collect();
        store.put_undo(file, "v1", &journal, NOW).unwrap();
        let undo = store.get_undo(file).unwrap().unwrap();
        assert_eq!(undo.journal, journal);
        assert_eq!(undo.at, NOW);
        assert_eq!(count(&store, "SELECT codec FROM undo"), Codec::Lzfse.id());

        store.put_undo(file, "v2", b"", NOW + 1).unwrap();
        let undo = store.get_undo(file).unwrap().unwrap();
        assert_eq!(undo.journal, b"");
        assert_eq!(undo.base, crate::ext::verify::sha256(b"v2").unwrap());
        assert_eq!(
            count(&store, "SELECT count(*) FROM blobs"),
            1,
            "the old base went with the journal that used it"
        );
        store.db.execute("UPDATE undo SET size = 5").unwrap();
        assert_eq!(store.get_undo(file).unwrap(), None, "damaged reads as none");
        cleanup(&path);
    }

    #[test]
    fn rename_moves_or_merges_a_timeline() {
        let path = temp("rename");
        let store = Store::open(&path).unwrap();
        let a = timeline(&store, "/a", &[NOW, NOW + 2]);
        store
            .put_undo(Path::new("/a"), "/a 1", b"a's undo", NOW)
            .unwrap();
        store.rename(Path::new("/a"), Path::new("/b")).unwrap();
        assert!(store.list(Path::new("/a")).unwrap().is_empty());
        assert_eq!(kept(&store, "/b"), [a[0].0, a[1].0]);
        assert_eq!(
            store.get_undo(Path::new("/b")).unwrap().unwrap().journal,
            b"a's undo"
        );

        let c = timeline(&store, "/c", &[NOW + 1]);
        store
            .put_undo(Path::new("/c"), "/c 0", b"c's undo", NOW)
            .unwrap();
        store.rename(Path::new("/b"), Path::new("/c")).unwrap();
        assert_eq!(
            ids(&store.list(Path::new("/c")).unwrap()),
            [c[0].0, a[1].0, a[0].0],
            "both kept, newest id first"
        );
        assert_eq!(
            store.get_undo(Path::new("/c")).unwrap().unwrap().journal,
            b"a's undo"
        );
        assert_eq!(count(&store, "SELECT count(*) FROM files"), 1);
        assert_eq!(count(&store, "SELECT count(*) FROM undo"), 1);
        // Renaming something without history is not an error.
        store
            .rename(Path::new("/nothing"), Path::new("/c"))
            .unwrap();
        assert_eq!(store.list(Path::new("/c")).unwrap().len(), 3);
        cleanup(&path);
    }

    #[test]
    fn rename_folder_moves_only_what_is_inside() {
        let path = temp("rename-folder");
        let store = Store::open(&path).unwrap();
        let one = timeline(&store, "/a/b/one.txt", &[NOW]);
        let deep = timeline(&store, "/a/b/sub/deep.txt", &[NOW]);
        let sibling = timeline(&store, "/a/bc/one.txt", &[NOW]);
        let same = timeline(&store, "/a/b", &[NOW]);
        // Already has a timeline of its own at the destination.
        let there = timeline(&store, "/x/one.txt", &[NOW + 1]);
        store
            .rename_folder(Path::new("/a/b"), Path::new("/x/"))
            .unwrap();
        assert!(store.list(Path::new("/a/b/one.txt")).unwrap().is_empty());
        assert!(
            store
                .list(Path::new("/a/b/sub/deep.txt"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            ids(&store.list(Path::new("/x/one.txt")).unwrap()),
            [there[0].0, one[0].0],
            "merged with the timeline already there"
        );
        assert_eq!(kept(&store, "/x/sub/deep.txt"), [deep[0].0]);
        assert_eq!(
            kept(&store, "/a/bc/one.txt"),
            [sibling[0].0],
            "/a/bc is not inside /a/b"
        );
        assert_eq!(
            kept(&store, "/a/b"),
            [same[0].0],
            "nor is a file named like the folder"
        );
        assert_eq!(count(&store, "SELECT count(*) FROM files"), 4);
        cleanup(&path);
    }

    #[test]
    fn forget_and_deleted_paths() {
        let path = temp("forget");
        let dir = path.parent().unwrap().join("project");
        std::fs::create_dir_all(&dir).unwrap();
        let kept_file = dir.join("kept.txt");
        let deleted = dir.join("deleted.txt");
        let older = dir.join("older.txt");
        for file in [&kept_file, &deleted, &older] {
            std::fs::write(file, "x").unwrap();
        }
        let store = Store::open(&path).unwrap();
        put(&store, older.to_str().unwrap(), "older", NOW);
        put(&store, deleted.to_str().unwrap(), "deleted", NOW + 1);
        put(&store, kept_file.to_str().unwrap(), "kept", NOW + 2);
        std::fs::remove_file(&deleted).unwrap();
        std::fs::remove_file(&older).unwrap();
        assert_eq!(
            store.deleted_paths(|p| p.exists()).unwrap(),
            [deleted.clone(), older.clone()]
        );

        store.forget(&deleted).unwrap();
        assert!(store.list(&deleted).unwrap().is_empty());
        assert_eq!(store.deleted_paths(|p| p.exists()).unwrap(), [older]);
        assert_eq!(store.list(&kept_file).unwrap().len(), 1);

        store.forget_all().unwrap();
        assert!(store.list(&kept_file).unwrap().is_empty());
        for table in ["files", "blobs", "snapshots", "undo"] {
            assert_eq!(count(&store, &format!("SELECT count(*) FROM {table}")), 0);
        }
        cleanup(&path);
    }

    #[test]
    fn a_corrupt_file_is_set_aside_not_deleted() {
        let path = temp("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let garbage = noise(8192, 7);
        std::fs::write(&path, &garbage).unwrap();
        let wal = PathBuf::from(format!("{}-wal", path.display()));
        std::fs::write(&wal, "not a wal").unwrap();

        let mut store = Store::open(&path).unwrap();
        let aside = store.take_quarantined().expect("reported");
        assert_eq!(store.take_quarantined(), None, "reported once");
        let name = aside.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("history.db.corrupt-"), "{name}");
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), garbage);
        let aside_wal = PathBuf::from(format!("{}-wal", aside.display()));
        // Moved along with it. SQLite resets a WAL whose header is invalid
        // as it reads the database, so only the file, not its bytes, can
        // be kept here.
        assert!(aside_wal.exists());
        // And the new one works.
        put(&store, "/notes/plan.md", "fresh", NOW);
        assert_eq!(store.list(Path::new("/notes/plan.md")).unwrap().len(), 1);
        cleanup(&path);
    }

    #[test]
    fn a_newer_schema_opens_read_only() {
        let path = temp("newer");
        let store = Store::open(&path).unwrap();
        let id = put(&store, "/notes/plan.md", "from before", NOW);
        store.db.execute("PRAGMA user_version = 99").unwrap();
        drop(store);

        let store = Store::open(&path).unwrap();
        assert!(store.read_only());
        assert_eq!(store.list(Path::new("/notes/plan.md")).unwrap().len(), 1);
        assert_eq!(store.read(id).unwrap().as_deref(), Some("from before"));
        let writes = [
            store
                .put_snapshot(Path::new("/x"), "y", Reason::Saved, NOW)
                .map(|_| ()),
            store.put_undo(Path::new("/x"), "y", b"", NOW),
            store.rename(Path::new("/a"), Path::new("/b")),
            store.forget(Path::new("/a")),
            store.forget_all(),
            store.prune(NOW, &Caps::default()).map(|_| ()),
        ];
        for write in writes {
            assert!(matches!(write, Err(Error::ReadOnly)), "{write:?}");
        }
        cleanup(&path);
    }

    #[test]
    fn two_connections_write_at_once() {
        let path = temp("concurrent");
        drop(Store::open(&path).unwrap());
        let threads: Vec<_> = (0..2)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let store = Store::open(&path).unwrap();
                    for i in 0..40 {
                        // Shared texts too, so both race for the same blob.
                        let text = if i % 2 == 0 {
                            format!("shared {i}")
                        } else {
                            format!("thread {t} {i}")
                        };
                        store
                            .put_snapshot(
                                Path::new(&format!("/t{t}")),
                                &text,
                                Reason::Idle,
                                NOW + i,
                            )
                            .unwrap();
                    }
                    if t == 0 {
                        store.prune(NOW, &Caps::default()).unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.list(Path::new("/t0")).unwrap().len(), 40);
        assert_eq!(store.list(Path::new("/t1")).unwrap().len(), 40);
        assert_eq!(count(&store, "SELECT count(*) FROM blobs"), 60);
        cleanup(&path);
    }

    #[test]
    fn a_full_database_is_its_own_error_and_stays_consistent() {
        let path = temp("full");
        let store = Store::open(&path).unwrap();
        let id = put(&store, "/notes/plan.md", "before", NOW);
        let pages = count(&store, "PRAGMA page_count");
        store
            .db
            .execute(&format!("PRAGMA max_page_count = {}", pages + 2))
            .unwrap();
        let result = store.put_snapshot(
            Path::new("/notes/plan.md"),
            &noise(1 << 20, 3),
            Reason::Saved,
            NOW + 1,
        );
        assert!(matches!(result, Err(Error::Full(_))), "{result:?}");
        assert_eq!(
            ids(&store.list(Path::new("/notes/plan.md")).unwrap()),
            [id.0]
        );
        assert_eq!(store.read(id).unwrap().as_deref(), Some("before"));
        let mut check = store.db.prepare("PRAGMA quick_check").unwrap();
        assert!(check.step().unwrap());
        assert_eq!(check.text(0), "ok");
        drop(check);
        // A small write still fits, and the connection is not stuck in a
        // transaction.
        put(&store, "/notes/plan.md", "after", NOW + 2);
        cleanup(&path);
    }

    #[test]
    fn a_path_that_is_not_utf8_is_refused() {
        use std::os::unix::ffi::OsStrExt;
        let path = temp("utf8");
        let store = Store::open(&path).unwrap();
        let bad = Path::new(std::ffi::OsStr::from_bytes(b"/notes/\xff.md"));
        assert!(matches!(
            store.put_snapshot(bad, "x", Reason::Saved, NOW),
            Err(Error::NotUtf8)
        ));
        cleanup(&path);
    }
}
