//! Bindings to the system SQLite, mirroring `src/syntax/ffi.rs`.
//!
//! macOS ships libsqlite3 and the linker resolves `-lsqlite3` through the SDK
//! stub with no build configuration at all. That is why this costs zero
//! crates and zero build scripts: `rusqlite` would have cost seven crates and
//! a build script that shells out to `pkg-config`, for a library already on
//! every Mac.
//!
//! It is also the same move made twice already in this codebase: CoreText
//! instead of a Rust font stack, Metal instead of wgpu. The OS ships the
//! thing; take it.
//!
//! Two properties of Apple's specific build shape the wrapper above:
//!
//! - `SQLITE_OMIT_LOAD_EXTENSION` is compiled in, so this database is
//!   structurally incapable of loading a dylib. For a file external tools and
//!   agents are invited to open, that is a real safety property.
//! - `SQLITE_THREADSAFE=2` means multi-thread, not serialized: a connection
//!   must never be touched by two threads at once. The design has exactly one
//!   writer, owned by one thread.

use std::ffi::{c_char, c_int, c_void};

/// Opaque handles.
#[repr(C)]
pub struct Sqlite3 {
    _private: [u8; 0],
}

#[repr(C)]
pub struct Stmt {
    _private: [u8; 0],
}

// Result codes we actually branch on.
pub const SQLITE_OK: c_int = 0;
pub const SQLITE_ROW: c_int = 100;
pub const SQLITE_DONE: c_int = 101;

// Flags for sqlite3_open_v2.
pub const SQLITE_OPEN_READWRITE: c_int = 0x0000_0002;
pub const SQLITE_OPEN_CREATE: c_int = 0x0000_0004;
pub const SQLITE_OPEN_READONLY: c_int = 0x0000_0001;
pub const SQLITE_OPEN_FULLMUTEX: c_int = 0x0001_0000;

// Column types. Only NULL is branched on so far; the others are here
// because a partial list of an enum is worse than a complete one.
#[allow(dead_code)]
pub const SQLITE_INTEGER: c_int = 1;
#[allow(dead_code)]
pub const SQLITE_TEXT: c_int = 3;
pub const SQLITE_NULL: c_int = 5;

/// Configuration verbs for `sqlite3_db_config`.
///
/// Apple builds with `SQLITE_DQS=3`, which makes a double-quoted string fall
/// back to a string literal when it does not resolve as an identifier. That
/// turns a typo'd column name into a silent literal instead of an error, so
/// both of these get switched off at open.
pub const SQLITE_DBCONFIG_DQS_DML: c_int = 1013;
pub const SQLITE_DBCONFIG_DQS_DDL: c_int = 1014;

/// Tells SQLite to copy a bound string rather than borrow it.
///
/// The sentinel is `(void*)-1`, not null. Passing null means
/// `SQLITE_STATIC`, under which SQLite keeps the caller's pointer and reads
/// it after the call returns: a use-after-free waiting for the first
/// temporary `String`.
pub const SQLITE_TRANSIENT: *const c_void = usize::MAX as *const c_void;

#[link(name = "sqlite3")]
unsafe extern "C" {
    pub fn sqlite3_libversion_number() -> c_int;

    pub fn sqlite3_open_v2(
        filename: *const c_char,
        db: *mut *mut Sqlite3,
        flags: c_int,
        vfs: *const c_char,
    ) -> c_int;
    pub fn sqlite3_close_v2(db: *mut Sqlite3) -> c_int;

    /// Variadic. Rust supports declaring these, which is what lets the DQS
    /// settings above be applied without a shim.
    pub fn sqlite3_db_config(db: *mut Sqlite3, op: c_int, ...) -> c_int;

    pub fn sqlite3_exec(
        db: *mut Sqlite3,
        sql: *const c_char,
        callback: *const c_void,
        arg: *mut c_void,
        errmsg: *mut *mut c_char,
    ) -> c_int;

    pub fn sqlite3_prepare_v3(
        db: *mut Sqlite3,
        sql: *const c_char,
        n_byte: c_int,
        prep_flags: u32,
        stmt: *mut *mut Stmt,
        tail: *mut *const c_char,
    ) -> c_int;
    pub fn sqlite3_finalize(stmt: *mut Stmt) -> c_int;
    pub fn sqlite3_step(stmt: *mut Stmt) -> c_int;
    /// Apple builds with `SQLITE_OMIT_AUTORESET`, so a statement that stops
    /// on anything other than DONE must be reset explicitly before reuse.
    pub fn sqlite3_reset(stmt: *mut Stmt) -> c_int;
    pub fn sqlite3_clear_bindings(stmt: *mut Stmt) -> c_int;

    pub fn sqlite3_bind_int64(stmt: *mut Stmt, index: c_int, value: i64) -> c_int;
    pub fn sqlite3_bind_text(
        stmt: *mut Stmt,
        index: c_int,
        text: *const c_char,
        n_byte: c_int,
        destructor: *const c_void,
    ) -> c_int;
    pub fn sqlite3_bind_null(stmt: *mut Stmt, index: c_int) -> c_int;

    pub fn sqlite3_column_count(stmt: *mut Stmt) -> c_int;
    pub fn sqlite3_column_type(stmt: *mut Stmt, col: c_int) -> c_int;
    pub fn sqlite3_column_int64(stmt: *mut Stmt, col: c_int) -> i64;
    /// Must be called before `sqlite3_column_bytes` for the length to match
    /// the UTF-8 encoding rather than some other representation.
    pub fn sqlite3_column_text(stmt: *mut Stmt, col: c_int) -> *const u8;
    pub fn sqlite3_column_bytes(stmt: *mut Stmt, col: c_int) -> c_int;

    pub fn sqlite3_last_insert_rowid(db: *mut Sqlite3) -> i64;
    pub fn sqlite3_changes(db: *mut Sqlite3) -> c_int;
    pub fn sqlite3_errmsg(db: *mut Sqlite3) -> *const c_char;
    pub fn sqlite3_extended_errcode(db: *mut Sqlite3) -> c_int;
    pub fn sqlite3_busy_timeout(db: *mut Sqlite3, ms: c_int) -> c_int;
    pub fn sqlite3_interrupt(db: *mut Sqlite3);
    pub fn sqlite3_free(ptr: *mut c_void);
}
