//! A small safe wrapper over the SQLite C API.
//!
//! Only what the index needs: open, pragmas, prepared statements, binding,
//! stepping, and transactions. Everything returns `Result` rather than
//! panicking, because `panic = "abort"` is set in the release profile and
//! this code runs on a background thread: a panic here would take the editor
//! and any unsaved buffer with it.

use std::ffi::{CString, c_char, c_int, c_void};
use std::path::Path;
use std::ptr::NonNull;

use super::ffi;

/// The oldest SQLite this code is willing to talk to.
///
/// 3.37 is where `STRICT` tables arrived, which is what stops a bound float
/// from quietly landing in an integer column. macOS 14 ships 3.43, so the
/// floor has margin; the check exists because the library is the OS's, not
/// ours, and its version is not something we pin.
const MIN_VERSION: c_int = 3_037_000;

#[derive(Debug)]
pub struct Error {
    pub code: i32,
    pub message: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sqlite error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn err(code: c_int, message: impl Into<String>) -> Error {
    Error {
        code,
        message: message.into(),
    }
}

/// `prefix` with LIKE's wildcards made literal.
pub fn escape_like(prefix: &str) -> String {
    let mut out = String::with_capacity(prefix.len());
    for c in prefix.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// An open database. Owns its connection exclusively.
///
/// Not `Sync`: Apple builds SQLite with `THREADSAFE=2` (multi-thread, not
/// serialized), so one connection must never be used from two threads at
/// once. It is `Send`, so it can be moved to the indexing thread and stay
/// there.
pub struct Db {
    handle: NonNull<ffi::Sqlite3>,
}

// SAFETY: the handle is owned exclusively and never aliased. Moving it
// between threads is fine; using it from two at once is not, which is why
// Sync is deliberately not implemented.
unsafe impl Send for Db {}

impl Db {
    /// Opens (creating if needed) a database at `path`.
    pub fn open(path: &Path) -> Result<Db> {
        Db::open_with(
            path,
            ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE | ffi::SQLITE_OPEN_FULLMUTEX,
        )
    }

    /// Opens read-only, for a reader that must not modify the index.
    pub fn open_readonly(path: &Path) -> Result<Db> {
        Db::open_with(path, ffi::SQLITE_OPEN_READONLY | ffi::SQLITE_OPEN_FULLMUTEX)
    }

    fn open_with(path: &Path, flags: c_int) -> Result<Db> {
        let version = unsafe { ffi::sqlite3_libversion_number() };
        if version < MIN_VERSION {
            return Err(err(
                0,
                format!("libsqlite3 {version} is older than the required {MIN_VERSION}"),
            ));
        }

        let c_path = CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| err(0, "path contains an interior NUL"))?;

        let mut raw: *mut ffi::Sqlite3 = std::ptr::null_mut();
        // SAFETY: c_path outlives the call; raw is written on success and on
        // failure both, which is why it is closed in the error path too.
        let rc =
            unsafe { ffi::sqlite3_open_v2(c_path.as_ptr(), &mut raw, flags, std::ptr::null()) };
        let Some(handle) = NonNull::new(raw) else {
            return Err(err(rc, "sqlite3_open_v2 returned no handle"));
        };
        let db = Db { handle };
        if rc != ffi::SQLITE_OK {
            // open_v2 hands back a handle even on failure, purely so the
            // error message can be read off it. It still has to be closed.
            return Err(db.last_error(rc));
        }

        // Apple builds with DQS=3, under which a double-quoted identifier
        // that does not resolve silently becomes a string literal. A typo'd
        // column name would then be a constant instead of an error.
        //
        // The variadic tail is `(int, int*)`: the new value, and where to
        // write the resulting one, or null. It has to be spelled as a pointer.
        // A literal `0` is an `i32`, which fills half of a pointer-sized
        // variadic slot and leaves the rest to whatever was there, and SQLite
        // writes through the result if it is not null.
        let no_result = std::ptr::null_mut::<std::ffi::c_int>();
        for verb in [ffi::SQLITE_DBCONFIG_DQS_DDL, ffi::SQLITE_DBCONFIG_DQS_DML] {
            let off: std::ffi::c_int = 0;
            let rc = unsafe { ffi::sqlite3_db_config(db.handle.as_ptr(), verb, off, no_result) };
            if rc != ffi::SQLITE_OK {
                return Err(db.last_error(rc));
            }
        }
        unsafe { ffi::sqlite3_busy_timeout(db.handle.as_ptr(), 5_000) };
        Ok(db)
    }

    /// How long a statement waits on another connection's lock before it
    /// fails with SQLITE_BUSY. Five seconds from `open`.
    pub fn set_busy_timeout(&self, ms: i32) {
        // SAFETY: the handle is open for as long as `self` is.
        unsafe { ffi::sqlite3_busy_timeout(self.handle.as_ptr(), ms) };
    }

    pub fn version() -> i32 {
        unsafe { ffi::sqlite3_libversion_number() as i32 }
    }

    fn last_error(&self, code: c_int) -> Error {
        // SAFETY: errmsg points into the connection and is valid until the
        // next call on it.
        let message = unsafe {
            let ptr = ffi::sqlite3_errmsg(self.handle.as_ptr());
            if ptr.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
            }
        };
        let extended = unsafe { ffi::sqlite3_extended_errcode(self.handle.as_ptr()) };
        err(if extended != 0 { extended } else { code }, message)
    }

    /// Runs one or more statements with no bindings and no results.
    pub fn execute(&self, sql: &str) -> Result<()> {
        let c_sql = CString::new(sql).map_err(|_| err(0, "sql contains an interior NUL"))?;
        let mut raw_err: *mut c_char = std::ptr::null_mut();
        // SAFETY: c_sql outlives the call; raw_err is freed below.
        let rc = unsafe {
            ffi::sqlite3_exec(
                self.handle.as_ptr(),
                c_sql.as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
                &mut raw_err,
            )
        };
        if rc != ffi::SQLITE_OK {
            let message = if raw_err.is_null() {
                String::new()
            } else {
                // SAFETY: sqlite allocated it and we own it now.
                let owned = unsafe {
                    std::ffi::CStr::from_ptr(raw_err)
                        .to_string_lossy()
                        .into_owned()
                };
                unsafe { ffi::sqlite3_free(raw_err as *mut c_void) };
                owned
            };
            return Err(err(rc, message));
        }
        Ok(())
    }

    pub fn prepare(&self, sql: &str) -> Result<Statement<'_>> {
        let c_sql = CString::new(sql).map_err(|_| err(0, "sql contains an interior NUL"))?;
        let mut raw: *mut ffi::Stmt = std::ptr::null_mut();
        // SAFETY: c_sql outlives the call.
        let rc = unsafe {
            ffi::sqlite3_prepare_v3(
                self.handle.as_ptr(),
                c_sql.as_ptr(),
                -1,
                0,
                &mut raw,
                std::ptr::null_mut(),
            )
        };
        if rc != ffi::SQLITE_OK {
            return Err(self.last_error(rc));
        }
        let Some(handle) = NonNull::new(raw) else {
            return Err(err(rc, "empty statement"));
        };
        Ok(Statement { db: self, handle })
    }

    pub fn last_insert_rowid(&self) -> i64 {
        unsafe { ffi::sqlite3_last_insert_rowid(self.handle.as_ptr()) }
    }

    pub fn changes(&self) -> i32 {
        unsafe { ffi::sqlite3_changes(self.handle.as_ptr()) as i32 }
    }

    /// Runs `body` inside a transaction, rolling back if it fails.
    ///
    /// Rollback errors are swallowed deliberately: the caller wants to see
    /// why the body failed, not why the cleanup after it also failed.
    pub fn transaction<T>(&self, body: impl FnOnce() -> Result<T>) -> Result<T> {
        self.execute("BEGIN IMMEDIATE")?;
        match body() {
            Ok(value) => match self.execute("COMMIT") {
                Ok(()) => Ok(value),
                Err(e) => {
                    // A COMMIT that fails (busy, or a deferred constraint)
                    // leaves the transaction open, and every later BEGIN on
                    // this connection would then fail too.
                    let _ = self.execute("ROLLBACK");
                    Err(e)
                }
            },
            Err(e) => {
                let _ = self.execute("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Asks SQLite to abort whatever this connection is currently running.
    /// Safe to call from another thread, which is the one exception to the
    /// one-thread rule and is documented as such by SQLite.
    pub fn interrupt(&self) {
        unsafe { ffi::sqlite3_interrupt(self.handle.as_ptr()) };
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        // SAFETY: the handle is owned and dropped once. close_v2 tolerates
        // outstanding statements by deferring the close, which is why it is
        // used rather than close.
        unsafe { ffi::sqlite3_close_v2(self.handle.as_ptr()) };
    }
}

/// A prepared statement, borrowed from its database.
pub struct Statement<'db> {
    db: &'db Db,
    handle: NonNull<ffi::Stmt>,
}

impl Statement<'_> {
    /// Binds parameters, one-based, resetting any previous bindings.
    pub fn bind(&mut self, params: &[Value<'_>]) -> Result<()> {
        // Apple builds with OMIT_AUTORESET, so reuse requires an explicit
        // reset; doing it here means callers cannot forget.
        unsafe {
            ffi::sqlite3_reset(self.handle.as_ptr());
            ffi::sqlite3_clear_bindings(self.handle.as_ptr());
        }
        for (i, value) in params.iter().enumerate() {
            let index = (i + 1) as c_int;
            let rc = match value {
                Value::Int(v) => unsafe {
                    ffi::sqlite3_bind_int64(self.handle.as_ptr(), index, *v)
                },
                Value::Text(s) => unsafe {
                    // SQLITE_TRANSIENT, so sqlite copies rather than keeping
                    // a pointer to a string that may be a temporary here.
                    ffi::sqlite3_bind_text(
                        self.handle.as_ptr(),
                        index,
                        s.as_ptr() as *const c_char,
                        s.len() as c_int,
                        ffi::SQLITE_TRANSIENT,
                    )
                },
                Value::Blob(b) => {
                    // The cast below would silently truncate a length past
                    // i32::MAX, storing the wrong bytes rather than failing.
                    let Ok(len) = c_int::try_from(b.len()) else {
                        return Err(err(
                            ffi::SQLITE_TOOBIG,
                            format!("blob of {} bytes is too big to bind", b.len()),
                        ));
                    };
                    // SQLITE_TRANSIENT for the same reason as Text above.
                    unsafe {
                        ffi::sqlite3_bind_blob(
                            self.handle.as_ptr(),
                            index,
                            b.as_ptr() as *const c_void,
                            len,
                            ffi::SQLITE_TRANSIENT,
                        )
                    }
                }
                Value::Null => unsafe { ffi::sqlite3_bind_null(self.handle.as_ptr(), index) },
            };
            if rc != ffi::SQLITE_OK {
                return Err(self.db.last_error(rc));
            }
        }
        Ok(())
    }

    /// Steps once. `Ok(true)` means a row is available.
    pub fn step(&mut self) -> Result<bool> {
        let rc = unsafe { ffi::sqlite3_step(self.handle.as_ptr()) };
        match rc {
            ffi::SQLITE_ROW => Ok(true),
            ffi::SQLITE_DONE => Ok(false),
            _ => {
                unsafe { ffi::sqlite3_reset(self.handle.as_ptr()) };
                Err(self.db.last_error(rc))
            }
        }
    }

    /// Runs a statement expected to return no rows.
    pub fn run(&mut self, params: &[Value<'_>]) -> Result<()> {
        self.bind(params)?;
        while self.step()? {}
        unsafe { ffi::sqlite3_reset(self.handle.as_ptr()) };
        Ok(())
    }

    pub fn column_count(&self) -> i32 {
        unsafe { ffi::sqlite3_column_count(self.handle.as_ptr()) as i32 }
    }

    pub fn int(&self, col: i32) -> i64 {
        unsafe { ffi::sqlite3_column_int64(self.handle.as_ptr(), col as c_int) }
    }

    pub fn is_null(&self, col: i32) -> bool {
        unsafe { ffi::sqlite3_column_type(self.handle.as_ptr(), col as c_int) == ffi::SQLITE_NULL }
    }

    pub fn text(&self, col: i32) -> String {
        // SAFETY: column_text must come first; column_bytes then reports the
        // length of that UTF-8 encoding rather than of some other one.
        unsafe {
            let ptr = ffi::sqlite3_column_text(self.handle.as_ptr(), col as c_int);
            if ptr.is_null() {
                return String::new();
            }
            let len = ffi::sqlite3_column_bytes(self.handle.as_ptr(), col as c_int) as usize;
            String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned()
        }
    }

    /// The column's bytes. A zero-length blob and SQL NULL both read as
    /// empty; `is_null` tells them apart.
    pub fn blob(&self, col: i32) -> Vec<u8> {
        // SAFETY: column_blob must come first, as with text. A zero-length
        // blob comes back as a null pointer, which from_raw_parts forbids.
        unsafe {
            let ptr = ffi::sqlite3_column_blob(self.handle.as_ptr(), col as c_int);
            if ptr.is_null() {
                return Vec::new();
            }
            let len = ffi::sqlite3_column_bytes(self.handle.as_ptr(), col as c_int) as usize;
            std::slice::from_raw_parts(ptr as *const u8, len).to_vec()
        }
    }
}

impl Drop for Statement<'_> {
    fn drop(&mut self) {
        unsafe { ffi::sqlite3_finalize(self.handle.as_ptr()) };
    }
}

/// A bindable value.
#[derive(Debug, Clone, Copy)]
pub enum Value<'a> {
    Int(i64),
    Text(&'a str),
    Blob(&'a [u8]),
    Null,
}

impl<'a> From<i64> for Value<'a> {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}

impl<'a> From<usize> for Value<'a> {
    fn from(v: usize) -> Self {
        Value::Int(v as i64)
    }
}

impl<'a> From<&'a str> for Value<'a> {
    fn from(v: &'a str) -> Self {
        Value::Text(v)
    }
}

impl<'a> From<&'a [u8]> for Value<'a> {
    fn from(v: &'a [u8]) -> Self {
        Value::Blob(v)
    }
}

impl<'a> From<bool> for Value<'a> {
    fn from(v: bool) -> Self {
        Value::Int(v as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("caio-db-tests");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join(format!("{name}.db"));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn links_against_a_recent_enough_sqlite() {
        assert!(
            Db::version() >= MIN_VERSION,
            "system sqlite is {} which is below the floor",
            Db::version()
        );
    }

    #[test]
    fn creates_reads_and_writes() {
        let path = temp("roundtrip");
        let db = Db::open(&path).expect("open");
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL) STRICT")
            .expect("create");

        let mut insert = db
            .prepare("INSERT INTO t (name) VALUES (?1)")
            .expect("prepare");
        insert.run(&["first".into()]).expect("insert");
        insert.run(&["second".into()]).expect("insert");
        assert_eq!(db.last_insert_rowid(), 2);

        let mut select = db
            .prepare("SELECT id, name FROM t ORDER BY id")
            .expect("prepare");
        select.bind(&[]).expect("bind");
        let mut rows = Vec::new();
        while select.step().expect("step") {
            rows.push((select.int(0), select.text(1)));
        }
        assert_eq!(rows, vec![(1, "first".into()), (2, "second".into())]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn wal_mode_is_available() {
        let path = temp("wal");
        let db = Db::open(&path).expect("open");
        db.execute("PRAGMA journal_mode = WAL").expect("wal");

        let mut q = db.prepare("PRAGMA journal_mode").expect("prepare");
        q.bind(&[]).expect("bind");
        assert!(q.step().expect("step"));
        assert_eq!(q.text(0).to_lowercase(), "wal");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn strict_tables_reject_the_wrong_type() {
        let path = temp("strict");
        let db = Db::open(&path).expect("open");
        db.execute("CREATE TABLE t (n INTEGER NOT NULL) STRICT")
            .expect("create");
        let mut insert = db
            .prepare("INSERT INTO t (n) VALUES (?1)")
            .expect("prepare");
        assert!(
            insert.run(&["not a number".into()]).is_err(),
            "STRICT is what stops a wrong type landing in a published contract"
        );
        std::fs::remove_file(&path).ok();
    }

    /// Apple builds with DQS=3, where an unresolvable double-quoted name
    /// degrades into a string literal. Turning it off makes a typo an error.
    #[test]
    fn double_quoted_typos_are_errors_not_literals() {
        let path = temp("dqs");
        let db = Db::open(&path).expect("open");
        db.execute("CREATE TABLE t (real_column INTEGER)")
            .expect("create");
        assert!(
            db.execute("SELECT \"no_such_column\" FROM t").is_err(),
            "a misspelled column must not silently become a string"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_failed_transaction_rolls_back() {
        let path = temp("rollback");
        let db = Db::open(&path).expect("open");
        db.execute("CREATE TABLE t (n INTEGER) STRICT")
            .expect("create");

        let outcome: Result<()> = db.transaction(|| {
            db.execute("INSERT INTO t VALUES (1)")?;
            Err(err(0, "deliberate failure"))
        });
        assert!(outcome.is_err());

        let mut count = db.prepare("SELECT count(*) FROM t").expect("prepare");
        count.bind(&[]).expect("bind");
        assert!(count.step().expect("step"));
        assert_eq!(count.int(0), 0, "the insert should have been rolled back");
        std::fs::remove_file(&path).ok();
    }

    /// The body succeeds and it is the COMMIT that fails, here on a deferred
    /// foreign key. Without a rollback the connection stays inside that
    /// transaction and every later one dies on BEGIN.
    #[test]
    fn a_failed_commit_does_not_wedge_the_connection() {
        let path = temp("failed-commit");
        let db = Db::open(&path).expect("open");
        db.execute("PRAGMA foreign_keys = ON").expect("pragma");
        db.execute("CREATE TABLE parent (id INTEGER PRIMARY KEY) STRICT")
            .expect("create parent");
        db.execute(
            "CREATE TABLE child (p INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED) STRICT",
        )
        .expect("create child");

        let outcome = db.transaction(|| db.execute("INSERT INTO child VALUES (42)"));
        assert!(outcome.is_err(), "the orphan should fail at COMMIT");

        db.transaction(|| db.execute("INSERT INTO parent VALUES (1)"))
            .expect("the next transaction must be able to begin");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_successful_transaction_commits() {
        let path = temp("commit");
        let db = Db::open(&path).expect("open");
        db.execute("CREATE TABLE t (n INTEGER) STRICT")
            .expect("create");
        db.transaction(|| db.execute("INSERT INTO t VALUES (7)"))
            .expect("commit");

        let mut q = db.prepare("SELECT n FROM t").expect("prepare");
        q.bind(&[]).expect("bind");
        assert!(q.step().expect("step"));
        assert_eq!(q.int(0), 7);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn text_with_multibyte_and_nulls_round_trips() {
        let path = temp("unicode");
        let db = Db::open(&path).expect("open");
        db.execute("CREATE TABLE t (s TEXT, maybe TEXT) STRICT")
            .expect("create");
        let mut insert = db
            .prepare("INSERT INTO t VALUES (?1, ?2)")
            .expect("prepare");
        insert
            .run(&["café 漢字 🌍".into(), Value::Null])
            .expect("insert");

        let mut q = db.prepare("SELECT s, maybe FROM t").expect("prepare");
        q.bind(&[]).expect("bind");
        assert!(q.step().expect("step"));
        assert_eq!(q.text(0), "café 漢字 🌍");
        assert!(q.is_null(1));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_bad_statement_reports_rather_than_panics() {
        let path = temp("badsql");
        let db = Db::open(&path).expect("open");
        let Err(e) = db.prepare("SELEKT nonsense") else {
            panic!("a syntax error should not prepare");
        };
        assert!(
            !e.message.is_empty(),
            "the error should say something useful"
        );
        std::fs::remove_file(&path).ok();
    }

    fn blob_table(name: &str) -> (std::path::PathBuf, Db) {
        let path = temp(name);
        let db = Db::open(&path).expect("open");
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, b BLOB) STRICT")
            .expect("create");
        (path, db)
    }

    fn read_blob(db: &Db, id: i64) -> (bool, Vec<u8>) {
        let mut q = db
            .prepare("SELECT b FROM t WHERE id = ?1")
            .expect("prepare");
        q.bind(&[id.into()]).expect("bind");
        assert!(q.step().expect("step"), "row {id} should exist");
        (q.is_null(0), q.blob(0))
    }

    #[test]
    fn blobs_round_trip_empty_large_and_binary() {
        let (path, db) = blob_table("blob-roundtrip");
        let large: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i * 31 % 251) as u8).collect();
        let binary: &[u8] = &[0x00, 0xff, 0xfe, 0x00, 0xc3, 0x28, 0x80, 0x00];
        let mut insert = db
            .prepare("INSERT INTO t (id, b) VALUES (?1, ?2)")
            .expect("prepare");
        insert.run(&[1i64.into(), Value::Blob(&[])]).expect("empty");
        insert
            .run(&[2i64.into(), large.as_slice().into()])
            .expect("large");
        insert.run(&[3i64.into(), binary.into()]).expect("binary");

        assert_eq!(read_blob(&db, 1), (false, Vec::new()));
        let (null, got) = read_blob(&db, 2);
        assert!(!null);
        assert!(
            got == large,
            "the 2 MiB blob should come back byte for byte"
        );
        assert_eq!(read_blob(&db, 3), (false, binary.to_vec()));

        let mut ty = db
            .prepare("SELECT typeof(b) FROM t ORDER BY id")
            .expect("prepare");
        ty.bind(&[]).expect("bind");
        let mut types = Vec::new();
        while ty.step().expect("step") {
            types.push(ty.text(0));
        }
        assert_eq!(types, ["blob", "blob", "blob"], "stored as BLOB, not TEXT");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_null_blob_is_not_an_empty_blob() {
        let (path, db) = blob_table("blob-null");
        let mut insert = db
            .prepare("INSERT INTO t (id, b) VALUES (?1, ?2)")
            .expect("prepare");
        insert.run(&[1i64.into(), Value::Null]).expect("null");
        insert.run(&[2i64.into(), Value::Blob(&[])]).expect("empty");

        assert_eq!(read_blob(&db, 1), (true, Vec::new()));
        assert_eq!(read_blob(&db, 2), (false, Vec::new()));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn blobs_match_in_a_where_clause() {
        let path = temp("blob-where");
        let db = Db::open(&path).expect("open");
        db.execute("CREATE TABLE blobs (hash BLOB PRIMARY KEY, size INTEGER NOT NULL) STRICT")
            .expect("create");
        let a = [0xabu8; 32];
        let mut b = a;
        b[31] = 0x00;
        let mut insert = db
            .prepare("INSERT INTO blobs (hash, size) VALUES (?1, ?2)")
            .expect("prepare");
        insert.run(&[a.as_slice().into(), 10i64.into()]).expect("a");
        insert.run(&[b.as_slice().into(), 20i64.into()]).expect("b");

        let mut find = db
            .prepare("SELECT size, hash FROM blobs WHERE hash = ?1")
            .expect("prepare");
        find.bind(&[b.as_slice().into()]).expect("bind");
        assert!(find.step().expect("step"));
        assert_eq!(find.int(0), 20);
        assert_eq!(find.blob(1), b.to_vec());
        assert!(!find.step().expect("step"), "exactly one row should match");

        find.bind(&[[0u8; 32].as_slice().into()]).expect("bind");
        assert!(
            !find.step().expect("step"),
            "an unknown hash matches nothing"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn statements_can_be_rerun() {
        let path = temp("reuse");
        let db = Db::open(&path).expect("open");
        db.execute("CREATE TABLE t (n INTEGER) STRICT")
            .expect("create");
        let mut insert = db.prepare("INSERT INTO t VALUES (?1)").expect("prepare");
        for n in 0..5i64 {
            insert.run(&[n.into()]).expect("insert");
        }
        let mut count = db.prepare("SELECT count(*) FROM t").expect("prepare");
        count.bind(&[]).expect("bind");
        assert!(count.step().expect("step"));
        assert_eq!(count.int(0), 5);
        std::fs::remove_file(&path).ok();
    }
}
