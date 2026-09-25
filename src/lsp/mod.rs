//! Language servers: completion, diagnostics, go to definition and hover
//! from the tools that already know each language.
//!
//! One server per language per project root, started the first time a file
//! of that language is opened and stopped when the editor quits. Each is a
//! child process speaking JSON-RPC over its stdio ([`transport`]); the
//! conversation itself is in [`client`]. The servers are found on disk, not
//! in the shell: an app launched from the Dock has no profile, so the usual
//! install locations are searched directly ([`servers`]).
//!
//! Positions on the wire are lines and UTF-16 units; the editor thinks in
//! bytes. The two conversions live here and nowhere else.

pub mod client;
pub mod servers;
pub mod transport;

use std::path::{Path, PathBuf};

use crate::text::rope::Rope;

/// A line and a UTF-16 column, as the protocol counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// Where a byte offset in `rope` is, in protocol terms.
pub fn position_of(rope: &Rope, byte: usize) -> Position {
    let byte = byte.min(rope.len_bytes());
    let line = rope.byte_to_line(byte);
    let start = rope.line_to_byte(line);
    let character = rope.slice_to_string(start..byte).encode_utf16().count();
    Position {
        line: line as u32,
        character: character as u32,
    }
}

/// The byte offset of a protocol position, clamped to the text. A column
/// past the end of its line lands at the end of that line, which is what
/// every server means by it.
pub fn offset_of(rope: &Rope, position: Position) -> usize {
    let lines = rope.len_lines();
    if lines == 0 {
        return 0;
    }
    let line = (position.line as usize).min(lines - 1);
    let start = rope.line_to_byte(line);
    let text = rope.line(line);
    let content = text.trim_end_matches('\n').trim_end_matches('\r');
    let mut units = 0u32;
    for (index, ch) in content.char_indices() {
        if units >= position.character {
            return start + index;
        }
        units += ch.len_utf16() as u32;
    }
    start + content.len()
}

/// A `file://` URI for `path`, percent-encoding what a URI cannot carry.
pub fn uri_for(path: &Path) -> String {
    let mut out = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The path of a `file://` URI, or `None` for any other scheme.
pub fn path_for(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // A host part (file://localhost/...) is rare and dropped.
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let mut bytes = Vec::with_capacity(rest.len());
    let raw = rest.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%'
            && i + 2 < raw.len()
            && let Ok(hex) = std::str::from_utf8(&raw[i + 1..i + 3])
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            bytes.push(byte);
            i += 3;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(String::from_utf8_lossy(&bytes).into_owned()))
}

/// A problem a server reported in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub start: Position,
    pub end: Position,
    pub severity: Severity,
    pub message: String,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Hint,
    Information,
    Warning,
    Error,
}

impl Severity {
    fn from_wire(value: Option<u64>) -> Severity {
        match value {
            Some(1) => Severity::Error,
            Some(2) => Severity::Warning,
            Some(3) => Severity::Information,
            Some(4) => Severity::Hint,
            _ => Severity::Error,
        }
    }
}

/// One completion the server offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub label: String,
    pub detail: Option<String>,
    /// What the label means: function, variable, keyword and so on, as the
    /// protocol numbers them.
    pub kind: u64,
    /// The text to insert and the range it replaces, when the server gave
    /// one; otherwise the text replaces the word before the caret.
    pub edit: Option<(Position, Position, String)>,
    pub insert_text: String,
    pub sort_text: String,
    pub filter_text: String,
}

/// Somewhere in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub path: PathBuf,
    pub start: Position,
    pub end: Position,
}

/// Replace the text between `start` and `end` with `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub start: Position,
    pub end: Position,
    pub text: String,
}

/// Applies `edits` to `rope`'s text as the protocol means them: every
/// position refers to the text before any edit. Returns the new text, or
/// `None` when two edits overlap.
pub fn apply_edits(rope: &Rope, edits: &[TextEdit]) -> Option<String> {
    let text = rope.to_string();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (range, insert) in edit_ranges(rope, edits)? {
        out.push_str(&text[at..range.start]);
        out.push_str(&insert);
        at = range.end;
    }
    out.push_str(&text[at..]);
    Some(out)
}

/// `edits` as byte ranges of `rope`, in order, for `Buffer::replace_ranges`.
/// `None` when two overlap.
pub fn edit_ranges(
    rope: &Rope,
    edits: &[TextEdit],
) -> Option<Vec<(std::ops::Range<usize>, String)>> {
    let mut ranges: Vec<(std::ops::Range<usize>, String)> = edits
        .iter()
        .map(|e| {
            (
                offset_of(rope, e.start)..offset_of(rope, e.end),
                e.text.clone(),
            )
        })
        .collect();
    // Stable, so inserts at one offset keep the server's order.
    ranges.sort_by_key(|(r, _)| (r.start, r.end));
    let mut at = 0;
    for (range, _) in &ranges {
        if range.start < at || range.end < range.start {
            return None;
        }
        at = range.end;
    }
    Some(ranges)
}

/// A signature the server is helping with, and which parameter the caret
/// is in, as a byte range of `label`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub label: String,
    pub active: Option<std::ops::Range<usize>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_count_utf16_units_and_clamp() {
        let rope = Rope::from_text("ab\né😀x\n");
        assert_eq!(
            position_of(&rope, 0),
            Position {
                line: 0,
                character: 0
            }
        );
        assert_eq!(
            position_of(&rope, 2),
            Position {
                line: 0,
                character: 2
            }
        );
        // "é" is one unit, "😀" is two.
        let x = "ab\né😀".len();
        assert_eq!(
            position_of(&rope, x),
            Position {
                line: 1,
                character: 3
            }
        );
        assert_eq!(
            offset_of(
                &rope,
                Position {
                    line: 1,
                    character: 3
                }
            ),
            x
        );
        assert_eq!(
            offset_of(
                &rope,
                Position {
                    line: 1,
                    character: 99
                }
            ),
            "ab\né😀x".len()
        );
        assert_eq!(
            offset_of(
                &rope,
                Position {
                    line: 99,
                    character: 0
                }
            ),
            rope.line_to_byte(rope.len_lines() - 1)
        );
        assert_eq!(position_of(&rope, 9999).line as usize, rope.len_lines() - 1);
    }

    #[test]
    fn edits_apply_against_the_original_text() {
        let rope = Rope::from_text("let a = a + 1;\nuse(a);\n");
        let at = |line, character| Position { line, character };
        let edit = |start, end, text: &str| TextEdit {
            start,
            end,
            text: text.into(),
        };
        // Given out of order, as servers do.
        let edits = [
            edit(at(1, 4), at(1, 5), "total"),
            edit(at(0, 4), at(0, 5), "total"),
            edit(at(0, 8), at(0, 9), "total"),
        ];
        assert_eq!(
            apply_edits(&rope, &edits).as_deref(),
            Some("let total = total + 1;\nuse(total);\n")
        );
        let overlapping = [edit(at(0, 0), at(0, 5), "x"), edit(at(0, 3), at(0, 6), "y")];
        assert_eq!(apply_edits(&rope, &overlapping), None);
    }

    #[test]
    fn uris_round_trip_with_spaces_and_unicode() {
        let path = Path::new("/Users/me/my project/café.rs");
        let uri = uri_for(path);
        assert_eq!(uri, "file:///Users/me/my%20project/caf%C3%A9.rs");
        assert_eq!(path_for(&uri).as_deref(), Some(path));
        assert_eq!(path_for("https://example.com/x"), None);
    }
}
