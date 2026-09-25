//! HTTP requests written as text, sent through the system `curl`.
//!
//! The format is the one JetBrains' HTTP client and the VS Code REST Client
//! read, so a request file works in three editors and a Postman or Insomnia
//! export converts to it:
//!
//! ```http
//! @host = https://api.example.com
//!
//! ### List users
//! GET {{host}}/users
//! Authorization: Bearer {{token}}
//!
//! ### Create one
//! # @name create
//! POST {{host}}/users
//! Content-Type: application/json
//!
//! {"name": "Ada"}
//! ```
//!
//! Blocks are separated by lines starting with `###`. Inside a block: comment
//! lines (`#`, `//`), the request line, headers until a blank line, then the
//! body, which may instead be `< ./file.json`. `@name = value` lines outside
//! a request define variables; the rest come from `http-client.env.json` and
//! `http-client.private.env.json` beside the file (or in a parent folder),
//! keyed by environment. `# @env prod` picks the environment; otherwise the
//! first one in the file is used.
//!
//! The editor itself has no network code, by roadmap rule. `curl` ships with
//! macOS and is driven the way `git` is: one subprocess, a size cap, a
//! timeout, and its stderr shown verbatim when it fails.

pub mod curl;
pub use crate::json;

use std::ops::Range;
use std::path::{Path, PathBuf};

/// File extensions the request format is recognised under.
pub fn is_request_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "http" | "rest"))
}

/// One request as written, before variables are expanded.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// `# @name` or the text after `###`, when either is given.
    pub name: Option<String>,
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Body>,
    /// The byte range of the block in the file, so the request under the
    /// caret can be found.
    pub span: Range<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Inline(String),
    /// `< path`, relative to the request file.
    File(String),
}

/// Everything found in a request file.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct File {
    pub variables: Vec<(String, String)>,
    /// `# @env name`, when the file names one.
    pub environment: Option<String>,
    pub requests: Vec<Request>,
}

/// Parses a request file. Never fails: a block without a request line is
/// skipped, so a comment-only block or a stray `###` costs nothing.
pub fn parse(text: &str) -> File {
    let mut file = File::default();
    // A block with no request line (the variable prologue, a note) is folded
    // into the request that follows, so a caret anywhere before the first
    // request still sends it.
    let mut block_start = 0;
    let mut block_name: Option<String> = None;
    let mut lines: Vec<(usize, &str)> = Vec::new();

    let mut offset = 0;
    for raw in text.split_inclusive('\n') {
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(rest) = line.strip_prefix("###") {
            if let Some(request) =
                parse_block(&mut file, &lines, block_name.take(), block_start..offset)
            {
                file.requests.push(request);
                block_start = offset;
            }
            lines.clear();
            let title = rest.trim();
            block_name = (!title.is_empty()).then(|| title.to_owned());
        } else {
            lines.push((offset, line));
        }
        offset += raw.len();
    }
    if let Some(request) = parse_block(
        &mut file,
        &lines,
        block_name.take(),
        block_start..text.len(),
    ) {
        file.requests.push(request);
    }
    file
}

fn parse_block(
    file: &mut File,
    lines: &[(usize, &str)],
    mut name: Option<String>,
    span: Range<usize>,
) -> Option<Request> {
    let mut i = 0;
    // Everything before the request line: comments, directives, variables.
    let mut request_line: Option<String> = None;
    while i < lines.len() {
        let line = lines[i].1;
        let trimmed = line.trim();
        i += 1;
        if trimmed.is_empty() {
            continue;
        }
        if let Some(comment) = trimmed
            .strip_prefix('#')
            .or_else(|| trimmed.strip_prefix("//"))
        {
            let comment = comment.trim();
            if let Some(n) = comment.strip_prefix("@name") {
                let n = n.trim().trim_start_matches('=').trim();
                if !n.is_empty() {
                    name = Some(n.to_owned());
                }
            } else if let Some(env) = comment.strip_prefix("@env") {
                let env = env.trim().trim_start_matches('=').trim();
                if !env.is_empty() {
                    file.environment = Some(env.to_owned());
                }
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('@')
            && let Some((key, value)) = rest.split_once('=')
            && !key.trim().is_empty()
            && key
                .trim()
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
        {
            file.variables
                .push((key.trim().to_owned(), value.trim().to_owned()));
            continue;
        }
        request_line = Some(trimmed.to_owned());
        break;
    }
    let mut request_line = request_line?;

    // A query string may continue on indented lines starting with ? or &.
    while i < lines.len() {
        let line = lines[i].1;
        if line.starts_with([' ', '\t'])
            && matches!(line.trim_start().chars().next(), Some('?' | '&'))
        {
            request_line.push_str(line.trim());
            i += 1;
        } else {
            break;
        }
    }

    let mut parts = request_line.split_whitespace();
    let first = parts.next()?;
    let (method, url) = if is_method(first) {
        (first.to_ascii_uppercase(), parts.next()?.to_owned())
    } else {
        ("GET".to_owned(), first.to_owned())
    };
    // A trailing `HTTP/1.1` is part of the format and carries nothing here.

    let mut headers = Vec::new();
    while i < lines.len() {
        let line = lines[i].1;
        i += 1;
        if line.trim().is_empty() {
            break;
        }
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.starts_with("//") {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            headers.push((k.trim().to_owned(), v.trim().to_owned()));
        }
    }

    // The body is what remains, minus response handlers and redirects
    // (`> {% ... %}`, `>> file`), which are part of the format but not of
    // the request.
    let mut body_lines: Vec<&str> = Vec::new();
    let mut in_handler = false;
    while i < lines.len() {
        let line = lines[i].1;
        i += 1;
        let trimmed = line.trim_start();
        if in_handler {
            if trimmed.contains("%}") {
                in_handler = false;
            }
            continue;
        }
        if trimmed.starts_with("> {%") {
            in_handler = !trimmed.contains("%}");
            continue;
        }
        if trimmed.starts_with(">>") || trimmed.starts_with("> ") {
            continue;
        }
        body_lines.push(line);
    }
    while body_lines.last().is_some_and(|l| l.trim().is_empty()) {
        body_lines.pop();
    }
    let body = if body_lines.is_empty() {
        None
    } else if body_lines.len() == 1
        && body_lines[0].trim().starts_with('<')
        && !body_lines[0].trim().starts_with("<>")
    {
        let path = body_lines[0].trim()[1..].trim();
        // `<@ path` means "expand variables in the file"; treated the same.
        let path = path.strip_prefix('@').map_or(path, str::trim);
        Some(Body::File(path.to_owned()))
    } else {
        Some(Body::Inline(body_lines.join("\n")))
    };

    Some(Request {
        name,
        method,
        url,
        headers,
        body,
        span,
    })
}

fn is_method(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS" | "TRACE" | "CONNECT"
    )
}

impl File {
    /// The request whose block contains byte `offset`.
    pub fn request_at(&self, offset: usize) -> Option<&Request> {
        self.requests
            .iter()
            .find(|r| r.span.contains(&offset))
            .or_else(|| self.requests.last().filter(|r| offset >= r.span.end))
    }
}

/// A request with every variable expanded and its body read: what `curl`
/// is handed.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub name: Option<String>,
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

impl Prepared {
    /// The tab title: `GET /users` for a URL with a path, the host otherwise.
    pub fn title(&self) -> String {
        if let Some(name) = &self.name {
            return format!("{} {}", self.method, name);
        }
        let rest = self
            .url
            .split_once("://")
            .map_or(self.url.as_str(), |(_, rest)| rest);
        let path = rest.find('/').map_or("", |i| &rest[i..]);
        let shown = if path.is_empty() || path == "/" {
            rest
        } else {
            path
        };
        let shown = shown.split(['?', '#']).next().unwrap_or(shown);
        format!("{} {}", self.method, shown)
    }
}

/// Resolves the request under `offset` in `text`, a file at `path`: variables
/// from the file and its environment, and a `< file` body read from disk.
pub fn prepare(text: &str, offset: usize, path: Option<&Path>) -> Result<Prepared, String> {
    let file = parse(text);
    let request = file
        .request_at(offset)
        .ok_or("No request under the caret. A request starts with a method and a URL, for example `GET https://example.com`.")?;
    let dir = path.and_then(Path::parent);
    let env = dir
        .map(|d| load_environment(d, file.environment.as_deref()))
        .unwrap_or_default();
    let expand = |s: &str| expand(s, &file.variables, &env);

    let url = expand(&request.url)?;
    let mut headers = Vec::with_capacity(request.headers.len());
    for (k, v) in &request.headers {
        headers.push((k.clone(), expand(v)?));
    }
    let body = match &request.body {
        None => None,
        Some(Body::Inline(text)) => Some(expand(text)?.into_bytes()),
        Some(Body::File(relative)) => {
            let relative = expand(relative)?;
            let full = dir.map_or_else(|| PathBuf::from(&relative), |d| d.join(&relative));
            Some(
                std::fs::read(&full)
                    .map_err(|e| format!("Could not read body file {}: {e}", full.display()))?,
            )
        }
    };
    Ok(Prepared {
        name: request.name.clone(),
        method: request.method.clone(),
        url,
        headers,
        body,
    })
}

/// Expands `{{name}}` against file variables first, then the environment.
/// An unknown name is an error, not an empty string: a request sent to
/// `https:///users` because `host` was misspelt helps nobody.
pub fn expand(
    text: &str,
    variables: &[(String, String)],
    env: &[(String, String)],
) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let name = after[..end].trim();
        let value = if let Some(builtin) = builtin(name) {
            builtin
        } else if let Some((_, v)) = variables.iter().rev().find(|(k, _)| k == name) {
            // A file variable may itself use the environment.
            expand(v, &[], env)?
        } else if let Some((_, v)) = env.iter().find(|(k, _)| k == name) {
            v.clone()
        } else {
            return Err(format!(
                "Undefined variable {{{{{name}}}}}. Define it with `@{name} = value` in the file or in http-client.env.json."
            ));
        };
        out.push_str(&value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn builtin(name: &str) -> Option<String> {
    match name {
        "$timestamp" => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs().to_string()),
        "$uuid" | "$guid" => Some(pseudo_uuid()),
        _ => None,
    }
}

/// A version-4-shaped identifier from the clock and the address of a fresh
/// allocation. Not cryptographic, and not claimed to be: it is a request
/// correlation id.
fn pseudo_uuid() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let salt = Box::new(nanos);
    let address = std::ptr::from_ref(&*salt) as usize as u128;
    let mixed = nanos ^ (address << 17) ^ ((std::process::id() as u128) << 96);
    let h = format!("{mixed:032x}");
    format!(
        "{}-{}-4{}-a{}-{}",
        &h[..8],
        &h[8..12],
        &h[13..16],
        &h[17..20],
        &h[20..32]
    )
}

/// Reads `http-client.env.json` and `http-client.private.env.json` from
/// `dir` or the nearest parent that has either, and returns the variables
/// of one environment: `named`, else the first in the file. The private
/// file wins on a conflict, which is how a token stays out of the committed
/// one.
pub fn load_environment(dir: &Path, named: Option<&str>) -> Vec<(String, String)> {
    let mut vars: Vec<(String, String)> = Vec::new();
    let Some(dir) = dir.ancestors().find(|d| {
        d.join("http-client.env.json").is_file() || d.join("http-client.private.env.json").is_file()
    }) else {
        return vars;
    };
    let mut chosen = named.map(str::to_owned);
    for file in ["http-client.env.json", "http-client.private.env.json"] {
        let Ok(text) = std::fs::read_to_string(dir.join(file)) else {
            continue;
        };
        let Ok(json::Value::Object(envs)) = json::parse(&text) else {
            continue;
        };
        if chosen.is_none() {
            chosen = envs.first().map(|(k, _)| k.clone());
        }
        let Some(name) = &chosen else {
            continue;
        };
        let Some(json::Value::Object(members)) =
            envs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
        else {
            continue;
        };
        for (k, v) in members {
            let Some(v) = v.as_text() else {
                continue;
            };
            match vars.iter_mut().find(|(key, _)| key == k) {
                Some(slot) => slot.1 = v,
                None => vars.push((k.clone(), v)),
            }
        }
    }
    vars
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "@host = https://api.example.com\n\n### List users\nGET {{host}}/users?page=1\n    &limit=20\nAccept: application/json\n\n### \n# @name create\nPOST {{host}}/users HTTP/1.1\nContent-Type: application/json\n\n{\"name\": \"Ada\"}\n\n\n###\nhttps://example.com/plain\n\n### From file\nPUT {{host}}/upload\n\n< ./payload.json\n";

    #[test]
    fn parses_blocks_variables_and_bodies() {
        let file = parse(SAMPLE);
        assert_eq!(
            file.variables,
            [("host".to_owned(), "https://api.example.com".to_owned())]
        );
        assert_eq!(file.requests.len(), 4);
        let list = &file.requests[0];
        assert_eq!(list.name.as_deref(), Some("List users"));
        assert_eq!(list.method, "GET");
        assert_eq!(
            list.url, "{{host}}/users?page=1&limit=20",
            "query continuation lines join the URL"
        );
        assert_eq!(
            list.headers,
            [("Accept".to_owned(), "application/json".to_owned())]
        );
        assert_eq!(list.body, None);

        let create = &file.requests[1];
        assert_eq!(create.name.as_deref(), Some("create"));
        assert_eq!(create.method, "POST");
        assert_eq!(create.url, "{{host}}/users");
        assert_eq!(
            create.body,
            Some(Body::Inline("{\"name\": \"Ada\"}".into())),
            "trailing blank lines drop"
        );

        let plain = &file.requests[2];
        assert_eq!(plain.method, "GET", "a bare URL is a GET");
        assert_eq!(plain.url, "https://example.com/plain");

        assert_eq!(
            file.requests[3].body,
            Some(Body::File("./payload.json".into()))
        );
    }

    #[test]
    fn request_at_finds_the_block_under_the_caret() {
        let file = parse(SAMPLE);
        let post = SAMPLE.find("POST").unwrap();
        assert_eq!(
            file.request_at(post).map(|r| r.method.as_str()),
            Some("POST")
        );
        assert_eq!(
            file.request_at(0).map(|r| r.url.as_str()),
            Some("{{host}}/users?page=1&limit=20"),
            "the variable prologue belongs to the first block"
        );
        assert_eq!(
            file.request_at(SAMPLE.len()).map(|r| r.method.as_str()),
            Some("PUT"),
            "end of file is the last block"
        );
    }

    #[test]
    fn a_file_without_a_request_line_yields_nothing() {
        assert!(parse("# just notes\n\n@x = 1\n").requests.is_empty());
        assert!(parse("").requests.is_empty());
    }

    #[test]
    fn handlers_and_redirects_are_not_body() {
        let file = parse(
            "POST https://x.test/a\n\n{\"a\":1}\n\n> {%\n  client.test(\"ok\", () => {});\n%}\n>> ./out.json\n",
        );
        assert_eq!(
            file.requests[0].body,
            Some(Body::Inline("{\"a\":1}".into()))
        );
    }

    #[test]
    fn expands_variables_in_order_and_refuses_unknown_ones() {
        let vars = vec![("host".to_owned(), "https://{{domain}}".to_owned())];
        let env = vec![
            ("domain".to_owned(), "api.test".to_owned()),
            ("token".to_owned(), "t0k".to_owned()),
        ];
        assert_eq!(
            expand("{{host}}/x?t={{ token }}", &vars, &env).unwrap(),
            "https://api.test/x?t=t0k"
        );
        let error = expand("{{nope}}", &vars, &env).unwrap_err();
        assert!(error.contains("Undefined variable {{nope}}"), "{error}");
        assert_eq!(expand("{{unclosed", &vars, &env).unwrap(), "{{unclosed");
        assert!(expand("{{$uuid}}", &[], &[]).unwrap().len() == 36);
    }

    #[test]
    fn environment_files_merge_with_private_winning() {
        let dir = std::env::temp_dir().join(format!("caio-http-env-{}", std::process::id()));
        let nested = dir.join("requests");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            dir.join("http-client.env.json"),
            r#"{"dev": {"host": "http://localhost:3000", "token": "public"}, "prod": {"host": "https://api.test", "port": 443}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("http-client.private.env.json"),
            r#"{"dev": {"token": "secret"}}"#,
        )
        .unwrap();

        let dev = load_environment(&nested, None);
        assert_eq!(
            dev,
            [
                ("host".to_owned(), "http://localhost:3000".to_owned()),
                ("token".to_owned(), "secret".to_owned())
            ],
            "first environment by default, found from a child folder, private token wins"
        );
        let prod = load_environment(&nested, Some("prod"));
        assert_eq!(
            prod,
            [
                ("host".to_owned(), "https://api.test".to_owned()),
                ("port".to_owned(), "443".to_owned())
            ]
        );
        assert!(load_environment(&nested, Some("missing")).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prepare_resolves_the_request_under_the_caret() {
        let dir = std::env::temp_dir().join(format!("caio-http-prep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("payload.json"), b"{\"file\":true}").unwrap();
        std::fs::write(
            dir.join("http-client.env.json"),
            r#"{"local": {"host": "http://127.0.0.1:1"}}"#,
        )
        .unwrap();
        let path = dir.join("api.http");
        let text = "# @env local\nGET {{host}}/a\n\n###\nPUT {{host}}/b\nX-Id: {{$timestamp}}\n\n< ./payload.json\n";
        let put = prepare(text, text.find("PUT").unwrap(), Some(&path)).unwrap();
        assert_eq!(put.url, "http://127.0.0.1:1/b");
        assert_eq!(put.body.as_deref(), Some(&b"{\"file\":true}"[..]));
        assert!(put.headers[0].1.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(put.title(), "PUT /b");
        let get = prepare(text, 0, Some(&path)).unwrap();
        assert_eq!(get.title(), "GET /a");
        assert!(prepare("# nothing here\n", 0, Some(&path)).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn titles_prefer_names_then_paths_then_hosts() {
        let mut p = Prepared {
            name: None,
            method: "GET".into(),
            url: "https://example.com".into(),
            headers: vec![],
            body: None,
        };
        assert_eq!(p.title(), "GET example.com");
        p.url = "https://example.com/v1/users?x=1".into();
        assert_eq!(p.title(), "GET /v1/users");
        p.name = Some("List users".into());
        assert_eq!(p.title(), "GET List users");
    }
}
