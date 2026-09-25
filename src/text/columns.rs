//! Shared cell widths for the character fallback and rope summaries.
//! CoreText shaping remains responsible for graphemes and bidi text.

pub const TAB_WIDTH: usize = 4;

pub fn advance(column: usize, ch: char) -> usize {
    match ch {
        '\n' | '\r' => column,
        '\t' => column + TAB_WIDTH - column % TAB_WIDTH,
        _ => column + display_width(ch),
    }
}

pub fn is_icon(ch: char) -> bool {
    matches!(ch as u32, 0xE000..=0xF8FF | 0xF_0000..=0xF_FFFD)
}

/// Fallback cell width; intentionally preserves the character renderer policy.
pub fn display_width(ch: char) -> usize {
    let c = ch as u32;
    let wide = matches!(c,
        0x1100..=0x115F        // Hangul Jamo
        | 0x2E80..=0x303E      // CJK radicals, Kangxi, CJK symbols
        | 0x3041..=0x33FF      // Hiragana, Katakana, Hangul, CJK compatibility
        | 0x3400..=0x4DBF      // CJK Unified Extension A
        | 0x4E00..=0x9FFF      // CJK Unified
        | 0xA000..=0xA4CF      // Yi
        | 0xAC00..=0xD7A3      // Hangul syllables
        | 0xF900..=0xFAFF      // CJK compatibility ideographs
        | 0xFE30..=0xFE6F      // CJK compatibility forms
        | 0xFF00..=0xFF60      // Fullwidth forms
        | 0xFFE0..=0xFFE6
        // Emoji that are wide by default. Not the whole of the symbol
        // blocks they sit in: a check mark or a star is a text character one
        // cell wide, and only the ones Unicode gives emoji presentation are
        // drawn at two. Left out, the green tick was drawn into one cell and
        // cut in half.
        | 0x231A..=0x231B | 0x23E9..=0x23EC | 0x23F0 | 0x23F3
        | 0x25FD..=0x25FE | 0x2614..=0x2615 | 0x2648..=0x2653
        | 0x267F | 0x2693 | 0x26A1 | 0x26AA..=0x26AB | 0x26BD..=0x26BE
        | 0x26C4..=0x26C5 | 0x26CE | 0x26D4 | 0x26EA | 0x26F2..=0x26F3
        | 0x26F5 | 0x26FA | 0x26FD | 0x2705 | 0x270A..=0x270B | 0x2728
        | 0x274C | 0x274E | 0x2753..=0x2755 | 0x2757 | 0x2795..=0x2797
        | 0x27B0 | 0x27BF | 0x2B1B..=0x2B1C | 0x2B50 | 0x2B55
        | 0x1F004 | 0x1F0CF | 0x1F18E | 0x1F191..=0x1F19A
        | 0x1F1E6..=0x1F1FF    // regional indicators: flags
        | 0x1F200..=0x1F2FF    // enclosed ideographic supplement
        | 0x1F300..=0x1F64F    // emoji: symbols, pictographs, emoticons
        | 0x1F680..=0x1F6FF    // transport and map
        | 0x1F7E0..=0x1F7EB    // coloured circles and squares
        | 0x1F900..=0x1F9FF    // supplemental symbols
        | 0x1FA70..=0x1FAFF    // symbols and pictographs extended-A
        | 0x20000..=0x3FFFD    // CJK Unified Extensions B and beyond
    ) || is_icon(ch);
    if wide { 2 } else { 1 }
}
