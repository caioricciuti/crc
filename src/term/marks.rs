//! Where the person's prompts to an agent are in a terminal's history,
//! for the marks the panel draws down its right edge and jumps between.
//! A prompt is first placed where Return was pressed, or where the cursor
//! was when its hook was heard, then moved to the line that shows it once
//! the agent prints it there. Kept in memory only: a session reattached
//! starts with none.

use super::Term;

/// The most marks a terminal keeps; the oldest go first.
const CAP: usize = 500;
/// How far above its anchor a prompt's own line may start: a prompt of
/// several lines starts above the line Return was pressed on.
const ABOVE: u64 = 12;
/// How much of the prompt's first line is looked for: enough to tell it
/// apart, little enough to fit on one row.
const PREFIX: usize = 40;
/// How many screens of output past its anchor a prompt's line is looked
/// for before the anchor is kept.
const SCREENS: u64 = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    /// Its line, numbered as [`Term::line`] numbers them.
    pub line: u64,
    /// The prompt's first line.
    pub text: String,
    /// While its line is still looked for: from this line on, up to the
    /// second.
    pending: Option<(u64, u64)>,
}

impl Mark {
    /// Its line is still looked for; until found it is the anchor.
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }
}

/// One terminal's prompt marks, in line order.
#[derive(Debug, Default)]
pub struct PromptMarks {
    marks: Vec<Mark>,
}

impl PromptMarks {
    pub fn is_empty(&self) -> bool {
        self.marks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.marks.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Mark> {
        self.marks.iter()
    }

    /// Whether a mark still waits for its line, so output is worth a look.
    pub fn has_pending(&self) -> bool {
        self.marks.iter().any(Mark::pending)
    }

    /// A prompt sent at line `anchor`, with `end` the terminal's last line
    /// and `rows` its height now. Any mark still looked for keeps its
    /// anchor: its line would have shown before the next prompt was sent.
    pub fn add(&mut self, anchor: u64, text: &str, end: u64, rows: usize) {
        for mark in &mut self.marks {
            mark.pending = None;
        }
        if self.marks.len() == CAP {
            self.marks.remove(0);
        }
        let looked = !text.trim().is_empty();
        let mark = Mark {
            line: anchor,
            text: text.to_owned(),
            pending: looked.then(|| {
                (
                    anchor.saturating_sub(ABOVE),
                    end.max(anchor) + SCREENS * rows as u64,
                )
            }),
        };
        let at = self.marks.partition_point(|m| m.line <= anchor);
        self.marks.insert(at, mark);
    }

    /// Drops the marks on lines the terminal no longer keeps, `first`
    /// being the first it does.
    pub fn trim(&mut self, first: u64) {
        let gone = self.marks.partition_point(|m| m.line < first);
        self.marks.drain(..gone);
    }

    /// The line of the last mark above line `from`.
    pub fn prev(&self, from: u64) -> Option<u64> {
        self.marks.iter().rev().map(|m| m.line).find(|&l| l < from)
    }

    /// The line of the first mark below line `from`.
    pub fn next(&self, from: u64) -> Option<u64> {
        self.marks.iter().map(|m| m.line).find(|&l| l > from)
    }

    /// Moves each mark still looked for to the first line from its anchor
    /// on that shows its prompt, or keeps the anchor once the output has
    /// gone a few screens past it. Lines above the screen are looked at
    /// once, so a look after each burst of output stays cheap. Whether a
    /// mark moved.
    pub fn resolve(&mut self, term: &Term) -> bool {
        if term.is_alternate() || !self.has_pending() {
            return false;
        }
        let first = term.first_line();
        let top = term.view_line(0, 0);
        let end = term.view_line(term.rows().saturating_sub(1), 0);
        // The prompt's start, as much of it as fits on one row.
        let room = PREFIX.min(term.cols().saturating_sub(4).max(1));
        let mut moved = false;
        for mark in &mut self.marks {
            let Some((from, until)) = mark.pending else {
                continue;
            };
            let needle: String = mark.text.trim().chars().take(room).collect();
            let hit = (from.max(first)..=end.min(until)).find(|&line| {
                term.line_chars(line)
                    .into_iter()
                    .map(|(_, c)| c)
                    .collect::<String>()
                    .contains(&needle)
            });
            match hit {
                Some(line) => {
                    moved |= line != mark.line;
                    mark.line = line;
                    mark.pending = None;
                }
                None if end >= until => mark.pending = None,
                None => mark.pending = Some((from.max(top), until)),
            }
        }
        if moved {
            self.marks.sort_by_key(|m| m.line);
        }
        moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(marks: &PromptMarks) -> Vec<u64> {
        marks.iter().map(|m| m.line).collect()
    }

    #[test]
    fn prev_and_next_go_by_line() {
        let mut marks = PromptMarks::default();
        marks.add(40, "b", 50, 10);
        marks.add(10, "a", 50, 10);
        marks.add(90, "c", 100, 10);
        assert_eq!(lines(&marks), vec![10, 40, 90]);
        assert_eq!(marks.prev(40), Some(10));
        assert_eq!(marks.prev(41), Some(40));
        assert_eq!(marks.prev(10), None);
        assert_eq!(marks.next(40), Some(90));
        assert_eq!(marks.next(0), Some(10));
        assert_eq!(marks.next(90), None);
    }

    #[test]
    fn trimmed_with_the_history_they_were_in() {
        let mut t = Term::new(20, 3);
        for n in 0..10 {
            t.advance(format!("line {n}\r\n").as_bytes());
        }
        let mut marks = PromptMarks::default();
        let first = t.first_line();
        marks.add(first, "kept", first, 3);
        marks.add(first + 4, "later", first + 4, 3);
        // The first line kept keeps its mark.
        marks.trim(t.first_line());
        assert_eq!(marks.len(), 2);
        // Clear Scrollback: the history goes, and its marks with it.
        t.advance(b"\x1b[3J");
        assert!(t.first_line() > first + 4);
        marks.trim(t.first_line());
        assert!(marks.is_empty());
        marks.add(t.first_line(), "new", t.first_line(), 3);
        t.clear_all();
        marks.trim(t.first_line());
        assert!(marks.is_empty(), "Cmd-K drops the screen's too");
    }

    #[test]
    fn the_oldest_goes_past_the_cap() {
        let mut marks = PromptMarks::default();
        for n in 0..=CAP as u64 {
            marks.add(n, "p", n, 10);
        }
        assert_eq!(marks.len(), CAP);
        assert_eq!(marks.iter().next().map(|m| m.line), Some(1));
    }

    #[test]
    fn resolve_finds_the_prompt_after_its_anchor() {
        let mut t = Term::new(40, 5);
        t.advance(b"one\r\ntwo\r\n");
        let anchor = t.view_line(t.cursor().0, 0);
        let mut marks = PromptMarks::default();
        let end = t.view_line(4, 0);
        marks.add(anchor, "fix the bug in parse", end, 5);
        // Not shown yet: still looked for, at its anchor.
        assert!(!marks.resolve(&t));
        assert!(marks.has_pending());
        t.advance(b"\x1b[2m\xe2\x94\x80\x1b[m\r\n> fix the bug in parse\r\nworking\r\n");
        assert!(marks.resolve(&t));
        let mark = marks.iter().next().expect("mark");
        assert!(!mark.pending());
        assert_eq!(mark.line, anchor + 1);
        assert_eq!(
            t.text_between((mark.line, 0), (mark.line, 40)),
            "> fix the bug in parse"
        );
    }

    #[test]
    fn a_prompt_never_shown_keeps_its_anchor() {
        let mut t = Term::new(40, 4);
        t.advance(b"$ \r\n");
        let anchor = t.view_line(t.cursor().0, 0);
        let mut marks = PromptMarks::default();
        marks.add(anchor, "hello there", t.view_line(3, 0), 4);
        for n in 0..(SCREENS as usize * 4 + 6) {
            t.advance(format!("output {n}\r\n").as_bytes());
            marks.resolve(&t);
        }
        let mark = marks.iter().next().expect("mark");
        assert!(!mark.pending(), "given up");
        assert_eq!(mark.line, anchor);
        // Shown after all, too late: not moved.
        t.advance(b"> hello there\r\n");
        assert!(!marks.resolve(&t));
        // The next prompt ends the look for the one before.
        marks.add(anchor + 20, "next", anchor + 20, 4);
        marks.add(anchor + 30, "last", anchor + 30, 4);
        assert_eq!(marks.iter().filter(|m| m.pending()).count(), 1);
    }
}
