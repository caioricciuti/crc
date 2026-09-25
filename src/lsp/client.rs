//! One language server: its lifecycle, the documents it knows about, and
//! the requests in flight.
//!
//! The server is driven from the main thread. Replies and notifications
//! arrive through the transport's channel and are turned into [`Event`]s by
//! [`Server::poll`], which the window calls whenever the transport wakes
//! it. Nothing here blocks.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::transport::{Incoming, Transport, Wake};
use super::{
    Completion, Diagnostic, Location, Position, Severity, Signature, TextEdit, offset_of, path_for,
    uri_for,
};
use crate::json::{Value, number, object, string};

/// Where a server is in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// `initialize` sent, no answer yet.
    Starting,
    Ready,
    /// Gone, with the reason the status line shows.
    Failed(String),
}

/// What a reply is for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pending {
    Initialize,
    Completion { path: PathBuf, at: Position },
    Definition,
    Hover { path: PathBuf },
    References,
    Rename,
    Formatting { path: PathBuf, save: bool },
    Signature { path: PathBuf },
    Shutdown,
}

/// Something the server told us, ready for the window to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Ready,
    /// Diagnostics for a file were replaced; read them from
    /// [`Server::diagnostics`].
    Diagnostics(PathBuf),
    Completions {
        path: PathBuf,
        at: Position,
        items: Vec<Completion>,
        request: u64,
    },
    Definition(Vec<Location>),
    Hover {
        path: PathBuf,
        text: String,
    },
    References(Vec<Location>),
    /// Edits per file: a rename's answer.
    Rename(Vec<(PathBuf, Vec<TextEdit>)>),
    /// Edits for one file. `save` when the format was asked for by a save.
    Formatting {
        path: PathBuf,
        edits: Vec<TextEdit>,
        save: bool,
    },
    /// `None` when the caret is not inside a call any more.
    Signature {
        path: PathBuf,
        signature: Option<Signature>,
    },
    /// A request the person made was refused, with the server's reason.
    Refused(String),
    Failed(String),
}

pub struct Server {
    pub name: String,
    transport: Transport,
    pub phase: Phase,
    root: PathBuf,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    /// Open documents and the version last sent for each.
    versions: HashMap<PathBuf, u64>,
    pub diagnostics: HashMap<PathBuf, Vec<Diagnostic>>,
    /// What the server said it can do.
    pub capabilities: Value,
    /// Characters after which the server wants to be asked for completions.
    pub trigger_characters: Vec<String>,
    pub started_at: Instant,
}

impl Server {
    /// Starts `program` for `root` and sends `initialize`.
    pub fn start(
        name: &str,
        program: &Path,
        args: &[&str],
        root: &Path,
        wake: Wake,
    ) -> std::io::Result<Server> {
        let transport = Transport::spawn(program, args, root, wake)?;
        let mut server = Server {
            name: name.to_owned(),
            transport,
            phase: Phase::Starting,
            root: root.to_path_buf(),
            next_id: 1,
            pending: HashMap::new(),
            versions: HashMap::new(),
            diagnostics: HashMap::new(),
            capabilities: Value::Null,
            trigger_characters: Vec::new(),
            started_at: Instant::now(),
        };
        let params = object([
            ("processId", number(std::process::id())),
            ("rootUri", string(&uri_for(root))),
            (
                "workspaceFolders",
                Value::Array(vec![object([
                    ("uri", string(&uri_for(root))),
                    (
                        "name",
                        string(
                            &root
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                        ),
                    ),
                ])]),
            ),
            (
                "clientInfo",
                object([
                    ("name", string("crc")),
                    ("version", string(env!("CARGO_PKG_VERSION"))),
                ]),
            ),
            (
                "capabilities",
                object([
                    (
                        "textDocument",
                        object([
                            ("synchronization", object([("didSave", Value::Bool(true))])),
                            (
                                "completion",
                                object([(
                                    "completionItem",
                                    object([
                                        ("snippetSupport", Value::Bool(false)),
                                        (
                                            "documentationFormat",
                                            Value::Array(vec![string("plaintext")]),
                                        ),
                                    ]),
                                )]),
                            ),
                            (
                                "hover",
                                object([(
                                    "contentFormat",
                                    Value::Array(vec![string("plaintext"), string("markdown")]),
                                )]),
                            ),
                            (
                                "publishDiagnostics",
                                object([("relatedInformation", Value::Bool(false))]),
                            ),
                            ("definition", object([])),
                        ]),
                    ),
                    (
                        "workspace",
                        object([
                            ("configuration", Value::Bool(true)),
                            ("workspaceFolders", Value::Bool(true)),
                        ]),
                    ),
                    ("window", object([("workDoneProgress", Value::Bool(true))])),
                ]),
            ),
        ]);
        server.request("initialize", params, Pending::Initialize);
        Ok(server)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn is_ready(&self) -> bool {
        self.phase == Phase::Ready
    }

    pub fn knows(&self, path: &Path) -> bool {
        self.versions.contains_key(path)
    }

    fn request(&mut self, method: &str, params: Value, pending: Pending) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let message = object([
            ("jsonrpc", string("2.0")),
            ("id", number(id)),
            ("method", string(method)),
            ("params", params),
        ]);
        self.pending.insert(id, pending);
        self.write(&message);
        id
    }

    fn notify(&mut self, method: &str, params: Value) {
        let message = object([
            ("jsonrpc", string("2.0")),
            ("method", string(method)),
            ("params", params),
        ]);
        self.write(&message);
    }

    fn respond(&mut self, id: &Value, result: Value) {
        let message = object([
            ("jsonrpc", string("2.0")),
            ("id", id.clone()),
            ("result", result),
        ]);
        self.write(&message);
    }

    fn respond_error(&mut self, id: &Value, code: i64, message: &str) {
        let message = object([
            ("jsonrpc", string("2.0")),
            ("id", id.clone()),
            (
                "error",
                object([("code", number(code)), ("message", string(message))]),
            ),
        ]);
        self.write(&message);
    }

    fn write(&mut self, message: &Value) {
        if let Err(error) = self.transport.send(message)
            && !matches!(self.phase, Phase::Failed(_))
        {
            self.phase = Phase::Failed(format!("{}: {error}", self.name));
        }
    }

    fn text_document(path: &Path) -> Value {
        object([("uri", string(&uri_for(path)))])
    }

    fn position(at: Position) -> Value {
        object([
            ("line", number(at.line)),
            ("character", number(at.character)),
        ])
    }

    /// Tells the server about a document. Sent once per path; later
    /// changes go through [`Server::did_change`].
    pub fn did_open(&mut self, path: &Path, language_id: &str, text: &str) {
        if self.versions.contains_key(path) {
            return;
        }
        self.versions.insert(path.to_path_buf(), 1);
        let params = object([(
            "textDocument",
            object([
                ("uri", string(&uri_for(path))),
                ("languageId", string(language_id)),
                ("version", number(1)),
                ("text", string(text)),
            ]),
        )]);
        self.notify("textDocument/didOpen", params);
    }

    /// The whole text again. Full sync is what the server was told to
    /// expect; a file of a few megabytes is still a few milliseconds.
    pub fn did_change(&mut self, path: &Path, text: &str) {
        let Some(version) = self.versions.get_mut(path) else {
            return;
        };
        *version += 1;
        let version = *version;
        let params = object([
            (
                "textDocument",
                object([
                    ("uri", string(&uri_for(path))),
                    ("version", number(version)),
                ]),
            ),
            (
                "contentChanges",
                Value::Array(vec![object([("text", string(text))])]),
            ),
        ]);
        self.notify("textDocument/didChange", params);
    }

    pub fn did_save(&mut self, path: &Path) {
        if !self.versions.contains_key(path) {
            return;
        }
        let params = object([("textDocument", Self::text_document(path))]);
        self.notify("textDocument/didSave", params);
    }

    pub fn did_close(&mut self, path: &Path) {
        if self.versions.remove(path).is_none() {
            return;
        }
        self.diagnostics.remove(path);
        let params = object([("textDocument", Self::text_document(path))]);
        self.notify("textDocument/didClose", params);
    }

    /// Asks for completions at `at`. The reply comes back as
    /// [`Event::Completions`] with the same request id.
    pub fn completion(&mut self, path: &Path, at: Position) -> u64 {
        let params = object([
            ("textDocument", Self::text_document(path)),
            ("position", Self::position(at)),
        ]);
        self.request(
            "textDocument/completion",
            params,
            Pending::Completion {
                path: path.to_path_buf(),
                at,
            },
        )
    }

    pub fn definition(&mut self, path: &Path, at: Position) -> u64 {
        let params = object([
            ("textDocument", Self::text_document(path)),
            ("position", Self::position(at)),
        ]);
        self.request("textDocument/definition", params, Pending::Definition)
    }

    pub fn hover(&mut self, path: &Path, at: Position) -> u64 {
        let params = object([
            ("textDocument", Self::text_document(path)),
            ("position", Self::position(at)),
        ]);
        self.request(
            "textDocument/hover",
            params,
            Pending::Hover {
                path: path.to_path_buf(),
            },
        )
    }

    pub fn references(&mut self, path: &Path, at: Position) -> u64 {
        let params = object([
            ("textDocument", Self::text_document(path)),
            ("position", Self::position(at)),
            (
                "context",
                object([("includeDeclaration", Value::Bool(true))]),
            ),
        ]);
        self.request("textDocument/references", params, Pending::References)
    }

    pub fn rename(&mut self, path: &Path, at: Position, new_name: &str) -> u64 {
        let params = object([
            ("textDocument", Self::text_document(path)),
            ("position", Self::position(at)),
            ("newName", string(new_name)),
        ]);
        self.request("textDocument/rename", params, Pending::Rename)
    }

    /// Whether the server said it formats documents.
    pub fn formats(&self) -> bool {
        provides(&self.capabilities, "documentFormattingProvider")
    }

    pub fn formatting(&mut self, path: &Path, tab_size: u32, spaces: bool, save: bool) -> u64 {
        let params = object([
            ("textDocument", Self::text_document(path)),
            (
                "options",
                object([
                    ("tabSize", number(tab_size as u64)),
                    ("insertSpaces", Value::Bool(spaces)),
                ]),
            ),
        ]);
        self.request(
            "textDocument/formatting",
            params,
            Pending::Formatting {
                path: path.to_path_buf(),
                save,
            },
        )
    }

    /// Characters after which the server offers signature help.
    pub fn signature_triggers(&self) -> Vec<String> {
        let mut out: Vec<String> = ["triggerCharacters", "retriggerCharacters"]
            .iter()
            .filter_map(|key| {
                self.capabilities
                    .path(&format!("signatureHelpProvider.{key}"))
                    .and_then(Value::as_array)
            })
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        out.dedup();
        out
    }

    pub fn signature_help(&mut self, path: &Path, at: Position) -> u64 {
        let params = object([
            ("textDocument", Self::text_document(path)),
            ("position", Self::position(at)),
        ]);
        self.request(
            "textDocument/signatureHelp",
            params,
            Pending::Signature {
                path: path.to_path_buf(),
            },
        )
    }

    /// Asks the server to stop. The process is killed when the server is
    /// dropped whether or not it answers.
    pub fn shutdown(&mut self) {
        if self.phase == Phase::Ready {
            self.request("shutdown", Value::Null, Pending::Shutdown);
        }
    }

    /// Everything that arrived since the last call, as events.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        while let Some(incoming) = self.transport.try_recv() {
            self.take(incoming, &mut events);
        }
        if self.phase == Phase::Starting
            && self.started_at.elapsed() > std::time::Duration::from_secs(30)
        {
            let reason = format!("{} did not answer initialize", self.name);
            self.phase = Phase::Failed(reason.clone());
            events.push(Event::Failed(reason));
        }
        events
    }

    /// One thing from the transport. A closed stream is the server gone,
    /// which fails the client once, however the close was read.
    fn take(&mut self, incoming: Incoming, events: &mut Vec<Event>) {
        match incoming {
            Incoming::Message(message) => self.handle(message, events),
            Incoming::Closed => {
                if !matches!(self.phase, Phase::Failed(_)) {
                    let tail = self.transport.stderr_tail();
                    let reason = if tail.is_empty() {
                        format!("{} exited", self.name)
                    } else {
                        format!(
                            "{} exited: {}",
                            self.name,
                            tail.lines().last().unwrap_or("")
                        )
                    };
                    self.phase = Phase::Failed(reason.clone());
                    events.push(Event::Failed(reason));
                }
            }
        }
    }

    /// Blocks until `predicate` accepts an event or `timeout` passes. Tests
    /// only; the app never waits on a server.
    pub fn wait_for(
        &mut self,
        timeout: std::time::Duration,
        mut predicate: impl FnMut(&Event) -> bool,
    ) -> Option<Event> {
        let deadline = Instant::now() + timeout;
        loop {
            for event in self.poll() {
                if predicate(&event) {
                    return Some(event);
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            // A close read here used to return at once without failing the
            // client, so the event a caller waited for never came, and a
            // test fell back on racing the child's exit.
            if let Some(incoming) = self
                .transport
                .recv_timeout(left.min(std::time::Duration::from_millis(50)))
            {
                let closed = matches!(incoming, Incoming::Closed);
                let mut events = Vec::new();
                self.take(incoming, &mut events);
                for event in events {
                    if predicate(&event) {
                        return Some(event);
                    }
                }
                if closed {
                    return None;
                }
            }
        }
    }

    fn handle(&mut self, message: Value, events: &mut Vec<Event>) {
        let id = message.get("id").cloned();
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match (id, method) {
            // A request from the server.
            (Some(id), Some(method)) => self.handle_request(&id, &method, message.get("params")),
            // A notification.
            (None, Some(method)) => {
                self.handle_notification(&method, message.get("params"), events)
            }
            // A reply to one of ours.
            (Some(id), None) => {
                let Some(pending) = id.as_u64().and_then(|id| self.pending.remove(&id)) else {
                    return;
                };
                if let Some(error) = message.get("error") {
                    let text = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("request failed")
                        .to_owned();
                    match pending {
                        Pending::Initialize => {
                            let reason = format!("{}: {text}", self.name);
                            self.phase = Phase::Failed(reason.clone());
                            events.push(Event::Failed(reason));
                        }
                        Pending::References | Pending::Rename | Pending::Formatting { .. } => {
                            events.push(Event::Refused(text));
                        }
                        _ => {}
                    }
                    return;
                }
                let result = message.get("result").cloned().unwrap_or(Value::Null);
                self.handle_reply(pending, id.as_u64().unwrap_or(0), result, events);
            }
            (None, None) => {}
        }
    }

    fn handle_reply(&mut self, pending: Pending, id: u64, result: Value, events: &mut Vec<Event>) {
        match pending {
            Pending::Initialize => {
                self.capabilities = result.get("capabilities").cloned().unwrap_or(Value::Null);
                self.trigger_characters = self
                    .capabilities
                    .path("completionProvider.triggerCharacters")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                self.notify("initialized", object([]));
                self.phase = Phase::Ready;
                events.push(Event::Ready);
            }
            Pending::Completion { path, at } => {
                let items = result
                    .get("items")
                    .and_then(Value::as_array)
                    .or_else(|| result.as_array())
                    .map(|items| items.iter().filter_map(parse_completion).collect())
                    .unwrap_or_default();
                events.push(Event::Completions {
                    path,
                    at,
                    items,
                    request: id,
                });
            }
            Pending::Definition => {
                let locations = match &result {
                    Value::Array(items) => items.iter().filter_map(parse_location).collect(),
                    Value::Object(_) => parse_location(&result).into_iter().collect(),
                    _ => Vec::new(),
                };
                events.push(Event::Definition(locations));
            }
            Pending::Hover { path } => {
                let text = hover_text(result.get("contents"));
                if !text.is_empty() {
                    events.push(Event::Hover { path, text });
                }
            }
            Pending::References => {
                let locations = result
                    .as_array()
                    .map(|items| items.iter().filter_map(parse_location).collect())
                    .unwrap_or_default();
                events.push(Event::References(locations));
            }
            Pending::Rename => events.push(Event::Rename(parse_workspace_edit(&result))),
            Pending::Formatting { path, save } => {
                let edits = result
                    .as_array()
                    .map(|items| items.iter().filter_map(parse_text_edit).collect())
                    .unwrap_or_default();
                events.push(Event::Formatting { path, edits, save });
            }
            Pending::Signature { path } => events.push(Event::Signature {
                path,
                signature: parse_signature(&result),
            }),
            Pending::Shutdown => {
                self.notify("exit", Value::Null);
            }
        }
    }

    fn handle_notification(
        &mut self,
        method: &str,
        params: Option<&Value>,
        events: &mut Vec<Event>,
    ) {
        if method == "textDocument/publishDiagnostics"
            && let Some(params) = params
            && let Some(path) = params.get("uri").and_then(Value::as_str).and_then(path_for)
        {
            let list = params
                .get("diagnostics")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(parse_diagnostic).collect())
                .unwrap_or_default();
            self.diagnostics.insert(path.clone(), list);
            events.push(Event::Diagnostics(path));
        }
        // window/logMessage, $/progress and the rest are noise here.
    }

    fn handle_request(&mut self, id: &Value, method: &str, params: Option<&Value>) {
        match method {
            "workspace/configuration" => {
                // Nothing configured: one null per item asked for.
                let count = params
                    .and_then(|p| p.get("items"))
                    .and_then(Value::as_array)
                    .map_or(0, <[Value]>::len);
                self.respond(id, Value::Array(vec![Value::Null; count]));
            }
            "client/registerCapability"
            | "client/unregisterCapability"
            | "window/workDoneProgress/create"
            | "workspace/semanticTokens/refresh"
            | "workspace/inlayHint/refresh"
            | "workspace/codeLens/refresh"
            | "workspace/diagnostic/refresh" => self.respond(id, Value::Null),
            "workspace/workspaceFolders" => {
                let folder = object([
                    ("uri", string(&uri_for(&self.root))),
                    (
                        "name",
                        string(
                            &self
                                .root
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                        ),
                    ),
                ]);
                self.respond(id, Value::Array(vec![folder]));
            }
            "window/showMessageRequest" => self.respond(id, Value::Null),
            _ => self.respond_error(id, -32601, "method not supported by crc"),
        }
    }
}

fn parse_position(value: &Value) -> Option<Position> {
    Some(Position {
        line: value.get("line")?.as_u64()? as u32,
        character: value.get("character")?.as_u64()? as u32,
    })
}

fn parse_diagnostic(value: &Value) -> Option<Diagnostic> {
    let range = value.get("range")?;
    Some(Diagnostic {
        start: parse_position(range.get("start")?)?,
        end: parse_position(range.get("end")?)?,
        severity: Severity::from_wire(value.get("severity").and_then(Value::as_u64)),
        message: value.get("message")?.as_str()?.to_owned(),
        source: value
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn parse_completion(value: &Value) -> Option<Completion> {
    let label = value.get("label")?.as_str()?.to_owned();
    let edit = value.get("textEdit").and_then(|edit| {
        // A plain edit has `range`; an insert/replace edit has both, and
        // `replace` is what a typed prefix wants.
        let range = edit.get("range").or_else(|| edit.get("replace"))?;
        Some((
            parse_position(range.get("start")?)?,
            parse_position(range.get("end")?)?,
            edit.get("newText")?.as_str()?.to_owned(),
        ))
    });
    let insert_text = value
        .get("insertText")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| label.clone());
    Some(Completion {
        sort_text: value
            .get("sortText")
            .and_then(Value::as_str)
            .unwrap_or(&label)
            .to_owned(),
        filter_text: value
            .get("filterText")
            .and_then(Value::as_str)
            .unwrap_or(&label)
            .to_owned(),
        detail: value
            .get("detail")
            .and_then(Value::as_str)
            .map(str::to_owned),
        kind: value.get("kind").and_then(Value::as_u64).unwrap_or(0),
        edit,
        insert_text,
        label,
    })
}

fn parse_location(value: &Value) -> Option<Location> {
    // Location has uri + range; LocationLink has targetUri + targetRange.
    let uri = value
        .get("uri")
        .or_else(|| value.get("targetUri"))?
        .as_str()?;
    let range = value
        .get("targetSelectionRange")
        .or_else(|| value.get("range"))
        .or_else(|| value.get("targetRange"))?;
    Some(Location {
        path: path_for(uri)?,
        start: parse_position(range.get("start")?)?,
        end: parse_position(range.get("end")?)?,
    })
}

/// Whether a capability is on: `true` or an options object.
fn provides(capabilities: &Value, name: &str) -> bool {
    match capabilities.get(name) {
        Some(Value::Bool(on)) => *on,
        Some(Value::Object(_)) => true,
        _ => false,
    }
}

fn parse_text_edit(value: &Value) -> Option<TextEdit> {
    let range = value.get("range")?;
    Some(TextEdit {
        start: parse_position(range.get("start")?)?,
        end: parse_position(range.get("end")?)?,
        text: value.get("newText")?.as_str()?.to_owned(),
    })
}

/// A WorkspaceEdit's text edits by file, from `documentChanges` when the
/// server sent them, else `changes`. File creations, renames and deletions
/// are not applied, and are left out.
fn parse_workspace_edit(value: &Value) -> Vec<(PathBuf, Vec<TextEdit>)> {
    let edits = |items: Option<&Value>| -> Vec<TextEdit> {
        items
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(parse_text_edit).collect())
            .unwrap_or_default()
    };
    if let Some(changes) = value.get("documentChanges").and_then(Value::as_array) {
        return changes
            .iter()
            .filter_map(|change| {
                let uri = change.path("textDocument.uri")?.as_str()?;
                Some((path_for(uri)?, edits(change.get("edits"))))
            })
            .collect();
    }
    match value.get("changes") {
        Some(Value::Object(files)) => files
            .iter()
            .filter_map(|(uri, list)| Some((path_for(uri)?, edits(Some(list)))))
            .collect(),
        _ => Vec::new(),
    }
}

/// The active signature and parameter, from a SignatureHelp.
fn parse_signature(value: &Value) -> Option<Signature> {
    let signatures = value.get("signatures")?.as_array()?;
    let index = value
        .get("activeSignature")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let signature = signatures.get(index).or_else(|| signatures.first())?;
    let label = signature.get("label")?.as_str()?.to_owned();
    let parameter = signature
        .get("activeParameter")
        .or_else(|| value.get("activeParameter"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let active = signature
        .get("parameters")
        .and_then(Value::as_array)
        .and_then(|parameters| parameters.get(parameter))
        .and_then(|p| p.get("label"))
        .and_then(|l| match l {
            // A substring of the label, or [start, end] in UTF-16 units.
            Value::String(text) => label.find(text.as_str()).map(|at| at..at + text.len()),
            Value::Array(bounds) => {
                let unit = |i: usize| bounds.get(i)?.as_u64().map(|n| n as usize);
                let (start, end) = (unit(0)?, unit(1)?);
                let byte = |units: usize| {
                    let mut seen = 0;
                    for (at, c) in label.char_indices() {
                        if seen >= units {
                            return at;
                        }
                        seen += c.len_utf16();
                    }
                    label.len()
                };
                Some(byte(start)..byte(end))
            }
            _ => None,
        });
    Some(Signature { label, active })
}

/// Hover contents come in four shapes; all of them become plain lines.
fn hover_text(contents: Option<&Value>) -> String {
    fn one(value: &Value) -> String {
        match value {
            Value::String(s) => s.clone(),
            Value::Object(_) => value
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            Value::Array(items) => items.iter().map(one).collect::<Vec<_>>().join("\n"),
            _ => String::new(),
        }
    }
    contents.map(one).unwrap_or_default().trim().to_owned()
}

/// The byte range a completion replaces, given the text it was asked in.
pub fn completion_range(
    item: &Completion,
    rope: &crate::text::rope::Rope,
    caret: usize,
) -> std::ops::Range<usize> {
    if let Some((start, end, _)) = &item.edit {
        return offset_of(rope, *start)..offset_of(rope, *end);
    }
    // No edit given: the word before the caret.
    let line = rope.byte_to_line(caret);
    let line_start = rope.line_to_byte(line);
    let before = rope.slice_to_string(line_start..caret);
    let word_len = before
        .chars()
        .rev()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .map(char::len_utf8)
        .sum::<usize>();
    caret - word_len..caret
}

/// What a completion inserts.
pub fn completion_text(item: &Completion) -> &str {
    match &item.edit {
        Some((_, _, text)) => text,
        None => &item.insert_text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn workspace_edits_come_from_either_shape() {
        let changes = crate::json::parse(
            r#"{"changes":{"file:///p/a.rs":[{"range":{"start":{"line":1,"character":2},"end":{"line":1,"character":5}},"newText":"x"}]}}"#,
        )
        .unwrap();
        let parsed = parse_workspace_edit(&changes);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, PathBuf::from("/p/a.rs"));
        assert_eq!(parsed[0].1[0].text, "x");
        let document = crate::json::parse(
            r#"{"documentChanges":[{"textDocument":{"uri":"file:///p/b.rs","version":3},"edits":[]},{"kind":"create","uri":"file:///p/c.rs"}]}"#,
        )
        .unwrap();
        let parsed = parse_workspace_edit(&document);
        assert_eq!(parsed, vec![(PathBuf::from("/p/b.rs"), Vec::new())]);
    }

    #[test]
    fn signature_parameters_by_substring_or_utf16_bounds() {
        let by_text = crate::json::parse(
            r#"{"signatures":[{"label":"fn f(a: u8, b: u8)","parameters":[{"label":"a: u8"},{"label":"b: u8"}]}],"activeParameter":1}"#,
        )
        .unwrap();
        let s = parse_signature(&by_text).unwrap();
        assert_eq!(&s.label[s.active.unwrap()], "b: u8");
        let by_bounds = crate::json::parse(
            r#"{"signatures":[{"label":"é(a, b)","parameters":[[2,3],[5,6]],"activeParameter":0}]}"#,
        )
        .unwrap();
        // Bounds are UTF-16 units; "é" is one unit and two bytes.
        let s = parse_signature(&by_bounds).unwrap();
        assert_eq!(s.active, None, "parameters without labels are not ranges");
        let with_labels = crate::json::parse(
            r#"{"signatures":[{"label":"é(a, b)","parameters":[{"label":[2,3]},{"label":[5,6]}],"activeParameter":1}]}"#,
        )
        .unwrap();
        let s = parse_signature(&with_labels).unwrap();
        assert_eq!(&s.label[s.active.unwrap()], "b");
        assert_eq!(parse_signature(&Value::Null), None);
    }

    fn fake() -> Option<(Server, tempdir::Dir)> {
        let python = ["/usr/bin/python3", "/opt/homebrew/bin/python3"]
            .iter()
            .map(Path::new)
            .find(|p| p.exists())?;
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/fake-lsp.py");
        let dir = tempdir::Dir::new("caio-lsp");
        let server = Server::start(
            "fake",
            python,
            &[script.to_str().unwrap()],
            &dir.0,
            Box::new(|| {}),
        )
        .ok()?;
        Some((server, dir))
    }

    mod tempdir {
        pub struct Dir(pub std::path::PathBuf);
        impl Dir {
            pub fn new(prefix: &str) -> Dir {
                let dir = std::env::temp_dir().join(format!("{prefix}-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).unwrap();
                Dir(dir)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn talks_to_a_server_end_to_end() {
        let Some((mut server, dir)) = fake() else {
            eprintln!("no python3; skipping the fake server test");
            return;
        };
        let ready = server.wait_for(Duration::from_secs(10), |e| matches!(e, Event::Ready));
        assert_eq!(
            ready,
            Some(Event::Ready),
            "{}",
            server.transport.stderr_tail()
        );
        assert_eq!(server.trigger_characters, ["."]);

        let file = dir.0.join("main.rs");
        let text = "fn main() {\n    // TODO later\n    let x = ERROR;\n}\n";
        server.did_open(&file, "rust", text);
        let published = server.wait_for(Duration::from_secs(5), |e| {
            matches!(e, Event::Diagnostics(_))
        });
        assert_eq!(published, Some(Event::Diagnostics(file.clone())));
        let diagnostics = &server.diagnostics[&file];
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics[0].severity, Severity::Warning);
        assert_eq!(
            diagnostics[0].start,
            Position {
                line: 1,
                character: 7
            }
        );
        assert_eq!(diagnostics[1].severity, Severity::Error);
        assert_eq!(diagnostics[1].message, "error on line 3");

        server.did_change(&file, "fn main() {}\n");
        let republished = server.wait_for(Duration::from_secs(5), |e| {
            matches!(e, Event::Diagnostics(_))
        });
        assert!(republished.is_some());
        assert!(
            server.diagnostics[&file].is_empty(),
            "fixed text clears diagnostics"
        );

        // The server computes edit ranges from the text it was sent.
        server.did_change(&file, "let a = alp\n");
        assert!(
            server
                .wait_for(Duration::from_secs(5), |e| matches!(
                    e,
                    Event::Diagnostics(_)
                ))
                .is_some()
        );
        let rope = crate::text::rope::Rope::from_text("let a = alp\n");
        let caret = "let a = alp".len();
        let at = super::super::position_of(&rope, caret);
        let id = server.completion(&file, at);
        let reply = server.wait_for(Duration::from_secs(5), |e| {
            matches!(e, Event::Completions { .. })
        });
        let Some(Event::Completions { items, request, .. }) = reply else {
            panic!("no completions");
        };
        assert_eq!(request, id);
        assert_eq!(
            items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(),
            ["alpha", "alphabet", "beta", "gamma"]
        );
        assert_eq!(
            completion_range(&items[0], &rope, caret),
            8..11,
            "the edit replaces the typed word"
        );
        assert_eq!(completion_text(&items[0]), "alpha()");
        assert_eq!(
            completion_range(&items[3], &rope, caret),
            8..11,
            "without an edit, the word before the caret"
        );
        assert_eq!(completion_text(&items[3]), "gamma_value");

        server.definition(&file, at);
        let reply = server.wait_for(Duration::from_secs(5), |e| {
            matches!(e, Event::Definition(_))
        });
        assert_eq!(
            reply,
            Some(Event::Definition(vec![Location {
                path: file.clone(),
                start: Position {
                    line: 0,
                    character: 0
                },
                end: Position {
                    line: 0,
                    character: 4
                }
            }]))
        );

        server.hover(&file, at);
        let reply = server.wait_for(Duration::from_secs(5), |e| matches!(e, Event::Hover { .. }));
        assert_eq!(
            reply,
            Some(Event::Hover {
                path: file.clone(),
                text: "**hover** text".into()
            })
        );

        server.did_close(&file);
        assert!(!server.diagnostics.contains_key(&file));
        server.shutdown();
        // The server answers shutdown, gets exit, and its stream closes.
        let closed = server.wait_for(Duration::from_secs(5), |e| matches!(e, Event::Failed(_)));
        assert!(closed.is_some(), "the stream closed without a Failed event");
    }

    #[test]
    fn a_missing_binary_fails_to_start() {
        let result = Server::start(
            "none",
            Path::new("/nonexistent/caio-lsp"),
            &[],
            Path::new("/tmp"),
            Box::new(|| {}),
        );
        assert!(result.is_err());
    }

    #[test]
    fn hover_contents_flatten_every_shape() {
        assert_eq!(hover_text(Some(&string("plain"))), "plain");
        assert_eq!(
            hover_text(Some(&object([
                ("kind", string("markdown")),
                ("value", string("v"))
            ]))),
            "v"
        );
        assert_eq!(
            hover_text(Some(&Value::Array(vec![
                string("a"),
                object([("language", string("rust")), ("value", string("b"))])
            ]))),
            "a\nb"
        );
        assert_eq!(hover_text(None), "");
    }
}
