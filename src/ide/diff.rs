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

use crate::project::git::{Diff, DiffKind, DiffLine};

/// Unchanged lines shown around each change, as `git diff` shows.
const CONTEXT: usize = 3;
/// The largest edit distance searched for a minimal diff.
const MAX_EDITS: usize = 2000;

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
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| !matches!(op, Op::Equal(..)))
        .map(|(i, _)| i)
        .collect();
    let Some(&first) = changed.first() else {
        return out;
    };
    // Ranges of ops to show: each change widened by the context, merged
    // when two would touch.
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
    let line = |kind, old: Option<usize>, new: Option<usize>, text: &str| DiffLine {
        kind,
        old: old.map(|n| n + 1),
        new: new.map(|n| n + 1),
        text: text.trim_end_matches(['\n', '\r']).to_owned(),
        hunk: None,
    };
    for (start, end) in ranges {
        out.lines.push(line(DiffKind::Hunk, None, None, ""));
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
    match myers(mid_a, mid_b) {
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
fn myers(a: &[&str], b: &[&str]) -> Option<Vec<Op>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let max = (a.len() + b.len()).min(MAX_EDITS) as isize;
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
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
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
}
