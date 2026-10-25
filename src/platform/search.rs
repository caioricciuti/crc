//! Search semantics shared by the find bar and project search.
//! Foundation supplies ICU regular expressions; offsets are translated back
//! to UTF-8 byte ranges before they touch the rope.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

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

/// UTF-16 offsets to UTF-8 byte offsets, for offsets asked in rising
/// order: each answer walks on from the last, so a whole list of matches
/// costs one pass over the text rather than one per match.
struct Utf16Walker<'a> {
    text: &'a str,
    byte: usize,
    units: usize,
}

impl<'a> Utf16Walker<'a> {
    fn new(text: &'a str) -> Self {
        Utf16Walker {
            text,
            byte: 0,
            units: 0,
        }
    }

    /// The byte offset of UTF-16 offset `utf16`, or `None` inside a
    /// surrogate pair or past the end.
    fn byte(&mut self, utf16: usize) -> Option<usize> {
        if utf16 < self.units {
            (self.byte, self.units) = (0, 0);
        }
        while self.units < utf16 {
            let ch = self.text[self.byte..].chars().next()?;
            self.units += ch.len_utf16();
            self.byte += ch.len_utf8();
        }
        (self.units == utf16).then_some(self.byte)
    }
}

/// Returns non-overlapping matches and their expanded replacement text.
pub fn find(
    text: &str,
    query: &str,
    template: &str,
    options: Options,
) -> Result<Vec<Match>, String> {
    find_first(text, query, template, options, usize::MAX)
}

/// [`find`], stopping after `limit` matches: project search, which shows
/// a few hundred, need not convert every match of a minified file.
pub fn find_first(
    text: &str,
    query: &str,
    template: &str,
    options: Options,
    limit: usize,
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
    let mut out = Vec::with_capacity(matches.count().min(limit));
    let mut walker = Utf16Walker::new(text);
    for found in matches.iter() {
        if out.len() >= limit {
            break;
        }
        let r = found.range();
        let Some(start) = walker.byte(r.location) else {
            continue;
        };
        let Some(end) = walker.byte(r.location + r.length) else {
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

/// A match in a file of the project, as project search lists it.
pub struct ProjectHit {
    pub path: PathBuf,
    pub range: Range<usize>,
    pub line: usize,
    /// Bytes from the start of `line` to the match: how the hit is found
    /// again in an open document edited since the search read the disk.
    pub column: usize,
    /// The line around the match, from [`snippet_around`].
    pub snippet: String,
    /// The match's bytes in `snippet`, to show which part of the line hit.
    pub at: Range<usize>,
}

/// A snippet shows this many characters of a line, this many of them
/// before the match.
const SNIPPET_WIDTH: usize = 100;
const SNIPPET_BEFORE: usize = 30;

/// Up to [`SNIPPET_WIDTH`] characters of `line` around the match at
/// `at` (bytes of `line`), with an ellipsis at each end that was cut,
/// leading blanks left out and trailing ones trimmed; and where the
/// match is in it. The hits of one line get the same window, so the
/// rows that list them can be one row with each match marked.
pub fn snippet_around(line: &str, at: Range<usize>) -> (String, Range<usize>) {
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let count = chars.len();
    let index = chars.partition_point(|&(b, _)| b < at.start.min(line.len()));
    let mut from = index
        .saturating_sub(SNIPPET_BEFORE)
        .min(count.saturating_sub(SNIPPET_WIDTH));
    // Blanks at the window's start are left out, not cut: an ellipsis
    // stands only for characters.
    let cut = from > 0;
    while from < index && chars[from].1.is_whitespace() {
        from += 1;
    }
    let to = (from + SNIPPET_WIDTH).min(count);
    let byte_at = |i: usize| chars.get(i).map_or(line.len(), |&(b, _)| b);
    let shown = line[byte_at(from)..byte_at(to)].trim_end();
    let mut out = String::new();
    if cut {
        out.push('\u{2026}');
    }
    let lead = out.len();
    out.push_str(shown);
    if to < count {
        out.push('\u{2026}');
    }
    let start = (at.start.min(line.len()).saturating_sub(byte_at(from))).min(shown.len());
    let end = (at.end.min(line.len()).saturating_sub(byte_at(from))).clamp(start, shown.len());
    (out, lead + start..lead + end)
}

/// Project search shows this many hits at most.
pub const PROJECT_HITS: usize = 500;

/// Files larger than this are not searched.
const PROJECT_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Which files a search looks in: the Search view's "files to include"
/// and "files to exclude", each a comma-separated list of patterns as
/// `.gitignore` reads them (`src`, `*.rs`, `docs/**`, `tests/`). A
/// file is searched when some include pattern matches it or a folder
/// it is in (every file, with no includes), and no exclude pattern does.
#[derive(Default)]
pub struct Filter {
    include: Vec<crate::complete::glob::Pattern>,
    exclude: Vec<crate::complete::glob::Pattern>,
}

impl Filter {
    pub fn parse(include: &str, exclude: &str) -> Self {
        let patterns = |text: &str| {
            text.split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .filter_map(crate::complete::glob::parse)
                .collect::<Vec<_>>()
        };
        Filter {
            include: patterns(include),
            exclude: patterns(exclude),
        }
    }

    /// Whether the file at `relative` (slash-separated, from the root)
    /// is searched.
    pub fn allows(&self, relative: &str) -> bool {
        let hit = |patterns: &[crate::complete::glob::Pattern]| {
            patterns.iter().any(|pattern| {
                if pattern.matches(relative, false) {
                    return true;
                }
                // A folder the file is in: `src` means everything under src.
                let mut end = relative.len();
                while let Some(slash) = relative[..end].rfind('/') {
                    if pattern.matches(&relative[..slash], true) {
                        return true;
                    }
                    end = slash;
                }
                false
            })
        };
        (self.include.is_empty() || hit(&self.include)) && !hit(&self.exclude)
    }
}

/// Searches every file under `root` the project index would list and
/// `filter` allows, up to [`PROJECT_HITS`] hits. `None` when `cancel`
/// was set before it finished.
pub fn search_tree(
    root: PathBuf,
    query: &str,
    options: Options,
    filter: &Filter,
    cancel: &AtomicBool,
) -> Option<Result<Vec<ProjectHit>, String>> {
    let mut finder = crate::project::finder::Finder::new();
    finder.scan(root);
    let mut results = Vec::new();
    for i in 0..finder.len() {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let Some(path) = finder
            .entry(i)
            .filter(|e| filter.allows(&e.relative))
            .map(|e| e.path.clone())
        else {
            continue;
        };
        if !std::fs::metadata(&path).is_ok_and(|m| m.len() <= PROJECT_FILE_BYTES) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok((text, _)) = crate::text::file_format::decode(&bytes) else {
            continue;
        };
        let room = PROJECT_HITS - results.len();
        match hits_in(&text, &path, query, options, room, cancel) {
            Ok(Some(hits)) => results.extend(hits),
            Ok(None) => return None,
            Err(error) => return Some(Err(error)),
        }
        if results.len() >= PROJECT_HITS {
            break;
        }
    }
    (!cancel.load(Ordering::Relaxed)).then_some(Ok(results))
}

/// Up to `room` hits for `query` in `text`, the contents of `path`, with
/// line, column and a snippet of the line around each. `Ok(None)` when
/// `cancel` was set part way.
pub fn hits_in(
    text: &str,
    path: &Path,
    query: &str,
    options: Options,
    room: usize,
    cancel: &AtomicBool,
) -> Result<Option<Vec<ProjectHit>>, String> {
    let matches = find_first(text, query, "", options, room)?;
    let mut hits = Vec::with_capacity(matches.len());
    // Lines counted on from the previous match, not from the top of the
    // file each time.
    let (mut counted_to, mut line, mut line_start) = (0, 0, 0);
    for found in matches {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let span = &text[counted_to..found.range.start];
        line += span.bytes().filter(|b| *b == b'\n').count();
        if let Some(i) = span.rfind('\n') {
            line_start = counted_to + i + 1;
        }
        counted_to = found.range.start;
        // The snippet is a hundred characters: look no further than a
        // couple of hundred bytes either way for the line's ends, or a
        // minified file is walked from its start for every match.
        let mut lo = found.range.start.saturating_sub(200);
        while !text.is_char_boundary(lo) {
            lo += 1;
        }
        let mut hi = (found.range.end + 200).min(text.len());
        while !text.is_char_boundary(hi) {
            hi -= 1;
        }
        let start = text[lo..found.range.start]
            .rfind('\n')
            .map_or(lo, |i| lo + i + 1);
        let end = text[found.range.end..hi]
            .find('\n')
            .map_or(hi, |i| found.range.end + i);
        let (snippet, at) = snippet_around(
            &text[start..end],
            found.range.start - start..found.range.end - start,
        );
        hits.push(ProjectHit {
            path: path.to_path_buf(),
            column: found.range.start - line_start,
            range: found.range,
            line,
            snippet,
            at,
        });
    }
    Ok(Some(hits))
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

    #[test]
    fn project_hits_carry_line_column_and_a_trimmed_snippet() {
        let text = "soil: loam\n  water: evening\nwater the beds\n";
        let none = AtomicBool::new(false);
        let path = Path::new("garden-log/notes.txt");
        let hits = hits_in(text, path, "water", Options::default(), 10, &none)
            .unwrap()
            .unwrap();
        let at: Vec<_> = hits
            .iter()
            .map(|h| (h.line, h.column, h.snippet.as_str()))
            .collect();
        assert_eq!(at, [(1, 2, "water: evening"), (2, 0, "water the beds")]);
        assert_eq!(&text[hits[0].range.clone()], "water");
        assert_eq!(&hits[0].snippet[hits[0].at.clone()], "water");

        let one = hits_in(text, path, "water", Options::default(), 1, &none)
            .unwrap()
            .unwrap();
        assert_eq!(one.len(), 1, "room bounds the hits");
        let cancelled = AtomicBool::new(true);
        assert!(
            hits_in(text, path, "water", Options::default(), 10, &cancelled)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_snippet_stops_near_the_match_on_a_long_line() {
        let text = format!("{}needle{}", "é".repeat(300), "x".repeat(300));
        let none = AtomicBool::new(false);
        let hits = hits_in(
            &text,
            Path::new("min.js"),
            "needle",
            Options::default(),
            10,
            &none,
        )
        .unwrap()
        .unwrap();
        assert_eq!(hits[0].column, 600);
        assert_eq!(hits[0].snippet.chars().count(), 102, "100 and two ellipses");
        assert!(hits[0].snippet.starts_with("\u{2026}é"));
        assert_eq!(&hits[0].snippet[hits[0].at.clone()], "needle");
    }

    #[test]
    fn the_filter_reads_include_and_exclude_patterns() {
        let none = Filter::parse("", "");
        assert!(none.allows("src/main.rs"));
        let src = Filter::parse("src, *.md", "");
        assert!(src.allows("src/main.rs"));
        assert!(src.allows("src/deep/down.rs"));
        assert!(src.allows("docs/notes.md"));
        assert!(!src.allows("tests/held.rs"));
        let not_tests = Filter::parse("", "tests/, *.lock");
        assert!(not_tests.allows("src/main.rs"));
        assert!(!not_tests.allows("tests/held.rs"));
        assert!(!not_tests.allows("Cargo.lock"));
        let anchored = Filter::parse("docs/**", "docs/old");
        assert!(anchored.allows("docs/a/b.md"));
        assert!(!anchored.allows("docs/old/x.md"));
        assert!(!anchored.allows("src/docs/x.md"));
    }

    #[test]
    fn a_snippet_marks_the_match_and_shares_a_short_line() {
        let (snippet, at) = snippet_around("    let plant = plants[0];", 8..13);
        assert_eq!(snippet, "let plant = plants[0];");
        assert_eq!(&snippet[at], "plant");
        // The second match of the same line: the same window, so a row
        // can mark both.
        let (again, at) = snippet_around("    let plant = plants[0];", 16..21);
        assert_eq!(again, snippet);
        assert_eq!(&again[at], "plant");
        // Empty and out-of-range stay inside the string.
        let (s, at) = snippet_around("", 0..0);
        assert_eq!((s.as_str(), at), ("", 0..0));
        let (s, at) = snippet_around("abc", 10..12);
        assert_eq!((s.as_str(), at), ("abc", 3..3));
    }
}
