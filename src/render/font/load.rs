//! Finding and loading the fonts: the code font, checked monospace, and
//! the icon font.

use super::*;
use objc2_core_text::CTFontUIFontType;

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

    // None by name. The system's own fixed-pitch font is what Terminal and
    // Xcode fall back to, whatever is installed.
    let system = unsafe {
        CTFont::new_ui_font_for_language(CTFontUIFontType::UserFixedPitch, size_px as CGFloat, None)
    };
    if let Some(loaded) = system
        .clone()
        .and_then(|font| measure_if_monospace(font, SYSTEM_FIXED_PITCH))
    {
        return loaded;
    }

    // Still nothing. This runs on the shaping and glyph worker threads, so
    // a panic here would end them and leave every window blank for good.
    // Columns may misalign with a proportional face; text still shows.
    let (font, name) = match system {
        Some(font) => (font, SYSTEM_FIXED_PITCH),
        None => {
            let cf = CFString::from_str(LAST_RESORT);
            let font = unsafe { CTFont::with_name(&cf, size_px as CGFloat, std::ptr::null()) };
            (font, LAST_RESORT)
        }
    };
    if !WARNED_NO_MONOSPACE.swap(true, Ordering::Relaxed) {
        eprintln!(
            "crc: no monospace font among {tried:?}; drawing with {name} as it is, columns may misalign"
        );
    }
    let mut loaded = measure(font, name).loaded;
    if loaded.advance_px <= 0.0 {
        // No advance at all; a plausible cell keeps the grid finite.
        loaded.advance_px = (size_px * 0.6).max(1.0);
    }
    loaded
}
/// The name recorded for the system fixed-pitch font, which has no
/// PostScript name of its own to ask for.
const SYSTEM_FIXED_PITCH: &str = "system fixed pitch";
/// Drawn with when nothing else resolves at all; `with_name` always returns
/// some font for it.
const LAST_RESORT: &str = "Menlo-Regular";
/// Set once the first worker has said that no monospace font was found, so
/// the other workers do not say it again.
static WARNED_NO_MONOSPACE: AtomicBool = AtomicBool::new(false);
/// Resolves ASCII glyphs and returns the font only if every advance matches.
pub(super) fn measure_if_monospace(font: CFRetained<CTFont>, name: &str) -> Option<Loaded> {
    let measured = measure(font, name);
    (measured.complete && measured.uniform).then_some(measured.loaded)
}
/// A font measured over ASCII, with what the measurement found.
struct Measured {
    loaded: Loaded,
    /// Every character has a glyph.
    complete: bool,
    /// Every advance matches the first, which is positive.
    uniform: bool,
}
/// Resolves ASCII glyphs and measures their advances, judging nothing: the
/// caller decides whether the font passes. `advance_px` is the first
/// advance, or the widest when the first is empty, so even a font that
/// fails has a cell width to draw with.
fn measure(font: CFRetained<CTFont>, name: &str) -> Measured {
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
    // A real monospace font reports identical advances; allow a hair of
    // floating-point slack but nothing that would visibly misalign a column.
    let uniform = first > 0.0
        && advances
            .iter()
            .all(|a| ((a.width as f32) - first).abs() < 0.01);
    let advance_px = if first > 0.0 {
        first
    } else {
        advances.iter().map(|a| a.width as f32).fold(0.0, f32::max)
    };

    Measured {
        loaded: Loaded {
            font,
            glyphs,
            advance_px,
            resolved_name: name.to_string(),
        },
        complete,
        uniform,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The last resort draws with a font that failed the monospace check,
    /// so measuring one must still give usable glyphs and a cell width.
    #[test]
    fn a_proportional_font_still_measures_for_the_last_resort() {
        let cf = CFString::from_str("Helvetica");
        let font = unsafe { CTFont::with_name(&cf, 13.0, std::ptr::null()) };
        assert!(measure_if_monospace(font.clone(), "Helvetica").is_none());
        let measured = measure(font, "Helvetica");
        assert!(!measured.uniform);
        assert!(measured.loaded.advance_px > 0.0);
        assert_eq!(measured.loaded.resolved_name, "Helvetica");
    }
}
