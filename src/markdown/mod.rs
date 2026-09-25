//! A small Markdown parser for the preview mode.
//!
//! Deliberately not CommonMark. CommonMark is a 300-page specification whose
//! hard parts (nested emphasis precedence, link reference definitions, HTML
//! blocks, lazy continuation) exist to describe what the reference
//! implementations already did, and none of them change how a README reads.
//! This covers what people actually write: headings, paragraphs, fenced and
//! indented code, lists, block quotes, rules, tables, and inline emphasis,
//! code, links and strikethrough.

//!
//! The output is deliberately flat. The renderer draws rows of styled runs,
//! so a tree would only have to be flattened again; blocks carry their own
//! indent depth instead.

use std::ops::Range;

/// A styled run of text within one line.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub text: String,
    pub style: Style,
}

/// How a run should be drawn. The renderer maps these onto theme colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Plain,
    Strong,
    Emphasis,
    /// Both at once, from `***text***` or nested runs.
    StrongEmphasis,
    Code,
    Link,
    /// The alt text of `![alt](src)`. Shown as the alt text, marked as an
    /// image: previously the `[` handler claimed it and left the `!` behind
    /// as a stray character in the prose.
    Image,
    Strike,
}

/// One block of the document, already flattened to rows.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Heading {
        level: u8,
        runs: Vec<Run>,
    },
    Paragraph {
        runs: Vec<Run>,
    },
    /// A fenced or indented code block. Kept as raw lines so it can be
    /// syntax-highlighted later using `lang`.
    Code {
        lang: String,
        lines: Vec<String>,
    },
    ListItem {
        /// Nesting depth, 0 for a top-level item.
        depth: usize,
        /// `Some(n)` for an ordered item, `None` for a bullet.
        number: Option<u64>,
        /// `Some(checked)` for a task list item.
        task: Option<bool>,
        runs: Vec<Run>,
    },
    Quote {
        depth: usize,
        runs: Vec<Run>,
    },
    /// A table row. The first is the header; the delimiter row is dropped.
    TableRow {
        cells: Vec<Vec<Run>>,
        header: bool,
    },
    Rule,
    Blank,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpannedBlock {
    pub block: Block,
    /// Source line numbers, end exclusive.
    pub lines: Range<usize>,
}

/// Maps each visible character boundary back to a UTF-8 source offset.
/// The renderer uses this for editing a formatted block without exposing its
/// Markdown delimiters. The source remains the sole stored representation.
pub fn source_offsets(source: &str, lines: Range<usize>, visible: &str) -> Vec<usize> {
    let starts: Vec<usize> = std::iter::once(0)
        .chain(source.match_indices('\n').map(|(at, _)| at + 1))
        .collect();
    let start = *starts.get(lines.start).unwrap_or(&source.len());
    let end = *starts.get(lines.end).unwrap_or(&source.len());
    source_offsets_in_range(source, start..end, visible)
}

pub fn source_offsets_in_range(source: &str, range: Range<usize>, visible: &str) -> Vec<usize> {
    let (start, end) = (range.start, range.end);
    let part = &source[start..end];
    let mut cursor = 0;
    let mut offsets = Vec::with_capacity(visible.chars().count() + 1);
    for ch in visible.chars() {
        if let Some((relative, _)) = part[cursor..].char_indices().find(|(_, c)| *c == ch) {
            cursor += relative;
            offsets.push(start + cursor);
            cursor += ch.len_utf8();
        } else {
            offsets.push(start + cursor);
        }
    }
    offsets.push(start + cursor);
    offsets
}

/// Parses Markdown into blocks.
pub fn parse(source: &str) -> Vec<Block> {
    parse_spanned(source).into_iter().map(|b| b.block).collect()
}

/// Parses while retaining the source lines behind every rendered block.
pub fn parse_spanned(source: &str) -> Vec<SpannedBlock> {
    let lines: Vec<&str> = source.lines().collect();
    let mut blocks: Vec<SpannedBlock> = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let start = i;
        let line = lines[i];
        // The last block that was not a blank line, which is what decides
        // whether an indented line continues a list or opens a code block.
        let previous = blocks
            .iter()
            .rev()
            .map(|b| &b.block)
            .find(|b| !matches!(b, Block::Blank));
        // Spaces and tabs only. Markdown indentation is made of those, and
        // `trim_start` also strips the ideographic space that Chinese and
        // Japanese paragraphs open with: two of them measured as six bytes of
        // "indent", which made the line indented code and then sliced it at
        // byte 4, in the middle of the second one.
        let trimmed = line.trim_start_matches([' ', '\t']);
        // Indentation in columns, with a tab advancing to the next multiple
        // of four. Counting bytes made one tab worth one column, so a
        // tab-indented list item came out at its parent's depth and the
        // nesting collapsed.
        let indent = indent_columns(line);

        // Indented code, before anything else can claim the line. This used
        // to be checked last, so `    # comment` inside a code block was
        // taken for a level-one heading and the remaining lines became a
        // second, separate block. A list item's own continuation is gathered
        // by the item itself and never reaches here.
        if indent >= 4 && !matches!(previous, Some(Block::ListItem { .. })) {
            let mut body = vec![strip_columns(line, 4)];
            i += 1;
            while i < lines.len() && (lines[i].trim().is_empty() || indent_columns(lines[i]) >= 4) {
                if lines[i].trim().is_empty() {
                    let ends = lines
                        .get(i + 1)
                        .is_none_or(|next| indent_columns(next) < 4 && !next.trim().is_empty());
                    if ends {
                        break;
                    }
                }
                body.push(strip_columns(lines[i], 4));
                i += 1;
            }
            blocks.push(SpannedBlock {
                block: Block::Code {
                    lang: String::new(),
                    lines: body,
                },
                lines: start..i,
            });
            continue;
        }

        // Fenced code. Everything inside is literal, which is why this is
        // checked before anything else could claim it.
        if let Some(fence) = fence_of(trimmed) {
            let lang = trimmed[fence.len()..].trim().to_string();
            let mut body = Vec::new();
            i += 1;
            let marker = fence.chars().next().unwrap_or('`');
            while i < lines.len() {
                let candidate = lines[i].trim_start();
                // A closing fence may be longer than the opening one, and
                // CommonMark says so. Requiring an exact match meant a file
                // that closed ``` with ```` was read as code to its end.
                let closes = candidate.starts_with(&fence)
                    && candidate.trim_end().chars().all(|c| c == marker);
                if closes {
                    i += 1;
                    break;
                }
                body.push(lines[i].to_string());
                i += 1;
            }
            blocks.push(SpannedBlock {
                block: Block::Code { lang, lines: body },
                lines: start..i,
            });
            continue;
        }

        if trimmed.is_empty() {
            i += 1;
            blocks.push(SpannedBlock {
                block: Block::Blank,
                lines: start..i,
            });
            continue;
        }

        if is_rule(trimmed) {
            i += 1;
            blocks.push(SpannedBlock {
                block: Block::Rule,
                lines: start..i,
            });
            continue;
        }

        // ATX heading.
        if let Some(level) = heading_level(trimmed) {
            let text = trimmed[level as usize..].trim_start();
            // A closing run of #s is decoration, not content.
            let text = text.trim_end().trim_end_matches('#').trim_end();
            i += 1;
            blocks.push(SpannedBlock {
                block: Block::Heading {
                    level,
                    runs: parse_inline(text),
                },
                lines: start..i,
            });
            continue;
        }

        // Setext heading: a line underlined with = or -.
        if i + 1 < lines.len() {
            let next = lines[i + 1].trim();
            // `-----` under text is ambiguous: it is both a valid setext
            // underline and a valid thematic break. Content above settles it
            // in favour of the heading, and we only reach here with content
            // above, since a bare rule was already claimed by the check
            // further up.
            let underlined = !next.is_empty()
                && (next.chars().all(|c| c == '=') || next.chars().all(|c| c == '-'));
            if underlined {
                i += 2;
                blocks.push(SpannedBlock {
                    block: Block::Heading {
                        level: if next.starts_with('=') { 1 } else { 2 },
                        runs: parse_inline(trimmed),
                    },
                    lines: start..i,
                });
                continue;
            }
        }

        // A quote, as deep as it has markers. Stripping one `>` and fixing
        // the depth at zero meant `> > nested` drew the inner marker as if it
        // were text.
        if trimmed.starts_with('>') {
            let mut rest = trimmed;
            let mut depth = 0;
            while let Some(inner) = rest.strip_prefix('>') {
                depth += 1;
                rest = inner.strip_prefix(' ').unwrap_or(inner);
            }
            i += 1;
            blocks.push(SpannedBlock {
                block: Block::Quote {
                    depth: depth - 1,
                    runs: parse_inline(rest),
                },
                lines: start..i,
            });
            continue;
        }

        if let Some((number, task, content)) = list_item(trimmed) {
            // An item runs on over as many source lines as it likes, exactly
            // as a paragraph does. Stopping at the first line left the rest
            // of the item as a separate paragraph, which then drew flush with
            // the margin instead of hanging beside its own marker.
            let mut text = content.to_string();
            i += 1;
            gather_continuation(&lines, &mut i, &mut text);
            blocks.push(SpannedBlock {
                block: Block::ListItem {
                    // Two columns per level, so two-space, four-space and
                    // tab-indented files all nest, in columns rather than
                    // bytes. CommonMark's real rule is subtler.
                    depth: indent / 2,
                    number,
                    task,
                    runs: parse_inline(&text),
                },
                lines: start..i,
            });
            continue;
        }

        // A table needs its delimiter row to be a table at all.
        if trimmed.contains('|') && i + 1 < lines.len() && is_table_delimiter(lines[i + 1].trim()) {
            i += 2;
            blocks.push(SpannedBlock {
                block: Block::TableRow {
                    cells: split_row(trimmed),
                    header: true,
                },
                lines: start..i,
            });
            while i < lines.len() && lines[i].trim().contains('|') {
                let row = i;
                blocks.push(SpannedBlock {
                    block: Block::TableRow {
                        cells: split_row(lines[i].trim()),
                        header: false,
                    },
                    lines: row..row + 1,
                });
                i += 1;
            }
            continue;
        }

        // Paragraph: consume until something else claims a line.
        let mut text = String::from(trimmed);
        i += 1;
        gather_continuation(&lines, &mut i, &mut text);
        blocks.push(SpannedBlock {
            block: Block::Paragraph {
                runs: parse_inline(&text),
            },
            lines: start..i,
        });
    }

    blocks
}

/// Leading indentation in columns, with tabs advancing to the next multiple
/// of four.
fn indent_columns(line: &str) -> usize {
    let mut columns = 0;
    for ch in line.chars() {
        match ch {
            ' ' => columns += 1,
            '\t' => columns = (columns / 4 + 1) * 4,
            _ => break,
        }
    }
    columns
}

/// Drops `want` columns of leading indentation, tabs included, and returns
/// what is left. Slicing at a byte offset split multi-byte characters.
fn strip_columns(line: &str, want: usize) -> String {
    let mut columns = 0;
    let mut chars = line.char_indices();
    for (at, ch) in chars.by_ref() {
        if columns >= want {
            return line[at..].to_string();
        }
        match ch {
            ' ' => columns += 1,
            '\t' => columns = (columns / 4 + 1) * 4,
            _ => return line[at..].to_string(),
        }
    }
    String::new()
}

/// Appends the lazy continuation lines of a paragraph-like block to `text`,
/// advancing `i` past them.
///
/// A paragraph and a list item both run on until a blank line or a line that
/// opens some other block, so both want the same rule. Leading whitespace on
/// a continuation is dropped: it is indentation to line the source up under a
/// marker, not content, and keeping it would put runs of spaces mid-sentence.
fn gather_continuation(lines: &[&str], i: &mut usize, text: &mut String) {
    while *i < lines.len() {
        let next = lines[*i].trim_start();
        if next.is_empty()
            || heading_level(next).is_some()
            || fence_of(next).is_some()
            || is_rule(next)
            || next.starts_with('>')
            || list_item(next).is_some()
        {
            break;
        }
        text.push(' ');
        text.push_str(next);
        *i += 1;
    }
}

/// The fence that opens a code block, if this line does.
fn fence_of(line: &str) -> Option<String> {
    for marker in ['`', '~'] {
        let count = line.chars().take_while(|c| *c == marker).count();
        if count >= 3 {
            return Some(std::iter::repeat_n(marker, count).collect());
        }
    }
    None
}

fn heading_level(line: &str) -> Option<u8> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    // `#text` without a space is not a heading; `#` alone is.
    let valid = (1..=6).contains(&hashes) && line[hashes..].chars().next().is_none_or(|c| c == ' ');
    valid.then_some(hashes as u8)
}

fn is_rule(line: &str) -> bool {
    for marker in ['-', '*', '_'] {
        let stripped: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        if stripped.len() >= 3 && stripped.chars().all(|c| c == marker) {
            return true;
        }
    }
    false
}

/// Splits a list marker from its content.
fn list_item(line: &str) -> Option<(Option<u64>, Option<bool>, &str)> {
    let mut number = None;
    let rest = if let Some(rest) = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))
    {
        rest
    } else {
        // Ordered: digits then `.` or `)`.
        let digits = line.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 || digits > 9 {
            return None;
        }
        let after = &line[digits..];
        let rest = after
            .strip_prefix(". ")
            .or_else(|| after.strip_prefix(") "))?;
        number = line[..digits].parse::<u64>().ok();
        rest
    };

    // Task list markers, which are the one list extension worth having.
    let (task, content) = if let Some(c) = rest.strip_prefix("[ ] ") {
        (Some(false), c)
    } else if let Some(c) = rest
        .strip_prefix("[x] ")
        .or_else(|| rest.strip_prefix("[X] "))
    {
        (Some(true), c)
    } else {
        (None, rest)
    };

    Some((number, task, content))
}

fn is_table_delimiter(line: &str) -> bool {
    let body = line.trim_matches('|');
    !body.is_empty()
        && body.split('|').all(|cell| {
            let c = cell.trim();
            !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':')
        })
}

fn split_row(line: &str) -> Vec<Vec<Run>> {
    line.trim_matches('|')
        .split('|')
        .map(|cell| parse_inline(cell.trim()))
        .collect()
}

/// Splits a line into styled runs.
///
/// Single-pass and non-recursive: nested emphasis is rare in practice and
/// getting CommonMark's precedence rules right would cost more than it
/// returns for a preview.
pub fn parse_inline(text: &str) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut plain = String::new();
    let mut i = 0;

    let flush = |plain: &mut String, runs: &mut Vec<Run>| {
        if !plain.is_empty() {
            runs.push(Run {
                text: std::mem::take(plain),
                style: Style::Plain,
            });
        }
    };

    while i < chars.len() {
        // Backslash escape, but only of ASCII punctuation, which is all
        // CommonMark allows. Escaping anything meant every backslash in the
        // text disappeared: `C:\Users\caio` rendered as `C:Userscaio`.
        if chars[i] == '\\'
            && let Some(next) = chars.get(i + 1)
            && next.is_ascii_punctuation()
        {
            plain.push(*next);
            i += 2;
            continue;
        }

        // An image, before the link rule can claim its `[`.
        if chars[i] == '!'
            && chars.get(i + 1) == Some(&'[')
            && let Some(close) = find_from(&chars, i + 2, ']')
            && chars.get(close + 1) == Some(&'(')
            && let Some(paren) = find_from(&chars, close + 2, ')')
        {
            flush(&mut plain, &mut runs);
            runs.push(Run {
                text: chars[i + 2..close].iter().collect(),
                style: Style::Image,
            });
            i = paren + 1;
            continue;
        }

        // Inline code wins over everything inside it.
        if chars[i] == '`'
            && let Some(end) = find_from(&chars, i + 1, '`')
        {
            flush(&mut plain, &mut runs);
            runs.push(Run {
                text: chars[i + 1..end].iter().collect(),
                style: Style::Code,
            });
            i = end + 1;
            continue;
        }

        if let Some((marker, style)) = delimiter_at(&chars, i)
            && let Some(end) = find_run(&chars, i + marker, &chars[i], marker)
        {
            flush(&mut plain, &mut runs);
            // Emphasis nests: `**bold with *both* inside**`. Parsing the
            // inside as well is what makes `***both***` work at all.
            let inner = parse_inline(&chars[i + marker..end].iter().collect::<String>());
            for run in inner {
                runs.push(Run {
                    style: combine(style, run.style),
                    text: run.text,
                });
            }
            i = end + marker;
            continue;
        }

        // A link: [text](target). Only the text is shown.
        if chars[i] == '['
            && let Some(close) = find_from(&chars, i + 1, ']')
            && chars.get(close + 1) == Some(&'(')
            && let Some(paren) = find_from(&chars, close + 2, ')')
        {
            flush(&mut plain, &mut runs);
            runs.push(Run {
                text: chars[i + 1..close].iter().collect(),
                style: Style::Link,
            });
            i = paren + 1;
            continue;
        }

        plain.push(chars[i]);
        i += 1;
    }

    flush(&mut plain, &mut runs);
    runs
}

/// An inner style applied inside an outer one.
fn combine(outer: Style, inner: Style) -> Style {
    match (outer, inner) {
        (outer, Style::Plain) => outer,
        (Style::Strong, Style::Emphasis) | (Style::Emphasis, Style::Strong) => {
            Style::StrongEmphasis
        }
        // Code, links and images keep their own identity inside emphasis.
        (_, inner) => inner,
    }
}

/// The emphasis delimiter starting at `i`, as (length, style).
///
/// A delimiter only opens emphasis when it is *left-flanking*: not followed by
/// whitespace. Without that rule a lone `*` in `2 * 3 * 4` paired with the
/// next one and ate both, and the arithmetic came out as `2  3  4`.
///
/// `_` is stricter still, and may not open inside a word. That rule exists so
/// that `min_release_age` and `snake_case_name` survive, which they did not:
/// they rendered as `minreleaseage` and `snakecasename`.
fn delimiter_at(chars: &[char], i: usize) -> Option<(usize, Style)> {
    let c = chars[i];
    let run = chars[i..].iter().take_while(|x| **x == c).count();
    let (len, style) = match (c, run) {
        ('~', n) if n >= 2 => (2, Style::Strike),
        ('*' | '_', n) if n >= 3 => (3, Style::StrongEmphasis),
        ('*' | '_', 2) => (2, Style::Strong),
        ('*' | '_', 1) => (1, Style::Emphasis),
        _ => return None,
    };
    // Left-flanking: something that is not whitespace follows the run.
    if chars.get(i + run).is_none_or(|next| next.is_whitespace()) {
        return None;
    }
    if c == '_' && i > 0 && chars[i - 1].is_alphanumeric() {
        return None;
    }
    Some((len, style))
}

/// Finds the closing delimiter of a run, or `None` if it is unterminated.
///
/// The closer has to be *right-flanking*: preceded by something that is not
/// whitespace. `_` additionally may not close inside a word.
fn find_run(chars: &[char], from: usize, marker: &char, len: usize) -> Option<usize> {
    let mut i = from;
    while i + len <= chars.len() {
        if chars[i] == *marker
            && chars[i..].iter().take_while(|c| *c == marker).count() >= len
            && i > from
            && !chars[i - 1].is_whitespace()
            && !(*marker == '_' && chars.get(i + len).is_some_and(|c| c.is_alphanumeric()))
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn find_from(chars: &[char], from: usize, needle: char) -> Option<usize> {
    chars[from..]
        .iter()
        .position(|c| *c == needle)
        .map(|p| p + from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_cursor_maps_through_heading_and_link_markup() {
        let source = "## Hello **café** [site](https://example.com)\n";
        let offsets = source_offsets(source, 0..1, "Hello café site");
        assert_eq!(&source[offsets[0]..offsets[0] + 1], "H");
        assert_eq!(&source[offsets[6]..offsets[6] + 1], "c");
        assert_eq!(&source[offsets[11]..offsets[11] + 1], "s");
    }

    #[test]
    fn source_spans_follow_multiline_blocks() {
        let blocks = parse_spanned("# Title\n\nfirst\nsecond\n\n```rs\nlet x = 1;\n```\n");
        assert_eq!(
            blocks.iter().map(|b| b.lines.clone()).collect::<Vec<_>>(),
            vec![0..1, 1..2, 2..4, 4..5, 5..8]
        );
    }

    fn plain(text: &str) -> Vec<Run> {
        vec![Run {
            text: text.into(),
            style: Style::Plain,
        }]
    }

    #[test]
    fn headings_by_hash_and_underline() {
        let blocks = parse("# One\n\n## Two ##\n\nThree\n=====\n\nFour\n-----\n");
        let levels: Vec<u8> = blocks
            .iter()
            .filter_map(|b| match b {
                Block::Heading { level, .. } => Some(*level),
                _ => None,
            })
            .collect();
        assert_eq!(levels, vec![1, 2, 1, 2]);

        assert_eq!(
            blocks[0],
            Block::Heading {
                level: 1,
                runs: plain("One")
            }
        );
        // The trailing hashes are decoration.
        assert_eq!(
            blocks[2],
            Block::Heading {
                level: 2,
                runs: plain("Two")
            }
        );
    }

    /// A panic here is a panic inside `render()`, on every frame, for as
    /// long as the preview is open: the app cannot be used to fix the file.
    #[test]
    fn unicode_whitespace_is_text_not_indentation() {
        // Ideographic spaces, em spaces, a no-break space, and mixtures with
        // real indentation on either side.
        for lead in [
            "\u{3000}\u{3000}",
            "\u{2003}\u{2003}",
            "\u{a0}\u{a0}\u{a0}\u{a0}",
            "  \u{3000}\u{3000}",
            "\u{3000}    ",
            "    \u{3000}",
        ] {
            let source = format!("{lead}\u{6bb5}\u{843d}\n\n{lead}next\n");
            let blocks = parse(&source);
            assert!(!blocks.is_empty(), "{lead:?}");
        }

        let blocks = parse("\u{3000}\u{3000}\u{6bb5}\u{843d}\n");
        assert!(
            matches!(&blocks[0], Block::Paragraph { .. }),
            "a CJK paragraph indent is not a code block"
        );
    }

    #[test]
    fn a_hash_without_a_space_is_not_a_heading() {
        let blocks = parse("#hashtag\n");
        assert!(matches!(blocks[0], Block::Paragraph { .. }));
    }

    #[test]
    fn fenced_code_keeps_its_content_literal() {
        let blocks = parse("```rust\nlet x = # not a heading;\n```\n");
        assert_eq!(
            blocks[0],
            Block::Code {
                lang: "rust".into(),
                lines: vec!["let x = # not a heading;".into()],
            }
        );
    }

    #[test]
    fn tildes_also_fence() {
        let blocks = parse("~~~\nplain\n~~~\n");
        assert!(matches!(blocks[0], Block::Code { .. }));
    }

    #[test]
    fn an_unterminated_fence_runs_to_the_end() {
        let blocks = parse("```\nstill code\nand more\n");
        assert_eq!(
            blocks[0],
            Block::Code {
                lang: String::new(),
                lines: vec!["still code".into(), "and more".into()],
            }
        );
    }

    #[test]
    fn lists_ordered_unordered_and_tasks() {
        let blocks = parse("- one\n* two\n1. three\n2) four\n- [ ] todo\n- [x] done\n");
        let items: Vec<(Option<u64>, Option<bool>)> = blocks
            .iter()
            .filter_map(|b| match b {
                Block::ListItem { number, task, .. } => Some((*number, *task)),
                _ => None,
            })
            .collect();
        assert_eq!(
            items,
            vec![
                (None, None),
                (None, None),
                (Some(1), None),
                (Some(2), None),
                (None, Some(false)),
                (None, Some(true)),
            ]
        );
    }

    #[test]
    fn nested_lists_carry_depth() {
        let blocks = parse("- top\n  - nested\n    - deeper\n");
        let depths: Vec<usize> = blocks
            .iter()
            .filter_map(|b| match b {
                Block::ListItem { depth, .. } => Some(*depth),
                _ => None,
            })
            .collect();
        assert_eq!(depths, vec![0, 1, 2]);
    }

    #[test]
    fn rules_but_not_setext_underlines() {
        assert!(matches!(parse("---\n")[0], Block::Rule));
        assert!(matches!(parse("***\n")[0], Block::Rule));
        assert!(matches!(parse("___\n")[0], Block::Rule));
        // A single dash under text is a heading, not a rule.
        assert!(matches!(parse("Title\n-\n")[0], Block::Heading { .. }));
    }

    #[test]
    fn paragraphs_join_wrapped_lines() {
        let blocks = parse("one two\nthree four\n\nseparate\n");
        assert_eq!(
            blocks[0],
            Block::Paragraph {
                runs: plain("one two three four")
            }
        );
    }

    #[test]
    fn tables_need_a_delimiter_row() {
        let blocks = parse("| a | b |\n|---|---|\n| 1 | 2 |\n");
        assert_eq!(
            blocks[0],
            Block::TableRow {
                cells: vec![plain("a"), plain("b")],
                header: true
            }
        );
        assert_eq!(
            blocks[1],
            Block::TableRow {
                cells: vec![plain("1"), plain("2")],
                header: false
            }
        );

        // Pipes without a delimiter row are just text.
        let not_a_table = parse("a | b\nc | d\n");
        assert!(matches!(not_a_table[0], Block::Paragraph { .. }));
    }

    #[test]
    fn inline_emphasis_code_and_links() {
        let runs = parse_inline("plain **strong** _em_ `code` [label](http://x) ~~gone~~");
        let styled: Vec<(&str, Style)> = runs.iter().map(|r| (r.text.as_str(), r.style)).collect();
        assert!(styled.contains(&("strong", Style::Strong)));
        assert!(styled.contains(&("em", Style::Emphasis)));
        assert!(styled.contains(&("code", Style::Code)));
        assert!(styled.contains(&("label", Style::Link)));
        assert!(styled.contains(&("gone", Style::Strike)));
    }

    #[test]
    fn code_spans_swallow_their_contents() {
        let runs = parse_inline("`**not strong**`");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].style, Style::Code);
        assert_eq!(runs[0].text, "**not strong**");
    }

    #[test]
    fn unterminated_emphasis_stays_literal() {
        let runs = parse_inline("a * b");
        assert_eq!(runs, plain("a * b"));
        let runs = parse_inline("unclosed `code");
        assert_eq!(runs, plain("unclosed `code"));
    }

    #[test]
    fn backslash_escapes_the_next_character() {
        let runs = parse_inline("\\*not emphasis\\*");
        assert_eq!(runs, plain("*not emphasis*"));
    }

    #[test]
    fn multibyte_text_survives() {
        let runs = parse_inline("**café 漢字 🌍**");
        assert_eq!(runs[0].text, "café 漢字 🌍");
        assert_eq!(runs[0].style, Style::Strong);
    }

    #[test]
    fn an_empty_document_is_empty() {
        assert!(parse("").is_empty());
    }

    #[test]
    fn a_realistic_readme_parses_without_losing_content() {
        let source = "\
# Title

Some **bold** intro with a [link](http://example.com).

## Section

- one
- two

```sh
cargo build
```

| a | b |
|---|---|
| 1 | 2 |

> a quote

---
";
        let blocks = parse(source);
        let kinds = |f: fn(&Block) -> bool| blocks.iter().filter(|b| f(b)).count();
        assert_eq!(kinds(|b| matches!(b, Block::Heading { .. })), 2);
        assert_eq!(kinds(|b| matches!(b, Block::ListItem { .. })), 2);
        assert_eq!(kinds(|b| matches!(b, Block::Code { .. })), 1);
        assert_eq!(kinds(|b| matches!(b, Block::TableRow { .. })), 2);
        assert_eq!(kinds(|b| matches!(b, Block::Quote { .. })), 1);
        assert_eq!(kinds(|b| matches!(b, Block::Rule)), 1);
    }

    /// Regression: an item that ran over two source lines became an item plus
    /// a paragraph, and the paragraph drew at the margin rather than hanging
    /// under the marker.
    #[test]
    fn a_list_item_absorbs_its_continuation_lines() {
        let blocks = parse("1. first line\n   second line\n2. next item\n");
        let items: Vec<&Block> = blocks
            .iter()
            .filter(|b| matches!(b, Block::ListItem { .. }))
            .collect();
        assert_eq!(items.len(), 2, "two markers, two items");
        assert!(
            !blocks.iter().any(|b| matches!(b, Block::Paragraph { .. })),
            "a continuation line must not become its own paragraph"
        );
        let Block::ListItem { runs, .. } = items[0] else {
            unreachable!()
        };
        let text: String = runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(text, "first line second line");
    }

    /// A continuation indented far enough to look like indented code is still
    /// part of the item, because the item claims it first.
    #[test]
    fn a_deeply_indented_continuation_is_not_a_code_block() {
        let blocks = parse("- marker\n      still the item\n");
        assert!(
            !blocks.iter().any(|b| matches!(b, Block::Code { .. })),
            "that is item text, not a code block"
        );
    }
}

#[cfg(test)]
mod commonmark_tests {
    use super::*;

    fn rendered(text: &str) -> String {
        parse_inline(text)
            .iter()
            .map(|r| match r.style {
                Style::Plain => r.text.clone(),
                Style::Emphasis => format!("<i>{}</i>", r.text),
                Style::Strong => format!("<b>{}</b>", r.text),
                Style::StrongEmphasis => format!("<bi>{}</bi>", r.text),
                Style::Code => format!("<code>{}</code>", r.text),
                Style::Link => format!("<a>{}</a>", r.text),
                Style::Image => format!("<img>{}</img>", r.text),
                Style::Strike => format!("<s>{}</s>", r.text),
            })
            .collect()
    }

    /// `min_release_age` rendered as `minreleaseage`: the underscores became
    /// emphasis and were dropped, so prose about code silently lost
    /// characters. A `_` may not open emphasis inside a word.
    #[test]
    fn underscores_inside_a_word_are_literal() {
        assert_eq!(rendered("min_release_age"), "min_release_age");
        assert_eq!(rendered("snake_case_name here"), "snake_case_name here");
        assert_eq!(rendered("a_b_c_d"), "a_b_c_d");
        // At a word boundary it is still emphasis, as CommonMark says.
        assert_eq!(rendered("_yes_ and __also__"), "<i>yes</i> and <b>also</b>");
    }

    /// `2 * 3 * 4 = 24` came out as `2  3  4 = 24`. A delimiter followed by
    /// whitespace cannot open emphasis.
    #[test]
    fn asterisks_surrounded_by_spaces_are_literal() {
        assert_eq!(rendered("2 * 3 * 4 = 24"), "2 * 3 * 4 = 24");
        assert_eq!(rendered("5 * 3"), "5 * 3");
        assert_eq!(rendered("a * b * c * d"), "a * b * c * d");
        assert_eq!(rendered("this *is* emphasis"), "this <i>is</i> emphasis");
    }

    /// Every backslash disappeared, because the escape accepted any next
    /// character. CommonMark escapes ASCII punctuation and nothing else.
    #[test]
    fn only_ascii_punctuation_is_escapable() {
        assert_eq!(
            rendered(r"C:\Users\caio\file.txt"),
            r"C:\Users\caio\file.txt"
        );
        assert_eq!(rendered(r"\*not emphasis\*"), "*not emphasis*");
        assert_eq!(rendered(r"a\_b"), "a_b");
        // A trailing backslash is literal, not a truncated escape.
        assert_eq!(rendered(r"ends with \"), r"ends with \");
    }

    /// An image left its `!` behind in the prose and showed the alt text as
    /// a link, because the link rule claimed the `[`.
    #[test]
    fn an_image_is_its_alt_text_without_a_stray_bang() {
        assert_eq!(rendered("![a cat](cat.png) x"), "<img>a cat</img> x");
        assert_eq!(rendered("[a link](x.html)"), "<a>a link</a>");
        // A bang that is not an image stays put.
        assert_eq!(rendered("wow! [link](x)"), "wow! <a>link</a>");
    }

    #[test]
    fn emphasis_nests() {
        assert_eq!(rendered("***both***"), "<bi>both</bi>");
        assert_eq!(
            rendered("**bold *and* more**"),
            "<b>bold </b><bi>and</bi><b> more</b>"
        );
        assert_eq!(
            rendered("**`code` in bold**"),
            "<code>code</code><b> in bold</b>"
        );
    }

    /// A `#` on the first line of an indented code block was read as a
    /// heading, and the rest of the block became a second, separate one.
    #[test]
    fn indented_code_is_not_searched_for_headings() {
        let blocks = parse("    # not a heading\n    let x = 1;\n");
        assert_eq!(blocks.len(), 1, "one block, not a heading plus code");
        match &blocks[0] {
            Block::Code { lines, .. } => {
                assert_eq!(lines, &["# not a heading", "let x = 1;"]);
            }
            other => panic!("expected code, got {other:?}"),
        }
    }

    /// A tab counted as one column, so a tab-indented item sat at its
    /// parent's depth and the nesting collapsed.
    #[test]
    fn tab_and_space_indentation_both_nest() {
        let depths = |src: &str| {
            parse(src)
                .into_iter()
                .filter_map(|b| match b {
                    Block::ListItem { depth, .. } => Some(depth),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let tabs = depths("- one\n\t- two\n\t\t- three\n");
        assert!(
            tabs[0] < tabs[1] && tabs[1] < tabs[2],
            "tab nesting collapsed: {tabs:?}"
        );
        let spaces = depths("- one\n    - two\n        - three\n");
        assert!(spaces[0] < spaces[1] && spaces[1] < spaces[2]);
    }

    /// A closing fence longer than the opening one is legal, and rejecting it
    /// meant the rest of the file was swallowed as code.
    #[test]
    fn a_longer_closing_fence_closes_the_block() {
        let blocks = parse("```\ncode\n````\nafter\n");
        match &blocks[0] {
            Block::Code { lines, .. } => assert_eq!(lines, &["code"]),
            other => panic!("expected code, got {other:?}"),
        }
        assert!(
            blocks.iter().any(|b| matches!(b, Block::Paragraph { .. })),
            "the text after the fence was eaten: {blocks:?}"
        );
    }

    /// `> > b` showed a literal `>` in the quoted text instead of nesting.
    #[test]
    fn quotes_carry_their_nesting_depth() {
        let blocks = parse("> a\n> > b\n> > > c\n");
        let depths: Vec<_> = blocks
            .iter()
            .filter_map(|b| match b {
                Block::Quote { depth, runs } => Some((
                    *depth,
                    runs.iter().map(|r| r.text.as_str()).collect::<String>(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            depths,
            vec![
                (0, "a".to_string()),
                (1, "b".to_string()),
                (2, "c".to_string())
            ]
        );
    }

    /// The reported crash: two ideographic spaces measured six bytes of
    /// "indent", which made the line indented code and then sliced it at
    /// byte 4, inside a character.
    #[test]
    fn an_ideographic_indent_does_not_slice_a_character() {
        let blocks = parse("\u{3000}\u{3000}Japanese paragraph indent\n");
        assert!(matches!(blocks[0], Block::Paragraph { .. }));
        // And a genuinely indented multi-byte line survives the strip.
        let blocks = parse("    café ☕\n");
        match &blocks[0] {
            Block::Code { lines, .. } => assert_eq!(lines, &["café ☕"]),
            other => panic!("expected code, got {other:?}"),
        }
    }
}
