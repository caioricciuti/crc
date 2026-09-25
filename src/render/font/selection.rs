//! Source-ordered bounds tree: skip offscreen subtrees before reading bands.
//! Leaves cover 32 UTF-16 units so the index stays small relative to geometry.
use std::ops::Range;
const BLOCK: usize = 32;
const EMPTY: (f32, f32) = (f32::INFINITY, f32::NEG_INFINITY);

#[derive(Clone)]
pub(super) struct SelectionIndex {
    bounds: Vec<(f32, f32)>,
    leaves: usize,
}

pub(super) fn band(low: &[f32], high: &[f32], index: usize) -> Option<(f32, f32)> {
    let mut best = (0.0, 0.0, f32::INFINITY);
    for start in [low[index], high[index]] {
        for end in [low[index + 1], high[index + 1]] {
            let width = (end - start).abs();
            if width > 0.01 && width < best.2 {
                best = (start.min(end), start.max(end), width);
            }
        }
    }
    best.2.is_finite().then_some((best.0, best.1))
}

impl SelectionIndex {
    pub fn new(low: &[f32], high: &[f32]) -> Self {
        let leaves = low
            .len()
            .saturating_sub(1)
            .div_ceil(BLOCK)
            .next_power_of_two();
        let mut bounds = vec![EMPTY; leaves * 2];
        for index in 0..low.len() - 1 {
            if let Some((left, right)) = band(low, high, index) {
                let node = &mut bounds[leaves + index / BLOCK];
                node.0 = node.0.min(left);
                node.1 = node.1.max(right);
            }
        }
        for index in (1..leaves).rev() {
            bounds[index] = (
                bounds[index * 2].0.min(bounds[index * 2 + 1].0),
                bounds[index * 2].1.max(bounds[index * 2 + 1].1),
            );
        }
        Self { bounds, leaves }
    }

    pub fn intervals(
        &self,
        low: &[f32],
        high: &[f32],
        source: Range<usize>,
        viewport: Range<f32>,
        extend: f32,
    ) -> Vec<(f32, f32)> {
        struct Query<'a> {
            tree: &'a SelectionIndex,
            low: &'a [f32],
            high: &'a [f32],
            source: Range<usize>,
            viewport: Range<f32>,
            bands: Vec<(f32, f32)>,
        }
        impl Query<'_> {
            fn visit(&mut self, node: usize, from: usize, to: usize) {
                if from >= self.source.end || to <= self.source.start {
                    return;
                }
                let (left, right) = self.tree.bounds[node];
                if right < self.viewport.start - 0.5 || left > self.viewport.end + 0.5 {
                    return;
                }
                if node >= self.tree.leaves {
                    for index in from.max(self.source.start)..to.min(self.source.end) {
                        if let Some((left, right)) = band(self.low, self.high, index)
                            && right >= self.viewport.start - 0.5
                            && left <= self.viewport.end + 0.5
                        {
                            self.bands.push((left, right));
                        }
                    }
                } else {
                    let mid = (from + to) / 2;
                    self.visit(node * 2, from, mid);
                    self.visit(node * 2 + 1, mid, to);
                }
            }
            fn rightmost(&self, node: usize, from: usize, to: usize) -> f32 {
                if from >= self.source.end || to <= self.source.start {
                    return f32::NEG_INFINITY;
                }
                if self.source.start <= from && to <= self.source.end {
                    return self.tree.bounds[node].1;
                }
                if node >= self.tree.leaves {
                    return (from.max(self.source.start)..to.min(self.source.end))
                        .filter_map(|i| band(self.low, self.high, i).map(|b| b.1))
                        .fold(f32::NEG_INFINITY, f32::max);
                }
                let mid = (from + to) / 2;
                self.rightmost(node * 2, from, mid)
                    .max(self.rightmost(node * 2 + 1, mid, to))
            }
        }
        let mut query = Query {
            tree: self,
            low,
            high,
            source,
            viewport,
            bands: Vec::new(),
        };
        query.visit(1, 0, self.leaves * BLOCK);
        if extend > 0.0 {
            let right = query.rightmost(1, 0, self.leaves * BLOCK);
            if right.is_finite() {
                query.bands.push((right, right + extend));
            }
        }
        query.bands.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut merged: Vec<(f32, f32)> = Vec::new();
        for (left, right) in query.bands {
            if let Some(last) = merged.last_mut()
                && left <= last.1 + 0.5
            {
                last.1 = last.1.max(right);
            } else {
                merged.push((left, right));
            }
        }
        merged
            .into_iter()
            .filter_map(|(left, right)| {
                let left = left.max(query.viewport.start);
                let right = right.min(query.viewport.end);
                (right > left).then_some((left, right))
            })
            .collect()
    }
}
