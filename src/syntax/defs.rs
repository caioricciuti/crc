//! Names a file defines, for the project index: functions, types,
//! constants, fields, modules, with their line.
//!
//! Our own short queries, one per language, rather than the grammars'
//! `tags.scm`: only two of the vendored grammars ship one, and those lean on
//! predicates this editor does not implement. A capture's name is the kind.
//! Runs on any thread: it makes its own parser and query each call, which
//! costs microseconds against the parse itself.

use std::ffi::c_void;
use std::ptr::NonNull;

use super::{Language, ReadState, ffi, read_rope};
use crate::text::rope::Rope;

/// One definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Definition {
    pub name: String,
    /// `function`, `type`, `constant`, `field` or `module`.
    pub kind: &'static str,
    /// Zero-based.
    pub line: u32,
}

const KINDS: [&str; 5] = ["function", "type", "constant", "field", "module"];

fn query(language: Language) -> Option<&'static str> {
    Some(match language {
        Language::Rust => {
            "(function_item name: (identifier) @function)
             (function_signature_item name: (identifier) @function)
             (macro_definition name: (identifier) @function)
             (struct_item name: (type_identifier) @type)
             (enum_item name: (type_identifier) @type)
             (union_item name: (type_identifier) @type)
             (trait_item name: (type_identifier) @type)
             (type_item name: (type_identifier) @type)
             (const_item name: (identifier) @constant)
             (static_item name: (identifier) @constant)
             (enum_variant name: (identifier) @constant)
             (field_declaration name: (field_identifier) @field)
             (mod_item name: (identifier) @module)"
        }
        Language::Python => {
            "(function_definition name: (identifier) @function)
             (class_definition name: (identifier) @type)
             (module (expression_statement (assignment left: (identifier) @constant)))"
        }
        Language::Go => {
            "(function_declaration name: (identifier) @function)
             (method_declaration name: (field_identifier) @function)
             (type_spec name: (type_identifier) @type)
             (const_spec name: (identifier) @constant)
             (var_spec name: (identifier) @constant)
             (field_declaration name: (field_identifier) @field)"
        }
        Language::C => {
            "(function_declarator declarator: (identifier) @function)
             (preproc_function_def name: (identifier) @function)
             (struct_specifier name: (type_identifier) @type)
             (enum_specifier name: (type_identifier) @type)
             (union_specifier name: (type_identifier) @type)
             (type_definition declarator: (type_identifier) @type)
             (preproc_def name: (identifier) @constant)
             (enumerator name: (identifier) @constant)
             (field_declaration declarator: (field_identifier) @field)"
        }
        Language::Cpp => {
            "(function_declarator declarator: (identifier) @function)
             (function_declarator declarator: (field_identifier) @function)
             (function_declarator declarator: (qualified_identifier name: (identifier) @function))
             (preproc_function_def name: (identifier) @function)
             (class_specifier name: (type_identifier) @type)
             (struct_specifier name: (type_identifier) @type)
             (enum_specifier name: (type_identifier) @type)
             (type_definition declarator: (type_identifier) @type)
             (preproc_def name: (identifier) @constant)
             (enumerator name: (identifier) @constant)
             (field_declaration declarator: (field_identifier) @field)
             (namespace_definition name: (namespace_identifier) @module)"
        }
        Language::JavaScript => {
            "(function_declaration name: (identifier) @function)
             (generator_function_declaration name: (identifier) @function)
             (method_definition name: (property_identifier) @function)
             (class_declaration name: (identifier) @type)
             (variable_declarator name: (identifier) @constant)"
        }
        Language::TypeScript | Language::Tsx => {
            "(function_declaration name: (identifier) @function)
             (generator_function_declaration name: (identifier) @function)
             (method_definition name: (property_identifier) @function)
             (method_signature name: (property_identifier) @function)
             (class_declaration name: (type_identifier) @type)
             (abstract_class_declaration name: (type_identifier) @type)
             (interface_declaration name: (type_identifier) @type)
             (type_alias_declaration name: (type_identifier) @type)
             (enum_declaration name: (identifier) @type)
             (variable_declarator name: (identifier) @constant)
             (public_field_definition name: (property_identifier) @field)
             (property_signature name: (property_identifier) @field)"
        }
        Language::Bash => {
            "(function_definition name: (word) @function)
             (variable_assignment name: (variable_name) @constant)"
        }
        Language::Toml => "(pair (bare_key) @field) (table (bare_key) @module)",
        Language::Css => "(class_selector (class_name) @type) (id_selector (id_name) @type)",
        Language::Html | Language::Json | Language::Yaml => return None,
    })
}

/// Whether the query for `language` compiles against its grammar. A test
/// asks this of every language, so a node name typed wrong fails there
/// rather than quietly indexing nothing.
pub fn query_compiles(language: Language) -> bool {
    match query(language) {
        Some(source) => super::compile_query(language, source).is_some_and(|q| {
            unsafe { ffi::ts_query_delete(q.as_ptr()) };
            true
        }),
        None => true,
    }
}

/// What `text` defines, in document order. Empty for a language without a
/// query or one that fails to parse.
pub fn definitions(language: Language, text: &str) -> Vec<Definition> {
    let Some(source) = query(language) else {
        return Vec::new();
    };
    let Some(query) = super::compile_query(language, source) else {
        return Vec::new();
    };
    let rope = Rope::from_text(text);
    let mut out = Vec::new();
    unsafe {
        let parser = ffi::ts_parser_new();
        if !parser.is_null() && ffi::ts_parser_set_language(parser, language.raw()) {
            let mut state = ReadState {
                rope: &rope,
                chunk: Vec::with_capacity(4096),
            };
            let input = ffi::TSInput {
                payload: &mut state as *mut ReadState as *mut c_void,
                read: Some(read_rope),
                encoding: ffi::TS_INPUT_ENCODING_UTF8,
                decode: std::ptr::null(),
            };
            let tree = ffi::ts_parser_parse(parser, std::ptr::null(), input);
            if let Some(tree) = NonNull::new(tree) {
                collect(query, tree, text, &mut out);
                ffi::ts_tree_delete(tree.as_ptr());
            }
        }
        if !parser.is_null() {
            ffi::ts_parser_delete(parser);
        }
        ffi::ts_query_delete(query.as_ptr());
    }
    out
}

/// # Safety
/// `query` and `tree` are live; `text` is what `tree` was parsed from.
unsafe fn collect(
    query: NonNull<ffi::TSQuery>,
    tree: NonNull<ffi::TSTree>,
    text: &str,
    out: &mut Vec<Definition>,
) {
    unsafe {
        let count = ffi::ts_query_capture_count(query.as_ptr());
        let kinds: Vec<Option<&'static str>> = (0..count)
            .map(|i| {
                let mut len = 0u32;
                let ptr = ffi::ts_query_capture_name_for_id(query.as_ptr(), i, &mut len);
                if ptr.is_null() {
                    return None;
                }
                let bytes = std::slice::from_raw_parts(ptr as *const u8, len as usize);
                let name = std::str::from_utf8(bytes).ok()?;
                KINDS.iter().copied().find(|k| *k == name)
            })
            .collect();
        let Some(cursor) = NonNull::new(ffi::ts_query_cursor_new()) else {
            return;
        };
        let root = ffi::ts_tree_root_node(tree.as_ptr());
        ffi::ts_query_cursor_exec(cursor.as_ptr(), query.as_ptr(), root);
        let mut m = std::mem::zeroed::<ffi::TSQueryMatch>();
        while ffi::ts_query_cursor_next_match(cursor.as_ptr(), &mut m) {
            if m.captures.is_null() {
                continue;
            }
            for capture in std::slice::from_raw_parts(m.captures, m.capture_count as usize) {
                let Some(Some(kind)) = kinds.get(capture.index as usize).copied() else {
                    continue;
                };
                let (start, end) = (
                    ffi::ts_node_start_byte(capture.node) as usize,
                    ffi::ts_node_end_byte(capture.node) as usize,
                );
                let Some(name) = text.get(start..end) else {
                    continue;
                };
                if name.is_empty() || name.len() > 200 {
                    continue;
                }
                out.push(Definition {
                    name: name.to_owned(),
                    kind,
                    line: ffi::ts_node_start_point(capture.node).row,
                });
            }
        }
        ffi::ts_query_cursor_delete(cursor.as_ptr());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_query_fits_its_grammar() {
        for language in Language::ALL {
            assert!(
                query_compiles(language),
                "{language:?}'s definition query does not compile"
            );
        }
    }

    fn names(language: Language, text: &str) -> Vec<(String, &'static str, u32)> {
        definitions(language, text)
            .into_iter()
            .map(|d| (d.name, d.kind, d.line))
            .collect()
    }

    #[test]
    fn finds_what_each_language_defines() {
        assert_eq!(
            names(
                Language::Rust,
                "struct Tree { rows: u32 }\nfn open() {}\nconst MAX: u8 = 1;\n"
            ),
            [
                ("Tree".into(), "type", 0),
                ("rows".into(), "field", 0),
                ("open".into(), "function", 1),
                ("MAX".into(), "constant", 2)
            ]
        );
        assert_eq!(
            names(
                Language::Python,
                "LIMIT = 3\nclass Bed:\n    def water(self): pass\n"
            ),
            [
                ("LIMIT".into(), "constant", 0),
                ("Bed".into(), "type", 1),
                ("water".into(), "function", 2)
            ]
        );
        assert_eq!(
            names(
                Language::TypeScript,
                "interface Entry { date: string }\nexport function parseLog() {}\n"
            ),
            [
                ("Entry".into(), "type", 0),
                ("date".into(), "field", 0),
                ("parseLog".into(), "function", 1)
            ]
        );
        assert_eq!(
            names(
                Language::Go,
                "package x\ntype Bed struct{ Name string }\nfunc Water() {}\n"
            ),
            [
                ("Bed".into(), "type", 1),
                ("Name".into(), "field", 1),
                ("Water".into(), "function", 2)
            ]
        );
        assert_eq!(
            names(Language::Bash, "build() { :; }\n"),
            [("build".into(), "function", 0)]
        );
        assert!(names(Language::Json, "{\"a\": 1}").is_empty());
    }
}
