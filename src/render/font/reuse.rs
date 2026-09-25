//! Exact, bounded remapping of whole lines between immutable rope snapshots.
use crate::text::rope::Rope;
use std::ops::Range;

pub(super) struct LineReuse<'a> {
    old: &'a Rope,
    pub new: &'a Rope,
    prefix: usize,
    suffix: usize,
}

impl<'a> LineReuse<'a> {
    pub fn new(old: &'a Rope, new: &'a Rope) -> Self {
        let (prefix, suffix) = old.unchanged_edges(new);
        Self {
            old,
            new,
            prefix,
            suffix,
        }
    }

    pub fn map(&self, line: usize) -> Option<(usize, Range<usize>)> {
        if line >= self.old.len_lines() {
            return None;
        }
        let start = self.old.line_to_byte(line);
        let end = self.old.line_to_byte(line + 1);
        let prefix = (end <= self.prefix).then_some(start..end);
        let suffix = if start >= self.old.len_bytes() - self.suffix {
            let new_start = self.new.len_bytes() - (self.old.len_bytes() - start);
            Some(new_start..new_start + (end - start))
        } else {
            None
        };
        // An equal byte range is insufficient: inserting at its boundary can
        // join it to a different paragraph, changing shaping context.
        [prefix, suffix].into_iter().flatten().find_map(|mapped| {
            let new_line = self.new.byte_to_line(mapped.start);
            (self.new.line_to_byte(new_line) == mapped.start
                && self.new.line_to_byte(new_line + 1) == mapped.end)
                .then_some((new_line, mapped))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remapped_lines_match_complete_native_paragraph_sources() {
        // Try every UTF-8 edit boundary, including both sides of CRLF and EOF.
        let source = "é\r\nאב\n👩‍💻\t漢\nlast";
        let old = Rope::from_text(source);
        let boundaries: Vec<_> = source
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(source.len()))
            .collect();
        for &start in &boundaries {
            for &end in boundaries.iter().filter(|&&end| end >= start) {
                for insertion in ["", "x", "\n", "\r\n", "e\u{301}"] {
                    let mut new = old.clone();
                    new.delete(start..end);
                    new.insert(start, insertion);
                    let mapping = LineReuse::new(&old, &new);
                    for line in 0..old.len_lines() {
                        if let Some((mapped, range)) = mapping.map(line) {
                            assert_eq!(
                                old.line(line),
                                new.line(mapped),
                                "{start}..{end} + {insertion:?}: {line} -> {mapped}"
                            );
                            assert_eq!(
                                range,
                                new.line_to_byte(mapped)..new.line_to_byte(mapped + 1)
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn newline_insertions_shift_suffix_and_joins_invalidate_context() {
        let old = Rope::from_text("é\r\nאב\n👩‍💻");
        let mut new = old.clone();
        new.insert(old.line_to_byte(1), "inserted\n");
        let mapping = LineReuse::new(&old, &new);
        assert_eq!(mapping.map(0).unwrap().0, 0);
        assert_eq!(mapping.map(1).unwrap().0, 2);
        assert_eq!(mapping.map(2).unwrap().0, 3);
        let restored = LineReuse::new(&new, &old);
        assert_eq!(restored.map(2).unwrap().0, 1);
        assert_eq!(restored.map(3).unwrap().0, 2);

        let mut joined = old.clone();
        joined.delete(2..4);
        let mapping = LineReuse::new(&old, &joined);
        assert!(mapping.map(0).is_none());
        assert!(mapping.map(1).is_none());
        assert_eq!(mapping.map(2).unwrap().0, 1);
    }

    #[test]
    fn overlapping_equal_edges_preserve_whole_paragraphs() {
        let old = Rope::from_text(&format!("e\u{301}{}\n", "漢字".repeat(2000)));
        let mut new = old.clone();
        new.insert(0, "edited header\n");
        // Both texts start with 'e'; clipping the equal suffix to exclude
        // that byte would needlessly invalidate the entire unchanged line.
        assert_eq!(LineReuse::new(&old, &new).map(0).unwrap().0, 1);
        assert_eq!(LineReuse::new(&new, &old).map(1).unwrap().0, 0);

        let old = Rope::from_text("é\né\n");
        let mut new = old.clone();
        new.delete(0..3);
        let mapping = LineReuse::new(&old, &new);
        assert_eq!(mapping.map(0).unwrap().0, 0);
        assert_eq!(mapping.map(1).unwrap().0, 0);
    }
}
