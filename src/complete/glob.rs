//! One `.gitignore` line, matched the way Git matches it, for the preview:
//! the Explorer shows what a line would ignore while it is being typed,
//! before the file is saved and Git is asked for real.
//!
//! Covers what people write: `*`, `?`, `**`, `[abc]` and `[a-z]`, a
//! trailing `/` for folders only, and a slash anywhere but the end
//! anchoring the pattern to the file's folder. A negated line (`!keep`)
//! previews nothing, since it can only take away.

/// A parsed line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    glob: String,
    dir_only: bool,
    anchored: bool,
}

/// The pattern on `line`, or `None` for a blank line, a comment or a
/// negation.
pub fn parse(line: &str) -> Option<Pattern> {
    let line = line.trim_end_matches(['\r', '\n']);
    let line = trim_unescaped_spaces(line);
    if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
        return None;
    }
    let line = line.strip_prefix('\\').unwrap_or(line);
    let (line, dir_only) = match line.strip_suffix('/') {
        Some(rest) => (rest, true),
        None => (line, false),
    };
    let anchored = line.contains('/');
    let glob = line.strip_prefix('/').unwrap_or(line).to_owned();
    if glob.is_empty() {
        return None;
    }
    Some(Pattern {
        glob,
        dir_only,
        anchored,
    })
}

fn trim_unescaped_spaces(line: &str) -> &str {
    let mut end = line.len();
    while end > 0
        && line.as_bytes()[end - 1] == b' '
        && !(end >= 2 && line.as_bytes()[end - 2] == b'\\')
    {
        end -= 1;
    }
    &line[..end]
}

impl Pattern {
    /// Whether this line ignores `relative` (slash-separated, relative to
    /// the ignore file's folder) on its own. Paths inside a matched folder
    /// are not matched here; the Explorer dims those through their parent.
    pub fn matches(&self, relative: &str, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        if self.anchored {
            return glob(self.glob.as_bytes(), relative.as_bytes());
        }
        let name = relative.rsplit('/').next().unwrap_or(relative);
        glob(self.glob.as_bytes(), name.as_bytes())
    }
}

/// Wildcard match. `*` and `?` stop at `/`; `**` crosses it.
fn glob(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.first() {
        None => text.is_empty(),
        Some(b'*') if pattern.get(1) == Some(&b'*') => {
            // `**/` also matches nothing at all: `**/foo` is `foo` too.
            let rest = &pattern[2..];
            let rest_after_slash = rest.strip_prefix(b"/").unwrap_or(rest);
            if glob(rest_after_slash, text) {
                return true;
            }
            (0..text.len())
                .any(|i| glob(rest, &text[i + 1..]) || glob(rest_after_slash, &text[i + 1..]))
        }
        Some(b'*') => {
            let rest = &pattern[1..];
            let mut i = 0;
            loop {
                if glob(rest, &text[i..]) {
                    return true;
                }
                if i == text.len() || text[i] == b'/' {
                    return false;
                }
                i += 1;
            }
        }
        Some(b'?') => text.first().is_some_and(|&c| c != b'/') && glob(&pattern[1..], &text[1..]),
        Some(b'[') => {
            let Some(close) = pattern
                .iter()
                .skip(1)
                .position(|&c| c == b']')
                .map(|p| p + 1)
            else {
                return text.first() == Some(&b'[') && glob(&pattern[1..], &text[1..]);
            };
            let Some(&c) = text.first() else {
                return false;
            };
            let set = &pattern[1..close];
            let (negate, set) = match set.first() {
                Some(b'!' | b'^') => (true, &set[1..]),
                _ => (false, set),
            };
            let mut hit = false;
            let mut i = 0;
            while i < set.len() {
                if i + 2 < set.len() && set[i + 1] == b'-' {
                    hit |= (set[i]..=set[i + 2]).contains(&c);
                    i += 3;
                } else {
                    hit |= set[i] == c;
                    i += 1;
                }
            }
            hit != negate && c != b'/' && glob(&pattern[close + 1..], &text[1..])
        }
        Some(&p) => text.first() == Some(&p) && glob(&pattern[1..], &text[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(line: &str, path: &str, is_dir: bool) -> bool {
        parse(line).is_some_and(|p| p.matches(path, is_dir))
    }

    #[test]
    fn names_match_at_any_depth_and_slashes_anchor() {
        assert!(m("dist", "dist", true));
        assert!(m("dist", "site/dist", true));
        assert!(m("/dist", "dist", true));
        assert!(!m("/dist", "site/dist", true), "a leading slash anchors");
        assert!(m("site/dist", "site/dist", true));
        assert!(
            !m("site/dist", "x/site/dist", true),
            "a middle slash anchors too"
        );
    }

    #[test]
    fn a_trailing_slash_is_for_folders_only() {
        assert!(m("public/", "public", true));
        assert!(!m("public/", "public", false));
    }

    #[test]
    fn wildcards_behave_like_git() {
        assert!(m("*.log", "debug.log", false));
        assert!(m("*.log", "logs/debug.log", false));
        assert!(
            !m("/*.log", "logs/debug.log", false),
            "* does not cross a slash"
        );
        assert!(m("**/cache", "a/b/cache", true));
        assert!(m("**/cache", "cache", true));
        assert!(m("a/**/z", "a/z", false));
        assert!(m("a/**/z", "a/b/c/z", false));
        assert!(m("file?.txt", "file1.txt", false));
        assert!(m("[ab].rs", "a.rs", false));
        assert!(!m("[!ab].rs", "a.rs", false));
        assert!(m("v[0-9]", "v7", false));
    }

    #[test]
    fn comments_blanks_and_negations_preview_nothing() {
        assert_eq!(parse("# dist"), None);
        assert_eq!(parse("   "), None);
        assert_eq!(parse("!keep.log"), None);
        assert!(m("\\#literal", "#literal", false));
        assert!(m("trailing   ", "trailing", false));
    }
}
