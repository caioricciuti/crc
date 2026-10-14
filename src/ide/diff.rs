//! A line diff of two texts, for reviewing a proposed edit.
//!
//! Myers' algorithm, producing the same [`Diff`] that Source Control draws,
//! so a proposal from Claude looks exactly like a change in the working
//! tree. Common leading and trailing lines are set aside first, which is
//! what makes the usual edit, a few lines in a long file, cost almost
//! nothing. The search keeps one snapshot per edit distance, so memory grows
//! with the square of the number of changed lines; past [`MAX_EDITS`] the
//! differing middle is shown as removed then added, still correct, only
//! coarser.

use crate::project::git::{Diff, DiffKind, DiffLine, Hunk, MarkKind};

/// Unchanged lines shown around each change, as `git diff` shows.
const CONTEXT: usize = 3;
/// The largest edit distance searched for a minimal diff.
const MAX_EDITS: usize = 2000;
/// Steps of the search before it gives up, whatever the distance: bounds
/// the time on the main thread (a review opens synchronously) to tens of
/// milliseconds.
const MAX_WORK: usize = 20_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Equal(usize, usize),
    Delete(usize),
    Insert(usize),
}

/// The diff from `old` to `new`, grouped into hunks with context.
pub fn diff(old: &str, new: &str) -> Diff {
    let a: Vec<&str> = old.split_inclusive('\n').collect();
    let b: Vec<&str> = new.split_inclusive('\n').collect();
    let ops = script(&a, &b);
    let mut out = Diff::default();
    let line = |kind, old: Option<usize>, new: Option<usize>, text: &str| DiffLine {
        kind,
        old: old.map(|n| n + 1),
        new: new.map(|n| n + 1),
        text: text.trim_end_matches(['\n', '\r']).to_owned(),
        hunk: None,
    };
    for (ordinal, (start, end)) in hunk_ranges(&ops).into_iter().enumerate() {
        // Numbered, so a review can act on one hunk; nothing is staged,
        // and there is no patch text: the hunk is rebuilt from the texts.
        out.lines.push(DiffLine {
            hunk: Some(ordinal),
            ..line(DiffKind::Hunk, None, None, "")
        });
        out.hunks.push(Hunk {
            staged: false,
            ordinal,
            raw: String::new(),
        });
        for op in &ops[start..end] {
            out.lines.push(match *op {
                Op::Equal(i, j) => line(DiffKind::Context, Some(i), Some(j), a[i]),
                Op::Delete(i) => {
                    out.removed += 1;
                    line(DiffKind::Removed, Some(i), None, a[i])
                }
                Op::Insert(j) => {
                    out.added += 1;
                    line(DiffKind::Added, None, Some(j), b[j])
                }
            });
        }
    }
    out
}

/// The ranges of ops each hunk shows: every change widened by the
/// context, merged when two would touch.
fn hunk_ranges(ops: &[Op]) -> Vec<(usize, usize)> {
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| !matches!(op, Op::Equal(..)))
        .map(|(i, _)| i)
        .collect();
    let Some(&first) = changed.first() else {
        return Vec::new();
    };
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut start = first.saturating_sub(CONTEXT);
    let mut end = (first + CONTEXT + 1).min(ops.len());
    for &i in &changed[1..] {
        if i.saturating_sub(CONTEXT) <= end {
            end = (i + CONTEXT + 1).min(ops.len());
        } else {
            ranges.push((start, end));
            start = i.saturating_sub(CONTEXT);
            end = (i + CONTEXT + 1).min(ops.len());
        }
    }
    ranges.push((start, end));
    ranges
}

/// One run of changed lines from `old` to `new`, with no context around
/// it: what the gutter marks, and what Next Change and Revert Change act
/// on. Lines are zero-based and counted with their line breaks, as
/// [`unchanged_lines`] counts them; ranges are half-open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    /// The lines of `new` the change holds. Empty for a deletion, which
    /// sits above line `new_start` (the line count, removed from the end).
    pub new_start: usize,
    pub new_end: usize,
    /// The lines of `old` it replaced. Empty for an addition.
    pub old_start: usize,
    pub old_end: usize,
    pub kind: MarkKind,
}

impl Change {
    /// Whether the caret on `line` is in this change. A deletion holds the
    /// line it sits above.
    pub fn holds(&self, line: usize) -> bool {
        (self.new_start..self.held_end()).contains(&line)
    }

    fn held_end(&self) -> usize {
        self.new_end.max(self.new_start + 1)
    }
}

/// The runs of changed lines from `old` to `new`, in order. A run of
/// removed and added lines is one change: a modification when it has both,
/// an addition or a deletion otherwise.
pub fn changes(old: &str, new: &str) -> Vec<Change> {
    let a: Vec<&str> = old.split_inclusive('\n').collect();
    let b: Vec<&str> = new.split_inclusive('\n').collect();
    let mut out = Vec::new();
    // The next line of each text, where a run starting here begins.
    let (mut i, mut j) = (0, 0);
    let mut run: Option<Change> = None;
    let finish = |mut c: Change| {
        c.kind = if c.new_start == c.new_end {
            MarkKind::Removed
        } else if c.old_start == c.old_end {
            MarkKind::Added
        } else {
            MarkKind::Modified
        };
        c
    };
    let empty = |old: usize, new: usize| Change {
        new_start: new,
        new_end: new,
        old_start: old,
        old_end: old,
        kind: MarkKind::Modified,
    };
    for op in script(&a, &b) {
        match op {
            Op::Equal(x, y) => {
                out.extend(run.take().map(finish));
                (i, j) = (x + 1, y + 1);
            }
            Op::Delete(x) => {
                run.get_or_insert(empty(x, j)).old_end = x + 1;
                i = x + 1;
            }
            Op::Insert(y) => {
                run.get_or_insert(empty(i, y)).new_end = y + 1;
                j = y + 1;
            }
        }
    }
    out.extend(run.map(finish));
    out
}

/// The change holding `line`, if any.
pub fn change_at(changes: &[Change], line: usize) -> Option<usize> {
    let i = changes.partition_point(|c| c.held_end() <= line);
    changes.get(i).filter(|c| c.holds(line)).map(|_| i)
}

/// The next change after the one holding `line`, or the one before it going
/// backwards, wrapping around the file. The change the caret is in is
/// skipped either way.
pub fn step_change(changes: &[Change], line: usize, forward: bool) -> Option<usize> {
    if changes.is_empty() {
        return None;
    }
    if forward {
        let i = changes.partition_point(|c| c.new_start <= line);
        Some(if i < changes.len() { i } else { 0 })
    } else {
        let i = changes.partition_point(|c| c.held_end() <= line);
        Some(if i > 0 { i - 1 } else { changes.len() - 1 })
    }
}

/// What reverting `change` replaces: a byte range of `new`, and the lines
/// `old` had there. Taken whole from `old`, so its line breaks come back as
/// they were, a missing final one included. `None` when the change does not
/// fit the texts.
pub fn revert_change(
    old: &str,
    new: &str,
    change: &Change,
) -> Option<(std::ops::Range<usize>, String)> {
    let offset = |text: &str, line: usize| {
        let mut pieces = text.split_inclusive('\n');
        (0..line).try_fold(0, |at, _| Some(at + pieces.next()?.len()))
    };
    let (start, end) = (offset(new, change.new_start)?, offset(new, change.new_end)?);
    let (from, to) = (offset(old, change.old_start)?, offset(old, change.old_end)?);
    (start <= end && from <= to).then(|| (start..end, old[from..to].to_owned()))
}

/// `old` with hunk `index` of `diff(old, new)` applied and every other
/// change left out: keeping that hunk moves the baseline forward.
pub fn keep_hunk(old: &str, new: &str, index: usize) -> Option<String> {
    rebuild(old, new, index, true)
}

/// `new` with hunk `index` of `diff(old, new)` taken back and every other
/// change left in: undoing that hunk alone.
pub fn undo_hunk(old: &str, new: &str, index: usize) -> Option<String> {
    rebuild(old, new, index, false)
}

/// One walk for both: a removed line survives where it is outside the hunk
/// (keep) or inside it (undo), and an added line the other way round.
fn rebuild(old: &str, new: &str, index: usize, keep: bool) -> Option<String> {
    let a: Vec<&str> = old.split_inclusive('\n').collect();
    let b: Vec<&str> = new.split_inclusive('\n').collect();
    let ops = script(&a, &b);
    let (start, end) = *hunk_ranges(&ops).get(index)?;
    let mut out = String::with_capacity(old.len().max(new.len()));
    for (n, op) in ops.iter().enumerate() {
        let inside = (start..end).contains(&n);
        match *op {
            Op::Equal(i, _) => out.push_str(a[i]),
            Op::Delete(i) if inside != keep => out.push_str(a[i]),
            Op::Insert(j) if inside == keep => out.push_str(b[j]),
            _ => {}
        }
    }
    Some(out)
}

/// Pairs of (line in `old`, line in `new`) that the diff keeps unchanged,
/// in order. Lines are counted with their line breaks, so line `i` ends
/// with the `i`th newline.
pub fn unchanged_lines(old: &str, new: &str) -> Vec<(usize, usize)> {
    let a: Vec<&str> = old.split_inclusive('\n').collect();
    let b: Vec<&str> = new.split_inclusive('\n').collect();
    script(&a, &b)
        .into_iter()
        .filter_map(|op| match op {
            Op::Equal(i, j) => Some((i, j)),
            _ => None,
        })
        .collect()
}

/// Where line `line` of `old` is in `new`, given `changes(old, new)`: the
/// lines around the changes shift with them. A line inside a change lands
/// on the line at the same offset into what replaced it, or that run's
/// last line when it is shorter; a removed line lands on the line the
/// deletion sits above, which is the line count when it was at the end.
pub fn map_line(changes: &[Change], line: usize) -> usize {
    translate(changes, line, |c| {
        ((c.old_start, c.old_end), (c.new_start, c.new_end))
    })
}

/// [`map_line`] the other way: where line `line` of `new` was in `old`.
pub fn map_line_back(changes: &[Change], line: usize) -> usize {
    translate(changes, line, |c| {
        ((c.new_start, c.new_end), (c.old_start, c.old_end))
    })
}

/// `line` from the first side of `changes` to the second; `sides` gives a
/// change's (from, to) line ranges.
fn translate(
    changes: &[Change],
    line: usize,
    sides: impl Fn(&Change) -> ((usize, usize), (usize, usize)),
) -> usize {
    // The changes ending at or before the line; past the last of them
    // the two texts are in step again.
    let i = changes.partition_point(|c| sides(c).0.1 <= line);
    if let Some(c) = changes.get(i) {
        let ((from, _), (start, end)) = sides(c);
        if from <= line {
            return if end > start {
                (start + (line - from)).min(end - 1)
            } else {
                start
            };
        }
    }
    match i.checked_sub(1).map(|p| sides(&changes[p])) {
        Some(((_, from_end), (_, to_end))) => line - from_end + to_end,
        None => line,
    }
}

/// Which text a line of [`merged`] comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergedKind {
    Both,
    OldOnly,
    NewOnly,
}

/// One line of [`merged`], with its zero-based line in each text that has
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MergedLine {
    pub kind: MergedKind,
    pub old: Option<usize>,
    pub new: Option<usize>,
}

/// Every line of `old` and of `new` once, in reading order: unchanged
/// lines once, and each run of changes as its removed lines then its added
/// ones, so removed lines can be drawn where they were. Lines are counted
/// with their line breaks, as [`changes`] counts them.
pub fn merged(old: &str, new: &str) -> Vec<MergedLine> {
    let a: Vec<&str> = old.split_inclusive('\n').collect();
    let b: Vec<&str> = new.split_inclusive('\n').collect();
    let mut out = Vec::with_capacity(a.len().max(b.len()));
    // A run's added lines, held until its removed ones are all out.
    let mut added = Vec::new();
    let line = |kind, old, new| MergedLine { kind, old, new };
    for op in script(&a, &b) {
        match op {
            Op::Equal(i, j) => {
                out.append(&mut added);
                out.push(line(MergedKind::Both, Some(i), Some(j)));
            }
            Op::Delete(i) => out.push(line(MergedKind::OldOnly, Some(i), None)),
            Op::Insert(j) => added.push(line(MergedKind::NewOnly, None, Some(j))),
        }
    }
    out.append(&mut added);
    out
}

/// The one edit that turns `old` into `new`: everything between their
/// common start and common end, on whole characters, in the shape the
/// incremental parser takes. `None` when the texts are the same. Points
/// count rows by `\n`, so both texts should have the buffer's line breaks.
pub fn prefix_suffix_edit(old: &str, new: &str) -> Option<crate::text::buffer::Edit> {
    let (a, b) = (old.as_bytes(), new.as_bytes());
    let shorter = a.len().min(b.len());
    let mut prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let mut suffix = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take(shorter - prefix)
        .take_while(|(x, y)| x == y)
        .count();
    while !(old.is_char_boundary(prefix) && new.is_char_boundary(prefix)) {
        prefix -= 1;
    }
    while !(old.is_char_boundary(a.len() - suffix) && new.is_char_boundary(b.len() - suffix)) {
        suffix -= 1;
    }
    let (old_end, new_end) = (a.len() - suffix, b.len() - suffix);
    if prefix == old_end && prefix == new_end {
        return None;
    }
    // (row, byte column) of `at`, counting on from `from`, a point of
    // `text` already known.
    let point = |text: &str, from: (usize, (usize, usize)), at: usize| {
        let (start, (row, column)) = from;
        let span = &text[start..at];
        match span.rfind('\n') {
            Some(last) => (row + span.matches('\n').count(), at - (start + last + 1)),
            None => (row, column + (at - start)),
        }
    };
    let start_point = point(old, (0, (0, 0)), prefix);
    Some(crate::text::buffer::Edit {
        start_byte: prefix,
        old_end_byte: old_end,
        new_end_byte: new_end,
        start_point,
        old_end_point: point(old, (prefix, start_point), old_end),
        new_end_point: point(new, (prefix, start_point), new_end),
    })
}

/// The edit script from `a` to `b`, in order.
fn script(a: &[&str], b: &[&str]) -> Vec<Op> {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (mid_a, mid_b) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let mut ops: Vec<Op> = (0..prefix).map(|i| Op::Equal(i, i)).collect();
    // Lines as numbers, so the search compares integers, not strings.
    fn numbered<'a>(
        ids: &mut std::collections::HashMap<&'a str, u32>,
        lines: &[&'a str],
    ) -> Vec<u32> {
        lines
            .iter()
            .map(|&line| {
                let next = ids.len() as u32;
                *ids.entry(line).or_insert(next)
            })
            .collect()
    }
    let mut ids = std::collections::HashMap::new();
    let mid_a = numbered(&mut ids, mid_a);
    let mid_b = numbered(&mut ids, mid_b);
    match myers(&mid_a, &mid_b) {
        Some(middle) => ops.extend(middle.into_iter().map(|op| match op {
            Op::Equal(i, j) => Op::Equal(i + prefix, j + prefix),
            Op::Delete(i) => Op::Delete(i + prefix),
            Op::Insert(j) => Op::Insert(j + prefix),
        })),
        None => {
            ops.extend((0..mid_a.len()).map(|i| Op::Delete(i + prefix)));
            ops.extend((0..mid_b.len()).map(|j| Op::Insert(j + prefix)));
        }
    }
    let (tail_a, tail_b) = (a.len() - suffix, b.len() - suffix);
    ops.extend((0..suffix).map(|n| Op::Equal(tail_a + n, tail_b + n)));
    ops
}

/// A shortest edit script, or `None` past [`MAX_EDITS`].
fn myers(a: &[u32], b: &[u32]) -> Option<Vec<Op>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    // The difference in length alone is a lower bound on the distance.
    if a.len().abs_diff(b.len()) > MAX_EDITS {
        return None;
    }
    let max = (a.len() + b.len()).min(MAX_EDITS) as isize;
    let mut work = 0usize;
    let offset = max + 1;
    let mut v = vec![0isize; (2 * max + 3) as usize];
    // trace[d] holds v[-d-1..=d+1] as it was before step d.
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let at = |k: isize| (k + offset) as usize;
    for d in 0..=max {
        trace.push(v[at(-d - 1)..=at(d + 1)].to_vec());
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && v[at(k - 1)] < v[at(k + 1)]) {
                v[at(k + 1)]
            } else {
                v[at(k - 1)] + 1
            };
            let mut y = x - k;
            let from = x;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            work += 1 + (x - from) as usize;
            if work > MAX_WORK {
                return None;
            }
            v[at(k)] = x;
            if x >= n && y >= m {
                return Some(backtrack(&trace, n, m));
            }
            k += 2;
        }
    }
    None
}

fn backtrack(trace: &[Vec<isize>], n: isize, m: isize) -> Vec<Op> {
    let (mut x, mut y) = (n, m);
    let mut ops = Vec::new();
    for (d, v) in trace.iter().enumerate().rev() {
        let d = d as isize;
        let get = |k: isize| v[(k + d + 1) as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && get(k - 1) < get(k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = get(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push(Op::Equal((x - 1) as usize, (y - 1) as usize));
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if x == prev_x {
                ops.push(Op::Insert((y - 1) as usize));
            } else {
                ops.push(Op::Delete((x - 1) as usize));
            }
        }
        x = prev_x;
        y = prev_y;
    }
    ops.reverse();
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applies a script to `a`, which must give `b`.
    fn apply(a: &[&str], b: &[&str], ops: &[Op]) -> Vec<String> {
        let mut out = Vec::new();
        let (mut i, mut j) = (0, 0);
        for op in ops {
            match *op {
                Op::Equal(x, y) => {
                    assert_eq!((x, y), (i, j), "equal out of order");
                    assert_eq!(a[x], b[y]);
                    out.push(a[x].to_string());
                    i += 1;
                    j += 1;
                }
                Op::Delete(x) => {
                    assert_eq!(x, i);
                    i += 1;
                }
                Op::Insert(y) => {
                    assert_eq!(y, j);
                    out.push(b[y].to_string());
                    j += 1;
                }
            }
        }
        assert_eq!((i, j), (a.len(), b.len()));
        out
    }

    fn edits(ops: &[Op]) -> usize {
        ops.iter().filter(|op| !matches!(op, Op::Equal(..))).count()
    }

    #[test]
    fn scripts_rebuild_the_new_text_minimally() {
        let cases: &[(&str, &str, usize)] = &[
            ("", "", 0),
            ("a\n", "a\n", 0),
            ("", "a\nb\n", 2),
            ("a\nb\n", "", 2),
            ("a\nb\nc\n", "a\nx\nc\n", 2),
            ("a\nb\nc\na\nb\nb\na\n", "c\nb\na\nb\na\nc\n", 5),
            ("x\n", "x", 2),
        ];
        for &(old, new, expected) in cases {
            let a: Vec<&str> = old.split_inclusive('\n').collect();
            let b: Vec<&str> = new.split_inclusive('\n').collect();
            let ops = script(&a, &b);
            assert_eq!(apply(&a, &b, &ops).concat(), new, "{old:?} -> {new:?}");
            assert_eq!(edits(&ops), expected, "{old:?} -> {new:?}");
        }
    }

    #[test]
    fn random_texts_round_trip() {
        // A fixed LCG, so a failure is reproducible.
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut next = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 33) as usize
        };
        for _ in 0..300 {
            let pick = |next: &mut dyn FnMut() -> usize| {
                let len = next() % 12;
                (0..len)
                    .map(|_| ["a\n", "b\n", "c\n", "d\n"][next() % 4])
                    .collect::<Vec<_>>()
            };
            let a = pick(&mut next);
            let b = pick(&mut next);
            let ops = script(&a, &b);
            assert_eq!(apply(&a, &b, &ops), b);
        }
    }

    #[test]
    fn hunks_carry_context_and_line_numbers() {
        let old: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        let new = old
            .replace("line 5\n", "line five\n")
            .replace("line 18\n", "");
        let d = diff(&old, &new);
        assert_eq!((d.added, d.removed), (1, 2));
        let hunks = d.lines.iter().filter(|l| l.kind == DiffKind::Hunk).count();
        assert_eq!(hunks, 2, "changes 13 lines apart are two hunks");
        let removed: Vec<_> = d
            .lines
            .iter()
            .filter(|l| l.kind == DiffKind::Removed)
            .map(|l| (l.old, l.text.as_str()))
            .collect();
        assert_eq!(removed, [(Some(5), "line 5"), (Some(18), "line 18")]);
        let added = d.lines.iter().find(|l| l.kind == DiffKind::Added).unwrap();
        assert_eq!((added.new, added.text.as_str()), (Some(5), "line five"));
        // Three lines of context either side of the first change.
        let first: Vec<_> = d.lines[1..]
            .iter()
            .take_while(|l| l.kind != DiffKind::Hunk)
            .collect();
        assert_eq!(first.first().unwrap().old, Some(2));
        assert_eq!(first.last().unwrap().old, Some(8));
        assert!(diff(&old, &old).lines.is_empty());
    }

    #[test]
    fn crlf_is_kept_out_of_the_shown_text() {
        let d = diff("a\r\nb\r\n", "a\r\nc\r\n");
        let texts: Vec<_> = d.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["", "a", "b", "c"]);
    }

    #[test]
    fn past_the_edit_limit_the_middle_is_replaced_whole() {
        let old: String = (0..3000).map(|n| format!("{n}\n")).collect();
        let new: String = (0..3000).map(|n| format!("x{n}\n")).collect();
        let d = diff(&format!("keep\n{old}end\n"), &format!("keep\n{new}end\n"));
        assert_eq!((d.added, d.removed), (3000, 3000));
    }

    const OLD: &str = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n";
    const NEW: &str = "a\nB\nc\nd\ne\nf\ng\nh\ni\nj\nk\nL\nm";

    #[test]
    fn hunks_are_numbered_for_a_review() {
        let d = diff(OLD, NEW);
        assert_eq!(d.hunks.len(), 2);
        let headers: Vec<Option<usize>> = d
            .lines
            .iter()
            .filter(|l| l.kind == DiffKind::Hunk)
            .map(|l| l.hunk)
            .collect();
        assert_eq!(headers, vec![Some(0), Some(1)]);
    }

    #[test]
    fn keeping_or_undoing_one_hunk_leaves_the_other() {
        let kept = keep_hunk(OLD, NEW, 0).expect("hunk 0");
        assert_eq!(kept, "a\nB\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n");
        assert_eq!(diff(&kept, NEW).hunks.len(), 1);

        let undone = undo_hunk(OLD, NEW, 1).expect("hunk 1");
        assert_eq!(undone, "a\nB\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n");
        assert_eq!(keep_hunk(OLD, NEW, 2), None);
    }

    /// (new range, old range, kind) of each change.
    type Run = (std::ops::Range<usize>, std::ops::Range<usize>, MarkKind);

    fn runs(old: &str, new: &str) -> Vec<Run> {
        changes(old, new)
            .into_iter()
            .map(|c| (c.new_start..c.new_end, c.old_start..c.old_end, c.kind))
            .collect()
    }

    #[test]
    fn changes_group_runs_with_old_ranges() {
        use MarkKind::{Added, Modified, Removed};
        assert_eq!(runs("a\nb\n", "a\nb\n"), []);
        // At the start.
        assert_eq!(runs("a\nb\n", "x\na\nb\n"), [(0..1, 0..0, Added)]);
        assert_eq!(runs("a\nb\n", "A\nb\n"), [(0..1, 0..1, Modified)]);
        assert_eq!(runs("a\nb\n", "b\n"), [(0..0, 0..1, Removed)]);
        // In the middle.
        assert_eq!(runs("a\nb\nc\n", "a\nx\ny\nb\nc\n"), [(1..3, 1..1, Added)]);
        assert_eq!(runs("a\nb\nc\n", "a\nB\nc\n"), [(1..2, 1..2, Modified)]);
        assert_eq!(
            runs("a\nb\nc\nd\n", "a\nd\n"),
            [(1..1, 1..3, Removed)],
            "a deletion sits above the line that survived"
        );
        // At the end, with and without a final line break.
        assert_eq!(runs("a\n", "a\nb\n"), [(1..2, 1..1, Added)]);
        assert_eq!(runs("a\nb\n", "a\n"), [(1..1, 1..2, Removed)]);
        assert_eq!(runs("a\nb", "a\nc"), [(1..2, 1..2, Modified)]);
        assert_eq!(runs("a\n", "a"), [(0..1, 0..1, Modified)]);
        // Removed and added lines next to each other are one change; an
        // unchanged line between them makes two.
        assert_eq!(
            runs("a\nb\nc\nd\n", "a\nX\nY\nZ\nd\n"),
            [(1..4, 1..3, Modified)]
        );
        assert_eq!(
            runs("a\nb\nc\nd\ne\n", "a\nB\nc\ne\nf\n"),
            [
                (1..2, 1..2, Modified),
                (3..3, 3..4, Removed),
                (4..5, 5..5, Added)
            ]
        );
        assert_eq!(runs("", "a\nb\n"), [(0..2, 0..0, Added)]);
        assert_eq!(runs("a\nb\n", ""), [(0..0, 0..2, Removed)]);
    }

    #[test]
    fn stepping_through_changes_wraps_and_skips_the_one_at_the_caret() {
        // Lines 1-2 modified, a deletion above 5, line 8 added.
        let old = "0\n1\n2\n3\n4\ngone\n5\n6\n7\n9\n";
        let new = "0\nA\nB\n3\n4\n5\n6\n7\nnew\n9\n";
        let found = changes(old, new);
        assert_eq!(found.len(), 3);
        assert_eq!(step_change(&found, 0, true), Some(0));
        assert_eq!(step_change(&found, 1, true), Some(1), "skips its own");
        assert_eq!(step_change(&found, 2, true), Some(1));
        assert_eq!(step_change(&found, 5, true), Some(2));
        assert_eq!(step_change(&found, 8, true), Some(0), "wraps");
        assert_eq!(step_change(&found, 9, false), Some(2));
        assert_eq!(step_change(&found, 8, false), Some(1), "skips its own");
        assert_eq!(step_change(&found, 5, false), Some(0), "the deletion");
        assert_eq!(step_change(&found, 2, false), Some(2), "wraps back");
        assert_eq!(step_change(&found, 0, false), Some(2));
        assert_eq!(step_change(&[], 0, true), None);
        let at: Vec<_> = (0..10).map(|line| change_at(&found, line)).collect();
        assert_eq!(
            at,
            [
                None,
                Some(0),
                Some(0),
                None,
                None,
                Some(1),
                None,
                None,
                Some(2),
                None
            ]
        );
    }

    /// `new` with change `index` reverted the way the editor does it.
    fn reverted(old: &str, new: &str, index: usize) -> String {
        let change = changes(old, new)[index];
        let (range, text) = revert_change(old, new, &change).expect("fits");
        let mut out = new.to_owned();
        out.replace_range(range, &text);
        out
    }

    #[test]
    fn reverting_a_change_restores_only_its_lines() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nB\nc\ne\nf\n";
        assert_eq!(reverted(old, new, 0), "a\nb\nc\ne\nf\n");
        assert_eq!(reverted(old, new, 1), "a\nB\nc\nd\ne\nf\n", "a deletion");
        assert_eq!(reverted(old, new, 2), "a\nB\nc\ne\n", "an addition");
        // The last line, with and without its line break.
        assert_eq!(reverted("a\nb\n", "a\n", 0), "a\nb\n");
        assert_eq!(reverted("a\nb", "a\nc", 0), "a\nb");
        assert_eq!(reverted("a\nb\n", "a\nb", 0), "a\nb\n");
        assert_eq!(reverted("a\nb", "a\nb\nc\n", 0), "a\nb");
        assert_eq!(reverted("", "new\n", 0), "");
        assert_eq!(reverted("all\n", "", 0), "all\n");
        // CRLF lines come back with their CR.
        assert_eq!(
            reverted("x\r\ny\r\nz\r\n", "x\r\nY\r\nz\r\n", 0),
            "x\r\ny\r\nz\r\n"
        );
        assert_eq!(reverted("x\r\ny\r\n", "x\r\n", 0), "x\r\ny\r\n");
        // Every change reverted, last first, is the old text.
        let cases = [(old, new), (OLD, NEW), ("1\n2\n3\n", "0\n2\n4\n5")];
        for (old, new) in cases {
            let mut now = new.to_owned();
            for change in changes(old, new).iter().rev() {
                let (range, text) = revert_change(old, &now, change).expect("fits");
                now.replace_range(range, &text);
            }
            assert_eq!(now, old, "{old:?} -> {new:?}");
        }
        let past = Change {
            new_start: 5,
            new_end: 6,
            old_start: 0,
            old_end: 0,
            kind: MarkKind::Added,
        };
        assert_eq!(revert_change("a\n", "a\n", &past), None, "stale lines");
    }

    /// Reverts the change at `line` of `buffer` as Git > Revert Change does.
    fn revert_in(buffer: &mut crate::text::buffer::Buffer, head: &str, line: usize) -> usize {
        let text = buffer.rope.to_string();
        let found = changes(head, &text);
        let index = change_at(&found, line).expect("a change at the line");
        let (range, replacement) = revert_change(head, &text, &found[index]).expect("fits");
        let caret = buffer.cursor();
        buffer.place_cursor(caret, crate::text::buffer::Motion::Move);
        buffer.replace_ranges(&[(range, replacement)])
    }

    #[test]
    fn a_reverted_change_is_one_undo_step_in_the_buffer() {
        use crate::text::buffer::Buffer;
        let head = "a\nb\nc\nd\ne\n";
        let mut buffer = Buffer::from_text(head);
        buffer.place_cursor(2, crate::text::buffer::Motion::Move);
        buffer.insert("B");
        buffer.place_cursor(buffer.rope.len_bytes(), crate::text::buffer::Motion::Move);
        buffer.insert("f\n");
        let edited = buffer.rope.to_string();
        assert_eq!(edited, "a\nBb\nc\nd\ne\nf\n");
        // Typing a moment before is still its own step.
        assert_eq!(revert_in(&mut buffer, head, 5), 1);
        assert_eq!(
            buffer.rope.to_string(),
            "a\nBb\nc\nd\ne\n",
            "only that change"
        );
        assert!(buffer.undo());
        assert_eq!(buffer.rope.to_string(), edited);
        // The last line, which has no line break, back as it was.
        let mut buffer = Buffer::from_text("x\ny");
        buffer.place_cursor(3, crate::text::buffer::Motion::Move);
        buffer.insert("z\n");
        assert_eq!(revert_in(&mut buffer, "x\ny", 1), 1);
        assert_eq!(buffer.rope.to_string(), "x\ny");
    }

    #[test]
    fn reverting_in_a_crlf_file_keeps_its_line_breaks() {
        use crate::text::buffer::{Buffer, Motion};
        let dir = std::env::temp_dir().join(format!("crc-revert-crlf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("notes.txt");
        let committed = b"one\r\ntwo\r\nthree\r\n";
        std::fs::write(&path, committed).unwrap();
        // The HEAD text as the gutter reads it: line breaks as the buffer
        // has them.
        let head = crate::text::buffer::normalize_newlines(std::str::from_utf8(committed).unwrap())
            .into_owned();
        let mut buffer = Buffer::open(&path).unwrap();
        buffer.place_cursor(4, Motion::Move);
        buffer.insert("2\nmore ");
        buffer.place_cursor(0, Motion::Move);
        buffer.insert("zero\n");
        assert_eq!(buffer.rope.to_string(), "zero\none\n2\nmore two\nthree\n");
        assert_eq!(revert_in(&mut buffer, &head, 2), 1);
        buffer.save(None).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"zero\r\none\r\ntwo\r\nthree\r\n"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn every_hunk_kept_is_the_new_text_and_every_hunk_undone_the_old() {
        let cases = [
            (OLD, NEW),
            ("", "only\n"),
            ("gone\n", ""),
            ("x\r\ny\r\n", "x\r\nY\r\n"),
            ("no newline", "no newline\nnow"),
            (
                "1\n2\n3\n4\n5\n6\n7\n8\n9\n",
                "0\n1\n2\n3\n5\n6\n7\n8\n9\n10\n",
            ),
        ];
        for (old, new) in cases {
            let mut base = old.to_owned();
            while !diff(&base, new).hunks.is_empty() {
                base = keep_hunk(&base, new, 0).expect("keep");
            }
            assert_eq!(base, new, "keeping {old:?} -> {new:?}");
            let mut now = new.to_owned();
            while let Some(last) = diff(old, &now).hunks.len().checked_sub(1) {
                now = undo_hunk(old, &now, last).expect("undo");
            }
            assert_eq!(now, old, "undoing {old:?} -> {new:?}");
        }
    }

    #[test]
    fn lines_map_across_changes_and_back() {
        // Lines 1-2 modified into three, a deletion above old 6, an
        // addition after old 8.
        let old = "0\n1\n2\n3\n4\n5\ngone\n6\n7\n8\n";
        let new = "0\nA\nB\nC\n3\n4\n5\n6\n7\n8\nnew\n";
        let found = changes(old, new);
        assert_eq!(found.len(), 3);
        let forward: Vec<usize> = (0..11).map(|l| map_line(&found, l)).collect();
        // `gone` lands on `6`, the line its deletion sits above; past the
        // end the texts stay in step.
        assert_eq!(forward, [0, 1, 2, 4, 5, 6, 7, 7, 8, 9, 11]);
        let back: Vec<usize> = (0..12).map(|l| map_line_back(&found, l)).collect();
        // `C` has no old line at its offset, so it takes the run's last.
        assert_eq!(back, [0, 1, 2, 2, 3, 4, 5, 7, 8, 9, 10, 10]);
        // Every unchanged line goes there and back to itself.
        for (i, j) in unchanged_lines(old, new) {
            assert_eq!(map_line(&found, i), j);
            assert_eq!(map_line_back(&found, j), i);
        }
        assert_eq!(map_line(&[], 7), 7);
        // Removed from the end: the line count, for the caller to clamp.
        assert_eq!(map_line(&changes("a\nb\nc\n", "a\n"), 2), 1);
        // Everything added: old line 0 is past the new text.
        assert_eq!(map_line(&changes("", "a\nb\n"), 0), 2);
        assert_eq!(map_line_back(&changes("", "a\nb\n"), 1), 0);
        // A random pair agrees with the unchanged lines too.
        let a = "a\nb\nc\nd\na\nb\nb\na\nc\n";
        let b = "c\nb\na\nb\na\nc\nd\nd\n";
        let found = changes(a, b);
        for (i, j) in unchanged_lines(a, b) {
            assert_eq!((map_line(&found, i), map_line_back(&found, j)), (j, i));
        }
    }

    #[test]
    fn merged_lines_put_removed_before_added_in_place() {
        use MergedKind::{Both, NewOnly, OldOnly};
        let got: Vec<(MergedKind, Option<usize>, Option<usize>)> =
            merged("a\nb\nc\nd\ne\n", "a\nB\nc\ne\nf\n")
                .into_iter()
                .map(|l| (l.kind, l.old, l.new))
                .collect();
        assert_eq!(
            got,
            [
                (Both, Some(0), Some(0)),
                (OldOnly, Some(1), None),
                (NewOnly, None, Some(1)),
                (Both, Some(2), Some(2)),
                (OldOnly, Some(3), None),
                (Both, Some(4), Some(3)),
                (NewOnly, None, Some(4)),
            ]
        );
        assert!(merged("", "").is_empty());
        // Each text read off the merge is that text.
        let cases = [
            (OLD, NEW),
            ("x\ny\n", ""),
            ("", "x\n"),
            ("a\nb\nc\na\nb\nb\na\n", "c\nb\na\nb\na\nc\n"),
        ];
        for (old, new) in cases {
            let lines = merged(old, new);
            let olds: Vec<usize> = lines.iter().filter_map(|l| l.old).collect();
            let news: Vec<usize> = lines.iter().filter_map(|l| l.new).collect();
            assert_eq!(
                olds,
                (0..old.split_inclusive('\n').count()).collect::<Vec<_>>()
            );
            assert_eq!(
                news,
                (0..new.split_inclusive('\n').count()).collect::<Vec<_>>()
            );
            // Within a run, no added line comes before a removed one.
            for pair in lines.windows(2) {
                assert!(!(pair[0].kind == NewOnly && pair[1].kind == OldOnly));
            }
        }
        // Past the edit limit: the middle removed whole, then added whole.
        let old: String = (0..3000).map(|n| format!("{n}\n")).collect();
        let new: String = (0..3000).map(|n| format!("x{n}\n")).collect();
        let lines = merged(&old, &new);
        assert_eq!(lines.len(), 6000);
        assert!(lines[..3000].iter().all(|l| l.kind == OldOnly));
        assert!(lines[3000..].iter().all(|l| l.kind == NewOnly));
    }

    /// `old` with `edit` applied, the replacement taken from `new`.
    fn applied(old: &str, new: &str) -> String {
        let Some(edit) = prefix_suffix_edit(old, new) else {
            return old.to_owned();
        };
        let mut out = old.to_owned();
        out.replace_range(
            edit.start_byte..edit.old_end_byte,
            &new[edit.start_byte..edit.new_end_byte],
        );
        out
    }

    #[test]
    fn one_edit_spans_the_difference_on_whole_characters() {
        use crate::text::buffer::Edit;
        assert_eq!(prefix_suffix_edit("same\n", "same\n"), None);
        assert_eq!(
            prefix_suffix_edit("let a = 1;\nok\n", "let a = 22;\nok\n"),
            Some(Edit {
                start_byte: 8,
                old_end_byte: 9,
                new_end_byte: 10,
                start_point: (0, 8),
                old_end_point: (0, 9),
                new_end_point: (0, 10),
            })
        );
        // Lines added in the middle: the points move down.
        assert_eq!(
            prefix_suffix_edit("a\nb\n", "a\nx\ny\nb\n"),
            Some(Edit {
                start_byte: 2,
                old_end_byte: 2,
                new_end_byte: 6,
                start_point: (1, 0),
                old_end_point: (1, 0),
                new_end_point: (3, 0),
            })
        );
        // é and è share their first byte, and the text after them: the
        // edit still starts and ends on character boundaries.
        let edit = prefix_suffix_edit("caf\u{e9}\u{e9}\n", "caf\u{e8}\u{e9}\n").unwrap();
        assert_eq!(
            (edit.start_byte, edit.old_end_byte, edit.new_end_byte),
            (3, 5, 5)
        );
        assert_eq!(edit.old_end_point, (0, 5), "columns are bytes");
        // Repeated text: prefix and suffix do not overlap.
        let edit = prefix_suffix_edit("aaaa", "aa").unwrap();
        assert_eq!(
            (edit.start_byte, edit.old_end_byte, edit.new_end_byte),
            (2, 4, 2)
        );
        let cases = [
            ("", "\u{1f600}\n"),
            ("\u{1f600}\n", ""),
            ("\u{1f600}\u{1f601}", "\u{1f600}\u{1f602}"),
            ("\u{4e2d}\u{6587}\n\u{6587}", "\u{4e2d}\n\u{6587}"),
            ("x\ny\u{e9}z\n", "x\nyz\n"),
        ];
        for (old, new) in cases {
            assert_eq!(applied(old, new), new, "{old:?} -> {new:?}");
            let edit = prefix_suffix_edit(old, new).unwrap();
            // The points match a count of the text.
            let point = |text: &str, at: usize| {
                let row = text[..at].matches('\n').count();
                let column = at - text[..at].rfind('\n').map_or(0, |n| n + 1);
                (row, column)
            };
            assert_eq!(edit.start_point, point(old, edit.start_byte));
            assert_eq!(edit.old_end_point, point(old, edit.old_end_byte));
            assert_eq!(edit.new_end_point, point(new, edit.new_end_byte));
        }
    }
}
