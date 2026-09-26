//! MCP, the JSON-RPC conversation on top of the socket.
//!
//! `claude` is the client: it sends `initialize`, lists the tools and calls
//! them. The editor answers from what it already has, through [`Host`], so
//! this file is the protocol and nothing else, and is tested with a fake
//! editor. One tool, `openDiff`, does not answer at once: the user decides,
//! possibly minutes later, and the host replies with [`tool_reply`].
//!
//! Result shapes follow what Claude Code expects from VS Code, as recorded
//! by claudecode.nvim's PROTOCOL.md. Positions are 0-based lines and UTF-16
//! columns, the same as the language servers', so they pass straight
//! through.

use std::path::{Path, PathBuf};

use crate::json::{self, Value};
use crate::lsp::{self, Diagnostic, Position, Severity};

/// The protocol version answered to `initialize`. The one both open-source
/// implementations use; Claude Code accepts it.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// A document as `getOpenEditors` describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Editor {
    pub path: Option<PathBuf>,
    pub label: String,
    pub language_id: String,
    pub active: bool,
    pub dirty: bool,
}

/// The selection, or the caret as an empty one.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    pub path: PathBuf,
    pub text: String,
    pub start: Position,
    pub end: Position,
}

/// A diff `claude` wants reviewed.
#[derive(Debug, Clone, PartialEq)]
pub struct DiffRequest {
    pub old_path: PathBuf,
    pub new_path: PathBuf,
    pub new_contents: String,
    pub tab_name: String,
}

/// What the editor provides. Errors are messages for the client.
pub trait Host {
    /// For one file, or every file with diagnostics.
    fn diagnostics(&self, path: Option<&Path>) -> Vec<(PathBuf, Vec<Diagnostic>)>;
    fn open_editors(&self) -> Vec<Editor>;
    fn workspace_folders(&self) -> Vec<PathBuf>;
    fn selection(&self) -> Option<Selection>;
    /// `(dirty, untitled)` for an open document, `None` when not open.
    fn document_state(&self, path: &Path) -> Option<(bool, bool)>;
    /// Saves an open document. `Ok(false)` when it was not open.
    fn save(&mut self, path: &Path) -> Result<bool, String>;
    /// Opens and focuses a file, selecting from the first `start_text` to
    /// the end of the next `end_text` when given.
    fn open_file(
        &mut self,
        path: &Path,
        start_text: Option<&str>,
        end_text: Option<&str>,
    ) -> Result<(), String>;
    /// Shows a diff for review. The reply is sent later, with `request`.
    fn open_diff(&mut self, request: Value, diff: DiffRequest) -> Result<(), String>;
    /// Closes a diff tab by name. `false` when there is none.
    fn close_tab(&mut self, tab_name: &str) -> bool;
    /// Closes every diff tab, answering pending ones as rejected.
    fn close_all_diffs(&mut self) -> usize;
}

/// Handles one message from the client. The reply to send, if any.
pub fn handle(text: &str, host: &mut impl Host) -> Option<String> {
    let message = match json::parse(text) {
        Ok(message) => message,
        Err(_) => return Some(error(&Value::Null, -32700, "parse error")),
    };
    let method = message.get("method").and_then(Value::as_str)?;
    // A notification: `notifications/initialized`, `ide_connected`. Nothing
    // is owed, and nothing in them needs acting on.
    let id = message.get("id")?.clone();
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let reply = match method {
        "initialize" => Ok(initialize()),
        "ping" => Ok(Value::Object(Vec::new())),
        "tools/list" => Ok(json::object([("tools", Value::Array(tools()))])),
        "prompts/list" => Ok(json::object([("prompts", Value::Array(Vec::new()))])),
        "resources/list" => Ok(json::object([("resources", Value::Array(Vec::new()))])),
        "tools/call" => match call(&id, &params, host) {
            Ok(Some(content)) => Ok(content),
            // Answered later.
            Ok(None) => return None,
            Err(message) => Err((-32602, message)),
        },
        _ => Err((-32601, format!("method not found: {method}"))),
    };
    Some(match reply {
        Ok(result) => response(&id, result),
        Err((code, message)) => error(&id, code, &message),
    })
}

/// The deferred reply to a tool call: each string one text item.
pub fn tool_reply(id: &Value, texts: &[&str]) -> String {
    response(id, content(texts))
}

/// `selection_changed`, sent as the selection moves.
pub fn selection_changed(selection: &Selection) -> String {
    notification(
        "selection_changed",
        json::object([
            ("text", json::string(&selection.text)),
            ("filePath", json::string(&selection.path.to_string_lossy())),
            ("fileUrl", json::string(&lsp::uri_for(&selection.path))),
            (
                "selection",
                json::object([
                    ("start", position(selection.start)),
                    ("end", position(selection.end)),
                    ("isEmpty", Value::Bool(selection.start == selection.end)),
                ]),
            ),
        ]),
    )
}

/// `at_mentioned`: puts a file, or lines of it, into Claude's prompt.
/// Lines are 0-based and inclusive.
pub fn at_mentioned(path: &Path, lines: Option<(u32, u32)>) -> String {
    let mut params = vec![("filePath".to_owned(), json::string(&path.to_string_lossy()))];
    if let Some((start, end)) = lines {
        params.push(("lineStart".to_owned(), json::number(start)));
        params.push(("lineEnd".to_owned(), json::number(end)));
    }
    notification("at_mentioned", Value::Object(params))
}

fn initialize() -> Value {
    let listed = || json::object([("listChanged", Value::Bool(true))]);
    json::object([
        ("protocolVersion", json::string(PROTOCOL_VERSION)),
        (
            "capabilities",
            json::object([
                ("tools", listed()),
                ("prompts", listed()),
                ("logging", Value::Object(Vec::new())),
            ]),
        ),
        (
            "serverInfo",
            json::object([
                ("name", json::string(super::lock::IDE_NAME)),
                ("version", json::string(env!("CARGO_PKG_VERSION"))),
            ]),
        ),
    ])
}

/// Every tool with its input schema. `close_tab` is called by `claude`
/// itself after a diff and is not listed, as in the other editors.
fn tools() -> Vec<Value> {
    let string = |description: &str| {
        json::object([
            ("type", json::string("string")),
            ("description", json::string(description)),
        ])
    };
    let boolean = |description: &str| {
        json::object([
            ("type", json::string("boolean")),
            ("description", json::string(description)),
        ])
    };
    let tool =
        |name: &str, description: &str, properties: Vec<(&str, Value)>, required: &[&str]| {
            json::object([
                ("name", json::string(name)),
                ("description", json::string(description)),
                (
                    "inputSchema",
                    json::object([
                        ("type", json::string("object")),
                        (
                            "properties",
                            Value::Object(
                                properties
                                    .into_iter()
                                    .map(|(k, v)| (k.to_owned(), v))
                                    .collect(),
                            ),
                        ),
                        (
                            "required",
                            Value::Array(required.iter().map(|r| json::string(r)).collect()),
                        ),
                    ]),
                ),
            ])
        };
    vec![
        tool(
            "openDiff",
            "Show a proposed change to a file for the user to accept or reject",
            vec![
                ("old_file_path", string("Path of the file as it is")),
                ("new_file_path", string("Path of the file after the change")),
                ("new_file_contents", string("The whole proposed file")),
                ("tab_name", string("Name of the review tab")),
            ],
            &[
                "old_file_path",
                "new_file_path",
                "new_file_contents",
                "tab_name",
            ],
        ),
        tool(
            "getDiagnostics",
            "Language server diagnostics for one file, or for every file when uri is omitted",
            vec![("uri", string("file:// URI of the file"))],
            &[],
        ),
        tool(
            "openFile",
            "Open a file in the editor, optionally selecting a range",
            vec![
                ("filePath", string("Path of the file")),
                ("preview", boolean("Ignored")),
                ("startText", string("Text where the selection starts")),
                ("endText", string("Text where the selection ends")),
                ("selectToEndOfLine", boolean("Ignored")),
                ("makeFrontmost", boolean("Focus the file (default true)")),
            ],
            &["filePath"],
        ),
        tool(
            "getCurrentSelection",
            "The selection in the active document",
            vec![],
            &[],
        ),
        tool(
            "getLatestSelection",
            "The most recent selection in any document",
            vec![],
            &[],
        ),
        tool("getOpenEditors", "The open documents", vec![], &[]),
        tool("getWorkspaceFolders", "The project folders", vec![], &[]),
        tool(
            "checkDocumentDirty",
            "Whether an open document has unsaved changes",
            vec![("filePath", string("Path of the file"))],
            &["filePath"],
        ),
        tool(
            "saveDocument",
            "Save an open document",
            vec![("filePath", string("Path of the file"))],
            &["filePath"],
        ),
        tool(
            "closeAllDiffTabs",
            "Close every diff review tab",
            vec![],
            &[],
        ),
    ]
}

/// Runs a tool. `Ok(None)` when the reply comes later.
fn call(id: &Value, params: &Value, host: &mut impl Host) -> Result<Option<Value>, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or("tools/call without a name")?;
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let arg = |key: &str| args.get(key).and_then(Value::as_str);
    let path_arg = |key: &str| {
        arg(key)
            .map(PathBuf::from)
            .ok_or_else(|| format!("{name}: missing {key}"))
    };
    let text = |s: &str| Ok(Some(content(&[s])));
    let json_text = |v: Value| Ok(Some(content(&[&json::compact(&v)])));
    match name {
        "openDiff" => {
            let diff = DiffRequest {
                old_path: path_arg("old_file_path")?,
                new_path: path_arg("new_file_path")?,
                new_contents: arg("new_file_contents")
                    .ok_or("openDiff: missing new_file_contents")?
                    .to_owned(),
                tab_name: arg("tab_name")
                    .ok_or("openDiff: missing tab_name")?
                    .to_owned(),
            };
            host.open_diff(id.clone(), diff)?;
            Ok(None)
        }
        "close_tab" => {
            let tab = arg("tab_name").ok_or("close_tab: missing tab_name")?;
            host.close_tab(tab);
            text("TAB_CLOSED")
        }
        "closeAllDiffTabs" => text(&format!("CLOSED_{}_DIFF_TABS", host.close_all_diffs())),
        "getDiagnostics" => {
            let path = arg("uri").and_then(|uri| lsp::path_for(uri).or(Some(PathBuf::from(uri))));
            let files = host
                .diagnostics(path.as_deref())
                .into_iter()
                .map(|(path, list)| {
                    json::object([
                        ("uri", json::string(&lsp::uri_for(&path))),
                        (
                            "diagnostics",
                            Value::Array(list.iter().map(diagnostic).collect()),
                        ),
                    ])
                })
                .collect();
            json_text(Value::Array(files))
        }
        "openFile" => {
            let path = path_arg("filePath")?;
            host.open_file(&path, arg("startText"), arg("endText"))?;
            if args.get("makeFrontmost").and_then(Value::as_bool) == Some(false) {
                json_text(json::object([
                    ("success", Value::Bool(true)),
                    ("filePath", json::string(&path.to_string_lossy())),
                ]))
            } else {
                text(&format!("Opened file: {}", path.display()))
            }
        }
        "getCurrentSelection" | "getLatestSelection" => json_text(match host.selection() {
            Some(selection) => json::object([
                ("success", Value::Bool(true)),
                ("text", json::string(&selection.text)),
                ("filePath", json::string(&selection.path.to_string_lossy())),
                (
                    "selection",
                    json::object([
                        ("start", position(selection.start)),
                        ("end", position(selection.end)),
                        ("isEmpty", Value::Bool(selection.start == selection.end)),
                    ]),
                ),
            ]),
            None => json::object([
                ("success", Value::Bool(false)),
                ("message", json::string("No active editor found")),
            ]),
        }),
        "getOpenEditors" => {
            let tabs = host
                .open_editors()
                .into_iter()
                .map(|editor| {
                    let uri = editor
                        .path
                        .as_deref()
                        .map(lsp::uri_for)
                        .unwrap_or_else(|| format!("untitled:{}", editor.label));
                    json::object([
                        ("uri", json::string(&uri)),
                        ("isActive", Value::Bool(editor.active)),
                        ("label", json::string(&editor.label)),
                        ("languageId", json::string(&editor.language_id)),
                        ("isDirty", Value::Bool(editor.dirty)),
                    ])
                })
                .collect();
            json_text(json::object([("tabs", Value::Array(tabs))]))
        }
        "getWorkspaceFolders" => {
            let folders = host.workspace_folders();
            let listed = folders
                .iter()
                .map(|folder| {
                    let name = folder
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    json::object([
                        ("name", json::string(&name)),
                        ("uri", json::string(&lsp::uri_for(folder))),
                        ("path", json::string(&folder.to_string_lossy())),
                    ])
                })
                .collect();
            let root = folders
                .first()
                .map(|f| json::string(&f.to_string_lossy()))
                .unwrap_or(Value::Null);
            json_text(json::object([
                ("success", Value::Bool(true)),
                ("folders", Value::Array(listed)),
                ("rootPath", root),
            ]))
        }
        "checkDocumentDirty" => {
            let path = path_arg("filePath")?;
            let shown = json::string(&path.to_string_lossy());
            json_text(match host.document_state(&path) {
                Some((dirty, untitled)) => json::object([
                    ("success", Value::Bool(true)),
                    ("filePath", shown),
                    ("isDirty", Value::Bool(dirty)),
                    ("isUntitled", Value::Bool(untitled)),
                ]),
                None => not_open(&path),
            })
        }
        "saveDocument" => {
            let path = path_arg("filePath")?;
            let shown = json::string(&path.to_string_lossy());
            json_text(match host.save(&path) {
                Ok(true) => json::object([
                    ("success", Value::Bool(true)),
                    ("filePath", shown),
                    ("saved", Value::Bool(true)),
                    ("message", json::string("Document saved successfully")),
                ]),
                Ok(false) => not_open(&path),
                Err(message) => json::object([
                    ("success", Value::Bool(false)),
                    ("filePath", shown),
                    ("saved", Value::Bool(false)),
                    ("message", json::string(&message)),
                ]),
            })
        }
        _ => Err(format!("unknown tool: {name}")),
    }
}

fn not_open(path: &Path) -> Value {
    json::object([
        ("success", Value::Bool(false)),
        (
            "message",
            json::string(&format!("Document not open: {}", path.display())),
        ),
    ])
}

fn diagnostic(d: &Diagnostic) -> Value {
    let severity = match d.severity {
        Severity::Error => "Error",
        Severity::Warning => "Warning",
        Severity::Information => "Information",
        Severity::Hint => "Hint",
    };
    let mut members = vec![
        ("message".to_owned(), json::string(&d.message)),
        ("severity".to_owned(), json::string(severity)),
        (
            "range".to_owned(),
            json::object([("start", position(d.start)), ("end", position(d.end))]),
        ),
    ];
    if let Some(source) = &d.source {
        members.push(("source".to_owned(), json::string(source)));
    }
    Value::Object(members)
}

fn position(p: Position) -> Value {
    json::object([
        ("line", json::number(p.line)),
        ("character", json::number(p.character)),
    ])
}

fn content(texts: &[&str]) -> Value {
    json::object([(
        "content",
        Value::Array(
            texts
                .iter()
                .map(|t| json::object([("type", json::string("text")), ("text", json::string(t))]))
                .collect(),
        ),
    )])
}

fn response(id: &Value, result: Value) -> String {
    json::compact(&json::object([
        ("jsonrpc", json::string("2.0")),
        ("id", id.clone()),
        ("result", result),
    ]))
}

fn error(id: &Value, code: i64, message: &str) -> String {
    json::compact(&json::object([
        ("jsonrpc", json::string("2.0")),
        ("id", id.clone()),
        (
            "error",
            json::object([
                ("code", json::number(code)),
                ("message", json::string(message)),
            ]),
        ),
    ]))
}

fn notification(method: &str, params: Value) -> String {
    json::compact(&json::object([
        ("jsonrpc", json::string("2.0")),
        ("method", json::string(method)),
        ("params", params),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        diffs: Vec<(Value, DiffRequest)>,
        closed: Vec<String>,
        saved: Vec<PathBuf>,
        opened: Vec<(PathBuf, Option<String>, Option<String>)>,
    }

    impl Host for Fake {
        fn diagnostics(&self, path: Option<&Path>) -> Vec<(PathBuf, Vec<Diagnostic>)> {
            let all = vec![(
                PathBuf::from("/p/src/a b.rs"),
                vec![Diagnostic {
                    start: Position {
                        line: 2,
                        character: 4,
                    },
                    end: Position {
                        line: 2,
                        character: 9,
                    },
                    severity: Severity::Error,
                    message: "mismatched types".into(),
                    source: Some("rustc".into()),
                    raw: String::new(),
                }],
            )];
            all.into_iter()
                .filter(|(p, _)| path.is_none_or(|want| want == p))
                .collect()
        }
        fn open_editors(&self) -> Vec<Editor> {
            vec![
                Editor {
                    path: Some("/p/src/main.rs".into()),
                    label: "main.rs".into(),
                    language_id: "rust".into(),
                    active: true,
                    dirty: true,
                },
                Editor {
                    path: None,
                    label: "Untitled".into(),
                    language_id: "plaintext".into(),
                    active: false,
                    dirty: false,
                },
            ]
        }
        fn workspace_folders(&self) -> Vec<PathBuf> {
            vec!["/p".into()]
        }
        fn selection(&self) -> Option<Selection> {
            Some(Selection {
                path: "/p/src/main.rs".into(),
                text: "fn main".into(),
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 7,
                },
            })
        }
        fn document_state(&self, path: &Path) -> Option<(bool, bool)> {
            (path == Path::new("/p/src/main.rs")).then_some((true, false))
        }
        fn save(&mut self, path: &Path) -> Result<bool, String> {
            if path != Path::new("/p/src/main.rs") {
                return Ok(false);
            }
            self.saved.push(path.to_path_buf());
            Ok(true)
        }
        fn open_file(
            &mut self,
            path: &Path,
            start: Option<&str>,
            end: Option<&str>,
        ) -> Result<(), String> {
            self.opened.push((
                path.to_path_buf(),
                start.map(Into::into),
                end.map(Into::into),
            ));
            Ok(())
        }
        fn open_diff(&mut self, request: Value, diff: DiffRequest) -> Result<(), String> {
            self.diffs.push((request, diff));
            Ok(())
        }
        fn close_tab(&mut self, tab_name: &str) -> bool {
            self.closed.push(tab_name.into());
            true
        }
        fn close_all_diffs(&mut self) -> usize {
            3
        }
    }

    fn reply(host: &mut Fake, text: &str) -> Value {
        json::parse(&handle(text, host).expect("a reply")).unwrap()
    }

    fn call_tool(host: &mut Fake, name: &str, args: &str) -> Value {
        reply(
            host,
            &format!(
                r#"{{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#
            ),
        )
    }

    /// The text of the first content item.
    fn first_text(value: &Value) -> String {
        value
            .path("result.content")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("text"))
            .and_then(Value::as_str)
            .expect("text content")
            .to_owned()
    }

    #[test]
    fn initialize_lists_capabilities_as_objects() {
        let mut host = Fake::default();
        let text = handle(
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
            &mut host,
        )
        .unwrap();
        // An empty object must stay an object: `[]` breaks the client.
        assert!(text.contains(r#""logging":{}"#), "{text}");
        let value = json::parse(&text).unwrap();
        assert_eq!(value.path("id").and_then(Value::as_u64), Some(0));
        assert_eq!(
            value.path("result.protocolVersion").and_then(Value::as_str),
            Some("2024-11-05")
        );
        assert_eq!(
            value.path("result.serverInfo.name").and_then(Value::as_str),
            Some("crc")
        );
        assert_eq!(
            value
                .path("result.capabilities.tools.listChanged")
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn notifications_get_no_reply_and_unknown_methods_an_error() {
        let mut host = Fake::default();
        assert_eq!(
            handle(
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                &mut host
            ),
            None
        );
        assert_eq!(
            handle(
                r#"{"jsonrpc":"2.0","method":"ide_connected","params":{"pid":1}}"#,
                &mut host
            ),
            None
        );
        let value = reply(&mut host, r#"{"jsonrpc":"2.0","id":"x","method":"nope"}"#);
        assert_eq!(
            value.path("error.code").and_then(Value::as_i64),
            Some(-32601)
        );
        assert_eq!(value.path("id").and_then(Value::as_str), Some("x"));
        let value = reply(&mut host, "{not json");
        assert_eq!(
            value.path("error.code").and_then(Value::as_i64),
            Some(-32700)
        );
    }

    #[test]
    fn tools_are_listed_with_schemas_and_close_tab_is_not() {
        let mut host = Fake::default();
        let value = reply(
            &mut host,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        );
        let tools = value
            .path("result.tools")
            .and_then(Value::as_array)
            .unwrap();
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"openDiff") && names.contains(&"getDiagnostics"));
        assert!(!names.contains(&"close_tab"));
        for tool in tools {
            assert_eq!(
                tool.path("inputSchema.type").and_then(Value::as_str),
                Some("object")
            );
        }
        let text = handle(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            &mut host,
        )
        .unwrap();
        // A tool without arguments still has an object of properties.
        assert!(text.contains(r#""properties":{}"#), "{text}");
    }

    #[test]
    fn open_diff_is_answered_later() {
        let mut host = Fake::default();
        let request = r#"{"jsonrpc":"2.0","id":42,"method":"tools/call","params":{"name":"openDiff","arguments":{"old_file_path":"/p/a.rs","new_file_path":"/p/a.rs","new_file_contents":"fn a() {}\n","tab_name":"✻ [Claude Code] a.rs"}}}"#;
        assert_eq!(handle(request, &mut host), None);
        let (id, diff) = &host.diffs[0];
        assert_eq!(diff.new_contents, "fn a() {}\n");
        assert_eq!(diff.tab_name, "✻ [Claude Code] a.rs");

        let saved = json::parse(&tool_reply(id, &["FILE_SAVED", "fn a() {}\n"])).unwrap();
        assert_eq!(saved.path("id").and_then(Value::as_u64), Some(42));
        let items = saved
            .path("result.content")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(
            items[0].get("text").and_then(Value::as_str),
            Some("FILE_SAVED")
        );
        assert_eq!(
            items[1].get("text").and_then(Value::as_str),
            Some("fn a() {}\n")
        );

        let value = call_tool(&mut host, "openDiff", r#"{"tab_name":"x"}"#);
        assert_eq!(
            value.path("error.code").and_then(Value::as_i64),
            Some(-32602)
        );
    }

    #[test]
    fn close_tab_and_close_all() {
        let mut host = Fake::default();
        let value = call_tool(&mut host, "close_tab", r#"{"tab_name":"t"}"#);
        assert_eq!(first_text(&value), "TAB_CLOSED");
        assert_eq!(host.closed, ["t"]);
        let value = call_tool(&mut host, "closeAllDiffTabs", "{}");
        assert_eq!(first_text(&value), "CLOSED_3_DIFF_TABS");
    }

    #[test]
    fn diagnostics_are_uris_severity_names_and_zero_based_ranges() {
        let mut host = Fake::default();
        let value = call_tool(&mut host, "getDiagnostics", "{}");
        let files = json::parse(&first_text(&value)).unwrap();
        let file = &files.as_array().unwrap()[0];
        assert_eq!(
            file.get("uri").and_then(Value::as_str),
            Some("file:///p/src/a%20b.rs")
        );
        let d = &file.get("diagnostics").and_then(Value::as_array).unwrap()[0];
        assert_eq!(d.get("severity").and_then(Value::as_str), Some("Error"));
        assert_eq!(d.get("source").and_then(Value::as_str), Some("rustc"));
        assert_eq!(d.path("range.start.line").and_then(Value::as_u64), Some(2));
        assert_eq!(
            d.path("range.end.character").and_then(Value::as_u64),
            Some(9)
        );

        let value = call_tool(
            &mut host,
            "getDiagnostics",
            r#"{"uri":"file:///p/src/a%20b.rs"}"#,
        );
        assert_eq!(
            json::parse(&first_text(&value))
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let value = call_tool(
            &mut host,
            "getDiagnostics",
            r#"{"uri":"file:///elsewhere.rs"}"#,
        );
        assert_eq!(first_text(&value), "[]");
    }

    #[test]
    fn editors_selection_folders_dirty_and_save() {
        let mut host = Fake::default();
        let value = call_tool(&mut host, "getOpenEditors", "{}");
        let tabs = json::parse(&first_text(&value)).unwrap();
        let tabs = tabs.get("tabs").and_then(Value::as_array).unwrap();
        assert_eq!(
            tabs[0].get("uri").and_then(Value::as_str),
            Some("file:///p/src/main.rs")
        );
        assert_eq!(tabs[0].get("isDirty").and_then(Value::as_bool), Some(true));
        assert_eq!(
            tabs[1].get("uri").and_then(Value::as_str),
            Some("untitled:Untitled")
        );

        let value = call_tool(&mut host, "getCurrentSelection", "{}");
        let selection = json::parse(&first_text(&value)).unwrap();
        assert_eq!(
            selection.get("text").and_then(Value::as_str),
            Some("fn main")
        );
        assert_eq!(
            selection
                .path("selection.end.character")
                .and_then(Value::as_u64),
            Some(7)
        );
        assert_eq!(
            selection.path("selection.isEmpty").and_then(Value::as_bool),
            Some(false)
        );

        let value = call_tool(&mut host, "getWorkspaceFolders", "{}");
        let folders = json::parse(&first_text(&value)).unwrap();
        assert_eq!(folders.get("rootPath").and_then(Value::as_str), Some("/p"));
        assert_eq!(
            folders.get("folders").and_then(Value::as_array).unwrap()[0]
                .get("name")
                .and_then(Value::as_str),
            Some("p")
        );

        let value = call_tool(
            &mut host,
            "checkDocumentDirty",
            r#"{"filePath":"/p/src/main.rs"}"#,
        );
        let state = json::parse(&first_text(&value)).unwrap();
        assert_eq!(state.get("isDirty").and_then(Value::as_bool), Some(true));
        let value = call_tool(&mut host, "checkDocumentDirty", r#"{"filePath":"/p/x.rs"}"#);
        let state = json::parse(&first_text(&value)).unwrap();
        assert_eq!(state.get("success").and_then(Value::as_bool), Some(false));

        let value = call_tool(
            &mut host,
            "saveDocument",
            r#"{"filePath":"/p/src/main.rs"}"#,
        );
        let saved = json::parse(&first_text(&value)).unwrap();
        assert_eq!(saved.get("saved").and_then(Value::as_bool), Some(true));
        assert_eq!(host.saved, [PathBuf::from("/p/src/main.rs")]);
    }

    #[test]
    fn open_file_passes_the_range_text_through() {
        let mut host = Fake::default();
        let value = call_tool(
            &mut host,
            "openFile",
            r#"{"filePath":"/p/a.rs","startText":"fn a","endText":"}"}"#,
        );
        assert_eq!(first_text(&value), "Opened file: /p/a.rs");
        assert_eq!(
            host.opened,
            [("/p/a.rs".into(), Some("fn a".into()), Some("}".into()))]
        );
    }

    #[test]
    fn notifications_carry_zero_based_positions() {
        let text = selection_changed(&Selection {
            path: "/p/a.rs".into(),
            text: String::new(),
            start: Position {
                line: 3,
                character: 2,
            },
            end: Position {
                line: 3,
                character: 2,
            },
        });
        let value = json::parse(&text).unwrap();
        assert_eq!(
            value.get("method").and_then(Value::as_str),
            Some("selection_changed")
        );
        assert_eq!(
            value
                .path("params.selection.start.line")
                .and_then(Value::as_u64),
            Some(3)
        );
        assert_eq!(
            value
                .path("params.selection.isEmpty")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            value.path("params.fileUrl").and_then(Value::as_str),
            Some("file:///p/a.rs")
        );
        assert!(!text.contains("\"id\""));

        let value = json::parse(&at_mentioned(Path::new("/p/a.rs"), Some((4, 9)))).unwrap();
        assert_eq!(
            value.path("params.lineStart").and_then(Value::as_u64),
            Some(4)
        );
        assert_eq!(
            value.path("params.lineEnd").and_then(Value::as_u64),
            Some(9)
        );
        let value = json::parse(&at_mentioned(Path::new("/p/a.rs"), None)).unwrap();
        assert!(value.path("params.lineStart").is_none());
    }
}
