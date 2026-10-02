//! A call written as a document: a small JSON object naming the server
//! and the tool, resource or prompt, with its arguments.
//!
//! ```json
//! {
//!   "server": "notes",
//!   "tool": "search",
//!   "arguments": { "query": "tomatoes", "limit": 10 }
//! }
//! ```
//!
//! Clicking a tool opens one with every argument its schema names filled
//! with a placeholder, and `about` saying what the tool does. Cmd-Return
//! runs it; the answer opens in a tab of its own. Saved to a file, a call
//! runs again the same way, so a useful call is kept by saving it.

use crate::json::{self, Value, object, string};

use super::{Prompt, Tool};

/// What a call document asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Tool(String),
    Resource(String),
    Prompt(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub server: String,
    pub target: Target,
    pub arguments: Value,
}

impl Call {
    /// A short name for the answer's tab: `notes · search`.
    pub fn title(&self) -> String {
        let what = match &self.target {
            Target::Tool(name) | Target::Prompt(name) => name.as_str(),
            Target::Resource(uri) => uri.as_str(),
        };
        format!("{} \u{b7} {what}", self.server)
    }
}

/// Whether `text` reads as a call document, cheaply, before parsing it.
pub fn looks_like_call(text: &str) -> bool {
    let head: String = text.chars().take(400).collect();
    head.trim_start().starts_with('{')
        && head.contains("\"server\"")
        && ["\"tool\"", "\"resource\"", "\"prompt\""]
            .iter()
            .any(|k| head.contains(k))
}

pub fn parse(text: &str) -> Result<Call, String> {
    let value = json::parse(text).map_err(|e| format!("the call is not valid JSON: {e}"))?;
    let field = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let server = field("server").ok_or("the call names no \"server\"")?;
    let target = if let Some(tool) = field("tool") {
        Target::Tool(tool)
    } else if let Some(uri) = field("resource") {
        Target::Resource(uri)
    } else if let Some(prompt) = field("prompt") {
        Target::Prompt(prompt)
    } else {
        return Err("the call names no \"tool\", \"resource\" or \"prompt\"".into());
    };
    let arguments = match value.get("arguments") {
        None | Some(Value::Null) => Value::Object(Vec::new()),
        Some(args @ Value::Object(_)) => args.clone(),
        Some(_) => return Err("\"arguments\" has to be an object".into()),
    };
    Ok(Call {
        server,
        target,
        arguments,
    })
}

/// A placeholder for a value the schema describes: its default, its first
/// allowed value, or an empty one of its type.
fn placeholder(schema: &Value, depth: usize) -> Value {
    if let Some(default) = schema.get("default") {
        return default.clone();
    }
    if let Some(first) = schema
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|v| v.first())
    {
        return first.clone();
    }
    if let Some(constant) = schema.get("const") {
        return constant.clone();
    }
    let kind = match schema.get("type") {
        Some(Value::String(t)) => t.as_str(),
        // ["string", "null"]: the first that is not null.
        Some(Value::Array(types)) => types
            .iter()
            .filter_map(Value::as_str)
            .find(|t| *t != "null")
            .unwrap_or("null"),
        _ => "",
    };
    match kind {
        "string" => string(""),
        "integer" | "number" => json::number(0),
        "boolean" => Value::Bool(false),
        "array" => Value::Array(Vec::new()),
        "object" if depth < 3 => properties(schema, depth + 1),
        "object" => Value::Object(Vec::new()),
        _ => Value::Null,
    }
}

/// An object with a placeholder for every property, required ones first.
fn properties(schema: &Value, depth: usize) -> Value {
    let Some(Value::Object(props)) = schema.get("properties") else {
        return Value::Object(Vec::new());
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let mut members: Vec<(String, Value)> = props
        .iter()
        .map(|(name, prop)| (name.clone(), placeholder(prop, depth)))
        .collect();
    members.sort_by_key(|(name, _)| !required.contains(&name.as_str()));
    Value::Object(members)
}

/// What the call document says about the tool, in one line.
fn about_tool(tool: &Tool) -> String {
    let required: Vec<&str> = tool
        .input_schema
        .get("required")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let mut about = tool
        .description
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if !required.is_empty() {
        about.push_str(&format!(" Required: {}.", required.join(", ")));
    }
    if tool.read_only {
        about.push_str(" Changes nothing.");
    } else if tool.destructive {
        about.push_str(" May change or delete things; crc asks before running it.");
    }
    about.trim().to_owned()
}

pub fn tool_document(server: &str, tool: &Tool) -> String {
    json::pretty(&object([
        ("server", string(server)),
        ("tool", string(&tool.name)),
        ("about", string(&about_tool(tool))),
        ("arguments", properties(&tool.input_schema, 0)),
    ]))
}

pub fn resource_document(server: &str, uri: &str) -> String {
    json::pretty(&object([
        ("server", string(server)),
        ("resource", string(uri)),
    ]))
}

pub fn prompt_document(server: &str, prompt: &Prompt) -> String {
    let arguments = Value::Object(
        prompt
            .arguments
            .iter()
            .map(|(name, _)| (name.clone(), string("")))
            .collect(),
    );
    json::pretty(&object([
        ("server", string(server)),
        ("prompt", string(&prompt.name)),
        ("about", string(&prompt.description)),
        ("arguments", arguments),
    ]))
}

/// Text as shown: pretty when it is JSON, as it came otherwise.
fn shown(text: &str) -> String {
    let trimmed = text.trim();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && let Ok(value) = json::parse(trimmed)
    {
        return json::pretty(&value);
    }
    text.to_owned()
}

/// One content block of a tool result or prompt message, as text.
fn block(content: &Value) -> String {
    let kind = content.get("type").and_then(Value::as_str).unwrap_or("");
    let text = |key: &str| content.get(key).and_then(Value::as_str).unwrap_or("");
    match kind {
        "text" => shown(text("text")),
        "image" | "audio" => format!(
            "[{kind}, {}, {} bytes of base64 not shown]",
            text("mimeType"),
            text("data").len()
        ),
        "resource_link" => format!("[link] {} {}", text("name"), text("uri")),
        "resource" => match content.get("resource") {
            Some(resource) => contents(resource),
            None => String::new(),
        },
        _ => json::pretty(content),
    }
}

/// A resource's contents: its text, or a note for binary data.
fn contents(resource: &Value) -> String {
    let uri = resource.get("uri").and_then(Value::as_str).unwrap_or("");
    match resource.get("text").and_then(Value::as_str) {
        Some(text) => shown(text),
        None => format!(
            "[{uri}: {} bytes of base64 not shown]",
            resource
                .get("blob")
                .and_then(Value::as_str)
                .map_or(0, str::len)
        ),
    }
}

/// A result as the text of its answer tab, and whether it is an error.
pub fn render(method: &str, result: &Value) -> (String, bool) {
    let mut parts: Vec<String> = Vec::new();
    let mut error = false;
    match method {
        "tools/call" => {
            error = result
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            for content in result
                .get("content")
                .and_then(Value::as_array)
                .unwrap_or_default()
            {
                parts.push(block(content));
            }
            if let Some(structured) = result.get("structuredContent")
                && parts.is_empty()
            {
                parts.push(json::pretty(structured));
            }
        }
        "resources/read" => {
            for resource in result
                .get("contents")
                .and_then(Value::as_array)
                .unwrap_or_default()
            {
                parts.push(contents(resource));
            }
        }
        "prompts/get" => {
            for message in result
                .get("messages")
                .and_then(Value::as_array)
                .unwrap_or_default()
            {
                let role = message.get("role").and_then(Value::as_str).unwrap_or("?");
                let body = message.get("content").map(block).unwrap_or_default();
                parts.push(format!("{role}:\n{body}"));
            }
        }
        _ => parts.push(json::pretty(result)),
    }
    if parts.is_empty() {
        parts.push("(no content)".into());
    }
    let mut text = parts.join("\n\n");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    (text, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(schema: &str) -> Tool {
        Tool {
            name: "search".into(),
            title: None,
            description: "Search the\n  garden notes.".into(),
            input_schema: json::parse(schema).unwrap(),
            read_only: true,
            destructive: false,
        }
    }

    #[test]
    fn a_tool_document_has_a_placeholder_for_each_argument() {
        let t = tool(
            r#"{"type": "object", "required": ["query"], "properties": {
                "limit": {"type": "integer", "default": 10},
                "query": {"type": "string"},
                "order": {"enum": ["new", "old"]},
                "tags": {"type": "array"},
                "exact": {"type": ["boolean", "null"]},
                "range": {"type": "object", "properties": {"from": {"type": "string"}}}
            }}"#,
        );
        let doc = tool_document("notes", &t);
        let call = parse(&doc).unwrap();
        assert_eq!(call.server, "notes");
        assert_eq!(call.target, Target::Tool("search".into()));
        assert_eq!(
            json::compact(&call.arguments),
            r#"{"query":"","limit":10,"order":"new","tags":[],"exact":false,"range":{"from":""}}"#
        );
        let about = json::parse(&doc).unwrap();
        assert_eq!(
            about.get("about").and_then(Value::as_str),
            Some("Search the garden notes. Required: query. Changes nothing.")
        );
        assert!(looks_like_call(&doc));
        assert!(!looks_like_call("{\"name\": \"package\"}"));
    }

    #[test]
    fn a_call_names_its_server_and_target() {
        assert_eq!(
            parse(r#"{"server": "s", "resource": "file:///a"}"#)
                .unwrap()
                .target,
            Target::Resource("file:///a".into())
        );
        assert!(parse(r#"{"tool": "x"}"#).unwrap_err().contains("server"));
        assert!(parse(r#"{"server": "s"}"#).unwrap_err().contains("tool"));
        assert!(parse(r#"{"server": "s", "tool": "t", "arguments": []}"#).is_err());
        assert!(parse("{").is_err());
    }

    #[test]
    fn results_read_as_text() {
        let result = json::parse(
            r#"{"content": [
                {"type": "text", "text": "{\"plants\":2}"},
                {"type": "image", "mimeType": "image/png", "data": "AAAA"},
                {"type": "resource_link", "name": "beds", "uri": "file:///beds.md"}
            ], "isError": false}"#,
        )
        .unwrap();
        let (text, error) = render("tools/call", &result);
        assert!(!error);
        assert_eq!(
            text,
            "{\n  \"plants\": 2\n}\n\n[image, image/png, 4 bytes of base64 not shown]\n\n[link] beds file:///beds.md\n"
        );
        let failed = json::parse(
            r#"{"content": [{"type": "text", "text": "no such bed"}], "isError": true}"#,
        )
        .unwrap();
        assert_eq!(
            render("tools/call", &failed),
            ("no such bed\n".into(), true)
        );
        let read = json::parse(r#"{"contents": [{"uri": "a", "text": "hello"}]}"#).unwrap();
        assert_eq!(render("resources/read", &read).0, "hello\n");
        let prompt = json::parse(
            r#"{"messages": [{"role": "user", "content": {"type": "text", "text": "Plan the beds"}}]}"#,
        )
        .unwrap();
        assert_eq!(render("prompts/get", &prompt).0, "user:\nPlan the beds\n");
    }
}
