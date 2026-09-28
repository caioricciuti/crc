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
    // Columns from the line's start, so a tab stops where it would on the
    // whole line, as `column_in_row` and `byte_at_column` measure; a row's
    // width is measured from the column it starts at.
    let mut column = 0usize;
    let mut row_base = 0usize;
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
            if next - row_base > columns && column > row_base && !space {
                match after_space.filter(|(at, _)| *at > *rows.last().unwrap_or(&start)) {
                    Some((at, at_column)) => {
                        rows.push(at);
                        row_base = at_column;
                    }
                    None => {
                        rows.push(byte);
                        row_base = column;
                    }
                }
                after_space = None;
            }
            column = next;
            byte += ch.len_utf8();
            if space {
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

/// Columns from `from` to `to` on one line, counting from column zero at
/// `from`.
pub fn columns_between(rope: &Rope, from: usize, to: usize) -> usize {
    rope.visual_column(from..to)
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
    byte_at_fraction(rope, row_start, row_end, column as f32)
}

/// [`byte_at_column`] for a column with a fraction, as a click gives: at or
/// past the middle of a character is after it. The one rule every click
/// and every row motion uses, so a click lands on the same side with
/// wrapping on or off.
pub fn byte_at_fraction(rope: &Rope, row_start: usize, row_end: usize, column: f32) -> usize {
    let line_start = rope.line_to_byte(rope.byte_to_line(row_start));
    let base = columns_between(rope, line_start, row_start);
    let column = column.max(0.0) + base as f32;
    // The character that holds the column, found through the rope's
    // summaries rather than by walking the row.
    let (byte, at) = rope.visual_seek(line_start..row_end, column as usize);
    let byte = byte.max(row_start);
    let Some(ch) = rope
        .chunks_in(byte..row_end)
        .next()
        .and_then(|s| s.chars().next())
    else {
        return row_end;
    };
    if ch == '\r' || ch == '\n' {
        return byte;
    }
    if after_middle(column, at, columns::advance(at, ch)) {
        byte + ch.len_utf8()
    } else {
        byte
    }
}

/// Whether `column` is at or past the middle of a character spanning the
/// columns `at..next`.
pub fn after_middle(column: f32, at: usize, next: usize) -> bool {
    column * 2.0 >= (at + next) as f32
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
    fn rows_never_run_past_the_width_with_tabs_after_a_break() {
        // A tab after a break stops where it does on the whole line, and the
        // row's columns, measured the same way, stay within the width.
        for extra in 0..6 {
            let text = format!(
                "{} bb\tcccccccccccccc{} end",
                "a".repeat(18),
                "c".repeat(extra)
            );
            let rope = Rope::from_text(&text);
            let starts = row_starts(&rope, 0, 20);
            let end = line_end(&rope, 0);
            for (i, &row) in starts.iter().enumerate() {
                let row_end = starts.get(i + 1).copied().unwrap_or(end);
                let last = if row_end > row {
                    let before = rope.slice_to_string(row..row_end);
                    let trimmed = before.trim_end_matches([' ', '\t']);
                    row + trimmed.len()
                } else {
                    row
                };
                assert!(
                    column_in_row(&rope, row, last) <= 20,
                    "{text:?} row {i} is {} wide",
                    column_in_row(&rope, row, last)
                );
            }
        }
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
    fn byte_at_fraction_matches_a_walk_of_the_row() {
        // The seek through rope summaries against the plain walk it
        // replaced, over tabs, wide and zero-width characters, many leaves,
        // and rows starting mid-line.
        let alphabet = ['a', '\t', '世', ' ', 'é', '\u{301}', '😀'];
        let mut seed = 0x2545_f491_u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as usize
        };
        let text: String = (0..6000)
            .map(|_| alphabet[next() % alphabet.len()])
            .collect();
        let rope = Rope::from_text(&text);
        let end = line_end(&rope, 0);
        let walk = |row_start: usize, column: f32| {
            let base = columns_between(&rope, 0, row_start);
            let column = column + base as f32;
            let mut at = base;
            let mut byte = row_start;
            for ch in text[row_start..end].chars() {
                let next = columns::advance(at, ch);
                if next as f32 > column {
                    return if after_middle(column, at, next) {
                        byte + ch.len_utf8()
                    } else {
                        byte
                    };
                }
                at = next;
                byte += ch.len_utf8();
            }
            end
        };
        for _ in 0..400 {
            let mut row_start = next() % end;
            while !text.is_char_boundary(row_start) {
                row_start -= 1;
            }
            let column = (next() % 2000) as f32 / 4.0;
            assert_eq!(
                byte_at_fraction(&rope, row_start, end, column),
                walk(row_start, column),
                "row at {row_start}, column {column}"
            );
        }
    }

    #[test]
    fn very_long_lines_are_left_alone() {
        let rope = Rope::from_text(&"word ".repeat(MAX_WRAP_BYTES / 4));
        assert_eq!(row_starts(&rope, 0, 80).len(), 1);
    }
}
