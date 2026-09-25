//! A small JSON reader and printer.
//!
//! Three jobs: reading `http-client.env.json`, pretty-printing response
//! bodies, and speaking JSON-RPC to language servers. None needs a
//! data-binding library. Numbers keep their source
//! text so a body is reprinted exactly, never rounded through a float, and
//! object keys keep file order so the first environment in a file is the
//! first one the user wrote.

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    /// The number as written, which is all a pretty-printer needs.
    Number(String),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    /// The member named `key` of an object, or `None` for anything else.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Number(n) => n.parse().ok(),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Number(n) => n
                .parse()
                .ok()
                .or_else(|| n.parse::<f64>().ok().map(|f| f as i64)),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    /// `get` along a dotted path: `"result.capabilities.completionProvider"`.
    pub fn path(&self, dotted: &str) -> Option<&Value> {
        dotted.split('.').try_fold(self, |v, key| v.get(key))
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// A scalar as the text a variable expands to. Containers do not expand.
    pub fn as_text(&self) -> Option<String> {
        match self {
            Value::Null => Some("null".into()),
            Value::Bool(b) => Some(b.to_string()),
            Value::Number(n) => Some(n.clone()),
            Value::String(s) => Some(s.clone()),
            Value::Array(_) | Value::Object(_) => None,
        }
    }
}

/// Parses one JSON document. Trailing whitespace is fine; anything else after
/// the value is an error, so a half-JSON body is not reprinted as if it were
/// whole.
pub fn parse(text: &str) -> Result<Value, String> {
    let mut p = Parser {
        bytes: text.as_bytes(),
        at: 0,
        depth: 0,
    };
    p.skip_ws();
    let value = p.value()?;
    p.skip_ws();
    if p.at != p.bytes.len() {
        return Err(p.error("unexpected text after the value"));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
    depth: usize,
}

impl Parser<'_> {
    fn error(&self, what: &str) -> String {
        let (line, column) =
            self.bytes[..self.at.min(self.bytes.len())]
                .iter()
                .fold(
                    (1, 1),
                    |(l, c), b| if *b == b'\n' { (l + 1, 1) } else { (l, c + 1) },
                );
        format!("{what} at line {line}, column {column}")
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.bytes.get(self.at)
            && matches!(b, b' ' | b'\t' | b'\n' | b'\r')
        {
            self.at += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn expect(&mut self, b: u8) -> Result<(), String> {
        if self.peek() == Some(b) {
            self.at += 1;
            Ok(())
        } else {
            Err(self.error(&format!("expected '{}'", b as char)))
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        match self.peek() {
            None => Err(self.error("unexpected end of input")),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string().map(Value::String),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'n') => self.literal("null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("unexpected character")),
        }
    }

    fn literal(&mut self, word: &str, value: Value) -> Result<Value, String> {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            Ok(value)
        } else {
            Err(self.error("unexpected character"))
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        let digits = |p: &mut Self| {
            let from = p.at;
            while matches!(p.peek(), Some(b'0'..=b'9')) {
                p.at += 1;
            }
            p.at > from
        };
        if !digits(self) {
            return Err(self.error("expected a digit"));
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            if !digits(self) {
                return Err(self.error("expected a digit after '.'"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if !digits(self) {
                return Err(self.error("expected an exponent"));
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.at]).map_err(|e| e.to_string())?;
        Ok(Value::Number(text.to_owned()))
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let Some(b) = self.peek() else {
                return Err(self.error("unterminated string"));
            };
            self.at += 1;
            match b {
                b'"' => return Ok(out),
                b'\\' => {
                    let Some(esc) = self.peek() else {
                        return Err(self.error("unterminated escape"));
                    };
                    self.at += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let mut code = self.hex4()?;
                            if (0xD800..0xDC00).contains(&code) {
                                // A high surrogate must be followed by a low one.
                                if self.bytes[self.at..].starts_with(b"\\u") {
                                    self.at += 2;
                                    let low = self.hex4()?;
                                    if (0xDC00..0xE000).contains(&low) {
                                        code = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                                    } else {
                                        return Err(self.error("invalid surrogate pair"));
                                    }
                                } else {
                                    return Err(self.error("lone surrogate"));
                                }
                            }
                            out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                        }
                        _ => return Err(self.error("invalid escape")),
                    }
                }
                b if b < 0x20 => return Err(self.error("control character in string")),
                _ => {
                    // Copy a whole UTF-8 sequence at once.
                    let start = self.at - 1;
                    let len = utf8_len(b);
                    let end = (start + len).min(self.bytes.len());
                    match std::str::from_utf8(&self.bytes[start..end]) {
                        Ok(s) => out.push_str(s),
                        Err(_) => return Err(self.error("invalid UTF-8")),
                    }
                    self.at = end;
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let end = self.at + 4;
        let Some(slice) = self.bytes.get(self.at..end) else {
            return Err(self.error("short \\u escape"));
        };
        let text = std::str::from_utf8(slice).map_err(|_| self.error("bad \\u escape"))?;
        let code = u32::from_str_radix(text, 16).map_err(|_| self.error("bad \\u escape"))?;
        self.at = end;
        Ok(code)
    }

    fn array(&mut self) -> Result<Value, String> {
        self.expect(b'[')?;
        self.enter()?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.at += 1;
            self.depth -= 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    self.depth -= 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }
    }

    fn object(&mut self) -> Result<Value, String> {
        self.expect(b'{')?;
        self.enter()?;
        let mut members = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.at += 1;
            self.depth -= 1;
            return Ok(Value::Object(members));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let value = self.value()?;
            members.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    self.depth -= 1;
                    return Ok(Value::Object(members));
                }
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
    }

    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > 256 {
            return Err(self.error("nesting too deep"));
        }
        Ok(())
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// Builds an object in source order: `object([("a", Value::Null)])`.
pub fn object<const N: usize>(members: [(&str, Value); N]) -> Value {
    Value::Object(
        members
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect(),
    )
}

pub fn string(s: &str) -> Value {
    Value::String(s.to_owned())
}

pub fn number(n: impl std::fmt::Display) -> Value {
    Value::Number(n.to_string())
}

/// One line, no spaces: the wire form.
pub fn compact(value: &Value) -> String {
    let mut out = String::new();
    write_compact(value, &mut out);
    out
}

fn write_compact(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(n),
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_compact(item, out);
            }
            out.push(']');
        }
        Value::Object(members) => {
            out.push('{');
            for (i, (key, item)) in members.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_compact(item, out);
            }
            out.push('}');
        }
    }
}

/// Reprints `value` with two-space indentation, one member per line, empty
/// containers kept on one line.
pub fn pretty(value: &Value) -> String {
    let mut out = String::new();
    write_pretty(value, 0, &mut out);
    out
}

fn write_pretty(value: &Value, depth: usize, out: &mut String) {
    let indent = |out: &mut String, depth: usize| {
        for _ in 0..depth {
            out.push_str("  ");
        }
    };
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(n),
        Value::String(s) => write_string(s, out),
        Value::Array(items) if items.is_empty() => out.push_str("[]"),
        Value::Array(items) => {
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                indent(out, depth + 1);
                write_pretty(item, depth + 1, out);
                out.push_str(if i + 1 < items.len() { ",\n" } else { "\n" });
            }
            indent(out, depth);
            out.push(']');
        }
        Value::Object(members) if members.is_empty() => out.push_str("{}"),
        Value::Object(members) => {
            out.push_str("{\n");
            for (i, (key, item)) in members.iter().enumerate() {
                indent(out, depth + 1);
                write_string(key, out);
                out.push_str(": ");
                write_pretty(item, depth + 1, out);
                out.push_str(if i + 1 < members.len() { ",\n" } else { "\n" });
            }
            indent(out, depth);
            out.push('}');
        }
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_documents_in_order() {
        let v = parse(r#"{"b": [1, 2.5e3, -0.1], "a": {"x": null, "y": true}, "s": "hi"}"#)
            .expect("valid JSON");
        let Value::Object(members) = &v else {
            panic!("expected an object");
        };
        let keys: Vec<&str> = members.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["b", "a", "s"], "object order must be file order");
        assert_eq!(v.get("s").and_then(Value::as_text).as_deref(), Some("hi"));
        assert_eq!(
            v.get("b"),
            Some(&Value::Array(vec![
                Value::Number("1".into()),
                Value::Number("2.5e3".into()),
                Value::Number("-0.1".into()),
            ]))
        );
    }

    #[test]
    fn decodes_escapes_and_surrogates() {
        let v = parse(r#""a\"b\\c\né😀""#).expect("valid");
        assert_eq!(v, Value::String("a\"b\\c\né😀".into()));
    }

    #[test]
    fn rejects_trailing_garbage_and_bad_input() {
        assert!(parse("{} x").is_err());
        assert!(parse("[1,]").is_err());
        assert!(parse("{\"a\" 1}").is_err());
        assert!(parse("\"unterminated").is_err());
        assert!(parse("tru").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn pretty_prints_two_space_indent_and_keeps_numbers() {
        let v = parse(r#"{"n":12345678901234567890.100,"e":[],"o":{},"l":[{"k":"v\"q"}]}"#)
            .expect("valid");
        assert_eq!(
            pretty(&v),
            "{\n  \"n\": 12345678901234567890.100,\n  \"e\": [],\n  \"o\": {},\n  \"l\": [\n    {\n      \"k\": \"v\\\"q\"\n    }\n  ]\n}"
        );
    }

    #[test]
    fn compact_round_trips_and_paths_walk_objects() {
        let v = object([
            ("jsonrpc", string("2.0")),
            ("id", number(7)),
            (
                "result",
                object([("items", Value::Array(vec![number(1), Value::Null]))]),
            ),
        ]);
        let wire = compact(&v);
        assert_eq!(
            wire,
            r#"{"jsonrpc":"2.0","id":7,"result":{"items":[1,null]}}"#
        );
        assert_eq!(parse(&wire).unwrap(), v);
        assert_eq!(
            v.path("result.items")
                .and_then(Value::as_array)
                .map(<[Value]>::len),
            Some(2)
        );
        assert_eq!(v.path("id").and_then(Value::as_u64), Some(7));
        assert!(v.path("result.missing").is_none());
    }

    #[test]
    fn deep_nesting_is_refused_rather_than_overflowing() {
        let deep = "[".repeat(1000) + &"]".repeat(1000);
        assert!(parse(&deep).is_err());
    }
}
