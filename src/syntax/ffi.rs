//! Minimal hand-written bindings to the vendored tree-sitter C runtime.
//!
//! Deliberately not generated. The surface an editor needs is about twenty
//! functions, and writing them out means no `bindgen` (which would pull in
//! libclang) and no generated code nobody reads. Everything here is declared
//! against `third_party/tree-sitter/include/tree_sitter/api.h`, which is
//! checked in, so the declarations and the definitions cannot drift.
//!
//! The safe wrappers live in the parent module; this file is only the raw
//! boundary.

use std::ffi::{c_char, c_void};

/// Opaque C types. Represented as private zero-variant enums so a pointer to
/// one cannot be dereferenced or constructed by accident.
#[repr(C)]
pub struct TSParser {
    _private: [u8; 0],
}

#[repr(C)]
pub struct TSTree {
    _private: [u8; 0],
}

#[repr(C)]
pub struct TSLanguage {
    _private: [u8; 0],
}

#[repr(C)]
pub struct TSQuery {
    _private: [u8; 0],
}

#[repr(C)]
pub struct TSQueryCursor {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TSPoint {
    pub row: u32,
    pub column: u32,
}

/// A stretch of the document a parser should look at. Both forms of each end
/// are required, exactly as in `TSInputEdit`. Field order is the header's.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TSRange {
    pub start_point: TSPoint,
    pub end_point: TSPoint,
    pub start_byte: u32,
    pub end_byte: u32,
}

/// A node in the parse tree. Passed and returned by value, as the C API does.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TSNode {
    pub context: [u32; 4],
    pub id: *const c_void,
    pub tree: *const TSTree,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TSQueryCapture {
    pub node: TSNode,
    pub index: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TSQueryMatch {
    pub id: u32,
    pub pattern_index: u16,
    pub capture_count: u16,
    pub captures: *const TSQueryCapture,
}

/// Edit applied to a tree before re-parsing, so tree-sitter can reuse the
/// unchanged parts instead of starting over.
///
/// Bound but not yet used: full re-parses are comfortably inside the frame
/// budget at the sizes this editor currently highlights, and incremental
/// parsing means tracking every edit's byte and point deltas. Kept here
/// because it is the documented path once a file large enough to need it
/// turns up, and because the struct layout is part of the ABI either way.
#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TSInputEdit {
    pub start_byte: u32,
    pub old_end_byte: u32,
    pub new_end_byte: u32,
    pub start_point: TSPoint,
    pub old_end_point: TSPoint,
    pub new_end_point: TSPoint,
}

/// One step of a query predicate, e.g. `(#match? @type "^[A-Z]")`.
///
/// Steps arrive as a flat list terminated by a `Done` step: the predicate
/// name is the first string, then alternating captures and strings.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct TSQueryPredicateStep {
    pub kind: u32,
    pub value_id: u32,
}

impl TSQueryPredicateStep {
    pub const DONE: u32 = 0;
    pub const CAPTURE: u32 = 1;
    pub const STRING: u32 = 2;
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TSQueryError(pub u32);

impl TSQueryError {
    pub const NONE: TSQueryError = TSQueryError(0);
}

/// UTF-8, which is the only encoding this editor stores.
pub const TS_INPUT_ENCODING_UTF8: u32 = 0;

/// Callback shape for reading buffer text during a parse.
pub type TSReadFn = unsafe extern "C" fn(
    payload: *mut c_void,
    byte_index: u32,
    position: TSPoint,
    bytes_read: *mut u32,
) -> *const c_char;

#[repr(C)]
pub struct TSInput {
    pub payload: *mut c_void,
    pub read: Option<TSReadFn>,
    pub encoding: u32,
    /// Added in a later API revision; unused, but the struct layout must
    /// match what the compiled C expects.
    pub decode: *const c_void,
}

unsafe extern "C" {
    pub fn ts_parser_new() -> *mut TSParser;
    pub fn ts_parser_delete(parser: *mut TSParser);
    pub fn ts_parser_set_language(parser: *mut TSParser, language: *const TSLanguage) -> bool;
    pub fn ts_parser_parse(
        parser: *mut TSParser,
        old_tree: *const TSTree,
        input: TSInput,
    ) -> *mut TSTree;

    pub fn ts_tree_delete(tree: *mut TSTree);
    pub fn ts_tree_root_node(tree: *const TSTree) -> TSNode;
    /// See [`TSInputEdit`] for why this is bound but unused.
    #[allow(dead_code)]
    pub fn ts_tree_edit(tree: *mut TSTree, edit: *const TSInputEdit);

    /// Restricts the next parse to `ranges`, which is how one language is
    /// parsed out of the middle of another. A count of zero lifts it again.
    /// False if the ranges overlap or are out of order.
    pub fn ts_parser_set_included_ranges(
        parser: *mut TSParser,
        ranges: *const TSRange,
        count: u32,
    ) -> bool;

    pub fn ts_node_start_byte(node: TSNode) -> u32;
    pub fn ts_node_end_byte(node: TSNode) -> u32;
    pub fn ts_node_start_point(node: TSNode) -> TSPoint;
    pub fn ts_node_end_point(node: TSNode) -> TSPoint;
    pub fn ts_node_has_error(node: TSNode) -> bool;

    pub fn ts_query_new(
        language: *const TSLanguage,
        source: *const c_char,
        source_len: u32,
        error_offset: *mut u32,
        error_type: *mut TSQueryError,
    ) -> *mut TSQuery;
    pub fn ts_query_delete(query: *mut TSQuery);
    pub fn ts_query_capture_count(query: *const TSQuery) -> u32;
    pub fn ts_query_capture_name_for_id(
        query: *const TSQuery,
        index: u32,
        length: *mut u32,
    ) -> *const c_char;

    pub fn ts_query_pattern_count(query: *const TSQuery) -> u32;
    pub fn ts_query_predicates_for_pattern(
        query: *const TSQuery,
        pattern_index: u32,
        step_count: *mut u32,
    ) -> *const TSQueryPredicateStep;
    pub fn ts_query_string_value_for_id(
        query: *const TSQuery,
        id: u32,
        length: *mut u32,
    ) -> *const c_char;

    pub fn ts_query_cursor_new() -> *mut TSQueryCursor;
    pub fn ts_query_cursor_delete(cursor: *mut TSQueryCursor);
    pub fn ts_query_cursor_set_byte_range(cursor: *mut TSQueryCursor, start: u32, end: u32);
    pub fn ts_query_cursor_exec(cursor: *mut TSQueryCursor, query: *const TSQuery, node: TSNode);
    pub fn ts_query_cursor_next_match(
        cursor: *mut TSQueryCursor,
        match_out: *mut TSQueryMatch,
    ) -> bool;

    /// Provided by the compiled grammars, not by the runtime.
    pub fn tree_sitter_rust() -> *const TSLanguage;
    pub fn tree_sitter_html() -> *const TSLanguage;
    pub fn tree_sitter_javascript() -> *const TSLanguage;
    pub fn tree_sitter_css() -> *const TSLanguage;
    pub fn tree_sitter_json() -> *const TSLanguage;
    pub fn tree_sitter_typescript() -> *const TSLanguage;
    pub fn tree_sitter_tsx() -> *const TSLanguage;
    pub fn tree_sitter_python() -> *const TSLanguage;
    pub fn tree_sitter_c() -> *const TSLanguage;
    pub fn tree_sitter_cpp() -> *const TSLanguage;
    pub fn tree_sitter_go() -> *const TSLanguage;
    pub fn tree_sitter_toml() -> *const TSLanguage;
    pub fn tree_sitter_yaml() -> *const TSLanguage;
    pub fn tree_sitter_bash() -> *const TSLanguage;
}
