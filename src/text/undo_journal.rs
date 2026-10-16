//! The undo journal: a buffer's undo and redo stacks as bytes, so they can
//! outlive the window that made them.
//!
//! Every step is stored as a delta from its neighbour, walking outward from
//! the current text: undo steps nearest first, then redo steps nearest
//! first. A step is a byte span replaced by new text plus the cursors that
//! belong to it. Decoding replays the walk on clones of the current rope, so
//! the restored steps share structure the way live snapshots do.
//!
//! The bytes are plain; the history store compresses them. Decoding trusts
//! nothing: a wrong base, a damaged byte, an offset out of bounds or inside a
//! character, or a format from a newer crc reads as no journal at all,
//! never as a panic or a cursor the renderer would trip over.

use crate::text::rope::Rope;

const MAGIC: &[u8; 4] = b"CRCU";
const VERSION: u8 = 1;
/// Undo steps kept, matching the buffer's own limit.
const UNDO_LIMIT: usize = 1000;
/// Redo steps kept.
const REDO_LIMIT: usize = 1000;
/// Largest journal written. Past it the oldest undo steps go first, then
/// the furthest redo steps.
const MAX_BYTES: usize = 4 << 20;
/// Room kept for the header and the checksum: magic, version, and three
/// LEB128 integers of at most 10 bytes each.
const FRAME_BYTES: usize = MAGIC.len() + 1 + 3 * 10 + CHECKSUM_BYTES;
const CHECKSUM_BYTES: usize = 8;

/// The selection state of one step, offsets into that step's text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cursors {
    pub cursor: usize,
    pub anchor: usize,
    /// Extra cursors as (anchor, head) pairs.
    pub extra: Vec<(usize, usize)>,
}

/// One restorable point: a text and the cursors placed in it.
#[derive(Clone, Debug)]
pub struct Step {
    pub rope: Rope,
    pub cursors: Cursors,
}

/// Decoded stacks, in the buffer's own order.
#[derive(Clone, Debug, Default)]
pub struct Journal {
    /// Oldest first.
    pub undo: Vec<Step>,
    /// Top last: the next redo is `redo.last()`.
    pub redo: Vec<Step>,
}

/// Encodes `undo` (oldest first) and `redo` (top last) against `current`,
/// the text they step away from. Keeps the newest [`UNDO_LIMIT`] undo
/// steps and the nearest [`REDO_LIMIT`] redo steps, then trims to
/// [`MAX_BYTES`]. Cursors are written as given; ones that do not fit their
/// text make the whole journal decode as `None`.
pub fn encode(current: &Rope, undo: &[Step], redo: &[Step]) -> Vec<u8> {
    let mut budget = MAX_BYTES - FRAME_BYTES;
    // Redo is walked first so the budget runs out on the oldest undo steps
    // before it touches any redo.
    let (redo_count, redo_body) = walk(current, redo.iter().rev().take(REDO_LIMIT), &mut budget);
    let (undo_count, undo_body) = walk(current, undo.iter().rev().take(UNDO_LIMIT), &mut budget);
    let mut out = Writer(Vec::with_capacity(
        FRAME_BYTES + undo_body.len() + redo_body.len(),
    ));
    out.0.extend_from_slice(MAGIC);
    out.0.push(VERSION);
    out.uint(current.len_bytes());
    out.uint(undo_count);
    out.uint(redo_count);
    out.0.extend_from_slice(&undo_body);
    out.0.extend_from_slice(&redo_body);
    let sum = crate::platform::fnv1a(&out.0);
    out.0.extend_from_slice(&sum.to_le_bytes());
    out.0
}

/// Decodes a journal written by [`encode`] against `current`, or `None` for
/// any damage, a different base text, or a newer format.
pub fn decode(bytes: &[u8], current: &Rope) -> Option<Journal> {
    let split = bytes.len().checked_sub(CHECKSUM_BYTES)?;
    let (body, sum) = bytes.split_at(split);
    if crate::platform::fnv1a(body).to_le_bytes() != sum {
        return None;
    }
    let mut r = Reader(body);
    if r.take(MAGIC.len())? != MAGIC || r.take(1)? != [VERSION] {
        return None;
    }
    if r.uint()? != current.len_bytes() {
        return None;
    }
    let undo_count = r.uint()?;
    let redo_count = r.uint()?;
    let mut undo = replay(&mut r, current, undo_count)?;
    let mut redo = replay(&mut r, current, redo_count)?;
    if !r.0.is_empty() {
        return None;
    }
    // Both walks come out nearest first.
    undo.reverse();
    redo.reverse();
    Some(Journal { undo, redo })
}

/// Encodes `steps`, nearest first, each against the one before it, while
/// they fit in `budget`. Returns how many fit and their bytes.
fn walk<'a>(
    current: &Rope,
    steps: impl Iterator<Item = &'a Step>,
    budget: &mut usize,
) -> (usize, Vec<u8>) {
    let mut out = Vec::new();
    let mut count = 0;
    let mut prev = current;
    for step in steps {
        let mut w = Writer(Vec::new());
        let (start, removed, inserted) = span(prev, &step.rope);
        w.uint(start);
        w.uint(removed);
        w.uint(inserted.len());
        for chunk in step.rope.chunks_in(inserted) {
            w.0.extend_from_slice(chunk.as_bytes());
        }
        let c = &step.cursors;
        w.uint(c.cursor);
        w.uint(c.anchor);
        w.uint(c.extra.len());
        for &(anchor, head) in &c.extra {
            w.uint(anchor);
            w.uint(head);
        }
        if w.0.len() > *budget {
            break;
        }
        *budget -= w.0.len();
        out.extend_from_slice(&w.0);
        count += 1;
        prev = &step.rope;
    }
    (count, out)
}

/// Reads `count` steps, each applied to the text before it in the walk.
fn replay(r: &mut Reader, current: &Rope, count: usize) -> Option<Vec<Step>> {
    // No capacity from `count`: it is untrusted until the steps are read.
    let mut steps: Vec<Step> = Vec::new();
    for _ in 0..count {
        let prev = steps.last().map_or(current, |s| &s.rope);
        let start = r.uint()?;
        let end = start.checked_add(r.uint()?)?;
        let inserted = r.bytes()?;
        if !on_boundary(prev, start) || !on_boundary(prev, end) {
            return None;
        }
        let inserted = std::str::from_utf8(inserted).ok()?;
        let mut rope = prev.clone();
        rope.delete(start..end);
        rope.insert(start, inserted);
        let cursor = position(r, &rope)?;
        let anchor = position(r, &rope)?;
        let mut extra = Vec::new();
        for _ in 0..r.uint()? {
            extra.push((position(r, &rope)?, position(r, &rope)?));
        }
        steps.push(Step {
            rope,
            cursors: Cursors {
                cursor,
                anchor,
                extra,
            },
        });
    }
    Some(steps)
}

/// An offset into `rope`, refused unless it is a char boundary in it.
fn position(r: &mut Reader, rope: &Rope) -> Option<usize> {
    let at = r.uint()?;
    on_boundary(rope, at).then_some(at)
}

/// Whether `at` is in bounds and not inside a multi-byte character.
fn on_boundary(rope: &Rope, at: usize) -> bool {
    at <= rope.len_bytes() && rope.byte_at(at).is_none_or(|b| b & 0xC0 != 0x80)
}

/// What turns `prev` into `next`: the start, the bytes removed there, and
/// the range of `next` inserted in their place. Shared edges are found by
/// tree identity, then pulled back onto char boundaries.
fn span(prev: &Rope, next: &Rope) -> (usize, usize, std::ops::Range<usize>) {
    let (prefix, suffix) = prev.unchanged_edges(next);
    let shorter = prev.len_bytes().min(next.len_bytes());
    let mut prefix = prefix.min(shorter);
    while !(on_boundary(prev, prefix) && on_boundary(next, prefix)) {
        prefix -= 1;
    }
    let mut suffix = suffix.min(shorter - prefix);
    while !(on_boundary(prev, prev.len_bytes() - suffix)
        && on_boundary(next, next.len_bytes() - suffix))
    {
        suffix -= 1;
    }
    let removed = prev.len_bytes() - prefix - suffix;
    (prefix, removed, prefix..next.len_bytes() - suffix)
}

/// LEB128 integers and length-prefixed bytes.
struct Writer(Vec<u8>);

impl Writer {
    fn uint(&mut self, value: usize) {
        let mut v = value as u64;
        loop {
            let low = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                self.0.push(low);
                return;
            }
            self.0.push(low | 0x80);
        }
    }
}

/// The other side of [`Writer`]. Every read is `None` past the end.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.0.len() {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }

    fn uint(&mut self) -> Option<usize> {
        let mut value: u64 = 0;
        for shift in (0..64).step_by(7) {
            let byte = *self.take(1)?.first()?;
            let low = u64::from(byte & 0x7F);
            // The tenth byte may only carry the top bit of a u64.
            if shift == 63 && low > 1 {
                return None;
            }
            value |= low << shift;
            if byte & 0x80 == 0 {
                return usize::try_from(value).ok();
            }
        }
        None
    }

    fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.uint()?;
        self.take(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic xorshift, so failures reproduce. Not worth a dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: usize) -> usize {
            if n == 0 {
                0
            } else {
                (self.next() % n as u64) as usize
            }
        }
    }

    const PIECES: &[&str] = &["a", "b", "é", "🌍", "\n", "漢", " ", "xyz", "\r\n"];

    fn words(rng: &mut Rng, max: usize) -> String {
        (0..rng.below(max + 1))
            .map(|_| PIECES[rng.below(PIECES.len())])
            .collect()
    }

    fn boundary(rng: &mut Rng, text: &str) -> usize {
        let mut at = rng.below(text.len() + 1);
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        at
    }

    fn cursors(rng: &mut Rng, text: &str) -> Cursors {
        Cursors {
            cursor: boundary(rng, text),
            anchor: boundary(rng, text),
            extra: (0..rng.below(4))
                .map(|_| (boundary(rng, text), boundary(rng, text)))
                .collect(),
        }
    }

    /// A history of `n` states, each an edit of the one before: the rope as
    /// the buffer would hold it, the oracle text, and its cursors.
    fn history(rng: &mut Rng, n: usize) -> Vec<(Step, String)> {
        let mut text = words(rng, 40);
        let mut rope = Rope::from_text(&text);
        let mut out = Vec::new();
        for _ in 0..n {
            match rng.below(10) {
                // Replaced wholesale, as a reload does: nothing shared.
                0 => {
                    text = words(rng, 40);
                    rope = Rope::from_text(&text);
                }
                // The same text again, in the same tree.
                1 => {}
                _ => {
                    let a = boundary(rng, &text);
                    let b = boundary(rng, &text);
                    let (start, end) = (a.min(b), a.max(b));
                    let end = if rng.below(2) == 0 { start } else { end };
                    let inserted = words(rng, 6);
                    text.replace_range(start..end, &inserted);
                    rope.delete(start..end);
                    rope.insert(start, &inserted);
                }
            }
            let cursors = cursors(rng, &text);
            out.push((
                Step {
                    rope: rope.clone(),
                    cursors,
                },
                text.clone(),
            ));
        }
        out
    }

    /// Splits a history at `current` into the buffer's stacks.
    fn stacks(states: &[(Step, String)], current: usize) -> (Vec<Step>, Vec<Step>) {
        let undo = states[..current].iter().map(|(s, _)| s.clone()).collect();
        let redo = states[current + 1..]
            .iter()
            .rev()
            .map(|(s, _)| s.clone())
            .collect();
        (undo, redo)
    }

    fn assert_steps(got: &[Step], want: &[&(Step, String)]) {
        assert_eq!(got.len(), want.len());
        for (got, (step, text)) in got.iter().zip(want) {
            assert_eq!(got.rope.to_string(), *text);
            assert_eq!(got.rope.len_bytes(), text.len());
            assert_eq!(got.cursors, step.cursors);
        }
    }

    fn reseal(mut body: Vec<u8>) -> Vec<u8> {
        let sum = crate::platform::fnv1a(&body);
        body.extend_from_slice(&sum.to_le_bytes());
        body
    }

    fn unsealed(bytes: &[u8]) -> Vec<u8> {
        bytes[..bytes.len() - CHECKSUM_BYTES].to_vec()
    }

    /// A small journal with multi-byte text and extra cursors on both sides.
    fn sample() -> (Rope, Vec<u8>) {
        let mut rng = Rng(0x5eed);
        let states = history(&mut rng, 8);
        let (undo, redo) = stacks(&states, 4);
        let current = states[4].0.rope.clone();
        let bytes = encode(&current, &undo, &redo);
        (current, bytes)
    }

    #[test]
    fn undo_journal_round_trips_every_step() {
        for seed in 1..400u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let n = 1 + rng.below(30);
            let states = history(&mut rng, n);
            let current = rng.below(n);
            let (undo, redo) = stacks(&states, current);
            let base = &states[current].0.rope;
            let bytes = encode(base, &undo, &redo);
            let journal = decode(&bytes, base).expect("decodes");
            let want_undo: Vec<_> = states[..current].iter().collect();
            let want_redo: Vec<_> = states[current + 1..].iter().rev().collect();
            assert_steps(&journal.undo, &want_undo);
            assert_steps(&journal.redo, &want_redo);
        }
    }

    #[test]
    fn undo_journal_decoded_stacks_step_like_the_buffer() {
        for seed in 1..200u64 {
            let mut rng = Rng(seed.wrapping_mul(0xD1B5_4A32_D192_ED03) | 1);
            let n = 1 + rng.below(25);
            let states = history(&mut rng, n);
            let mut at = rng.below(n);
            let (undo, redo) = stacks(&states, at);
            let journal = decode(
                &encode(&states[at].0.rope, &undo, &redo),
                &states[at].0.rope,
            )
            .expect("decodes");
            let (mut undo, mut redo) = (journal.undo, journal.redo);
            let mut now = states[at].0.clone();
            // As `step_history`: pop one side, push the current state on the
            // other, and restore what was popped.
            for _ in 0..60 {
                let undoing = rng.below(2) == 0;
                let taken = if undoing { undo.pop() } else { redo.pop() };
                let Some(taken) = taken else {
                    assert_eq!(at, if undoing { 0 } else { n - 1 });
                    continue;
                };
                if undoing {
                    redo.push(now);
                    at -= 1;
                } else {
                    undo.push(now);
                    at += 1;
                }
                now = taken;
                assert_eq!(now.rope.to_string(), states[at].1);
                assert_eq!(now.cursors, states[at].0.cursors);
            }
        }
    }

    #[test]
    fn undo_journal_keeps_the_newest_thousand_each_way() {
        let mut rng = Rng(7);
        let states = history(&mut rng, 3001);
        let (undo, redo) = stacks(&states, 1500);
        let base = &states[1500].0.rope;
        let journal = decode(&encode(base, &undo, &redo), base).expect("decodes");
        let want_undo: Vec<_> = states[500..1500].iter().collect();
        let want_redo: Vec<_> = states[1501..2501].iter().rev().collect();
        assert_steps(&journal.undo, &want_undo);
        assert_steps(&journal.redo, &want_redo);
    }

    #[test]
    fn undo_journal_trims_oldest_undo_then_furthest_redo_to_the_cap() {
        // Unrelated texts, so every step is stored whole.
        let big = |i: usize| {
            let text = char::from(b'a' + (i % 26) as u8)
                .to_string()
                .repeat(300_000);
            let step = Step {
                rope: Rope::from_text(&text),
                cursors: Cursors {
                    cursor: i,
                    anchor: 0,
                    extra: vec![],
                },
            };
            (step, text)
        };
        let states: Vec<_> = (0..41).map(big).collect();

        // Undo only: the newest that fit are kept.
        let (undo, _) = stacks(&states[..21], 20);
        let base = &states[20].0.rope;
        let bytes = encode(base, &undo, &[]);
        assert!(bytes.len() <= MAX_BYTES);
        let journal = decode(&bytes, base).expect("decodes");
        let kept = journal.undo.len();
        assert!(kept > 0 && kept < 20);
        let want: Vec<_> = states[20 - kept..20].iter().collect();
        assert_steps(&journal.undo, &want);

        // Both ways, more redo than fits: all undo goes, then the furthest
        // redo.
        let (undo, redo) = stacks(&states, 10);
        let base = &states[10].0.rope;
        let bytes = encode(base, &undo, &redo);
        assert!(bytes.len() <= MAX_BYTES);
        let journal = decode(&bytes, base).expect("decodes");
        assert!(journal.undo.is_empty());
        let kept = journal.redo.len();
        assert!(kept > 0 && kept < 30);
        let want: Vec<_> = states[11..11 + kept].iter().rev().collect();
        assert_steps(&journal.redo, &want);

        // Undo that fits beside the redo stays.
        let (undo, redo) = stacks(&states[..16], 10);
        let journal = decode(&encode(base, &undo, &redo), base).expect("decodes");
        assert_eq!(journal.redo.len(), 5);
        let kept = journal.undo.len();
        assert!(kept > 0 && kept < 10);
        let want: Vec<_> = states[10 - kept..10].iter().collect();
        assert_steps(&journal.undo, &want);
    }

    #[test]
    fn undo_journal_handles_empty_journal_and_empty_text() {
        let empty = Rope::new();
        let journal = decode(&encode(&empty, &[], &[]), &empty).expect("decodes");
        assert!(journal.undo.is_empty() && journal.redo.is_empty());

        let blank = Step {
            rope: Rope::new(),
            cursors: Cursors::default(),
        };
        let text = Step {
            rope: Rope::from_text("é🌍\n"),
            cursors: Cursors {
                cursor: 2,
                anchor: 6,
                extra: vec![(7, 0)],
            },
        };
        let bytes = encode(
            &empty,
            &[text.clone(), blank.clone()],
            std::slice::from_ref(&blank),
        );
        let journal = decode(&bytes, &empty).expect("decodes");
        assert_eq!(journal.undo.len(), 2);
        assert_eq!(journal.undo[0].rope.to_string(), "é🌍\n");
        assert_eq!(journal.undo[0].cursors, text.cursors);
        assert!(journal.undo[1].rope.is_empty());
        assert_eq!(journal.redo.len(), 1);
        assert!(journal.redo[0].rope.is_empty());
    }

    #[test]
    fn undo_journal_rejects_truncation_and_flips() {
        let (base, bytes) = sample();
        assert!(decode(&bytes, &base).is_some());
        assert!(decode(&[], &base).is_none());
        for len in 0..bytes.len() {
            assert!(decode(&bytes[..len], &base).is_none(), "truncated to {len}");
        }
        for at in 0..bytes.len() {
            for flip in [0x01, 0x80, 0xFF] {
                let mut damaged = bytes.clone();
                damaged[at] ^= flip;
                assert!(decode(&damaged, &base).is_none(), "flip {flip:#x} at {at}");
            }
        }
        // Past the checksum, the parser itself must refuse or survive any
        // damage: a resealed cut or flip never panics.
        let body = unsealed(&bytes);
        for len in 0..body.len() {
            assert!(decode(&reseal(body[..len].to_vec()), &base).is_none());
        }
        for at in 0..body.len() {
            for flip in [0x01, 0x40, 0x80, 0xFF] {
                let mut damaged = body.clone();
                damaged[at] ^= flip;
                let _ = decode(&reseal(damaged), &base);
            }
        }
    }

    #[test]
    fn undo_journal_rejects_other_versions_magic_and_bases() {
        let (base, bytes) = sample();
        let body = unsealed(&bytes);
        let mut newer = body.clone();
        newer[MAGIC.len()] = 2;
        assert!(decode(&reseal(newer), &base).is_none());
        let mut magic = body.clone();
        magic[0] ^= 0x20;
        assert!(decode(&reseal(magic), &base).is_none());
        let mut trailing = body;
        trailing.push(0);
        assert!(decode(&reseal(trailing), &base).is_none());

        let mut longer = base.clone();
        longer.insert(0, "x");
        assert!(decode(&bytes, &longer).is_none());
        assert!(decode(&bytes, &Rope::new()).is_none());
    }

    #[test]
    fn undo_journal_rejects_huge_counts_and_lengths() {
        let base = Rope::from_text("abc");
        let frame = |tail: &[usize]| {
            let mut w = Writer(Vec::new());
            w.0.extend_from_slice(MAGIC);
            w.0.push(VERSION);
            w.uint(3);
            for &n in tail {
                w.uint(n);
            }
            reseal(w.0)
        };
        // Undo count, redo count, then a step: start, removed, inserted
        // length, cursor, anchor, extra count.
        let max = usize::MAX;
        assert!(decode(&frame(&[max, 0]), &base).is_none());
        assert!(decode(&frame(&[0, max]), &base).is_none());
        assert!(decode(&frame(&[1, 0, 0, 0, max]), &base).is_none());
        assert!(decode(&frame(&[1, 0, max, 0, 0, 0, 0, 0]), &base).is_none());
        assert!(decode(&frame(&[1, 0, 2, max, 0, 0, 0, 0]), &base).is_none());
        assert!(decode(&frame(&[1, 0, 0, 0, 0, 0, 0, max]), &base).is_none());
        assert!(decode(&frame(&[1, 0, 0, 0, 0, max, 0, 0]), &base).is_none());
        // The same frame with sane numbers decodes, so the above failed on
        // the numbers alone.
        let journal = decode(&frame(&[1, 0, 0, 0, 0, 3, 1, 0]), &base).expect("decodes");
        assert_eq!(journal.undo[0].rope.to_string(), "abc");
        // A varint longer than a u64.
        let mut w = Writer(Vec::new());
        w.0.extend_from_slice(MAGIC);
        w.0.push(VERSION);
        w.0.extend_from_slice(&[0xFF; 11]);
        w.0.push(0);
        assert!(decode(&reseal(w.0), &base).is_none());
    }

    #[test]
    fn undo_journal_rejects_cursors_that_do_not_fit_their_text() {
        let base = Rope::from_text("base");
        let step = |text: &str, cursors: Cursors| Step {
            rope: Rope::from_text(text),
            cursors,
        };
        let at = |cursor, anchor, extra: Vec<(usize, usize)>| Cursors {
            cursor,
            anchor,
            extra,
        };
        let bad = [
            step("ab", at(3, 0, vec![])),
            step("ab", at(0, 3, vec![])),
            step("ab", at(0, 0, vec![(0, 3)])),
            step("ab", at(0, 0, vec![(3, 0)])),
            step("é", at(1, 0, vec![])),
            step("a🌍", at(0, 3, vec![])),
            step("a🌍", at(0, 0, vec![(2, 1)])),
        ];
        for bad in bad {
            let fine = step("é", at(2, 0, vec![]));
            let one = std::slice::from_ref;
            assert!(decode(&encode(&base, one(&fine), &[]), &base).is_some());
            assert!(decode(&encode(&base, one(&bad), &[]), &base).is_none());
            assert!(decode(&encode(&base, &[], one(&bad)), &base).is_none());
            assert!(decode(&encode(&base, &[bad, fine.clone()], &[]), &base).is_none());
        }
    }

    #[test]
    fn undo_journal_rejects_spans_inside_a_character() {
        let base = Rope::from_text("é🌍");
        let frame = |start: usize, removed: usize| {
            let mut w = Writer(Vec::new());
            w.0.extend_from_slice(MAGIC);
            w.0.push(VERSION);
            w.uint(6);
            for n in [1, 0, start, removed, 0, 0, 0, 0] {
                w.uint(n);
            }
            reseal(w.0)
        };
        assert!(decode(&frame(0, 2), &base).is_some());
        assert!(decode(&frame(1, 0), &base).is_none());
        assert!(decode(&frame(0, 1), &base).is_none());
        assert!(decode(&frame(2, 1), &base).is_none());
        assert!(decode(&frame(2, 5), &base).is_none());
        assert!(decode(&frame(7, 0), &base).is_none());
    }
}
