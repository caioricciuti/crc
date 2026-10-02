//! MCP servers the person uses directly: started on a click, listed with
//! their tools, resources and prompts, and called from a document.
//!
//! This is the client side of the Model Context Protocol over stdio. It
//! speaks both eras of the protocol: the stateless revision of 2026-07-28,
//! where every request carries its version in `_meta`, and the earlier
//! revisions that open with an `initialize` handshake. A server is probed
//! with `server/discover` first; any answer but a modern one, or none
//! within a few seconds, means it is a legacy server, as the stdio binding
//! of the spec says.
//!
//! Everything here runs on the main thread except the transport's reader
//! and writer threads: [`Server::poll`] takes what arrived and moves the
//! state machine, the way the language-server client does.
//!
//! What crc does not do yet, and says so when a server asks: answer
//! sampling, elicitation or roots requests (legacy servers send them as
//! requests, modern ones as `input_required` results), Streamable HTTP,
//! and OAuth.

pub mod call;
pub mod config;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::json::{self, Value, object, string};
use crate::lsp::transport::{Framing, Incoming, Transport, Wake};

pub use config::{Config, ServerConfig};

/// The modern revision crc speaks.
pub const MODERN: &str = "2026-07-28";
/// The legacy revision crc asks for in `initialize`.
pub const LEGACY: &str = "2025-11-25";
/// How long the `server/discover` probe waits before calling the server
/// legacy.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long any other request waits.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// How many pages of a list are followed before the rest is left out.
const MAX_PAGES: usize = 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Era {
    Modern,
    Legacy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Stopped,
    Starting,
    Ready,
    Failed(String),
}

impl Status {
    pub fn label(&self) -> String {
        match self {
            Status::Stopped => "stopped".into(),
            Status::Starting => "starting".into(),
            Status::Ready => "ready".into(),
            Status::Failed(why) => format!("failed: {why}"),
        }
    }
}

/// A tool, as the server lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct Tool {
    pub name: String,
    pub title: Option<String>,
    pub description: String,
    pub input_schema: Value,
    /// `annotations.readOnlyHint`: says it changes nothing.
    pub read_only: bool,
    /// `annotations.destructiveHint`: may delete or overwrite. The spec's
    /// default for a tool that is not read-only is true.
    pub destructive: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Resource {
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime_type: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Prompt {
    pub name: String,
    pub description: String,
    /// Argument names, and whether each is required.
    pub arguments: Vec<(String, bool)>,
}

/// What a request was for, so its answer lands in the right place.
#[derive(Clone, Debug)]
enum Purpose {
    Discover,
    Initialize,
    ListTools(Vec<Tool>, usize),
    ListResources(Vec<Resource>, usize),
    ListPrompts(Vec<Prompt>, usize),
    /// A call made from a document: the tab its answer goes to.
    Call(u64),
}

struct Pending {
    purpose: Purpose,
    method: String,
    sent: Instant,
}

/// What [`Server::poll`] tells the window.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// The status or a list changed: redraw.
    Changed,
    /// A call's answer, for the tab `id`, as text to show.
    Answer { id: u64, text: String, error: bool },
}

pub struct Server {
    pub name: String,
    pub config: ServerConfig,
    pub status: Status,
    pub era: Option<Era>,
    pub tools: Vec<Tool>,
    pub resources: Vec<Resource>,
    pub prompts: Vec<Prompt>,
    /// What the server says it is, from its info.
    pub info: String,
    transport: Option<Transport>,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    /// Calls asked for before the server was ready, sent once it is.
    queued: Vec<(Purpose, String, Value)>,
}

impl Server {
    pub fn new(name: &str, config: ServerConfig) -> Server {
        Server {
            name: name.to_owned(),
            config,
            status: Status::Stopped,
            era: None,
            tools: Vec::new(),
            resources: Vec::new(),
            prompts: Vec::new(),
            info: String::new(),
            transport: None,
            next_id: 1,
            pending: HashMap::new(),
            queued: Vec::new(),
        }
    }

    pub fn is_running(&self) -> bool {
        self.transport.is_some()
    }

    /// Starts the server in `cwd` and probes its era. A command that would
    /// fetch code, or that is not installed, is refused before anything
    /// runs.
    pub fn start(&mut self, cwd: &std::path::Path, wake: Wake) {
        self.stop();
        let program = match self.config.resolve() {
            Ok(program) => program,
            Err(why) => {
                self.status = Status::Failed(why);
                return;
            }
        };
        let args: Vec<&str> = self.config.args.iter().map(String::as_str).collect();
        let mut env: Vec<(String, String)> = self.config.env.clone();
        // Interpreters a script names on its first line are found on the
        // same directories crc searched for the command.
        if !env.iter().any(|(k, _)| k == "PATH")
            && let Ok(path) = std::env::join_paths(crate::lsp::servers::search_dirs())
        {
            env.push(("PATH".into(), path.to_string_lossy().into_owned()));
        }
        let cwd = self.config.cwd.clone().unwrap_or_else(|| cwd.to_path_buf());
        match Transport::spawn_with(&program, &args, &cwd, &env, Framing::Lines, wake) {
            Ok(transport) => {
                self.transport = Some(transport);
                self.status = Status::Starting;
                self.era = None;
                let params = self.modern_params(Value::Object(Vec::new()));
                self.send(Purpose::Discover, "server/discover", params);
            }
            Err(e) => {
                self.status = Status::Failed(format!("could not start {}: {e}", program.display()))
            }
        }
    }

    /// Stops the server: its process is killed, waiting calls fail.
    pub fn stop(&mut self) {
        self.transport = None;
        self.pending.clear();
        self.queued.clear();
        self.era = None;
        if !matches!(self.status, Status::Failed(_)) {
            self.status = Status::Stopped;
        }
    }

    /// Calls a tool; the answer comes back as [`Event::Answer`] for `id`.
    pub fn call_tool(&mut self, id: u64, name: &str, arguments: Value) {
        let params = object([("name", string(name)), ("arguments", arguments)]);
        self.request_when_ready(Purpose::Call(id), "tools/call", params);
    }

    pub fn read_resource(&mut self, id: u64, uri: &str) {
        let params = object([("uri", string(uri))]);
        self.request_when_ready(Purpose::Call(id), "resources/read", params);
    }

    pub fn get_prompt(&mut self, id: u64, name: &str, arguments: Value) {
        let params = object([("name", string(name)), ("arguments", arguments)]);
        self.request_when_ready(Purpose::Call(id), "prompts/get", params);
    }

    fn request_when_ready(&mut self, purpose: Purpose, method: &str, params: Value) {
        match self.status {
            Status::Ready => {
                let params = self.with_meta(params);
                self.send(purpose, method, params);
            }
            Status::Starting => self.queued.push((purpose, method.to_owned(), params)),
            _ => {}
        }
    }

    /// Takes what arrived and what timed out.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        let mut incoming = Vec::new();
        let mut closed = false;
        if let Some(transport) = &self.transport {
            while let Some(message) = transport.try_recv() {
                match message {
                    Incoming::Message(value) => incoming.push(value),
                    Incoming::Closed => closed = true,
                }
            }
        }
        for message in incoming {
            self.handle(message, &mut events);
        }
        // The probe's silence is an answer: a legacy server.
        let now = Instant::now();
        let late: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, p)| {
                let limit = if matches!(p.purpose, Purpose::Discover) {
                    PROBE_TIMEOUT
                } else {
                    REQUEST_TIMEOUT
                };
                now.duration_since(p.sent) > limit
            })
            .map(|(id, _)| *id)
            .collect();
        for id in late {
            if let Some(pending) = self.pending.remove(&id) {
                self.fail(pending, "no answer in time".into(), &mut events);
            }
        }
        if closed {
            let tail = self
                .transport
                .as_ref()
                .map(Transport::stderr_tail)
                .unwrap_or_default();
            let last = tail.lines().last().unwrap_or("").trim().to_owned();
            for (_, pending) in std::mem::take(&mut self.pending) {
                if let Purpose::Call(id) = pending.purpose {
                    events.push(Event::Answer {
                        id,
                        text: "The server exited before answering.".into(),
                        error: true,
                    });
                }
            }
            self.transport = None;
            self.status = Status::Failed(if last.is_empty() {
                "the server exited".into()
            } else {
                format!("the server exited: {last}")
            });
            events.push(Event::Changed);
        }
        events
    }

    /// Whether anything waits on an answer, so the display link keeps
    /// polling.
    pub fn busy(&self) -> bool {
        !self.pending.is_empty()
    }

    fn handle(&mut self, message: Value, events: &mut Vec<Event>) {
        // A request from a legacy server: crc answers ping, and says it
        // cannot do the rest yet rather than leaving the server waiting.
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            if let Some(id) = message.get("id") {
                let reply = if method == "ping" {
                    json::rpc::response(id, Value::Object(Vec::new()))
                } else {
                    json::rpc::error(id, -32601, &format!("crc does not answer {method} yet"))
                };
                self.write(&reply);
            } else if method.ends_with("/list_changed") {
                self.refresh_lists();
            }
            return;
        }
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            return;
        };
        let Some(pending) = self.pending.remove(&id) else {
            return;
        };
        if let Some(error) = message.get("error") {
            self.on_error(pending, error, events);
            return;
        }
        let result = message.get("result").cloned().unwrap_or(Value::Null);
        match result.get("resultType").and_then(Value::as_str) {
            None | Some("complete") => self.on_result(pending, result, events),
            Some("input_required") => self.fail(
                pending,
                "the server asked for input (a question, sampling or roots), which crc cannot answer yet".into(),
                events,
            ),
            Some(other) => self.fail(pending, format!("unknown resultType {other}"), events),
        }
    }

    fn on_error(&mut self, pending: Pending, error: &Value, events: &mut Vec<Event>) {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("error")
            .to_owned();
        if matches!(pending.purpose, Purpose::Discover) {
            if code == -32022 {
                // Modern, but not this version: the spec says never to
                // fall back to initialize then.
                let supported = error
                    .path("data.supported")
                    .and_then(Value::as_array)
                    .map(|v| {
                        v.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                self.status = Status::Failed(format!(
                    "the server speaks MCP {supported}; crc speaks {MODERN} and {LEGACY}"
                ));
                self.transport = None;
                events.push(Event::Changed);
            } else {
                self.initialize();
            }
            return;
        }
        self.fail(pending, format!("{message} ({code})"), events);
    }

    fn fail(&mut self, pending: Pending, why: String, events: &mut Vec<Event>) {
        match pending.purpose {
            Purpose::Discover => self.initialize(),
            Purpose::Initialize => {
                self.status = Status::Failed(why);
                self.transport = None;
                events.push(Event::Changed);
            }
            Purpose::ListTools(..) | Purpose::ListResources(..) | Purpose::ListPrompts(..) => {
                // A server without resources or prompts says so with an
                // error; the list is simply empty.
                events.push(Event::Changed);
            }
            Purpose::Call(id) => events.push(Event::Answer {
                id,
                text: format!("{} failed: {why}", pending.method),
                error: true,
            }),
        }
    }

    fn on_result(&mut self, pending: Pending, result: Value, events: &mut Vec<Event>) {
        match pending.purpose {
            Purpose::Discover => {
                let versions: Vec<&str> = result
                    .get("supportedVersions")
                    .and_then(Value::as_array)
                    .map(|v| v.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                if versions.contains(&MODERN) {
                    self.era = Some(Era::Modern);
                    self.info = implementation(
                        result
                            .get("_meta")
                            .and_then(|m| m.get("io.modelcontextprotocol/serverInfo")),
                    )
                    .or_else(|| implementation(result.get("serverInfo")))
                    .unwrap_or_default();
                    self.ready(events);
                } else {
                    // A discover result without our version: a server that
                    // answers anything, or a newer one. The handshake is the
                    // safer guess.
                    self.initialize();
                }
            }
            Purpose::Initialize => {
                self.era = Some(Era::Legacy);
                self.info = implementation(result.get("serverInfo")).unwrap_or_default();
                let note =
                    json::rpc::notification("notifications/initialized", Value::Object(Vec::new()));
                self.write(&note);
                self.ready(events);
            }
            Purpose::ListTools(mut tools, pages) => {
                tools.extend(
                    result
                        .get("tools")
                        .and_then(Value::as_array)
                        .unwrap_or_default()
                        .iter()
                        .filter_map(tool_from),
                );
                if let Some(cursor) = next_cursor(&result, pages) {
                    self.list(
                        "tools/list",
                        Purpose::ListTools(tools, pages + 1),
                        Some(cursor),
                    );
                } else {
                    self.tools = tools;
                    events.push(Event::Changed);
                }
            }
            Purpose::ListResources(mut resources, pages) => {
                resources.extend(
                    result
                        .get("resources")
                        .and_then(Value::as_array)
                        .unwrap_or_default()
                        .iter()
                        .filter_map(resource_from),
                );
                if let Some(cursor) = next_cursor(&result, pages) {
                    self.list(
                        "resources/list",
                        Purpose::ListResources(resources, pages + 1),
                        Some(cursor),
                    );
                } else {
                    self.resources = resources;
                    events.push(Event::Changed);
                }
            }
            Purpose::ListPrompts(mut prompts, pages) => {
                prompts.extend(
                    result
                        .get("prompts")
                        .and_then(Value::as_array)
                        .unwrap_or_default()
                        .iter()
                        .filter_map(prompt_from),
                );
                if let Some(cursor) = next_cursor(&result, pages) {
                    self.list(
                        "prompts/list",
                        Purpose::ListPrompts(prompts, pages + 1),
                        Some(cursor),
                    );
                } else {
                    self.prompts = prompts;
                    events.push(Event::Changed);
                }
            }
            Purpose::Call(id) => {
                let (text, error) = call::render(&pending.method, &result);
                events.push(Event::Answer { id, text, error });
            }
        }
    }

    fn initialize(&mut self) {
        let params = object([
            ("protocolVersion", string(LEGACY)),
            ("capabilities", Value::Object(Vec::new())),
            ("clientInfo", client_info()),
        ]);
        self.send(Purpose::Initialize, "initialize", params);
    }

    fn ready(&mut self, events: &mut Vec<Event>) {
        self.status = Status::Ready;
        events.push(Event::Changed);
        self.refresh_lists();
        for (purpose, method, params) in std::mem::take(&mut self.queued) {
            let params = self.with_meta(params);
            self.send(purpose, &method, params);
        }
    }

    fn refresh_lists(&mut self) {
        if self.status != Status::Ready {
            return;
        }
        self.list("tools/list", Purpose::ListTools(Vec::new(), 0), None);
        self.list(
            "resources/list",
            Purpose::ListResources(Vec::new(), 0),
            None,
        );
        self.list("prompts/list", Purpose::ListPrompts(Vec::new(), 0), None);
    }

    fn list(&mut self, method: &str, purpose: Purpose, cursor: Option<String>) {
        let params = match cursor {
            Some(c) => object([("cursor", string(&c))]),
            None => Value::Object(Vec::new()),
        };
        let params = self.with_meta(params);
        self.send(purpose, method, params);
    }

    /// `params` with the per-request fields a modern server requires.
    fn with_meta(&self, params: Value) -> Value {
        if self.era == Some(Era::Modern) {
            self.modern_params(params)
        } else {
            params
        }
    }

    fn modern_params(&self, params: Value) -> Value {
        let meta = object([
            ("io.modelcontextprotocol/protocolVersion", string(MODERN)),
            ("io.modelcontextprotocol/clientInfo", client_info()),
            (
                "io.modelcontextprotocol/clientCapabilities",
                Value::Object(Vec::new()),
            ),
        ]);
        let mut members = match params {
            Value::Object(members) => members,
            _ => Vec::new(),
        };
        members.retain(|(k, _)| k != "_meta");
        members.push(("_meta".into(), meta));
        Value::Object(members)
    }

    fn send(&mut self, purpose: Purpose, method: &str, params: Value) {
        let id = self.next_id;
        self.next_id += 1;
        let message = json::rpc::request(id, method, params);
        self.write(&message);
        self.pending.insert(
            id,
            Pending {
                purpose,
                method: method.to_owned(),
                sent: Instant::now(),
            },
        );
    }

    fn write(&mut self, message: &Value) {
        if let Some(transport) = self.transport.as_mut() {
            // A failed write shows up as the stream closing.
            let _ = transport.send(message);
        }
    }
}

fn client_info() -> Value {
    object([
        ("name", string("crc")),
        ("version", string(env!("CARGO_PKG_VERSION"))),
    ])
}

fn implementation(value: Option<&Value>) -> Option<String> {
    let value = value?;
    let name = value.get("name").and_then(Value::as_str)?;
    Some(match value.get("version").and_then(Value::as_str) {
        Some(v) => format!("{name} {v}"),
        None => name.to_owned(),
    })
}

fn next_cursor(result: &Value, pages: usize) -> Option<String> {
    result
        .get("nextCursor")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty() && pages + 1 < MAX_PAGES)
        .map(str::to_owned)
}

fn text_of(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn tool_from(value: &Value) -> Option<Tool> {
    let name = value.get("name")?.as_str()?.to_owned();
    let hint = |key: &str| {
        value
            .path(&format!("annotations.{key}"))
            .and_then(Value::as_bool)
    };
    let read_only = hint("readOnlyHint").unwrap_or(false);
    Some(Tool {
        name,
        title: value
            .get("title")
            .or_else(|| value.path("annotations.title"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        description: text_of(value, "description"),
        input_schema: value.get("inputSchema").cloned().unwrap_or(Value::Null),
        read_only,
        destructive: !read_only && hint("destructiveHint").unwrap_or(true),
    })
}

fn resource_from(value: &Value) -> Option<Resource> {
    Some(Resource {
        uri: value.get("uri")?.as_str()?.to_owned(),
        name: text_of(value, "name"),
        description: text_of(value, "description"),
        mime_type: value
            .get("mimeType")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn prompt_from(value: &Value) -> Option<Prompt> {
    Some(Prompt {
        name: value.get("name")?.as_str()?.to_owned(),
        description: text_of(value, "description"),
        arguments: value
            .get("arguments")
            .and_then(Value::as_array)
            .unwrap_or_default()
            .iter()
            .filter_map(|a| {
                let name = a.get("name")?.as_str()?.to_owned();
                Some((
                    name,
                    a.get("required").and_then(Value::as_bool).unwrap_or(false),
                ))
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests;
