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
mod worker;

use std::ffi::{CString, c_char, c_void};
use std::ptr::NonNull;

use crate::syntax::predicate::{Predicate, load_predicates};
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
    // Markdown, styled in the source rather than by a grammar.
    /// The characters that make the syntax: `#`, `**`, backticks, pipes.
    MdMarker,
    MdHeading,
    MdStrong,
    MdEmphasis,
    MdStrongEmphasis,
    /// Inline code, drawn on a tinted background.
    MdCode,
    /// A fenced block's code where no grammar colours it.
    MdCodeBlock,
    MdLink,
    MdUrl,
    MdQuote,
    MdList,
    MdStrike,
    MdRule,
    MdTableHeader,
}

impl Kind {
    /// Drawn in the bold face.
    pub fn bold(self) -> bool {
        matches!(
            self,
            Kind::MdHeading | Kind::MdStrong | Kind::MdStrongEmphasis | Kind::MdTableHeader
        )
    }

    /// Drawn in the italic face.
    pub fn italic(self) -> bool {
        matches!(
            self,
            Kind::MdEmphasis | Kind::MdStrongEmphasis | Kind::MdQuote
        )
    }

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

impl<'a> ReadState<'a> {
    fn new(rope: &'a Rope) -> Self {
        ReadState {
            rope,
            chunk: Vec::with_capacity(4096),
        }
    }

    /// The input tree-sitter reads `rope` through. It points at `self`,
    /// which must stay where it is until the parse returns.
    fn input(&mut self) -> ffi::TSInput {
        ffi::TSInput {
            payload: self as *mut ReadState as *mut c_void,
            read: Some(read_rope),
            encoding: ffi::TS_INPUT_ENCODING_UTF8,
            decode: std::ptr::null(),
        }
    }
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

/// A query running over a tree. Owns its cursor and frees it on drop, so
/// no early exit can leak one.
pub(crate) struct QueryMatches {
    cursor: NonNull<ffi::TSQueryCursor>,
    current: ffi::TSQueryMatch,
}

impl QueryMatches {
    /// Starts `query` at `root`, limited to `bytes` when given. `None` when
    /// the cursor cannot be allocated.
    ///
    /// # Safety
    /// `query` and the tree `root` belongs to must outlive the value.
    pub(crate) unsafe fn new(
        query: *const ffi::TSQuery,
        root: ffi::TSNode,
        bytes: Option<std::ops::Range<usize>>,
    ) -> Option<Self> {
        let cursor = NonNull::new(unsafe { ffi::ts_query_cursor_new() })?;
        unsafe {
            if let Some(bytes) = bytes {
                ffi::ts_query_cursor_set_byte_range(
                    cursor.as_ptr(),
                    bytes.start as u32,
                    bytes.end as u32,
                );
            }
            ffi::ts_query_cursor_exec(cursor.as_ptr(), query, root);
        }
        Some(Self {
            cursor,
            // SAFETY: a plain C struct; all zeroes is a valid empty match.
            current: unsafe { std::mem::zeroed() },
        })
    }

    /// The next match: its pattern index and captures, valid until the
    /// next call.
    pub(crate) fn next_match(&mut self) -> Option<(u16, &[ffi::TSQueryCapture])> {
        // SAFETY: the cursor is live; the captures point into it and stay
        // valid until it advances, which the borrow on `self` rules out.
        unsafe {
            if !ffi::ts_query_cursor_next_match(self.cursor.as_ptr(), &mut self.current) {
                return None;
            }
            let captures = if self.current.captures.is_null() {
                &[][..]
            } else {
                std::slice::from_raw_parts(
                    self.current.captures,
                    self.current.capture_count as usize,
                )
            };
            Some((self.current.pattern_index, captures))
        }
    }
}

impl Drop for QueryMatches {
    fn drop(&mut self) {
        // SAFETY: allocated in `new`, freed once here.
        unsafe { ffi::ts_query_cursor_delete(self.cursor.as_ptr()) }
    }
}

/// The byte range a node covers.
///
/// # Safety
/// `node` must belong to a live tree.
pub(crate) unsafe fn node_bytes(node: ffi::TSNode) -> (usize, usize) {
    unsafe {
        (
            ffi::ts_node_start_byte(node) as usize,
            ffi::ts_node_end_byte(node) as usize,
        )
    }
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

    /// A copy for another thread. Cheap: the two share every node, and
    /// editing either one copies only the nodes the edit touches.
    pub fn copy(&self) -> Option<Tree> {
        // SAFETY: `raw` is live; the copy is a new tree owned by the result.
        NonNull::new(unsafe { ffi::ts_tree_copy(self.raw.as_ptr()) }).map(|raw| Tree { raw })
    }

    /// Moves the tree's positions through `edits`, in the order they
    /// happened, without parsing anything. What the edits touched keeps
    /// its old shape until the next parse; everything after them keeps its
    /// colour at its new offset.
    pub fn edit(&mut self, edits: &[crate::text::buffer::Edit]) {
        apply_edits(self, edits);
    }
}

/// [`Tree::edit`] through a shared reference, which tree-sitter allows: an
/// edit only changes what the next parse reuses and where nodes sit.
fn apply_edits(tree: &Tree, edits: &[crate::text::buffer::Edit]) {
    for edit in edits {
        let raw = ffi::TSInputEdit {
            start_byte: edit.start_byte as u32,
            old_end_byte: edit.old_end_byte as u32,
            new_end_byte: edit.new_end_byte as u32,
            start_point: point(edit.start_point),
            old_end_point: point(edit.old_end_point),
            new_end_point: point(edit.new_end_point),
        };
        // SAFETY: `tree` owns a live tree, and `raw` is valid for the call.
        unsafe { ffi::ts_tree_edit(tree.raw.as_ptr(), &raw) };
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
        // SAFETY: query is live for this whole function.
        let capture_kinds: Vec<Option<Kind>> = unsafe { ffi::capture_names(query.as_ptr()) }
            .into_iter()
            .map(Kind::from_capture)
            .collect();

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
            // SAFETY: query and tree are live for the duration.
            let Some(mut matches) = (unsafe {
                QueryMatches::new(
                    query.as_ptr(),
                    ffi::ts_tree_root_node(tree.raw.as_ptr()),
                    None,
                )
            }) else {
                continue;
            };
            let mut ranges = Vec::new();
            while let Some((_, captures)) = matches.next_match() {
                for capture in captures {
                    // SAFETY: capture nodes belong to the live tree.
                    let range = unsafe {
                        ffi::TSRange {
                            start_point: ffi::ts_node_start_point(capture.node),
                            end_point: ffi::ts_node_end_point(capture.node),
                            start_byte: ffi::ts_node_start_byte(capture.node),
                            end_byte: ffi::ts_node_end_byte(capture.node),
                        }
                    };
                    if range.end_byte > range.start_byte {
                        ranges.push(range);
                    }
                }
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
        apply_edits(old, edits);
        self.parse_from(rope, Some(old))
    }

    /// Parses a rope into a tree.
    pub fn parse(&mut self, rope: &Rope) -> Option<Tree> {
        self.parse_from(rope, None)
    }

    /// Parses `rope`, reusing what `old` says is unchanged. `old` has to
    /// have been moved through every edit since it was parsed already.
    pub fn parse_from(&mut self, rope: &Rope, old: Option<&Tree>) -> Option<Tree> {
        let mut state = ReadState::new(rope);
        let input = state.input();
        let old = old.map_or(std::ptr::null(), |tree| tree.raw.as_ptr() as *const _);
        // SAFETY: the parser is live, the input payload outlives the call
        // because `state` is on this stack frame, and `old` is either null,
        // for a fresh parse, or a live tree whose untouched subtrees are
        // reused.
        let raw = unsafe { ffi::ts_parser_parse(self.parser.as_ptr(), old, input) };
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
        // SAFETY: query and tree are live for the duration.
        let Some(mut matches) = (unsafe {
            QueryMatches::new(
                self.query.as_ptr(),
                ffi::ts_tree_root_node(tree.raw.as_ptr()),
                Some(range),
            )
        }) else {
            return Vec::new();
        };
        let mut out: Vec<(u16, Span)> = Vec::new();

        while let Some((pattern, captures)) = matches.next_match() {
            // Every predicate on this pattern has to hold for every
            // capture in the match, or the match is not really a match.
            let predicates = self
                .pattern_predicates
                .get(pattern as usize)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            if !predicates.is_empty() {
                // Text only for the captures a predicate is about: most
                // captures in a match are not, and copying each one's
                // text per match per frame was most of the cost.
                let satisfied = !predicates.iter().any(|p| p.capture().is_none())
                    && captures.iter().all(|capture| {
                        if !predicates
                            .iter()
                            .any(|p| p.capture() == Some(capture.index))
                        {
                            return true;
                        }
                        // SAFETY: capture nodes belong to the live tree.
                        let (start, end) = unsafe { node_bytes(capture.node) };
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
                // SAFETY: as above.
                let (start, end) = unsafe { node_bytes(capture.node) };
                if end > start {
                    out.push((pattern, Span { start, end, kind }));
                }
            }
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

/// The row and byte column of `byte` in `rope`.
fn point_at(rope: &Rope, byte: usize) -> ffi::TSPoint {
    let row = rope.byte_to_line(byte);
    point((row, byte - rope.line_to_byte(row)))
}

/// A row/column pair as tree-sitter's point type.
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
pub(crate) fn flatten(mut spans: Vec<Span>) -> Vec<Span> {
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
///
/// Small documents parse on the main thread, in [`SyntaxStore::update`].
/// Big ones parse on a worker, through [`SyntaxStore::update_async`] and
/// [`SyntaxStore::poll`]: the tree on screen is only moved through each
/// edit, which costs microseconds, and the parse lands a frame or a few
/// later.
///
/// [`Buffer::id`]: crate::text::buffer::Buffer::id
#[derive(Default)]
pub struct SyntaxStore {
    highlighters: Vec<Highlighter>,
    trees: std::collections::HashMap<u64, DocumentTrees>,
    /// The parse thread, started by the first document too big to parse
    /// here.
    worker: Option<worker::Worker>,
    /// The last generation handed to a job. Each job takes the next, so a
    /// result can only ever be taken for the job it answers: one sent for a
    /// document since closed, reloaded or renamed to another language finds
    /// a different generation, or no document, and is dropped.
    generation: u64,
}

/// Past this nothing is parsed, on any thread: a parse holds a tree about
/// the size of the text, and a first parse of a file this size already
/// takes seconds.
pub const PARSE_CAP_BYTES: usize = 64 * 1024 * 1024;

/// Everything parsed for one document.
struct DocumentTrees {
    /// The document's own grammar; none for Markdown, whose own styling
    /// is `markdown` and whose code blocks are layers.
    language: Option<Language>,
    tree: Option<Tree>,
    markdown: Option<crate::markdown::source::Styled>,
    /// One tree per language embedded in the document: the JavaScript in an
    /// HTML file's `<script>` elements, the CSS in its `<style>`. Each covers
    /// all of that language's pieces at once and nothing in between.
    layers: Vec<(Language, Tree)>,
    /// The parse on the worker, when one is running.
    pending: Option<Pending>,
}

/// A parse running on the worker for one document.
struct Pending {
    generation: u64,
    /// Edits made since the job's text, in order. Already applied to the
    /// trees on screen; owed to the result when it lands.
    edits: Vec<crate::text::buffer::Edit>,
    /// The text as of the last update, for the job that follows when
    /// `edits` is not empty.
    rope: Rope,
}

impl DocumentTrees {
    /// Moves every tree's positions through `edits`, without parsing.
    fn edit(&mut self, edits: &[crate::text::buffer::Edit]) {
        if edits.is_empty() {
            return;
        }
        if let Some(tree) = &mut self.tree {
            tree.edit(edits);
        }
        for (_, layer) in &mut self.layers {
            layer.edit(edits);
        }
    }
}

/// The highlighter for a language in `highlighters`, built on first use.
fn highlighter_in(
    highlighters: &mut Vec<Highlighter>,
    language: Language,
) -> Option<&mut Highlighter> {
    if !highlighters.iter().any(|h| h.language == language) {
        highlighters.push(Highlighter::new(language)?);
    }
    highlighters.iter_mut().find(|h| h.language == language)
}

/// Parses a document in `language`, and every language embedded in it.
///
/// `old` and `old_layers` are the last parse, already moved through every
/// edit since, for an incremental parse; `None` and empty for a fresh one.
/// The same edits apply to a layer as to the host: they are positions in the
/// one document all the layers are parsed out of.
fn parse_document(
    highlighters: &mut Vec<Highlighter>,
    language: Language,
    rope: &Rope,
    old: Option<&Tree>,
    old_layers: &[(Language, Tree)],
) -> Option<(Tree, Vec<(Language, Tree)>)> {
    let host = highlighter_in(highlighters, language)?;
    let tree = host.parse_from(rope, old)?;
    let injections = host.injection_ranges(&tree);
    let mut layers = Vec::new();
    for (embedded, ranges) in injections {
        let Some(highlighter) = highlighter_in(highlighters, embedded) else {
            continue;
        };
        let previous = old_layers
            .iter()
            .find(|(l, _)| *l == embedded)
            .map(|(_, tree)| (tree, &[][..]));
        if let Some(layer) = highlighter.parse_ranges(rope, &ranges, previous) {
            layers.push((embedded, layer));
        }
    }
    Some((tree, layers))
}

impl SyntaxStore {
    pub fn new() -> Self {
        SyntaxStore::default()
    }

    /// The highlighter for a language, built on first use.
    fn highlighter(&mut self, language: Language) -> Option<&mut Highlighter> {
        highlighter_in(&mut self.highlighters, language)
    }

    /// Brings `buffer`'s trees up to date with its text, on this thread.
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

        if buffer
            .extension()
            .is_some_and(|e| crate::markdown::is_markdown_extension(&e))
        {
            self.update_markdown(id, rope, edits.as_deref(), budget);
            return;
        }
        let language = buffer
            .extension()
            .and_then(|e| Language::from_extension(&e));
        let Some(language) = language.filter(|_| rope.len_bytes() <= budget) else {
            self.trees.remove(&id);
            return;
        };

        // Taken out of the map, which also drops any parse still running
        // for it on the worker. Incremental only onto this document's own
        // trees, in the same language, with a complete account of what
        // changed since.
        let mut old = self
            .trees
            .remove(&id)
            .filter(|t| t.language == Some(language) && t.tree.is_some() && edits.is_some());
        if let (Some(old), Some(edits)) = (&mut old, &edits) {
            old.edit(edits);
        }
        let parsed = parse_document(
            &mut self.highlighters,
            language,
            rope,
            old.as_ref().and_then(|t| t.tree.as_ref()),
            old.as_ref().map_or(&[][..], |t| &t.layers),
        );
        let Some((tree, layers)) = parsed else {
            return;
        };
        self.trees.insert(
            id,
            DocumentTrees {
                language: Some(language),
                tree: Some(tree),
                markdown: None,
                layers,
                pending: None,
            },
        );
    }

    /// As [`SyntaxStore::update`], but a document past `edit_budget` parses
    /// its edits on the worker instead of here.
    ///
    /// What this thread does for one is move the trees on screen through
    /// the edits, so what is drawn until the parse lands is the last parse
    /// at its new offsets, and hand the worker the text and a copy of those
    /// trees. A parse already running is not interrupted: edits made while
    /// it runs are kept, and [`SyntaxStore::poll`] replays them onto its
    /// result and sends the next job.
    ///
    /// A document's first parse runs here up to `open_budget`, so a file
    /// that size opens coloured rather than plain for a moment; past it the
    /// first parse runs on the worker too, and the colour lands with it.
    /// Markdown parses here whatever its size, up to its own cap.
    pub fn update_async(
        &mut self,
        buffer: &mut crate::text::buffer::Buffer,
        edit_budget: usize,
        open_budget: usize,
    ) {
        let len = buffer.rope.len_bytes();
        let extension = buffer.extension();
        let markdown = extension
            .as_deref()
            .is_some_and(crate::markdown::is_markdown_extension);
        let language = extension.as_deref().and_then(Language::from_extension);
        // Parsed, or being parsed, in this language: what comes now is an
        // edit, not an opening.
        let opened = self.trees.get(&buffer.id()).is_some_and(|t| {
            t.language.is_some()
                && t.language == language
                && (t.tree.is_some() || t.pending.is_some())
        });
        let budget = if opened {
            edit_budget
        } else {
            open_budget.max(edit_budget)
        };
        let Some(language) =
            language.filter(|_| !markdown && len > budget && len <= PARSE_CAP_BYTES)
        else {
            // Every case that parses here is within this; the rest, too
            // big or without a grammar, lose their trees.
            self.update(buffer, open_budget.max(edit_budget).min(PARSE_CAP_BYTES));
            return;
        };
        let edits = buffer.drain_edits();
        let id = buffer.id();
        // The spans are queried here, so this thread needs the query too.
        if self.highlighter(language).is_none() {
            self.trees.remove(&id);
            return;
        }
        // Another language's trees are no help, and its job is stale.
        let mut doc = self
            .trees
            .remove(&id)
            .filter(|t| t.language == Some(language))
            .unwrap_or(DocumentTrees {
                language: Some(language),
                tree: None,
                markdown: None,
                layers: Vec::new(),
                pending: None,
            });
        match edits {
            Some(edits) => {
                doc.edit(&edits);
                match &mut doc.pending {
                    Some(pending) => {
                        pending.edits.extend(edits);
                        pending.rope = buffer.rope.clone();
                    }
                    None if edits.is_empty() && doc.tree.is_some() => {}
                    None => self.queue(id, &mut doc, &buffer.rope, false),
                }
            }
            // Nothing leads from the trees to this text. They stay drawn,
            // the nearest thing to right there is, and the parse from
            // scratch replaces any job in flight, whose text is gone.
            None => self.queue(id, &mut doc, &buffer.rope, true),
        }
        self.trees.insert(id, doc);
    }

    /// Sends `doc`'s next parse to the worker, from its trees on screen or,
    /// `from_scratch`, from nothing. Without a worker the document keeps
    /// what it has and nothing is pending.
    fn queue(&mut self, id: u64, doc: &mut DocumentTrees, rope: &Rope, from_scratch: bool) {
        doc.pending = None;
        let Some(language) = doc.language else {
            return;
        };
        let (old, old_layers) = if from_scratch {
            (None, Vec::new())
        } else {
            (
                doc.tree.as_ref().and_then(Tree::copy),
                doc.layers
                    .iter()
                    .filter_map(|(l, tree)| Some((*l, tree.copy()?)))
                    .collect(),
            )
        };
        self.generation += 1;
        let job = worker::Job {
            id,
            generation: self.generation,
            language,
            rope: rope.clone(),
            old,
            old_layers,
        };
        if self.send(job) {
            doc.pending = Some(Pending {
                generation: self.generation,
                edits: Vec::new(),
                rope: rope.clone(),
            });
        }
    }

    /// Hands `job` to the worker, starting one if there is none or the one
    /// there was has gone. False when no thread could be started.
    fn send(&mut self, job: worker::Job) -> bool {
        let job = match &self.worker {
            Some(worker) => match worker.send(job) {
                Ok(()) => return true,
                Err(job) => job,
            },
            None => job,
        };
        // None yet, or the thread is gone: a fresh one.
        self.worker = worker::Worker::spawn();
        self.worker.as_ref().is_some_and(|w| w.send(job).is_ok())
    }

    /// Takes in whatever the worker has finished. True when a document's
    /// trees changed, which means a redraw. Called every refresh while
    /// [`SyntaxStore::busy`].
    pub fn poll(&mut self) -> bool {
        let mut landed = false;
        while let Some(done) = self.worker.as_ref().and_then(|w| w.done.try_recv().ok()) {
            landed |= self.land(done);
        }
        landed
    }

    /// Installs one finished parse, if it is still the one its document is
    /// waiting for, and sends the next when the text moved on meanwhile.
    fn land(&mut self, done: worker::Done) -> bool {
        let Some(mut doc) = self.trees.remove(&done.id) else {
            return false;
        };
        let pending = match doc.pending.take() {
            Some(pending) if pending.generation == done.generation => pending,
            other => {
                doc.pending = other;
                self.trees.insert(done.id, doc);
                return false;
            }
        };
        if let Some(tree) = done.tree {
            let replaced = doc.tree.replace(tree);
            let replaced_layers = std::mem::replace(&mut doc.layers, done.layers);
            if let Some(worker) = &self.worker {
                worker.free(
                    replaced
                        .into_iter()
                        .chain(replaced_layers.into_iter().map(|(_, tree)| tree))
                        .collect(),
                );
            }
            for (language, _) in &doc.layers {
                let _ = highlighter_in(&mut self.highlighters, *language);
            }
            // The result describes the text the job was sent with; what was
            // typed since moves it to now, as it moved the trees it replaces.
            doc.edit(&pending.edits);
        }
        if !pending.edits.is_empty() {
            self.queue(done.id, &mut doc, &pending.rope, false);
        }
        self.trees.insert(done.id, doc);
        true
    }

    /// A parse is running on the worker, so [`SyntaxStore::poll`] has
    /// something to wait for.
    pub fn busy(&self) -> bool {
        self.trees.values().any(|t| t.pending.is_some())
    }

    /// A Markdown document: its own styling, and a layer per language its
    /// fenced code blocks name.
    fn update_markdown(
        &mut self,
        id: u64,
        rope: &Rope,
        edits: Option<&[crate::text::buffer::Edit]>,
        budget: usize,
    ) {
        let old = self.trees.remove(&id).filter(|t| t.markdown.is_some());
        // Styling reads the whole text on every edit; past this a Markdown
        // file is plain text rather than a slow one.
        if rope.len_bytes() > budget.min(256 * 1024) {
            return;
        }
        let source = rope.to_string();
        let styled = crate::markdown::source::style(&source);
        let mut by_language: Vec<(Language, Vec<ffi::TSRange>)> = Vec::new();
        for fence in &styled.fences {
            let Some(language) = fence.language else {
                continue;
            };
            let range = ffi::TSRange {
                start_point: point_at(rope, fence.content.start),
                end_point: point_at(rope, fence.content.end),
                start_byte: fence.content.start as u32,
                end_byte: fence.content.end as u32,
            };
            match by_language.iter_mut().find(|(l, _)| *l == language) {
                Some((_, ranges)) => ranges.push(range),
                None => by_language.push((language, vec![range])),
            }
        }
        let mut layers = Vec::new();
        for (language, ranges) in by_language {
            let previous = old.as_ref().zip(edits).and_then(|(trees, edits)| {
                let (_, tree) = trees.layers.iter().find(|(l, _)| *l == language)?;
                Some((tree, edits))
            });
            let Some(highlighter) = self.highlighter(language) else {
                continue;
            };
            if let Some(layer) = highlighter.parse_ranges(rope, &ranges, previous) {
                layers.push((language, layer));
            }
        }
        self.trees.insert(
            id,
            DocumentTrees {
                language: None,
                tree: None,
                markdown: Some(styled),
                layers,
                pending: None,
            },
        );
    }

    /// A Markdown document's styling: code bands, hidden syntax, table pads.
    pub fn markdown(&self, id: u64) -> Option<&crate::markdown::source::Styled> {
        self.trees.get(&id).and_then(|t| t.markdown.as_ref())
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
        let host = trees.language.zip(trees.tree.as_ref());
        let all = host.into_iter().chain(
            trees
                .layers
                .iter()
                .map(|(language, tree)| (*language, tree)),
        );

        let mut spans = Vec::new();
        if let Some(markdown) = &trees.markdown {
            let first = markdown.spans.partition_point(|s| s.end <= range.start);
            spans.extend(
                markdown.spans[first..]
                    .iter()
                    .take_while(|s| s.start < range.end)
                    .copied(),
            );
        }
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

    /// Multi-cursor edits are recorded one per cursor, back to front, and
    /// replay to the same tree a full parse gives.
    #[test]
    fn multi_cursor_edits_parse_incrementally() {
        use crate::text::buffer::{Buffer, Motion};

        let mut h = rust();
        let source = "fn a() { f(1); }\nfn b() { f(2); }\nfn c() { f(3); }\n";
        let mut buffer = Buffer::from_text(source);
        let mut tree = h.parse(&buffer.rope).expect("initial parse");
        buffer.drain_edits();
        let calls: Vec<usize> = source.match_indices("f(").map(|(at, _)| at + 2).collect();
        let mut step = |buffer: &mut Buffer, edit: &dyn Fn(&mut Buffer)| {
            edit(buffer);
            let edits = buffer.drain_edits().expect("edits should be replayable");
            let incremental = h
                .parse_incremental(&buffer.rope, &tree, &edits)
                .expect("incremental parse");
            let full = h.parse(&buffer.rope).expect("full parse");
            let range = 0..buffer.rope.len_bytes();
            assert_eq!(
                h.spans(&incremental, range.clone()),
                h.spans(&full, range),
                "incremental and full parses disagreed after {:?}",
                buffer.rope.to_string()
            );
            tree = incremental;
        };
        // Typing at three carets, a backspace at each, a selection at each
        // replaced, and an empty pair deleted around each.
        step(&mut buffer, &|b| {
            b.place_cursor(calls[0], Motion::Move);
            b.add_cursor(calls[1], calls[1]);
            b.add_cursor(calls[2], calls[2]);
            b.insert("x + ");
        });
        step(&mut buffer, &|b| b.backspace());
        step(&mut buffer, &|b| {
            let text = b.rope.to_string();
            let at: Vec<usize> = text.match_indices("x +").map(|(at, _)| at).collect();
            b.select_range(at[0], at[0] + 3);
            b.add_cursor(at[1], at[1] + 3);
            b.add_cursor(at[2], at[2] + 3);
            b.insert("\"()\"");
        });
        step(&mut buffer, &|b| {
            let text = b.rope.to_string();
            let at: Vec<usize> = text.match_indices("()").map(|(at, _)| at + 1).collect();
            b.place_cursor(at[1], Motion::Move);
            b.add_cursor(at[2], at[2]);
            b.add_cursor(at[3], at[3]);
            b.backspace_paired();
        });
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
    fn undo_and_redo_are_described_as_one_edit_and_parse_incrementally() {
        let mut store = SyntaxStore::new();
        let mut buffer = rust_buffer("fn a() {}\nfn b() { let 日本 = 1; }\n", "u.rs");
        store.update(&mut buffer, BUDGET);
        buffer.place_cursor(
            buffer.rope.len_bytes() - 4,
            crate::text::buffer::Motion::Move,
        );
        buffer.insert("struct S;\n");
        store.update(&mut buffer, BUDGET);
        buffer.undo();
        let edits = buffer
            .drain_edits()
            .expect("undo is an edit, not a full reparse");
        assert_eq!(edits.len(), 1);
        buffer.redo();
        buffer.undo();
        store.update(&mut buffer, BUDGET);
        assert_store_matches_a_full_parse(&store, &buffer);
        buffer.redo();
        store.update(&mut buffer, BUDGET);
        assert_store_matches_a_full_parse(&store, &buffer);
    }

    /// Stepping a document through versions of a file: each one is a single
    /// edit, and the tree it parses to is the one a full parse gives.
    #[test]
    fn a_version_shown_in_place_parses_incrementally() {
        let mut store = SyntaxStore::new();
        let mut buffer = rust_buffer("fn a() {}\nfn b() { let 日本 = 1; }\n", "v.rs");
        buffer.set_frozen(true);
        store.update(&mut buffer, BUDGET);
        for version in [
            "fn a() {}\nstruct S;\nfn b() { let 日本 = 1; }\n",
            "fn a() { /* 本 */ }\n",
            "",
            "fn c(x: u8) -> u8 { x }\n",
        ] {
            // The buffer's own tests check this is one edit, not a full
            // parse; here, that the parse it leads to is right.
            buffer.show_text(version, None, 1, 4);
            store.update(&mut buffer, BUDGET);
            assert_store_matches_a_full_parse(&store, &buffer);
        }
    }

    /// The same with the history's diffs: an edit per change, several in
    /// one step, and the tree still the one a full parse gives.
    #[test]
    fn a_version_shown_with_its_diff_parses_to_the_same_tree() {
        let versions = [
            "fn a() {}\nfn b() { let 日本 = 1; }\nfn c() {}\n",
            "struct S;\nfn a() {}\nfn b() { let 日本 = 2; }\nfn c() {}\nfn d() {}\n",
            "struct S;\nfn b() { let 日本 = 2; }\nfn c() { /* 本 */ }\nfn d() {}",
            "fn b() {}\nfn c() { /* 本 */ }\n",
            "",
            "fn e(x: u8) -> u8 { x }\n",
        ];
        let mut store = SyntaxStore::new();
        let mut buffer = rust_buffer(versions[0], "v.rs");
        buffer.set_frozen(true);
        store.update(&mut buffer, BUDGET);
        // Forward through them, then back, as the scrubber steps.
        let order: Vec<usize> = (1..versions.len())
            .chain((0..versions.len() - 1).rev())
            .collect();
        let mut shown = 0;
        for next in order {
            let changes = crate::ide::diff::changes(versions[shown], versions[next]);
            buffer.show_text(versions[next], Some(&changes), 0, 0);
            store.update(&mut buffer, BUDGET);
            assert_store_matches_a_full_parse(&store, &buffer);
            shown = next;
        }
    }

    // ---- parsing on the worker ---------------------------------------------

    /// A sync budget that sends every document to the worker.
    const ON_WORKER: usize = 0;

    /// Waits for the worker's next result and takes it in, as `poll` does
    /// once one is there. True when it was installed.
    fn land_next(store: &mut SyntaxStore) -> bool {
        let done = store
            .worker
            .as_ref()
            .expect("a worker")
            .done
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the worker answers");
        store.land(done)
    }

    /// Takes in results until nothing is waiting on the worker.
    fn settle(store: &mut SyntaxStore) {
        while store.busy() {
            land_next(store);
        }
    }

    fn kinds_at(
        store: &SyntaxStore,
        buffer: &crate::text::buffer::Buffer,
        needle: &str,
    ) -> Vec<Kind> {
        let at = buffer.rope.to_string().find(needle).expect("present");
        store
            .spans_with(buffer.id(), at..at + needle.len(), |r| {
                buffer.rope.slice_to_string(r)
            })
            .into_iter()
            .filter(|s| s.start == at && s.end == at + needle.len())
            .map(|s| s.kind)
            .collect()
    }

    #[test]
    fn async_parse_matches_a_full_parse() {
        use crate::text::buffer::Motion;

        let mut store = SyntaxStore::new();
        let text = "fn alpha() { let s = \"caf\u{e9}\"; }\n".repeat(400);
        let mut buffer = rust_buffer(&text, "big.rs");
        store.update_async(&mut buffer, ON_WORKER, ON_WORKER);
        assert!(store.busy(), "the first parse runs on the worker");
        assert!(
            kinds_at(&store, &buffer, "fn").is_empty(),
            "plain until it lands"
        );
        settle(&mut store);
        assert_store_matches_a_full_parse(&store, &buffer);

        // Each keystroke is a job of its own once the last one has landed.
        buffer.place_cursor(40, Motion::Move);
        for piece in ["struct Typed;\n", "// note\n", "const N: u8 = 1;"] {
            buffer.insert(piece);
            store.update_async(&mut buffer, ON_WORKER, ON_WORKER);
            assert!(store.busy());
            settle(&mut store);
            assert_store_matches_a_full_parse(&store, &buffer);
        }
        buffer.backspace();
        store.update_async(&mut buffer, ON_WORKER, ON_WORKER);
        settle(&mut store);
        assert_store_matches_a_full_parse(&store, &buffer);

        // Nothing typed, nothing sent.
        store.update_async(&mut buffer, ON_WORKER, ON_WORKER);
        assert!(!store.busy());

        // An HTML page's embedded languages parse there as well.
        let mut page = rust_buffer(PAGE, "index.html");
        store.update_async(&mut page, ON_WORKER, ON_WORKER);
        settle(&mut store);
        assert_store_matches_a_full_parse(&store, &page);
        let at = PAGE.find("const").expect("script body");
        page.place_cursor(at, Motion::Move);
        page.insert("let early = \"s\";\n");
        store.update_async(&mut page, ON_WORKER, ON_WORKER);
        settle(&mut store);
        assert_store_matches_a_full_parse(&store, &page);
    }

    /// The window's budgets: edits past 64 KiB on the worker, first parses
    /// here up to 512 KiB.
    const EDITS: usize = 64 * 1024;
    const OPENS: usize = 512 * 1024;

    #[test]
    fn a_mid_sized_document_opens_here_and_reparses_on_the_worker() {
        let mut store = SyntaxStore::new();
        let text = "fn alpha() { let s = \"caf\u{e9}\"; }\n".repeat(3300);
        assert!(text.len() > 100 * 1024 && text.len() < OPENS);
        let mut buffer = rust_buffer(&text, "mid.rs");

        // Opened: coloured on the first frame, nothing on the worker.
        store.update_async(&mut buffer, EDITS, OPENS);
        assert!(!store.busy(), "the first parse is synchronous");
        assert_store_matches_a_full_parse(&store, &buffer);

        // Typed in: the parse goes to the worker.
        buffer.insert("struct Typed;\n");
        store.update_async(&mut buffer, EDITS, OPENS);
        assert!(store.busy(), "a keystroke past 64 KiB parses on the worker");
        settle(&mut store);
        assert_store_matches_a_full_parse(&store, &buffer);

        // Under 64 KiB everything stays here.
        let mut small = rust_buffer("fn small() {}\n", "small.rs");
        store.update_async(&mut small, EDITS, OPENS);
        small.insert("// typed\n");
        store.update_async(&mut small, EDITS, OPENS);
        assert!(!store.busy());
        assert_store_matches_a_full_parse(&store, &small);
    }

    #[test]
    fn edits_during_a_parse_are_replayed() {
        use crate::text::buffer::Motion;

        let mut store = SyntaxStore::new();
        let mut buffer = rust_buffer(&"fn f() { g(1); }\n".repeat(300), "typed.rs");
        store.update_async(&mut buffer, ON_WORKER, ON_WORKER);
        settle(&mut store);

        // The first keystroke sends a job; the rest arrive while it runs,
        // and are only applied to the tree on screen.
        buffer.place_cursor(0, Motion::Move);
        buffer.insert("// head\n");
        store.update_async(&mut buffer, ON_WORKER, ON_WORKER);
        let end = buffer.rope.len_bytes();
        buffer.place_cursor(end, Motion::Move);
        buffer.insert("struct Tail;\n");
        store.update_async(&mut buffer, ON_WORKER, ON_WORKER);
        buffer.place_cursor(8, Motion::Move);
        buffer.delete_word_forward();
        store.update_async(&mut buffer, ON_WORKER, ON_WORKER);
        buffer.place_cursor(3, Motion::Move);
        buffer.add_cursor(30, 30);
        buffer.insert("\u{65e5}");
        store.update_async(&mut buffer, ON_WORKER, ON_WORKER);

        // What is drawn meanwhile is the old parse at its new offsets: the
        // second function's keyword moved with the text in front of it.
        let second = buffer.rope.to_string().find("}\nfn").expect("second fn") + 2;
        let spans = store.spans_with(buffer.id(), second..second + 2, |r| {
            buffer.rope.slice_to_string(r)
        });
        assert!(
            spans
                .iter()
                .any(|s| s.start == second && s.end == second + 2 && s.kind == Kind::Keyword),
            "the shown tree follows the edits: {spans:?}"
        );

        assert!(land_next(&mut store), "the first job's result is taken");
        assert!(
            store.busy(),
            "and the edits made since go out as the next job"
        );
        settle(&mut store);
        assert_store_matches_a_full_parse(&store, &buffer);
    }

    #[test]
    fn a_stale_result_is_dropped() {
        let mut store = SyntaxStore::new();
        let text = "fn f() { let s = 1; }\n".repeat(300);
        let mut buffer = rust_buffer(&text, "stale.rs");

        // Reloaded while its parse runs: no edits lead to the new text, so
        // a parse from scratch replaces the one in flight.
        store.update_async(&mut buffer, ON_WORKER, ON_WORKER);
        buffer.regenerate(&"struct Reloaded { n: u8 }\n".repeat(200));
        store.update_async(&mut buffer, ON_WORKER, ON_WORKER);
        assert!(!land_next(&mut store), "the first job's text is gone");
        assert!(land_next(&mut store));
        assert!(!store.busy());
        assert_store_matches_a_full_parse(&store, &buffer);

        // Renamed to another language while it parses.
        buffer.insert("// typed\n");
        store.update_async(&mut buffer, ON_WORKER, ON_WORKER);
        buffer.path = Some(std::path::PathBuf::from("stale.js"));
        buffer.insert("const later = 1;\n");
        store.update_async(&mut buffer, ON_WORKER, ON_WORKER);
        assert!(!land_next(&mut store), "a Rust tree for a JavaScript file");
        settle(&mut store);
        assert_store_matches_a_full_parse(&store, &buffer);

        // Parsed here while the worker still has it, after shrinking.
        buffer.insert("let more = 2;\n");
        store.update_async(&mut buffer, ON_WORKER, ON_WORKER);
        store.update(&mut buffer, BUDGET);
        assert!(!store.busy());
        assert!(!land_next(&mut store), "the main thread's parse is newer");
        assert_store_matches_a_full_parse(&store, &buffer);
    }

    #[test]
    fn a_closed_documents_result_is_dropped() {
        let mut store = SyntaxStore::new();
        let mut kept = rust_buffer(&"fn kept() {}\n".repeat(300), "kept.rs");
        let mut closed = rust_buffer(&"fn closed() {}\n".repeat(300), "closed.rs");
        store.update_async(&mut kept, ON_WORKER, ON_WORKER);
        store.update_async(&mut closed, ON_WORKER, ON_WORKER);
        store.retain(|id| id == kept.id());
        assert!(store.busy(), "the open one still waits");

        // In the order they were sent.
        assert!(land_next(&mut store));
        assert!(!land_next(&mut store), "nothing to install it into");
        assert!(!store.has(closed.id()));
        assert!(!store.busy());
        assert_store_matches_a_full_parse(&store, &kept);
    }

    #[test]
    fn an_empty_buffer_parses_to_nothing() {
        let mut h = rust();
        let rope = Rope::new();
        let tree = h.parse(&rope).expect("parse");
        assert!(h.spans(&tree, 0..0).is_empty());
    }
}
