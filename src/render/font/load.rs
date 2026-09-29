//! Finding and loading the fonts: the code font, checked monospace, and
//! the icon font.

use super::*;

pub(super) fn load_icon_font(size_px: f32) -> Option<CFRetained<CTFont>> {
    let data = CFData::from_static_bytes(ICON_FONT);
    let provider = CGDataProvider::with_cf_data(Some(&data))?;
    let graphics_font = CGFont::with_data_provider(&provider)?;
    // SAFETY: a live CGFont, a null matrix meaning identity, no descriptor.
    Some(unsafe {
        CTFont::with_graphics_font(&graphics_font, size_px as CGFloat, std::ptr::null(), None)
    })
}
/// A font that has been confirmed usable, with its ASCII glyphs resolved.
pub(super) struct Loaded {
    pub(super) font: CFRetained<CTFont>,
    /// Glyph ids for [`FIRST_ASCII`]..=[`LAST_ASCII`], in order.
    pub(super) glyphs: Vec<u16>,
    /// The uniform advance width, in device pixels.
    pub(super) advance_px: f32,
    /// The candidate name that actually resolved to this font.
    pub(super) resolved_name: String,
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
pub(super) fn load_monospace(preferred: &str, size_px: f32) -> Loaded {
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
pub(super) fn measure_if_monospace(font: CFRetained<CTFont>, name: &str) -> Option<Loaded> {
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
