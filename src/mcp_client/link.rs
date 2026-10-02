//! How a server is reached: its own process over stdio, or a URL over
//! Streamable HTTP. Both deliver [`Incoming`] messages to the same state
//! machine.
//!
//! Over HTTP each message is one POST through the system curl, on a thread
//! of its own; the reply is JSON, or a short `text/event-stream` that ends
//! with the response. The headers the spec asks for go on every POST:
//! `MCP-Protocol-Version`, and for the modern revision `Mcp-Method` and
//! `Mcp-Name`. A legacy server's `Mcp-Session-Id` is kept and sent back.
//! A reply that is not JSON-RPC (a 401, a 404, a proxy's page) becomes an
//! error for that request with a code outside the protocol's ranges, so it
//! is never mistaken for one the server sent.
//!
//! No sign-in: a server that answers 401 is said to need one.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use crate::json::{self, Value};
use crate::lsp::transport::{Incoming, Transport, Wake};

/// The code a local failure of an HTTP request is reported with: below the
/// JSON-RPC reserved range, so no server's error is ever this.
pub const LOCAL_ERROR: i64 = -1;

pub enum Link {
    Stdio(Transport),
    Http(Http),
}

impl Link {
    pub fn send(&mut self, message: &Value) {
        match self {
            // A failed write shows up as the stream closing.
            Link::Stdio(transport) => {
                let _ = transport.send(message);
            }
            Link::Http(http) => http.send(message),
        }
    }

    pub fn try_recv(&self) -> Option<Incoming> {
        match self {
            Link::Stdio(transport) => transport.try_recv(),
            Link::Http(http) => http.rx.try_recv().ok(),
        }
    }

    /// What the server last said on stderr; nothing for HTTP.
    pub fn stderr_tail(&self) -> String {
        match self {
            Link::Stdio(transport) => transport.stderr_tail(),
            Link::Http(_) => String::new(),
        }
    }
}

pub struct Http {
    /// Messages for the dispatcher thread, in the order they were sent.
    queue: mpsc::Sender<Value>,
    rx: mpsc::Receiver<Incoming>,
}

impl Http {
    /// A link to `url`; nothing is sent until the first message. Only
    /// `http` and `https` URLs are taken.
    ///
    /// One dispatcher thread takes messages in order. A notification is
    /// posted before the next message is looked at, so `initialized` always
    /// reaches a legacy server before the requests that follow it; each
    /// request then gets a thread of its own, so a slow tool does not hold
    /// up the lists.
    pub fn new(url: &str, wake: Wake) -> Result<Http, String> {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(format!("{url} is not an http or https URL"));
        }
        let (tx, rx) = mpsc::channel();
        let (queue, messages) = mpsc::channel::<Value>();
        let url = url.to_owned();
        let wake = Arc::new(wake);
        std::thread::spawn(move || {
            let session = Arc::new(Mutex::new(None::<String>));
            for message in messages {
                let current = session.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let request = prepare(&url, &message, current);
                let id = message.get("id").cloned();
                let (tx, wake, session) = (tx.clone(), wake.clone(), session.clone());
                let post = move || {
                    let replies = match crate::http::curl::send(&request) {
                        Ok(response) => {
                            if let Some((_, value)) = response
                                .headers
                                .iter()
                                .find(|(k, _)| k.eq_ignore_ascii_case("mcp-session-id"))
                            {
                                *session.lock().unwrap_or_else(|e| e.into_inner()) =
                                    Some(value.clone());
                            }
                            replies(&response, id.as_ref())
                        }
                        Err(error) => id
                            .as_ref()
                            .map(|id| local_error(id, &error))
                            .into_iter()
                            .collect(),
                    };
                    for reply in replies {
                        if tx.send(Incoming::Message(reply)).is_err() {
                            return;
                        }
                    }
                    (*wake)();
                };
                if message.get("id").is_some() {
                    std::thread::spawn(post);
                } else {
                    post();
                }
            }
        });
        Ok(Http { queue, rx })
    }

    fn send(&self, message: &Value) {
        let _ = self.queue.send(message.clone());
    }
}

/// The POST for one message, with the headers the transport requires.
pub fn prepare(url: &str, message: &Value, session: Option<String>) -> crate::http::Prepared {
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params");
    let modern = params
        .and_then(|p| p.get("_meta"))
        .and_then(|m| m.get("io.modelcontextprotocol/protocolVersion"))
        .and_then(Value::as_str);
    let mut headers = vec![
        ("Content-Type".to_owned(), "application/json".to_owned()),
        (
            "Accept".to_owned(),
            "application/json, text/event-stream".to_owned(),
        ),
    ];
    match modern {
        Some(version) => {
            headers.push(("MCP-Protocol-Version".into(), version.to_owned()));
            headers.push(("Mcp-Method".into(), method.to_owned()));
            let name = params
                .and_then(|p| p.get("name").or_else(|| p.get("uri")))
                .and_then(Value::as_str);
            if let Some(name) = name {
                headers.push(("Mcp-Name".into(), name.to_owned()));
            }
        }
        // The legacy handshake carries its version in the body; after it,
        // the header says which was agreed.
        None if method != "initialize" => {
            headers.push(("MCP-Protocol-Version".into(), super::LEGACY.to_owned()));
        }
        None => {}
    }
    if let Some(session) = session {
        headers.push(("Mcp-Session-Id".into(), session));
    }
    crate::http::Prepared {
        name: None,
        method: "POST".into(),
        url: url.to_owned(),
        headers,
        body: Some(json::compact(message).into_bytes()),
    }
}

/// The JSON-RPC messages in a reply: one JSON body, or each `data:` event
/// of an event stream. A reply with none, for a request, becomes an error
/// for it.
pub fn replies(response: &crate::http::curl::Response, id: Option<&Value>) -> Vec<Value> {
    let content_type = response
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.to_ascii_lowercase())
        .unwrap_or_default();
    let body = String::from_utf8_lossy(&response.body);
    let mut found: Vec<Value> = if content_type.starts_with("text/event-stream") {
        events(&body)
            .iter()
            .filter_map(|data| json::parse(data).ok())
            .collect()
    } else {
        json::parse(body.trim()).ok().into_iter().collect()
    };
    found.retain(|v| v.get("jsonrpc").is_some());
    let answered = id.is_some_and(|id| found.iter().any(|v| v.get("id") == Some(id)));
    if let Some(id) = id
        && !answered
    {
        let why = match response.status {
            401 | 403 => format!(
                "HTTP {}: the server needs a sign-in, which crc does not do yet",
                response.status
            ),
            0 => "no HTTP status".into(),
            status => format!("HTTP {status} {}", response.reason),
        };
        found.push(local_error(id, &why));
    }
    found
}

/// The `data` of each event in an event stream, multi-line data joined.
fn events(stream: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut data: Vec<&str> = Vec::new();
    for line in stream.lines().chain(std::iter::once("")) {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            if !data.is_empty() {
                out.push(data.join("\n"));
                data.clear();
            }
        } else if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    out
}

fn local_error(id: &Value, why: &str) -> Value {
    json::rpc::error(id, LOCAL_ERROR, why)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::curl::Response;

    fn response(status: u16, content_type: &str, body: &str) -> Response {
        Response {
            status,
            reason: "Whatever".into(),
            headers: vec![("Content-Type".into(), content_type.into())],
            body: body.as_bytes().to_vec(),
            ..Response::default()
        }
    }

    #[test]
    fn headers_follow_the_era() {
        let modern = json::parse(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"add",
               "_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#,
        )
        .unwrap();
        let p = prepare("http://127.0.0.1:9/mcp", &modern, None);
        let has = |p: &crate::http::Prepared, k: &str, v: &str| {
            p.headers.iter().any(|(a, b)| a == k && b == v)
        };
        assert!(has(&p, "MCP-Protocol-Version", "2026-07-28"));
        assert!(has(&p, "Mcp-Method", "tools/call"));
        assert!(has(&p, "Mcp-Name", "add"));
        assert_eq!(p.method, "POST");

        let legacy =
            json::parse(r#"{"jsonrpc":"2.0","id":4,"method":"tools/list","params":{}}"#).unwrap();
        let p = prepare("http://h/mcp", &legacy, Some("abc".into()));
        assert!(has(&p, "MCP-Protocol-Version", super::super::LEGACY));
        assert!(has(&p, "Mcp-Session-Id", "abc"));
        assert!(!p.headers.iter().any(|(k, _)| k == "Mcp-Method"));
        let init =
            json::parse(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#).unwrap();
        assert!(
            !prepare("http://h", &init, None)
                .headers
                .iter()
                .any(|(k, _)| k == "MCP-Protocol-Version")
        );
    }

    #[test]
    fn replies_come_from_json_or_an_event_stream() {
        let id = json::number(7);
        let json_reply = response(
            200,
            "application/json",
            r#"{"jsonrpc":"2.0","id":7,"result":{}}"#,
        );
        assert_eq!(replies(&json_reply, Some(&id)).len(), 1);

        let stream = response(
            200,
            "text/event-stream",
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n\
             data: {\"jsonrpc\":\"2.0\",\r\ndata: \"id\":7,\"result\":{}}\n\n",
        );
        let got = replies(&stream, Some(&id));
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].get("id"), Some(&id));

        let denied = replies(&response(401, "text/html", "<h1>no</h1>"), Some(&id));
        assert_eq!(denied.len(), 1);
        assert_eq!(
            denied[0].path("error.code").and_then(Value::as_i64),
            Some(LOCAL_ERROR)
        );
        assert!(
            denied[0]
                .path("error.message")
                .and_then(Value::as_str)
                .unwrap()
                .contains("sign-in")
        );
        // A notification's 202 needs no answer.
        assert!(replies(&response(202, "", ""), None).is_empty());
    }
}
