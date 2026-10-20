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
        let mut out = String::new();
        for piece in self.pieces_between(start, end) {
            if piece.last {
                out.push_str(piece.text.trim_end());
            } else if piece.wrapped {
                out.push_str(&piece.text);
            } else {
                out.push_str(piece.text.trim_end());
                out.push('\n');
            }
        }
        out
    }
    /// The text from `start` to `end` as one line, for pasting a command a
    /// program broke over several rows itself (Claude Code does, with hard
    /// newlines and indentation): lines the terminal wrapped are joined
    /// first, then [`join_rows`] joins the rest.
    pub fn one_line_between(&self, start: (u64, usize), end: (u64, usize)) -> String {
        let mut rows: Vec<(String, bool)> = Vec::new();
        let mut open = false;
        for piece in self.pieces_between(start, end) {
            if !open {
                rows.push((String::new(), false));
            }
            let row = rows.last_mut().expect("pushed above");
            row.0.push_str(&piece.text);
            row.1 = piece.full;
            open = piece.wrapped && !piece.last;
        }
        join_rows(&rows, self.cols())
    }
    /// The selected part of each line from `start` to `end`, in order.
    fn pieces_between(&self, start: (u64, usize), end: (u64, usize)) -> Vec<Piece> {
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let mut pieces = Vec::new();
        for number in start.0..=end.0 {
            let Some(cells) = self.line(number) else {
                continue;
            };
            let from = if number == start.0 { start.1 } else { 0 };
            let to = if number == end.0 { end.1 } else { cells.len() };
            let to_end = to >= cells.len();
            pieces.push(Piece {
                text: cells_text(&cells[from.min(cells.len())..to.min(cells.len())]),
                wrapped: to_end && cells.last().is_some_and(|c| c.flags & WRAPPED != 0),
                full: to_end
                    && cells
                        .last()
                        .is_some_and(|c| c.ch != ' ' || c.flags & WIDE_TAIL != 0),
                last: number == end.0,
            });
        }
        pieces
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
    /// What Find searches: the history and the screen as one text (the
    /// screen alone while a full-screen program shows), lines the terminal
    /// wrapped joined, the rest each ending in a newline without their
    /// trailing blanks.
    pub fn haystack(&self) -> Haystack {
        let first = if self.alternate {
            self.screen_base()
        } else {
            self.dropped
        };
        let last = self.screen_base() + self.rows as u64;
        let mut hay = Haystack::default();
        for number in first..last {
            let Some(cells) = self.line(number) else {
                continue;
            };
            let wrapped = cells.last().is_some_and(|c| c.flags & WRAPPED != 0);
            let end = if wrapped {
                cells.len()
            } else {
                cells
                    .iter()
                    .rposition(|c| c.ch != ' ' || c.flags & WIDE_TAIL != 0)
                    .map_or(0, |i| i + 1)
            };
            let mut row = HayRow {
                line: number,
                start: hay.text.len(),
                end: 0,
                columns: Vec::new(),
            };
            for (col, cell) in cells[..end].iter().enumerate() {
                if cell.flags & WIDE_TAIL != 0 {
                    // The left half's character spans this column too.
                    if let Some(last) = row.columns.last_mut() {
                        last.1 = col as u16 + 1;
                    }
                    continue;
                }
                hay.text.push(cell.ch);
                row.columns.push((col as u16, col as u16 + 1));
            }
            row.end = hay.text.len();
            hay.rows.push(row);
            if !wrapped {
                hay.text.push('\n');
            }
        }
        hay
    }

    /// The link under column `col` of line `line`, for a Command-click:
    /// the one the program marked there with OSC 8, or else a web address
    /// in the text, either found whole across lines the terminal wrapped.
    pub fn link_at(&self, line: u64, col: usize) -> Option<Link> {
        let chars = self.logical_line(line);
        let at = chars
            .iter()
            .position(|c| c.line == line && (c.from..c.to).contains(&col))?;
        let (from, to, url) = match chars[at].link {
            0 => {
                let text: Vec<char> = chars.iter().map(|c| c.ch).collect();
                let (from, to) = url_span(&text, at)?;
                (from, to, text[from..to].iter().collect())
            }
            id => {
                let (from, to) = span_while(chars.len(), at, |i| chars[i].link == id);
                (from, to, self.link_url(id)?.to_owned())
            }
        };
        let mut spans: Vec<(u64, usize, usize)> = Vec::new();
        for c in &chars[from..to] {
            match spans.last_mut() {
                Some(span) if span.0 == c.line => span.2 = c.to,
                _ => spans.push((c.line, c.from, c.to)),
            }
        }
        Some(Link { url, spans })
    }

    /// The characters of line `line` joined with the lines the terminal
    /// wrapped it from and onto, a few rows each way at most, so a long
    /// wrapped line is no slower than a short one.
    fn logical_line(&self, line: u64) -> Vec<LineChar> {
        const REACH: u64 = 16;
        let wrapped = |n: u64| {
            self.line(n)
                .and_then(|cells| cells.last())
                .is_some_and(|c| c.flags & WRAPPED != 0)
        };
        let mut first = line;
        while line - first < REACH && first > 0 && wrapped(first - 1) {
            first -= 1;
        }
        let mut last = line;
        while last - line < REACH && wrapped(last) {
            last += 1;
        }
        let mut chars: Vec<LineChar> = Vec::new();
        for number in first..=last {
            let Some(cells) = self.line(number) else {
                continue;
            };
            for (col, cell) in cells.iter().enumerate() {
                if cell.flags & WIDE_TAIL != 0 {
                    // The left half's character spans this column too.
                    if let Some(c) = chars.last_mut().filter(|c| c.line == number) {
                        c.to = col + 1;
                    }
                    continue;
                }
                chars.push(LineChar {
                    ch: cell.ch,
                    line: number,
                    from: col,
                    to: col + 1,
                    link: cell.link,
                });
            }
        }
        chars
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

/// A terminal's text for a search, from [`Term::haystack`], with where on
/// screen each character of it is.
#[derive(Default)]
pub struct Haystack {
    pub text: String,
    rows: Vec<HayRow>,
}

/// One line of a [`Haystack`]: its number, the bytes of its text, and
/// the columns `[from, to)` each of its characters covers.
struct HayRow {
    line: u64,
    start: usize,
    end: usize,
    columns: Vec<(u16, u16)>,
}

impl Haystack {
    /// The bytes `range` of the text as a line number and columns
    /// `[from, to)` per line it covers: more than one for a match across a
    /// wrap. Nothing for a range that holds no character.
    pub fn spans(&self, range: std::ops::Range<usize>) -> Vec<(u64, usize, usize)> {
        let first = self.rows.partition_point(|r| r.end <= range.start);
        let mut spans = Vec::new();
        for row in &self.rows[first..] {
            if row.start >= range.end {
                break;
            }
            let mut cover: Option<(usize, usize)> = None;
            let text = &self.text[row.start..row.end];
            for ((at, _), (from, to)) in text.char_indices().zip(&row.columns) {
                let at = row.start + at;
                if at >= range.end {
                    break;
                }
                if at >= range.start {
                    let from = cover.map_or(*from as usize, |(f, _)| f);
                    cover = Some((from, *to as usize));
                }
            }
            if let Some((from, to)) = cover {
                spans.push((row.line, from, to));
            }
        }
        spans
    }

    /// The number of the first line it holds.
    pub fn first_line(&self) -> Option<u64> {
        self.rows.first().map(|row| row.line)
    }

    /// The text of the line that line `line` is part of, with the rows the
    /// terminal wrapped it over joined, and the byte in it where column
    /// `col` of `line` is. For a match's snippet.
    pub fn row_text(&self, line: u64, col: usize) -> Option<(&str, usize)> {
        let row = &self.rows[self.rows.binary_search_by_key(&line, |r| r.line).ok()?];
        let text = &self.text[row.start..row.end];
        let at = row.start
            + text
                .char_indices()
                .zip(&row.columns)
                .find(|(_, (_, to))| *to as usize > col)
                .map_or(text.len(), |((at, _), _)| at);
        let start = self.text[..at].rfind('\n').map_or(0, |i| i + 1);
        let end = self.text[at..]
            .find('\n')
            .map_or(self.text.len(), |i| at + i);
        Some((&self.text[start..end], at - start))
    }
}

/// A link on screen, from [`Term::link_at`]: where it goes, and the
/// columns `[from, to)` it covers on each line it is on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub url: String,
    pub spans: Vec<(u64, usize, usize)>,
}

/// A character of a line, from [`Term::logical_line`]: its line, the
/// columns it covers and its OSC 8 link.
struct LineChar {
    ch: char,
    line: u64,
    from: usize,
    to: usize,
    link: u16,
}

/// One line's part of a selection.
struct Piece {
    text: String,
    /// The terminal wrapped it onto the next line.
    wrapped: bool,
    /// Its last column holds a character.
    full: bool,
    /// The selection's last line.
    last: bool,
}

/// Rows of text as one line: each `(text, full)`, where `full` says the
/// row's last column holds a character. Rows lose their indentation and
/// trailing blanks, empty ones go, and the rest are joined with a space,
/// except after a full row whose last word and the next row's first word
/// would not have fitted on one line together: a word too long for a line,
/// split at the edge, is glued back.
pub fn join_rows(rows: &[(String, bool)], cols: usize) -> String {
    let width = |s: &str| s.chars().map(display_width).sum::<usize>();
    let mut out = String::new();
    let mut last_full = false;
    for (text, full) in rows {
        let text = text.trim_end();
        let trimmed = text.trim_start();
        if trimmed.is_empty() {
            last_full = false;
            continue;
        }
        if !out.is_empty() {
            let indent = width(&text[..text.len() - trimmed.len()]);
            let before = out.rsplit(' ').next().map_or(0, width);
            let after = trimmed.split(' ').next().map_or(0, width);
            if !(last_full && before + after > cols.saturating_sub(indent)) {
                out.push(' ');
            }
        }
        out.push_str(trimmed);
        last_full = *full;
    }
    out
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
/// The web address (`http://` or `https://`) under character `index` of
/// `text`, as linkifiers find one: punctuation that ends a sentence after
/// it is not part of it, nor is a closing bracket or quote it did not open,
/// so `(see https://example.com/a_(b))` gives `https://example.com/a_(b)`.
pub fn url_at(text: &[char], index: usize) -> Option<String> {
    let (from, to) = url_span(text, index)?;
    Some(text[from..to].iter().collect())
}

/// The characters `[from, to)` of the address [`url_at`] finds.
fn url_span(text: &[char], index: usize) -> Option<(usize, usize)> {
    let allowed = |c: char| {
        (c.is_ascii_graphic() && !"\"<>`{}|\\^".contains(c))
            || (!c.is_ascii() && c.is_alphanumeric())
    };
    if index >= text.len() || !allowed(text[index]) {
        return None;
    }
    let (start, end) = span_while(text.len(), index, |i| allowed(text[i]));
    let scheme_at = |i: usize| {
        ["https://", "http://"].into_iter().find_map(|scheme| {
            let found = text.get(i..i + scheme.len())?;
            found
                .iter()
                .zip(scheme.chars())
                .all(|(a, b)| a.to_ascii_lowercase() == b)
                .then_some(scheme.len())
        })
    };
    // The nearest address starting at or before the click; another one
    // right after it in the same run ends it.
    let (from, scheme) = (start..=index)
        .rev()
        .find_map(|i| scheme_at(i).map(|len| (i, len)))?;
    let mut to = (from + 1..end)
        .find(|&i| scheme_at(i).is_some())
        .unwrap_or(end);
    let count = |range: &[char], c: char| range.iter().filter(|&&x| x == c).count();
    while to > from + scheme {
        let url = &text[from..to];
        let last = url[url.len() - 1];
        let unbalanced = match last {
            '.' | ',' | ';' | ':' | '!' | '?' => true,
            ')' => count(url, ')') > count(url, '('),
            ']' => count(url, ']') > count(url, '['),
            '\'' => count(url, '\'') % 2 == 1,
            _ => false,
        };
        if !unbalanced {
            break;
        }
        to -= 1;
    }
    (to > from + scheme && index < to).then_some((from, to))
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
