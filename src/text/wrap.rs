//! Soft wrap: where each line breaks into screen rows.
//!
//! A pure function of the text and a width in columns, so the buffer can
//! move and scroll by rows without asking the renderer, and the renderer,
//! hit testing and the caret all agree on the same breaks. Widths are the
//! same cells the character renderer uses (`columns::advance`); a shaped
//! line is drawn inside the row its columns say it belongs to.
//!
//! A row breaks after the last space or tab that fits, or mid-word when a
//! word is wider than the row. Whitespace at a break stays at the end of
//! the row it follows, so a caret after it is still on that row.

use super::columns;
use super::rope::Rope;

/// Lines longer than this are not wrapped: walking them every frame would
/// cost more than the frame. They keep horizontal scrolling.
pub const MAX_WRAP_BYTES: usize = 256 * 1024;

/// The narrowest wrap that is still readable.
pub const MIN_COLUMNS: usize = 20;

/// Byte offsets where each row of `line` starts. The first is the line's
/// start; a line that fits, or is too long to wrap, has only that one.
pub fn row_starts(rope: &Rope, line: usize, columns: usize) -> Vec<usize> {
    let start = rope.line_to_byte(line);
    let end = line_end(rope, line);
    let mut rows = vec![start];
    if end - start <= columns || end - start > MAX_WRAP_BYTES {
        // Fewer bytes than columns always fits: no character is narrower
        // than one byte is long, and a tab is the only thing wider than its
        // bytes. Check tabs the long way.
        if end - start > MAX_WRAP_BYTES || !rope.slice_to_string(start..end).contains('\t') {
            return rows;
        }
    }
    let columns = columns.max(MIN_COLUMNS);
    let mut column = 0usize;
    // The last place a row could break: just after whitespace, and the
    // column there.
    let mut after_space: Option<(usize, usize)> = None;
    let mut byte = start;
    for chunk in rope.chunks_in(start..end) {
        for ch in chunk.chars() {
            let next = columns::advance(column, ch);
            // Whitespace hangs past the edge instead of taking the word
            // before it down a row; the row breaks after it.
            let space = ch == ' ' || ch == '\t';
            if next > columns && column > 0 && !space {
                match after_space.filter(|(at, _)| *at > *rows.last().unwrap_or(&start)) {
                    Some((at, at_column)) => {
                        rows.push(at);
                        column -= at_column;
                    }
                    None => {
                        rows.push(byte);
                        column = 0;
                    }
                }
                after_space = None;
            }
            column = columns::advance(column, ch);
            byte += ch.len_utf8();
            if ch == ' ' || ch == '\t' {
                after_space = Some((byte, column));
            }
        }
    }
    rows
}

/// How many rows `line` takes.
pub fn row_count(rope: &Rope, line: usize, columns: usize) -> usize {
    row_starts(rope, line, columns).len()
}

/// Which row of `rows` holds `byte`. A byte on a break belongs to the row
/// it starts.
pub fn row_of(rows: &[usize], byte: usize) -> usize {
    rows.partition_point(|&start| start <= byte)
        .saturating_sub(1)
}

/// The end of `line`, before its newline.
pub fn line_end(rope: &Rope, line: usize) -> usize {
    if line + 1 < rope.len_lines() {
        let next = rope.line_to_byte(line + 1);
        let mut end = next - 1;
        if end > rope.line_to_byte(line) && rope.byte_at(end - 1) == Some(b'\r') {
            end -= 1;
        }
        end
    } else {
        rope.len_bytes()
    }
}

/// Columns from `from` to `to` on one line.
pub fn columns_between(rope: &Rope, from: usize, to: usize) -> usize {
    let mut column = 0;
    for chunk in rope.chunks_in(from..to) {
        for ch in chunk.chars() {
            column = columns::advance(column, ch);
        }
    }
    column
}

/// Columns from the start of the row that begins at `row_start` to `byte`.
/// Measured from the line's start, so a tab stops where it would on the
/// whole line, which is where the breaks were computed.
pub fn column_in_row(rope: &Rope, row_start: usize, byte: usize) -> usize {
    let line_start = rope.line_to_byte(rope.byte_to_line(row_start));
    columns_between(rope, line_start, byte) - columns_between(rope, line_start, row_start)
}

/// The byte on the row `row_start..row_end` closest to `column` columns
/// from the row's start, measured as [`column_in_row`] does.
pub fn byte_at_column(rope: &Rope, row_start: usize, row_end: usize, column: usize) -> usize {
    let line_start = rope.line_to_byte(rope.byte_to_line(row_start));
    let base = columns_between(rope, line_start, row_start);
    let column = column + base;
    let mut at = base;
    let mut byte = row_start;
    for chunk in rope.chunks_in(row_start..row_end) {
        for ch in chunk.chars() {
            let next = columns::advance(at, ch);
            if next > column {
                // Past the middle of the character: after it.
                return if column - at > (next - at) / 2 {
                    byte + ch.len_utf8()
                } else {
                    byte
                };
            }
            at = next;
            byte += ch.len_utf8();
        }
    }
    row_end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &str, columns: usize) -> Vec<String> {
        let rope = Rope::from_text(text);
        let starts = row_starts(&rope, 0, columns);
        let end = line_end(&rope, 0);
        starts
            .iter()
            .enumerate()
            .map(|(i, &s)| rope.slice_to_string(s..*starts.get(i + 1).unwrap_or(&end)))
            .collect()
    }

    #[test]
    fn breaks_after_the_last_space_that_fits() {
        assert_eq!(
            rows("the quick brown fox jumps over the lazy dog", 20),
            ["the quick brown fox ", "jumps over the lazy ", "dog"]
        );
    }

    #[test]
    fn a_word_that_ends_on_the_edge_stays_and_its_space_hangs() {
        // Twenty columns exactly, then a space, then more.
        let text = format!("{} {} tu", "a".repeat(10), "b".repeat(9));
        assert_eq!(
            rows(&text, 20),
            [
                format!("{} {} ", "a".repeat(10), "b".repeat(9)),
                "tu".into()
            ]
        );
        assert_eq!(
            rows(&format!("{} {}", "a".repeat(20), "b"), 20),
            ["a".repeat(20) + " ", "b".into()]
        );
    }

    #[test]
    fn a_word_wider_than_the_row_breaks_mid_word() {
        let long = "x".repeat(45);
        assert_eq!(
            rows(&long, 20),
            ["x".repeat(20), "x".repeat(20), "x".repeat(5)]
        );
    }

    #[test]
    fn a_line_that_fits_is_one_row() {
        assert_eq!(rows("short", 20), ["short"]);
        let rope = Rope::from_text("a\nb c\n");
        assert_eq!(row_starts(&rope, 1, 20), [2]);
    }

    #[test]
    fn tabs_and_wide_characters_count_their_cells() {
        // Ten CJK characters are twenty columns.
        let got = rows(&"漢".repeat(12), 20);
        assert_eq!(got, ["漢".repeat(10), "漢".repeat(2)]);
        // A tab is four columns here, and a place to break after.
        let got = rows("ab\tcdefghijklmnopqrstu", 20);
        assert_eq!(got, ["ab\t", "cdefghijklmnopqrstu"]);
    }

    #[test]
    fn row_of_puts_a_break_on_the_next_row() {
        let starts = [0, 20, 40];
        assert_eq!(row_of(&starts, 0), 0);
        assert_eq!(row_of(&starts, 19), 0);
        assert_eq!(row_of(&starts, 20), 1);
        assert_eq!(row_of(&starts, 45), 2);
    }

    #[test]
    fn byte_at_column_rounds_to_the_nearer_edge() {
        let rope = Rope::from_text("abcdef");
        assert_eq!(byte_at_column(&rope, 0, 6, 2), 2);
        assert_eq!(byte_at_column(&rope, 0, 6, 99), 6);
        assert_eq!(columns_between(&rope, 0, 4), 4);
    }

    #[test]
    fn very_long_lines_are_left_alone() {
        let rope = Rope::from_text(&"word ".repeat(MAX_WRAP_BYTES / 4));
        assert_eq!(row_starts(&rope, 0, 80).len(), 1);
    }
}
