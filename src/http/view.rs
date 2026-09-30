//! A request and its reply as a response tab shows them: the three views
//! of the reply, the verdict the strip colours, and the text of each.

use super::Prepared;
use super::curl::{BODY_LIMIT, Response};
use super::json;
use crate::text::buffer::human_size;

/// Which of the three views of a reply a response tab shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Segment {
    #[default]
    Body,
    Headers,
    Request,
}

impl Segment {
    pub const ALL: [Segment; 3] = [Segment::Body, Segment::Headers, Segment::Request];

    pub fn label(self) -> &'static str {
        match self {
            Segment::Body => "Body",
            Segment::Headers => "Headers",
            Segment::Request => "Request",
        }
    }
}

/// How a status reads at a glance, which is what the strip colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pending,
    Success,
    Redirect,
    ClientError,
    ServerError,
    Failed,
}

/// The body as shown and the extension to highlight it as.
pub type Shown = (String, Option<&'static str>);

/// What the request worker hands back: the reply with its body already
/// worked out there, since pretty-printing up to 4 MB of JSON would stall
/// the frame that received it.
pub type Answer = Result<(Response, Shown), String>;

/// A request and what came back, behind one response tab.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub request: Prepared,
    /// `None` while the request is in flight.
    pub outcome: Option<Result<Response, String>>,
    pub segment: Segment,
    /// The body as shown, worked out once: pretty-printing a few megabytes
    /// of JSON is not a job for every switch between segments.
    body: std::cell::OnceCell<Shown>,
}

impl View {
    pub fn pending(request: Prepared) -> Self {
        View {
            request,
            outcome: None,
            segment: Segment::Body,
            body: std::cell::OnceCell::new(),
        }
    }

    /// The answer, replacing whatever was there.
    pub fn set_outcome(&mut self, answer: Answer) {
        self.body = std::cell::OnceCell::new();
        self.outcome = Some(answer.map(|(response, shown)| {
            let _ = self.body.set(shown);
            response
        }));
    }

    pub fn verdict(&self) -> Verdict {
        match &self.outcome {
            None => Verdict::Pending,
            Some(Err(_)) => Verdict::Failed,
            Some(Ok(r)) => match r.status {
                200..=299 => Verdict::Success,
                300..=399 => Verdict::Redirect,
                400..=499 => Verdict::ClientError,
                _ => Verdict::ServerError,
            },
        }
    }

    /// The status word: `200 OK`, `Sending…`, `Failed`.
    pub fn status(&self) -> String {
        match &self.outcome {
            None => "Sending…".into(),
            Some(Err(_)) => "Failed".into(),
            Some(Ok(r)) => {
                let reason = if r.reason.is_empty() {
                    reason_for(r.status)
                } else {
                    r.reason.as_str()
                };
                format!("{} {}", r.status, reason).trim().to_owned()
            }
        }
    }

    /// Time, size, protocol and peer, after the status.
    pub fn facts(&self) -> String {
        let Some(Ok(r)) = &self.outcome else {
            return String::new();
        };
        let mut facts = Vec::new();
        if let Some(t) = r.time_total {
            facts.push(millis(t));
        }
        facts.push(human_size(r.body.len() as u64));
        if !r.version.is_empty() {
            facts.push(r.version.clone());
        }
        if let Some(ip) = &r.remote_ip {
            facts.push(ip.clone());
        }
        facts.join(" · ")
    }

    /// How many lines the Headers segment has, for its tab label.
    pub fn header_count(&self) -> usize {
        match &self.outcome {
            Some(Ok(r)) => r.headers.len(),
            _ => 0,
        }
    }

    /// The text of `segment` and the extension it should be highlighted as.
    pub fn text(&self, segment: Segment) -> (String, Option<&'static str>) {
        match segment {
            Segment::Request => (request_text(&self.request), None),
            Segment::Headers => match &self.outcome {
                Some(Ok(r)) => (headers_text(r), None),
                Some(Err(e)) => (e.trim().to_owned(), None),
                None => (String::new(), None),
            },
            Segment::Body => match &self.outcome {
                Some(Ok(r)) => self.body.get_or_init(|| body_text(r)).clone(),
                Some(Err(e)) => (e.trim().to_owned(), None),
                None => (String::new(), None),
            },
        }
    }
}

fn reason_for(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

/// The body as shown: pretty-printed when it is JSON, a note when it is
/// binary, cut with a note when it went past the cap.
pub fn body_text(response: &Response) -> Shown {
    if response.body.is_empty() {
        return (String::new(), None);
    }
    let content_type = response
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.to_ascii_lowercase())
        .unwrap_or_default();
    let (mut text, ext) = match std::str::from_utf8(&response.body) {
        Ok(text) => {
            // Parsed once: the guess and the pretty print share it.
            let parsed = (content_type.contains("json") || starts_like_json(text))
                .then(|| json::parse(text).ok())
                .flatten();
            if let Some(value) = parsed {
                (json::pretty(&value), Some("json"))
            } else if content_type.contains("json") {
                (text.to_owned(), Some("json"))
            } else if content_type.contains("html") {
                (text.to_owned(), Some("html"))
            } else if content_type.contains("javascript") {
                (text.to_owned(), Some("js"))
            } else if content_type.contains("css") {
                (text.to_owned(), Some("css"))
            } else {
                (text.to_owned(), None)
            }
        }
        Err(_) => (
            format!(
                "Binary body, {}{}.",
                human_size(response.body.len() as u64),
                if content_type.is_empty() {
                    String::new()
                } else {
                    format!(", {content_type}")
                }
            ),
            None,
        ),
    };
    if response.truncated {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!(
            "\n[body cut at {}; the rest was not kept]\n",
            human_size(BODY_LIMIT as u64)
        ));
    }
    (text, ext)
}

/// Status line and headers, one per line, as they arrived.
pub fn headers_text(response: &Response) -> String {
    let mut out = format!(
        "{} {} {}\n",
        response.version, response.status, response.reason
    );
    out = out.trim_end().to_owned() + "\n";
    for (k, v) in &response.headers {
        out.push_str(&format!("{k}: {v}\n"));
    }
    out
}

/// What was sent, after variable expansion: the way to catch a wrong token.
pub fn request_text(request: &Prepared) -> String {
    let mut out = format!("{} {}\n", request.method, request.url);
    for (k, v) in &request.headers {
        out.push_str(&format!("{k}: {v}\n"));
    }
    if let Some(body) = &request.body {
        out.push('\n');
        match std::str::from_utf8(body) {
            Ok(text) => out.push_str(text),
            Err(_) => out.push_str(&format!("[binary body, {}]", human_size(body.len() as u64))),
        }
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Worth trying as JSON without a content type saying so.
fn starts_like_json(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with('{') || t.starts_with('[')
}

fn millis(seconds: f64) -> String {
    let ms = seconds * 1000.0;
    if ms >= 1000.0 {
        format!("{:.2} s", seconds)
    } else if ms >= 10.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{ms:.1} ms")
    }
}
