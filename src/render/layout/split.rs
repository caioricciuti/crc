//! A layout of panes: a binary tree of splits with a pane at each leaf,
//! the geometry that goes with it, and a text form for saving it. No
//! drawing and no window here.
//!
//! An empty layout is `None` at the owner: a [`Node`] always holds at
//! least one leaf, which is why [`Node::remove`] takes the owner's slot.

use super::Viewport;

/// How deep a tree may nest. Splitting stops here and the parser refuses
/// more, so nothing recurses further than this.
pub const MAX_DEPTH: usize = 32;

/// How many panes a tree may hold.
pub const MAX_LEAVES: usize = 64;

/// The share of a pane, in from each edge, where a drop splits it.
const EDGE_BAND: f32 = 0.25;

/// Which way a split lays out its two sides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// Side by side: `a` on the left, `b` on the right.
    Horizontal,
    /// Stacked: `a` on top, `b` under it.
    Vertical,
}

impl Dir {
    fn cross(self) -> Dir {
        match self {
            Dir::Horizontal => Dir::Vertical,
            Dir::Vertical => Dir::Horizontal,
        }
    }

    /// The coordinate of a point that runs along this direction.
    fn along(self, x: f32, y: f32) -> f32 {
        match self {
            Dir::Horizontal => x,
            Dir::Vertical => y,
        }
    }

    /// Where `area` starts and how far it runs along this direction.
    fn span(self, area: Viewport) -> (f32, f32) {
        match self {
            Dir::Horizontal => (area.x, area.width),
            Dir::Vertical => (area.y, area.height),
        }
    }

    /// `area` with its span along this direction replaced.
    fn with_span(self, area: Viewport, start: f32, extent: f32) -> Viewport {
        match self {
            Dir::Horizontal => Viewport {
                x: start,
                width: extent,
                ..area
            },
            Dir::Vertical => Viewport {
                y: start,
                height: extent,
                ..area
            },
        }
    }
}

/// An edge of a pane: where a new pane goes, or which way to look for a
/// neighbour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    /// The split a pane added at this edge makes.
    fn dir(self) -> Dir {
        match self {
            Edge::Left | Edge::Right => Dir::Horizontal,
            Edge::Top | Edge::Bottom => Dir::Vertical,
        }
    }

    /// Whether a pane at this edge is the first side of its split.
    fn first(self) -> bool {
        matches!(self, Edge::Left | Edge::Top)
    }
}

/// Where in a pane something dragged over it would land.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zone {
    /// Beside the pane, splitting it at this edge.
    Edge(Edge),
    /// In the pane itself.
    Center,
}

/// A layout: one pane, or two layouts with a divider between them.
#[derive(Clone, Debug, PartialEq)]
pub enum Node<L> {
    Leaf(L),
    Split {
        dir: Dir,
        /// The share of the space, less the gap, that `a` takes.
        ratio: f32,
        a: Box<Node<L>>,
        b: Box<Node<L>>,
    },
}

/// The gap between the two sides of a split.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Divider {
    /// Which split it belongs to: its place among the splits, a parent
    /// before its sides and `a` before `b`. It holds until the tree
    /// changes shape; moving a divider does not change it.
    pub id: usize,
    /// The split's direction: a horizontal split's divider moves left and
    /// right.
    pub dir: Dir,
    /// The gap itself.
    pub rect: Viewport,
    /// Where a press grabs it: the gap and the slop on each side.
    pub hit: Viewport,
}

impl Divider {
    /// How far a point is from the middle of the gap, across it.
    fn miss(&self, x: f32, y: f32) -> f32 {
        let (start, extent) = self.dir.span(self.rect);
        (self.dir.along(x, y) - (start + extent * 0.5)).abs()
    }
}

/// A ratio that can be laid out, whatever was stored.
fn unit(ratio: f32) -> f32 {
    if ratio.is_finite() {
        ratio.clamp(0.0, 1.0)
    } else {
        0.5
    }
}

/// `area` cut in two along `dir`: the first side, the gap, the second
/// side. The first side is on a whole point and the second takes what is
/// left, so the three tile `area`. An area smaller than the gap is all
/// gap, with both sides empty.
fn cut(area: Viewport, dir: Dir, ratio: f32, gap: f32) -> (Viewport, Viewport, Viewport) {
    let (start, extent) = dir.span(area);
    let extent = extent.max(0.0);
    let gap = gap.max(0.0).min(extent);
    let free = extent - gap;
    let first = (free * unit(ratio)).floor();
    (
        dir.with_span(area, start, first),
        dir.with_span(area, start + first, gap),
        dir.with_span(area, start + first + gap, extent - first - gap),
    )
}

impl<L: Clone + PartialEq> Node<L> {
    pub fn contains(&self, leaf: &L) -> bool {
        self.depth_of(leaf).is_some()
    }

    pub fn leaf_count(&self) -> usize {
        match self {
            Node::Leaf(_) => 1,
            Node::Split { a, b, .. } => a.leaf_count() + b.leaf_count(),
        }
    }

    /// The panes in order: left before right, top before bottom.
    pub fn leaves(&self) -> Vec<&L> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect<'a>(&'a self, out: &mut Vec<&'a L>) {
        match self {
            Node::Leaf(leaf) => out.push(leaf),
            Node::Split { a, b, .. } => {
                a.collect(out);
                b.collect(out);
            }
        }
    }

    /// How many splits are above `leaf`.
    fn depth_of(&self, leaf: &L) -> Option<usize> {
        match self {
            Node::Leaf(own) => (own == leaf).then_some(0),
            Node::Split { a, b, .. } => a
                .depth_of(leaf)
                .or_else(|| b.depth_of(leaf))
                .map(|depth| depth + 1),
        }
    }

    fn find_mut(&mut self, leaf: &L) -> Option<&mut Node<L>> {
        match self {
            Node::Split { a, b, .. } => a.find_mut(leaf).or_else(|| b.find_mut(leaf)),
            Node::Leaf(own) => (own == leaf).then_some(self),
        }
    }

    /// Each pane and where it is in `area`, in the order of
    /// [`Node::leaves`], with `gap` between the two sides of every split.
    pub fn rects(&self, area: Viewport, gap: f32) -> Vec<(&L, Viewport)> {
        let mut out = Vec::new();
        self.place(area, gap, &mut out);
        out
    }

    fn place<'a>(&'a self, area: Viewport, gap: f32, out: &mut Vec<(&'a L, Viewport)>) {
        match self {
            Node::Leaf(leaf) => out.push((leaf, area)),
            Node::Split { dir, ratio, a, b } => {
                let (first, _, second) = cut(area, *dir, *ratio, gap);
                a.place(first, gap, out);
                b.place(second, gap, out);
            }
        }
    }

    pub fn rect_of(&self, area: Viewport, gap: f32, leaf: &L) -> Option<Viewport> {
        self.rects(area, gap)
            .into_iter()
            .find(|(own, _)| *own == leaf)
            .map(|(_, rect)| rect)
    }

    /// The pane under a point. A point in a gap is in none.
    pub fn leaf_at(&self, area: Viewport, gap: f32, x: f32, y: f32) -> Option<&L> {
        self.rects(area, gap)
            .into_iter()
            .find(|(_, rect)| rect.contains(x, y))
            .map(|(leaf, _)| leaf)
    }

    /// The pane the keyboard moves to from `leaf` toward an edge: the
    /// nearest one on that side that `leaf` runs alongside, and of several
    /// equally near, the topmost or leftmost.
    pub fn neighbour(&self, area: Viewport, gap: f32, leaf: &L, toward: Edge) -> Option<&L> {
        let rects = self.rects(area, gap);
        let (_, from) = *rects.iter().find(|(own, _)| *own == leaf)?;
        let dir = toward.dir();
        let (start, extent) = dir.span(from);
        let (lo, len) = dir.cross().span(from);
        let mut best: Option<(f32, f32, &L)> = None;
        for (other, rect) in rects {
            let (at, size) = dir.span(rect);
            let (side, run) = dir.cross().span(rect);
            let distance = if toward.first() {
                start - (at + size)
            } else {
                at - (start + extent)
            };
            let overlap = (lo + len).min(side + run) - lo.max(side);
            // Half a point of slack for the rounding of nested origins.
            if other == leaf || distance < -0.5 || overlap <= 0.0 {
                continue;
            }
            if best.is_none_or(|(d, s, _)| distance < d || (distance == d && side < s)) {
                best = Some((distance, side, other));
            }
        }
        best.map(|(_, _, other)| other)
    }

    /// Every divider in `area`, in the order of their ids. `slop` is how
    /// far past the gap, on each side, a press still grabs one.
    pub fn dividers(&self, area: Viewport, gap: f32, slop: f32) -> Vec<Divider> {
        let mut out = Vec::new();
        self.bars(area, gap, slop.max(0.0), &mut out);
        out
    }

    fn bars(&self, area: Viewport, gap: f32, slop: f32, out: &mut Vec<Divider>) {
        let Node::Split { dir, ratio, a, b } = self else {
            return;
        };
        let (first, bar, second) = cut(area, *dir, *ratio, gap);
        let (start, extent) = dir.span(bar);
        let (lo, len) = dir.span(area);
        let from = (start - slop).max(lo);
        let to = (start + extent + slop).min(lo + len.max(0.0));
        out.push(Divider {
            id: out.len(),
            dir: *dir,
            rect: bar,
            hit: dir.with_span(bar, from, (to - from).max(0.0)),
        });
        a.bars(first, gap, slop, out);
        b.bars(second, gap, slop, out);
    }

    /// The divider a press at a point grabs: where two are in reach, the
    /// one whose gap is nearer.
    pub fn divider_at(
        &self,
        area: Viewport,
        gap: f32,
        slop: f32,
        x: f32,
        y: f32,
    ) -> Option<Divider> {
        self.dividers(area, gap, slop)
            .into_iter()
            .filter(|divider| divider.hit.contains(x, y))
            .min_by(|p, q| p.miss(x, y).total_cmp(&q.miss(x, y)))
    }

    /// The split with divider `id`, counting down in `left`, and its area.
    fn nth_split(
        &mut self,
        area: Viewport,
        gap: f32,
        left: &mut usize,
    ) -> Option<(&mut Node<L>, Viewport)> {
        let (dir, ratio) = match self {
            Node::Split { dir, ratio, .. } => (*dir, *ratio),
            Node::Leaf(_) => return None,
        };
        if *left == 0 {
            return Some((self, area));
        }
        *left -= 1;
        let (first, _, second) = cut(area, dir, ratio, gap);
        let Node::Split { a, b, .. } = self else {
            return None;
        };
        if let Some(found) = a.nth_split(first, gap, left) {
            return Some(found);
        }
        b.nth_split(second, gap, left)
    }

    /// The least room the panes under this node need along `dir` for
    /// each to have `min`.
    fn need(&self, dir: Dir, min: f32, gap: f32) -> f32 {
        match self {
            Node::Leaf(_) => min,
            Node::Split { dir: own, a, b, .. } => {
                let (a, b) = (a.need(dir, min, gap), b.need(dir, min, gap));
                if *own == dir { a + gap + b } else { a.max(b) }
            }
        }
    }

    /// Moves divider `id` so the middle of its gap is at the point, as far
    /// as leaving every pane on both sides `min` allows. Returns whether
    /// the split changed: it does not when the area has no room for `min`
    /// on both sides, or `id` is not a divider.
    ///
    /// `area` and `gap` are the ones the tree is laid out with. A drag
    /// that starts off the middle of the gap passes the pointer less that
    /// offset.
    pub fn drag_divider(
        &mut self,
        id: usize,
        area: Viewport,
        gap: f32,
        x: f32,
        y: f32,
        min: f32,
    ) -> bool {
        self.place_divider(id, area, gap, min, false, |dir, start, gap, _| {
            (dir.along(x, y) - start - gap * 0.5).round()
        })
    }

    /// Puts divider `id` back in the middle of its split, as far as
    /// leaving every pane on both sides `min` allows: an even split where
    /// there is room for one. Returns whether the split changed, as
    /// [`Node::drag_divider`] does.
    pub fn center_divider(&mut self, id: usize, area: Viewport, gap: f32, min: f32) -> bool {
        self.place_divider(id, area, gap, min, true, |_, _, _, free| {
            (free * 0.5).floor()
        })
    }

    /// Gives the first side of the split with divider `id` the size
    /// `wanted` works out from the split's direction, where its area
    /// starts, its gap and the room its two sides share, held to what
    /// `min` allows. With `even`, a size that is half the room is kept as
    /// the ratio a new split has.
    fn place_divider(
        &mut self,
        id: usize,
        area: Viewport,
        gap: f32,
        min: f32,
        even: bool,
        wanted: impl Fn(Dir, f32, f32, f32) -> f32,
    ) -> bool {
        let mut left = id;
        let Some((Node::Split { dir, ratio, a, b }, area)) = self.nth_split(area, gap, &mut left)
        else {
            return false;
        };
        let (start, extent) = dir.span(area);
        let extent = extent.max(0.0);
        let gap = gap.max(0.0).min(extent);
        let free = extent - gap;
        let min = min.max(0.0);
        let lo = a.need(*dir, min, gap).ceil();
        let hi = (free - b.need(*dir, min, gap)).floor();
        let first = wanted(*dir, start, gap, free);
        if !first.is_finite() || free <= 0.0 || lo > hi {
            return false;
        }
        let first = first.clamp(lo, hi);
        // Half a point past the whole size, so that flooring the product
        // in the layout gives that size back.
        let next = if even && first == (free * 0.5).floor() {
            0.5
        } else {
            ((first + 0.5) / free).min(1.0)
        };
        let changed = next != *ratio;
        *ratio = next;
        changed
    }

    /// Cuts the pane `target` in two, evenly, with `new` at its `edge`.
    /// Returns false, leaving the tree alone, when `target` is not here,
    /// `new` already is, or the tree is as deep or as full as it may get.
    pub fn split(&mut self, target: &L, edge: Edge, new: L) -> bool {
        if self.contains(&new)
            || self.leaf_count() >= MAX_LEAVES
            || self.depth_of(target).is_none_or(|d| d >= MAX_DEPTH)
        {
            return false;
        }
        let Some(node) = self.find_mut(target) else {
            return false;
        };
        let old = Box::new(Node::Leaf(target.clone()));
        let new = Box::new(Node::Leaf(new));
        let (a, b) = if edge.first() { (new, old) } else { (old, new) };
        *node = Node::Split {
            dir: edge.dir(),
            ratio: 0.5,
            a,
            b,
        };
        true
    }

    /// Shows `new` where the pane `leaf` is, the tree otherwise as it was.
    /// Returns false, leaving the tree alone, when `leaf` is not here or
    /// `new` already is.
    pub fn replace(&mut self, leaf: &L, new: L) -> bool {
        if self.contains(&new) {
            return false;
        }
        match self.find_mut(leaf) {
            Some(node) => {
                *node = Node::Leaf(new);
                true
            }
            None => false,
        }
    }

    /// The pane that gets the room of the pane `leaf` when it goes: the
    /// nearest one of what is beside it, the first of a side after it or
    /// the last of a side before it. None for the only pane, or one not
    /// here.
    pub fn heir(&self, leaf: &L) -> Option<&L> {
        let Node::Split { a, b, .. } = self else {
            return None;
        };
        if matches!(&**a, Node::Leaf(own) if own == leaf) {
            b.leaves().first().copied()
        } else if matches!(&**b, Node::Leaf(own) if own == leaf) {
            a.leaves().last().copied()
        } else {
            a.heir(leaf).or_else(|| b.heir(leaf))
        }
    }

    /// Takes `leaf` out of the layout in `tree`; what was beside it gets
    /// the room the two had. Taking the last pane leaves `None`. Returns
    /// whether `leaf` was there.
    pub fn remove(tree: &mut Option<Node<L>>, leaf: &L) -> bool {
        if !tree.as_ref().is_some_and(|node| node.contains(leaf)) {
            return false;
        }
        *tree = tree.take().and_then(|node| node.without(leaf));
        true
    }

    fn without(self, leaf: &L) -> Option<Node<L>> {
        match self {
            Node::Leaf(own) => (own != *leaf).then_some(Node::Leaf(own)),
            Node::Split { dir, ratio, a, b } => match (a.without(leaf), b.without(leaf)) {
                (Some(a), Some(b)) => Some(Node::Split {
                    dir,
                    ratio,
                    a: Box::new(a),
                    b: Box::new(b),
                }),
                (a, b) => a.or(b),
            },
        }
    }

    /// Moves the pane `leaf` to an edge of the pane `target`: taken out,
    /// then put back as a split of `target`. Returns false, leaving the
    /// tree alone, when the two are the same pane, either is not here, or
    /// the split is refused.
    pub fn move_leaf(&mut self, leaf: &L, target: &L, edge: Edge) -> bool {
        if leaf == target || !self.contains(leaf) || !self.contains(target) {
            return false;
        }
        let Some(mut next) = self.clone().without(leaf) else {
            return false;
        };
        if !next.split(target, edge, leaf.clone()) {
            return false;
        }
        *self = next;
        true
    }

    /// Whether `other` is the same panes cut the same ways, wherever its
    /// dividers are: a move that ends in the shape it started from has
    /// moved nothing.
    pub fn alike(&self, other: &Node<L>) -> bool {
        match (self, other) {
            (Node::Leaf(own), Node::Leaf(their)) => own == their,
            (
                Node::Split { dir, a, b, .. },
                Node::Split {
                    dir: their,
                    a: p,
                    b: q,
                    ..
                },
            ) => dir == their && a.alike(p) && b.alike(q),
            _ => false,
        }
    }

    /// The tree as text: a pane is what `leaf` gives for it, a split is
    /// `h(ratio,a,b)` or `v(ratio,a,b)`. In a pane's text the characters
    /// the form uses, spaces and control characters are written `%XX`.
    pub fn encode(&self, leaf: impl Fn(&L) -> String) -> String {
        let mut out = String::new();
        self.write(&leaf, &mut out);
        out
    }

    fn write(&self, leaf: &impl Fn(&L) -> String, out: &mut String) {
        match self {
            Node::Leaf(own) => escape(&leaf(own), out),
            Node::Split { dir, ratio, a, b } => {
                out.push(match dir {
                    Dir::Horizontal => 'h',
                    Dir::Vertical => 'v',
                });
                out.push('(');
                out.push_str(&unit(*ratio).to_string());
                out.push(',');
                a.write(leaf, out);
                out.push(',');
                b.write(leaf, out);
                out.push(')');
            }
        }
    }

    /// Reads what [`Node::encode`] wrote, `leaf` turning a pane's text
    /// back into the pane. `None` for anything else: text that is not a
    /// tree, a pane `leaf` refuses, the same pane twice, or a tree past
    /// [`MAX_DEPTH`] or [`MAX_LEAVES`].
    pub fn decode(text: &str, leaf: impl Fn(&str) -> Option<L>) -> Option<Node<L>> {
        let mut reader = Reader {
            rest: text,
            leaves: 0,
        };
        let tree = reader.node(0, &leaf)?;
        if !reader.rest.is_empty() {
            return None;
        }
        let leaves = tree.leaves();
        let twice = leaves
            .iter()
            .enumerate()
            .any(|(index, own)| leaves[..index].contains(own));
        (!twice).then_some(tree)
    }
}

/// What is left of a text being read as a tree.
struct Reader<'a> {
    rest: &'a str,
    leaves: usize,
}

impl Reader<'_> {
    fn node<L>(&mut self, depth: usize, leaf: &impl Fn(&str) -> Option<L>) -> Option<Node<L>> {
        let end = self.rest.find(['(', ')', ',']).unwrap_or(self.rest.len());
        let (word, rest) = self.rest.split_at(end);
        let Some(inside) = rest.strip_prefix('(') else {
            self.rest = rest;
            self.leaves += 1;
            if self.leaves > MAX_LEAVES {
                return None;
            }
            return leaf(&unescape(word)?).map(Node::Leaf);
        };
        let dir = match word {
            "h" => Dir::Horizontal,
            "v" => Dir::Vertical,
            _ => return None,
        };
        if depth >= MAX_DEPTH {
            return None;
        }
        let (ratio, rest) = inside.split_once(',')?;
        let ratio = ratio.parse::<f32>().ok()?;
        if !(0.0..=1.0).contains(&ratio) {
            return None;
        }
        self.rest = rest;
        let a = self.node(depth + 1, leaf)?;
        self.rest = self.rest.strip_prefix(',')?;
        let b = self.node(depth + 1, leaf)?;
        self.rest = self.rest.strip_prefix(')')?;
        Some(Node::Split {
            dir,
            ratio,
            a: Box::new(a),
            b: Box::new(b),
        })
    }
}

fn escape(text: &str, out: &mut String) {
    for c in text.chars() {
        if matches!(c, '%' | '(' | ')' | ',' | ' ') || c.is_ascii_control() {
            out.push_str(&format!("%{:02X}", c as u32));
        } else {
            out.push(c);
        }
    }
}

fn unescape(text: &str) -> Option<String> {
    let mut out = Vec::with_capacity(text.len());
    let mut bytes = text.bytes();
    while let Some(byte) = bytes.next() {
        if byte != b'%' {
            out.push(byte);
            continue;
        }
        let high = (bytes.next()? as char).to_digit(16)?;
        let low = (bytes.next()? as char).to_digit(16)?;
        out.push((high * 16 + low) as u8);
    }
    String::from_utf8(out).ok()
}

/// The zone of `pane` a point is in, `None` outside it. The outer quarter
/// on each side is that edge's; in a corner the edge the point is nearer
/// to has it, as a share of the pane, and left or right on the diagonal.
pub fn zone_at(pane: Viewport, x: f32, y: f32) -> Option<Zone> {
    if !pane.contains(x, y) {
        return None;
    }
    let fx = (x - pane.x) / pane.width;
    let fy = (y - pane.y) / pane.height;
    let (dx, sideways) = if fx < 0.5 {
        (fx, Edge::Left)
    } else {
        (1.0 - fx, Edge::Right)
    };
    let (dy, upright) = if fy < 0.5 {
        (fy, Edge::Top)
    } else {
        (1.0 - fy, Edge::Bottom)
    };
    Some(if dx.min(dy) >= EDGE_BAND {
        Zone::Center
    } else if dx <= dy {
        Zone::Edge(sideways)
    } else {
        Zone::Edge(upright)
    })
}

/// What to light up for a zone: the whole pane for its center, and for an
/// edge the half a pane split in there would get.
pub fn zone_rect(pane: Viewport, gap: f32, zone: Zone) -> Viewport {
    let Zone::Edge(edge) = zone else {
        return pane;
    };
    let (first, _, second) = cut(pane, edge.dir(), 0.5, gap);
    if edge.first() { first } else { second }
}

/// The zone of `pane` under a point and what to light up for it.
pub fn drop_zone(pane: Viewport, gap: f32, x: f32, y: f32) -> Option<(Zone, Viewport)> {
    zone_at(pane, x, y).map(|zone| (zone, zone_rect(pane, gap, zone)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the terminal leaves between two panes.
    const GAP: f32 = 17.0;

    fn leaf(id: u32) -> Node<u32> {
        Node::Leaf(id)
    }

    fn split(dir: Dir, ratio: f32, a: Node<u32>, b: Node<u32>) -> Node<u32> {
        Node::Split {
            dir,
            ratio,
            a: Box::new(a),
            b: Box::new(b),
        }
    }

    fn h(ratio: f32, a: Node<u32>, b: Node<u32>) -> Node<u32> {
        split(Dir::Horizontal, ratio, a, b)
    }

    fn v(ratio: f32, a: Node<u32>, b: Node<u32>) -> Node<u32> {
        split(Dir::Vertical, ratio, a, b)
    }

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Viewport {
        Viewport {
            x,
            y,
            width,
            height,
        }
    }

    /// 1 and 2 over 3 and 4.
    fn grid() -> Node<u32> {
        h(0.5, v(0.5, leaf(1), leaf(3)), v(0.5, leaf(2), leaf(4)))
    }

    fn text(tree: &Node<u32>) -> String {
        tree.encode(|id| id.to_string())
    }

    fn read(text: &str) -> Option<Node<u32>> {
        Node::decode(text, |id| id.parse().ok())
    }

    fn overlap(p: Viewport, q: Viewport) -> bool {
        p.x < q.x + q.width && q.x < p.x + p.width && p.y < q.y + q.height && q.y < p.y + p.height
    }

    #[test]
    fn a_single_pane_fills_the_area() {
        let area = rect(12.0, 40.0, 801.0, 433.0);
        let tree = leaf(7);
        assert_eq!(tree.rects(area, GAP), vec![(&7, area)]);
        assert!(tree.dividers(area, GAP, 3.0).is_empty());
        assert_eq!(tree.rect_of(area, GAP, &7), Some(area));
        assert_eq!(tree.rect_of(area, GAP, &8), None);
    }

    #[test]
    fn an_even_split_gives_each_side_half_on_whole_points() {
        let tree = h(0.5, leaf(1), leaf(2));
        // The first side is half of what the gap leaves, rounded down; the
        // second starts after the gap and takes the rest.
        for (screen, left, right) in [
            (
                rect(8.0, 30.0, 1000.0, 400.0),
                rect(8.0, 30.0, 491.0, 400.0),
                rect(516.0, 30.0, 492.0, 400.0),
            ),
            (
                rect(0.0, 0.0, 1183.0, 700.0),
                rect(0.0, 0.0, 583.0, 700.0),
                rect(600.0, 0.0, 583.0, 700.0),
            ),
            (
                rect(248.0, 52.0, 1183.5, 611.0),
                rect(248.0, 52.0, 583.0, 611.0),
                rect(848.0, 52.0, 583.5, 611.0),
            ),
            (
                rect(3.0, 4.0, 18.0, 9.0),
                rect(3.0, 4.0, 0.0, 9.0),
                rect(20.0, 4.0, 1.0, 9.0),
            ),
            (
                rect(3.0, 4.0, GAP, 9.0),
                rect(3.0, 4.0, 0.0, 9.0),
                rect(20.0, 4.0, 0.0, 9.0),
            ),
        ] {
            assert_eq!(
                tree.rects(screen, GAP),
                vec![(&1, left), (&2, right)],
                "{screen:?}"
            );
        }
        // Stacked, the same along the other direction.
        assert_eq!(
            v(0.5, leaf(1), leaf(2)).rects(rect(8.0, 30.0, 1000.0, 400.0), GAP),
            vec![
                (&1, rect(8.0, 30.0, 1000.0, 191.0)),
                (&2, rect(8.0, 238.0, 1000.0, 192.0)),
            ]
        );
    }

    #[test]
    fn a_divider_goes_back_to_the_middle_as_far_as_the_minimum_allows() {
        let area = rect(100.0, 0.0, 317.0, 200.0);
        let mut tree = h(0.5, leaf(1), leaf(2));
        assert!(!tree.center_divider(0, area, GAP, 80.0), "already even");
        assert!(tree.drag_divider(0, area, GAP, 5000.0, 0.0, 80.0));
        assert!(tree.center_divider(0, area, GAP, 80.0));
        assert_eq!(tree, h(0.5, leaf(1), leaf(2)), "the ratio a new split has");
        // From the other side too, and it lays out as a new split does.
        assert!(tree.drag_divider(0, area, GAP, -5000.0, 0.0, 80.0));
        assert!(tree.center_divider(0, area, GAP, 80.0));
        assert_eq!(tree, h(0.5, leaf(1), leaf(2)));
        assert_eq!(
            tree.rects(area, GAP),
            vec![
                (&1, rect(100.0, 0.0, 150.0, 200.0)),
                (&2, rect(267.0, 0.0, 150.0, 200.0)),
            ]
        );

        // Two panes on the right need more than half: the divider stops
        // where each of them still has the minimum.
        let mut tree = h(0.2, leaf(1), h(0.5, leaf(2), leaf(3)));
        let area = rect(0.0, 0.0, 317.0, 200.0);
        assert!(tree.center_divider(0, area, GAP, 80.0));
        let panes = tree.rects(area, GAP);
        assert_eq!(panes[0].1.width, 300.0 - (80.0 + GAP + 80.0));
        assert!(panes.iter().all(|(_, pane)| pane.width >= 80.0));
        // The nested one, in its own area.
        assert!(tree.drag_divider(1, area, GAP, 5000.0, 0.0, 0.0));
        assert!(tree.center_divider(1, area, GAP, 0.0));
        let panes = tree.rects(area, GAP);
        assert_eq!(panes[1].1.width, 80.0);
        assert_eq!(panes[2].1.width, 80.0);

        let before = tree.clone();
        assert!(!tree.center_divider(2, area, GAP, 80.0), "no such divider");
        assert!(!tree.center_divider(0, rect(0.0, 0.0, 100.0, 100.0), GAP, 80.0));
        assert!(!leaf(1).center_divider(0, area, GAP, 80.0));
        assert_eq!(tree, before);
    }

    #[test]
    fn a_pane_is_replaced_where_it_is_and_its_room_goes_to_the_nearest() {
        let mut tree = h(0.3, leaf(1), v(0.6, leaf(2), h(0.7, leaf(3), leaf(4))));
        assert!(tree.replace(&3, 9));
        assert_eq!(
            tree,
            h(0.3, leaf(1), v(0.6, leaf(2), h(0.7, leaf(9), leaf(4))))
        );
        let before = tree.clone();
        assert!(!tree.replace(&3, 5), "no such pane");
        assert!(!tree.replace(&1, 4), "4 is already here");
        assert!(!tree.replace(&1, 1), "onto itself");
        assert_eq!(tree, before);
        let mut one = leaf(1);
        assert!(one.replace(&1, 2));
        assert_eq!(one, leaf(2));

        // Whoever is beside a pane gets its room: the first pane of a side
        // after it, the last of a side before it.
        let heir = |from: u32| tree.heir(&from).copied();
        assert_eq!(heir(1), Some(2));
        assert_eq!(heir(2), Some(9));
        assert_eq!(heir(9), Some(4));
        assert_eq!(heir(4), Some(9));
        assert_eq!(heir(7), None, "no such pane");
        assert_eq!(leaf(1).heir(&1), None, "the only pane");
        let wide = h(0.5, v(0.5, leaf(1), leaf(2)), leaf(3));
        assert_eq!(wide.heir(&3), Some(&2), "the last of the side before");
        // And it is the pane that has the room once the other is gone.
        for id in [1, 2, 9, 4] {
            let next = heir(id).unwrap();
            let area = rect(0.0, 0.0, 817.0, 617.0);
            let was = tree.rect_of(area, GAP, &id).unwrap();
            let mut slot = Some(tree.clone());
            assert!(Node::remove(&mut slot, &id));
            let grown = slot.unwrap().rect_of(area, GAP, &next).unwrap();
            assert!(
                grown.width * grown.height > was.width * was.height || grown.contains(was.x, was.y),
                "{id} to {next}: {grown:?}"
            );
        }
    }

    #[test]
    fn nested_splits_tile_the_area() {
        let tree = h(
            0.3,
            leaf(1),
            v(0.6, leaf(2), h(0.5, leaf(3), v(0.25, leaf(4), leaf(5)))),
        );
        let area = rect(10.0, 20.0, 1001.0, 707.0);
        let panes = tree.rects(area, GAP);
        assert_eq!(
            panes.iter().map(|(id, _)| **id).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5]
        );
        let mut all: Vec<Viewport> = panes.iter().map(|(_, rect)| *rect).collect();
        all.extend(tree.dividers(area, GAP, 0.0).iter().map(|d| d.rect));
        assert_eq!(all.len(), 9, "five panes and four dividers");
        for (index, piece) in all.iter().enumerate() {
            for value in [piece.x, piece.y, piece.width, piece.height] {
                assert_eq!(value, value.floor(), "{piece:?} is on whole points");
            }
            assert!(piece.width > 0.0 && piece.height > 0.0, "{piece:?}");
            assert!(
                piece.x >= area.x
                    && piece.y >= area.y
                    && piece.x + piece.width <= area.x + area.width
                    && piece.y + piece.height <= area.y + area.height,
                "{piece:?} is inside the area"
            );
            for other in &all[..index] {
                assert!(!overlap(*piece, *other), "{piece:?} over {other:?}");
            }
        }
        // Nothing overlaps and nothing is outside, so covering the area
        // means adding up to it.
        let covered: f32 = all.iter().map(|piece| piece.width * piece.height).sum();
        assert_eq!(covered, area.width * area.height);
    }

    #[test]
    fn an_area_too_small_to_split_gives_empty_panes_not_bad_ones() {
        let tree = h(0.5, leaf(1), v(f32::NAN, leaf(2), leaf(3)));
        for area in [
            rect(5.0, 5.0, 0.0, 0.0),
            rect(5.0, 5.0, 9.0, 3.0),
            rect(5.0, 5.0, -40.0, 2.0),
            rect(5.0, 5.0, 18.0, 18.0),
        ] {
            for (id, pane) in tree.rects(area, GAP) {
                for value in [pane.x, pane.y, pane.width, pane.height] {
                    assert!(value.is_finite(), "pane {id} in {area:?}: {pane:?}");
                }
                assert!(pane.width >= 0.0 && pane.height >= 0.0, "{pane:?}");
                assert!(
                    pane.x >= area.x && pane.x + pane.width <= area.x + area.width.max(0.0),
                    "pane {id} in {area:?}: {pane:?}"
                );
            }
        }
    }

    #[test]
    fn dividers_count_parents_first_and_are_grabbed_past_the_gap() {
        let tree = h(0.5, v(0.5, leaf(1), leaf(2)), v(0.25, leaf(3), leaf(4)));
        let area = rect(0.0, 0.0, 817.0, 417.0);
        let bars = tree.dividers(area, GAP, 4.0);
        assert_eq!(bars.iter().map(|d| d.id).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(
            bars.iter().map(|d| d.dir).collect::<Vec<_>>(),
            [Dir::Horizontal, Dir::Vertical, Dir::Vertical]
        );
        assert_eq!(bars[0].rect, rect(400.0, 0.0, GAP, 417.0));
        assert_eq!(bars[0].hit, rect(396.0, 0.0, GAP + 8.0, 417.0));
        assert_eq!(bars[1].rect, rect(0.0, 200.0, 400.0, GAP));
        assert_eq!(bars[1].hit, rect(0.0, 196.0, 400.0, GAP + 8.0));
        assert_eq!(bars[2].rect, rect(417.0, 100.0, 400.0, GAP));

        let at = |x, y| tree.divider_at(area, GAP, 4.0, x, y).map(|d| d.id);
        assert_eq!(at(408.0, 50.0), Some(0), "in the gap");
        assert_eq!(at(397.0, 50.0), Some(0), "in the slop, over pane 1");
        assert_eq!(at(395.0, 50.0), None, "past the slop");
        assert_eq!(at(100.0, 197.0), Some(1));
        assert_eq!(at(600.0, 120.0), Some(2));
        assert_eq!(at(600.0, 300.0), None, "in pane 4");
        // Where the slop of the first reaches the end of the third, the
        // nearer gap has it.
        assert_eq!(at(418.0, 108.0), Some(2));
        assert_eq!(at(418.0, 50.0), Some(0));
        // The slop stops at the edge of the area the split has.
        let tight = h(0.0, leaf(1), leaf(2)).dividers(area, GAP, 4.0);
        assert_eq!(tight[0].hit, rect(0.0, 0.0, GAP + 4.0, 417.0));
    }

    #[test]
    fn dragging_a_divider_follows_the_pointer_to_the_minimum() {
        let area = rect(100.0, 0.0, 317.0, 200.0);
        let mut tree = h(0.5, leaf(1), leaf(2));
        let widths = |tree: &Node<u32>| -> Vec<f32> {
            tree.rects(area, GAP).iter().map(|(_, r)| r.width).collect()
        };

        // The middle of the gap goes under the pointer.
        assert!(tree.drag_divider(0, area, GAP, 208.5, 9.0, 80.0));
        assert_eq!(widths(&tree), [100.0, 200.0]);
        assert_eq!(tree.dividers(area, GAP, 0.0)[0].rect.x, 200.0);
        assert!(
            !tree.drag_divider(0, area, GAP, 208.5, 150.0, 80.0),
            "already there"
        );

        assert!(tree.drag_divider(0, area, GAP, -5000.0, 0.0, 80.0));
        assert_eq!(widths(&tree), [80.0, 220.0], "the left pane keeps 80");
        assert!(tree.drag_divider(0, area, GAP, 5000.0, 0.0, 80.0));
        assert_eq!(widths(&tree), [220.0, 80.0], "the right pane keeps 80");

        // Every whole size comes back out of the layout as itself.
        for width in 0..=300 {
            let x = area.x + width as f32 + GAP * 0.5;
            tree.drag_divider(0, area, GAP, x, 0.0, 0.0);
            assert_eq!(widths(&tree), [width as f32, (300 - width) as f32]);
        }

        // A stacked split reads the other coordinate.
        let mut stacked = v(0.5, leaf(1), leaf(2));
        assert!(stacked.drag_divider(0, area, GAP, 0.0, 5000.0, 60.0));
        assert_eq!(
            stacked.rects(area, GAP)[1].1,
            rect(100.0, 140.0, 317.0, 60.0)
        );
    }

    #[test]
    fn dragging_in_an_area_too_small_for_the_minimum_changes_nothing() {
        let mut tree = h(0.5, leaf(1), leaf(2));
        let before = tree.clone();
        for area in [
            rect(0.0, 0.0, 176.0, 100.0),
            rect(0.0, 0.0, 10.0, 100.0),
            rect(0.0, 0.0, 0.0, 0.0),
            rect(0.0, 0.0, -30.0, 100.0),
        ] {
            for x in [-900.0, 0.0, 44.0, 900.0] {
                assert!(!tree.drag_divider(0, area, GAP, x, 0.0, 80.0), "{area:?}");
            }
        }
        // One point more and there is exactly one place for it.
        let area = rect(0.0, 0.0, 177.0, 100.0);
        tree.drag_divider(0, area, GAP, 900.0, 0.0, 80.0);
        assert_eq!(tree.rects(area, GAP)[0].1.width, 80.0);
        assert_eq!(tree.rects(area, GAP)[1].1.width, 80.0);

        let mut tree = before.clone();
        let area = rect(0.0, 0.0, 400.0, 100.0);
        assert!(!tree.drag_divider(0, area, GAP, f32::NAN, 0.0, 80.0));
        assert!(!tree.drag_divider(0, area, GAP, f32::INFINITY, 0.0, 80.0));
        assert!(
            !tree.drag_divider(1, area, GAP, 50.0, 0.0, 80.0),
            "no such divider"
        );
        assert!(!leaf(1).drag_divider(0, area, GAP, 50.0, 0.0, 80.0));
        assert_eq!(tree, before);
        // No minimum and no room at all: still a ratio that lays out.
        assert!(tree.drag_divider(0, area, GAP, 9000.0, 0.0, f32::NAN));
        assert_eq!(tree.rects(area, GAP)[0].1.width, 383.0);
        assert_eq!(tree.rects(area, GAP)[1].1.width, 0.0);
    }

    #[test]
    fn dragging_leaves_room_for_the_panes_nested_on_a_side() {
        // 1 beside 2 and 3: the right side holds two panes and a gap.
        let mut tree = h(0.5, leaf(1), h(0.5, leaf(2), v(0.5, leaf(3), leaf(4))));
        let area = rect(0.0, 0.0, 617.0, 300.0);
        assert!(tree.drag_divider(0, area, GAP, 5000.0, 0.0, 80.0));
        let panes = tree.rects(area, GAP);
        assert_eq!(panes[0].1.width, 600.0 - (80.0 + GAP + 80.0));
        assert!(panes.iter().all(|(_, pane)| pane.width >= 80.0));
        // The second divider is the nested one, in the right side's area.
        assert!(tree.drag_divider(1, area, GAP, 0.0, 0.0, 80.0));
        let panes = tree.rects(area, GAP);
        assert_eq!(panes[1].1.width, 80.0);
        assert_eq!(panes[1].1.x, panes[0].1.width + GAP);
    }

    #[test]
    fn splitting_puts_the_new_pane_at_the_edge() {
        for (edge, expected) in [
            (Edge::Left, h(0.5, leaf(2), leaf(1))),
            (Edge::Right, h(0.5, leaf(1), leaf(2))),
            (Edge::Top, v(0.5, leaf(2), leaf(1))),
            (Edge::Bottom, v(0.5, leaf(1), leaf(2))),
        ] {
            let mut tree = leaf(1);
            assert!(tree.split(&1, edge, 2), "{edge:?}");
            assert_eq!(tree, expected, "{edge:?}");
        }

        // Along the split it is already in, the pane is cut again and the
        // other side keeps what it had.
        let mut tree = h(0.5, leaf(1), leaf(2));
        assert!(tree.split(&2, Edge::Right, 3));
        assert_eq!(tree, h(0.5, leaf(1), h(0.5, leaf(2), leaf(3))));
        assert!(tree.split(&1, Edge::Bottom, 4));
        assert_eq!(
            tree,
            h(0.5, v(0.5, leaf(1), leaf(4)), h(0.5, leaf(2), leaf(3)))
        );
        let panes = tree.rects(rect(0.0, 0.0, 900.0, 500.0), GAP);
        assert!(
            panes
                .iter()
                .all(|(_, pane)| pane.width > 0.0 && pane.height > 0.0)
        );

        let before = tree.clone();
        assert!(!tree.split(&9, Edge::Left, 5), "no such pane");
        assert!(!tree.split(&1, Edge::Left, 3), "3 is already here");
        assert!(!tree.split(&1, Edge::Left, 1), "onto itself");
        assert_eq!(tree, before);
    }

    #[test]
    fn splitting_stops_at_the_bounds() {
        let mut tree = leaf(0);
        for id in 1..=MAX_DEPTH as u32 {
            assert!(tree.split(&(id - 1), Edge::Right, id), "{id}");
        }
        let deepest = MAX_DEPTH as u32;
        assert!(
            !tree.split(&deepest, Edge::Right, 999),
            "as deep as it goes"
        );
        assert_eq!(tree.leaf_count(), MAX_DEPTH + 1);
        assert_eq!(read(&text(&tree)), Some(tree.clone()), "and it still reads");

        // Wide instead of deep, up to the most panes there may be.
        let mut tree = leaf(0);
        let mut next = 1;
        while tree.leaf_count() < MAX_LEAVES {
            for id in tree.leaves().into_iter().copied().collect::<Vec<_>>() {
                if tree.leaf_count() < MAX_LEAVES {
                    assert!(tree.split(&id, Edge::Bottom, next));
                    next += 1;
                }
            }
        }
        assert!(!tree.split(&0, Edge::Bottom, next), "full");
        assert_eq!(read(&text(&tree)), Some(tree));
    }

    #[test]
    fn removing_a_pane_gives_its_room_to_what_was_beside_it() {
        // At the root.
        let mut tree = Some(h(0.3, leaf(1), leaf(2)));
        assert!(Node::remove(&mut tree, &1));
        assert_eq!(tree, Some(leaf(2)));

        // Nested: the split above 2 goes, the one above that stays.
        let mut tree = Some(h(0.3, leaf(1), v(0.6, leaf(2), h(0.7, leaf(3), leaf(4)))));
        assert!(Node::remove(&mut tree, &2));
        assert_eq!(tree, Some(h(0.3, leaf(1), h(0.7, leaf(3), leaf(4)))));
        assert!(Node::remove(&mut tree, &4));
        assert_eq!(tree, Some(h(0.3, leaf(1), leaf(3))));
        let area = rect(0.0, 0.0, 517.0, 300.0);
        assert_eq!(
            tree.as_ref().unwrap().rect_of(area, GAP, &3),
            Some(rect(167.0, 0.0, 350.0, 300.0)),
            "3 has all of the right side"
        );

        assert!(!Node::remove(&mut tree, &9), "no such pane");
        assert_eq!(tree, Some(h(0.3, leaf(1), leaf(3))));

        // The last pane leaves no layout.
        assert!(Node::remove(&mut tree, &1));
        assert!(Node::remove(&mut tree, &3));
        assert_eq!(tree, None);
        assert!(!Node::remove(&mut tree, &3));
    }

    #[test]
    fn moving_a_pane_takes_it_out_and_splits_the_target() {
        let mut tree = h(0.3, leaf(1), v(0.6, leaf(2), leaf(3)));
        assert!(tree.move_leaf(&3, &1, Edge::Top));
        assert_eq!(tree, h(0.3, v(0.5, leaf(3), leaf(1)), leaf(2)));

        // Beside the pane it was already beside: the two trade places.
        let mut tree = h(0.3, leaf(1), leaf(2));
        assert!(tree.move_leaf(&1, &2, Edge::Right));
        assert_eq!(tree, h(0.5, leaf(2), leaf(1)));

        let before = tree.clone();
        assert!(!tree.move_leaf(&1, &1, Edge::Left), "onto itself");
        assert!(!tree.move_leaf(&9, &1, Edge::Left), "no such pane");
        assert!(!tree.move_leaf(&1, &9, Edge::Left), "no such target");
        assert!(!leaf(1).move_leaf(&1, &1, Edge::Left));
        assert_eq!(tree, before);

        // A move the depth bound refuses leaves the tree as it was.
        let mut tree = leaf(0);
        for id in 1..=MAX_DEPTH as u32 {
            assert!(tree.split(&(id - 1), Edge::Right, id));
        }
        assert!(tree.split(&0, Edge::Top, 100));
        let before = tree.clone();
        assert!(!tree.move_leaf(&100, &(MAX_DEPTH as u32), Edge::Bottom));
        assert_eq!(tree, before);
    }

    #[test]
    fn two_trees_are_alike_whatever_their_ratios() {
        let tree = h(0.3, leaf(1), v(0.6, leaf(2), leaf(3)));
        assert!(tree.alike(&tree));
        assert!(tree.alike(&h(0.5, leaf(1), v(0.5, leaf(2), leaf(3)))));
        assert!(
            !tree.alike(&h(0.3, leaf(1), v(0.6, leaf(3), leaf(2)))),
            "traded"
        );
        assert!(
            !tree.alike(&h(0.3, leaf(1), h(0.6, leaf(2), leaf(3)))),
            "cut the other way"
        );
        assert!(!tree.alike(&h(0.3, leaf(1), leaf(2))), "a pane fewer");
        assert!(!tree.alike(&leaf(1)));
        assert!(leaf(1).alike(&leaf(1)));
        assert!(!leaf(1).alike(&leaf(2)));

        // A pane moved to where it already is: the same shape, the
        // divider in the middle again.
        let mut moved = tree.clone();
        assert!(moved.move_leaf(&2, &3, Edge::Top));
        assert_eq!(moved, h(0.3, leaf(1), v(0.5, leaf(2), leaf(3))));
        assert!(moved.alike(&tree));
        // And to the other side of it: another shape.
        assert!(moved.move_leaf(&2, &3, Edge::Bottom));
        assert!(!moved.alike(&tree));
    }

    #[test]
    fn a_tree_answers_for_its_panes() {
        let tree = h(0.5, leaf(5), v(0.5, leaf(3), leaf(8)));
        assert_eq!(tree.leaves(), [&5, &3, &8]);
        assert_eq!(tree.leaf_count(), 3);
        assert_eq!(leaf(1).leaf_count(), 1);
        assert!(tree.contains(&3) && tree.contains(&8));
        assert!(!tree.contains(&4));

        // 5 is 400 wide; 3 and 8 are 400 by 200 with the gaps between.
        let area = rect(10.0, 10.0, 817.0, 417.0);
        let at = |x, y| tree.leaf_at(area, GAP, x, y).copied();
        assert_eq!(at(10.0, 10.0), Some(5), "the top left corner is inside");
        assert_eq!(at(409.9, 426.9), Some(5));
        assert_eq!(at(410.0, 100.0), None, "in the gap");
        assert_eq!(at(427.0, 100.0), Some(3));
        assert_eq!(at(700.0, 215.0), None, "between 3 and 8");
        assert_eq!(at(700.0, 227.0), Some(8));
        assert_eq!(at(827.0, 100.0), None, "the right edge is outside");
        assert_eq!(at(0.0, 0.0), None);
    }

    #[test]
    fn the_neighbour_of_a_pane_is_the_one_beside_it() {
        let area = rect(0.0, 0.0, 817.0, 617.0);
        let tree = grid();
        let go = |from: u32, toward| tree.neighbour(area, GAP, &from, toward).copied();
        assert_eq!(go(1, Edge::Right), Some(2));
        assert_eq!(go(1, Edge::Bottom), Some(3));
        assert_eq!(go(1, Edge::Left), None);
        assert_eq!(go(1, Edge::Top), None);
        assert_eq!(go(2, Edge::Left), Some(1));
        assert_eq!(go(2, Edge::Bottom), Some(4));
        assert_eq!(go(3, Edge::Top), Some(1));
        assert_eq!(go(3, Edge::Right), Some(4));
        assert_eq!(go(4, Edge::Left), Some(3));
        assert_eq!(go(4, Edge::Top), Some(2));
        assert_eq!(go(4, Edge::Right), None);
        assert_eq!(go(4, Edge::Bottom), None);
        assert_eq!(go(9, Edge::Left), None, "no such pane");
        assert_eq!(leaf(1).neighbour(area, GAP, &1, Edge::Left), None);

        // A tall pane beside a column goes to the top of the column, a
        // pane in the column comes back to it, and the nearest wins over
        // one further on.
        let tree = h(0.3, leaf(1), h(0.5, v(0.7, leaf(2), leaf(3)), leaf(4)));
        let go = |from: u32, toward| tree.neighbour(area, GAP, &from, toward).copied();
        assert_eq!(go(1, Edge::Right), Some(2));
        assert_eq!(go(3, Edge::Left), Some(1));
        assert_eq!(go(4, Edge::Left), Some(2));
        assert_eq!(go(3, Edge::Right), Some(4));
        assert_eq!(go(2, Edge::Bottom), Some(3));
        assert_eq!(go(1, Edge::Bottom), None);
    }

    #[test]
    fn a_drop_lands_in_the_middle_or_on_the_nearest_edge() {
        let pane = rect(100.0, 50.0, 417.0, 217.0);
        let zone = |x, y| zone_at(pane, x, y);
        assert_eq!(zone(308.0, 158.0), Some(Zone::Center));
        assert_eq!(
            zone(210.0, 110.0),
            Some(Zone::Center),
            "just past a quarter"
        );
        assert_eq!(zone(120.0, 158.0), Some(Zone::Edge(Edge::Left)));
        assert_eq!(zone(500.0, 158.0), Some(Zone::Edge(Edge::Right)));
        assert_eq!(zone(308.0, 60.0), Some(Zone::Edge(Edge::Top)));
        assert_eq!(zone(308.0, 260.0), Some(Zone::Edge(Edge::Bottom)));
        assert_eq!(zone(203.0, 158.0), Some(Zone::Edge(Edge::Left)));
        assert_eq!(zone(205.0, 158.0), Some(Zone::Center));

        // A corner belongs to the edge the point is nearer to, as a share
        // of the pane, and to the side on the diagonal.
        assert_eq!(zone(100.0, 50.0), Some(Zone::Edge(Edge::Left)));
        assert_eq!(zone(104.0, 70.0), Some(Zone::Edge(Edge::Left)));
        assert_eq!(zone(160.0, 52.0), Some(Zone::Edge(Edge::Top)));
        assert_eq!(zone(516.0, 51.0), Some(Zone::Edge(Edge::Right)));
        assert_eq!(zone(470.0, 51.0), Some(Zone::Edge(Edge::Top)));
        assert_eq!(zone(101.0, 266.0), Some(Zone::Edge(Edge::Left)));
        assert_eq!(zone(150.0, 266.0), Some(Zone::Edge(Edge::Bottom)));
        assert_eq!(zone(516.0, 266.0), Some(Zone::Edge(Edge::Right)));
        assert_eq!(zone(480.0, 266.0), Some(Zone::Edge(Edge::Bottom)));

        assert_eq!(zone(99.0, 158.0), None);
        assert_eq!(zone(517.0, 158.0), None, "the right edge is outside");
        assert_eq!(zone(308.0, 267.0), None);
        assert_eq!(zone_at(rect(0.0, 0.0, 0.0, 0.0), 0.0, 0.0), None);

        // What lights up is what a split there gives the new pane.
        let lit = |zone| zone_rect(pane, GAP, zone);
        assert_eq!(lit(Zone::Center), pane);
        assert_eq!(lit(Zone::Edge(Edge::Left)), rect(100.0, 50.0, 200.0, 217.0));
        assert_eq!(
            lit(Zone::Edge(Edge::Right)),
            rect(317.0, 50.0, 200.0, 217.0)
        );
        assert_eq!(lit(Zone::Edge(Edge::Top)), rect(100.0, 50.0, 417.0, 100.0));
        assert_eq!(
            lit(Zone::Edge(Edge::Bottom)),
            rect(100.0, 167.0, 417.0, 100.0)
        );
        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            let mut tree = leaf(1);
            assert!(tree.split(&1, edge, 2));
            assert_eq!(
                tree.rect_of(pane, GAP, &2),
                Some(lit(Zone::Edge(edge))),
                "{edge:?}"
            );
        }
        assert_eq!(
            drop_zone(pane, GAP, 120.0, 158.0),
            Some((Zone::Edge(Edge::Left), rect(100.0, 50.0, 200.0, 217.0)))
        );
        assert_eq!(drop_zone(pane, GAP, 0.0, 0.0), None);
    }

    #[test]
    fn a_tree_reads_back_from_its_text() {
        let tree = h(0.5, leaf(1), v(0.25, leaf(2), h(0.7, leaf(3), leaf(4))));
        assert_eq!(text(&tree), "h(0.5,1,v(0.25,2,h(0.7,3,4)))");
        assert_eq!(read(&text(&tree)), Some(tree));
        assert_eq!(text(&leaf(12)), "12");
        assert_eq!(read("12"), Some(leaf(12)));

        // Ratios a drag leaves, the ends included, come back exactly.
        let area = rect(0.0, 0.0, 1017.0, 600.0);
        let mut tree = grid();
        for x in [-50.0, 123.0, 333.0, 508.5, 9000.0] {
            tree.drag_divider(0, area, GAP, x, 0.0, 0.0);
            assert_eq!(read(&text(&tree)), Some(tree.clone()), "{}", text(&tree));
        }
        // A ratio that is not a number is written as one that lays out.
        assert_eq!(text(&h(f32::NAN, leaf(1), leaf(2))), "h(0.5,1,2)");
        assert_eq!(text(&h(7.0, leaf(1), leaf(2))), "h(1,1,2)");

        // Whatever the panes are called.
        let names = [
            "plain",
            "a,b",
            "(x)",
            "50%",
            "two words",
            "h",
            "v(",
            "",
            "tab\there\n",
            "caf\u{e9} \u{1f600}",
            "%41",
        ];
        let mut tree = Node::Leaf(names[0].to_string());
        for pair in names.windows(2) {
            assert!(tree.split(&pair[0].to_string(), Edge::Bottom, pair[1].to_string()));
        }
        let written = tree.encode(|name| name.clone());
        assert!(written.is_ascii() || written.contains('\u{e9}'));
        assert!(!written.contains(' ') && !written.contains('\n'));
        assert_eq!(
            Node::decode(&written, |name| Some(name.to_string())),
            Some(tree)
        );
        let one = Node::Leaf("a,b (c)".to_string());
        assert_eq!(one.encode(|name| name.clone()), "a%2Cb%20%28c%29");
    }

    #[test]
    fn text_that_is_not_a_tree_reads_as_nothing() {
        for bad in [
            "",
            "x",
            "1,2",
            "1)",
            "(1)",
            "h",
            "h(",
            "h()",
            "h(0.5)",
            "h(0.5,1)",
            "h(0.5,1,2",
            "h(0.5,1,2))",
            "h(0.5,1,2)3",
            "h(0.5,1,2,3)",
            "h(0.5,,2)",
            "h(,1,2)",
            "h(half,1,2)",
            "h(NaN,1,2)",
            "h(inf,1,2)",
            "h(-0.1,1,2)",
            "h(1.5,1,2)",
            "h( 0.5,1,2)",
            "h(0.5, 1, 2)",
            "H(0.5,1,2)",
            "x(0.5,1,2)",
            "hv(0.5,1,2)",
            "h(0.5,1,1)",
            "h(0.5,1,v(0.5,2,1))",
            "h(0.5,1,v(0.5,2))",
            "v(0.5,h(0.5,1,2)",
            "h(0.5,%31,2",
            "\u{e9}(0.5,1,2)",
            "h(0.5,1,2)\n",
        ] {
            assert_eq!(read(bad), None, "{bad:?}");
        }
        let words = |text: &str| Node::decode(text, |name| Some(name.to_string()));
        for bad in [
            "%",
            "%4",
            "%zz",
            "a%",
            "%FF",
            "%C3",
            "h(0.5,%,b)",
            "%\u{e9}",
        ] {
            assert_eq!(words(bad), None, "{bad:?}");
        }
        assert_eq!(words("%41%e9").map(|tree| tree.leaf_count()), None);
        assert_eq!(words("%41%c3%a9"), Some(Node::Leaf("A\u{e9}".to_string())));
        assert_eq!(read("h(0.5,%31,2)"), Some(h(0.5, leaf(1), leaf(2))));

        // As deep as a tree may be reads; one more does not, and a text
        // nested far past that is refused without being followed down.
        let nest = |depth: usize| {
            let tail: String = (1..=depth).map(|id| format!(",{id})")).collect();
            format!("{}0{tail}", "h(0.5,".repeat(depth))
        };
        assert_eq!(
            read(&nest(MAX_DEPTH)).map(|tree| tree.leaf_count()),
            Some(MAX_DEPTH + 1)
        );
        assert_eq!(read(&nest(MAX_DEPTH + 1)), None);
        assert_eq!(read(&"h(0.5,".repeat(200_000)), None);
        assert_eq!(read(&"v(0.5,1,".repeat(200_000)), None);

        // More panes than a tree may hold.
        let mut tree = leaf(0);
        let mut next = 1;
        while tree.leaf_count() < MAX_LEAVES {
            for id in tree.leaves().into_iter().copied().collect::<Vec<_>>() {
                if tree.leaf_count() < MAX_LEAVES {
                    assert!(tree.split(&id, Edge::Right, next));
                    next += 1;
                }
            }
        }
        let full = text(&tree);
        assert!(read(&full).is_some());
        assert_eq!(read(&format!("h(0.5,{full},9999)")), None);
    }
}
