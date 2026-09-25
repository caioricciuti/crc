//! The project index: a queryable SQLite database of symbols and references.
//!
//! This is bet #1 from the README. The claim is that a code editor's index
//! should be a database rather than a bespoke in-process structure, so that
//! "find all callers" is a query anyone can run and an external tool or an
//! agent can read it without asking the editor's permission.
//!
//! Zero new crates: macOS ships libsqlite3 and the linker finds it through
//! the SDK stub. See `ffi` for why that matters beyond the crate count.

pub mod db;
mod ffi;
pub mod store;
