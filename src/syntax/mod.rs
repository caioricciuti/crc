//! Syntax highlighting: parse with tree-sitter, colour with a query.
//!
//! The parse runs against a [`Rope`] read through a callback, so a 100MB
//! buffer is never copied into a contiguous `String` just to be parsed. That
//! is the whole reason the rope exposes chunk iteration.
//!
//! Highlighting is a two-step: tree-sitter produces a parse tree, then a
//! capture query maps node patterns to names like `keyword` or `string`,
//! which a theme turns into colours. The query is the grammar's own
//! `highlights.scm`, checked in beside the grammar.
//!
//! Only the visible byte range is queried each frame. Highlighting an entire
//! 100MB file to draw sixty lines of it would undo the point of everything
//! below this module.

pub mod defs;
mod ffi;
mod predicate;

use std::ffi::{CString, c_char, c_void};
use std::ptr::NonNull;

use crate::syntax::predicate::{Pattern, Predicate};
use crate::text::rope::Rope;

/// What a captured node means, once the capture name has been classified.
///
/// A small closed set rather than free-form strings: the theme has to have a
/// colour for every one of them, and an enum makes that a compile error
/// rather than a silently-black glyph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Keyword,
    Function,
    Type,
    String,
    Number,
    Comment,
    Constant,
    Attribute,
    Operator,
    Punctuation,
    Variable,
    Property,
}

impl Kind {
    /// Maps a tree-sitter capture name onto a highlight kind.
    ///
    /// Capture names are dotted and hierarchical (`function.method`,
    /// `string.escape`), so this matches on the most specific prefix it
    /// recognises and falls back to the general one.
    fn from_capture(name: &str) -> Option<Kind> {
        let head = name.split('.').next().unwrap_or(name);
        Some(match head {
            "keyword" => Kind::Keyword,
            // Markup tags take the function colour: the name that says what
            // a thing is, set apart from its attributes and their values.
            "function" | "method" | "tag" => Kind::Function,
            "type" | "constructor" | "namespace" => Kind::Type,
            "string" | "character" => Kind::String,
            "number" | "float" | "integer" => Kind::Number,
            "comment" => Kind::Comment,
            "constant" | "boolean" | "escape" => Kind::Constant,
            "attribute" | "label" => Kind::Attribute,
            "operator" => Kind::Operator,
            "punctuation" | "delimiter" => Kind::Punctuation,
            "variable" => Kind::Variable,
            "property" | "field" => Kind::Property,
            _ => return None,
        })
    }
}

/// A coloured byte range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
}

/// Languages with a compiled-in grammar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Html,
    JavaScript,
    Css,
    Json,
    TypeScript,
    Tsx,
    Python,
    C,
    Cpp,
    Go,
    Toml,
    Yaml,
    Bash,
}

impl Language {
    pub const ALL: [Language; 14] = [
        Language::Rust,
        Language::Html,
        Language::JavaScript,
        Language::Css,
        Language::Json,
        Language::TypeScript,
        Language::Tsx,
        Language::Python,
        Language::C,
        Language::Cpp,
        Language::Go,
        Language::Toml,
        Language::Yaml,
        Language::Bash,
    ];

    /// Picks a language from a file extension, or `None` for plain text.
    pub fn from_path(path: &std::path::Path) -> Option<Language> {
        Language::from_extension(&path.extension()?.to_str()?.to_ascii_lowercase())
    }

    /// Picks a language from a lowercase extension without the dot.
    pub fn from_extension(extension: &str) -> Option<Language> {
        match extension {
            "rs" => Some(Language::Rust),
            // Svelte and Vue are HTML with script and style blocks, which the
            // HTML grammar's injections already colour. `{#if}` and the like
            // stay plain text.
            "html" | "htm" | "svelte" | "vue" => Some(Language::Html),
            // The JavaScript grammar parses JSX as well.
            "js" | "mjs" | "cjs" | "jsx" => Some(Language::JavaScript),
            "css" => Some(Language::Css),
            "json" => Some(Language::Json),
            "ts" | "mts" | "cts" => Some(Language::TypeScript),
            "tsx" => Some(Language::Tsx),
            "py" | "pyw" => Some(Language::Python),
            "c" | "h" => Some(Language::C),
            "cc" | "cpp" | "cxx" | "c++" | "hh" | "hpp" | "hxx" => Some(Language::Cpp),
            "go" => Some(Language::Go),
            "toml" => Some(Language::Toml),
            "yml" | "yaml" => Some(Language::Yaml),
            // fish is its own language; zsh is close enough to bash for
            // colouring.
            "sh" | "bash" | "zsh" => Some(Language::Bash),
            _ => None,
        }
    }

    /// The line-comment token, for Cmd-/. `None` where the language has only
    /// block comments, or none at all.
    pub fn line_comment(self) -> Option<&'static str> {
        match self {
            Language::Rust
            | Language::JavaScript
            | Language::TypeScript
            | Language::Tsx
            | Language::C
            | Language::Cpp
            | Language::Go => Some("//"),
            Language::Python | Language::Toml | Language::Yaml | Language::Bash => Some("#"),
            Language::Html | Language::Css | Language::Json => None,
        }
    }

    fn raw(self) -> *const ffi::TSLanguage {
        // SAFETY: each is provided by a grammar compiled in build.rs, and
        // only returns a pointer to a static table.
        unsafe {
            match self {
                Language::Rust => ffi::tree_sitter_rust(),
                Language::Html => ffi::tree_sitter_html(),
                Language::JavaScript => ffi::tree_sitter_javascript(),
                Language::Css => ffi::tree_sitter_css(),
                Language::Json => ffi::tree_sitter_json(),
                Language::TypeScript => ffi::tree_sitter_typescript(),
                Language::Tsx => ffi::tree_sitter_tsx(),
                Language::Python => ffi::tree_sitter_python(),
                Language::C => ffi::tree_sitter_c(),
                Language::Cpp => ffi::tree_sitter_cpp(),
                Language::Go => ffi::tree_sitter_go(),
                Language::Toml => ffi::tree_sitter_toml(),
                Language::Yaml => ffi::tree_sitter_yaml(),
                Language::Bash => ffi::tree_sitter_bash(),
            }
        }
    }

    /// The grammar's own highlight queries. The order is part of the meaning,
    /// since it decides which of two patterns capturing the same node wins.
    /// TypeScript's queries only cover what it adds to JavaScript's.
    fn highlights_query(self) -> &'static str {
        macro_rules! queries {
            ($($path:literal),+) => {
                concat!($(include_str!(concat!("../../third_party/", $path)), "\n"),+)
            };
        }
        match self {
            Language::Rust => queries!("tree-sitter-rust/queries/highlights.scm"),
            Language::Html => queries!("tree-sitter-html/queries/highlights.scm"),
            Language::JavaScript => queries!(
                "tree-sitter-javascript/queries/highlights.scm",
                "tree-sitter-javascript/queries/highlights-jsx.scm",
                "tree-sitter-javascript/queries/highlights-params.scm"
            ),
            Language::Css => queries!("tree-sitter-css/queries/highlights.scm"),
            Language::Json => queries!("tree-sitter-json/queries/highlights.scm"),
            Language::Python => queries!("tree-sitter-python/queries/highlights.scm"),
            Language::C => queries!("tree-sitter-c/queries/highlights.scm"),
            Language::Cpp => queries!(
                "tree-sitter-c/queries/highlights.scm",
                "tree-sitter-cpp/queries/highlights.scm"
            ),
            Language::Go => queries!("tree-sitter-go/queries/highlights.scm"),
            Language::Toml => queries!("tree-sitter-toml/queries/highlights.scm"),
            Language::Yaml => queries!("tree-sitter-yaml/queries/highlights.scm"),
            Language::Bash => queries!("tree-sitter-bash/queries/highlights.scm"),
            // Not upstream's order, which lists TypeScript's query first. That
            // order predates the JavaScript query being rewritten so that the
            // later pattern wins (see `later_pattern_wins`). Under that rule a
            // refinement has to come after what it refines, so JavaScript's
            // generic patterns go first and JSX and TypeScript's go after.
            Language::TypeScript => queries!(
                "tree-sitter-javascript/queries/highlights.scm",
                "tree-sitter-typescript/queries/highlights.scm"
            ),
            Language::Tsx => queries!(
                "tree-sitter-javascript/queries/highlights.scm",
                "tree-sitter-javascript/queries/highlights-jsx.scm",
                "tree-sitter-typescript/queries/highlights.scm"
            ),
        }
    }

    /// Which of two patterns capturing the same node wins.
    ///
    /// The grammars do not agree, because the convention changed under them.
    /// The older queries put the specific pattern first and expect the first
    /// match to win: Rust lists `@constant` above `@constructor` above `@type`
    /// for the same identifier, JSON lists a pair's key above `(string)`. The
    /// JavaScript query was rewritten the other way round. It opens with a
    /// bare `(identifier) @variable` and refines it further down, so under
    /// first-wins every identifier in a file is a plain variable: no function
    /// names, no constructors, no constants. TypeScript's own query is from
    /// the older era, but most of what it colours comes from JavaScript's,
    /// which it is followed by.
    fn later_pattern_wins(self) -> bool {
        matches!(
            self,
            Language::JavaScript
                | Language::TypeScript
                | Language::Tsx
                | Language::Python
                | Language::C
                | Language::Cpp
        )
    }

    /// Other languages embedded in this one: a query whose
    /// `@injection.content` captures are the embedded text, and what to parse
    /// it as.
    ///
    /// Upstream says the same thing in `injections.scm` with a
    /// `#set! injection.language` directive. Naming the language here instead
    /// keeps it a closed set checked by the compiler, and an editor with
    /// seven grammars has no use for looking one up by string.
    fn injections(self) -> &'static [(&'static str, Language)] {
        match self {
            Language::Html => &[
                (
                    "(script_element (raw_text) @injection.content)",
                    Language::JavaScript,
                ),
                (
                    "(style_element (raw_text) @injection.content)",
                    Language::Css,
                ),
            ],
            _ => &[],
        }
    }
}

/// Payload handed to the C read callback.
struct ReadState<'a> {
    rope: &'a Rope,
    /// Holds the chunk currently being returned, because the callback hands
    /// C a borrowed pointer that must outlive the call. Bytes, not a
    /// `String`: a fixed-size read ends wherever it ends, often inside a
    /// character, and tree-sitter copes with that by asking again from the
    /// start of the character it could not finish.
    chunk: Vec<u8>,
}

/// Feeds rope text to the parser on demand.
///
/// # Safety
/// `payload` must point to a live `ReadState`, which `Parser::parse`
/// guarantees for the duration of the parse.
unsafe extern "C" fn read_rope(
    payload: *mut c_void,
    byte_index: u32,
    _position: ffi::TSPoint,
    bytes_read: *mut u32,
) -> *const c_char {
    // SAFETY: the pointer came from `&mut ReadState` in `parse` below and is
    // not aliased; tree-sitter calls this synchronously from that frame.
    let state = unsafe { &mut *(payload as *mut ReadState) };
    let at = byte_index as usize;
    let len = state.rope.len_bytes();
    if at >= len {
        unsafe { *bytes_read = 0 };
        return std::ptr::null();
    }

    // One leaf at a time. Returning the whole remainder would defeat the
    // rope; returning a byte at a time would make parsing quadratic in
    // call overhead.
    let end = (at + 4096).min(len);
    state.chunk.clear();
    for piece in state.rope.bytes_in(at..end) {
        state.chunk.extend_from_slice(piece);
    }
    unsafe { *bytes_read = state.chunk.len() as u32 };
    state.chunk.as_ptr() as *const c_char
}

/// An owned parse tree.
pub struct Tree {
    raw: NonNull<ffi::TSTree>,
}

impl Tree {
    /// Whether the parse hit a syntax error anywhere.
    pub fn has_error(&self) -> bool {
        // SAFETY: `raw` is a live tree for the lifetime of `self`.
        unsafe { ffi::ts_node_has_error(ffi::ts_tree_root_node(self.raw.as_ptr())) }
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        // SAFETY: `raw` was produced by ts_parser_parse and is dropped once.
        unsafe { ffi::ts_tree_delete(self.raw.as_ptr()) };
    }
}

// SAFETY: a TSTree is an owned immutable value once parsing finishes. It is
// only read through &self here, and nothing shares interior state with the
// parser that produced it. This is what lets a parse result move to the main
// thread from the background one.
unsafe impl Send for Tree {}

/// A parser bound to one language, with its highlight query compiled.
pub struct Highlighter {
    parser: NonNull<ffi::TSParser>,
    query: NonNull<ffi::TSQuery>,
    /// Kind per capture index, resolved once rather than per match. `None`
    /// for capture names this editor does not colour.
    capture_kinds: Vec<Option<Kind>>,
    /// Predicates per query pattern, resolved once at compile time.
    ///
    /// tree-sitter hands back every match and expects the client to filter.
    /// Not filtering does not lose highlights, it invents them: a pattern
    /// gated on `#match?` fires unconditionally, which made every identifier
    /// in a Rust file capture as constant, constructor *and* type at once.
    pattern_predicates: Vec<Vec<Predicate>>,
    /// Queries that find text written in another language, with which one.
    injections: Vec<(NonNull<ffi::TSQuery>, Language)>,
    pub language: Language,
}

/// Compiles a query, or `None` if it does not fit the grammar.
fn compile_query(language: Language, source: &str) -> Option<NonNull<ffi::TSQuery>> {
    let mut error_offset = 0u32;
    let mut error_type = ffi::TSQueryError::NONE;
    // SAFETY: source is a valid UTF-8 slice with its length passed explicitly;
    // the out-params are live for the call.
    NonNull::new(unsafe {
        ffi::ts_query_new(
            language.raw(),
            source.as_ptr() as *const c_char,
            source.len() as u32,
            &mut error_offset,
            &mut error_type,
        )
    })
}

impl Highlighter {
    /// Builds a highlighter, or `None` if the grammar and query disagree.
    pub fn new(language: Language) -> Option<Highlighter> {
        // SAFETY: ts_parser_new allocates; the pointer is owned from here.
        let parser = NonNull::new(unsafe { ffi::ts_parser_new() })?;
        // SAFETY: both pointers are valid; a false return means the grammar
        // was built against an incompatible ABI.
        if !unsafe { ffi::ts_parser_set_language(parser.as_ptr(), language.raw()) } {
            unsafe { ffi::ts_parser_delete(parser.as_ptr()) };
            return None;
        }

        let Some(query) = compile_query(language, language.highlights_query()) else {
            unsafe { ffi::ts_parser_delete(parser.as_ptr()) };
            return None;
        };

        // Resolve capture names once. A query has a fixed capture list, and
        // doing this per match would mean a string comparison per token.
        // SAFETY: query is live and index is bounded by the reported count.
        let count = unsafe { ffi::ts_query_capture_count(query.as_ptr()) };
        let mut capture_kinds = Vec::with_capacity(count as usize);
        for i in 0..count {
            let mut len = 0u32;
            let ptr = unsafe { ffi::ts_query_capture_name_for_id(query.as_ptr(), i, &mut len) };
            let name = if ptr.is_null() {
                ""
            } else {
                // SAFETY: tree-sitter returns a pointer plus a length into
                // the query's own storage, which outlives this borrow.
                let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
                std::str::from_utf8(bytes).unwrap_or("")
            };
            capture_kinds.push(Kind::from_capture(name));
        }

        let pattern_predicates = load_predicates(query.as_ptr());

        // An injection query that does not compile costs the embedded
        // language its colour and nothing else, so it is skipped, not fatal.
        let injections = language
            .injections()
            .iter()
            .filter_map(|&(source, embedded)| Some((compile_query(language, source)?, embedded)))
            .collect();

        Some(Highlighter {
            parser,
            query,
            capture_kinds,
            pattern_predicates,
            injections,
            language,
        })
    }

    /// Where other languages are embedded in `tree`, grouped by language.
    ///
    /// All of one language's pieces are returned together, because they are
    /// parsed together: two `<script>` elements are one JavaScript program as
    /// far as a name declared in the first and used in the second goes.
    pub fn injection_ranges(&self, tree: &Tree) -> Vec<(Language, Vec<ffi::TSRange>)> {
        let mut out: Vec<(Language, Vec<ffi::TSRange>)> = Vec::new();
        for &(query, language) in &self.injections {
            // SAFETY: the cursor is allocated and freed here; query and tree
            // are live for the duration.
            let Some(cursor) = NonNull::new(unsafe { ffi::ts_query_cursor_new() }) else {
                continue;
            };
            let mut ranges = Vec::new();
            unsafe {
                let root = ffi::ts_tree_root_node(tree.raw.as_ptr());
                ffi::ts_query_cursor_exec(cursor.as_ptr(), query.as_ptr(), root);
                let mut m = std::mem::zeroed::<ffi::TSQueryMatch>();
                while ffi::ts_query_cursor_next_match(cursor.as_ptr(), &mut m) {
                    if m.captures.is_null() {
                        continue;
                    }
                    for capture in std::slice::from_raw_parts(m.captures, m.capture_count as usize)
                    {
                        let range = ffi::TSRange {
                            start_point: ffi::ts_node_start_point(capture.node),
                            end_point: ffi::ts_node_end_point(capture.node),
                            start_byte: ffi::ts_node_start_byte(capture.node),
                            end_byte: ffi::ts_node_end_byte(capture.node),
                        };
                        if range.end_byte > range.start_byte {
                            ranges.push(range);
                        }
                    }
                }
                ffi::ts_query_cursor_delete(cursor.as_ptr());
            }
            if ranges.is_empty() {
                continue;
            }
            // The parser insists on ascending, non-overlapping ranges.
            ranges.sort_by_key(|r| r.start_byte);
            ranges.dedup_by(|b, a| b.start_byte < a.end_byte);
            match out.iter_mut().find(|(l, _)| *l == language) {
                Some((_, all)) => all.extend(ranges),
                None => out.push((language, ranges)),
            }
        }
        out
    }

    /// Parses only `ranges` of the rope: this language, out of the middle of
    /// a document in another. `old` is the previous tree for the same layer,
    /// already edited or about to be, for an incremental parse.
    pub fn parse_ranges(
        &mut self,
        rope: &Rope,
        ranges: &[ffi::TSRange],
        old: Option<(&Tree, &[crate::text::buffer::Edit])>,
    ) -> Option<Tree> {
        // SAFETY: the slice is live for the call, which copies it.
        let accepted = unsafe {
            ffi::ts_parser_set_included_ranges(
                self.parser.as_ptr(),
                ranges.as_ptr(),
                ranges.len() as u32,
            )
        };
        let tree = if accepted {
            match old {
                Some((tree, edits)) => self.parse_incremental(rope, tree, edits),
                None => self.parse(rope),
            }
        } else {
            None
        };
        // Lifted again whatever happened. This parser also parses whole
        // documents in its language, and a leftover restriction would have
        // it silently parse a corner of the next one.
        unsafe { ffi::ts_parser_set_included_ranges(self.parser.as_ptr(), std::ptr::null(), 0) };
        tree
    }

    /// Re-parses after a set of edits, reusing the unchanged parts.
    ///
    /// This is what makes highlighting viable on real files. A full parse is
    /// linear in file size: measured at 1.5ms for 16KiB but 23ms at 256KiB
    /// and 381ms at 4MiB, so re-parsing everything on each keystroke would
    /// blow the frame budget on any file worth having an editor for. Told
    /// where the text changed, tree-sitter reuses every subtree the edit did
    /// not touch.
    ///
    /// Edits must be applied in the order they happened, since each one's
    /// offsets are relative to the text as of that moment.
    pub fn parse_incremental(
        &mut self,
        rope: &Rope,
        old: &Tree,
        edits: &[crate::text::buffer::Edit],
    ) -> Option<Tree> {
        for edit in edits {
            let raw = ffi::TSInputEdit {
                start_byte: edit.start_byte as u32,
                old_end_byte: edit.old_end_byte as u32,
                new_end_byte: edit.new_end_byte as u32,
                start_point: point(edit.start_point),
                old_end_point: point(edit.old_end_point),
                new_end_point: point(edit.new_end_point),
            };
            // SAFETY: `old` owns a live tree, and `raw` is valid for the call.
            unsafe { ffi::ts_tree_edit(old.raw.as_ptr(), &raw) };
        }

        let mut state = ReadState {
            rope,
            chunk: Vec::with_capacity(4096),
        };
        let input = ffi::TSInput {
            payload: &mut state as *mut ReadState as *mut c_void,
            read: Some(read_rope),
            encoding: ffi::TS_INPUT_ENCODING_UTF8,
            decode: std::ptr::null(),
        };
        // SAFETY: as `parse`, but handing the edited old tree back so its
        // untouched subtrees can be reused.
        let raw = unsafe { ffi::ts_parser_parse(self.parser.as_ptr(), old.raw.as_ptr(), input) };
        NonNull::new(raw).map(|raw| Tree { raw })
    }

    /// Parses a rope into a tree.
    pub fn parse(&mut self, rope: &Rope) -> Option<Tree> {
        let mut state = ReadState {
            rope,
            chunk: Vec::with_capacity(4096),
        };
        let input = ffi::TSInput {
            payload: &mut state as *mut ReadState as *mut c_void,
            read: Some(read_rope),
            encoding: ffi::TS_INPUT_ENCODING_UTF8,
            decode: std::ptr::null(),
        };
        // SAFETY: the parser is live, the input payload outlives the call
        // because `state` is on this stack frame, and a null old_tree means
        // a fresh parse.
        let raw = unsafe { ffi::ts_parser_parse(self.parser.as_ptr(), std::ptr::null(), input) };
        NonNull::new(raw).map(|raw| Tree { raw })
    }

    /// Highlight spans overlapping a byte range, in order.
    ///
    /// Without buffer text available, predicates cannot be checked; prefer
    /// [`Highlighter::spans_with`].
    pub fn spans(&self, tree: &Tree, range: std::ops::Range<usize>) -> Vec<Span> {
        self.spans_with(tree, range, |_| String::new())
    }

    /// Highlight spans overlapping a byte range, in order.
    ///
    /// Restricted to the range on purpose: this runs per frame, and querying
    /// a whole large file to draw one screen of it would cost more than
    /// everything else in the frame put together.
    /// As [`Highlighter::spans`], reading capture text through `source` so
    /// predicates can be evaluated.
    pub fn spans_with(
        &self,
        tree: &Tree,
        range: std::ops::Range<usize>,
        source: impl Fn(std::ops::Range<usize>) -> String,
    ) -> Vec<Span> {
        // SAFETY: cursor is allocated and freed in this function.
        let Some(cursor) = NonNull::new(unsafe { ffi::ts_query_cursor_new() }) else {
            return Vec::new();
        };
        let mut out: Vec<(u16, Span)> = Vec::new();

        unsafe {
            ffi::ts_query_cursor_set_byte_range(
                cursor.as_ptr(),
                range.start as u32,
                range.end as u32,
            );
            let root = ffi::ts_tree_root_node(tree.raw.as_ptr());
            ffi::ts_query_cursor_exec(cursor.as_ptr(), self.query.as_ptr(), root);

            let mut m = std::mem::zeroed::<ffi::TSQueryMatch>();
            while ffi::ts_query_cursor_next_match(cursor.as_ptr(), &mut m) {
                if m.captures.is_null() {
                    continue;
                }
                let captures = std::slice::from_raw_parts(m.captures, m.capture_count as usize);

                // Every predicate on this pattern has to hold for every
                // capture in the match, or the match is not really a match.
                let predicates = self
                    .pattern_predicates
                    .get(m.pattern_index as usize)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                if !predicates.is_empty() {
                    let satisfied = captures.iter().all(|capture| {
                        let start = ffi::ts_node_start_byte(capture.node) as usize;
                        let end = ffi::ts_node_end_byte(capture.node) as usize;
                        let text = source(start..end);
                        predicates.iter().all(|p| p.accepts(capture.index, &text))
                    });
                    if !satisfied {
                        continue;
                    }
                }

                for capture in captures {
                    let Some(Some(kind)) = self.capture_kinds.get(capture.index as usize).copied()
                    else {
                        continue;
                    };
                    let start = ffi::ts_node_start_byte(capture.node) as usize;
                    let end = ffi::ts_node_end_byte(capture.node) as usize;
                    if end > start {
                        out.push((m.pattern_index, Span { start, end, kind }));
                    }
                }
            }
            ffi::ts_query_cursor_delete(cursor.as_ptr());
        }

        // Order matters in three ways, and getting any of them wrong shows
        // up as the wrong colour rather than as an error.
        //
        // By start, because the renderer walks spans alongside the text.
        // Then by *narrower last*, so a specific capture sits inside a
        // broader one. Then by pattern index, in whichever direction this
        // grammar's query was written for: in the Rust grammar `@constant`
        // is declared above `@constructor` above `@type`, all three match a
        // name like `MAX_SIZE`, and the first is meant to win.
        let later_wins = self.language.later_pattern_wins();
        out.sort_by(|(pa, a), (pb, b)| {
            let by_pattern = if later_wins { pb.cmp(pa) } else { pa.cmp(pb) };
            a.start
                .cmp(&b.start)
                .then(b.end.cmp(&a.end))
                .then(by_pattern)
        });

        // Keep only the winning span per start offset.
        let mut spans: Vec<Span> = Vec::with_capacity(out.len());
        for (_, span) in out {
            match spans.last() {
                Some(last) if last.start == span.start && last.end == span.end => {}
                _ => spans.push(span),
            }
        }
        spans
    }
}

impl Drop for Highlighter {
    fn drop(&mut self) {
        // SAFETY: every pointer is owned by self and dropped once.
        unsafe {
            for (query, _) in &self.injections {
                ffi::ts_query_delete(query.as_ptr());
            }
            ffi::ts_query_delete(self.query.as_ptr());
            ffi::ts_parser_delete(self.parser.as_ptr());
        }
    }
}

// SAFETY: a Highlighter owns its parser and query exclusively and exposes no
// interior mutability. Moving one to a background thread is the intended use.
unsafe impl Send for Highlighter {}

/// Reads each pattern's predicates out of a compiled query.
///
/// A predicate this implementation cannot represent becomes
/// [`Predicate::Unsupported`], which rejects everything, so the pattern goes
/// inert. That is the safe direction: a missing highlight is invisible, a
/// wrong one is not.
fn load_predicates(query: *const ffi::TSQuery) -> Vec<Vec<Predicate>> {
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
            if ptr.is_null() {
                return String::new();
            }
            let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
            String::from_utf8_lossy(bytes).into_owned()
        };

        // Steps come as a flat list of runs terminated by `Done`. Each run is
        // the predicate name followed by its arguments.
        let mut predicates = Vec::new();
        let mut run: Vec<(u32, u32)> = Vec::new();
        for step in steps {
            if step.kind == ffi::TSQueryPredicateStep::DONE {
                if !run.is_empty() {
                    predicates.push(build_predicate(&run, &string_at));
                    run.clear();
                }
                continue;
            }
            run.push((step.kind, step.value_id));
        }
        if !run.is_empty() {
            predicates.push(build_predicate(&run, &string_at));
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
fn build_predicate(run: &[(u32, u32)], string_at: &dyn Fn(u32) -> String) -> Predicate {
    const CAPTURE: u32 = ffi::TSQueryPredicateStep::CAPTURE;
    const STRING: u32 = ffi::TSQueryPredicateStep::STRING;

    let Some(&(kind, name_id)) = run.first() else {
        return Predicate::Unsupported;
    };
    if kind != STRING {
        return Predicate::Unsupported;
    }
    let name = string_at(name_id);
    let args = &run[1..];

    match name.as_str() {
        "match?" | "not-match?" => {
            let [(CAPTURE, capture), (STRING, pattern_id)] = args[..] else {
                return Predicate::Unsupported;
            };
            let source = string_at(pattern_id);
            // `^(a|b|c)$` is how queries spell "one of these words", and it
            // is the one use of alternation in the grammars vendored here.
            // It needs no regex engine: it is `any-of?` with more typing.
            if let Some(values) = literal_alternatives(&source).filter(|_| name == "match?") {
                return Predicate::AnyOf { capture, values };
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
                return Predicate::Unsupported;
            };
            Predicate::EqString {
                capture,
                value: string_at(value_id),
                negated: name.starts_with("not-"),
            }
        }
        "any-of?" => {
            let Some(&(CAPTURE, capture)) = args.first() else {
                return Predicate::Unsupported;
            };
            let values = args[1..]
                .iter()
                .filter(|(k, _)| *k == STRING)
                .map(|(_, id)| string_at(*id))
                .collect();
            Predicate::AnyOf { capture, values }
        }
        // `set!`, `is?` and friends are directives rather than filters; they
        // do not constrain a match.
        "set!" | "is?" | "is-not?" => Predicate::AnyOf {
            capture: u32::MAX,
            values: Vec::new(),
        },
        _ => Predicate::Unsupported,
    }
}

/// Row/column pair to tree-sitter's point type.
fn point((row, column): (usize, usize)) -> ffi::TSPoint {
    ffi::TSPoint {
        row: row as u32,
        column: column as u32,
    }
}

/// Unused today, kept because CString is the right type for future query
/// sources loaded at runtime rather than compiled in.
#[allow(dead_code)]
fn _assert_cstring_in_scope(s: &str) -> Option<CString> {
    CString::new(s).ok()
}

/// Turns spans that nest into a flat run of segments, innermost winning.
///
/// A query captures at every level at once: a whole template string and the
/// `${}` inside it, a call and the property it is called on. The renderer
/// walks one list alongside the text and assumes each span ends before the
/// next begins, so given nesting it drew the outer colour throughout and
/// never reached the inner one. Cutting the outer span around its children
/// gives it the list it expects, and says what was meant.
fn flatten(mut spans: Vec<Span>) -> Vec<Span> {
    spans.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));

    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    let mut open: Vec<Span> = Vec::new();
    // Everything before this offset has been emitted.
    let mut done = 0;
    let mut emit = |kind: Kind, start: usize, end: usize| {
        if end > start {
            out.push(Span { start, end, kind });
        }
    };

    for span in spans {
        // Close whatever ends before this one starts.
        while let Some(top) = open.last().copied() {
            if top.end > span.start {
                break;
            }
            emit(top.kind, done.max(top.start), top.end);
            done = done.max(top.end);
            open.pop();
        }
        // The part of the enclosing span in front of this one.
        if let Some(top) = open.last() {
            emit(top.kind, done.max(top.start), span.start.min(top.end));
        }
        done = done.max(span.start);
        open.push(span);
    }
    while let Some(top) = open.pop() {
        emit(top.kind, done.max(top.start), top.end);
        done = done.max(top.end);
    }
    out
}

/// Parse trees for every open document, and one highlighter per language.
///
/// A tree describes one text. There used to be a single tree for whichever
/// tab was showing, reused whenever the language matched, so switching between
/// two Rust files handed tree-sitter the other file's tree with no edits and
/// got that tree straight back: one file coloured with the other's offsets,
/// and a slice through the middle of a character when the offsets disagreed.
/// Keyed by [`Buffer::id`], a tree cannot outlive or stray from its document.
#[derive(Default)]
pub struct SyntaxStore {
    highlighters: Vec<Highlighter>,
    trees: std::collections::HashMap<u64, DocumentTrees>,
}

/// Everything parsed for one document.
struct DocumentTrees {
    language: Language,
    tree: Tree,
    /// One tree per language embedded in the document: the JavaScript in an
    /// HTML file's `<script>` elements, the CSS in its `<style>`. Each covers
    /// all of that language's pieces at once and nothing in between.
    layers: Vec<(Language, Tree)>,
}

impl SyntaxStore {
    pub fn new() -> Self {
        SyntaxStore::default()
    }

    /// The highlighter for a language, built on first use.
    fn highlighter(&mut self, language: Language) -> Option<&mut Highlighter> {
        if !self.highlighters.iter().any(|h| h.language == language) {
            self.highlighters.push(Highlighter::new(language)?);
        }
        self.highlighters
            .iter_mut()
            .find(|h| h.language == language)
    }

    /// Brings `buffer`'s trees up to date with its text.
    ///
    /// Always drains the buffer's edits, whatever happens next. An edit list
    /// that survives a full parse is replayed by the next incremental one
    /// onto a tree that already contains it, which shifts every subtree after
    /// it; and for a file with no grammar it would simply grow for ever.
    ///
    /// `budget` is the largest text that gets parsed at all.
    pub fn update(&mut self, buffer: &mut crate::text::buffer::Buffer, budget: usize) {
        let edits = buffer.drain_edits();
        let id = buffer.id();
        let rope = &buffer.rope;

        let language = buffer
            .extension()
            .and_then(|e| Language::from_extension(&e));
        let Some(language) = language.filter(|_| rope.len_bytes() <= budget) else {
            self.trees.remove(&id);
            return;
        };

        // Taken out of the map for the duration, so the old trees can be read
        // while `self` is borrowed for a highlighter. Incremental only onto
        // this document's own trees, in the same language, with a complete
        // account of what changed since.
        let old = self.trees.remove(&id).filter(|t| t.language == language);
        let old = old.as_ref().zip(edits.as_deref());

        let Some(host) = self.highlighter(language) else {
            return;
        };
        let tree = match old {
            Some((trees, edits)) => host.parse_incremental(rope, &trees.tree, edits),
            None => host.parse(rope),
        };
        let Some(tree) = tree else {
            return;
        };

        let mut layers = Vec::new();
        for (embedded, ranges) in host.injection_ranges(&tree) {
            let Some(highlighter) = self.highlighter(embedded) else {
                continue;
            };
            // The same edits apply to a layer's old tree: they are positions
            // in the one document all the layers are parsed out of.
            let previous = old.and_then(|(trees, edits)| {
                let (_, tree) = trees.layers.iter().find(|(l, _)| *l == embedded)?;
                Some((tree, edits))
            });
            if let Some(layer) = highlighter.parse_ranges(rope, &ranges, previous) {
                layers.push((embedded, layer));
            }
        }

        self.trees.insert(
            id,
            DocumentTrees {
                language,
                tree,
                layers,
            },
        );
    }

    /// Whether the document has been parsed.
    pub fn has(&self, id: u64) -> bool {
        self.trees.contains_key(&id)
    }

    /// Highlight spans overlapping a byte range, in document order, from the
    /// document's own language and from every language embedded in it.
    pub fn spans_with(
        &self,
        id: u64,
        range: std::ops::Range<usize>,
        source: impl Fn(std::ops::Range<usize>) -> String,
    ) -> Vec<Span> {
        let Some(trees) = self.trees.get(&id) else {
            return Vec::new();
        };
        let all = std::iter::once((trees.language, &trees.tree)).chain(
            trees
                .layers
                .iter()
                .map(|(language, tree)| (*language, tree)),
        );

        let mut spans = Vec::new();
        for (language, tree) in all {
            if let Some(h) = self.highlighters.iter().find(|h| h.language == language) {
                spans.extend(h.spans_with(tree, range.clone(), &source));
            }
        }
        flatten(spans)
    }

    /// Forgets the trees of documents that are no longer open.
    pub fn retain(&mut self, open: impl Fn(u64) -> bool) {
        self.trees.retain(|id, _| open(*id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust() -> Highlighter {
        Highlighter::new(Language::Rust).expect("the Rust grammar is compiled in")
    }

    #[test]
    fn detects_language_from_extension() {
        assert_eq!(
            Language::from_path(std::path::Path::new("src/main.rs")),
            Some(Language::Rust)
        );
        assert_eq!(Language::from_path(std::path::Path::new("README.md")), None);
        assert_eq!(
            Language::from_path(std::path::Path::new("src/App.svelte")),
            Some(Language::Html)
        );
        assert_eq!(Language::from_path(std::path::Path::new("Makefile")), None);
    }

    #[test]
    fn parses_valid_rust_without_error() {
        let mut h = rust();
        let rope = Rope::from_text("fn main() { println!(\"hi\"); }\n");
        let tree = h.parse(&rope).expect("parse");
        assert!(!tree.has_error(), "valid Rust should parse cleanly");
    }

    #[test]
    fn reports_a_syntax_error() {
        let mut h = rust();
        let rope = Rope::from_text("fn main( { this is not rust ]]]\n");
        let tree = h.parse(&rope).expect("parse");
        assert!(tree.has_error(), "broken Rust should be flagged");
    }

    #[test]
    fn highlights_keywords_strings_and_comments() {
        let mut h = rust();
        let src = "// a comment\nfn main() {\n    let x = \"text\";\n}\n";
        let rope = Rope::from_text(src);
        let tree = h.parse(&rope).expect("parse");
        let spans = h.spans(&tree, 0..rope.len_bytes());

        let kinds: Vec<Kind> = spans.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&Kind::Comment), "no comment captured");
        assert!(kinds.contains(&Kind::Keyword), "no keyword captured");
        assert!(kinds.contains(&Kind::String), "no string captured");

        // The comment span must actually cover the comment text.
        let comment = spans
            .iter()
            .find(|s| s.kind == Kind::Comment)
            .expect("comment");
        assert_eq!(&src[comment.start..comment.end], "// a comment");
    }

    /// Regression: tree-sitter returns every match and expects the client to
    /// evaluate predicates. Not doing so does not lose highlights, it invents
    /// them: every identifier was captured as constant AND constructor AND
    /// type, and sort order decided the colour.
    #[test]
    fn predicates_filter_matches() {
        let mut h = rust();
        let src = "fn f() { let lowercase = OTHER_CONST; }";
        let rope = Rope::from_text(src);
        let tree = h.parse(&rope).expect("parse");
        let spans = h.spans_with(&tree, 0..rope.len_bytes(), |r| rope.slice_to_string(r));

        let kinds = |needle: &str| -> Vec<Kind> {
            let at = src.find(needle).expect("present");
            spans
                .iter()
                .filter(|s| s.start == at && s.end == at + needle.len())
                .map(|s| s.kind)
                .collect()
        };

        assert!(
            kinds("lowercase").is_empty(),
            "a lowercase name matches no #match? pattern and should get no span"
        );
        assert_eq!(
            kinds("OTHER_CONST"),
            vec![Kind::Constant],
            "SCREAMING_CASE should be a constant, and exactly one span"
        );
    }

    /// Without predicate evaluation the same byte range got three spans.
    #[test]
    fn a_name_gets_exactly_one_span() {
        let mut h = rust();
        let src = "fn main() {}";
        let rope = Rope::from_text(src);
        let tree = h.parse(&rope).expect("parse");
        let spans = h.spans_with(&tree, 0..rope.len_bytes(), |r| rope.slice_to_string(r));
        let at = src.find("main").expect("present");
        let overlapping: Vec<Kind> = spans
            .iter()
            .filter(|s| s.start == at && s.end == at + 4)
            .map(|s| s.kind)
            .collect();
        assert_eq!(overlapping, vec![Kind::Function]);
    }

    #[test]
    fn spans_are_bounded_by_the_requested_range() {
        let mut h = rust();
        let src = "fn a() {}\nfn b() {}\nfn c() {}\n";
        let rope = Rope::from_text(src);
        let tree = h.parse(&rope).expect("parse");

        // Only the middle line.
        let spans = h.spans(&tree, 10..20);
        assert!(!spans.is_empty(), "should find something on line 2");
        assert!(
            spans.iter().all(|s| s.end > 10 && s.start < 20),
            "a range query must not return spans from outside it"
        );
    }

    #[test]
    fn spans_come_back_in_document_order() {
        let mut h = rust();
        let rope = Rope::from_text("fn one() {}\nfn two() {}\nfn three() {}\n");
        let tree = h.parse(&rope).expect("parse");
        let spans = h.spans(&tree, 0..rope.len_bytes());
        assert!(
            spans.windows(2).all(|w| w[0].start <= w[1].start),
            "spans must be sorted for the renderer to apply them in order"
        );
    }

    #[test]
    fn parses_a_large_rope_through_chunks() {
        // Exercises the read callback across many leaves, which is the part
        // that would break if chunk boundaries were mishandled.
        let mut h = rust();
        let unit = "fn f() { let s = \"x\"; } // c\n";
        let src = unit.repeat(4000);
        let rope = Rope::from_text(&src);
        assert!(rope.len_bytes() > 100_000);

        let tree = h.parse(&rope).expect("parse");
        assert!(!tree.has_error(), "chunked reads corrupted the input");

        let spans = h.spans(&tree, 0..2000);
        assert!(!spans.is_empty());
    }

    /// The read callback hands tree-sitter fixed 4096-byte windows, which
    /// have no reason to end on a char boundary. Every offset of a 3-byte
    /// character is tried against the window edge, because the bug this
    /// guards against only fired for some of them, and it fired as an abort
    /// inside an `extern "C"` frame.
    #[test]
    fn parses_multibyte_text_whatever_the_read_window_cuts() {
        let mut h = rust();
        for pad in 0..4 {
            let src = format!(
                "//{}\n{}",
                "x".repeat(pad),
                "// \u{2500}\u{e9}\u{65e5}\nfn f() {}\n".repeat(1500)
            );
            let rope = Rope::from_text(&src);
            assert!(rope.len_bytes() > 3 * 4096);
            let tree = h.parse(&rope).expect("parse");
            assert!(
                !tree.has_error(),
                "pad {pad}: a split character reached the parser broken"
            );
        }
    }

    /// The property that matters: an incremental re-parse must be
    /// indistinguishable from a full one. If these ever diverge, the editor
    /// shows stale or wrong colours, which is worse than slow ones.
    #[test]
    fn incremental_parse_matches_a_full_parse() {
        use crate::text::buffer::Buffer;

        let mut h = rust();
        let mut buffer =
            Buffer::from_text("fn main() {\n    let x = 1;\n    println!(\"hi\");\n}\n");
        let mut tree = h.parse(&buffer.rope).expect("initial parse");
        buffer.drain_edits();

        // A mix of edit shapes, since each records its offsets differently.
        buffer.move_buffer_end(crate::text::buffer::Motion::Move);
        buffer.insert("\nfn second() -> u32 { 42 }\n");
        buffer.move_buffer_start(crate::text::buffer::Motion::Move);
        buffer.insert("// leading comment\n");
        for _ in 0..4 {
            buffer.move_right(crate::text::buffer::Motion::Move);
        }
        buffer.delete_word_forward();

        let edits = buffer.drain_edits().expect("edits should be replayable");
        assert!(!edits.is_empty());

        let incremental = h
            .parse_incremental(&buffer.rope, &tree, &edits)
            .expect("incremental parse");
        let full = h.parse(&buffer.rope).expect("full parse");

        let range = 0..buffer.rope.len_bytes();
        assert_eq!(
            h.spans(&incremental, range.clone()),
            h.spans(&full, range),
            "incremental and full parses disagreed"
        );

        tree = incremental;
        assert!(!tree.has_error());
    }

    // ---- the vendored grammars --------------------------------------------

    /// A query that does not fit its grammar makes `Highlighter::new` return
    /// `None`, which the editor shows as a file with no colour and no error.
    #[test]
    fn every_language_builds_its_highlighter_and_injections() {
        for language in Language::ALL {
            let h = Highlighter::new(language)
                .unwrap_or_else(|| panic!("{language:?}: grammar ABI or highlight query rejected"));
            assert_eq!(
                h.injections.len(),
                language.injections().len(),
                "{language:?}: an injection query did not compile"
            );
        }
    }

    /// A predicate this implementation cannot evaluate switches its pattern
    /// off, quietly. Fine as a fallback, not fine as the normal state of a
    /// grammar we ship: every one of them has to be understood.
    #[test]
    fn no_vendored_query_has_a_predicate_we_cannot_evaluate() {
        for language in Language::ALL {
            let h = Highlighter::new(language).expect("highlighter");
            let inert = h
                .pattern_predicates
                .iter()
                .flatten()
                .filter(|p| matches!(p, Predicate::Unsupported))
                .count();
            assert_eq!(inert, 0, "{language:?} has {inert} patterns switched off");
        }
    }

    fn kinds_of(text: &str, name: &str) -> Vec<(String, Kind)> {
        let mut store = SyntaxStore::new();
        let mut buffer = rust_buffer(text, name);
        store.update(&mut buffer, BUDGET);
        store
            .spans_with(buffer.id(), 0..text.len(), |r| {
                buffer.rope.slice_to_string(r)
            })
            .into_iter()
            .map(|s| (text[s.start..s.end].to_string(), s.kind))
            .collect()
    }

    #[test]
    fn each_language_colours_something_recognisable() {
        let has = |spans: &[(String, Kind)], text: &str, kind: Kind| {
            assert!(
                spans.iter().any(|(t, k)| t == text && *k == kind),
                "expected {text:?} as {kind:?} in {spans:?}"
            );
        };
        let py = kinds_of("def hello():\n    return 'hi' # note\n", "a.py");
        has(&py, "def", Kind::Keyword);
        has(&py, "hello", Kind::Function);
        has(&py, "# note", Kind::Comment);
        let c = kinds_of("int main(void) { return 42; }\n", "a.c");
        has(&c, "int", Kind::Type);
        has(&c, "return", Kind::Keyword);
        let cpp = kinds_of(
            "class Greeter { public: int greet() { return 1; } };\n",
            "a.cpp",
        );
        has(&cpp, "class", Kind::Keyword);
        has(&cpp, "return", Kind::Keyword);
        let go = kinds_of("package main\nfunc hello() int { return 42 }\n", "a.go");
        has(&go, "func", Kind::Keyword);
        has(&go, "hello", Kind::Function);
        let js = kinds_of("const n = 42; console.log(\"hi\"); // done\n", "a.js");
        has(&js, "const", Kind::Keyword);
        has(&js, "42", Kind::Number);
        has(&js, "\"hi\"", Kind::String);
        has(&js, "// done", Kind::Comment);
        // The `^(arguments|module|console|window|document)$` pattern.
        has(&js, "console", Kind::Variable);
        // The JavaScript query opens with `(identifier) @variable` and
        // refines it below, so it only works if the later pattern wins.
        let js = kinds_of("function go() { post(1); new Thing(MAX_SIZE); }\n", "b.js");
        has(&js, "go", Kind::Function);
        has(&js, "post", Kind::Function);
        has(&js, "Thing", Kind::Type);
        has(&js, "MAX_SIZE", Kind::Constant);
        // Rust's is written the other way round, and must not change.
        let rs = kinds_of(
            "const MAX_SIZE: usize = 1;\nfn f() -> Option<u8> { Some(MAX_SIZE as u8) }\n",
            "a.rs",
        );
        has(&rs, "MAX_SIZE", Kind::Constant);
        has(&rs, "Some", Kind::Type);

        let ts = kinds_of(
            "interface Point { x: number }\nconst p: Point = { x: 1 };\n",
            "a.ts",
        );
        has(&ts, "interface", Kind::Keyword);
        has(&ts, "Point", Kind::Type);
        has(&ts, "number", Kind::Type);

        let tsx = kinds_of("const a = <div className=\"x\">hi</div>;\n", "a.tsx");
        has(&tsx, "div", Kind::Function);
        has(&tsx, "className", Kind::Attribute);

        let css = kinds_of("a.link { color: red; margin: 4px; }\n", "a.css");
        has(&css, "color", Kind::Property);
        // The unit is a node inside the number, and keeps its own colour.
        has(&css, "4", Kind::Number);
        has(&css, "px", Kind::Type);

        let json = kinds_of("{\"name\": \"caio\", \"n\": 1, \"ok\": true}\n", "a.json");
        has(&json, "\"caio\"", Kind::String);
        has(&json, "1", Kind::Number);
        has(&json, "true", Kind::Constant);
    }

    const PAGE: &str = "<!doctype html>\n<html>\n<head>\n<style>\nbody { color: red; }\n</style>\n</head>\n<body class=\"x\">\n<script>\nconst caf\u{e9} = 42; // one\n</script>\n<p>text</p>\n<script>\nfunction later() { return caf\u{e9}; }\n</script>\n</body>\n</html>\n";

    /// The file that prompted all this: HTML with its JavaScript and CSS
    /// inline. The host grammar sees the inside of `<script>` as one opaque
    /// `raw_text` node, so without injections it is a page of white text.
    #[test]
    fn html_colours_the_script_and_the_style_inside_it() {
        let spans = kinds_of(PAGE, "index.html");
        let has = |text: &str, kind: Kind| {
            assert!(
                spans.iter().any(|(t, k)| t == text && *k == kind),
                "expected {text:?} as {kind:?} in {spans:?}"
            );
        };
        has("body", Kind::Function); // a tag, from the HTML grammar
        has("class", Kind::Attribute);
        has("const", Kind::Keyword); // first <script>, from JavaScript
        has("42", Kind::Number);
        has("function", Kind::Keyword); // second <script>, same layer
        has("color", Kind::Property); // <style>, from CSS

        // In document order, which the renderer walks the text alongside.
        let mut store = SyntaxStore::new();
        let mut buffer = rust_buffer(PAGE, "index.html");
        store.update(&mut buffer, BUDGET);
        let all = store.spans_with(buffer.id(), 0..PAGE.len(), |r| {
            buffer.rope.slice_to_string(r)
        });
        assert!(all.windows(2).all(|w| w[0].start <= w[1].start));
    }

    #[test]
    fn editing_inside_an_embedded_language_matches_a_full_parse() {
        let mut store = SyntaxStore::new();
        let mut buffer = rust_buffer(PAGE, "index.html");
        store.update(&mut buffer, BUDGET);

        // Type inside the first script, which moves everything after it,
        // including the second script's range.
        let at = PAGE.find("const").expect("script body");
        buffer.place_cursor(at, crate::text::buffer::Motion::Move);
        for piece in ["let ", "early", " = \"s\";\n"] {
            buffer.insert(piece);
            store.update(&mut buffer, BUDGET);
            assert_store_matches_a_full_parse(&store, &buffer);
        }

        // Remove the second script altogether, then the style.
        let text = buffer.rope.to_string();
        let from = text.rfind("<script>").expect("second script");
        let to = text.rfind("</script>").expect("its end") + "</script>".len();
        buffer.place_cursor(from, crate::text::buffer::Motion::Move);
        buffer.place_cursor(to, crate::text::buffer::Motion::Extend);
        buffer.backspace();
        store.update(&mut buffer, BUDGET);
        assert_store_matches_a_full_parse(&store, &buffer);

        // A parser left restricted to the old ranges would parse only a
        // corner of the next whole document in that language.
        let mut script = rust_buffer("const whole = 1;\nfunction f() {}\n", "whole.js");
        store.update(&mut script, BUDGET);
        assert_store_matches_a_full_parse(&store, &script);
    }

    #[test]
    fn nested_spans_flatten_with_the_innermost_winning() {
        let span = |start, end, kind| Span { start, end, kind };
        // A string 0..20 holding a substitution 5..12 that holds a number.
        let flat = flatten(vec![
            span(7, 9, Kind::Number),
            span(0, 20, Kind::String),
            span(5, 12, Kind::Punctuation),
            span(30, 34, Kind::Keyword),
        ]);
        assert_eq!(
            flat,
            vec![
                span(0, 5, Kind::String),
                span(5, 7, Kind::Punctuation),
                span(7, 9, Kind::Number),
                span(9, 12, Kind::Punctuation),
                span(12, 20, Kind::String),
                span(30, 34, Kind::Keyword),
            ]
        );
        // What the renderer relies on: in order, and never overlapping.
        assert!(flat.windows(2).all(|w| w[0].end <= w[1].start));

        // Real text with real nesting.
        let mut store = SyntaxStore::new();
        let text = "const s = `a ${1 + n} b`; obj.method(x);\n#[derive(Debug)]\n";
        let mut buffer = rust_buffer(text, "nest.js");
        store.update(&mut buffer, BUDGET);
        let spans = store.spans_with(buffer.id(), 0..text.len(), |r| {
            buffer.rope.slice_to_string(r)
        });
        assert!(
            spans.windows(2).all(|w| w[0].end <= w[1].start),
            "{spans:?}"
        );
        assert!(
            spans
                .iter()
                .any(|s| &text[s.start..s.end] == "1" && s.kind == Kind::Number),
            "the number inside the template string keeps its own colour"
        );
    }

    /// What the store shows for a buffer, against a parse of that buffer's
    /// own text from scratch. Any disagreement means the tree on screen
    /// describes some other text.
    fn assert_store_matches_a_full_parse(
        store: &SyntaxStore,
        buffer: &crate::text::buffer::Buffer,
    ) {
        assert!(store.has(buffer.id()), "the document has a tree");
        let range = 0..buffer.rope.len_bytes();
        let language = Language::from_path(buffer.path.as_deref().expect("path")).expect("grammar");
        let mut fresh = Highlighter::new(language).expect("highlighter");
        let full = fresh.parse(&buffer.rope).expect("full parse");
        assert_eq!(
            store.spans_with(buffer.id(), range.clone(), |r| buffer
                .rope
                .slice_to_string(r)),
            {
                let mut expected =
                    fresh.spans_with(&full, range.clone(), |r| buffer.rope.slice_to_string(r));
                for (embedded, ranges) in fresh.injection_ranges(&full) {
                    let mut h = Highlighter::new(embedded).expect("embedded highlighter");
                    let layer = h.parse_ranges(&buffer.rope, &ranges, None).expect("layer");
                    expected.extend(
                        h.spans_with(&layer, range.clone(), |r| buffer.rope.slice_to_string(r)),
                    );
                }
                flatten(expected)
            }
        );
    }

    fn rust_buffer(text: &str, name: &str) -> crate::text::buffer::Buffer {
        let mut buffer = crate::text::buffer::Buffer::from_text(text);
        buffer.path = Some(std::path::PathBuf::from(name));
        buffer
    }

    const BUDGET: usize = 2 * 1024 * 1024;

    /// The bug this store exists for: one tree shared by every tab in the
    /// same language, so the second Rust file was drawn with the first one's
    /// tree.
    #[test]
    fn two_documents_in_one_language_keep_their_own_trees() {
        let mut store = SyntaxStore::new();
        let mut a = rust_buffer("fn alpha() { let s = \"caf\u{e9}\"; }\n", "a.rs");
        let mut b = rust_buffer(
            "// \u{2500}\u{2500} b \u{2500}\u{2500}\nstruct Beta;\n",
            "b.rs",
        );

        // Switch back and forth, editing in between, as tabs do.
        store.update(&mut a, BUDGET);
        store.update(&mut b, BUDGET);
        assert_store_matches_a_full_parse(&store, &a);
        assert_store_matches_a_full_parse(&store, &b);

        a.insert("// edited\n");
        store.update(&mut a, BUDGET);
        store.update(&mut b, BUDGET);
        b.insert("const N: u8 = 1;\n");
        store.update(&mut b, BUDGET);
        store.update(&mut a, BUDGET);
        assert_store_matches_a_full_parse(&store, &a);
        assert_store_matches_a_full_parse(&store, &b);
    }

    /// Type into an untitled buffer, Save As `x.rs`, keep typing. The edits
    /// made before the file had a grammar must not be replayed onto the tree
    /// of its first parse, which already contains them.
    #[test]
    fn edits_made_before_the_first_parse_are_not_replayed() {
        let mut store = SyntaxStore::new();
        let mut buffer = crate::text::buffer::Buffer::new();
        buffer.insert("fn main() {\n");
        buffer.insert("    let x = 1;\n}\n");
        store.update(&mut buffer, BUDGET);
        assert!(!store.has(buffer.id()), "untitled has no grammar");
        assert_eq!(
            buffer.drain_edits(),
            Some(Vec::new()),
            "edits are drained even when nothing parses, or they grow without bound"
        );

        buffer.insert("// more\n");
        buffer.path = Some(std::path::PathBuf::from("x.rs"));
        store.update(&mut buffer, BUDGET);
        buffer.insert("fn second() {}\n");
        store.update(&mut buffer, BUDGET);
        assert_store_matches_a_full_parse(&store, &buffer);
    }

    #[test]
    fn a_document_over_budget_or_renamed_away_loses_its_tree() {
        let mut store = SyntaxStore::new();
        let mut buffer = rust_buffer("fn f() {}\n", "f.rs");
        store.update(&mut buffer, BUDGET);
        assert!(store.has(buffer.id()));

        store.update(&mut buffer, 4);
        assert!(!store.has(buffer.id()), "over budget");

        // Back under it, after edits the store never saw parsed.
        buffer.insert("// grew while unparsed\n");
        store.update(&mut buffer, 4);
        store.update(&mut buffer, BUDGET);
        assert_store_matches_a_full_parse(&store, &buffer);

        buffer.path = Some(std::path::PathBuf::from("f.txt"));
        store.update(&mut buffer, BUDGET);
        assert!(!store.has(buffer.id()), "saved as plain text");
    }

    #[test]
    fn closed_documents_take_their_trees_with_them() {
        let mut store = SyntaxStore::new();
        let mut a = rust_buffer("fn a() {}\n", "a.rs");
        let mut b = rust_buffer("fn b() {}\n", "b.rs");
        store.update(&mut a, BUDGET);
        store.update(&mut b, BUDGET);
        let keep = a.id();
        store.retain(|id| id == keep);
        assert!(store.has(a.id()));
        assert!(!store.has(b.id()));
    }

    #[test]
    fn undo_invalidates_edits_and_forces_a_full_reparse() {
        use crate::text::buffer::Buffer;

        let mut buffer = Buffer::from_text("fn a() {}\n");
        buffer.insert("x");
        assert!(buffer.drain_edits().is_some());

        buffer.insert("y");
        buffer.undo();
        assert!(
            buffer.drain_edits().is_none(),
            "undo replaces the rope wholesale; edits cannot describe that"
        );
    }

    #[test]
    fn an_empty_buffer_parses_to_nothing() {
        let mut h = rust();
        let rope = Rope::new();
        let tree = h.parse(&rope).expect("parse");
        assert!(h.spans(&tree, 0..0).is_empty());
    }
}
