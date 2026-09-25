//! Native grapheme segmentation with a rope-backed NSString, without line copies.
use super::rope::Rope;
use std::ffi::c_void;

unsafe extern "C" {
    fn crc_rope_grapheme(
        context: *const c_void,
        count: usize,
        index: usize,
        read: extern "C" fn(*const c_void, usize, usize, *mut u16),
        start: *mut usize,
        end: *mut usize,
    );
}

struct Context<'a> {
    rope: &'a Rope,
    #[cfg(test)]
    read_units: std::cell::Cell<usize>,
}

extern "C" fn read(context: *const c_void, start: usize, count: usize, out: *mut u16) {
    // Synchronous bridge owns the writable count-unit output buffer and borrows
    // this context only until crc_rope_grapheme returns.
    let context = unsafe { &*context.cast::<Context<'_>>() };
    if count == 0 {
        return;
    }
    let out = unsafe { std::slice::from_raw_parts_mut(out, count) };
    context.rope.read_utf16(start, out);
    #[cfg(test)]
    context.read_units.set(context.read_units.get() + count);
}

fn query(context: &Context<'_>, index: usize) -> std::ops::Range<usize> {
    assert!(index < context.rope.len_utf16());
    let (mut start, mut end) = (0, 0);
    unsafe {
        crc_rope_grapheme(
            (context as *const Context<'_>).cast(),
            context.rope.len_utf16(),
            index,
            read,
            &mut start,
            &mut end,
        );
    }
    start..end
}

pub(super) fn range(rope: &Rope, index: usize) -> std::ops::Range<usize> {
    query(
        &Context {
            rope,
            #[cfg(test)]
            read_units: std::cell::Cell::new(0),
        },
        index,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rope_queries_match_native_strings_across_leaves_and_snapshots() {
        use objc2_foundation::NSString;
        let source = format!(
            "{}\r\n{}{}{}{}",
            "é漢👩‍💻e\u{301} ".repeat(80),
            "🇪🇸🇦".repeat(90),
            "\u{600}a\tक्‍ष".repeat(70),
            "a".to_owned() + &"\u{301}".repeat(1200),
            "\r\n끝"
        );
        let mut rope = Rope::from_text(&source);
        let snapshot = rope.clone();
        rope.insert(rope.char_to_byte(400), "edited👨‍👩‍👧‍👦");
        for rope in [&snapshot, &rope] {
            let text = rope.to_string();
            let native = NSString::from_str(&text);
            let units: Vec<_> = text.encode_utf16().collect();
            assert_eq!(rope.len_utf16(), units.len());
            let mut unit = 0;
            for (byte, ch) in text.char_indices() {
                assert_eq!(rope.byte_to_utf16(byte), unit);
                assert_eq!(rope.utf16_to_byte(unit), byte);
                if ch.len_utf16() == 2 {
                    assert_eq!(rope.utf16_to_byte(unit + 1), byte);
                }
                unit += ch.len_utf16();
            }
            for index in 0..units.len() {
                let expected = native.rangeOfComposedCharacterSequenceAtIndex(index);
                assert_eq!(
                    range(rope, index),
                    expected.location..expected.location + expected.length,
                    "at unit {index}"
                );
            }
            for start in (0..units.len()).step_by(31) {
                let count = (units.len() - start).min(97);
                let mut actual = vec![0; count];
                rope.read_utf16(start, &mut actual);
                assert_eq!(actual, units[start..start + count]);
            }
        }
    }

    #[test]
    fn deep_native_query_reads_local_context() {
        let rope = Rope::from_text("é漢👩‍💻e\u{301} ".repeat(100_000).as_str());
        let context = Context {
            rope: &rope,
            read_units: std::cell::Cell::new(0),
        };
        let at = rope.len_utf16() - 5;
        let result = query(&context, at);
        assert!(result.contains(&at));
        assert!(
            context.read_units.get() < 4096,
            "read {} units",
            context.read_units.get()
        );
    }
}
