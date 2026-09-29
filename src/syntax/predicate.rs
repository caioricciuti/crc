//! Query predicates, and the tiny pattern matcher `#match?` needs.
//!
//! tree-sitter does not evaluate predicates for you: `ts_query_cursor_next_match`
//! returns every match and the client is expected to filter. Skipping that
//! step does not under-colour, it *over*-colours, because a pattern gated on
//! `#match?` then fires unconditionally. In the Rust grammar that meant every
//! identifier was captured as `@constant` *and* `@constructor` *and* `@type`,
//! and which colour won came down to sort order.
//!
//! `#match?` takes a regex. Rather than add a regex engine for six patterns,
//! this implements the subset that highlight queries actually use: anchors,
//! character classes with ranges and `\d`/`\w`/`\s`, `+`, `*`, `?`, `.`, and
//! literals. A pattern using anything else is reported as unsupported, and an
//! unsupported predicate makes its whole pattern inert rather than
//! unconditionally true. Under-colouring is a missing highlight; over-colouring
//! is a wrong one.

use super::ffi;

/// One parsed predicate on a query pattern.
#[derive(Debug, Clone)]
pub enum Predicate {
    /// `(#match? @capture "regex")`
    Match {
        capture: u32,
        pattern: Pattern,
        negated: bool,
    },
    /// `(#eq? @capture "literal")`
    EqString {
        capture: u32,
        value: String,
        negated: bool,
    },
    /// `(#any-of? @capture "a" "b")`
    AnyOf { capture: u32, values: Vec<String> },
    /// A predicate this implementation does not understand. Its presence
    /// disables the pattern.
    Unsupported,
}

impl Predicate {
    /// The capture this predicate is about; `None` for one not understood,
    /// which no match satisfies.
    pub fn capture(&self) -> Option<u32> {
        match self {
            Predicate::Match { capture, .. }
            | Predicate::EqString { capture, .. }
            | Predicate::AnyOf { capture, .. } => Some(*capture),
            Predicate::Unsupported => None,
        }
    }

    /// Whether `text`, captured by `capture`, satisfies this predicate.
    /// Predicates about other captures do not constrain this one.
    pub fn accepts(&self, capture: u32, text: &str) -> bool {
        match self {
            Predicate::Match {
                capture: c,
                pattern,
                negated,
            } => *c != capture || (pattern.matches(text) != *negated),
            Predicate::EqString {
                capture: c,
                value,
                negated,
            } => *c != capture || ((text == value) != *negated),
            Predicate::AnyOf { capture: c, values } => {
                *c != capture || values.iter().any(|v| v == text)
            }
            Predicate::Unsupported => false,
        }
    }
}

/// A compiled subset-regex.
#[derive(Debug, Clone)]
pub struct Pattern {
    anchored_start: bool,
    anchored_end: bool,
    items: Vec<Item>,
}

#[derive(Debug, Clone)]
struct Item {
    class: Class,
    repeat: Repeat,
    /// `+?`, `*?`, `??`: as few as will do.
    lazy: bool,
}

#[derive(Debug, Clone)]
enum Class {
    Any,
    Literal(char),
    /// `\w`: a letter or digit of any script, or `_`.
    Word,
    /// Ranges plus negation, e.g. `[^A-Za-z_]`.
    Set {
        ranges: Vec<(char, char)>,
        negated: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Repeat {
    One,
    ZeroOrMore,
    OneOrMore,
    ZeroOrOne,
}

impl Pattern {
    /// Compiles `source`, or `None` if it uses anything outside the subset.
    pub fn compile(source: &str) -> Option<Pattern> {
        let chars: Vec<char> = source.chars().collect();
        let mut i = 0;
        let anchored_start = chars.first() == Some(&'^');
        if anchored_start {
            i += 1;
        }
        let anchored_end = chars.last() == Some(&'$') && chars.len() > i;
        let end = if anchored_end {
            chars.len() - 1
        } else {
            chars.len()
        };

        let mut items = Vec::new();
        while i < end {
            let class = match chars[i] {
                '[' => {
                    let (set, next) = parse_set(&chars, i, end)?;
                    i = next;
                    set
                }
                '\\' => {
                    i += 1;
                    if i >= end {
                        return None;
                    }
                    let c = chars[i];
                    i += 1;
                    escape_class(c)?
                }
                '.' => {
                    i += 1;
                    Class::Any
                }
                // Alternation, groups and counted repeats are outside the
                // subset. Refuse rather than mis-compile.
                '(' | ')' | '|' | '{' | '}' => return None,
                // A quantifier with nothing before it to repeat.
                '+' | '*' | '?' => return None,
                c => {
                    i += 1;
                    Class::Literal(c)
                }
            };

            let repeat = match chars.get(i) {
                Some('+') => {
                    i += 1;
                    Repeat::OneOrMore
                }
                Some('*') => {
                    i += 1;
                    Repeat::ZeroOrMore
                }
                Some('?') => {
                    i += 1;
                    Repeat::ZeroOrOne
                }
                _ => Repeat::One,
            };
            let lazy = repeat != Repeat::One && chars.get(i) == Some(&'?') && i < end;
            if lazy {
                i += 1;
            }
            items.push(Item {
                class,
                repeat,
                lazy,
            });
        }
        Some(Pattern {
            anchored_start,
            anchored_end,
            items,
        })
    }

    pub fn matches(&self, text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        // Failed (items left, position) pairs: each is tried once, so
        // optional items cannot make the backtracking exponential.
        let mut failed = std::collections::HashSet::new();
        if self.anchored_start {
            return self.match_items(&self.items, &chars, 0, &mut failed);
        }
        (0..=chars.len()).any(|start| self.match_items(&self.items, &chars, start, &mut failed))
    }

    fn match_items(
        &self,
        items: &[Item],
        chars: &[char],
        at: usize,
        failed: &mut std::collections::HashSet<(usize, usize)>,
    ) -> bool {
        let Some((item, rest)) = items.split_first() else {
            return !self.anchored_end || at == chars.len();
        };
        if failed.contains(&(items.len(), at)) {
            return false;
        }

        let (min, max) = match item.repeat {
            Repeat::One => (1, 1),
            Repeat::OneOrMore => (1, usize::MAX),
            Repeat::ZeroOrMore => (0, usize::MAX),
            Repeat::ZeroOrOne => (0, 1),
        };

        let mut most = 0;
        while most < max && at + most < chars.len() && item.class.matches(chars[at + most]) {
            most += 1;
        }
        if most >= min {
            // Greedy gives back one at a time; lazy takes one more at a time.
            let counts: Box<dyn Iterator<Item = usize>> = if item.lazy {
                Box::new(min..=most)
            } else {
                Box::new((min..=most).rev())
            };
            for taken in counts {
                if self.match_items(rest, chars, at + taken, failed) {
                    return true;
                }
            }
        }
        failed.insert((items.len(), at));
        false
    }
}

impl Class {
    fn matches(&self, c: char) -> bool {
        match self {
            Class::Any => true,
            Class::Literal(l) => *l == c,
            Class::Word => c.is_alphanumeric() || c == '_',
            Class::Set { ranges, negated } => {
                // A set of the ASCII letters also takes the letters of other
                // scripts in the same case: highlight queries say `^[A-Z]`
                // for "a type's name", and `Über` or `Σύνολο` is one too.
                let inside = ranges.iter().any(|(lo, hi)| c >= *lo && c <= *hi)
                    || (!c.is_ascii()
                        && ((c.is_uppercase() && ranges.contains(&('A', 'Z')))
                            || (c.is_lowercase() && ranges.contains(&('a', 'z')))));
                inside != *negated
            }
        }
    }
}

fn escape_class(c: char) -> Option<Class> {
    Some(match c {
        'd' => Class::Set {
            ranges: vec![('0', '9')],
            negated: false,
        },
        'w' => Class::Word,
        's' => Class::Set {
            ranges: vec![(' ', ' '), ('\t', '\t'), ('\n', '\n'), ('\r', '\r')],
            negated: false,
        },
        // A literal escape of a metacharacter.
        '.' | '\\' | '[' | ']' | '(' | ')' | '+' | '*' | '?' | '^' | '$' | '|' | '{' | '}'
        | '/' | '-' => Class::Literal(c),
        _ => return None,
    })
}

/// Parses `[...]` starting at `open`, returning the class and the index past
/// the closing bracket.
fn parse_set(chars: &[char], open: usize, end: usize) -> Option<(Class, usize)> {
    let mut i = open + 1;
    let negated = chars.get(i) == Some(&'^');
    if negated {
        i += 1;
    }
    let mut ranges = Vec::new();
    while i < end && chars[i] != ']' {
        let lo = if chars[i] == '\\' {
            i += 1;
            let c = *chars.get(i)?;
            i += 1;
            match escape_class(c)? {
                Class::Literal(l) => l,
                // A shorthand class inside a set contributes its own ranges.
                Class::Set { ranges: r, .. } => {
                    ranges.extend(r);
                    continue;
                }
                // `\w` in a set: its ASCII ranges, which the set widens to
                // other scripts' letters as it does for `A-Z` and `a-z`.
                Class::Word => {
                    ranges.extend([('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')]);
                    continue;
                }
                Class::Any => return None,
            }
        } else {
            let c = chars[i];
            i += 1;
            c
        };

        if chars.get(i) == Some(&'-') && chars.get(i + 1).is_some_and(|c| *c != ']') {
            let hi = chars[i + 1];
            i += 2;
            ranges.push((lo, hi));
        } else {
            ranges.push((lo, lo));
        }
    }
    if chars.get(i) != Some(&']') {
        return None;
    }
    Some((Class::Set { ranges, negated }, i + 1))
}

/// Reads each pattern's predicates out of a compiled query.
///
/// A predicate this implementation cannot represent becomes
/// [`Predicate::Unsupported`], which rejects everything, so the pattern goes
/// inert. That is the safe direction: a missing highlight is invisible, a
/// wrong one is not.
pub(super) fn load_predicates(query: *const ffi::TSQuery) -> Vec<Vec<Predicate>> {
    // SAFETY: `query` is a live query for the life of the Highlighter.
    let count = unsafe { ffi::ts_query_pattern_count(query) };
    let mut out = Vec::with_capacity(count as usize);

    for pattern in 0..count {
        let mut step_count = 0u32;
        let steps =
            unsafe { ffi::ts_query_predicates_for_pattern(query, pattern, &mut step_count) };
        if steps.is_null() || step_count == 0 {
            out.push(Vec::new());
            continue;
        }
        let steps = unsafe { std::slice::from_raw_parts(steps, step_count as usize) };

        let string_at = |id: u32| -> String {
            let mut len = 0u32;
            let ptr = unsafe { ffi::ts_query_string_value_for_id(query, id, &mut len) };
            // SAFETY: a pointer and length from the live query.
            String::from_utf8_lossy(unsafe { ffi::query_bytes(ptr, len) }).into_owned()
        };

        // Steps come as a flat list of runs terminated by `Done`. Each run is
        // the predicate name followed by its arguments.
        let mut predicates = Vec::new();
        let mut run: Vec<(u32, u32)> = Vec::new();
        for step in steps {
            if step.kind == ffi::TSQueryPredicateStep::DONE {
                if !run.is_empty() {
                    predicates.extend(build_predicate(&run, &string_at));
                    run.clear();
                }
                continue;
            }
            run.push((step.kind, step.value_id));
        }
        if !run.is_empty() {
            predicates.extend(build_predicate(&run, &string_at));
        }
        out.push(predicates);
    }
    out
}

/// The words of a pattern of the exact shape `^(word|word|...)$`.
fn literal_alternatives(pattern: &str) -> Option<Vec<String>> {
    let inner = pattern.strip_prefix("^(")?.strip_suffix(")$")?;
    let words: Vec<String> = inner.split('|').map(str::to_string).collect();
    let plain = |w: &String| {
        !w.is_empty()
            && w.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    words.iter().all(plain).then_some(words)
}

/// Turns one predicate run into a [`Predicate`].
fn build_predicate(run: &[(u32, u32)], string_at: &dyn Fn(u32) -> String) -> Option<Predicate> {
    const CAPTURE: u32 = ffi::TSQueryPredicateStep::CAPTURE;
    const STRING: u32 = ffi::TSQueryPredicateStep::STRING;

    let Some(&(kind, name_id)) = run.first() else {
        return Some(Predicate::Unsupported);
    };
    if kind != STRING {
        return Some(Predicate::Unsupported);
    }
    let name = string_at(name_id);
    let args = &run[1..];

    Some(match name.as_str() {
        "match?" | "not-match?" => {
            let [(CAPTURE, capture), (STRING, pattern_id)] = args[..] else {
                return Some(Predicate::Unsupported);
            };
            let source = string_at(pattern_id);
            // `^(a|b|c)$` is how queries spell "one of these words", and it
            // is the one use of alternation in the grammars vendored here.
            // It needs no regex engine: it is `any-of?` with more typing.
            if let Some(values) = literal_alternatives(&source).filter(|_| name == "match?") {
                return Some(Predicate::AnyOf { capture, values });
            }
            match Pattern::compile(&source) {
                Some(pattern) => Predicate::Match {
                    capture,
                    pattern,
                    negated: name.starts_with("not-"),
                },
                None => Predicate::Unsupported,
            }
        }
        "eq?" | "not-eq?" => {
            let [(CAPTURE, capture), (STRING, value_id)] = args[..] else {
                // Capture-to-capture equality needs both texts at once, which
                // this pass does not carry.
                return Some(Predicate::Unsupported);
            };
            Predicate::EqString {
                capture,
                value: string_at(value_id),
                negated: name.starts_with("not-"),
            }
        }
        "any-of?" => {
            let Some(&(CAPTURE, capture)) = args.first() else {
                return Some(Predicate::Unsupported);
            };
            let values = args[1..]
                .iter()
                .filter(|(k, _)| *k == STRING)
                .map(|(_, id)| string_at(*id))
                .collect();
            Predicate::AnyOf { capture, values }
        }
        // `set!`, `is?` and friends are directives rather than filters; they
        // do not constrain a match, so they are no predicate at all.
        "set!" | "is?" | "is-not?" => return None,
        _ => Predicate::Unsupported,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, text: &str) -> bool {
        Pattern::compile(pattern)
            .unwrap_or_else(|| panic!("should compile: {pattern}"))
            .matches(text)
    }

    /// The two patterns the vendored Rust grammar actually uses.
    #[test]
    fn lazy_unicode_and_many_optional_items() {
        let lazy = Pattern::compile("^a+?b$").unwrap();
        assert!(lazy.matches("aaab"));
        assert!(Pattern::compile("*a").is_none(), "nothing to repeat");
        let types = Pattern::compile("^[A-Z]").unwrap();
        assert!(types.matches("Über") && types.matches("Σύνολο") && !types.matches("über"));
        assert!(Pattern::compile("^\\w+$").unwrap().matches("naïve_日本"));
        let optional = Pattern::compile(&"a?".repeat(30)).unwrap();
        let started = std::time::Instant::now();
        assert!(
            !Pattern::compile(&format!("^{}b$", "a?".repeat(30)))
                .unwrap()
                .matches(&"a".repeat(30))
        );
        assert!(optional.matches("x"));
        assert!(started.elapsed() < std::time::Duration::from_millis(200));
    }

    #[test]
    fn matches_the_grammars_own_patterns() {
        assert!(m("^[A-Z]", "Mixed"));
        assert!(m("^[A-Z]", "CONST"));
        assert!(!m("^[A-Z]", "lowercase"));

        assert!(m("^[A-Z][A-Z\\d_]+$", "MAX_SIZE"));
        assert!(m("^[A-Z][A-Z\\d_]+$", "E1"));
        assert!(!m("^[A-Z][A-Z\\d_]+$", "Mixed"));
        assert!(!m("^[A-Z][A-Z\\d_]+$", "lowercase"));
        assert!(
            !m("^[A-Z][A-Z\\d_]+$", "A"),
            "needs at least two characters"
        );
    }

    #[test]
    fn anchors_are_honoured() {
        assert!(m("^abc", "abcdef"));
        assert!(!m("^abc", "xabcdef"));
        assert!(m("abc$", "xxabc"));
        assert!(!m("abc$", "abcxx"));
        assert!(m("^abc$", "abc"));
        assert!(!m("^abc$", "abcd"));
    }

    #[test]
    fn unanchored_patterns_search() {
        assert!(m("bc", "abcd"));
        assert!(!m("zz", "abcd"));
    }

    #[test]
    fn repeats_work() {
        assert!(m("^a*b$", "b"));
        assert!(m("^a*b$", "aaab"));
        assert!(m("^a+b$", "ab"));
        assert!(!m("^a+b$", "b"));
        assert!(m("^ab?c$", "ac"));
        assert!(m("^ab?c$", "abc"));
    }

    #[test]
    fn negated_and_shorthand_classes() {
        assert!(m("^[^0-9]+$", "abc"));
        assert!(!m("^[^0-9]+$", "ab1"));
        assert!(m("^\\w+$", "a_1"));
        assert!(!m("^\\d+$", "12a"));
    }

    /// Anything outside the subset must refuse to compile, so the caller can
    /// disable the pattern instead of guessing.
    #[test]
    fn unsupported_syntax_is_refused_not_guessed() {
        for src in ["(a|b)", "a{2,3}", "a|b", "\\Qliteral\\E"] {
            assert!(
                Pattern::compile(src).is_none(),
                "{src:?} should be refused rather than mis-compiled"
            );
        }
    }

    #[test]
    fn predicates_only_constrain_their_own_capture() {
        let p = Predicate::Match {
            capture: 3,
            pattern: Pattern::compile("^[A-Z]").expect("compiles"),
            negated: false,
        };
        assert!(p.accepts(3, "Upper"));
        assert!(!p.accepts(3, "lower"));
        assert!(
            p.accepts(9, "lower"),
            "a different capture is unconstrained"
        );
    }

    #[test]
    fn an_unsupported_predicate_rejects_everything() {
        assert!(!Predicate::Unsupported.accepts(0, "anything"));
    }
}
