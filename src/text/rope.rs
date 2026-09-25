//! A persistent rope: the data structure the whole editor sits on.
//!
//! Written here rather than pulled from crates.io on purpose. The rope's API
//! shape dictates undo, multi-cursor, syntax-tree sync and (later) CRDT
//! merging, so it is core to the product rather than plumbing.
//!
//! Structure: a B-tree whose leaves hold UTF-8 chunks of at most
//! [`MAX_BYTES`], and whose internal nodes cache a [`Summary`] of everything
//! below them. The summary is what makes "which byte does line 40,000 start
//! at" an O(log n) descent instead of a scan.
//!
//! Nodes sit behind [`Arc`] and are never mutated in place. Edits rebuild the
//! O(log n) nodes along one path and share everything else. That makes
//! [`Rope::clone`] O(1), which is the property the rest of the editor is
//! going to lean on hard: a background thread can hold a consistent snapshot
//! and parse it while the user keeps typing into the live buffer, with no
//! lock and no copy.

use super::columns::{TAB_WIDTH, advance};
use std::sync::Arc;

/// Maximum bytes in a leaf. Sized so a leaf plus its header lands in a couple
/// of cache lines, and so the memmove on a small edit stays trivial.
const MAX_BYTES: usize = 1024;

/// Maximum children per internal node. Fanout of 8 keeps a 100MB file about
/// 7 levels deep.
const MAX_CHILDREN: usize = 8;

/// Cached measurements of a subtree. Every internal node stores the sum of
/// its children's summaries, so a descent can pick the right child without
/// touching any text.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Summary {
    /// Total bytes of UTF-8.
    pub bytes: usize,
    /// Unicode scalar values, for logarithmic byte/column mapping.
    pub chars: usize,
    /// UTF-16 units for the native composed-character bridge.
    pub utf16: usize,
    /// Count of `\n`. Note this is line *terminators*, not display lines: a
    /// buffer with no trailing newline has `lines + 1` renderable lines.
    pub lines: usize,
    /// Visual advance for each possible incoming tab-stop residue.
    visual: [usize; TAB_WIDTH],
    /// CR and LF stop fallback hit testing.
    breaks: usize,
}

impl Summary {
    fn of(s: &str) -> Self {
        Summary {
            bytes: s.len(),
            chars: s.chars().count(),
            utf16: s.encode_utf16().count(),
            lines: count_newlines(s.as_bytes()),
            visual: {
                let mut columns: [usize; TAB_WIDTH] = std::array::from_fn(|i| i);
                for ch in s.chars() {
                    for column in &mut columns {
                        *column = advance(*column, ch);
                    }
                }
                std::array::from_fn(|i| columns[i] - i)
            },
            breaks: s.bytes().filter(|&b| b == b'\r' || b == b'\n').count(),
        }
    }
}

impl std::ops::Add for Summary {
    type Output = Summary;
    fn add(self, rhs: Summary) -> Summary {
        Summary {
            bytes: self.bytes + rhs.bytes,
            chars: self.chars + rhs.chars,
            utf16: self.utf16 + rhs.utf16,
            lines: self.lines + rhs.lines,
            visual: std::array::from_fn(|i| {
                let left = self.visual[i];
                left + rhs.visual[(i + left) % TAB_WIDTH]
            }),
            breaks: self.breaks + rhs.breaks,
        }
    }
}

/// Counts `\n` bytes. Scanning bytes rather than chars is safe because `\n`
/// cannot appear inside a multi-byte UTF-8 sequence (continuation bytes are
/// all >= 0x80).
fn count_newlines(s: &[u8]) -> usize {
    s.iter().filter(|&&b| b == b'\n').count()
}

#[derive(Debug)]
enum Node {
    Leaf {
        text: Box<str>,
        summary: Summary,
    },
    Internal {
        children: Vec<Arc<Node>>,
        summary: Summary,
        depth: u8,
    },
}

impl Node {
    fn summary(&self) -> Summary {
        match self {
            Node::Leaf { summary, .. } => *summary,
            Node::Internal { summary, .. } => *summary,
        }
    }

    fn depth(&self) -> u8 {
        match self {
            Node::Leaf { .. } => 0,
            Node::Internal { depth, .. } => *depth,
        }
    }

    fn children(&self) -> &[Arc<Node>] {
        match self {
            Node::Leaf { .. } => &[],
            Node::Internal { children, .. } => children,
        }
    }

    fn leaf(text: Box<str>) -> Arc<Node> {
        let summary = Summary::of(&text);
        Arc::new(Node::Leaf { text, summary })
    }

    fn internal(children: Vec<Arc<Node>>) -> Arc<Node> {
        debug_assert!(!children.is_empty());
        debug_assert!(children.len() <= MAX_CHILDREN);
        let depth = children[0].depth() + 1;
        debug_assert!(children.iter().all(|c| c.depth() + 1 == depth));
        let summary = children
            .iter()
            .fold(Summary::default(), |acc, c| acc + c.summary());
        Arc::new(Node::Internal {
            children,
            summary,
            depth,
        })
    }
}

/// Groups same-depth nodes into parents, splitting when a parent would
/// overflow [`MAX_CHILDREN`]. Returns nodes one level deeper.
fn group(nodes: Vec<Arc<Node>>) -> Vec<Arc<Node>> {
    let mut out = Vec::with_capacity(nodes.len().div_ceil(MAX_CHILDREN));
    let mut batch = Vec::with_capacity(MAX_CHILDREN);
    let total = nodes.len();
    let mut remaining = total;

    for node in nodes {
        batch.push(node);
        remaining -= 1;
        // Avoid leaving a final parent with a single child: if finishing this
        // batch would strand exactly one node, close the batch a child early.
        let would_strand = remaining == 1 && batch.len() == MAX_CHILDREN;
        if batch.len() == MAX_CHILDREN && !would_strand {
            out.push(Node::internal(std::mem::take(&mut batch)));
            batch.reserve(MAX_CHILDREN);
        } else if would_strand {
            let carry = batch.pop().expect("batch is full here");
            out.push(Node::internal(std::mem::take(&mut batch)));
            batch.push(carry);
        }
    }
    if !batch.is_empty() {
        out.push(Node::internal(batch));
    }
    out
}

/// Builds a balanced tree bottom-up from same-depth nodes. O(n), unlike
/// folding with `concat`, which matters when opening a large file.
fn from_nodes(mut nodes: Vec<Arc<Node>>) -> Arc<Node> {
    debug_assert!(!nodes.is_empty());
    while nodes.len() > 1 {
        nodes = group(nodes);
    }
    nodes.pop().expect("at least one node")
}

/// Joins two trees, restoring the B-tree depth invariant.
///
/// The interesting case is unequal depth: the shallower tree is pushed down
/// the deeper tree's edge until the depths match, and any overflow propagates
/// back up. The returned node is at most one level deeper than the deeper
/// input, which callers rely on.
fn concat(a: Arc<Node>, b: Arc<Node>) -> Arc<Node> {
    if a.summary().bytes == 0 {
        return b;
    }
    if b.summary().bytes == 0 {
        return a;
    }

    let (da, db) = (a.depth(), b.depth());

    if da == db {
        // Two small leaves are merged rather than parented, which is what
        // keeps repeated single-character inserts from fragmenting the tree
        // into thousands of near-empty leaves.
        if da == 0 && a.summary().bytes + b.summary().bytes <= MAX_BYTES {
            let (Node::Leaf { text: ta, .. }, Node::Leaf { text: tb, .. }) = (&*a, &*b) else {
                unreachable!("depth 0 is always a leaf")
            };
            let mut merged = String::with_capacity(ta.len() + tb.len());
            merged.push_str(ta);
            merged.push_str(tb);
            return Node::leaf(merged.into_boxed_str());
        }
        return Node::internal(vec![a, b]);
    }

    if da > db {
        let kids = a.children();
        let (last, rest) = kids.split_last().expect("internal node has children");
        let merged = concat(Arc::clone(last), b);
        let mut new_kids: Vec<Arc<Node>> = rest.to_vec();
        if merged.depth() == da - 1 {
            new_kids.push(merged);
        } else {
            debug_assert_eq!(merged.depth(), da);
            new_kids.extend(merged.children().iter().cloned());
        }
        from_nodes(new_kids)
    } else {
        let kids = b.children();
        let (first, rest) = kids.split_first().expect("internal node has children");
        let merged = concat(a, Arc::clone(first));
        let mut new_kids: Vec<Arc<Node>> = Vec::with_capacity(rest.len() + MAX_CHILDREN);
        if merged.depth() == db - 1 {
            new_kids.push(merged);
        } else {
            debug_assert_eq!(merged.depth(), db);
            new_kids.extend(merged.children().iter().cloned());
        }
        new_kids.extend(rest.iter().cloned());
        from_nodes(new_kids)
    }
}

/// Splits a tree at a byte offset, which must be a char boundary.
fn split(node: &Arc<Node>, at: usize) -> (Arc<Node>, Arc<Node>) {
    match &**node {
        Node::Leaf { text, .. } => {
            debug_assert!(text.is_char_boundary(at));
            let (l, r) = text.split_at(at);
            (Node::leaf(l.into()), Node::leaf(r.into()))
        }
        Node::Internal { children, .. } => {
            let mut offset = 0;
            for (i, child) in children.iter().enumerate() {
                let child_bytes = child.summary().bytes;
                if at < offset + child_bytes {
                    let (cl, cr) = split(child, at - offset);
                    let left = children[..i]
                        .iter()
                        .fold(Node::leaf("".into()), |acc, c| concat(acc, Arc::clone(c)));
                    let right = children[i + 1..]
                        .iter()
                        .fold(cr, |acc, c| concat(acc, Arc::clone(c)));
                    return (concat(left, cl), right);
                }
                offset += child_bytes;
            }
            // `at` is exactly the end of this subtree.
            (Arc::clone(node), Node::leaf("".into()))
        }
    }
}

/// An immutable UTF-8 text rope. Cloning is O(1) and shares structure.
#[derive(Clone, Debug)]
pub struct Rope {
    root: Arc<Node>,
}

impl Rope {
    pub fn new() -> Self {
        Rope {
            root: Node::leaf("".into()),
        }
    }

    /// Bulk-loads text, building the tree bottom-up in one pass.
    pub fn from_text(text: &str) -> Self {
        if text.is_empty() {
            return Rope::new();
        }
        let mut leaves = Vec::with_capacity(text.len().div_ceil(MAX_BYTES));
        let mut rest = text;
        while !rest.is_empty() {
            let take = chunk_boundary(rest, MAX_BYTES);
            let (chunk, tail) = rest.split_at(take);
            leaves.push(Node::leaf(chunk.into()));
            rest = tail;
        }
        Rope {
            root: from_nodes(leaves),
        }
    }

    pub fn len_bytes(&self) -> usize {
        self.root.summary().bytes
    }

    /// Number of renderable lines. A buffer not ending in `\n` still shows a
    /// final line, and an empty buffer still shows one.
    pub fn len_lines(&self) -> usize {
        self.root.summary().lines + 1
    }

    pub fn is_empty(&self) -> bool {
        self.len_bytes() == 0
    }

    /// Whether this is the very same tree as `other`, not merely equal text.
    ///
    /// O(1), and true for a clone until either side is edited, which is what
    /// lets a per-frame observer tell "unchanged since I last looked" without
    /// comparing any text.
    pub fn same_as(&self, other: &Rope) -> bool {
        Arc::ptr_eq(&self.root, &other.root)
    }

    /// Inserts at a byte offset. Panics if the offset is out of bounds or
    /// lands inside a multi-byte character.
    pub fn insert(&mut self, at: usize, text: &str) {
        assert!(at <= self.len_bytes(), "insert past end of rope");
        if text.is_empty() {
            return;
        }
        let (l, r) = split(&self.root, at);
        let mid = Rope::from_text(text).root;
        self.root = concat(concat(l, mid), r);
    }

    /// Removes a byte range. Both ends must be char boundaries.
    pub fn delete(&mut self, range: std::ops::Range<usize>) {
        assert!(range.start <= range.end, "inverted range");
        assert!(range.end <= self.len_bytes(), "delete past end of rope");
        if range.start == range.end {
            return;
        }
        let (l, rest) = split(&self.root, range.start);
        let (_, r) = split(&rest, range.end - range.start);
        self.root = concat(l, r);
    }

    /// Byte offset where `line` begins. `line` is 0-based; passing
    /// `len_lines()` yields the end of the buffer.
    pub fn line_to_byte(&self, line: usize) -> usize {
        assert!(line < self.len_lines() + 1, "line out of range");
        if line == 0 {
            return 0;
        }
        let mut node = &self.root;
        let mut remaining = line;
        let mut offset = 0;

        loop {
            match &**node {
                Node::Leaf { text, .. } => {
                    // Walk to just past the `remaining`-th newline.
                    let mut seen = 0;
                    for (i, b) in text.as_bytes().iter().enumerate() {
                        if *b == b'\n' {
                            seen += 1;
                            if seen == remaining {
                                return offset + i + 1;
                            }
                        }
                    }
                    return offset + text.len();
                }
                Node::Internal { children, .. } => {
                    let mut next = None;
                    for child in children {
                        let s = child.summary();
                        if remaining <= s.lines {
                            next = Some(child);
                            break;
                        }
                        remaining -= s.lines;
                        offset += s.bytes;
                    }
                    match next {
                        Some(c) => node = c,
                        None => return offset,
                    }
                }
            }
        }
    }

    /// Which line contains `byte`.
    pub fn byte_to_line(&self, byte: usize) -> usize {
        assert!(byte <= self.len_bytes(), "byte out of range");
        let mut node = &self.root;
        let mut remaining = byte;
        let mut line = 0;

        loop {
            match &**node {
                Node::Leaf { text, .. } => {
                    return line + count_newlines(&text.as_bytes()[..remaining]);
                }
                Node::Internal { children, .. } => {
                    let mut next = None;
                    for child in children {
                        let s = child.summary();
                        if remaining < s.bytes {
                            next = Some(child);
                            break;
                        }
                        remaining -= s.bytes;
                        line += s.lines;
                    }
                    match next {
                        Some(c) => node = c,
                        None => return line,
                    }
                }
            }
        }
    }

    /// Visual advance over a character-aligned range, starting at column zero.
    /// Whole subtrees compose their tab-aware summaries; only edge leaves scan.
    pub fn visual_column(&self, range: std::ops::Range<usize>) -> usize {
        fn visit(node: &Node, base: usize, range: &std::ops::Range<usize>, column: &mut usize) {
            let summary = node.summary();
            if base >= range.end || base + summary.bytes <= range.start {
                return;
            }
            if range.start <= base && base + summary.bytes <= range.end {
                *column += summary.visual[*column % TAB_WIDTH];
                return;
            }
            match node {
                Node::Leaf { text, .. } => {
                    let start = range.start.saturating_sub(base);
                    let end = (range.end - base).min(text.len());
                    for ch in text[start..end].chars() {
                        *column = advance(*column, ch);
                    }
                }
                Node::Internal { children, .. } => {
                    let mut at = base;
                    for child in children {
                        visit(child, at, range, column);
                        at += child.summary().bytes;
                    }
                }
            }
        }
        let mut column = 0;
        visit(&self.root, 0, &range, &mut column);
        column
    }

    /// Byte and column at or immediately before `target`, clamped to the first
    /// CR/LF or range end. The incoming column is zero at the range start.
    /// Skips complete subtrees; a tab or wide character is never split.
    pub fn visual_seek(&self, range: std::ops::Range<usize>, target: usize) -> (usize, usize) {
        fn visit(
            node: &Node,
            base: usize,
            range: &std::ops::Range<usize>,
            target: usize,
            byte: &mut usize,
            column: &mut usize,
        ) -> bool {
            let summary = node.summary();
            if base >= range.end || base + summary.bytes <= range.start {
                return false;
            }
            let next = *column + summary.visual[*column % TAB_WIDTH];
            if range.start <= base
                && base + summary.bytes <= range.end
                && summary.breaks == 0
                && next < target
            {
                *column = next;
                *byte = base + summary.bytes;
                return false;
            }
            match node {
                Node::Leaf { text, .. } => {
                    let start = range.start.saturating_sub(base);
                    let end = (range.end - base).min(text.len());
                    for (offset, ch) in text[start..end].char_indices() {
                        *byte = base + start + offset;
                        if *column >= target || ch == '\r' || ch == '\n' {
                            return true;
                        }
                        let next = advance(*column, ch);
                        if next > target {
                            return true;
                        }
                        *column = next;
                        *byte += ch.len_utf8();
                    }
                    false
                }
                Node::Internal { children, .. } => {
                    let mut at = base;
                    for child in children {
                        if visit(child, at, range, target, byte, column) {
                            return true;
                        }
                        at += child.summary().bytes;
                    }
                    false
                }
            }
        }
        let mut byte = range.start.min(self.len_bytes());
        let mut column = 0;
        visit(&self.root, 0, &range, target, &mut byte, &mut column);
        (byte, column)
    }

    /// Identity of immutable snapshots, without reading or hashing text.
    pub fn same_snapshot(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.root, &other.root)
    }

    /// Proven equal prefix/suffix byte counts, with no hashing or full scan.
    /// Shared subtrees are skipped by identity. Work is capped per edge; when
    /// unrelated snapshots exhaust the cap, the unexamined middle is treated
    /// as changed. Counts may stop inside UTF-8 and may overlap (e.g. when
    /// inserting a prefix that happens to start with the same bytes).
    pub(crate) fn unchanged_edges(&self, other: &Self) -> (usize, usize) {
        fn edge(a: &Arc<Node>, b: &Arc<Node>, reverse: bool) -> usize {
            let mut a = vec![(a, 0)];
            let mut b = vec![(b, 0)];
            let mut equal = 0;
            let mut bytes_left = 8192;
            for _ in 0..256 {
                let (Some(&(an, ao)), Some(&(bn, bo))) = (a.last(), b.last()) else {
                    break;
                };
                if Arc::ptr_eq(an, bn) && ao == bo {
                    equal += an.summary().bytes - ao;
                    a.pop();
                    b.pop();
                    continue;
                }
                let expand_a = an.depth() > 0 && an.depth() >= bn.depth();
                let expand_b = bn.depth() > 0 && bn.depth() >= an.depth();
                for (stack, expand) in [(&mut a, expand_a), (&mut b, expand_b)] {
                    if expand {
                        let (node, _) = stack.pop().unwrap();
                        if reverse {
                            stack.extend(node.children().iter().map(|n| (n, 0)));
                        } else {
                            stack.extend(node.children().iter().rev().map(|n| (n, 0)));
                        }
                    }
                }
                if expand_a || expand_b {
                    continue;
                }
                let (Node::Leaf { text: at, .. }, Node::Leaf { text: bt, .. }) =
                    (an.as_ref(), bn.as_ref())
                else {
                    unreachable!();
                };
                let n = (at.len() - ao).min(bt.len() - bo).min(bytes_left);
                for i in 0..n {
                    let ai = if reverse {
                        at.len() - ao - i - 1
                    } else {
                        ao + i
                    };
                    let bi = if reverse {
                        bt.len() - bo - i - 1
                    } else {
                        bo + i
                    };
                    if at.as_bytes()[ai] != bt.as_bytes()[bi] {
                        return equal + i;
                    }
                }
                equal += n;
                bytes_left -= n;
                if ao + n == at.len() {
                    a.pop();
                } else {
                    a.last_mut().unwrap().1 += n;
                }
                if bo + n == bt.len() {
                    b.pop();
                } else {
                    b.last_mut().unwrap().1 += n;
                }
                if bytes_left == 0 {
                    break;
                }
            }
            equal
        }
        let prefix = edge(&self.root, &other.root, false);
        let suffix = edge(&self.root, &other.root, true);
        (prefix, suffix)
    }

    /// Scalar index at a byte offset. An interior UTF-8 byte snaps left.
    /// Reads at most one leaf after descending cached subtree counts.
    pub fn byte_to_char(&self, byte: usize) -> usize {
        let mut node = &self.root;
        let mut remaining = byte.min(self.len_bytes());
        let mut chars = 0;
        loop {
            match &**node {
                Node::Leaf { text, .. } => {
                    while !text.is_char_boundary(remaining) {
                        remaining -= 1;
                    }
                    return chars + text[..remaining].chars().count();
                }
                Node::Internal { children, .. } => {
                    let mut next = None;
                    for child in children {
                        let s = child.summary();
                        if remaining < s.bytes {
                            next = Some(child);
                            break;
                        }
                        remaining -= s.bytes;
                        chars += s.chars;
                    }
                    match next {
                        Some(child) => node = child,
                        None => return chars,
                    }
                }
            }
        }
    }

    /// Byte offset of a Unicode scalar index, clamped to the rope end.
    pub fn char_to_byte(&self, index: usize) -> usize {
        let mut node = &self.root;
        let mut remaining = index;
        let mut byte = 0;
        loop {
            match &**node {
                Node::Leaf { text, .. } => {
                    return byte
                        + text
                            .char_indices()
                            .nth(remaining)
                            .map_or(text.len(), |(i, _)| i);
                }
                Node::Internal { children, .. } => {
                    let mut next = None;
                    for child in children {
                        let s = child.summary();
                        if remaining < s.chars {
                            next = Some(child);
                            break;
                        }
                        remaining -= s.chars;
                        byte += s.bytes;
                    }
                    match next {
                        Some(child) => node = child,
                        None => return byte,
                    }
                }
            }
        }
    }

    pub fn len_utf16(&self) -> usize {
        self.root.summary().utf16
    }

    /// UTF-16 index at a byte offset, snapping an interior byte left.
    pub fn byte_to_utf16(&self, byte: usize) -> usize {
        let mut node = &self.root;
        let mut remaining = byte.min(self.len_bytes());
        let mut units = 0;
        loop {
            match &**node {
                Node::Leaf { text, .. } => {
                    while !text.is_char_boundary(remaining) {
                        remaining -= 1;
                    }
                    return units + text[..remaining].encode_utf16().count();
                }
                Node::Internal { children, .. } => {
                    let mut next = None;
                    for child in children {
                        let s = child.summary();
                        if remaining < s.bytes {
                            next = Some(child);
                            break;
                        }
                        remaining -= s.bytes;
                        units += s.utf16;
                    }
                    match next {
                        Some(child) => node = child,
                        None => return units,
                    }
                }
            }
        }
    }

    /// Byte offset at a UTF-16 index. A low surrogate snaps to its scalar start.
    pub fn utf16_to_byte(&self, index: usize) -> usize {
        let mut node = &self.root;
        let mut remaining = index;
        let mut byte = 0;
        loop {
            match &**node {
                Node::Leaf { text, .. } => {
                    for (at, ch) in text.char_indices() {
                        if remaining < ch.len_utf16() {
                            return byte + at;
                        }
                        remaining -= ch.len_utf16();
                    }
                    return byte + text.len();
                }
                Node::Internal { children, .. } => {
                    let mut next = None;
                    for child in children {
                        let s = child.summary();
                        if remaining < s.utf16 {
                            next = Some(child);
                            break;
                        }
                        remaining -= s.utf16;
                        byte += s.bytes;
                    }
                    match next {
                        Some(child) => node = child,
                        None => return byte,
                    }
                }
            }
        }
    }

    /// Copy only a native string query's requested units, across leaf boundaries.
    pub(crate) fn read_utf16(&self, start: usize, out: &mut [u16]) {
        assert!(start <= self.len_utf16() && out.len() <= self.len_utf16() - start);
        let byte = self.utf16_to_byte(start);
        let skip = start - self.byte_to_utf16(byte);
        let units = self
            .chunks_in(byte..self.len_bytes())
            .flat_map(str::encode_utf16)
            .skip(skip);
        for (dest, unit) in out.iter_mut().zip(units) {
            *dest = unit;
        }
    }

    /// The raw byte at `idx`, or `None` past the end.
    ///
    /// Deliberately byte-level and not `char`-level: callers walking UTF-8
    /// boundaries cannot use the string-slicing helpers to do it, because
    /// slicing a `str` requires boundaries on *both* ends and finding the
    /// boundary is the whole problem.
    pub fn byte_at(&self, idx: usize) -> Option<u8> {
        if idx >= self.len_bytes() {
            return None;
        }
        let mut node = &self.root;
        let mut remaining = idx;
        loop {
            match &**node {
                Node::Leaf { text, .. } => return Some(text.as_bytes()[remaining]),
                Node::Internal { children, .. } => {
                    let mut next = None;
                    for child in children {
                        let bytes = child.summary().bytes;
                        if remaining < bytes {
                            next = Some(child);
                            break;
                        }
                        remaining -= bytes;
                    }
                    node = next?;
                }
            }
        }
    }

    /// Finds the next occurrence of `needle` at or after `from`.
    ///
    /// Walks chunks with a carry of `needle.len() - 1` bytes, so a match
    /// straddling a leaf boundary is still found. Deliberately lazy: on a
    /// 100MB buffer, rescanning everything on each keystroke of a search
    /// query would cost tens of milliseconds, while finding just the next
    /// match costs only the distance to it.
    ///
    /// `from` is a byte offset and does not have to be a char boundary:
    /// callers resume at `found + 1`, which is inside the match whenever the
    /// needle starts with a multi-byte character. UTF-8 is self-synchronising,
    /// so a valid needle can only ever match at a boundary anyway.
    pub fn find_from(&self, needle: &str, from: usize) -> Option<usize> {
        self.find_within(needle, from, self.len_bytes())
    }

    /// First occurrence of `needle` lying entirely inside `from..to`. Never
    /// reads a byte outside that range, which is what keeps the viewport
    /// search proportional to the viewport.
    fn find_within(&self, needle: &str, from: usize, to: usize) -> Option<usize> {
        let to = to.min(self.len_bytes());
        if needle.is_empty() || from >= to || needle.len() > to - from {
            return None;
        }
        let overlap = needle.len() - 1;
        let mut window: Vec<u8> = Vec::with_capacity(needle.len() * 2);
        // Byte offset in the rope of `window[0]`.
        let mut window_start = from;

        for chunk in self.bytes_in(from..to) {
            window.extend_from_slice(chunk);
            if let Some(at) = find_bytes(&window, needle.as_bytes()) {
                return Some(window_start + at);
            }
            // Keep only what a match could still straddle.
            if window.len() > overlap {
                let drop = window.len() - overlap;
                window.drain(..drop);
                window_start += drop;
            }
        }
        None
    }

    /// Finds the last occurrence of `needle` starting strictly before
    /// `before`.
    ///
    /// Scans backwards a block at a time, so the cost is the distance to the
    /// match rather than the distance from the top of the file.
    pub fn rfind_before(&self, needle: &str, before: usize) -> Option<usize> {
        const BLOCK: usize = 64 * 1024;
        if needle.is_empty() {
            return None;
        }
        let mut hi = before.min(self.len_bytes());
        while hi > 0 {
            let lo = hi.saturating_sub(BLOCK);
            // A match starting in `lo..hi` may run up to a needle past `hi`.
            let end = hi + needle.len() - 1;
            let mut last = None;
            let mut at = lo;
            while let Some(found) = self.find_within(needle, at, end) {
                last = Some(found);
                at = found + 1;
            }
            if last.is_some() {
                return last;
            }
            hi = lo;
        }
        None
    }

    /// All occurrences of `needle` overlapping a byte range.
    ///
    /// Bounded by the range, so highlighting a viewport stays proportional to
    /// the viewport rather than the file. Neither end of the range has to be
    /// a char boundary.
    pub fn find_in(&self, needle: &str, range: std::ops::Range<usize>) -> Vec<usize> {
        let mut out = Vec::new();
        if needle.is_empty() {
            return out;
        }
        // A match overlaps the range if it starts after `start - len` and
        // before `end`, so that is all that gets read.
        let mut at = range.start.saturating_sub(needle.len() - 1);
        let to = range.end.saturating_add(needle.len() - 1);
        while let Some(found) = self.find_within(needle, at, to) {
            if found >= range.end {
                break;
            }
            out.push(found);
            at = found + 1;
        }
        out
    }

    /// Iterates the chunks overlapping a byte range, in order. This is the
    /// render hot path: the viewport asks for the bytes of the visible lines
    /// and gets borrowed slices with no copying.
    ///
    /// Both ends of the range must be char boundaries, because the items are
    /// `&str`. Anything that cannot promise that (search resuming mid-match,
    /// the parser's fixed-size reads, offsets from a tree that may be stale)
    /// uses [`Rope::bytes_in`] instead.
    pub fn chunks_in(&self, range: std::ops::Range<usize>) -> Chunks<'_> {
        Chunks(Walk::new(&self.root, range))
    }

    /// As [`Rope::chunks_in`], but yields raw bytes and accepts any range:
    /// an end inside a multi-byte character is cut there, and an end past the
    /// buffer is clamped.
    pub fn bytes_in(&self, range: std::ops::Range<usize>) -> ByteChunks<'_> {
        ByteChunks(Walk::new(&self.root, range))
    }

    /// Collects a byte range into a `String`. Convenience for tests and cold
    /// paths; the renderer uses [`Rope::chunks_in`] instead.
    ///
    /// Total on purpose: a range that splits a character yields U+FFFD at the
    /// cut rather than a panic, since a panic here is an abort that takes
    /// every unsaved buffer with it.
    pub fn slice_to_string(&self, range: std::ops::Range<usize>) -> String {
        let mut out = Vec::with_capacity(range.end.saturating_sub(range.start));
        for c in self.bytes_in(range) {
            out.extend_from_slice(c);
        }
        match String::from_utf8(out) {
            Ok(text) => text,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        }
    }

    /// Text of a single line, including its trailing newline if present.
    pub fn line(&self, line: usize) -> String {
        let start = self.line_to_byte(line);
        let end = if line + 1 < self.len_lines() {
            self.line_to_byte(line + 1)
        } else {
            self.len_bytes()
        };
        self.slice_to_string(start..end)
    }
}

impl Default for Rope {
    fn default() -> Self {
        Rope::new()
    }
}

impl std::fmt::Display for Rope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for chunk in self.chunks_in(0..self.len_bytes()) {
            f.write_str(chunk)?;
        }
        Ok(())
    }
}

/// Naive substring search over bytes. Fine here: the haystack is one chunk
/// plus a small carry, never the whole buffer.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let first = needle[0];
    for i in 0..=haystack.len() - needle.len() {
        if haystack[i] == first && &haystack[i..i + needle.len()] == needle {
            return Some(i);
        }
    }
    None
}

/// Largest `n <= limit` that is a char boundary in `s`. Chunks never split a
/// character, so every leaf is independently valid UTF-8.
fn chunk_boundary(s: &str, limit: usize) -> usize {
    if s.len() <= limit {
        return s.len();
    }
    let mut n = limit;
    while n > 0 && !s.is_char_boundary(n) {
        n -= 1;
    }
    // A single character longer than the limit cannot happen (max 4 bytes vs
    // a 1KiB limit), but refuse to emit a zero-length chunk regardless.
    if n == 0 { s.len().min(limit + 4) } else { n }
}

/// Depth-first walk over the leaves overlapping a byte range. Yields each
/// leaf's text with the sub-range of it that falls inside, and leaves the
/// slicing, which is where `str` and `[u8]` differ, to the two iterators.
struct Walk<'a> {
    stack: Vec<(&'a Node, usize)>,
    /// Bytes still to skip before the range begins.
    skip: usize,
    /// Bytes still to yield.
    left: usize,
}

impl<'a> Walk<'a> {
    fn new(root: &'a Node, range: std::ops::Range<usize>) -> Self {
        Walk {
            stack: vec![(root, 0)],
            skip: range.start,
            left: range.end.saturating_sub(range.start),
        }
    }

    fn next(&mut self) -> Option<(&'a str, std::ops::Range<usize>)> {
        while self.left > 0 {
            let (node, idx) = *self.stack.last()?;
            match node {
                Node::Leaf { text, .. } => {
                    self.stack.pop();
                    if self.skip >= text.len() {
                        self.skip -= text.len();
                        continue;
                    }
                    let start = self.skip;
                    self.skip = 0;
                    let end = (start + self.left).min(text.len());
                    self.left -= end - start;
                    return Some((text, start..end));
                }
                Node::Internal { children, .. } => {
                    if idx >= children.len() {
                        self.stack.pop();
                        continue;
                    }
                    self.stack.last_mut().expect("just read it").1 = idx + 1;
                    let child = &children[idx];
                    // Skip whole subtrees that fall before the range.
                    let bytes = child.summary().bytes;
                    if self.skip >= bytes {
                        self.skip -= bytes;
                        continue;
                    }
                    self.stack.push((child, 0));
                }
            }
        }
        None
    }
}

/// The chunks overlapping a byte range, as text. See [`Rope::chunks_in`].
pub struct Chunks<'a>(Walk<'a>);

impl<'a> Iterator for Chunks<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        let (text, range) = self.0.next()?;
        Some(&text[range])
    }
}

/// The chunks overlapping a byte range, as bytes. See [`Rope::bytes_in`].
pub struct ByteChunks<'a>(Walk<'a>);

impl<'a> Iterator for ByteChunks<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        let (text, range) = self.0.next()?;
        Some(&text.as_bytes()[range])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_edges_skip_shared_megabytes_and_bound_unrelated_text() {
        let source = "é漢🌍\n".repeat(200_000);
        let old = Rope::from_text(&source);
        let at = source.len() / 2;
        let mut new = old.clone();
        new.insert(at, "inserted\n");
        // The inserted trailing newline also matches the old byte before at.
        assert_eq!(old.unchanged_edges(&new), (at, source.len() - at + 1));
        assert_eq!(new.unchanged_edges(&old), (at, source.len() - at + 1));
        assert_eq!(
            old.unchanged_edges(&old.clone()),
            (source.len(), source.len())
        );
        let unrelated = Rope::from_text(&source);
        let (prefix, suffix) = old.unchanged_edges(&unrelated);
        assert!(prefix <= 8192 && suffix <= 8192);
        assert_eq!(Rope::new().unchanged_edges(&old), (0, 0));
    }

    #[test]
    fn unchanged_edges_are_exact_across_multiple_edits_and_tree_rebalancing() {
        let mut rng = Rng(197);
        let mut source = "é\r\nאב 👩‍💻\t漢字".repeat(800);
        let mut rope = Rope::from_text(&source);
        for _ in 0..100 {
            let old = rope.clone();
            let before = source.clone();
            for edit in 0..3 {
                let boundaries: Vec<_> = source.char_indices().map(|(i, _)| i).collect();
                let index = rng.below(boundaries.len() - 4);
                let range = boundaries[index]..boundaries[index + 3];
                if edit % 2 == 0 {
                    rope.insert(range.start, "x\n🌍");
                    source.insert_str(range.start, "x\n🌍");
                } else {
                    rope.delete(range.clone());
                    source.replace_range(range, "");
                }
            }
            for (a, b, left, right) in [
                (&old, &rope, &before, &source),
                (&rope, &old, &source, &before),
            ] {
                let (prefix, suffix) = a.unchanged_edges(b);
                assert!(prefix <= left.len().min(right.len()));
                assert!(suffix <= left.len().min(right.len()));
                assert_eq!(&left.as_bytes()[..prefix], &right.as_bytes()[..prefix]);
                assert_eq!(
                    &left.as_bytes()[left.len() - suffix..],
                    &right.as_bytes()[right.len() - suffix..]
                );
            }
        }
    }

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

    /// Walks the tree asserting the invariants that make the summaries
    /// trustworthy: uniform depth, cached summaries matching reality, and no
    /// overfull nodes.
    fn check(node: &Node) -> Summary {
        match node {
            Node::Leaf { text, summary } => {
                assert!(text.len() <= MAX_BYTES, "leaf over MAX_BYTES");
                let actual = Summary::of(text);
                assert_eq!(actual, *summary, "leaf summary stale");
                actual
            }
            Node::Internal {
                children,
                summary,
                depth,
            } => {
                assert!(!children.is_empty(), "empty internal node");
                assert!(children.len() <= MAX_CHILDREN, "internal node overfull");
                let mut acc = Summary::default();
                for c in children {
                    assert_eq!(c.depth() + 1, *depth, "ragged tree depth");
                    acc = acc + check(c);
                }
                assert_eq!(acc, *summary, "internal summary stale");
                acc
            }
        }
    }

    #[test]
    fn visual_indexes_match_scanning_after_edits() {
        let mut source = "aé\t漢👩‍💻e\u{301}\r\n".repeat(900);
        let mut rope = Rope::from_text(&source);
        let snapshot = rope.clone();
        let mut rng = Rng(0xC011);
        for step in 0..20 {
            let boundaries: Vec<_> = source
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(source.len()))
                .collect();
            for _ in 0..80 {
                let a = rng.below(boundaries.len());
                let b = a + rng.below(boundaries.len() - a);
                let range = boundaries[a]..boundaries[b];
                let expected = source[range.clone()].chars().fold(0, advance);
                assert_eq!(rope.visual_column(range.clone()), expected);
                for target in [0, 1, 3, 4, 5, 11, expected, expected + 5] {
                    let mut byte = range.start;
                    let mut column = 0;
                    for ch in source[range.clone()].chars() {
                        if column >= target || ch == '\r' || ch == '\n' {
                            break;
                        }
                        let next = advance(column, ch);
                        if next > target {
                            break;
                        }
                        column = next;
                        byte += ch.len_utf8();
                    }
                    assert_eq!(rope.visual_seek(range.clone(), target), (byte, column));
                }
            }
            let a = rng.below(boundaries.len() - 4);
            let range = boundaries[a]..boundaries[a + 3];
            if step % 2 == 0 {
                source.insert_str(range.start, "漢\t\r\né");
                rope.insert(range.start, "漢\t\r\né");
            } else {
                source.replace_range(range.clone(), "");
                rope.delete(range);
            }
            check(&rope.root);
        }
        assert_eq!(snapshot.to_string(), "aé\t漢👩‍💻e\u{301}\r\n".repeat(900));
    }

    #[test]
    fn visual_seek_skips_deep_tabbed_prefixes_and_stops_at_breaks() {
        let source = "é漢\t🌍x\t".repeat(100_000);
        let rope = Rope::from_text(&(source.clone() + "\r\nlast"));
        let width = source.chars().fold(0, advance);
        assert_eq!(rope.visual_column(0..source.len()), width);
        assert_eq!(
            rope.visual_seek(0..rope.len_bytes(), width + 100),
            (source.len(), width)
        );
        let prefix = source.len() - "é漢\t🌍x\t".len();
        let column = width - 8;
        assert_eq!(
            rope.visual_seek(0..source.len(), column + 2),
            (prefix + 2, column + 1)
        );
        assert_eq!(
            rope.visual_seek(0..source.len(), column + 3),
            (prefix + 5, column + 3)
        );
        assert_eq!(
            rope.visual_seek(0..source.len(), column + 4),
            (prefix + 6, column + 4)
        );
    }

    #[test]
    fn scalar_mapping_matches_string_after_edits_and_snapshots() {
        let mut source = "aé👩‍💻e\u{301}\r\n漢字".repeat(700);
        let mut rope = Rope::from_text(&source);
        let original = rope.clone();
        for step in 0..12 {
            let boundaries: Vec<_> = source
                .char_indices()
                .map(|(byte, _)| byte)
                .chain(std::iter::once(source.len()))
                .collect();
            for (index, &byte) in boundaries.iter().enumerate() {
                assert_eq!(rope.byte_to_char(byte), index);
                assert_eq!(rope.char_to_byte(index), byte);
                if let Some(&next) = boundaries.get(index + 1) {
                    for interior in byte + 1..next {
                        assert_eq!(rope.byte_to_char(interior), index);
                    }
                }
            }
            assert_eq!(rope.char_to_byte(usize::MAX), source.len());
            assert_eq!(rope.byte_to_char(usize::MAX), boundaries.len() - 1);
            let at = boundaries[step * 101];
            if step % 2 == 0 {
                source.insert_str(at, "🇪🇸\n");
                rope.insert(at, "🇪🇸\n");
            } else {
                let end = boundaries[step * 101 + 3];
                source.replace_range(at..end, "");
                rope.delete(at..end);
            }
            check(&rope.root);
        }
        assert!(!rope.same_snapshot(&original));
        assert!(original.same_snapshot(&original.clone()));
        assert_eq!(original.to_string(), "aé👩‍💻e\u{301}\r\n漢字".repeat(700));
    }

    #[test]
    fn empty_rope_has_one_line() {
        let r = Rope::new();
        assert_eq!(r.len_bytes(), 0);
        assert_eq!(r.len_lines(), 1);
        assert_eq!(r.to_string(), "");
    }

    #[test]
    fn roundtrips_text_larger_than_a_leaf() {
        let src: String = (0..5000)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let r = Rope::from_text(&src);
        check(&r.root);
        assert_eq!(r.len_bytes(), src.len());
        assert_eq!(r.to_string(), src);
    }

    #[test]
    fn counts_lines() {
        let r = Rope::from_text("alpha\nbeta\ngamma");
        assert_eq!(r.len_lines(), 3);
        assert_eq!(r.line_to_byte(0), 0);
        assert_eq!(r.line_to_byte(1), 6);
        assert_eq!(r.line_to_byte(2), 11);
        assert_eq!(r.byte_to_line(0), 0);
        assert_eq!(r.byte_to_line(6), 1);
        assert_eq!(r.byte_to_line(15), 2);
        assert_eq!(r.line(1), "beta\n");
        assert_eq!(r.line(2), "gamma");
    }

    #[test]
    fn trailing_newline_does_not_invent_a_line() {
        let r = Rope::from_text("a\nb\n");
        // "a", "b", and the empty line after the final newline.
        assert_eq!(r.len_lines(), 3);
        assert_eq!(r.line(2), "");
    }

    #[test]
    fn preserves_multibyte_characters_across_chunks() {
        // Each emoji is 4 bytes; 2000 of them far exceeds one leaf, so the
        // chunker is forced to split repeatedly near character boundaries.
        let src: String = std::iter::repeat_n('🌍', 2000).collect();
        let r = Rope::from_text(&src);
        check(&r.root);
        assert_eq!(r.to_string(), src);
        assert_eq!(r.len_bytes(), 8000);
    }

    #[test]
    fn insert_and_delete_match_a_string_oracle() {
        let mut rng = Rng(0x5EED_1234_ABCD_9876);
        let mut rope = Rope::new();
        let mut oracle = String::new();

        for step in 0..600 {
            if step % 5 == 0 && !oracle.is_empty() {
                let a = {
                    let mut i = rng.below(oracle.len());
                    while !oracle.is_char_boundary(i) {
                        i -= 1;
                    }
                    i
                };
                let b = {
                    let mut i = a + rng.below(oracle.len() - a + 1);
                    while !oracle.is_char_boundary(i) {
                        i -= 1;
                    }
                    i
                };
                rope.delete(a..b);
                oracle.replace_range(a..b, "");
            } else {
                let at = {
                    let mut i = rng.below(oracle.len() + 1);
                    while !oracle.is_char_boundary(i) {
                        i -= 1;
                    }
                    i
                };
                let payload: String = (0..rng.below(80))
                    .map(|_| {
                        if rng.below(10) == 0 {
                            '\n'
                        } else if rng.below(7) == 0 {
                            'é'
                        } else {
                            char::from(b'a' + rng.below(26) as u8)
                        }
                    })
                    .collect();
                rope.insert(at, &payload);
                oracle.insert_str(at, &payload);
            }

            check(&rope.root);
            assert_eq!(
                rope.len_bytes(),
                oracle.len(),
                "byte length diverged at step {step}"
            );
            assert_eq!(rope.to_string(), oracle, "content diverged at step {step}");
        }

        // Line indexing must agree with the oracle after all that churn.
        assert_eq!(rope.len_lines(), oracle.matches('\n').count() + 1);
        for line in 0..rope.len_lines() {
            let start = rope.line_to_byte(line);
            assert_eq!(rope.byte_to_line(start), line, "line {line} round-trip");
        }
    }

    #[test]
    fn chunks_in_yields_exact_subranges() {
        let src: String = (0..3000)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let r = Rope::from_text(&src);
        for (start, end) in [
            (0, 0),
            (0, 1),
            (17, 900),
            (1023, 1025),
            (0, 3000),
            (2999, 3000),
        ] {
            let got: String = r.chunks_in(start..end).collect();
            assert_eq!(got, src[start..end], "range {start}..{end}");
        }
    }

    #[test]
    fn finds_matches_across_chunk_boundaries() {
        // Force a match to straddle leaves: fill past MAX_BYTES, then put the
        // needle right where a chunk boundary must fall.
        let mut src = "a".repeat(MAX_BYTES - 3);
        src.push_str("NEEDLE");
        src.push_str(&"b".repeat(MAX_BYTES));
        let r = Rope::from_text(&src);

        let expected = MAX_BYTES - 3;
        assert_eq!(r.find_from("NEEDLE", 0), Some(expected));
        assert_eq!(r.find_from("NEEDLE", expected), Some(expected));
        assert_eq!(r.find_from("NEEDLE", expected + 1), None);
    }

    #[test]
    fn find_matches_a_string_oracle() {
        let src = "the quick brown fox jumps over the lazy dog, the end";
        let r = Rope::from_text(src);
        for needle in ["the", "o", "fox", "zzz", "the end", "t"] {
            let mut expected = Vec::new();
            let mut at = 0;
            while let Some(i) = src[at..].find(needle) {
                expected.push(at + i);
                at += i + 1;
            }
            let got = r.find_in(needle, 0..r.len_bytes());
            assert_eq!(got, expected, "needle {needle:?}");
        }
    }

    #[test]
    fn find_in_is_bounded_by_its_range() {
        let r = Rope::from_text("xx target xx target xx");
        assert_eq!(r.find_in("target", 0..10), vec![3]);
        assert_eq!(r.find_in("target", 10..22), vec![13]);
    }

    #[test]
    fn rfind_walks_backwards() {
        let r = Rope::from_text("a b a b a");
        assert_eq!(r.rfind_before("a", 9), Some(8));
        assert_eq!(r.rfind_before("a", 8), Some(4));
        // Index 0 holds an "a", and 0 is strictly before 1.
        assert_eq!(r.rfind_before("a", 1), Some(0));
        assert_eq!(r.rfind_before("a", 0), None, "nothing is before the start");
    }

    /// Every search entry point used to resume at `found + 1`, which is
    /// inside the match when the needle starts with a multi-byte character,
    /// and the chunk iterator sliced a `str` there. Typing an accent into the
    /// find bar aborted the app.
    #[test]
    fn search_survives_multibyte_needles_and_offsets() {
        let text = "caf\u{e9} \u{e9}t\u{e9} \u{1f600}\u{1f600} \u{65e5}\u{672c}";
        let rope = Rope::from_text(text);
        let len = rope.len_bytes();

        for needle in ["\u{e9}", "\u{1f600}", "\u{672c}", "t\u{e9}"] {
            let expected: Vec<usize> = text.match_indices(needle).map(|(i, _)| i).collect();
            assert_eq!(rope.find_in(needle, 0..len), expected, "find_in {needle}");

            // Walk forwards the way Find Next does.
            let mut got = Vec::new();
            let mut at = 0;
            while let Some(found) = rope.find_from(needle, at) {
                got.push(found);
                at = found + 1;
            }
            assert_eq!(got, expected, "find_from {needle}");

            // And backwards the way Find Previous does.
            let mut got = Vec::new();
            let mut before = len;
            while let Some(found) = rope.rfind_before(needle, before) {
                got.push(found);
                before = found;
            }
            got.reverse();
            assert_eq!(got, expected, "rfind_before {needle}");
        }

        // Every possible start and end, boundary or not.
        for start in 0..=len {
            for end in start..=len {
                let got = rope.find_in("\u{e9}", start..end);
                let expected: Vec<usize> = text
                    .match_indices('\u{e9}')
                    .map(|(i, _)| i)
                    .filter(|&i| i < end && i + 2 > start)
                    .collect();
                assert_eq!(got, expected, "find_in over {start}..{end}");
            }
        }
    }

    /// An ASCII needle is not safe either: the viewport search backs up by a
    /// needle length, which lands inside whatever precedes the viewport.
    #[test]
    fn find_in_backs_up_into_a_multibyte_character() {
        let rope = Rope::from_text("\u{20ac}\nab ab");
        assert_eq!(rope.find_in("ab", 4..9), vec![4, 7]);
    }

    #[test]
    fn rfind_crosses_its_block_size() {
        let mut text = String::from("needle");
        text.push_str(&"x".repeat(200_000));
        let rope = Rope::from_text(&text);
        assert_eq!(rope.rfind_before("needle", rope.len_bytes()), Some(0));
        assert_eq!(rope.rfind_before("needle", 0), None);
    }

    #[test]
    fn find_in_reads_nothing_past_its_range() {
        // The only match is far beyond the range. A bounded search must not
        // go and find it, since that walk is what made an open find bar cost
        // a whole-file scan per frame.
        let mut text = "x".repeat(50_000);
        text.push_str("needle");
        let rope = Rope::from_text(&text);
        assert!(rope.find_in("needle", 0..100).is_empty());
        assert_eq!(rope.find_within("needle", 0, 105), None);
    }

    #[test]
    fn bytes_in_accepts_any_range() {
        let text = "a\u{e9}\u{1f600}b".repeat(700);
        let rope = Rope::from_text(&text);
        let bytes = text.as_bytes();
        let mut rng = Rng(7);
        for _ in 0..2000 {
            let start = rng.below(bytes.len() + 1);
            let end = start + rng.below(bytes.len() + 1 - start);
            let got: Vec<u8> = rope.bytes_in(start..end).flatten().copied().collect();
            assert_eq!(got, &bytes[start..end]);
        }
        // Past the end clamps instead of panicking.
        assert_eq!(
            rope.bytes_in(0..usize::MAX).map(<[u8]>::len).sum::<usize>(),
            bytes.len()
        );
    }

    #[test]
    fn slice_to_string_is_total() {
        let rope = Rope::from_text("a\u{e9}b");
        assert_eq!(rope.slice_to_string(0..4), "a\u{e9}b");
        // Cut through the two bytes of the accent, from either side.
        assert_eq!(rope.slice_to_string(0..2), "a\u{fffd}");
        assert_eq!(rope.slice_to_string(2..4), "\u{fffd}b");
        assert_eq!(rope.byte_to_line(2), 0);
    }

    #[test]
    fn empty_needle_finds_nothing() {
        let r = Rope::from_text("text");
        assert_eq!(r.find_from("", 0), None);
        assert!(r.find_in("", 0..4).is_empty());
    }

    #[test]
    fn clone_is_independent() {
        let mut a = Rope::from_text("shared prefix\n");
        let b = a.clone();
        a.insert(a.len_bytes(), "only in a");
        assert_eq!(b.to_string(), "shared prefix\n");
        assert_eq!(a.to_string(), "shared prefix\nonly in a");
    }
}
