//! Text storage: the rope, the editable buffer, and the open document set.

pub mod buffer;
pub mod columns;
pub mod documents;
pub mod file_format;
mod grapheme;
pub mod indent;
pub mod rope;
pub mod wrap;
