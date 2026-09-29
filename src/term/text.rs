//! Text read back off the screen: selections, words and paths under a
//! click, and the xterm palette.

use super::*;

impl Term {
    /// The screen as text, trailing blanks trimmed. For tests and dumps.
    pub fn screen_text(&self) -> String {
        let lines: Vec<String> = self
            .screen
            .iter()
            .map(|row| cells_text(row).trim_end().to_owned())
            .collect();
        let end = lines
            .iter()
            .rposition(|l| !l.is_empty())
            .map_or(0, |i| i + 1);
        lines[..end].join("\n")
    }
    /// The text from `start` to `end`, each a line number and a column
    /// boundary, in either order. Lines the terminal wrapped are joined;
    /// others end in a newline, without their trailing blanks.
    pub fn text_between(&self, start: (u64, usize), end: (u64, usize)) -> String {
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let mut out = String::new();
        for number in start.0..=end.0 {
            let Some(cells) = self.line(number) else {
                continue;
            };
            let from = if number == start.0 { start.1 } else { 0 };
            let to = if number == end.0 { end.1 } else { cells.len() };
            let piece = cells_text(&cells[from.min(cells.len())..to.min(cells.len())]);
            let wrapped = to >= cells.len() && cells.last().is_some_and(|c| c.flags & WRAPPED != 0);
            if number == end.0 || wrapped {
                out.push_str(if number == end.0 {
                    piece.trim_end()
                } else {
                    &piece
                });
            } else {
                out.push_str(piece.trim_end());
                out.push('\n');
            }
        }
        out
    }
    /// The columns `[from, to)` of the word or path under column `col`, for
    /// a double click: letters, digits and the characters paths and
    /// addresses are made of. Any other character is a word of one.
    pub fn word_at(&self, line: u64, col: usize) -> Option<(usize, usize)> {
        let cells = self.line(line)?;
        let col = col.min(cells.len().checked_sub(1)?);
        let col = if cells[col].flags & WIDE_TAIL != 0 {
            col.saturating_sub(1)
        } else {
            col
        };
        let wordy = |c: &Cell| c.flags & WIDE_TAIL != 0 || is_word_char(c.ch);
        if !wordy(&cells[col]) {
            return Some((col, col + 1));
        }
        Some(span_while(cells.len(), col, |i| wordy(&cells[i])))
    }
    /// Line `line` as text, with the column each character starts at.
    pub fn line_chars(&self, line: u64) -> Vec<(usize, char)> {
        self.line(line)
            .map(|cells| {
                cells
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.flags & WIDE_TAIL == 0)
                    .map(|(i, c)| (i, c.ch))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The characters of `cells`, a wide character once.
pub(super) fn cells_text(cells: &[Cell]) -> String {
    cells
        .iter()
        .filter(|c| c.flags & WIDE_TAIL == 0)
        .map(|c| c.ch)
        .collect()
}
pub(super) fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || "/._-~:@+#%=?&$".contains(ch)
}
/// A file reference under character `index` of `text`: a path, optionally
/// followed by `:line` and `:column`, as compilers and `claude` print them.
/// Quotes, brackets and trailing punctuation around it are not part of it.
/// The run `[from, to)` around `at`, within `0..len`, of indexes that
/// `keep` accepts. `at` itself is always inside.
pub(super) fn span_while(len: usize, at: usize, keep: impl Fn(usize) -> bool) -> (usize, usize) {
    let from = (0..at).rev().take_while(|&i| keep(i)).last().unwrap_or(at);
    let to = (at..len).take_while(|&i| keep(i)).last().unwrap_or(at) + 1;
    (from, to)
}
pub fn path_at(text: &[char], index: usize) -> Option<(String, Option<usize>, Option<usize>)> {
    let stop = |c: &char| c.is_whitespace() || "\"'`()[]{}<>,;|".contains(*c);
    if index >= text.len() || stop(&text[index]) {
        return None;
    }
    let (from, to) = span_while(text.len(), index, |i| !stop(&text[i]));
    let token: String = text[from..to].iter().collect();
    let token = token.trim_end_matches(['.', ':', '!', '?']);
    let token = token.strip_prefix("file://").unwrap_or(token);
    let mut parts = token.split(':');
    let path = parts.next().filter(|p| !p.is_empty())?;
    if !path.contains('/') && !path.contains('.') {
        return None;
    }
    let line = parts
        .next()
        .and_then(|p| p.parse().ok())
        .filter(|&n: &usize| n > 0);
    let column = line
        .and_then(|_| parts.next())
        .and_then(|p| p.parse().ok())
        .filter(|&n: &usize| n > 0);
    Some((path.to_owned(), line, column))
}
/// The colour of a palette entry, 0 to 255, for the 16 named colours given
/// by the theme and the rest computed as xterm does.
pub fn palette(index: u8, named: &[[f32; 4]; 16]) -> [f32; 4] {
    match index {
        0..=15 => named[index as usize],
        16..=231 => {
            let i = index - 16;
            let level = |v: u8| {
                if v == 0 {
                    0.0
                } else {
                    (55.0 + 40.0 * v as f32) / 255.0
                }
            };
            [level(i / 36), level((i / 6) % 6), level(i % 6), 1.0]
        }
        232..=255 => {
            let v = (8.0 + 10.0 * (index - 232) as f32) / 255.0;
            [v, v, v, 1.0]
        }
    }
}
