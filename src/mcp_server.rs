//! `crc --mcp [folder]`: crc as an MCP server, so any agent on the machine
//! can ask the project what an editor knows, without the editor running.
//!
//! Over stdio, one JSON-RPC message per line, in both protocol eras: a
//! client may open with `server/discover` and carry the revision in every
//! request's `_meta` (2026-07-28), or with the `initialize` handshake.
//!
//! Every tool reads; none writes, runs a command or reaches the network.
//! Answers come from the project index (built by the same indexer the app
//! uses, started here when the app is not running), from files under the
//! folder, and from the workspace's notes.
//!
//! Tools:
//!
//! - `project.symbols`: definitions whose name holds the query's letters in
//!   order, best first, with file and line.
//! - `project.files`: files whose path holds the query's letters in order.
//! - `project.read`: lines of a file under the folder, bounded.
//! - `workspace.state`: the state doc and what waits on the person.
//! - `workspace.repos`: each repository's branch, changes and status.
//!
//! The log (stderr) never carries a tool's arguments.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use crate::json::{self, Value, object, string};
use crate::mcp_client::{LEGACY, MODERN};

/// The most of a file `project.read` returns.
const READ_LIMIT: usize = 64 * 1024;
/// The most results a search returns.
const RESULT_LIMIT: usize = 200;
/// How many candidates the index is asked for before ranking.
const CANDIDATES: usize = 2_000;

pub struct Server {
    root: PathBuf,
    workspace: crate::project::workspace::Workspace,
    /// Keeps the index up to date while the server runs; `None` when the
    /// cache folder cannot be used.
    _indexer: Option<crate::index::store::Indexer>,
}

impl Server {
    /// A server for `root`. With `index`, the project index is built and
    /// kept current while it runs; tests leave it off, so they never write
    /// into the person's cache.
    pub fn new(root: &Path, index: bool) -> Server {
        let root = crate::platform::canonical(root);
        Server {
            workspace: crate::project::workspace::Workspace::open(&root),
            _indexer: index
                .then(|| crate::index::store::Indexer::start(root.clone()))
                .flatten(),
            root,
        }
    }

    /// Answers every message on `input` until it closes.
    pub fn serve(&self, input: impl BufRead, mut output: impl Write) {
        let mut input = input;
        loop {
            match crate::lsp::transport::read_line_message(&mut input) {
                Ok(None) | Err(_) => return,
                Ok(Some(Err(why))) => {
                    // Not JSON: no id to answer, so the parse error goes
                    // without one, as JSON-RPC says.
                    let reply = object([
                        ("jsonrpc", string("2.0")),
                        ("id", Value::Null),
                        (
                            "error",
                            object([("code", json::number(-32700)), ("message", string(&why))]),
                        ),
                    ]);
                    let _ = writeln!(output, "{}", json::compact(&reply));
                }
                Ok(Some(Ok(message))) => {
                    if let Some(reply) = self.handle(&message) {
                        let _ = writeln!(output, "{}", json::compact(&reply));
                    }
                }
            }
            let _ = output.flush();
        }
    }

    /// The reply to one message, `None` for a notification.
    pub fn handle(&self, message: &Value) -> Option<Value> {
        let id = message.get("id")?.clone();
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message
            .get("params")
            .cloned()
            .unwrap_or(Value::Object(Vec::new()));
        let version = params
            .get("_meta")
            .and_then(|m| m.get("io.modelcontextprotocol/protocolVersion"))
            .and_then(Value::as_str);
        let modern = match version {
            Some(v) if v == MODERN => true,
            Some(other) => {
                return Some(object([
                    ("jsonrpc", string("2.0")),
                    ("id", id),
                    (
                        "error",
                        object([
                            ("code", json::number(-32022)),
                            ("message", string("Unsupported protocol version")),
                            (
                                "data",
                                object([
                                    (
                                        "supported",
                                        Value::Array(vec![string(MODERN), string(LEGACY)]),
                                    ),
                                    ("requested", string(other)),
                                ]),
                            ),
                        ]),
                    ),
                ]));
            }
            None => false,
        };
        let result = match method {
            "server/discover" if modern => Ok(object([
                (
                    "supportedVersions",
                    Value::Array(vec![string(MODERN), string(LEGACY)]),
                ),
                ("capabilities", capabilities()),
                ("serverInfo", server_info()),
            ])),
            "initialize" if !modern => Ok(object([
                // A client asking for the legacy revision crc knows gets it;
                // any other gets the newest legacy one, as the handshake
                // allows.
                ("protocolVersion", string(LEGACY)),
                ("capabilities", capabilities()),
                ("serverInfo", server_info()),
            ])),
            "ping" => Ok(Value::Object(Vec::new())),
            "tools/list" => Ok(object([("tools", Value::Array(tools()))])),
            "tools/call" => Ok(self.call(&params)),
            "resources/list" => Ok(object([("resources", Value::Array(Vec::new()))])),
            "prompts/list" => Ok(object([("prompts", Value::Array(Vec::new()))])),
            _ => Err((-32601, format!("Method not found: {method}"))),
        };
        Some(match result {
            Ok(mut value) => {
                if modern && let Value::Object(members) = &mut value {
                    members.push(("resultType".into(), string("complete")));
                }
                json::rpc::response(&id, value)
            }
            Err((code, why)) => json::rpc::error(&id, code, &why),
        })
    }

    /// A tool's answer: the result as text, and as structured content. A
    /// tool that fails says so in the result, with `isError`, so the agent
    /// sees why.
    fn call(&self, params: &Value) -> Value {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or(Value::Object(Vec::new()));
        eprintln!("crc mcp: {name}");
        let outcome = match name {
            "project.symbols" => self.symbols(&args),
            "project.files" => self.files(&args),
            "project.read" => self.read(&args),
            "workspace.state" => self.state(),
            "workspace.repos" => self.repos(),
            _ => Err(format!("no tool named {name}")),
        };
        match outcome {
            Ok(value) => object([
                (
                    "content",
                    Value::Array(vec![object([
                        ("type", string("text")),
                        ("text", string(&json::pretty(&value))),
                    ])]),
                ),
                ("structuredContent", value),
            ]),
            Err(why) => object([
                (
                    "content",
                    Value::Array(vec![object([
                        ("type", string("text")),
                        ("text", string(&why)),
                    ])]),
                ),
                ("isError", Value::Bool(true)),
            ]),
        }
    }

    fn reader(&self) -> Result<crate::index::store::Reader, String> {
        let path =
            crate::index::store::db_path(&self.root).ok_or("no cache folder for the index")?;
        crate::index::store::Reader::open(&path)
            .ok_or_else(|| "the index is still being built; ask again in a moment".to_string())
    }

    fn symbols(&self, args: &Value) -> Result<Value, String> {
        let query = text_arg(args, "query")?;
        let limit = limit_arg(args);
        let found = self
            .reader()?
            .containing(&query, CANDIDATES)
            .map_err(|e| format!("{e:?}"))?;
        let order = crate::project::finder::ranked(&found, &query, |s| &s.name);
        let rows: Vec<Value> = order
            .into_iter()
            .take(limit)
            .map(|i| {
                let s = &found[i];
                object([
                    ("name", string(&s.name)),
                    ("kind", string(&s.kind)),
                    ("path", string(&s.path)),
                    ("line", json::number(s.line + 1)),
                ])
            })
            .collect();
        Ok(object([("symbols", Value::Array(rows))]))
    }

    fn files(&self, args: &Value) -> Result<Value, String> {
        // "git panel" means the letters of both words, in order.
        let query: String = text_arg(args, "query")?.split_whitespace().collect();
        let limit = limit_arg(args);
        let found = self
            .reader()?
            .paths(&query, CANDIDATES)
            .map_err(|e| format!("{e:?}"))?;
        let order = crate::project::finder::ranked(&found, &query, String::as_str);
        let rows: Vec<Value> = order
            .into_iter()
            .take(limit)
            .map(|i| string(&found[i]))
            .collect();
        Ok(object([("files", Value::Array(rows))]))
    }

    /// Lines `start` to `end` (1-based, inclusive) of a file under the
    /// folder. A path that leaves the folder, through `..` or a link, is
    /// refused.
    fn read(&self, args: &Value) -> Result<Value, String> {
        let rel = text_arg(args, "path")?;
        let path = inside(&self.root, &rel)?;
        let start = args
            .get("start_line")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let end = args
            .get("end_line")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        let bytes = std::fs::read(&path).map_err(|e| format!("{rel}: {e}"))?;
        let text = String::from_utf8_lossy(&bytes);
        let mut out = String::new();
        let mut truncated = false;
        let mut last = start.saturating_sub(1);
        for (n, line) in text.lines().enumerate().skip(start - 1) {
            if end.is_some_and(|e| n + 1 > e) {
                break;
            }
            if out.len() + line.len() + 1 > READ_LIMIT {
                truncated = true;
                break;
            }
            out.push_str(line);
            out.push('\n');
            last = n + 1;
        }
        Ok(object([
            ("path", string(&rel)),
            ("start_line", json::number(start)),
            ("end_line", json::number(last)),
            ("lines_in_file", json::number(text.lines().count())),
            ("truncated", Value::Bool(truncated)),
            ("text", string(&out)),
        ]))
    }

    fn state(&self) -> Result<Value, String> {
        let settings = crate::project::workspace::Settings::load(&self.root);
        let Some(path) = settings.state else {
            return Err("this workspace names no state doc (.crc/workspace.toml `state`)".into());
        };
        let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let waiting =
            crate::project::workspace::waiting_items(&text, settings.waiting_heading.as_deref());
        let shown: String = text.chars().take(READ_LIMIT).collect();
        Ok(object([
            (
                "path",
                string(
                    &path
                        .strip_prefix(&self.root)
                        .unwrap_or(&path)
                        .to_string_lossy(),
                ),
            ),
            (
                "waiting",
                Value::Array(waiting.iter().map(|w| string(w)).collect()),
            ),
            ("truncated", Value::Bool(shown.len() < text.len())),
            ("text", string(&shown)),
        ]))
    }

    fn repos(&self) -> Result<Value, String> {
        let summary = self.workspace.summary(None);
        Ok(object([(
            "repositories",
            Value::Array(
                summary
                    .repos
                    .iter()
                    .map(|r| {
                        object([
                            ("name", string(&r.label)),
                            ("status", string(&r.status)),
                            ("changes", json::number(r.changes)),
                        ])
                    })
                    .collect(),
            ),
        )]))
    }
}

/// `rel` under `root`, refused when it leaves it, lexically or through a
/// symbolic link.
fn inside(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let candidate = Path::new(rel);
    if candidate.is_absolute()
        || candidate.components().any(|c| {
            !matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        return Err(format!("{rel}: give a path inside the folder, without .."));
    }
    let resolved =
        std::fs::canonicalize(root.join(candidate)).map_err(|e| format!("{rel}: {e}"))?;
    if !resolved.starts_with(root) {
        return Err(format!("{rel}: leads outside the folder"));
    }
    if !resolved.is_file() {
        return Err(format!("{rel}: not a file"));
    }
    Ok(resolved)
}

fn text_arg(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("\"{key}\" is required"))
}

fn limit_arg(args: &Value) -> usize {
    args.get("limit")
        .and_then(Value::as_u64)
        .map_or(50, |n| n as usize)
        .clamp(1, RESULT_LIMIT)
}

fn capabilities() -> Value {
    object([("tools", Value::Object(Vec::new()))])
}

fn server_info() -> Value {
    object([
        ("name", string("crc")),
        ("version", string(env!("CARGO_PKG_VERSION"))),
    ])
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    object([
        ("name", string(name)),
        ("description", string(description)),
        (
            "inputSchema",
            object([
                ("type", string("object")),
                ("properties", properties),
                (
                    "required",
                    Value::Array(required.iter().map(|r| string(r)).collect()),
                ),
            ]),
        ),
        ("annotations", object([("readOnlyHint", Value::Bool(true))])),
    ])
}

fn tools() -> Vec<Value> {
    let query = |what: &str| object([("type", string("string")), ("description", string(what))]);
    let limit = || {
        object([
            ("type", string("integer")),
            (
                "description",
                string("At most this many, 1 to 200; 50 when left out."),
            ),
        ])
    };
    vec![
        tool(
            "project.symbols",
            "Definitions in the project (functions, types, constants) whose name holds the query's letters in order, best match first, with the file and line. Faster and more exact than searching the text.",
            object([
                (
                    "query",
                    query("Part of a name, e.g. \"parse_line\" or \"pl\"."),
                ),
                ("limit", limit()),
            ]),
            &["query"],
        ),
        tool(
            "project.files",
            "Files in the project whose path holds the query's letters in order, best match first. Paths are relative to the project folder.",
            object([
                (
                    "query",
                    query("Part of a path, e.g. \"git panel\" or \"src/main\"."),
                ),
                ("limit", limit()),
            ]),
            &["query"],
        ),
        tool(
            "project.read",
            "Lines of a file in the project, at most 64 KiB, with how many lines the file has.",
            object([
                ("path", query("Relative to the project folder.")),
                (
                    "start_line",
                    object([
                        ("type", string("integer")),
                        ("description", string("First line, from 1.")),
                    ]),
                ),
                (
                    "end_line",
                    object([
                        ("type", string("integer")),
                        ("description", string("Last line, included.")),
                    ]),
                ),
            ]),
            &["path"],
        ),
        tool(
            "workspace.state",
            "The workspace's state doc: where the work stands, what is next, and the list of what waits on the person. Read it before starting work.",
            Value::Object(Vec::new()),
            &[],
        ),
        tool(
            "workspace.repos",
            "Each Git repository in the workspace with its branch, distance from upstream, and number of changed files.",
            Value::Object(Vec::new()),
            &[],
        ),
    ]
}

/// The command an agent runs to reach this server for `folder`: this
/// binary, `--mcp`, and the folder, each quoted for a shell.
pub fn command_line(exe: &Path, folder: &Path) -> String {
    let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
    format!("{} --mcp {}", quote(exe), quote(folder))
}

/// Runs the server on stdin and stdout for `folder`.
pub fn run(folder: &Path) {
    let server = Server::new(folder, true);
    let stdin = std::io::stdin();
    server.serve(stdin.lock(), std::io::stdout().lock());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::TempTree;

    fn ask(server: &Server, line: &str) -> Value {
        server.handle(&json::parse(line).unwrap()).unwrap()
    }

    #[test]
    fn both_eras_open_and_list_the_tools() {
        let t = TempTree::new("mcp-server-eras", &[("a.txt", "one\n")]);
        let server = Server::new(&t.0, false);
        let init = ask(
            &server,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        );
        assert_eq!(
            init.path("result.protocolVersion").and_then(Value::as_str),
            Some(LEGACY)
        );
        assert!(init.path("result.resultType").is_none());
        let meta = format!(
            r#"{{"io.modelcontextprotocol/protocolVersion":"{MODERN}","io.modelcontextprotocol/clientCapabilities":{{}}}}"#
        );
        let discover = ask(
            &server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"server/discover","params":{{"_meta":{meta}}}}}"#
            ),
        );
        assert_eq!(
            discover.path("result.resultType").and_then(Value::as_str),
            Some("complete")
        );
        let list = ask(
            &server,
            &format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{{"_meta":{meta}}}}}"#
            ),
        );
        let names: Vec<&str> = list
            .path("result.tools")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();
        assert_eq!(
            names,
            [
                "project.symbols",
                "project.files",
                "project.read",
                "workspace.state",
                "workspace.repos"
            ]
        );
        let wrong = ask(
            &server,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"1999-01-01"}}}"#,
        );
        assert_eq!(
            wrong.path("error.code").and_then(Value::as_i64),
            Some(-32022)
        );
        assert!(
            server
                .handle(
                    &json::parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                        .unwrap()
                )
                .is_none()
        );
    }

    #[test]
    fn read_stays_inside_the_folder() {
        let t = TempTree::new(
            "mcp-server-read",
            &[
                ("src/a.rs", "one\ntwo\nthree\n"),
                ("../mcp-outside.txt", "secret"),
            ],
        );
        let server = Server::new(&t.0, false);
        let read = |args: &str| {
            ask(
                &server,
                &format!(
                    r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"project.read","arguments":{args}}}}}"#
                ),
            )
        };
        let ok = read(r#"{"path":"src/a.rs","start_line":2,"end_line":3}"#);
        assert_eq!(
            ok.path("result.structuredContent.text")
                .and_then(Value::as_str),
            Some("two\nthree\n")
        );
        assert_eq!(
            ok.path("result.structuredContent.lines_in_file")
                .and_then(Value::as_u64),
            Some(3)
        );
        for bad in [
            r#"{"path":"../mcp-outside.txt"}"#,
            r#"{"path":"/etc/hosts"}"#,
            r#"{"path":"src"}"#,
        ] {
            let answer = read(bad);
            assert_eq!(
                answer.path("result.isError").and_then(Value::as_bool),
                Some(true),
                "{bad}"
            );
        }
        std::os::unix::fs::symlink("/etc", t.0.join("etc")).unwrap();
        assert_eq!(
            read(r#"{"path":"etc/hosts"}"#)
                .path("result.isError")
                .and_then(Value::as_bool),
            Some(true)
        );
        let _ = std::fs::remove_file(t.0.join("../mcp-outside.txt"));
    }

    #[test]
    fn a_broken_stream_is_answered_and_never_ends_the_server() {
        let t = TempTree::new("mcp-server-stream", &[]);
        let server = Server::new(&t.0, false);
        let deep = "[".repeat(10_000);
        let input = format!(
            "not json\n\n{deep}\n{{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}}\n{{\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"nope\"}}\n"
        );
        let mut out = Vec::new();
        server.serve(std::io::BufReader::new(input.as_bytes()), &mut out);
        let lines: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| json::parse(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(
            lines[0].path("error.code").and_then(Value::as_i64),
            Some(-32700)
        );
        assert_eq!(
            lines[1].path("error.code").and_then(Value::as_i64),
            Some(-32700)
        );
        assert_eq!(lines[2].get("id").and_then(Value::as_u64), Some(7));
        assert_eq!(
            lines[3].path("error.code").and_then(Value::as_i64),
            Some(-32601)
        );
    }

    #[test]
    fn the_command_line_quotes_for_a_shell() {
        assert_eq!(
            command_line(
                Path::new("/Applications/crc.app/Contents/MacOS/crc"),
                Path::new("/tmp/Bob's garden")
            ),
            "'/Applications/crc.app/Contents/MacOS/crc' --mcp '/tmp/Bob'\\''s garden'"
        );
    }

    #[test]
    fn the_workspace_tools_read_its_notes() {
        let t = TempTree::new(
            "mcp-server-ws",
            &[(
                "docs/state.md",
                "# State\n\n## Waiting on you\n\n- Water the tomatoes\n",
            )],
        );
        let server = Server::new(&t.0, false);
        let state = ask(
            &server,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"workspace.state","arguments":{}}}"#,
        );
        let waiting = state
            .path("result.structuredContent.waiting")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(waiting, [string("Water the tomatoes")]);
    }
}
