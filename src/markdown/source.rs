//! Markdown styled where it is written.
//!
//! The preview draws a document; this colours the source, so a Markdown file
//! stays text in the editor: selection, find, multiple cursors and undo work
//! on it as on any other file. Headings, emphasis, code, links, lists,
//! quotes and tables get kinds the theme colours and the text layer draws in
//! bold or italic; the characters that make them (`#`, `**`, backticks, `|`)
//! are marked so they can be drawn faint. Fenced code is handed back with
//! its language, for the grammar that colours it.

use std::ops::Range;

use super::{fence_open, heading_level, is_rule, is_table_delimiter, list_marker};
use crate::syntax::{Kind, Language, Span};

/// A fenced block's code, and the language its info string names.
#[derive(Debug, Clone, PartialEq)]
pub struct Fence {
    pub language: Option<Language>,
    /// The lines between the fences, newlines included.
    pub content: Range<usize>,
}

/// Everything the editor needs to draw one Markdown document.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Styled {
    /// In document order, not overlapping.
    pub spans: Vec<Span>,
    pub fences: Vec<Fence>,
    /// Code blocks, fences included, for the band behind them.
    pub bands: Vec<Range<usize>>,
}

/// Styles a whole document. Linear in its length.
pub fn style(source: &str) -> Styled {
    let mut out = Styled::default();
    let lines: Vec<Range<usize>> = line_ranges(source);
    let text = |r: &Range<usize>| &source[r.clone()];
    let delimiter: Vec<bool> = lines.iter().map(|r| is_table_delimiter(text(r))).collect();

    let mut i = 0;
    // Front matter: YAML between `---` lines at the very top.
    if lines.first().is_some_and(|r| text(r).trim_end() == "---")
        && let Some(end) = (1..lines.len()).find(|&j| {
            let t = text(&lines[j]).trim_end();
            t == "---" || t == "..."
        })
    {
        mark(&mut out.spans, lines[0].clone(), Kind::MdMarker);
        mark(&mut out.spans, lines[end].clone(), Kind::MdMarker);
        let content = lines[1].start..lines[end].start;
        push_code(&mut out, source, content, Some(Language::Yaml));
        out.bands.push(lines[0].start..lines[end].end);
        i = end + 1;
    }

    let mut in_table = false;
    while i < lines.len() {
        let range = lines[i].clone();
        let line = text(&range);
        let indent = line.len() - line.trim_start_matches(' ').len();
        let body = &line[indent..];

        // A fence runs to its closing fence or to the end of the document.
        if indent <= 3
            && let Some((fence_char, fence_len)) = fence_open(body)
        {
            in_table = false;
            let info = body[fence_len..].trim();
            let language = info
                .split(|c: char| c.is_whitespace() || c == '{' || c == ',')
                .next()
                .and_then(fence_language);
            mark(&mut out.spans, range.clone(), Kind::MdMarker);
            let close = (i + 1..lines.len()).find(|&j| {
                let t = text(&lines[j]).trim();
                t.len() >= fence_len && t.chars().all(|c| c == fence_char)
            });
            let last = close.unwrap_or(lines.len());
            let content_end = close.map_or(source.len(), |j| lines[j].start);
            let content = lines.get(i + 1).map_or(source.len(), |r| r.start)..content_end;
            push_code(&mut out, source, content, language);
            if let Some(j) = close {
                mark(&mut out.spans, lines[j].clone(), Kind::MdMarker);
            }
            let band_end = close.map_or(source.len(), |j| lines[j].end);
            out.bands.push(range.start..band_end);
            i = last + 1;
            continue;
        }

        let base = range.start + indent;
        // Tables: a delimiter row, the header above it, the rows below.
        let starts_table = delimiter.get(i + 1).copied().unwrap_or(false) && line.contains('|');
        if delimiter[i] && (in_table || i > 0 && text(&lines[i - 1]).contains('|')) {
            mark(&mut out.spans, range.clone(), Kind::MdMarker);
            in_table = true;
            i += 1;
            continue;
        }
        if starts_table || (in_table && line.contains('|')) {
            table_row(&mut out.spans, source, range.clone(), starts_table);
            in_table = true;
            i += 1;
            continue;
        }
        in_table = false;

        if indent <= 3 {
            if let Some(level) = heading_level(body) {
                let hashes = level as usize;
                let marker_end =
                    base + hashes + (body[hashes..].len() - body[hashes..].trim_start().len());
                mark(&mut out.spans, base..marker_end, Kind::MdMarker);
                inline(
                    &mut out.spans,
                    source,
                    marker_end..range.end,
                    Some(Kind::MdHeading),
                );
                i += 1;
                continue;
            }
            if is_rule(body) {
                mark(&mut out.spans, base..range.end, Kind::MdRule);
                i += 1;
                continue;
            }
            if body.starts_with("<!--") && body.trim_end().ends_with("-->") {
                mark(&mut out.spans, base..range.end, Kind::MdMarker);
                i += 1;
                continue;
            }
        }
        if body.starts_with('>') {
            let quote = body
                .char_indices()
                .find(|&(_, c)| c != '>' && c != ' ')
                .map_or(body.len(), |(at, _)| at);
            mark(&mut out.spans, base..base + quote, Kind::MdMarker);
            inline(
                &mut out.spans,
                source,
                base + quote..range.end,
                Some(Kind::MdQuote),
            );
            i += 1;
            continue;
        }
        if let Some(marker) = list_marker(body) {
            mark(&mut out.spans, base..base + marker, Kind::MdList);
            let rest = &body[marker..];
            let task = ["[ ] ", "[x] ", "[X] "]
                .iter()
                .find(|t| rest.starts_with(**t))
                .map_or(0, |t| t.len() - 1);
            let after = base + marker + task;
            mark(&mut out.spans, base + marker..after, Kind::MdMarker);
            inline(&mut out.spans, source, after..range.end, None);
            i += 1;
            continue;
        }
        inline(&mut out.spans, source, range.clone(), None);
        i += 1;
    }
    out
}

/// Each line's text, without its line break.
fn line_ranges(source: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    for (at, _) in source.match_indices('\n') {
        let end = if at > start && source.as_bytes()[at - 1] == b'\r' {
            at - 1
        } else {
            at
        };
        out.push(start..end);
        start = at + 1;
    }
    if start < source.len() {
        out.push(start..source.len());
    }
    out
}

fn mark(spans: &mut Vec<Span>, range: Range<usize>, kind: Kind) {
    if range.end > range.start {
        spans.push(Span {
            start: range.start,
            end: range.end,
            kind,
        });
    }
}

/// A code block's lines: plain code where no grammar colours them, and the
/// range handed to the one that does.
fn push_code(out: &mut Styled, source: &str, content: Range<usize>, language: Option<Language>) {
    if content.end <= content.start {
        return;
    }
    for line in line_ranges(&source[content.clone()]) {
        mark(
            &mut out.spans,
            content.start + line.start..content.start + line.end,
            Kind::MdCodeBlock,
        );
    }
    out.fences.push(Fence { language, content });
}

/// `(character, length)` of an opening code fence.
/// The grammar for a fence's info string.
pub fn fence_language(info: &str) -> Option<Language> {
    let lower = info.to_ascii_lowercase();
    let extension = match lower.as_str() {
        "rust" => "rs",
        "javascript" | "node" => "js",
        "typescript" => "ts",
        "python" | "python3" => "py",
        "shell" | "console" | "shellsession" => "sh",
        "golang" => "go",
        "c++" => "cpp",
        "jsonc" | "json5" => "json",
        other => other,
    };
    Language::from_extension(extension)
}

/// A table row: the pipes faint, the cells as prose, the header's bold.
fn table_row(spans: &mut Vec<Span>, source: &str, range: Range<usize>, header: bool) {
    let base = if header {
        Some(Kind::MdTableHeader)
    } else {
        None
    };
    let line = &source[range.clone()];
    let mut cell = range.start;
    let mut escaped = false;
    for (at, c) in line.char_indices() {
        let at = range.start + at;
        if c == '|' && !escaped {
            inline(spans, source, cell..at, base);
            mark(spans, at..at + 1, Kind::MdMarker);
            cell = at + 1;
        }
        escaped = c == '\\' && !escaped;
    }
    inline(spans, source, cell..range.end, base);
}

/// The kind for text that is both `outer` and `inner`.
fn combine(outer: Option<Kind>, inner: Kind) -> Kind {
    match (outer, inner) {
        (Some(Kind::MdStrong), Kind::MdEmphasis) | (Some(Kind::MdEmphasis), Kind::MdStrong) => {
            Kind::MdStrongEmphasis
        }
        // A heading, a link or a table header stays what it is; its own
        // weight already carries the emphasis.
        (Some(k @ (Kind::MdHeading | Kind::MdLink | Kind::MdTableHeader)), _) => k,
        _ => inner,
    }
}

/// Inline Markdown within `range`, all of it `base` where nothing more
/// specific applies.
fn inline(spans: &mut Vec<Span>, source: &str, range: Range<usize>, base: Option<Kind>) {
    let bytes = source.as_bytes();
    let end = range.end;
    let mut i = range.start;
    let mut plain = i;
    let flush = |spans: &mut Vec<Span>, from: usize, to: usize| {
        if let Some(kind) = base {
            mark(spans, from..to, kind);
        }
    };
    while i < end {
        let c = bytes[i];
        // `\*` is a literal star.
        if c == b'\\' && i + 1 < end && bytes[i + 1].is_ascii_punctuation() {
            flush(spans, plain, i);
            mark(spans, i..i + 1, Kind::MdMarker);
            let next = i + 1 + utf8_len(bytes[i + 1]);
            flush(spans, i + 1, next);
            i = next;
            plain = i;
            continue;
        }
        if c == b'`' {
            let run = bytes[i..end].iter().take_while(|b| **b == b'`').count();
            if let Some(close) = find_run(bytes, i + run, end, b'`', run) {
                flush(spans, plain, i);
                mark(spans, i..i + run, Kind::MdMarker);
                mark(spans, i + run..close, Kind::MdCode);
                mark(spans, close..close + run, Kind::MdMarker);
                i = close + run;
                plain = i;
                continue;
            }
            i += run;
            continue;
        }
        if c == b'[' || (c == b'!' && bytes.get(i + 1) == Some(&b'[')) {
            let open = if c == b'!' { 2 } else { 1 };
            if let Some((text_end, url_end)) = link_at(bytes, i + open, end) {
                flush(spans, plain, i);
                mark(spans, i..i + open, Kind::MdMarker);
                inline(
                    spans,
                    source,
                    i + open..text_end,
                    Some(combine(base, Kind::MdLink)),
                );
                mark(spans, text_end..text_end + 2, Kind::MdMarker);
                mark(spans, text_end + 2..url_end, Kind::MdUrl);
                mark(spans, url_end..url_end + 1, Kind::MdMarker);
                i = url_end + 1;
                plain = i;
                continue;
            }
        }
        if c == b'<'
            && let Some(close) = bytes[i + 1..end].iter().position(|b| *b == b'>')
        {
            let inner = &source[i + 1..i + 1 + close];
            if (inner.starts_with("http://")
                || inner.starts_with("https://")
                || inner.starts_with("mailto:"))
                && !inner.contains(' ')
            {
                flush(spans, plain, i);
                mark(spans, i..i + 1, Kind::MdMarker);
                mark(spans, i + 1..i + 1 + close, Kind::MdUrl);
                mark(spans, i + 1 + close..i + 2 + close, Kind::MdMarker);
                i += close + 2;
                plain = i;
                continue;
            }
        }
        if (c == b'h')
            && (source[i..end].starts_with("https://") || source[i..end].starts_with("http://"))
            && (i == range.start || !bytes[i - 1].is_ascii_alphanumeric())
        {
            let mut stop = i + bytes[i..end]
                .iter()
                .take_while(|b| !b.is_ascii_whitespace())
                .count();
            while stop > i
                && matches!(
                    bytes[stop - 1],
                    b'.' | b',' | b')' | b';' | b':' | b'!' | b'?'
                )
            {
                stop -= 1;
            }
            flush(spans, plain, i);
            mark(spans, i..stop, Kind::MdUrl);
            i = stop;
            plain = i;
            continue;
        }
        if c == b'~' && bytes.get(i + 1) == Some(&b'~') {
            if let Some(close) = find_run(bytes, i + 2, end, b'~', 2)
                && close > i + 2
            {
                flush(spans, plain, i);
                mark(spans, i..i + 2, Kind::MdMarker);
                inline(
                    spans,
                    source,
                    i + 2..close,
                    Some(combine(base, Kind::MdStrike)),
                );
                mark(spans, close..close + 2, Kind::MdMarker);
                i = close + 2;
                plain = i;
                continue;
            }
            i += 2;
            continue;
        }
        if c == b'*' || c == b'_' {
            let run = bytes[i..end].iter().take_while(|b| **b == c).count();
            let before = (i > range.start).then(|| bytes[i - 1]);
            let after = bytes.get(i + run).copied().filter(|_| i + run < end);
            // Opens only before text, and `_` never inside a word.
            let opens = after.is_some_and(|b| !b.is_ascii_whitespace())
                && !(c == b'_' && before.is_some_and(|b| b.is_ascii_alphanumeric()));
            let len = run.min(3);
            if opens && let Some(close) = find_emphasis_close(bytes, i + len, end, c, len) {
                let kind = match len {
                    1 => Kind::MdEmphasis,
                    2 => Kind::MdStrong,
                    _ => Kind::MdStrongEmphasis,
                };
                flush(spans, plain, i);
                mark(spans, i..i + len, Kind::MdMarker);
                inline(spans, source, i + len..close, Some(combine(base, kind)));
                mark(spans, close..close + len, Kind::MdMarker);
                i = close + len;
                plain = i;
                continue;
            }
            i += run;
            continue;
        }
        i += utf8_len(c);
    }
    flush(spans, plain, end);
}

fn utf8_len(first: u8) -> usize {
    match first {
        0xf0.. => 4,
        0xe0.. => 3,
        0xc0.. => 2,
        _ => 1,
    }
}

/// Where a run of exactly `len` `c`s starts, from `from`.
fn find_run(bytes: &[u8], from: usize, end: usize, c: u8, len: usize) -> Option<usize> {
    let mut i = from;
    while i < end {
        if bytes[i] == c {
            let run = bytes[i..end].iter().take_while(|b| **b == c).count();
            if run == len {
                return Some(i);
            }
            i += run;
        } else {
            i += 1;
        }
    }
    None
}

/// The closing delimiter for emphasis opened with `len` `c`s: right after
/// text, and for `_` not inside a word.
fn find_emphasis_close(bytes: &[u8], from: usize, end: usize, c: u8, len: usize) -> Option<usize> {
    let mut i = from;
    while i < end {
        if bytes[i] == b'`' {
            // Code spans hide their contents from emphasis.
            let run = bytes[i..end].iter().take_while(|b| **b == b'`').count();
            i = find_run(bytes, i + run, end, b'`', run).map_or(i + run, |close| close + run);
            continue;
        }
        if bytes[i] == c {
            let run = bytes[i..end].iter().take_while(|b| **b == c).count();
            let after_text = i > from && !bytes[i - 1].is_ascii_whitespace();
            let word_after = bytes
                .get(i + run)
                .filter(|_| i + run < end)
                .is_some_and(|b| b.is_ascii_alphanumeric());
            if run >= len && after_text && !(c == b'_' && word_after) {
                return Some(i + run - len);
            }
            i += run;
            continue;
        }
        i += 1;
    }
    None
}

/// For a link whose text starts at `from`: where the text ends (the `]`)
/// and where the destination ends (the `)`).
fn link_at(bytes: &[u8], from: usize, end: usize) -> Option<(usize, usize)> {
    let mut depth = 0usize;
    let mut i = from;
    let text_end = loop {
        if i >= end {
            return None;
        }
        match bytes[i] {
            b'\\' => i += 1,
            b'[' => depth += 1,
            b']' if depth == 0 => break i,
            b']' => depth -= 1,
            _ => {}
        }
        i += 1;
    };
    if bytes.get(text_end + 1) != Some(&b'(') {
        return None;
    }
    let mut depth = 0usize;
    let mut j = text_end + 2;
    while j < end {
        match bytes[j] {
            b'(' => depth += 1,
            b')' if depth == 0 => return Some((text_end, j)),
            b')' => depth -= 1,
            b' ' if depth == 0 && bytes.get(j + 1) != Some(&b'"') => return None,
            _ => {}
        }
        j += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(source: &str) -> Vec<(String, Kind)> {
        style(source)
            .spans
            .iter()
            .map(|s| (source[s.start..s.end].to_string(), s.kind))
            .collect()
    }

    #[test]
    fn headings_mark_their_hashes() {
        assert_eq!(
            kinds("## Commands\n"),
            vec![
                ("## ".into(), Kind::MdMarker),
                ("Commands".into(), Kind::MdHeading)
            ]
        );
        assert!(kinds("#hashtag\n").is_empty());
    }

    #[test]
    fn emphasis_code_and_links() {
        let got = kinds("a **b** _c_ `d` [e](f) ~~g~~\n");
        assert!(got.contains(&("b".into(), Kind::MdStrong)));
        assert!(got.contains(&("c".into(), Kind::MdEmphasis)));
        assert!(got.contains(&("d".into(), Kind::MdCode)));
        assert!(got.contains(&("e".into(), Kind::MdLink)));
        assert!(got.contains(&("f".into(), Kind::MdUrl)));
        assert!(got.contains(&("g".into(), Kind::MdStrike)));
        assert!(got.contains(&("**".into(), Kind::MdMarker)));
    }

    #[test]
    fn snake_case_is_not_emphasis() {
        assert!(kinds("call some_long_name here\n").is_empty());
        assert!(kinds("2 * 3 * 4\n").is_empty());
    }

    #[test]
    fn nested_emphasis() {
        let got = kinds("**bold _both_**\n");
        assert!(got.contains(&("bold ".into(), Kind::MdStrong)));
        assert!(got.contains(&("both".into(), Kind::MdStrongEmphasis)));
    }

    #[test]
    fn fences_carry_their_language_and_band() {
        let source = "text\n```rust\nfn main() {}\n```\nafter\n";
        let styled = style(source);
        assert_eq!(styled.fences.len(), 1);
        assert_eq!(styled.fences[0].language, Some(Language::Rust));
        assert_eq!(&source[styled.fences[0].content.clone()], "fn main() {}\n");
        assert_eq!(
            &source[styled.bands[0].clone()],
            "```rust\nfn main() {}\n```"
        );
        // Nothing inside a fence is Markdown.
        assert!(
            kinds("```\n**not bold**\n```\n")
                .iter()
                .all(|(_, k)| *k != Kind::MdStrong)
        );
    }

    #[test]
    fn an_unclosed_fence_runs_to_the_end() {
        let source = "```py\nx = 1\n";
        let styled = style(source);
        assert_eq!(&source[styled.fences[0].content.clone()], "x = 1\n");
    }

    #[test]
    fn tables_lists_quotes_and_rules() {
        let got = kinds("| A | B |\n| --- | --- |\n| c | d |\n");
        assert!(got.contains(&(" A ".into(), Kind::MdTableHeader)));
        assert!(got.contains(&("| --- | --- |".into(), Kind::MdMarker)));
        assert!(
            got.iter()
                .filter(|(t, k)| t == "|" && *k == Kind::MdMarker)
                .count()
                >= 6
        );
        assert!(kinds("- item\n").contains(&("- ".into(), Kind::MdList)));
        assert!(kinds("12. item\n").contains(&("12. ".into(), Kind::MdList)));
        assert!(kinds("- [x] done\n").contains(&("[x]".into(), Kind::MdMarker)));
        assert!(kinds("> said\n").contains(&("said".into(), Kind::MdQuote)));
        assert_eq!(kinds("---\n"), vec![("---".into(), Kind::MdRule)]);
    }

    #[test]
    fn front_matter_is_yaml() {
        let source = "---\ntitle: x\n---\n# H\n";
        let styled = style(source);
        assert_eq!(styled.fences[0].language, Some(Language::Yaml));
        assert_eq!(&source[styled.fences[0].content.clone()], "title: x\n");
    }

    #[test]
    fn spans_are_ordered_and_disjoint() {
        let source = "# T\n\nSome *a* and **b** `c` [d](e) https://x.y.\n\n| a | b |\n|---|---|\n| `c` | d |\n\n```\ncode\n```\n";
        let spans = style(source).spans;
        for pair in spans.windows(2) {
            assert!(pair[0].end <= pair[1].start, "{pair:?}");
        }
    }
}
