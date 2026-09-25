//! Search semantics shared by the find bar and project search.
//! Foundation supplies ICU regular expressions; offsets are translated back
//! to UTF-8 byte ranges before they touch the rope.

use std::ops::Range;

use objc2_foundation::{
    NSMatchingOptions, NSRange, NSRegularExpression, NSRegularExpressionOptions, NSString,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub range: Range<usize>,
    pub replacement: String,
}

fn utf16_to_byte(text: &str, utf16: usize) -> Option<usize> {
    if utf16 == 0 {
        return Some(0);
    }
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        units += ch.len_utf16();
        if units == utf16 {
            return Some(byte + ch.len_utf8());
        }
        if units > utf16 {
            return None;
        }
    }
    (units == utf16).then_some(text.len())
}

/// Returns non-overlapping matches and their expanded replacement text.
pub fn find(
    text: &str,
    query: &str,
    template: &str,
    options: Options,
) -> Result<Vec<Match>, String> {
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let pattern = if options.whole_word {
        if options.regex {
            format!(r"\b(?:{query})\b")
        } else {
            let escaped = NSRegularExpression::escapedPatternForString(&NSString::from_str(query));
            format!(r"\b(?:{escaped})\b")
        }
    } else {
        query.to_string()
    };
    let mut flags = if options.regex || options.whole_word {
        NSRegularExpressionOptions::empty()
    } else {
        NSRegularExpressionOptions::IgnoreMetacharacters
    };
    if !options.case_sensitive {
        flags |= NSRegularExpressionOptions::CaseInsensitive;
    }
    let regex = NSRegularExpression::regularExpressionWithPattern_options_error(
        &NSString::from_str(&pattern),
        flags,
    )
    .map_err(|e| e.localizedDescription().to_string())?;
    let ns_text = NSString::from_str(text);
    let ns_template = NSString::from_str(template);
    let range = NSRange::new(0, text.encode_utf16().count());
    let matches = regex.matchesInString_options_range(&ns_text, NSMatchingOptions::empty(), range);
    let mut out = Vec::with_capacity(matches.count());
    for found in matches.iter() {
        let r = found.range();
        let Some(start) = utf16_to_byte(text, r.location) else {
            continue;
        };
        let Some(end) = utf16_to_byte(text, r.location + r.length) else {
            continue;
        };
        if start == end {
            continue;
        }
        let replacement = if options.regex {
            regex
                .replacementStringForResult_inString_offset_template(
                    &found,
                    &ns_text,
                    0,
                    &ns_template,
                )
                .to_string()
        } else {
            template.to_string()
        };
        out.push(Match {
            range: start..end,
            replacement,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_ranges_case_and_word_edges() {
        let options = Options {
            whole_word: true,
            ..Options::default()
        };
        let found = find("café caféine CAFÉ", "café", "X", options).unwrap();
        assert_eq!(
            found.iter().map(|m| m.range.clone()).collect::<Vec<_>>(),
            vec![0..5, 15..20]
        );
    }

    #[test]
    fn regex_captures_expand_and_bad_pattern_is_reported() {
        let options = Options {
            regex: true,
            ..Options::default()
        };
        let found = find("ab12 cd34", r"([a-z]+)(\d+)", "$2-$1", options).unwrap();
        assert_eq!(found[0].replacement, "12-ab");
        assert_eq!(found[1].range, 5..9);
        assert!(find("x", "(", "", options).is_err());
    }
}
