//! Sends a prepared request through `/usr/bin/curl` and turns the reply
//! into a document.
//!
//! One subprocess per request, on a worker thread, with the same shape of
//! limits Git commands have: a wall-clock timeout, a cap on how much of the
//! body is kept, and stderr shown verbatim on failure. The response document
//! is Markdown so the existing preview draws it: status and timing first,
//! then the headers, then the body, pretty-printed when it is JSON.

use super::Prepared;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Where macOS keeps its curl. A fixed path, not a PATH lookup: the editor
/// launched from the Dock has no shell profile, and a `curl` that turned up
/// first on PATH would be a surprise anyway.
pub const CURL: &str = "/usr/bin/curl";

/// curl set up for crc's own downloads (the update check, the extension
/// registry): https only, redirects too, failing on an HTTP error, at most
/// `max_bytes` and `seconds`. file:// only in tests and self-test runs.
/// The caller adds headers and the URL.
pub fn strict_get(max_bytes: u64, seconds: u32) -> Command {
    let protocols = if cfg!(test) || std::env::var_os("CRC_SELFTEST").is_some() {
        "=https,file"
    } else {
        "=https"
    };
    let mut command = Command::new(CURL);
    command
        .args(["--silent", "--show-error", "--fail", "--location"])
        .args(["--max-time", &seconds.to_string()])
        .args(["--max-filesize", &max_bytes.to_string()])
        .args(["--proto", protocols, "--proto-redir", "=https"])
        .arg("--user-agent")
        .arg(format!("crc/{}", env!("CARGO_PKG_VERSION")));
    command
}

/// How long a request may take end to end.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// The most body kept. Enough for any API answer a person reads; a file
/// download is not what a request tab is for.
pub const BODY_LIMIT: usize = 4 * 1024 * 1024;

/// A parsed reply.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Response {
    pub version: String,
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub truncated: bool,
    pub time_total: Option<f64>,
    pub time_first_byte: Option<f64>,
    pub remote_ip: Option<String>,
}

/// The curl invocation for `request`, as a command ready to spawn.
pub fn command(request: &Prepared) -> Command {
    let mut cmd = Command::new(CURL);
    cmd.arg("--silent")
        .arg("--show-error")
        .arg("--include")
        .arg("--no-buffer")
        .arg("--globoff")
        .arg("--max-time")
        .arg(TIMEOUT.as_secs().to_string())
        .arg("--connect-timeout")
        .arg("10")
        .arg("--request")
        .arg(&request.method)
        // HTTP only, redirects too: a .http file from a cloned repository
        // must not read `file:///` or speak `dict://`. Through a proxy the
        // tunnel's own `200 Connection established` is not the response.
        .args(["--proto", "=http,https", "--proto-redir", "=http,https"])
        .arg("--suppress-connect-headers")
        .arg("--write-out")
        .arg(WRITE_OUT);
    if request.method == "HEAD" {
        // `--request HEAD` alone makes curl wait for a body that never comes.
        cmd.arg("--head");
    }
    for (name, value) in &request.headers {
        // A header name is a token; `@file` as one makes curl read a file.
        if name.is_empty() || !name.bytes().all(is_token_byte) {
            continue;
        }
        if value.is_empty() {
            // `Name:` removes a header in curl's syntax; `Name;` sends it empty.
            cmd.arg("--header").arg(format!("{name};"));
        } else {
            cmd.arg("--header").arg(format!("{name}: {value}"));
        }
    }
    if request.body.is_some() {
        cmd.arg("--data-binary").arg("@-");
    }
    cmd.arg("--").arg(&request.url);
    cmd
}

/// A character HTTP allows in a header name (RFC 9110, `tchar`).
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Appended after the body by `--write-out`. The separators are control
/// characters no HTTP body should contain; a body that does is truncated at
/// the marker and says so.
const WRITE_OUT: &str =
    "\n\u{1e}caio\u{1f}%{time_total}\u{1f}%{time_starttransfer}\u{1f}%{remote_ip}\u{1e}";

/// Runs the request to completion. Blocking; call from a worker thread.
pub fn send(request: &Prepared) -> Result<Response, String> {
    if !std::path::Path::new(CURL).is_file() {
        return Err(format!("{CURL} is not installed"));
    }
    let mut child = command(request)
        .stdin(if request.body.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not start curl: {e}"))?;

    // Feed the body from another thread so a server that answers before
    // reading its input cannot deadlock the pipe.
    let feeder = request.body.clone().and_then(|body| {
        let mut stdin = child.stdin.take()?;
        Some(std::thread::spawn(move || {
            let _ = stdin.write_all(&body);
        }))
    });
    let mut stderr = child.stderr.take().ok_or("Missing curl stderr")?;
    let errors = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.by_ref().take(64 * 1024).read_to_end(&mut bytes);
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        bytes
    });

    let mut bytes = Vec::new();
    let limit = BODY_LIMIT + WRITE_OUT.len() + 128;
    let read = child
        .stdout
        .take()
        .ok_or("Missing curl stdout")?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes);
    let truncated = bytes.len() > limit;
    if truncated || read.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let errors = errors.join().unwrap_or_default();
    if let Some(feeder) = feeder {
        let _ = feeder.join();
    }
    read.map_err(|e| e.to_string())?;

    if !truncated && !status.success() {
        let text = String::from_utf8_lossy(&errors).trim().to_owned();
        return Err(if text.is_empty() {
            format!("curl exited with {status}")
        } else {
            text
        });
    }
    let mut response = parse(&bytes)?;
    response.truncated = truncated;
    Ok(response)
}

/// Splits curl's `--include` output into status, headers, body and the
/// `--write-out` trailer.
pub fn parse(raw: &[u8]) -> Result<Response, String> {
    // The trailer, if it arrived whole.
    let (raw, trailer) = split_trailer(raw);

    let mut rest = raw;
    let mut response = Response::default();
    loop {
        let (head, body) = split_head(rest).ok_or("curl returned no HTTP status line")?;
        let head = String::from_utf8_lossy(head);
        let mut lines = head.lines();
        let status_line = lines.next().unwrap_or_default();
        let mut parts = status_line.splitn(3, ' ');
        response.version = parts.next().unwrap_or_default().to_owned();
        response.status = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        response.reason = parts.next().unwrap_or_default().trim().to_owned();
        response.headers = lines
            .filter_map(super::header_line)
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        rest = body;
        // `100 Continue` and friends precede the real reply.
        if !(100..200).contains(&response.status) || rest.is_empty() {
            break;
        }
    }
    response.body = rest.to_vec();

    if let Some(trailer) = trailer {
        let mut fields = trailer.split('\u{1f}');
        fields.next(); // "caio"
        response.time_total = fields.next().and_then(|f| f.trim().parse().ok());
        response.time_first_byte = fields.next().and_then(|f| f.trim().parse().ok());
        response.remote_ip = fields
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
    }
    Ok(response)
}

fn split_trailer(raw: &[u8]) -> (&[u8], Option<String>) {
    let mut end = raw.len();
    if raw.ends_with(b"\x1e") {
        end -= 1;
    }
    let Some(start) = raw[..end].iter().rposition(|b| *b == 0x1e) else {
        return (raw, None);
    };
    let trailer = String::from_utf8_lossy(&raw[start + 1..end]).into_owned();
    if !trailer.starts_with("caio\u{1f}") {
        return (raw, None);
    }
    let body_end = raw[..start].strip_suffix(b"\n").map_or(start, |b| b.len());
    (&raw[..body_end], Some(trailer))
}

fn split_head(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    if !raw.starts_with(b"HTTP/") {
        return None;
    }
    if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
        return Some((&raw[..at], &raw[at + 4..]));
    }
    if let Some(at) = raw.windows(2).position(|w| w == b"\n\n") {
        return Some((&raw[..at], &raw[at + 2..]));
    }
    Some((raw, &[]))
}

/// Sends on a worker thread. The receiver yields the outcome.
pub fn spawn(request: Prepared) -> mpsc::Receiver<Result<Response, String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(send(&request));
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::view::*;

    fn prepared(method: &str, url: &str, body: Option<&str>) -> Prepared {
        Prepared {
            name: None,
            method: method.into(),
            url: url.into(),
            headers: vec![
                ("Accept".into(), "application/json".into()),
                ("X-Empty".into(), String::new()),
            ],
            body: body.map(|b| b.as_bytes().to_vec()),
        }
    }

    fn args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn builds_a_bounded_curl_invocation() {
        let cmd = command(&prepared("POST", "https://x.test/a?b=[1]", Some("{}")));
        assert_eq!(cmd.get_program(), CURL);
        let a = args(&cmd);
        assert!(a.contains(&"--max-time".to_owned()));
        assert!(
            a.contains(&"--globoff".to_owned()),
            "brackets in a URL are not a range"
        );
        assert!(a.windows(2).any(|w| w == ["--request", "POST"]));
        assert!(
            a.windows(2)
                .any(|w| w == ["--header", "Accept: application/json"])
        );
        assert!(
            a.windows(2).any(|w| w == ["--header", "X-Empty;"]),
            "an empty header is sent empty, not removed"
        );
        assert!(a.windows(2).any(|w| w == ["--data-binary", "@-"]));
        assert_eq!(a.last().map(String::as_str), Some("https://x.test/a?b=[1]"));
        assert!(
            a.windows(2).any(|w| w == ["--", "https://x.test/a?b=[1]"]),
            "a URL starting with a dash is still a URL"
        );
        let get = args(&command(&prepared("GET", "https://x.test", None)));
        assert!(!get.contains(&"--data-binary".to_owned()));
        let head = args(&command(&prepared("HEAD", "https://x.test", None)));
        assert!(head.contains(&"--head".to_owned()));
    }

    #[test]
    fn parses_headers_body_and_trailer() {
        let raw = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\nContent-Type: application/json\r\nX-Id: 7\r\n\r\n{\"ok\":true}\n\x1ecaio\x1f0.123456\x1f0.05\x1f93.184.216.34\x1e";
        let r = parse(raw).expect("parses");
        assert_eq!(r.status, 201);
        assert_eq!(r.reason, "Created");
        assert_eq!(r.version, "HTTP/1.1");
        assert_eq!(
            r.headers,
            [
                ("Content-Type".to_owned(), "application/json".to_owned()),
                ("X-Id".to_owned(), "7".to_owned())
            ]
        );
        assert_eq!(r.body, b"{\"ok\":true}");
        assert_eq!(r.time_total, Some(0.123456));
        assert_eq!(r.remote_ip.as_deref(), Some("93.184.216.34"));
    }

    #[test]
    fn a_body_without_trailer_is_kept_whole() {
        let r = parse(b"HTTP/1.1 204 No Content\r\n\r\n").expect("parses");
        assert_eq!(r.status, 204);
        assert!(r.body.is_empty());
        assert_eq!(r.time_total, None);
        assert!(parse(b"not http").is_err());
    }

    #[test]
    fn segments_show_body_headers_and_request() {
        let mut request = prepared("GET", "https://x.test/users", Some("{\"q\":1}"));
        request.headers.truncate(1);
        let response = Response {
            version: "HTTP/2".into(),
            status: 200,
            reason: String::new(),
            headers: vec![(
                "content-type".into(),
                "application/json; charset=utf-8".into(),
            )],
            body: b"{\"a\":[1,2]}".to_vec(),
            truncated: false,
            time_total: Some(0.0421),
            time_first_byte: Some(0.03),
            remote_ip: Some("1.2.3.4".into()),
        };
        let mut view = View::pending(request);
        assert_eq!(view.status(), "Sending…");
        assert_eq!(view.verdict(), Verdict::Pending);
        assert_eq!(view.text(Segment::Body), (String::new(), None));

        view.outcome = Some(Ok(response));
        assert_eq!(
            view.status(),
            "200 OK",
            "HTTP/2 carries no reason phrase; one is supplied"
        );
        assert_eq!(view.verdict(), Verdict::Success);
        assert_eq!(view.facts(), "42 ms · 11 bytes · HTTP/2 · 1.2.3.4");
        assert_eq!(view.header_count(), 1);
        assert_eq!(
            view.text(Segment::Body),
            (
                "{\n  \"a\": [\n    1,\n    2\n  ]\n}".to_owned(),
                Some("json")
            )
        );
        assert_eq!(
            view.text(Segment::Headers).0,
            "HTTP/2 200\ncontent-type: application/json; charset=utf-8\n"
        );
        assert_eq!(
            view.text(Segment::Request).0,
            "GET https://x.test/users\nAccept: application/json\n\n{\"q\":1}\n"
        );

        view.outcome = Some(Err("curl: (6) Could not resolve host: x.test".into()));
        assert_eq!(view.status(), "Failed");
        assert_eq!(view.verdict(), Verdict::Failed);
        assert_eq!(
            view.text(Segment::Body).0,
            "curl: (6) Could not resolve host: x.test"
        );
        assert_eq!(view.facts(), "");
    }

    #[test]
    fn verdicts_follow_status_classes() {
        let mut view = View::pending(prepared("GET", "https://x.test", None));
        for (status, verdict) in [
            (204, Verdict::Success),
            (302, Verdict::Redirect),
            (404, Verdict::ClientError),
            (503, Verdict::ServerError),
        ] {
            view.outcome = Some(Ok(Response {
                status,
                ..Response::default()
            }));
            assert_eq!(view.verdict(), verdict);
        }
        assert_eq!(view.status(), "503 Service Unavailable");
    }

    #[test]
    fn a_truncated_or_binary_body_says_so() {
        let cut = Response {
            status: 200,
            body: b"abc".to_vec(),
            truncated: true,
            ..Response::default()
        };
        assert!(body_text(&cut).0.contains("[body cut at 4 MB"));
        let binary = Response {
            status: 200,
            headers: vec![("Content-Type".into(), "image/png".into())],
            body: vec![0xff, 0xd8, 0x00],
            ..Response::default()
        };
        assert_eq!(
            body_text(&binary),
            ("Binary body, 3 bytes, image/png.".to_owned(), None)
        );
    }

    #[test]
    fn a_refused_connection_is_reported_not_hung() {
        // Port 1 on loopback is closed on any Mac; curl fails fast with a
        // real error message, which is what the tab must show.
        let request = prepared("GET", "http://127.0.0.1:1/", None);
        let error = send(&request).expect_err("nothing listens on port 1");
        assert!(error.contains("curl: (7)"), "{error}");
    }
}
