//! Merge conflicts as Git writes them into a file: `<<<<<<<`, an optional
//! `|||||||` base section (the `diff3` and `zdiff3` styles), `=======` and
//! `>>>>>>>`. Parsed from the document's own text, so a conflict left by a
//! merge, rebase, cherry-pick or stash pop in a terminal is found the same
//! way as any other, and editing a conflict by hand simply changes what is
//! found next time.
//!
//! Markers must be exactly seven characters at the start of a line. A
//! recursive merge writes its inner conflicts with longer markers, and those
//! belong to the text of a section, not to its structure. A block that never
//! closes is not a conflict: nothing here guesses where it was meant to end.
use crate::text::rope::Rope;
use std::ops::Range;

/// Longest a single conflict may run, in lines, before the opening marker
/// is taken to be text.
const MAX_CONFLICT_LINES: usize = 100_000;
/// More than this and the file is not being merged, it is a test fixture of
/// markers; the first ones are shown.
const MAX_CONFLICTS: usize = 10_000;

/// One conflict: where it is, as bytes and as lines, and its sections.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// From the start of the `<<<<<<<` line to past the `>>>>>>>` line's
    /// newline, or to the end of the text when it has none.
    pub range: Range<usize>,
    /// The sections' text, whole lines, without their markers.
    pub current: Range<usize>,
    pub base: Option<Range<usize>>,
    pub incoming: Range<usize>,
    /// Lines of the markers, 0-based.
    pub start_line: usize,
    pub base_line: Option<usize>,
    pub separator_line: usize,
    pub end_line: usize,
    /// What Git wrote after the markers: `HEAD`, a branch, a commit.
    pub current_label: String,
    pub base_label: Option<String>,
    pub incoming_label: String,
}

/// What to keep of a conflict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Take {
    Current,
    Incoming,
    /// Current, then incoming.
    Both,
    /// The common ancestor, when the markers carry it.
    Base,
}

impl Take {
    pub const ALL: [Take; 4] = [Take::Current, Take::Incoming, Take::Both, Take::Base];

    /// The button's label.
    pub fn label(self) -> &'static str {
        match self {
            Take::Current => "Accept Current",
            Take::Incoming => "Accept Incoming",
            Take::Both => "Accept Both",
            Take::Base => "Accept Base",
        }
    }

    /// The name a script uses, `conflict.<name>.<index>`.
    pub fn name(self) -> &'static str {
        match self {
            Take::Current => "current",
            Take::Incoming => "incoming",
            Take::Both => "both",
            Take::Base => "base",
        }
    }
}

impl Conflict {
    /// Lines of the current section, 0-based, half open.
    pub fn current_lines(&self) -> Range<usize> {
        self.start_line + 1..self.base_line.unwrap_or(self.separator_line)
    }
    pub fn base_lines(&self) -> Option<Range<usize>> {
        self.base_line.map(|b| b + 1..self.separator_line)
    }
    pub fn incoming_lines(&self) -> Range<usize> {
        self.separator_line + 1..self.end_line
    }

    /// Whether `take` makes sense here: only a conflict with a base section
    /// can be resolved to its base.
    pub fn offers(&self, take: Take) -> bool {
        take != Take::Base || self.base.is_some()
    }

    /// The text that replaces [`Conflict::range`] for `take`.
    pub fn resolution(&self, rope: &Rope, take: Take) -> String {
        let text = |r: &Range<usize>| rope.slice_to_string(r.clone());
        match take {
            Take::Current => text(&self.current),
            Take::Incoming => text(&self.incoming),
            Take::Both => {
                let mut both = text(&self.current);
                if !both.is_empty() && !both.ends_with('\n') {
                    both.push('\n');
                }
                both.push_str(&text(&self.incoming));
                both
            }
            Take::Base => self.base.as_ref().map(text).unwrap_or_default(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Marker {
    Start,
    Base,
    Separator,
    End,
}

/// Which marker `line` is, if any, and the label after it. `line` has its
/// line ending removed.
fn marker(line: &str) -> Option<(Marker, &str)> {
    let bytes = line.as_bytes();
    if line == "=======" {
        return Some((Marker::Separator, ""));
    }
    if bytes.len() < 7 {
        return None;
    }
    let kind = match &bytes[..7] {
        b"<<<<<<<" => Marker::Start,
        b"|||||||" => Marker::Base,
        b">>>>>>>" => Marker::End,
        _ => return None,
    };
    match bytes.get(7) {
        None => Some((kind, "")),
        Some(b' ') => Some((kind, line[8..].trim_end())),
        _ => None,
    }
}

/// Every conflict in `rope`, in order.
///
/// Cheap when there are none: one search for the opening marker, no copy of
/// the text. Only the lines of a conflict are read one by one.
pub fn parse(rope: &Rope) -> Vec<Conflict> {
    let len = rope.len_bytes();
    let total = rope.len_lines();
    let mut out = Vec::new();
    let mut resume_line = 0;
    let text_of = |line: usize| -> (String, usize, usize) {
        let start = rope.line_to_byte(line);
        let end = if line + 1 < total {
            rope.line_to_byte(line + 1)
        } else {
            len
        };
        let mut text = rope.slice_to_string(start..end);
        while text.ends_with('\n') || text.ends_with('\r') {
            text.pop();
        }
        (text, start, end)
    };
    for at in rope.find_in("<<<<<<<", 0..len) {
        if out.len() >= MAX_CONFLICTS {
            break;
        }
        let line = rope.byte_to_line(at);
        if rope.line_to_byte(line) != at || line < resume_line {
            continue;
        }
        let (first, start, first_end) = text_of(line);
        let Some((Marker::Start, current_label)) = marker(&first) else {
            continue;
        };
        let current_label = current_label.to_owned();
        let mut base_line = None;
        let mut base_label = None;
        let mut separator_line = None;
        let mut found = None;
        let last = total.min(line + MAX_CONFLICT_LINES);
        for n in line + 1..last {
            let (text, line_start, line_end) = text_of(n);
            match marker(&text) {
                // Another opening before this one closed: this one is not a
                // conflict, and the next candidate is that line.
                Some((Marker::Start, _)) => break,
                Some((Marker::Base, label)) if base_line.is_none() && separator_line.is_none() => {
                    base_line = Some((n, line_start, line_end));
                    base_label = Some(label.to_owned());
                }
                Some((Marker::Separator, _)) if separator_line.is_none() => {
                    separator_line = Some((n, line_start, line_end));
                }
                Some((Marker::End, label)) if separator_line.is_some() => {
                    found = Some((n, line_start, line_end, label.to_owned()));
                    break;
                }
                _ => {}
            }
        }
        let (Some((sep, sep_start, sep_end)), Some((end_line, end_start, end_end, incoming_label))) =
            (separator_line, found)
        else {
            continue;
        };
        let current_end = base_line.map_or(sep_start, |(_, s, _)| s);
        out.push(Conflict {
            range: start..end_end,
            current: first_end..current_end,
            base: base_line.map(|(_, _, e)| e..sep_start),
            incoming: sep_end..end_start,
            start_line: line,
            base_line: base_line.map(|(n, _, _)| n),
            separator_line: sep,
            end_line,
            current_label,
            base_label,
            incoming_label,
        });
        resume_line = end_line + 1;
    }
    out
}

/// The conflict holding `line`, if any.
pub fn at_line(conflicts: &[Conflict], line: usize) -> Option<usize> {
    let i = conflicts.partition_point(|c| c.end_line < line);
    conflicts
        .get(i)
        .filter(|c| c.start_line <= line && line <= c.end_line)
        .map(|_| i)
}

/// The first conflict starting after `line`, or before it going backwards,
/// wrapping around the file.
pub fn step(conflicts: &[Conflict], line: usize, forward: bool) -> Option<usize> {
    if conflicts.is_empty() {
        return None;
    }
    if forward {
        let i = conflicts.partition_point(|c| c.start_line <= line);
        Some(if i < conflicts.len() { i } else { 0 })
    } else {
        let i = conflicts.partition_point(|c| c.start_line < line);
        Some(if i > 0 { i - 1 } else { conflicts.len() - 1 })
    }
}

/// A line of one side in the side-by-side view: the document line it shows
/// and its number in that side's version of the file, 1-based.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub line: usize,
    pub number: usize,
}

/// One row of the side-by-side view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// A line outside every conflict, the same on each side.
    Common { current: Cell, incoming: Cell },
    /// The head of conflict `index`, carrying its buttons.
    Header { index: usize },
    /// The n-th line of each section of conflict `index`, where that section
    /// has one. The shorter sections are padded with empty cells, so the
    /// text after the conflict lines up again.
    Side {
        index: usize,
        current: Option<Cell>,
        base: Option<Cell>,
        incoming: Option<Cell>,
    },
}

/// The side-by-side rows for a text of `total_lines` lines holding
/// `conflicts`. Line numbers are those of each side's version of the file:
/// what it would read with every conflict resolved to that side.
pub fn rows(conflicts: &[Conflict], total_lines: usize) -> Vec<Row> {
    let mut rows = Vec::with_capacity(total_lines + conflicts.len());
    let (mut current_no, mut base_no, mut incoming_no) = (1, 1, 1);
    let mut line = 0;
    let common = |rows: &mut Vec<Row>,
                  line,
                  current_no: &mut usize,
                  base_no: &mut usize,
                  incoming_no: &mut usize| {
        rows.push(Row::Common {
            current: Cell {
                line,
                number: *current_no,
            },
            incoming: Cell {
                line,
                number: *incoming_no,
            },
        });
        *current_no += 1;
        *base_no += 1;
        *incoming_no += 1;
    };
    for (index, conflict) in conflicts.iter().enumerate() {
        while line < conflict.start_line.min(total_lines) {
            common(
                &mut rows,
                line,
                &mut current_no,
                &mut base_no,
                &mut incoming_no,
            );
            line += 1;
        }
        rows.push(Row::Header { index });
        let current = conflict.current_lines();
        let base = conflict.base_lines().unwrap_or(0..0);
        let incoming = conflict.incoming_lines();
        let height = current.len().max(base.len()).max(incoming.len()).max(1);
        for n in 0..height {
            let cell = |range: &Range<usize>, no: &mut usize| {
                (n < range.len()).then(|| {
                    let cell = Cell {
                        line: range.start + n,
                        number: *no,
                    };
                    *no += 1;
                    cell
                })
            };
            rows.push(Row::Side {
                index,
                current: cell(&current, &mut current_no),
                base: cell(&base, &mut base_no),
                incoming: cell(&incoming, &mut incoming_no),
            });
        }
        line = conflict.end_line + 1;
    }
    while line < total_lines {
        common(
            &mut rows,
            line,
            &mut current_no,
            &mut base_no,
            &mut incoming_no,
        );
        line += 1;
    }
    rows
}

/// Where conflict `index`'s header is in `rows`.
pub fn header_row(rows: &[Row], index: usize) -> Option<usize> {
    rows.iter()
        .position(|r| matches!(r, Row::Header { index: i } if *i == index))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(text: &str) -> Rope {
        Rope::from_text(text)
    }

    const TWO_WAY: &str = "fn main() {\n<<<<<<< HEAD\n    ours();\n=======\n    theirs();\n    more();\n>>>>>>> feature\n}\n";

    #[test]
    fn finds_a_two_way_conflict() {
        let r = rope(TWO_WAY);
        let found = parse(&r);
        assert_eq!(found.len(), 1);
        let c = &found[0];
        assert_eq!((c.start_line, c.separator_line, c.end_line), (1, 3, 6));
        assert_eq!(c.base_line, None);
        assert_eq!(c.current_label, "HEAD");
        assert_eq!(c.incoming_label, "feature");
        assert_eq!(r.slice_to_string(c.current.clone()), "    ours();\n");
        assert_eq!(
            r.slice_to_string(c.incoming.clone()),
            "    theirs();\n    more();\n"
        );
        assert_eq!(c.current_lines(), 2..3);
        assert_eq!(c.incoming_lines(), 4..6);
        assert!(!c.offers(Take::Base));
    }

    #[test]
    fn resolutions_replace_the_whole_block() {
        let r = rope(TWO_WAY);
        let c = &parse(&r)[0];
        let apply = |take| {
            let mut text = TWO_WAY.to_owned();
            text.replace_range(c.range.clone(), &c.resolution(&r, take));
            text
        };
        assert_eq!(apply(Take::Current), "fn main() {\n    ours();\n}\n");
        assert_eq!(
            apply(Take::Incoming),
            "fn main() {\n    theirs();\n    more();\n}\n"
        );
        assert_eq!(
            apply(Take::Both),
            "fn main() {\n    ours();\n    theirs();\n    more();\n}\n"
        );
    }

    #[test]
    fn reads_the_base_of_diff3_and_zdiff3() {
        let text = "a\n<<<<<<< HEAD\nx = 1\n||||||| merged common ancestors\nx = 0\n=======\nx = 2\n>>>>>>> topic\nz\n";
        let r = rope(text);
        let c = &parse(&r)[0];
        assert_eq!(c.base_line, Some(3));
        assert_eq!(c.base_label.as_deref(), Some("merged common ancestors"));
        assert_eq!(r.slice_to_string(c.current.clone()), "x = 1\n");
        assert_eq!(c.resolution(&r, Take::Base), "x = 0\n");
        assert_eq!(c.base_lines(), Some(4..5));
        assert!(c.offers(Take::Base));
    }

    #[test]
    fn empty_sections_and_several_conflicts() {
        let text = "<<<<<<< HEAD\n=======\nadded\n>>>>>>> b\nmid\n<<<<<<< HEAD\nkept\n=======\n>>>>>>> b\n";
        let r = rope(text);
        let found = parse(&r);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].resolution(&r, Take::Current), "");
        assert_eq!(found[0].resolution(&r, Take::Both), "added\n");
        assert_eq!(found[1].resolution(&r, Take::Both), "kept\n");
        assert_eq!(at_line(&found, 4), None);
        assert_eq!(at_line(&found, 6), Some(1));
        assert_eq!(step(&found, 4, true), Some(1));
        assert_eq!(step(&found, 6, true), Some(0), "wraps");
        assert_eq!(step(&found, 4, false), Some(0));
        assert_eq!(step(&found, 0, false), Some(1), "wraps back");
    }

    #[test]
    fn ignores_what_only_looks_like_markers() {
        // Longer markers (a recursive merge's inner conflict), an unclosed
        // block, a marker not at the start of a line, and an end with no
        // separator before it.
        for text in [
            "<<<<<<<< HEAD\na\n========\nb\n>>>>>>>> x\n",
            "<<<<<<< HEAD\na\n=======\nb\n",
            "  <<<<<<< HEAD\na\n=======\nb\n>>>>>>> x\n",
            "<<<<<<< HEAD\na\n>>>>>>> x\n",
            "<<<<<<<HEAD\na\n=======\nb\n>>>>>>> x\n",
        ] {
            assert!(parse(&rope(text)).is_empty(), "{text:?}");
        }
    }

    #[test]
    fn an_unclosed_block_does_not_swallow_the_next_conflict() {
        let text = "<<<<<<< HEAD\nstray\n<<<<<<< HEAD\na\n=======\nb\n>>>>>>> x\n";
        let found = parse(&rope(text));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].start_line, 2);
    }

    #[test]
    fn crlf_and_no_final_newline() {
        let text = "<<<<<<< HEAD\r\na\r\n=======\r\nb\r\n>>>>>>> x";
        let r = rope(text);
        let c = &parse(&r)[0];
        assert_eq!(c.range, 0..text.len());
        assert_eq!(c.incoming_label, "x");
        assert_eq!(c.resolution(&r, Take::Incoming), "b\r\n");
    }

    #[test]
    fn side_by_side_rows_align_and_number_each_side() {
        let r = rope(TWO_WAY);
        let found = parse(&r);
        let rows = rows(&found, r.len_lines());
        // "fn main() {", header, two side rows, "}", and the empty last line.
        assert_eq!(rows.len(), 6);
        assert!(matches!(rows[1], Row::Header { index: 0 }));
        assert_eq!(
            rows[2],
            Row::Side {
                index: 0,
                current: Some(Cell { line: 2, number: 2 }),
                base: None,
                incoming: Some(Cell { line: 4, number: 2 }),
            }
        );
        assert_eq!(
            rows[3],
            Row::Side {
                index: 0,
                current: None,
                base: None,
                incoming: Some(Cell { line: 5, number: 3 }),
            }
        );
        // After the conflict each side counts on from its own length.
        assert_eq!(
            rows[4],
            Row::Common {
                current: Cell { line: 7, number: 3 },
                incoming: Cell { line: 7, number: 4 },
            }
        );
        assert_eq!(header_row(&rows, 0), Some(1));
    }

    #[test]
    fn a_conflict_with_empty_sides_still_has_a_row() {
        let r = rope("<<<<<<< HEAD\n=======\n>>>>>>> b\n");
        let found = parse(&r);
        let rows = rows(&found, r.len_lines());
        assert!(matches!(
            rows[1],
            Row::Side {
                current: None,
                incoming: None,
                ..
            }
        ));
    }
}
