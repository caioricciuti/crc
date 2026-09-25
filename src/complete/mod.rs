//! Completion: where suggestions come from, and in what order they are
//! offered.
//!
//! Sources, each with a reason it can say out loud:
//!
//! - the language server, when the file has one;
//! - the project index (`crate::index`): names defined anywhere in the
//!   project and words used in its files, for every language;
//! - words in the file being edited, near the caret;
//! - paths, when the text before the caret is one (any `.gitignore`, a
//!   `./` import, a Markdown link);
//! - what you have accepted before, from the history database, which also
//!   lifts everything else it recognises.
//!
//! Everything here is plain data and functions. The worker thread that runs
//! the slow parts is in [`worker`]; the window only merges and draws.

pub mod glob;
pub mod history;
pub mod worker;

use std::path::{Path, PathBuf};

/// Where a suggestion came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    Server,
    Symbol,
    History,
    Word,
    Path,
}

/// One suggestion.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    /// What is shown, and matched against what was typed.
    pub label: String,
    /// What replaces the typed prefix.
    pub insert: String,
    pub source: Source,
    /// Why it is offered, in a few words: where it is defined, how often it
    /// is used, how often you picked it.
    pub why: String,
    /// Relevance by the source's own measure, 0 to 1.
    pub weight: f32,
    /// For server items: the index into the server's list, which carries
    /// the server's own edit range.
    pub server: Option<usize>,
}

/// How often a text was accepted before, as the history database says.
#[derive(Clone, Debug, PartialEq)]
pub struct Boost {
    pub text: String,
    pub count: u32,
    pub age_days: f32,
    /// Accepted after the same `context` (`tree.`, `Path::`) as now.
    pub same_context: bool,
    pub context: String,
}

pub fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Bytes of the identifier that ends `before`.
pub fn word_len(before: &str) -> usize {
    before
        .chars()
        .rev()
        .take_while(|c| is_word_char(*c))
        .map(char::len_utf8)
        .sum()
}

/// What sits right before the word being typed, when it says something
/// about what can follow: `tree.` in `state.tree.ro`, `Path::` in
/// `Path::ne`, `self->` in C. Empty otherwise.
pub fn context_before(before_word: &str) -> String {
    let separator = [".", "::", "->"]
        .into_iter()
        .find(|s| before_word.ends_with(s));
    let Some(separator) = separator else {
        return String::new();
    };
    let head = &before_word[..before_word.len() - separator.len()];
    let word = word_len(head);
    if word == 0 {
        return String::new();
    }
    format!("{}{separator}", &head[head.len() - word..])
}

/// How well `label` matches what was typed: 1 for an exact-case prefix,
/// less for a prefix in another case, less again for letters in order.
/// `None` when it does not match at all.
pub fn match_score(label: &str, prefix: &str) -> Option<f32> {
    if prefix.is_empty() {
        return Some(0.5);
    }
    if label.starts_with(prefix) {
        return Some(1.0);
    }
    let (label_lower, prefix_lower) = (label.to_lowercase(), prefix.to_lowercase());
    if label_lower.starts_with(&prefix_lower) {
        return Some(0.85);
    }
    // Letters in order, the first one first: `tsl` finds `to_string_lossy`.
    let mut wanted = prefix_lower.chars().peekable();
    let mut gaps = 0usize;
    let mut first = true;
    for c in label_lower.chars() {
        match wanted.peek() {
            Some(&w) if w == c => {
                wanted.next();
            }
            Some(_) if first => return None,
            Some(_) => gaps += 1,
            None => break,
        }
        first = false;
    }
    if wanted.peek().is_some() {
        return None;
    }
    Some((0.5 - gaps as f32 * 0.02).max(0.2))
}

/// Orders `candidates` for `prefix`, best first, at most `limit`.
///
/// One entry per label: the same name from the server, the index and the
/// file is one suggestion, which keeps the server's edit and the best
/// reason. History lifts what you have picked before, most for what you
/// picked after the same `context`.
pub fn rank(
    prefix: &str,
    candidates: Vec<Candidate>,
    boosts: &[Boost],
    limit: usize,
) -> Vec<Candidate> {
    let mut scored: Vec<(f32, Candidate)> = Vec::new();
    for mut candidate in candidates {
        if candidate.label == prefix || candidate.insert == prefix {
            continue;
        }
        let Some(matched) = match_score(&candidate.label, prefix) else {
            continue;
        };
        let mut score = matched * 0.6 + candidate.weight * 0.4;
        let best = boosts
            .iter()
            .filter(|b| b.text == candidate.insert || b.text == candidate.label)
            .max_by(|a, b| {
                a.same_context
                    .cmp(&b.same_context)
                    .then(a.count.cmp(&b.count))
            });
        if let Some(boost) = best {
            let recency = 0.5f32.powf(boost.age_days / 30.0);
            let lift = 0.25 * (1.0 + boost.count as f32).ln() * recency;
            score += if boost.same_context { lift * 2.0 } else { lift };
            let picked = if boost.same_context && !boost.context.is_empty() {
                format!("you picked this {}× after `{}`", boost.count, boost.context)
            } else {
                format!("you picked this {}× here", boost.count)
            };
            candidate.why = if candidate.why.is_empty() {
                picked
            } else {
                format!("{picked} · {}", candidate.why)
            };
        }
        match scored.iter_mut().find(|(_, c)| c.label == candidate.label) {
            Some((best, existing)) => {
                // The most specific source speaks for the name: the server
                // (which carries the edit), then a definition, then your
                // history, then a plain word. Its icon and reason win; the
                // score is the best either had.
                if candidate.source < existing.source {
                    if existing.why.starts_with("you picked")
                        && !candidate.why.starts_with("you picked")
                    {
                        let picked = existing.why.split(" · ").next().unwrap_or("").to_owned();
                        candidate.why = if candidate.why.is_empty() {
                            picked
                        } else {
                            format!("{picked} · {}", candidate.why)
                        };
                    }
                    *existing = candidate;
                }
                *best = best.max(score);
            }
            None => scored.push((score, candidate)),
        }
    }
    scored.sort_by(|(a, x), (b, y)| {
        b.total_cmp(a)
            .then(x.source.cmp(&y.source))
            .then(x.label.len().cmp(&y.label.len()))
            .then(x.label.cmp(&y.label))
    });
    scored.into_iter().take(limit).map(|(_, c)| c).collect()
}

/// Words of three characters or more in `text` that match `prefix`, with
/// how often each occurs. `skip` is where the word being typed sits, so it
/// does not suggest itself.
pub fn words(
    text: &str,
    prefix: &str,
    skip: Option<std::ops::Range<usize>>,
) -> Vec<(String, usize)> {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut start = None;
    for (at, c) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        match (is_word_char(c), start) {
            (true, None) => start = Some(at),
            (false, Some(from)) => {
                start = None;
                let word = &text[from..at];
                if skip.as_ref().is_some_and(|s| from < s.end && at > s.start) {
                    continue;
                }
                if word.chars().count() < 3
                    || word.chars().next().is_some_and(|c| c.is_ascii_digit())
                {
                    continue;
                }
                if prefix.is_empty() || match_score(word, prefix).is_some_and(|s| s >= 0.85) {
                    *counts.entry(word).or_default() += 1;
                }
            }
            _ => {}
        }
    }
    let mut out: Vec<(String, usize)> =
        counts.into_iter().map(|(w, n)| (w.to_owned(), n)).collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out.truncate(200);
    out
}

/// Files whose every line is a path pattern, relative to their folder.
pub fn is_ignore_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| {
            matches!(
                name,
                ".gitignore"
                    | ".dockerignore"
                    | ".npmignore"
                    | ".prettierignore"
                    | ".eslintignore"
                    | ".ignore"
            ) || path.ends_with(".git/info/exclude")
        })
}

/// A path being typed: the folder to list, and the part of the last
/// segment typed so far.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathQuery {
    pub dir: PathBuf,
    pub partial: String,
    /// Typed into an ignore file: hidden and ignored entries are the point.
    pub ignore_file: bool,
}

/// Whether the text before the caret is a path, and where it points.
///
/// In an ignore file, every line is one, relative to the file's folder. In
/// anything else it has to look like one: `./` or `../`, or a slash inside
/// quotes or a Markdown link, relative to the file or the project.
pub fn path_query(
    line_before: &str,
    file: Option<&Path>,
    root: Option<&Path>,
) -> Option<PathQuery> {
    let file_dir = file.and_then(Path::parent);
    if let Some(file) = file
        && is_ignore_file(file)
    {
        let token = line_before.trim_start();
        if token.starts_with('#') || token.contains(char::is_whitespace) {
            return None;
        }
        let token = token.strip_prefix('!').unwrap_or(token);
        let token = token.strip_prefix('/').unwrap_or(token);
        let base = if file.ends_with(".git/info/exclude") {
            file.parent()?.parent()?.parent()?
        } else {
            file_dir?
        };
        return split(base, token, true);
    }
    let token_start = line_before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace() || "\"'`()<>[]{},;=".contains(*c))
        .map_or(0, |(at, c)| at + c.len_utf8());
    let token = &line_before[token_start..];
    let opener = line_before[..token_start].chars().last();
    if let Some(rest) = token
        .strip_prefix("./")
        .or_else(|| token.strip_prefix("../").map(|_| token))
    {
        let base = file_dir?;
        let relative = if token.starts_with("../") {
            token
        } else {
            rest
        };
        return split(base, relative, false);
    }
    let quoted = matches!(opener, Some('"' | '\'' | '`' | '('));
    if quoted && token.contains('/') && !token.starts_with('/') && !token.contains("://") {
        // Relative to the file if that folder has the first segment,
        // otherwise to the project.
        let first = token.split('/').next().unwrap_or("");
        let base = match file_dir {
            Some(dir) if dir.join(first).exists() => dir,
            _ => root.or(file_dir)?,
        };
        return split(base, token, false);
    }
    None
}

fn split(base: &Path, token: &str, ignore_file: bool) -> Option<PathQuery> {
    let (dir, partial) = match token.rfind('/') {
        Some(at) => (&token[..at], &token[at + 1..]),
        None => ("", token),
    };
    // Glob characters mean a pattern, not a path to list.
    if dir.contains(['*', '?', '[']) {
        return None;
    }
    Some(PathQuery {
        dir: base.join(dir),
        partial: partial.to_owned(),
        ignore_file,
    })
}

/// What the folder of `query` holds that starts like `query.partial`,
/// folders first, each with a trailing slash.
pub fn path_candidates(query: &PathQuery, limit: usize) -> Vec<Candidate> {
    let Ok(entries) = std::fs::read_dir(&query.dir) else {
        return Vec::new();
    };
    let lower = query.partial.to_lowercase();
    let mut out: Vec<(bool, Candidate)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" || name == ".DS_Store" {
            continue;
        }
        if name.starts_with('.') && !query.partial.starts_with('.') && !query.ignore_file {
            continue;
        }
        if !name.to_lowercase().starts_with(&lower) {
            continue;
        }
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        let label = if is_dir { format!("{name}/") } else { name };
        let why = if is_dir {
            "folder".to_owned()
        } else {
            let size = entry.metadata().map_or(0, |m| m.len());
            format!("file · {}", human_bytes(size))
        };
        out.push((
            is_dir,
            Candidate {
                insert: label.clone(),
                label,
                source: Source::Path,
                why,
                weight: 1.0,
                server: None,
            },
        ));
    }
    out.sort_by(|(a, x), (b, y)| {
        b.cmp(a)
            .then(x.label.to_lowercase().cmp(&y.label.to_lowercase()))
    });
    out.into_iter().take(limit).map(|(_, c)| c).collect()
}

fn human_bytes(n: u64) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.1} KB", n as f64 / 1024.0),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(label: &str, source: Source) -> Candidate {
        Candidate {
            label: label.into(),
            insert: label.into(),
            source,
            why: String::new(),
            weight: 0.5,
            server: None,
        }
    }

    #[test]
    fn the_context_is_the_receiver_and_its_separator() {
        assert_eq!(context_before("let x = state.tree."), "tree.");
        assert_eq!(context_before("Path::"), "Path::");
        assert_eq!(context_before("node->"), "node->");
        assert_eq!(context_before("let x = "), "");
        assert_eq!(context_before("(1)."), "");
    }

    #[test]
    fn prefixes_beat_case_which_beats_scattered_letters() {
        assert_eq!(match_score("root", "ro"), Some(1.0));
        assert_eq!(match_score("Root", "ro"), Some(0.85));
        assert!(match_score("to_string_lossy", "tsl").unwrap() < 0.85);
        assert_eq!(
            match_score("string", "tsl"),
            None,
            "first letter must match"
        );
        assert_eq!(match_score("abc", "abd"), None);
    }

    #[test]
    fn one_entry_per_name_keeping_the_servers_edit() {
        let mut server = candidate("root", Source::Server);
        server.server = Some(3);
        let mut symbol = candidate("root", Source::Symbol);
        symbol.why = "fn in src/tree.rs:98".into();
        let ranked = rank(
            "ro",
            vec![symbol, server, candidate("rows", Source::Word)],
            &[],
            10,
        );
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].label, "root");
        assert_eq!(ranked[0].server, Some(3));
    }

    #[test]
    fn a_definition_speaks_for_a_name_that_is_also_a_word() {
        let mut word = candidate("harvest", Source::Word);
        word.why = "used 5× in this file".into();
        word.weight = 0.9;
        let mut symbol = candidate("harvest", Source::Symbol);
        symbol.why = "function in src/main.rs:64".into();
        for order in [vec![word.clone(), symbol.clone()], vec![symbol, word]] {
            let ranked = rank("harv", order, &[], 10);
            assert_eq!(ranked.len(), 1);
            assert_eq!(
                (ranked[0].source, ranked[0].why.as_str()),
                (Source::Symbol, "function in src/main.rs:64")
            );
        }
    }

    #[test]
    fn history_lifts_what_you_picked_most_after_the_same_receiver() {
        let boosts = [Boost {
            text: "rows".into(),
            count: 9,
            age_days: 1.0,
            same_context: true,
            context: "tree.".into(),
        }];
        let ranked = rank(
            "r",
            vec![
                candidate("root", Source::Server),
                candidate("rows", Source::Word),
            ],
            &boosts,
            10,
        );
        assert_eq!(ranked[0].label, "rows");
        assert_eq!(ranked[0].why, "you picked this 9× after `tree.`");
        let ranked = rank("r", vec![candidate("root", Source::Server)], &[], 10);
        assert_eq!(ranked[0].why, "");
    }

    #[test]
    fn what_was_typed_is_not_offered_back() {
        assert!(rank("root", vec![candidate("root", Source::Word)], &[], 10).is_empty());
    }

    #[test]
    fn words_are_counted_and_the_one_being_typed_is_skipped() {
        let text = "alpha alphabet alpha al beta alp";
        let found = words(text, "alp", Some(29..32));
        assert_eq!(found, vec![("alpha".into(), 2), ("alphabet".into(), 1)]);
        assert!(
            words("x1 12345 ab", "", None).is_empty(),
            "short words and numbers are not words"
        );
    }

    #[test]
    fn a_path_is_recognised_where_one_can_be() {
        let dir = std::env::temp_dir().join(format!("crc-complete-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("site/public")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("site/package.json"), "{}").unwrap();
        std::fs::write(dir.join("site/.gitignore"), "").unwrap();
        let ignore = dir.join("site/.gitignore");

        let q = path_query("/pu", Some(&ignore), Some(&dir)).unwrap();
        assert_eq!(
            (q.dir.clone(), q.partial.as_str(), q.ignore_file),
            (dir.join("site"), "pu", true)
        );
        let found = path_candidates(&q, 10);
        assert_eq!(
            found.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(),
            ["public/"]
        );
        let all = path_candidates(&path_query("", Some(&ignore), None).unwrap(), 10);
        assert_eq!(
            all.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(),
            ["public/", ".gitignore", "package.json"],
            "folders first; hidden files count in an ignore file"
        );
        assert!(path_query("# a comment", Some(&ignore), None).is_none());
        assert!(
            path_query("*.lo", Some(&ignore), None).is_some(),
            "a pattern still lists names"
        );

        let main = dir.join("src/main.ts");
        let q = path_query("import x from './", Some(&main), Some(&dir)).unwrap();
        assert_eq!(q.dir, dir.join("src"));
        let q = path_query("let p = \"site/pa", Some(&main), Some(&dir)).unwrap();
        assert_eq!(
            (q.dir.clone(), q.partial.as_str()),
            (dir.join("site"), "pa")
        );
        assert!(
            path_query("let x = a/b", Some(&main), Some(&dir)).is_none(),
            "division is not a path"
        );
        assert!(path_query("see \"https://x.y/z", Some(&main), Some(&dir)).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
