//! Shaped editor lines kept between frames, and the snapshots the
//! shaping worker hands back.

use super::*;

/// Shaped text by key, bounded by what was drawn lately: when full, what
/// was not used this frame or the last goes. Clearing it all at the limit
/// meant a page with more distinct words than that shaped every word again,
/// every frame.
pub(super) struct RecentCache<K> {
    pub(super) limit: usize,
    pub(super) lines: HashMap<K, (Rc<ShapedLine>, u64)>,
}
impl<K: std::hash::Hash + Eq> RecentCache<K> {
    pub(super) fn new(limit: usize) -> Self {
        RecentCache {
            limit,
            lines: HashMap::new(),
        }
    }

    pub(super) fn get<Q>(&mut self, key: &Q, frame: u64) -> Option<Rc<ShapedLine>>
    where
        K: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        let (line, used) = self.lines.get_mut(key)?;
        *used = frame;
        Some(line.clone())
    }

    pub(super) fn insert(&mut self, key: K, line: Rc<ShapedLine>, frame: u64) {
        if self.lines.len() >= self.limit {
            self.lines.retain(|_, (_, used)| *used + 1 >= frame);
            // Everything is in use: a page with more than the limit on it.
            if self.lines.len() >= self.limit {
                self.lines.clear();
            }
        }
        self.lines.insert(key, (line, frame));
    }
}
/// A document's shaped lines, kept while another document is drawn.
pub(super) struct Parked {
    pub(super) id: u64,
    pub(super) rope: Rope,
    pub(super) lines: HashMap<usize, Option<Rc<ShapedLine>>>,
}
impl Parked {
    /// What its lines count against the cache's budget.
    pub(super) fn utf16(&self) -> usize {
        self.lines.values().flatten().map(|l| l.offsets.len()).sum()
    }
}

impl Atlas {
    /// Makes `rope` of document `doc` the text the shaped editor lines belong
    /// to: another document's lines are parked and this one's taken back,
    /// and lines that survived the edit move to their new numbers.
    pub(super) fn adopt_snapshot(&mut self, doc: u64, rope: &Rope) {
        let mut old = self.editor_snapshot.take();
        // Another document: park this one's lines and take that one's,
        // if they were kept.
        if old.as_ref().is_some_and(|(id, _)| *id != doc) {
            if let Some((id, rope)) = old.take() {
                let lines = std::mem::take(&mut self.editor_lines);
                self.parked.push(Parked { id, rope, lines });
            }
            if let Some(at) = self.parked.iter().position(|p| p.id == doc) {
                let parked = self.parked.remove(at);
                self.editor_lines = parked.lines;
                old = Some((parked.id, parked.rope));
            }
            while self.parked.len() > MAX_PARKED {
                let dropped = self.parked.remove(0);
                self.cached_utf16 -= dropped.utf16();
            }
        }
        let mapping = old
            .as_ref()
            .filter(|(id, _)| *id == doc)
            .map(|(_, old)| reuse::LineReuse::new(old, rope));
        let mut retained = HashMap::new();
        for (line, shaped) in self.editor_lines.drain() {
            if let Some((line, _)) = mapping.as_ref().and_then(|m| m.map(line)) {
                // Deleting duplicate text can map two old paragraphs to
                // one new line via overlapping prefix/suffix proofs.
                if let Some(Some(displaced)) = retained.insert(line, shaped) {
                    self.cached_utf16 -= displaced.offsets.len();
                }
            } else if let Some(shaped) = shaped {
                self.cached_utf16 -= shaped.offsets.len();
            }
        }
        self.editor_lines = retained;
        if let Some(worker) = &mut self.worker {
            worker.rebase(doc, mapping.as_ref());
        }
        self.editor_snapshot = Some((doc, rope.clone()));
    }
    /// Cache by immutable rope identity, remapping proven unchanged complete
    /// lines on edits. Long source extraction and mapping stay on the worker.
    pub fn shape_editor_line(
        &mut self,
        owner: (u64, usize),
        rope: &Rope,
        range: std::ops::Range<usize>,
    ) -> Option<Rc<ShapedLine>> {
        if !self.editor_snapshot_is(owner.0, rope) {
            self.adopt_snapshot(owner.0, rope);
        }
        if let Some(shaped) = self.editor_lines.get(&owner.1) {
            if let Some(worker) = &mut self.worker {
                worker.cancel(owner);
            }
            return shaped.clone();
        }
        if range.len() > MAX_SHAPED_LINE_BYTES
            || rope.byte_to_char(range.end) - rope.byte_to_char(range.start) == range.len()
        {
            self.cache_editor_line(owner.1, None);
            return None;
        }
        if range.len().saturating_mul(4) <= SYNC_SHAPED_BYTES {
            let shaped = shape_paragraph(
                &self.font,
                rope,
                range,
                self.metrics.scale,
                &AtomicBool::new(false),
            )
            .map(Rc::new);
            self.cache_editor_line(owner.1, shaped.clone());
            return shaped;
        }
        let worker = self.worker.get_or_insert_with(|| {
            worker::Worker::new(
                self.font_name.clone(),
                unsafe { self.font.size() } as f32,
                self.metrics.scale,
            )
        });
        worker.request(owner, rope, range);
        None
    }
    /// Whether the shaped editor lines belong to document `id` as `rope` has it.
    pub(super) fn editor_snapshot_is(&self, id: u64, rope: &Rope) -> bool {
        self.editor_snapshot
            .as_ref()
            .is_some_and(|(owner, old)| *owner == id && old.same_as(rope))
    }
    pub fn cached_editor_line(&self, owner: (u64, usize), rope: &Rope) -> Option<&ShapedLine> {
        if self.editor_snapshot_is(owner.0, rope) {
            return self.editor_lines.get(&owner.1)?.as_deref();
        }
        // The other pane's document, while this one was drawn last: a click
        // there lands by its shaping too, not by monospace column math.
        self.parked
            .iter()
            .find(|p| p.id == owner.0 && p.rope.same_as(rope))?
            .lines
            .get(&owner.1)?
            .as_deref()
    }
    pub(super) fn make_cache_room(&mut self, units: usize) {
        // Shaped lines count against the limit; the marker that an ASCII
        // line needs no shaping costs nothing and does not, or a tall
        // window evicted every frame what it had just shaped.
        let shaped_editor = self.editor_lines.values().filter(|l| l.is_some()).count();
        let mut over = self.shaped_lines.len() + shaped_editor >= MAX_SHAPED_ENTRIES;
        while over || self.cached_utf16 + units > MAX_CACHED_UTF16 {
            if !self.parked.is_empty() {
                let dropped = self.parked.remove(0);
                self.cached_utf16 -= dropped.utf16();
                continue;
            }
            over = false;
            if let Some(key) = self.shaped_lines.keys().next().cloned() {
                self.cached_utf16 -= self.shaped_lines.remove(&key).unwrap().offsets.len();
            } else if let Some(key) = self
                .editor_lines
                .iter()
                .find(|(_, l)| l.is_some())
                .map(|(k, _)| *k)
            {
                if let Some(shaped) = self.editor_lines.remove(&key).flatten() {
                    self.cached_utf16 -= shaped.offsets.len();
                }
            } else {
                break;
            }
        }
        // The markers are bounded on their own.
        if self.editor_lines.len() > MAX_EDITOR_MARKERS {
            self.editor_lines.retain(|_, l| l.is_some());
        }
    }
    pub(super) fn cache_editor_line(&mut self, line: usize, shaped: Option<Rc<ShapedLine>>) {
        let units = shaped.as_ref().map_or(0, |s| s.offsets.len());
        self.make_cache_room(units);
        if let Some(old) = self.editor_lines.insert(line, shaped).flatten() {
            self.cached_utf16 -= old.offsets.len();
        }
        self.cached_utf16 += units;
    }
    pub fn finish_shaping_frame(&mut self) {
        if let Some(worker) = &mut self.worker {
            worker.finish_frame();
        }
    }
    /// Waits for the shaping and rasterising workers, calling `rebuild`
    /// every 2 ms the way the display link would, and panics past `limit`.
    /// For benchmarks, dumps and tests that want the settled frame.
    pub fn settle_shaping(
        &mut self,
        limit: std::time::Duration,
        mut rebuild: impl FnMut(&mut Self),
    ) {
        let started = std::time::Instant::now();
        while self.has_pending_shaping() {
            assert!(
                started.elapsed() < limit,
                "shaping did not finish in {limit:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
            rebuild(self);
        }
    }
    pub fn has_pending_shaping(&self) -> bool {
        self.worker.as_ref().is_some_and(worker::Worker::is_pending)
            || self
                .raster_worker
                .as_ref()
                .is_some_and(raster::Worker::is_pending)
    }
    pub(super) fn cache_shaped(&mut self, text: String, shaped: ShapedLine) -> Rc<ShapedLine> {
        self.make_cache_room(shaped.offsets.len());
        let shaped = Rc::new(shaped);
        if let Some(old) = self.shaped_lines.insert(text, shaped.clone()) {
            self.cached_utf16 -= old.offsets.len();
        }
        self.cached_utf16 += shaped.offsets.len();
        shaped
    }
}
