//! The atlas's pages: allocating cells, and evicting the page least
//! recently drawn from when they run out.

use super::*;

/// Where a character lives in the atlas and how it should be drawn.
#[derive(Clone, Copy, Debug)]
pub struct Slot {
    /// Atlas rect: u0, v0, u1, v1.
    pub uv: [f32; 4],
    /// Width in character cells. 2 for CJK and emoji, 1 for everything else.
    pub cells: u8,
    /// Whether the glyph carries its own colour and must not be tinted.
    pub color: bool,
    pub page: u32,
    /// Where the quad starts relative to the glyph's pen position, in
    /// points: negative when ink reaches left of it (an italic `f`), which
    /// the cell was shifted to hold.
    pub dx: f32,
}
impl Slot {
    pub fn flags(self) -> u32 {
        u32::from(self.color) | (self.page << 1)
    }
}
pub(super) struct Page {
    pub(super) pixels: Vec<u8>,
    pub(super) next_cell: usize,
    pub(super) last_used: u64,
    pub(super) dirty: Dirty,
}
/// What of a page has to reach its texture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Dirty {
    #[default]
    Clean,
    /// Glyphs were added in these rows of pixels, into cells no frame has
    /// used: they can be copied into the texture in place.
    Rows(usize, usize),
    /// New, or evicted and refilled: a new texture, so frames still in
    /// flight keep the old pixels.
    Whole,
}
impl Dirty {
    pub(super) fn add_rows(&mut self, from: usize, to: usize) {
        *self = match *self {
            Dirty::Clean => Dirty::Rows(from, to),
            Dirty::Rows(a, b) => Dirty::Rows(a.min(from), b.max(to)),
            Dirty::Whole => Dirty::Whole,
        };
    }
}

impl Atlas {
    pub fn page_count(&self) -> usize {
        1 + self.pages.len()
    }
    pub fn page_pixels(&self, page: usize) -> &[u8] {
        if page == 0 {
            &self.pixels
        } else {
            &self.pages[page - 1].pixels
        }
    }
    pub fn page_dirty(&self, page: usize) -> Dirty {
        if page == 0 {
            self.primary_dirty
        } else {
            self.pages[page - 1].dirty
        }
    }
    pub fn mark_uploaded(&mut self) {
        self.dirty = false;
        self.primary_dirty = Dirty::Clean;
        for page in &mut self.pages {
            page.dirty = Dirty::Clean;
        }
    }
    pub(super) fn touch_page(&mut self, page: u32) {
        if page != 0 {
            self.pages[page as usize - 1].last_used = self.frame;
        }
    }
    /// Reserves consecutive cells without crossing a row or overwriting a
    /// page referenced by this frame. At the memory cap, evict the oldest
    /// unused page, leaving ASCII and all currently emitted quads intact.
    pub(super) fn alloc(&mut self, cells: usize) -> Option<usize> {
        fn reserve(next: &mut usize, cells: usize) -> Option<usize> {
            let mut start = *next;
            if start / COLS != (start + cells - 1) / COLS {
                start = (start / COLS + 1) * COLS;
            }
            if start + cells > CELLS {
                return None;
            }
            *next = start + cells;
            Some(start)
        }
        if let Some(cell) = reserve(&mut self.next_cell, cells) {
            return Some(cell);
        }
        for (index, page) in self.pages.iter_mut().enumerate() {
            if let Some(cell) = reserve(&mut page.next_cell, cells) {
                page.last_used = self.frame;
                return Some((index + 1) * CELLS + cell);
            }
        }
        if self.page_count() < self.max_pages {
            self.pages.push(Page {
                pixels: vec![0; self.pixels.len()],
                next_cell: cells,
                last_used: self.frame,
                dirty: Dirty::Whole,
            });
            self.dirty = true;
            return Some(self.pages.len() * CELLS);
        }
        let victim = self
            .pages
            .iter()
            .enumerate()
            .filter(|(_, page)| page.last_used != self.frame)
            .min_by_key(|(_, page)| page.last_used)
            .map(|(index, _)| index);
        if let Some(index) = victim {
            let number = (index + 1) as u32;
            self.slots.retain(|_, slot| slot.page != number);
            self.face_slots.retain(|_, slot| slot.page != number);
            for glyphs in self.shaped_slots.values_mut() {
                glyphs.retain(|_, slot| slot.page != number);
            }
            let page = &mut self.pages[index];
            page.pixels.fill(0);
            page.next_cell = cells;
            page.last_used = self.frame;
            page.dirty = Dirty::Whole;
            self.dirty = true;
            return Some((index + 1) * CELLS);
        }
        self.exhausted = true;
        None
    }
    pub(super) fn cell_origin(&self, cell: usize) -> (usize, usize) {
        let (cw, ch) = self.cell_px;
        let cell = cell % CELLS;
        ((cell % COLS) * cw, (cell / COLS) * ch)
    }
    /// The slot for a glyph drawn at `cell`, `cells` wide, with no shift.
    pub(super) fn slot_at(&self, cell: usize, cells: usize, color: bool) -> Slot {
        Slot {
            dx: 0.0,
            uv: self.cell_uv(cell, cells),
            page: (cell / CELLS) as u32,
            cells: cells as u8,
            color,
        }
    }
    pub(super) fn cell_uv(&self, cell: usize, cells: usize) -> [f32; 4] {
        let (cw, ch) = self.cell_px;
        let (x, y) = self.cell_origin(cell);
        [
            x as f32 / self.width as f32,
            y as f32 / self.height as f32,
            (x + cw * cells) as f32 / self.width as f32,
            (y + ch) as f32 / self.height as f32,
        ]
    }
    pub(super) fn cell_uv_inset(&self, cell: usize, inset: f32) -> [f32; 4] {
        let (cw, ch) = self.cell_px;
        let (x, y) = self.cell_origin(cell);
        let cx = x as f32 + cw as f32 * 0.5;
        let cy = y as f32 + ch as f32 * 0.5;
        [
            (cx - inset) / self.width as f32,
            (cy - inset) / self.height as f32,
            (cx + inset) / self.width as f32,
            (cy + inset) / self.height as f32,
        ]
    }
}
