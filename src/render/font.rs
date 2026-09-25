//! Glyph rasterization and atlas packing, via CoreText.
//!
//! The font stack is the OS. CoreText handles rasterization, hinting, metrics
//! and font fallback, which is both better than anything we would write and
//! zero additional dependency surface.
//!
//! The atlas is a set of RGBA pages filled lazily. ASCII is rasterized up
//! front because a code file is mostly ASCII and that keeps the common path
//! free of any per-frame work; anything else is added the first time it is
//! seen, from whatever fallback font CoreText picks for it. The texture is
//! re-uploaded only when something new lands in it.
//!
//! RGBA rather than a coverage mask because emoji carry their own colour. A
//! monochrome glyph is stored as premultiplied white and tinted by the
//! instance colour; a colour glyph is stored as-is and used directly. Which
//! of the two applies travels with each quad as a flag.
//!
//! ASCII uses direct character lookup; other text is shaped as a CTLine.

use crate::text::columns::TAB_WIDTH;
use crate::text::rope::Rope;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

mod raster;
mod reuse;
mod selection;
mod worker;

use objc2_core_foundation::{
    CFAttributedString, CFData, CFDictionary, CFRange, CFRetained, CFString, CGFloat, CGPoint,
    CGRect, CGSize, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
};
use objc2_core_graphics::{CGColorSpace, CGContext, CGDataProvider, CGFont};
use objc2_core_text::{
    CTFont, CTFontOrientation, CTFontSymbolicTraits, CTLine, CTRun, CTTypesetter,
    kCTFontAttributeName,
};

unsafe extern "C" {
    fn crc_paragraph_typesetter(attributed: &CFAttributedString) -> Option<NonNull<CTTypesetter>>;
    fn crc_line_indexed_carets(
        line: &CTLine,
        attributed: &CFAttributedString,
        typesetter: &CTTypesetter,
        scale: f32,
        left: *mut f32,
        right: *mut f32,
        primary: *mut f32,
        count: usize,
        cancelled: Option<extern "C" fn(*const c_void) -> bool>,
        context: *const c_void,
    ) -> bool;
    #[cfg(test)]
    fn crc_line_caret_offsets(
        line: &CTLine,
        scale: f32,
        primary: *mut f32,
        secondary: *mut f32,
        count: usize,
        cancelled: Option<extern "C" fn(*const c_void) -> bool>,
        context: *const c_void,
    ) -> bool;
}

#[cfg(test)]
fn caret_offsets(
    line: &CTLine,
    text: &str,
    scale: f32,
    cancel: &AtomicBool,
) -> Option<(Vec<f32>, Vec<f32>)> {
    extern "C" fn cancelled(context: *const c_void) -> bool {
        // The synchronous C bridge borrows this AtomicBool only for this call.
        unsafe { &*context.cast::<AtomicBool>() }.load(Ordering::Relaxed)
    }
    let count = text.encode_utf16().count() + 1;
    let mut primary = vec![0.0; count];
    let mut secondary = vec![0.0; count];
    // Both buffers have count writable elements and the bridge is synchronous.
    let completed = unsafe {
        crc_line_caret_offsets(
            line,
            scale,
            primary.as_mut_ptr(),
            secondary.as_mut_ptr(),
            count,
            Some(cancelled),
            (cancel as *const AtomicBool).cast(),
        )
    };
    if !completed {
        return None;
    }
    // Invisible bidi controls can be omitted by enumeration even though they
    // are source boundaries. Resolve just these explicitly.
    for (index, unit) in text.encode_utf16().enumerate() {
        if index.is_multiple_of(128) && cancel.load(Ordering::Relaxed) {
            return None;
        }
        if matches!(unit, 0x202a..=0x202e | 0x2066..=0x2069) {
            let mut other = 0.0;
            let value = unsafe { line.offset_for_string_index(index as isize, &mut other) };
            primary[index] = value.min(other) as f32 / scale;
            secondary[index] = value.max(other) as f32 / scale;
        }
    }
    Some((primary, secondary))
}

#[derive(Clone, Copy)]
pub struct ShapedGlyph {
    pub x: f32,
    pub source_utf16: usize,
    glyph: u16,
    font: usize,
}

#[derive(Clone)]
pub struct ShapedLine {
    #[cfg(test)]
    line: CFRetained<CTLine>,
    #[cfg(test)]
    scale: f32,
    fonts: Vec<(String, CFRetained<CTFont>)>,
    pub glyphs: Vec<ShapedGlyph>,
    /// Visual caret edges; order is left/right, not bidi primary/secondary.
    pub offsets: Vec<f32>,
    pub secondary_offsets: Vec<f32>,
    /// Unique visual edges, sorted by x, with earliest source byte on ties.
    hit_stops: Vec<(f32, usize)>,
    pub source_bytes: Vec<usize>,
    primary_carets: Vec<f32>,
    selections: selection::SelectionIndex,
}

impl ShapedLine {
    /// Geometry is sorted by visual x once on the shaping worker. Include
    /// the same two-cell overhang used by layout's clipping predicate.
    pub fn visible_glyphs(&self, left: f32, right: f32, overhang: f32) -> &[ShapedGlyph] {
        let start = self
            .glyphs
            .partition_point(|glyph| glyph.x + overhang <= left);
        let end = self.glyphs.partition_point(|glyph| glyph.x <= right);
        &self.glyphs[start.min(end)..end]
    }

    /// The worker enumerates graphemes once; a click checks two visual neighbors.
    pub fn byte_at_x(&self, x: f32) -> usize {
        let right = self.hit_stops.partition_point(|stop| stop.0 < x);
        let mut best = (0, f32::INFINITY);
        for index in [right.checked_sub(1), Some(right)].into_iter().flatten() {
            if let Some(&(edge, byte)) = self.hit_stops.get(index) {
                let distance = (edge - x).abs();
                if distance < best.1 || (distance == best.1 && byte < best.0) {
                    best = (byte, distance);
                }
            }
        }
        best.0
    }

    pub fn selection_intervals(
        &self,
        from: usize,
        to: usize,
        viewport: std::ops::Range<f32>,
        extend: f32,
    ) -> Vec<(f32, f32)> {
        let first = self.source_bytes.partition_point(|&b| b < from);
        let last = self
            .source_bytes
            .partition_point(|&b| b < to)
            .min(self.offsets.len() - 1);
        self.selections.intervals(
            &self.offsets,
            &self.secondary_offsets,
            first..last,
            viewport,
            extend,
        )
    }

    /// Every native primary position is prepared on the worker, including
    /// ambiguous bidi boundaries and cluster interiors.
    pub fn caret_offset(&self, index: usize) -> f32 {
        self.primary_carets[index.min(self.primary_carets.len() - 1)]
    }
}

fn shape(font: &CTFont, source: &str, scale: f32, cancel: &AtomicBool) -> Option<ShapedLine> {
    let started = std::time::Instant::now();
    let (text, bytes) = shape_input_bounded(source, MAX_CACHED_UTF16 - 1, cancel)?;
    let input_time = started.elapsed();
    let text = text.as_str();
    let key = unsafe { kCTFontAttributeName } as *const CFString as *const c_void;
    let value = font as *const CTFont as *const c_void;
    let attributes = unsafe {
        CFDictionary::new(
            None,
            &key as *const _ as *mut _,
            &value as *const _ as *mut _,
            1,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )?
    };
    let string = CFString::from_str(text);
    let attributed = unsafe { CFAttributedString::new(None, Some(&string), Some(&attributes))? };
    let typesetter = unsafe { CFRetained::from_raw(crc_paragraph_typesetter(&attributed)?) };
    let line = unsafe {
        typesetter.line(CFRange {
            location: 0,
            length: 0,
        })
    };
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    let line_time = started.elapsed();
    extern "C" fn cancelled(context: *const c_void) -> bool {
        unsafe { &*context.cast::<AtomicBool>() }.load(Ordering::Relaxed)
    }
    let count = bytes.len();
    let mut offsets = vec![0.0; count];
    let mut secondary_offsets = vec![0.0; count];
    let mut primary_carets = vec![0.0; count];
    if !unsafe {
        crc_line_indexed_carets(
            &line,
            &attributed,
            &typesetter,
            scale,
            offsets.as_mut_ptr(),
            secondary_offsets.as_mut_ptr(),
            primary_carets.as_mut_ptr(),
            count,
            Some(cancelled),
            (cancel as *const AtomicBool).cast(),
        )
    } {
        return None;
    }
    let edges_time = started.elapsed();
    // Use the original source: expanded tab spaces are not cursor stops,
    // and a combining mark after a tab must keep its original grapheme context.
    let composed = objc2_foundation::NSString::from_str(source);
    let mut hit_stops = Vec::new();
    let mut utf16 = 0;
    let mut expanded_index = 0;
    let mut previous = None;
    for (byte, ch) in source.char_indices() {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        // Adjacent ASCII scalars have a grapheme break, except CRLF. Keep
        // native context at every Unicode transition and inside clusters.
        let ascii_break = ch.is_ascii()
            && previous.is_some_and(|prev: char| prev.is_ascii() && !(prev == '\r' && ch == '\n'));
        if ascii_break
            || composed
                .rangeOfComposedCharacterSequenceAtIndex(utf16)
                .location
                == utf16
        {
            while bytes[expanded_index] < byte {
                expanded_index += 1;
            }
            let index = expanded_index;
            hit_stops.push((offsets[index], byte));
            hit_stops.push((secondary_offsets[index], byte));
        }
        utf16 += ch.len_utf16();
        previous = Some(ch);
    }
    // Match the existing hit-test convention: only the primary end edge.
    hit_stops.push((*offsets.last()?, source.len()));
    hit_stops.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    hit_stops.dedup_by(|a, b| a.0 == b.0);
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    let stops_time = started.elapsed();
    let runs = unsafe { line.glyph_runs() };
    let mut result = Vec::new();
    let mut fonts = Vec::new();
    let mut font_indices = HashMap::new();
    for run_index in 0..runs.count() {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let run_ptr = unsafe { runs.value_at_index(run_index) } as *const CTRun;
        let run = unsafe { run_ptr.as_ref()? };
        let count = unsafe { run.glyph_count() } as usize;
        if count == 0 {
            continue;
        }
        let attrs = unsafe { run.attributes() };
        let font_ptr = unsafe { attrs.value(key) } as *const CTFont;
        let font = unsafe { font_ptr.as_ref()? };
        let mut glyphs = vec![0u16; count];
        let mut positions = vec![CGPoint { x: 0.0, y: 0.0 }; count];
        let mut indices = vec![0isize; count];
        let range = CFRange {
            location: 0,
            length: count as isize,
        };
        unsafe {
            run.glyphs(range, NonNull::new(glyphs.as_mut_ptr())?);
            run.positions(range, NonNull::new(positions.as_mut_ptr())?);
            run.string_indices(range, NonNull::new(indices.as_mut_ptr())?);
        }
        let font_index = *font_indices.entry(font_ptr as usize).or_insert_with(|| {
            let index = fonts.len();
            fonts.push((unsafe { font.full_name() }.to_string(), unsafe {
                CFRetained::retain(NonNull::from(font))
            }));
            index
        });
        for ((glyph, pos), index) in glyphs.into_iter().zip(positions).zip(indices) {
            result.push(ShapedGlyph {
                x: pos.x as f32 / scale,
                source_utf16: index.max(0) as usize,
                glyph,
                font: font_index,
            });
        }
    }
    result.sort_by(|a, b| a.x.total_cmp(&b.x));
    let selections = selection::SelectionIndex::new(&offsets, &secondary_offsets);
    if std::env::var_os("CRC_SHAPE_PROFILE").is_some() {
        eprintln!(
            "shape {} bytes: input {:?}, line {:?}, edges {:?}, stops {:?}, glyphs/index {:?}",
            source.len(),
            input_time,
            line_time - input_time,
            edges_time - line_time,
            stops_time - edges_time,
            started.elapsed() - stops_time
        );
    }
    Some(ShapedLine {
        selections,
        #[cfg(test)]
        line,
        #[cfg(test)]
        scale,
        fonts,
        glyphs: result,
        offsets,
        secondary_offsets,
        hit_stops,
        source_bytes: bytes,
        primary_carets,
    })
}

/// Expand tabs to monospace spaces for CoreText while retaining the source
/// byte for each UTF-16 unit. The final entry represents the line end.
#[cfg(test)]
pub(crate) fn shape_input(text: &str) -> (String, Vec<usize>) {
    shape_input_bounded(text, usize::MAX, &AtomicBool::new(false)).unwrap()
}

fn shape_input_bounded(
    text: &str,
    max_units: usize,
    cancel: &AtomicBool,
) -> Option<(String, Vec<usize>)> {
    let mut expanded = String::with_capacity(text.len().min(max_units));
    let mut source_bytes = Vec::with_capacity(text.len().min(max_units) + 1);
    let mut column = 0usize;
    for (byte, ch) in text.char_indices() {
        let units = if ch == '\t' {
            TAB_WIDTH - column % TAB_WIDTH
        } else {
            ch.len_utf16()
        };
        if source_bytes.len().saturating_add(units) > max_units || cancel.load(Ordering::Relaxed) {
            return None;
        }
        if ch == '\t' {
            let spaces = TAB_WIDTH - column % TAB_WIDTH;
            for _ in 0..spaces {
                expanded.push(' ');
                source_bytes.push(byte);
            }
            column += spaces;
        } else {
            expanded.push(ch);
            source_bytes.extend(std::iter::repeat_n(byte, ch.len_utf16()));
            column += display_width(ch);
        }
    }
    source_bytes.push(text.len());
    Some((expanded, source_bytes))
}

/// First character rasterized up front (space).
const FIRST_ASCII: u32 = 32;
/// Last character rasterized up front (tilde).
const LAST_ASCII: u32 = 126;

/// Fixed-size pages preserve UVs. Page zero pins ASCII and the solid swatch;
/// additional pages are allocated lazily and evicted by last frame used.
const COLS: usize = 48;
const ROWS: usize = 48;
const CELLS: usize = COLS * ROWS;
const MAX_ATLAS_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_ATLAS_PAGES: usize = 8;

/// Bound cold CoreText work and cached source mapping. Larger lines retain
/// the rope chunk renderer until incremental paragraph shaping is available.
pub const MAX_SHAPED_LINE_BYTES: usize = 2 * 1024 * 1024;
const SYNC_SHAPED_BYTES: usize = 4096;
const MAX_CACHED_UTF16: usize = 4 * 1024 * 1024 + 1;

/// Transparent gutter around each cell, so sampling at a cell edge cannot
/// bleed a neighbouring glyph in.
const PAD: usize = 2;

/// Prose never shrinks below this, however small the cells are.
const MD_MIN_PROSE_PT: f32 = 11.0;

/// A proportional weight and slant, for prose.
///
/// Bold and italic used to be expressed only as a colour, because there was a
/// single UI face: `**bold**` was drawn in `md_heading`, which is within a
/// percent of the body colour, so it was invisible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Face {
    Regular,
    Bold,
    Italic,
    BoldItalic,
}

impl Face {
    pub fn bold(self) -> bool {
        matches!(self, Face::Bold | Face::BoldItalic)
    }
    pub fn italic(self) -> bool {
        matches!(self, Face::Italic | Face::BoldItalic)
    }
    pub fn with_bold(self, bold: bool) -> Face {
        match (bold, self.italic()) {
            (true, true) => Face::BoldItalic,
            (true, false) => Face::Bold,
            (false, true) => Face::Italic,
            (false, false) => Face::Regular,
        }
    }
    pub fn with_italic(self, italic: bool) -> Face {
        match (self.bold(), italic) {
            (true, true) => Face::BoldItalic,
            (true, false) => Face::Bold,
            (false, true) => Face::Italic,
            (false, false) => Face::Regular,
        }
    }
}

/// Line height as a multiple of the font size.
///
/// The font's own metrics give roughly 1.23 for a monospace face, which is
/// what a terminal uses. Editors sit near 1.45, and the difference is the
/// first thing anyone reads as "modern" without being able to name it.
const LINE_HEIGHT_RATIO: f32 = 1.45;

// CoreGraphics functions the objc2 bindings do not generate. These are stable
// system APIs that have been in CoreGraphics for two decades; declaring them
// here links against the OS, it does not add a dependency.
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C-unwind" {
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: *const CGColorSpace,
        bitmap_info: u32,
    ) -> *mut CGContext;
    fn CGColorSpaceCreateDeviceRGB() -> *mut CGColorSpace;
    fn CGContextSetRGBFillColor(c: *mut CGContext, r: CGFloat, g: CGFloat, b: CGFloat, a: CGFloat);
    fn CGContextSetShouldAntialias(c: *mut CGContext, should: bool);
    fn CGContextSetShouldSmoothFonts(c: *mut CGContext, should: bool);
    fn CGContextRelease(c: *mut CGContext);
    fn CGColorSpaceRelease(s: *mut CGColorSpace);
}

/// 8 bits per component, alpha last, premultiplied.
const K_CG_IMAGE_ALPHA_PREMULTIPLIED_LAST: u32 = 1;

/// Metrics of one character cell, in logical points.
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    /// Horizontal advance of a single-width character.
    pub advance: f32,
    /// Baseline-to-baseline distance used for layout.
    ///
    /// Deliberately looser than the font's natural `ascent + descent +
    /// leading`, which for a 13pt monospace is about 16pt, a ratio of 1.23.
    /// That is terminal density. 1.45 is editor density, and it is the single
    /// loudest difference between the two.
    pub line_height: f32,
    /// Height of a rasterised glyph cell. Stays at the font's natural size,
    /// because it is what the atlas allocated; the extra leading is padding
    /// around it, not inside it.
    pub cell_height: f32,
    /// Cell top to baseline.
    pub ascent: f32,
    /// Baseline to cell bottom.
    pub descent: f32,
    /// Backing scale the atlas is rasterized at (2.0 on Retina).
    pub scale: f32,
}

impl Metrics {
    /// `points` moved to the nearest whole device pixel.
    ///
    /// Glyphs are bitmaps sampled with a nearest filter, one texel per
    /// pixel. That only works if the quad starts on a pixel. Half a pixel
    /// off, every pixel centre lands exactly on the line between two texels
    /// and rounding error picks the row: the same digit came out as four
    /// different bitmaps on four consecutive lines, and the row above a
    /// glyph's cell in the atlas leaked in along its top edge.
    pub fn snap(&self, points: f32) -> f32 {
        (points * self.scale).round() / self.scale
    }

    /// How far down a row of `row_height` a glyph cell starts so that the
    /// text is centred in it, on a whole pixel.
    pub fn glyph_dy(&self, row_height: f32) -> f32 {
        self.snap(((row_height - self.cell_height) * 0.5).max(0.0))
    }
}

/// Whether `ch` is drawn from the bundled icon font.
///
/// The private-use areas, where icon fonts keep their glyphs because Unicode
/// promises never to assign anything there. Icons get two cells: squeezed
/// into one they are eight points wide and read as specks.
pub use crate::text::columns::is_icon;

/// The icon font, compiled in. See third_party/SOURCES.md for where it came
/// from and what it is licensed under.
///
/// Built straight from these bytes and used only by this atlas. It is never
/// registered with the system, so nothing else in the process or on the
/// machine can see it, and there is no file to find at run time.
static ICON_FONT: &[u8] =
    include_bytes!("../../third_party/nerd-fonts-symbols/SymbolsNerdFont-Regular.ttf");

fn load_icon_font(size_px: f32) -> Option<CFRetained<CTFont>> {
    let data = CFData::from_static_bytes(ICON_FONT);
    let provider = CGDataProvider::with_cf_data(Some(&data))?;
    let graphics_font = CGFont::with_data_provider(&provider)?;
    // SAFETY: a live CGFont, a null matrix meaning identity, no descriptor.
    Some(unsafe {
        CTFont::with_graphics_font(&graphics_font, size_px as CGFloat, std::ptr::null(), None)
    })
}

pub use crate::text::columns::display_width;

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
}

impl Slot {
    pub fn flags(self) -> u32 {
        u32::from(self.color) | (self.page << 1)
    }
}

struct Page {
    pixels: Vec<u8>,
    next_cell: usize,
    last_used: u64,
    dirty: bool,
}

pub struct Atlas {
    pub width: u32,
    pub height: u32,
    /// RGBA8, premultiplied, `width * height * 4` bytes.
    pub pixels: Vec<u8>,
    pub metrics: Metrics,
    /// Which font candidate actually resolved. See [`load_monospace`].
    pub font_name: String,
    /// Set when new glyphs have landed and the texture needs re-uploading.
    pub dirty: bool,

    font: CFRetained<CTFont>,
    ui_font: CFRetained<CTFont>,
    ui_lines: HashMap<String, Rc<ShapedLine>>,
    /// Proportional faces for the Markdown view, built on first use and kept
    /// for the life of the atlas. A preview drawn in one monospace weight
    /// cannot show a heading as a heading or bold as bold; it reads as a
    /// terminal rather than as a document.
    prose_fonts: HashMap<(u16, Face), CFRetained<CTFont>>,
    prose_lines: HashMap<(String, u16, Face), Rc<ShapedLine>>,
    max_prose: Option<f32>,
    /// Where private-use characters come from. `None` if CoreGraphics
    /// refused the bytes, in which case those characters are simply missing.
    icons: Option<CFRetained<CTFont>>,
    /// Cell size in device pixels, including padding.
    cell_px: (usize, usize),
    slots: HashMap<char, Slot>,
    shaped_slots: HashMap<(String, u16), Slot>,
    shaped_lines: HashMap<String, Rc<ShapedLine>>,
    cached_utf16: usize,
    editor_snapshot: Option<(u64, Rope)>,
    editor_lines: HashMap<usize, Option<Rc<ShapedLine>>>,
    worker: Option<worker::Worker>,
    raster_worker: Option<raster::Worker>,
    /// Next free cell index.
    next_cell: usize,
    pages: Vec<Page>,
    max_pages: usize,
    frame: u64,
    primary_dirty: bool,
    exhausted: bool,
    /// Characters no font on this system can draw, remembered so they are not
    /// retried on every frame they appear in.
    missing: HashSet<char>,
    solid: [f32; 4],
}

impl Atlas {
    /// Rasterizes ASCII from `font_name` at `size_pt`, for a display with
    /// backing scale `scale`.
    pub fn build(font_name: &str, size_pt: f32, scale: f32) -> Atlas {
        Atlas::build_with_ui(font_name, size_pt, size_pt, scale)
    }

    /// As [`Atlas::build`], with the UI labels and icons at `ui_pt` rather
    /// than the code size: zooming the code must not grow the sidebar rows
    /// out from under their fixed geometry.
    pub fn build_with_ui(font_name: &str, size_pt: f32, ui_pt: f32, scale: f32) -> Atlas {
        let Loaded {
            font,
            glyphs,
            advance_px,
            resolved_name,
        } = load_monospace(font_name, size_pt * scale);

        // Metrics come back in device pixels because the font was created at
        // the scaled size.
        let ascent_px = unsafe { font.ascent() } as f32;
        let descent_px = unsafe { font.descent() } as f32;
        let leading_px = unsafe { font.leading() } as f32;

        let advance_px = advance_px.ceil();
        // The rasterisation cell keeps the font's natural metrics.
        let cell_px_h = (ascent_px + descent_px + leading_px).ceil();
        // Layout uses a looser line. Rounded to a whole device pixel so rows
        // never land on a half-pixel and blur.
        let line_px = (LINE_HEIGHT_RATIO * size_pt * scale).round().max(cell_px_h);
        let cell_w = advance_px as usize + PAD;
        let cell_h = cell_px_h as usize + PAD;

        let width = cell_w * COLS;
        let height = cell_h * ROWS;

        let mut atlas = Atlas {
            width: width as u32,
            height: height as u32,
            pixels: vec![0u8; width * height * 4],
            metrics: Metrics {
                advance: advance_px / scale,
                line_height: line_px / scale,
                cell_height: cell_px_h / scale,
                ascent: ascent_px / scale,
                descent: descent_px / scale,
                scale,
            },
            font_name: resolved_name,
            dirty: true,
            ui_font: unsafe {
                CTFont::with_name(
                    &CFString::from_str(".AppleSystemUIFont"),
                    (ui_pt * scale) as CGFloat,
                    std::ptr::null(),
                )
            },
            ui_lines: HashMap::new(),
            prose_fonts: HashMap::new(),
            prose_lines: HashMap::new(),
            max_prose: None,
            font,
            icons: load_icon_font(ui_pt * scale),
            cell_px: (cell_w, cell_h),
            slots: HashMap::with_capacity(CELLS),
            shaped_slots: HashMap::new(),
            shaped_lines: HashMap::new(),
            cached_utf16: 0,
            editor_snapshot: None,
            editor_lines: HashMap::new(),
            worker: None,
            raster_worker: None,
            next_cell: 0,
            pages: Vec::new(),
            max_pages: (MAX_ATLAS_BYTES / (width * height * 4)).clamp(1, MAX_ATLAS_PAGES),
            frame: 1,
            primary_dirty: true,
            exhausted: false,
            missing: HashSet::new(),
            solid: [0.0; 4],
        };

        // A fully opaque cell, so rectangles (cursor, selection, panels) go
        // through the glyph pipeline instead of needing a second one.
        let solid_cell = atlas.alloc(1).expect("atlas has room for the solid swatch");
        atlas.fill_cell_opaque(solid_cell);
        atlas.solid = atlas.cell_uv_inset(solid_cell, 2.0);

        // Rasterize ASCII up front: it is what a code file is mostly made of,
        // and doing it here keeps the steady-state render path free of any
        // rasterization at all.
        let font = atlas.font.clone();
        for (i, glyph) in glyphs.iter().enumerate() {
            let ch = char::from_u32(FIRST_ASCII + i as u32).expect("ascii");
            let Some(cell) = atlas.alloc(1) else { break };
            atlas.draw_glyph_into(cell, &font, *glyph, ascent_px, 1);
            let uv = atlas.cell_uv(cell, 1);
            atlas.slots.insert(
                ch,
                Slot {
                    uv,
                    cells: 1,
                    color: false,
                    page: 0,
                },
            );
        }

        atlas
    }

    /// Start before emitting any quads. Pages used in this frame cannot be
    /// evicted until a later frame; queued GPU commands retain their textures.
    pub fn begin_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        self.exhausted = false;
        if let Some(worker) = &mut self.raster_worker {
            let ready = worker.poll();
            for (ch, bitmap) in ready {
                if self.slots.contains_key(&ch) {
                    continue;
                }
                if let Some(bitmap) = bitmap {
                    if let Some(cell) = self.alloc(bitmap.cells) {
                        self.copy_bitmap(cell, bitmap.cells, &bitmap.pixels);
                        self.slots.insert(
                            ch,
                            Slot {
                                uv: self.cell_uv(cell, bitmap.cells),
                                page: (cell / CELLS) as u32,
                                cells: bitmap.cells as u8,
                                color: bitmap.color,
                            },
                        );
                    }
                } else {
                    self.missing.insert(ch);
                }
            }
        }
        if let Some(worker) = &mut self.worker {
            let completed = worker.poll();
            worker.begin_frame();
            for (owner, rope, shaped) in completed {
                if self
                    .editor_snapshot
                    .as_ref()
                    .is_some_and(|(id, snapshot)| *id == owner.0 && snapshot.same_snapshot(&rope))
                {
                    self.cache_editor_line(owner.1, shaped.map(Rc::new));
                }
            }
        }
    }

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

    pub fn page_dirty(&self, page: usize) -> bool {
        if page == 0 {
            self.primary_dirty
        } else {
            self.pages[page - 1].dirty
        }
    }

    pub fn mark_uploaded(&mut self) {
        self.dirty = false;
        self.primary_dirty = false;
        for page in &mut self.pages {
            page.dirty = false;
        }
    }

    fn touch_page(&mut self, page: u32) {
        if page != 0 {
            self.pages[page as usize - 1].last_used = self.frame;
        }
    }

    /// Looks a character up, rasterizing it on first sight.
    ///
    /// Returns `None` only when no font on the system has the character, or
    /// the atlas is full.
    pub fn slot_for(&mut self, ch: char) -> Option<Slot> {
        if let Some(slot) = self.slots.get(&ch).copied() {
            self.touch_page(slot.page);
            return Some(slot);
        }
        if self.missing.contains(&ch) {
            return None;
        }
        match self.rasterize_new(ch) {
            Some(slot) => {
                self.slots.insert(ch, slot);
                self.dirty = true;
                Some(slot)
            }
            None => {
                if !self.exhausted {
                    self.missing.insert(ch);
                }
                None
            }
        }
    }

    /// Cold fallback glyphs are resolved and rasterized off the UI thread.
    /// The resident question mark keeps pending text visible at its source
    /// column until the bitmap arrives. Offline/chrome callers keep slot_for.
    pub fn slot_for_fallback(&mut self, ch: char) -> Option<Slot> {
        if let Some(slot) = self.slots.get(&ch).copied() {
            self.touch_page(slot.page);
            return Some(slot);
        }
        if self.missing.contains(&ch) {
            return None;
        }
        // Private-use icons retain their dedicated font and fitting behavior.
        if is_icon(ch) {
            return self.slot_for(ch);
        }
        let worker = self.raster_worker.get_or_insert_with(|| {
            raster::Worker::new(
                self.font_name.clone(),
                unsafe { self.font.size() } as f32,
                self.cell_px,
                self.metrics.ascent * self.metrics.scale,
            )
        });
        worker.request(ch);
        self.peek('?')
    }

    /// Read-only lookup, for callers that cannot rasterize.
    pub fn peek(&self, ch: char) -> Option<Slot> {
        self.slots.get(&ch).copied()
    }

    /// Texture coordinates of a fully-opaque swatch.
    pub fn solid_uv(&self) -> [f32; 4] {
        self.solid
    }

    /// Cell size in logical points, including the sampling gutter.
    pub fn cell_size(&self) -> (f32, f32) {
        (
            self.cell_px.0 as f32 / self.metrics.scale,
            self.cell_px.1 as f32 / self.metrics.scale,
        )
    }

    /// How many glyphs are currently resident.
    pub fn resident(&self) -> usize {
        self.slots.len() + self.shaped_slots.len()
    }

    /// Synchronous shaping for bounded short strings and explicit offline callers.
    pub fn shape_line(&mut self, text: &str) -> Option<Rc<ShapedLine>> {
        if let Some(shaped) = self.shaped_lines.get(text) {
            return Some(shaped.clone());
        }
        if text.len() > MAX_SHAPED_LINE_BYTES {
            return None;
        }
        let shaped = shape(
            &self.font,
            text,
            self.metrics.scale,
            &AtomicBool::new(false),
        )?;
        Some(self.cache_shaped(text.to_owned(), shaped))
    }

    /// The proportional face for one size and weight, built once.
    ///
    /// Bold and italic come from the system UI font's own family rather than
    /// from a synthesised slant, so they are the faces the rest of macOS uses.
    /// A size the system has no face for falls back to the plain one, which is
    /// why this returns the regular font rather than `None`.
    fn prose_font(&mut self, size_pt: f32, face: Face) -> CFRetained<CTFont> {
        // Tenths of a point, so the key is exact without being a float.
        let key = ((size_pt * 10.0) as u16, face);
        if let Some(font) = self.prose_fonts.get(&key) {
            return font.clone();
        }
        let px = (size_pt * self.metrics.scale) as CGFloat;
        let base = unsafe {
            CTFont::with_name(
                &CFString::from_str(".AppleSystemUIFont"),
                px,
                std::ptr::null(),
            )
        };
        let mut traits = CTFontSymbolicTraits::empty();
        if face.bold() {
            traits |= CTFontSymbolicTraits::BoldTrait;
        }
        if face.italic() {
            traits |= CTFontSymbolicTraits::ItalicTrait;
        }
        let font = if traits.is_empty() {
            base
        } else {
            let mask = CTFontSymbolicTraits::BoldTrait | CTFontSymbolicTraits::ItalicTrait;
            unsafe { base.copy_with_symbolic_traits(px, std::ptr::null(), traits, mask) }
                .unwrap_or(base)
        };
        self.prose_fonts.insert(key, font.clone());
        font
    }

    /// The largest prose size this atlas can rasterize without clipping.
    ///
    /// Glyphs live in cells sized for the monospace face, so a proportional
    /// face taller than a cell loses its ascenders. Rather than draw clipped
    /// letters, prose is capped here and headings step down in proportion.
    /// Lifting this means giving the atlas a second grid with taller cells,
    /// which is a change to the allocator and the page model.
    pub fn max_prose_pt(&mut self) -> f32 {
        if let Some(cached) = self.max_prose {
            return cached;
        }
        let room = (self.cell_px.1 - PAD) as f32;
        // Measure the real face rather than assume a ratio: system fonts use
        // different ones at different optical sizes.
        let probe = 20.0f32;
        let font = self.prose_font(probe, Face::Bold);
        let height = unsafe { font.ascent() } as f32 + unsafe { font.descent() } as f32;
        let per_pt = height / (probe * self.metrics.scale);
        let cap = if per_pt > 0.0 {
            room / per_pt / self.metrics.scale
        } else {
            self.metrics.line_height
        };
        let cap = cap.max(MD_MIN_PROSE_PT);
        self.max_prose = Some(cap);
        cap
    }

    /// Shapes a run of prose at a size and weight, for the Markdown view.
    ///
    /// Cached by text *and* face and size: keying on the text alone, as the UI
    /// label cache does, would hand a heading back for the same words drawn in
    /// body text.
    pub fn shape_prose(&mut self, text: &str, size_pt: f32, face: Face) -> Option<Rc<ShapedLine>> {
        if text.len() > 512 {
            return None;
        }
        let key = (text.to_owned(), (size_pt * 10.0) as u16, face);
        if let Some(line) = self.prose_lines.get(&key) {
            return Some(line.clone());
        }
        let font = self.prose_font(size_pt, face);
        let line = Rc::new(shape(
            &font,
            text,
            self.metrics.scale,
            &AtomicBool::new(false),
        )?);
        // Bounded like every other shaping cache here: a document is not
        // allowed to grow the atlas without limit just by being long.
        if self.prose_lines.len() >= 512 {
            self.prose_lines.clear();
        }
        self.prose_lines.insert(key, line.clone());
        Some(line)
    }

    /// UI labels use proportional system text, isolated from document shaping.
    /// Both the input and cache are bounded; filenames never shape a paragraph.
    pub fn shape_ui(&mut self, text: &str) -> Option<Rc<ShapedLine>> {
        if text.len() > 512 {
            return None;
        }
        if let Some(line) = self.ui_lines.get(text) {
            return Some(line.clone());
        }
        let line = Rc::new(shape(
            &self.ui_font,
            text,
            self.metrics.scale,
            &AtomicBool::new(false),
        )?);
        if self.ui_lines.len() >= 128 {
            self.ui_lines.clear();
        }
        self.ui_lines.insert(text.to_owned(), line.clone());
        Some(line)
    }

    /// Cache by immutable rope identity, remapping proven unchanged complete
    /// lines on edits. Long source extraction and mapping stay on the worker.
    pub fn shape_editor_line(
        &mut self,
        owner: (u64, usize),
        rope: &Rope,
        range: std::ops::Range<usize>,
    ) -> Option<Rc<ShapedLine>> {
        if !self
            .editor_snapshot
            .as_ref()
            .is_some_and(|(id, old)| *id == owner.0 && old.same_snapshot(rope))
        {
            let old = self.editor_snapshot.take();
            let mapping = old
                .as_ref()
                .filter(|(id, _)| *id == owner.0)
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
                worker.rebase(owner.0, mapping.as_ref());
            }
            self.editor_snapshot = Some((owner.0, rope.clone()));
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
            let text = rope.slice_to_string(range);
            let shaped = shape(
                &self.font,
                text.trim_end_matches(['\r', '\n']),
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

    pub fn cached_editor_line(&self, owner: (u64, usize), rope: &Rope) -> Option<&ShapedLine> {
        if !self
            .editor_snapshot
            .as_ref()
            .is_some_and(|(id, old)| *id == owner.0 && old.same_snapshot(rope))
        {
            return None;
        }
        self.editor_lines.get(&owner.1)?.as_deref()
    }

    fn make_cache_room(&mut self, units: usize) {
        while self.shaped_lines.len() + self.editor_lines.len() >= 256
            || self.cached_utf16 + units > MAX_CACHED_UTF16
        {
            if let Some(key) = self.shaped_lines.keys().next().cloned() {
                self.cached_utf16 -= self.shaped_lines.remove(&key).unwrap().offsets.len();
            } else if let Some(key) = self.editor_lines.keys().next().copied() {
                if let Some(shaped) = self.editor_lines.remove(&key).flatten() {
                    self.cached_utf16 -= shaped.offsets.len();
                }
            } else {
                break;
            }
        }
    }

    fn cache_editor_line(&mut self, line: usize, shaped: Option<Rc<ShapedLine>>) {
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

    pub fn has_pending_shaping(&self) -> bool {
        self.worker.as_ref().is_some_and(worker::Worker::is_pending)
            || self
                .raster_worker
                .as_ref()
                .is_some_and(raster::Worker::is_pending)
    }

    fn cache_shaped(&mut self, text: String, shaped: ShapedLine) -> Rc<ShapedLine> {
        self.make_cache_room(shaped.offsets.len());
        let shaped = Rc::new(shaped);
        if let Some(old) = self.shaped_lines.insert(text, shaped.clone()) {
            self.cached_utf16 -= old.offsets.len();
        }
        self.cached_utf16 += shaped.offsets.len();
        shaped
    }

    /// Rasterize only glyphs the layout actually emits. Shaping a long line
    /// must not fill the atlas with thousands of offscreen glyphs.
    pub fn slot_for_shaped(&mut self, shaped: &ShapedLine, glyph: &ShapedGlyph) -> Option<Slot> {
        let (font_name, font) = &shaped.fonts[glyph.font];
        // Keyed by size as well as face. The PostScript name is the same for
        // every size of a font, so a heading and body text drawn from one
        // family would otherwise share a cached bitmap and the second one
        // rendered would come out at the first one's size.
        let cache_key = (
            format!("{font_name}@{}", unsafe { font.size() } as u32),
            glyph.glyph,
        );
        if let Some(slot) = self.shaped_slots.get(&cache_key).copied() {
            self.touch_page(slot.page);
            return Some(slot);
        }
        let mut measured = glyph.glyph;
        let ink = unsafe {
            font.bounding_rects_for_glyphs(
                CTFontOrientation::Default,
                NonNull::from(&mut measured),
                std::ptr::null_mut(),
                1,
            )
        };
        let cells = if ink.origin.x < -1.0
            || ink.origin.x + ink.size.width > (self.cell_px.0 - PAD) as CGFloat
        {
            2
        } else {
            1
        };
        let cell = self.alloc(cells)?;
        // The glyph's own font decides where its baseline sits. Using the
        // monospace ascent for every face put a larger proportional face's
        // ascenders above the top of the cell, where they were cut off: the
        // first line of a heading lost the tops of its letters.
        let ascent = unsafe { font.ascent() } as f32;
        let descent = unsafe { font.descent() } as f32;
        let room = (self.cell_px.1 - PAD) as f32;
        let ascent_px = if ascent + descent > room {
            // Taller than a cell even so. Keep the ascenders, which carry
            // more of a letter's identity than the descenders do.
            room - descent.min(room * 0.2)
        } else {
            ascent.max(self.metrics.ascent * self.metrics.scale)
        };
        self.draw_glyph_into(cell, font, glyph.glyph, ascent_px, cells);
        let slot = Slot {
            uv: self.cell_uv(cell, cells),
            page: (cell / CELLS) as u32,
            cells: cells as u8,
            color: unsafe { font.symbolic_traits() }
                .contains(CTFontSymbolicTraits::ColorGlyphsTrait),
        };
        self.shaped_slots.insert(cache_key, slot);
        Some(slot)
    }

    pub fn cached_shaping(&self, text: &str) -> Option<&ShapedLine> {
        self.shaped_lines.get(text).map(Rc::as_ref)
    }

    // ---- internals -------------------------------------------------------

    /// Reserves consecutive cells without crossing a row or overwriting a
    /// page referenced by this frame. At the memory cap, evict the oldest
    /// unused page, leaving ASCII and all currently emitted quads intact.
    fn alloc(&mut self, cells: usize) -> Option<usize> {
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
                dirty: true,
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
            self.shaped_slots.retain(|_, slot| slot.page != number);
            let page = &mut self.pages[index];
            page.pixels.fill(0);
            page.next_cell = cells;
            page.last_used = self.frame;
            page.dirty = true;
            self.dirty = true;
            return Some((index + 1) * CELLS);
        }
        self.exhausted = true;
        None
    }

    fn cell_origin(&self, cell: usize) -> (usize, usize) {
        let (cw, ch) = self.cell_px;
        let cell = cell % CELLS;
        ((cell % COLS) * cw, (cell / COLS) * ch)
    }

    fn cell_uv(&self, cell: usize, cells: usize) -> [f32; 4] {
        let (cw, ch) = self.cell_px;
        let (x, y) = self.cell_origin(cell);
        [
            x as f32 / self.width as f32,
            y as f32 / self.height as f32,
            (x + cw * cells) as f32 / self.width as f32,
            (y + ch) as f32 / self.height as f32,
        ]
    }

    fn cell_uv_inset(&self, cell: usize, inset: f32) -> [f32; 4] {
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

    fn fill_cell_opaque(&mut self, cell: usize) {
        let (cw, ch) = self.cell_px;
        let (x0, y0) = self.cell_origin(cell);
        let atlas_w = self.width as usize;
        // A clear border of one texel all round. The swatch is sampled well
        // inside it (`solid_uv` is inset), so nothing needs the edge, and an
        // opaque edge is one rounding error away from showing up along the
        // top of whichever glyph has the cell below.
        for y in (y0 + 1)..(y0 + ch - 1).min(self.height as usize) {
            for x in (x0 + 1)..(x0 + cw - 1).min(atlas_w) {
                let o = (y * atlas_w + x) * 4;
                self.pixels[o..o + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }

    /// Finds a font with `ch`, measures it, and draws it into a fresh cell.
    fn rasterize_new(&mut self, ch: char) -> Option<Slot> {
        let mut utf16 = [0u16; 2];
        let encoded: &[u16] = ch.encode_utf16(&mut utf16);

        // Private-use characters mean nothing outside the font that defines
        // them, so they go to ours and nowhere else. Asking the system would
        // get whatever some installed font happens to keep at that number.
        if is_icon(ch) {
            let icons = self.icons.clone()?;
            let glyph = glyph_in(&icons, encoded)?;
            let cells = display_width(ch);
            let cell = self.alloc(cells)?;
            self.draw_icon_into(cell, &icons, glyph, cells);
            return Some(Slot {
                uv: self.cell_uv(cell, cells),
                page: (cell / CELLS) as u32,
                cells: cells as u8,
                color: false,
            });
        }

        // Try the primary font, then let CoreText pick a fallback. This is
        // what makes accents, CJK and emoji work without shipping fonts.
        let (fallback, glyph) = resolve_glyph(&self.font, encoded)?;
        // `None` means the primary font already had it, which saves both a
        // retain dance and a pointless fallback lookup.
        let font = fallback.unwrap_or_else(|| self.font.clone());

        // Cell count comes from the same table the layout uses, never from
        // the measured advance: if those two ever disagreed, a glyph would be
        // drawn at a different width than the column arithmetic assumed.
        let cells = display_width(ch);

        let is_color =
            unsafe { font.symbolic_traits() }.contains(CTFontSymbolicTraits::ColorGlyphsTrait);

        let cell = self.alloc(cells)?;
        let ascent = self.metrics.ascent * self.metrics.scale;
        self.draw_glyph_into(cell, &font, glyph, ascent, cells);

        Some(Slot {
            uv: self.cell_uv(cell, cells),
            page: (cell / CELLS) as u32,
            cells: cells as u8,
            color: is_color,
        })
    }

    /// Rasterizes one glyph into a scratch bitmap and blits it into the cell.
    ///
    /// Scratch rather than drawing straight into `pixels`: a `CGContext` holds
    /// a raw pointer to its backing store, and keeping one alive across any
    /// reallocation of a `Vec` is a dangling pointer waiting to happen.
    /// Draws an icon centred in its cells, shrunk to fit if it has to be.
    ///
    /// Text sits on a baseline. An icon has no baseline worth the name: the
    /// sets in the font were drawn on different grids, and placed by ascent
    /// they sit at five different heights in one file tree. Centring each
    /// one's own ink in the box is what lines them up.
    fn draw_icon_into(&mut self, cell: usize, font: &CTFont, glyph: u16, cells: usize) {
        let (cw, ch) = self.cell_px;
        let (w, h) = ((cw * cells) as CGFloat, ch as CGFloat);
        let ink = |font: &CTFont| -> CGRect {
            let mut g = glyph;
            // SAFETY: one glyph in, the overall rect out, no per-glyph array.
            unsafe {
                font.bounding_rects_for_glyphs(
                    CTFontOrientation::Default,
                    NonNull::from(&mut g),
                    std::ptr::null_mut(),
                    1,
                )
            }
        };

        let mut rect = ink(font);
        let room = (w - PAD as CGFloat - 2.0, h - PAD as CGFloat);
        let fit = (room.0 / rect.size.width).min(room.1 / rect.size.height);
        let smaller = (fit.is_finite() && fit < 1.0).then(|| {
            // SAFETY: same font at a smaller size; null matrix and descriptor.
            unsafe { font.copy_with_attributes(font.size() * fit, std::ptr::null(), None) }
        });
        let font = smaller.as_deref().unwrap_or(font);
        if smaller.is_some() {
            rect = ink(font);
        }
        let origin = CGPoint {
            x: ((w - rect.size.width) / 2.0 - rect.origin.x).round(),
            y: ((h - rect.size.height) / 2.0 - rect.origin.y).round(),
        };
        self.draw_glyph_at(cell, font, glyph, origin, cells);
    }

    fn draw_glyph_into(
        &mut self,
        cell: usize,
        font: &CTFont,
        glyph: u16,
        ascent_px: f32,
        cells: usize,
    ) {
        // CoreGraphics puts the origin at the bottom left.
        let origin = CGPoint {
            x: (PAD / 2) as CGFloat,
            y: (self.cell_px.1 as f32 - (PAD / 2) as f32 - ascent_px) as CGFloat,
        };
        self.draw_glyph_at(cell, font, glyph, origin, cells);
    }

    /// Rasterizes one glyph with its origin at `pos`, in the cell's own
    /// bottom-left coordinates, and copies it into the atlas.
    fn draw_glyph_at(
        &mut self,
        cell: usize,
        font: &CTFont,
        glyph: u16,
        pos: CGPoint,
        cells: usize,
    ) {
        if let Some(scratch) = rasterize_bitmap(self.cell_px, font, glyph, pos, cells) {
            self.copy_bitmap(cell, cells, &scratch);
        }
    }

    fn copy_bitmap(&mut self, cell: usize, cells: usize, scratch: &[u8]) {
        let (cw, h) = self.cell_px;
        let w = cw * cells;
        let (x0, y0) = self.cell_origin(cell);
        let atlas_w = self.width as usize;
        let page_index = cell / CELLS;
        let pixels = if page_index == 0 {
            self.primary_dirty = true;
            &mut self.pixels
        } else {
            self.pages[page_index - 1].dirty = true;
            &mut self.pages[page_index - 1].pixels
        };
        for y in 0..h {
            let ty = y0 + y;
            if ty >= self.height as usize {
                break;
            }
            for x in 0..w {
                let tx = x0 + x;
                if tx >= atlas_w {
                    break;
                }
                let src = (y * w + x) * 4;
                let dst = (ty * atlas_w + tx) * 4;
                pixels[dst..dst + 4].copy_from_slice(&scratch[src..src + 4]);
            }
        }
        self.dirty = true;
    }
}

fn rasterize_bitmap(
    cell_px: (usize, usize),
    font: &CTFont,
    glyph: u16,
    pos: CGPoint,
    cells: usize,
) -> Option<Vec<u8>> {
    let (cw, ch) = cell_px;
    let w = cw * cells;
    let h = ch;
    let mut scratch = vec![0u8; w * h * 4];

    unsafe {
        let space = CGColorSpaceCreateDeviceRGB();
        if space.is_null() {
            return None;
        }
        let ctx = CGBitmapContextCreate(
            scratch.as_mut_ptr() as *mut c_void,
            w,
            h,
            8,
            w * 4,
            space,
            K_CG_IMAGE_ALPHA_PREMULTIPLIED_LAST,
        );
        if ctx.is_null() {
            CGColorSpaceRelease(space);
            return None;
        }

        CGContextSetShouldAntialias(ctx, true);
        // Subpixel font smoothing has been off on Retina since Mojave and
        // cannot survive an alpha-masked atlas anyway. Coverage-only AA is
        // both correct here and what native macOS text looks like today.
        CGContextSetShouldSmoothFonts(ctx, false);
        // White, so a monochrome glyph stores as premultiplied white and
        // can be tinted by the instance colour. A colour font ignores the
        // fill and brings its own.
        CGContextSetRGBFillColor(ctx, 1.0, 1.0, 1.0, 1.0);

        let mut g = glyph;
        if let Some(g) = NonNull::new(&raw mut g) {
            font.draw_glyphs(g, NonNull::from(&pos), 1, &*ctx);
        }

        CGContextRelease(ctx);
        CGColorSpaceRelease(space);
    }

    Some(scratch)
}

/// The glyph `font` has for `text`, if it has one.
fn glyph_in(font: &CTFont, text: &[u16]) -> Option<u16> {
    let mut glyphs = [0u16; 2];
    // SAFETY: both buffers hold `text.len()` items, which is at most two.
    let ok = unsafe {
        font.glyphs_for_characters(
            NonNull::new(text.as_ptr() as *mut u16)?,
            NonNull::new(glyphs.as_mut_ptr())?,
            text.len() as isize,
        )
    };
    (ok && glyphs[0] != 0).then_some(glyphs[0])
}

/// Finds a font containing `text`, returning it with the glyph id.
fn resolve_glyph(primary: &CTFont, text: &[u16]) -> Option<(Option<CFRetained<CTFont>>, u16)> {
    let mut glyphs = [0u16; 2];
    let ok = unsafe {
        primary.glyphs_for_characters(
            NonNull::new(text.as_ptr() as *mut u16)?,
            NonNull::new(glyphs.as_mut_ptr())?,
            text.len() as isize,
        )
    };
    if ok && glyphs[0] != 0 {
        return Some((None, glyphs[0]));
    }

    // CTFontCreateForString is the system's own fallback chain: it is how
    // every Mac app renders a character its chosen font lacks.
    let s = String::from_utf16_lossy(text);
    let cf = CFString::from_str(&s);
    let range = CFRange {
        location: 0,
        length: text.len() as isize,
    };
    let fallback = unsafe { CTFont::for_string(primary, &cf, range) };

    let mut glyphs = [0u16; 2];
    let ok = unsafe {
        fallback.glyphs_for_characters(
            NonNull::new(text.as_ptr() as *mut u16)?,
            NonNull::new(glyphs.as_mut_ptr())?,
            text.len() as isize,
        )
    };
    (ok && glyphs[0] != 0).then_some((Some(fallback), glyphs[0]))
}

/// A font that has been confirmed usable, with its ASCII glyphs resolved.
struct Loaded {
    font: CFRetained<CTFont>,
    /// Glyph ids for [`FIRST_ASCII`]..=[`LAST_ASCII`], in order.
    glyphs: Vec<u16>,
    /// The uniform advance width, in device pixels.
    advance_px: f32,
    /// The candidate name that actually resolved to this font.
    resolved_name: String,
}

/// Finds a genuinely monospace font, in device pixels at `size_px`.
///
/// `CTFont::with_name` never fails: handed a name it cannot resolve it
/// substitutes a default, which on this system is proportional. So the name
/// is only a request, and the returned font has to be *measured* to know what
/// it is. Candidates are tried in order and the first one whose ASCII
/// advances are all equal wins.
///
/// Names here are PostScript names, which is what `with_name` matches on.
/// "SF Mono" is a display name and does not resolve; "SFMono-Regular" does,
/// when Xcode or Terminal has installed it.
fn load_monospace(preferred: &str, size_px: f32) -> Loaded {
    let fallbacks = ["SFMono-Regular", "Menlo-Regular", "Monaco", "Courier"];
    let mut tried = Vec::with_capacity(fallbacks.len() + 1);

    for candidate in std::iter::once(preferred).chain(fallbacks) {
        tried.push(candidate);
        let cf = CFString::from_str(candidate);
        let font = unsafe { CTFont::with_name(&cf, size_px as CGFloat, std::ptr::null()) };
        if let Some(loaded) = measure_if_monospace(font, candidate) {
            return loaded;
        }
    }

    panic!("no monospace font among {tried:?}; all resolved to proportional or incomplete fonts");
}

/// Resolves ASCII glyphs and returns the font only if every advance matches.
fn measure_if_monospace(font: CFRetained<CTFont>, name: &str) -> Option<Loaded> {
    let count = (LAST_ASCII - FIRST_ASCII + 1) as usize;
    let chars: Vec<u16> = (FIRST_ASCII..=LAST_ASCII).map(|c| c as u16).collect();
    let mut glyphs = vec![0u16; count];

    // Returns false if *any* character has no glyph in this font.
    let complete = unsafe {
        font.glyphs_for_characters(
            NonNull::new(chars.as_ptr() as *mut u16).expect("non-empty"),
            NonNull::new(glyphs.as_mut_ptr()).expect("non-empty"),
            count as isize,
        )
    };
    if !complete {
        return None;
    }

    let mut advances = vec![
        CGSize {
            width: 0.0,
            height: 0.0
        };
        count
    ];
    unsafe {
        font.advances_for_glyphs(
            CTFontOrientation::Default,
            NonNull::new(glyphs.as_mut_ptr()).expect("non-empty"),
            advances.as_mut_ptr(),
            count as isize,
        );
    }

    let first = advances[0].width as f32;
    if first <= 0.0 {
        return None;
    }
    // A real monospace font reports identical advances; allow a hair of
    // floating-point slack but nothing that would visibly misalign a column.
    let uniform = advances
        .iter()
        .all(|a| ((a.width as f32) - first).abs() < 0.01);
    if !uniform {
        return None;
    }

    Some(Loaded {
        font,
        glyphs,
        advance_px: first,
        resolved_name: name.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atlas() -> Atlas {
        Atlas::build("SF Mono", 13.0, 2.0)
    }

    /// Regression guard. `CTFont::with_name` substitutes a proportional font
    /// for any name it cannot resolve, and the first version of this module
    /// shipped an atlas built on that substitute without noticing.
    #[test]
    fn primary_carets_cache_exact_native_positions() {
        for scale in [1.0, 1.25, 2.0] {
            let mut atlas = Atlas::build("SF Mono", 13.0, scale);
            for source in [
                "abc שלום xyz",
                "\té\u{301} 👩‍💻 لا",
                "a\u{2067}אב\u{2069}b",
                "é office ffi",
                "क्षि नमस्ते",
                "שלום abc עולם",
                "אב 123 גד",
                "a\u{202e}abc\u{202c}z",
                "🇦🇧🇨🇩",
                "é\t\u{301}x",
            ] {
                let shaped = atlas.shape_line(source).unwrap();
                for index in 0..shaped.offsets.len() {
                    let expected = unsafe {
                        shaped
                            .line
                            .offset_for_string_index(index as isize, std::ptr::null_mut())
                    } as f32
                        / shaped.scale;
                    assert_eq!(shaped.caret_offset(index), expected);
                    assert_eq!(shaped.caret_offset(index), expected);
                }
                assert_eq!(
                    shaped.primary_carets.iter().filter(|x| !x.is_nan()).count(),
                    shaped.offsets.len()
                );
            }
        }
    }

    #[test]
    fn windowed_carets_match_whole_paragraph_native_oracle() {
        for font in ["Menlo", "Times New Roman"] {
            let native_font =
                unsafe { CTFont::with_name(&CFString::from_str(font), 16.25, std::ptr::null()) };
            for fixture in [
                "e\u{301} 👩‍💻 لا שלום 漢字\t",
                "אב e\u{301} 👩‍💻 גד ",
                "क्षि नमस्ते",
                "e\u{301} 👩‍💻 لا क्षि",
                "x\u{2067}שלום abc\u{2069}y ",
                "a\u{202e}abc 123\u{202c}z ",
                "عربي 123 abc عربي ",
                "🇦🇧🇨🇩 office ffi fi fl ",
                "ไทย ภาษาไทย தமிழ் 한국어 ",
                "é漢\t🌍x\t",
                "אב 🇦",
                "\u{061c}abc \u{200e}123 \u{200f}office ffi fi fl ",
                "\u{202a}abc\u{202b}123\u{202c}\u{202d}ffi\u{202e}xyz\u{202c} ",
                "\u{2066}abc \u{2067}שלום \u{2068}ffi\u{2069}\u{2069}\u{2069} ",
                "\u{2067}\0abc e\u{301} 👩‍💻\u{2069}\0xyz ",
            ] {
                let source = fixture.repeat(43);
                let shaped = shape(&native_font, &source, 1.25, &AtomicBool::new(false)).unwrap();
                let (text, _) = shape_input(&source);
                let (left, right) =
                    caret_offsets(&shaped.line, &text, 1.25, &AtomicBool::new(false)).unwrap();
                for index in 0..shaped.offsets.len() {
                    let primary = unsafe {
                        shaped
                            .line
                            .offset_for_string_index(index as isize, std::ptr::null_mut())
                    } as f32
                        / 1.25;
                    for (actual, expected, kind) in [
                        (shaped.offsets[index], left[index], "left"),
                        (shaped.secondary_offsets[index], right[index], "right"),
                        (shaped.caret_offset(index), primary, "primary"),
                    ] {
                        assert!(
                            (actual - expected).abs() < 0.001,
                            "{font} {fixture:?} index {index} {kind}: {actual} != {expected}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn long_paragraphs_keep_native_clusters_and_direction() {
        let atlas = atlas();
        let fixture = "e\u{301} 👩‍💻 لا שלום 漢字\t";
        let short = shape(&atlas.font, fixture, 2.0, &AtomicBool::new(false)).unwrap();
        let long = shape(
            &atlas.font,
            &fixture.repeat(2400),
            2.0,
            &AtomicBool::new(false),
        )
        .unwrap();
        let prefix_units = short.source_bytes.len() - 1;
        let prefix = |line: &ShapedLine| {
            line.glyphs
                .iter()
                .filter(|glyph| glyph.source_utf16 < prefix_units)
                .map(|glyph| (glyph.source_utf16, glyph.glyph, glyph.x))
                .collect::<Vec<_>>()
        };
        assert_eq!(prefix(&long), prefix(&short));
        let runs = unsafe { long.line.glyph_runs() };
        assert!((0..runs.count()).any(|index| {
            let run = unsafe { &*(runs.value_at_index(index) as *const CTRun) };
            unsafe { run.status() }.contains(objc2_core_text::CTRunStatus::RightToLeft)
        }));
        assert!(
            long.fonts.len() < 32,
            "repeated runs must share retained fonts"
        );
    }

    #[test]
    fn windowed_carets_match_adversarial_paragraphs() {
        let atlas = atlas();
        let atoms = [
            "abc ",
            "אב ",
            "123 ",
            "لا",
            "e\u{301}",
            "👩‍💻",
            "क्षि",
            "\u{2067}",
            "\u{2069}",
            "\u{202e}",
            "\u{202c}",
            "\t",
            "🇦",
            "\u{200f}",
            "\0",
        ];
        let mut seed = 37_u64;
        for case in 0..256 {
            let mut source = String::new();
            for _ in 0..160 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                let atom = (seed >> 32) as usize % atoms.len();
                source.push_str(
                    if (16..128).contains(&case) && ((7..=10).contains(&atom) || atom == 13) {
                        "אב 123 "
                    } else {
                        atoms[atom]
                    },
                );
            }
            let shaped = shape(&atlas.font, &source, 2.0, &AtomicBool::new(false)).unwrap();
            let (text, _) = shape_input(&source);
            let (left, right) =
                caret_offsets(&shaped.line, &text, 2.0, &AtomicBool::new(false)).unwrap();
            for index in 0..left.len() {
                let expected = unsafe {
                    shaped
                        .line
                        .offset_for_string_index(index as isize, std::ptr::null_mut())
                } as f32
                    / 2.0;
                for (actual, expected, kind) in [
                    (shaped.offsets[index], left[index], "left"),
                    (shaped.secondary_offsets[index], right[index], "right"),
                    (shaped.caret_offset(index), expected, "primary"),
                ] {
                    assert!(
                        (actual - expected).abs() < 0.001,
                        "case {case} index {index} unit {:?} {kind}: {actual} != {expected}; {source:?}",
                        text.encode_utf16().nth(index)
                    );
                }
            }
        }
    }

    #[test]
    fn expanded_shaping_input_is_bounded_and_cancellable() {
        let cancel = AtomicBool::new(false);
        assert!(shape_input_bounded("\t\té", 9, &cancel).is_some());
        assert!(shape_input_bounded("\t\té", 8, &cancel).is_none());
        assert!(shape_input_bounded("🌍", 1, &cancel).is_none());
        cancel.store(true, Ordering::Relaxed);
        assert!(shape_input_bounded("é", 8, &cancel).is_none());
    }

    #[test]
    fn fallback_worker_bitmap_matches_synchronous_rasterization() {
        let mut synchronous = atlas();
        let mut asynchronous = atlas();
        for ch in ['é', '漢', '🌍'] {
            let expected = synchronous.slot_for(ch).unwrap();
            assert!(asynchronous.slot_for_fallback(ch).is_some());
            assert!(asynchronous.peek(ch).is_none());
            let started = std::time::Instant::now();
            while asynchronous.has_pending_shaping() {
                assert!(started.elapsed().as_secs() < 10);
                std::thread::sleep(std::time::Duration::from_millis(2));
                asynchronous.begin_frame();
            }
            let actual = asynchronous.peek(ch).unwrap();
            assert_eq!(actual.cells, expected.cells);
            assert_eq!(actual.color, expected.color);
            // Identical insertion order makes the whole page an exact bitmap oracle.
            assert_eq!(asynchronous.pixels, synchronous.pixels);
        }
    }

    #[test]
    fn rejected_expansion_is_cached_without_restarting_the_worker() {
        let mut atlas = atlas();
        let source = format!("{}é", "\t".repeat(1_100_000));
        let rope = Rope::from_text(&source);
        let started = std::time::Instant::now();
        loop {
            atlas.begin_frame();
            atlas.shape_editor_line((1, 0), &rope, 0..rope.len_bytes());
            atlas.finish_shaping_frame();
            if !atlas.has_pending_shaping() {
                break;
            }
            assert!(started.elapsed().as_secs() < 10);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(atlas.editor_lines.get(&0).is_some_and(Option::is_none));
        assert_eq!(atlas.cached_utf16, 0);
        atlas.shape_editor_line((1, 0), &rope, 0..rope.len_bytes());
        assert!(!atlas.has_pending_shaping());
    }

    #[test]
    fn first_carets_are_prepared_without_native_queries_in_ascii_spans() {
        let mut atlas = atlas();
        let source = format!("{}é", "a".repeat(100_000));
        let shaped = atlas.shape_line(&source).unwrap();
        // Inspect before any caret_offset call: a warm-cache test would miss this.
        assert!(shaped.primary_carets.iter().all(|slot| slot.is_finite()));
        for index in [0, 1, 50_000, 99_999, 100_000, 100_001] {
            let expected = unsafe {
                shaped
                    .line
                    .offset_for_string_index(index as isize, std::ptr::null_mut())
            } as f32
                / shaped.scale;
            assert_eq!(shaped.caret_offset(index), expected);
        }
    }

    #[test]
    fn prepared_hit_stops_match_native_grapheme_scan() {
        let mut atlas = atlas();
        for source in [
            "abc\r\ndef é",
            "abc\t\u{301}x",
            "é abc 👩‍💻 def",
            "אב 123 xyz",
            "a\0b é",
        ] {
            let shaped = atlas.shape_line(source).unwrap();
            let native = objc2_foundation::NSString::from_str(source);
            let mut utf16 = 0;
            let mut expected = Vec::new();
            for (byte, ch) in source.char_indices() {
                if native
                    .rangeOfComposedCharacterSequenceAtIndex(utf16)
                    .location
                    == utf16
                {
                    let index = shaped.source_bytes.partition_point(|&b| b < byte);
                    expected.push((shaped.offsets[index], byte));
                    expected.push((shaped.secondary_offsets[index], byte));
                }
                utf16 += ch.len_utf16();
            }
            expected.push((*shaped.offsets.last().unwrap(), source.len()));
            expected.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            expected.dedup_by(|a, b| a.0 == b.0);
            assert_eq!(shaped.hit_stops, expected, "{source:?}");
        }
    }

    #[test]
    fn editor_and_standalone_geometry_share_cache_bounds() {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let rope = Rope::from_text(&"ascii\né\t漢\n".repeat(300));
        for line in 0..600 {
            atlas.shape_editor_line(
                (1, line),
                &rope,
                rope.line_to_byte(line)..rope.line_to_byte(line + 1),
            );
            assert!(atlas.editor_lines.len() + atlas.shaped_lines.len() <= 256);
            assert!(atlas.cached_utf16 <= MAX_CACHED_UTF16);
            let actual: usize = atlas
                .editor_lines
                .values()
                .flatten()
                .chain(atlas.shaped_lines.values())
                .map(|s| s.offsets.len())
                .sum();
            assert_eq!(atlas.cached_utf16, actual);
        }
        atlas.shape_line("é separate").unwrap();
        let edited = Rope::from_text("é new snapshot");
        atlas.shape_editor_line((1, 0), &edited, 0..edited.len_bytes());
        assert_eq!(atlas.editor_lines.len(), 1);
        assert_eq!(
            atlas.cached_utf16,
            atlas
                .editor_lines
                .values()
                .flatten()
                .chain(atlas.shaped_lines.values())
                .map(|s| s.offsets.len())
                .sum::<usize>()
        );
    }

    #[test]
    fn unchanged_geometry_moves_across_edits_undo_and_keeps_cache_accounting() {
        let mut atlas = atlas();
        let original = Rope::from_text("é\r\nאב\t👩‍💻\n漢\n");
        let get = |atlas: &mut Atlas, rope: &Rope, line| {
            atlas
                .shape_editor_line(
                    (1, line),
                    rope,
                    rope.line_to_byte(line)..rope.line_to_byte(line + 1),
                )
                .unwrap()
        };
        let first = get(&mut atlas, &original, 0);
        let middle = get(&mut atlas, &original, 1);
        let last = get(&mut atlas, &original, 2);
        let mut edited = original.clone();
        edited.insert(original.line_to_byte(1), "new\n");
        assert!(Rc::ptr_eq(&middle, &get(&mut atlas, &edited, 2)));
        assert!(Rc::ptr_eq(&first, &get(&mut atlas, &edited, 0)));
        assert!(Rc::ptr_eq(&last, &get(&mut atlas, &edited, 3)));
        assert!(Rc::ptr_eq(&middle, &get(&mut atlas, &original, 1)));

        edited = original.clone();
        edited.insert(original.line_to_byte(1), "x");
        assert!(!Rc::ptr_eq(&middle, &get(&mut atlas, &edited, 1)));
        assert!(Rc::ptr_eq(&last, &get(&mut atlas, &edited, 2)));
        assert!(Rc::ptr_eq(&first, &get(&mut atlas, &edited, 0)));
        let actual: usize = atlas
            .editor_lines
            .values()
            .flatten()
            .chain(atlas.shaped_lines.values())
            .map(|s| s.offsets.len())
            .sum();
        assert_eq!(atlas.cached_utf16, actual);
        assert!(!atlas.has_pending_shaping());

        let other = atlas
            .shape_editor_line((2, 0), &original, 0..original.line_to_byte(1))
            .unwrap();
        assert!(
            !Rc::ptr_eq(&first, &other),
            "document changes invalidate live geometry"
        );
    }

    #[test]
    fn duplicate_paragraph_remaps_count_geometry_only_once() {
        let mut atlas = atlas();
        let mut rope = Rope::from_text("é\né\n");
        for line in 0..2 {
            atlas
                .shape_editor_line(
                    (1, line),
                    &rope,
                    rope.line_to_byte(line)..rope.line_to_byte(line + 1),
                )
                .unwrap();
        }
        rope.delete(0..3);
        let kept = atlas.shape_editor_line((1, 0), &rope, 0..3).unwrap();
        assert_eq!(atlas.editor_lines.len(), 1);
        assert_eq!(atlas.cached_utf16, kept.offsets.len());
    }

    #[test]
    fn indexed_visible_glyphs_match_full_scan_in_mixed_direction_text() {
        let mut atlas = Atlas::build("Menlo", 13.0, 2.0);
        let line = atlas
            .shape_line(&"e\u{301} 👩‍💻 لا שלום 漢字 ".repeat(32))
            .unwrap();
        for left in [-100.0, 0.0, 50.0, 300.0, 900.0, 100_000.0] {
            let right = left + 200.0;
            let overhang = atlas.cell_size().0 * 2.0;
            let expected: Vec<_> = line
                .glyphs
                .iter()
                .filter(|g| g.x + overhang > left && g.x <= right)
                .map(|g| (g.x, g.source_utf16, g.glyph))
                .collect();
            let actual: Vec<_> = line
                .visible_glyphs(left, right, overhang)
                .iter()
                .map(|g| (g.x, g.source_utf16, g.glyph))
                .collect();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn resolved_font_is_genuinely_monospace() {
        let a = atlas();
        let m = a.metrics;
        assert!(
            m.advance < m.line_height * 0.8,
            "{} looks proportional: advance {:.2} vs line height {:.2}",
            a.font_name,
            m.advance,
            m.line_height
        );
        assert!(
            (0.3..0.75).contains(&(m.advance / m.line_height)),
            "{} has an implausible aspect ratio",
            a.font_name
        );
    }

    #[test]
    fn ascii_is_resident_up_front() {
        let a = atlas();
        for c in FIRST_ASCII..=LAST_ASCII {
            let ch = char::from_u32(c).expect("ascii");
            assert!(a.peek(ch).is_some(), "{ch:?} should be preloaded");
        }
    }

    #[test]
    fn enumerated_caret_edges_match_coretext_at_grapheme_boundaries() {
        for text in [
            "",
            "e\u{301}x",
            "👩‍💻x",
            "لا",
            "office ffi",
            "abc שלום xyz",
            "שלום abc עולם",
            "אב e\u{301} גד",
            "אב 👩‍💻 גד",
            "a\u{200f}b",
            "x\u{2067}שלום abc\u{2069}y",
            "क्षि नमस्ते",
            "عربي 123 abc عربي",
            "abc عربي 123 xyz",
            "אב 123 גד",
        ] {
            let string = CFString::from_str(text);
            let attributed = unsafe { CFAttributedString::new(None, Some(&string), None).unwrap() };
            let line = unsafe { CTLine::with_attributed_string(&attributed) };
            let (primary, secondary) =
                caret_offsets(&line, text, 2.0, &AtomicBool::new(false)).unwrap();
            let source = objc2_foundation::NSString::from_str(text);
            for index in 0..primary.len() {
                // Interior UTF-16 units of a grapheme are never caret stops.
                if index + 1 < primary.len()
                    && source
                        .rangeOfComposedCharacterSequenceAtIndex(index)
                        .location
                        != index
                {
                    continue;
                }
                let mut other = 0.0;
                let expected = unsafe { line.offset_for_string_index(index as isize, &mut other) };
                let low = expected.min(other) as f32 / 2.0;
                let high = expected.max(other) as f32 / 2.0;
                assert!(
                    (primary[index] - low).abs() < 0.01,
                    "{text:?}, index {index}: {} != {}",
                    primary[index],
                    expected / 2.0
                );
                assert!(
                    (secondary[index] - high).abs() < 0.01,
                    "{text:?}, secondary index {index}: {} != {}",
                    secondary[index],
                    other / 2.0
                );
            }
        }
    }

    #[test]
    fn coretext_shapes_combining_marks_and_rtl_runs() {
        let mut a = atlas();
        for text in ["e\u{301}", "لا", "office fi", "👩‍💻", "abc שלום xyz"] {
            let shaped = a.shape_line(text).expect("atlas has room");
            assert!(!shaped.glyphs.is_empty(), "{text:?}");
            assert!(shaped.glyphs.iter().all(|g| g.x.is_finite()));
            assert!(
                shaped
                    .glyphs
                    .iter()
                    .all(|g| g.source_utf16 < text.encode_utf16().count())
            );
            assert_eq!(shaped.offsets.len(), text.encode_utf16().count() + 1);
        }
        assert_eq!(a.shape_line("e\u{301}").unwrap().glyphs.len(), 1);
        assert_eq!(a.shape_line("لا").unwrap().glyphs.len(), 1);
    }

    #[test]
    fn atlas_pages_preserve_current_frame_and_evict_oldest_unused_page() {
        let mut a = atlas();
        a.max_pages = 3;
        let ascii = a.peek('A').unwrap();
        a.next_cell = CELLS;
        let first = a.slot_for('漢').unwrap();
        assert_eq!(first.page, 1);
        a.pages[0].next_cell = CELLS;
        let second = a.slot_for('字').unwrap();
        assert_eq!(second.page, 2);
        a.pages[1].next_cell = CELLS;
        assert!(a.slot_for('語').is_none(), "never evict this frame's quads");
        assert_eq!(a.peek('漢').unwrap().uv, first.uv);
        a.begin_frame();
        a.slot_for('字').unwrap(); // Protect page 2, leaving page 1 as victim.
        let next = a.slot_for('語').unwrap();
        assert_eq!(next.page, 1);
        assert!(a.peek('漢').is_none());
        assert_eq!(a.peek('字').unwrap().uv, second.uv);
        assert_eq!(a.peek('A').unwrap().uv, ascii.uv);
        assert_eq!(a.page_count(), 3);
    }

    #[test]
    fn shaped_geometry_survives_atlas_eviction() {
        let mut a = atlas();
        a.max_pages = 2;
        a.next_cell = CELLS;
        let shaped = a.shape_line("é").unwrap();
        a.slot_for_shaped(&shaped, &shaped.glyphs[0]).unwrap();
        a.pages[0].next_cell = CELLS;
        a.begin_frame();
        a.slot_for('漢').unwrap();
        let cached = a.shape_line("é").unwrap();
        assert!(Rc::ptr_eq(&shaped, &cached));
        assert!(a.slot_for_shaped(&cached, &cached.glyphs[0]).is_some());
    }

    #[test]
    fn ascii_cells_do_not_overlap() {
        let a = atlas();
        let mut seen: Vec<[u32; 4]> = (FIRST_ASCII..=LAST_ASCII)
            .map(|c| {
                let uv = a
                    .peek(char::from_u32(c).expect("ascii"))
                    .expect("resident")
                    .uv;
                uv.map(|v| v.to_bits())
            })
            .collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), before, "atlas cells overlap");
    }

    #[test]
    fn non_ascii_is_rasterized_on_demand() {
        let mut a = atlas();
        assert!(a.peek('é').is_none(), "should not be preloaded");
        let slot = a.slot_for('é').expect("é should resolve in some font");
        assert_eq!(slot.cells, 1, "an accented latin letter is single width");
        assert!(!slot.color);
        assert!(a.peek('é').is_some(), "should now be resident");
    }

    #[test]
    fn falls_back_across_fonts_for_other_scripts() {
        let mut a = atlas();
        for ch in ['λ', 'Ж', '漢'] {
            assert!(
                a.slot_for(ch).is_some(),
                "{ch:?} should resolve through CoreText fallback"
            );
        }
    }

    #[test]
    fn wide_characters_take_two_cells() {
        let mut a = atlas();
        let slot = a.slot_for('漢').expect("CJK should resolve");
        assert_eq!(slot.cells, 2, "CJK is double width");
    }

    #[test]
    fn emoji_are_marked_as_colour() {
        let mut a = atlas();
        let slot = a.slot_for('🌍').expect("emoji should resolve");
        assert!(
            slot.color,
            "emoji carry their own colour and must not be tinted"
        );
    }

    #[test]
    fn lookups_are_cached_and_mark_the_atlas_dirty() {
        let mut a = atlas();
        a.dirty = false;
        let before = a.resident();
        a.slot_for('ß').expect("resolves");
        assert!(a.dirty, "a new glyph must trigger a texture upload");
        assert_eq!(a.resident(), before + 1);

        a.dirty = false;
        a.slot_for('ß').expect("cached");
        assert!(!a.dirty, "a cached lookup must not re-upload");
        assert_eq!(a.resident(), before + 1);
    }

    #[test]
    fn glyphs_actually_rasterized_ink() {
        let a = atlas();
        let ink = |ch: char| -> u64 {
            let uv = a.peek(ch).expect("ascii").uv;
            let (w, h) = (a.width as f32, a.height as f32);
            let (x0, y0) = ((uv[0] * w) as u32, (uv[1] * h) as u32);
            let (x1, y1) = ((uv[2] * w) as u32, (uv[3] * h) as u32);
            let mut sum = 0u64;
            for y in y0..y1 {
                for x in x0..x1 {
                    sum += a.pixels[((y * a.width + x) * 4 + 3) as usize] as u64;
                }
            }
            sum
        };
        assert_eq!(ink(' '), 0, "space should be blank");
        assert!(ink('M') > 0, "CoreText rasterized nothing for 'M'");
        assert!(ink('W') > ink('.'), "'W' should carry more ink than '.'");
    }

    #[test]
    fn the_solid_swatch_is_opaque() {
        let a = atlas();
        let uv = a.solid_uv();
        let x = ((uv[0] + uv[2]) * 0.5 * a.width as f32) as u32;
        let y = ((uv[1] + uv[3]) * 0.5 * a.height as f32) as u32;
        let o = ((y * a.width + x) * 4) as usize;
        assert_eq!(&a.pixels[o..o + 4], &[255, 255, 255, 255]);
    }
}
