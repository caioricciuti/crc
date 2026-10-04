//! `~/.config/crc/mcp.json`: the MCP servers crc may start.
//!
//! The shape is the one Claude Code's `.mcp.json` uses, so an entry can be
//! copied across as it is:
//!
//! ```json
//! { "mcpServers": { "notes": { "command": "/usr/local/bin/notes-mcp", "args": ["--root", "~/notes"] } } }
//! ```
//!
//! Nothing in this file starts anything. A server starts when it is
//! clicked, and a command that downloads code to run it (`npx`, `bunx`,
//! `uvx`, `pnpm dlx` and the like) is refused with the reason: point the
//! entry at an installed, reviewed binary instead.

use std::path::{Path, PathBuf};

use crate::json::Value;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    /// A Streamable HTTP server's address, used when there is no command.
    pub url: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    /// Servers by name, in file order.
    pub servers: Vec<(String, ServerConfig)>,
    /// Why the file could not be read, when it could not.
    pub error: Option<String>,
}

/// Where the file is: beside `config.toml`.
pub fn path() -> Option<PathBuf> {
    crate::platform::settings::Settings::path().and_then(|p| p.parent().map(|d| d.join("mcp.json")))
}

/// What a new `mcp.json` holds: the one key, empty.
pub const TEMPLATE: &str = "{\n  \"mcpServers\": {\n  }\n}\n";

/// Adds `entry` under `name`, writing the file first when there is none.
/// An entry of that name is replaced; everything else in the file stays
/// as written. Answers the file's path.
pub fn add(name: &str, entry: &ServerConfig) -> Result<PathBuf, String> {
    let Some(path) = path() else {
        return Err("no configuration folder for mcp.json".into());
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => TEMPLATE.to_owned(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let text = added(&text, name, entry)?;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    crate::platform::write_atomically(&path, text.as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// `text` with `entry` under `name` in `mcpServers`, pretty-printed.
pub fn added(text: &str, name: &str, entry: &ServerConfig) -> Result<String, String> {
    let mut root = if text.trim().is_empty() {
        Value::Object(Vec::new())
    } else {
        crate::json::parse(text).map_err(|e| format!("mcp.json is not valid JSON: {e}"))?
    };
    let Value::Object(members) = &mut root else {
        return Err("mcp.json is not a JSON object".into());
    };
    if !members.iter().any(|(k, _)| k == "mcpServers") {
        members.push(("mcpServers".into(), Value::Object(Vec::new())));
    }
    let servers = members
        .iter_mut()
        .find(|(k, _)| k == "mcpServers")
        .map(|(_, v)| v)
        .expect("just made sure it is there");
    if !matches!(servers, Value::Object(_)) {
        *servers = Value::Object(Vec::new());
    }
    let Value::Object(list) = servers else {
        unreachable!("made an object above");
    };
    let json = entry.to_json();
    match list.iter_mut().find(|(k, _)| k == name) {
        Some((_, v)) => *v = json,
        None => list.push((name.to_owned(), json)),
    }
    let mut out = crate::json::pretty(&root);
    out.push('\n');
    Ok(out)
}

/// A name for a new entry: the program's file name without its extension,
/// or a URL's host; `-2`, `-3`... when one of `taken` has it already.
pub fn suggest_name(entry: &ServerConfig, taken: &[String]) -> String {
    let base = match &entry.url {
        Some(url) if entry.command.is_empty() => url
            .split("://")
            .nth(1)
            .unwrap_or(url)
            .split(['/', ':', '?'])
            .next()
            .unwrap_or("server")
            .to_owned(),
        _ => Path::new(&entry.command)
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    let base = base.trim().to_lowercase();
    let base = if base.is_empty() {
        "server".to_owned()
    } else {
        base
    };
    if !taken.contains(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken.contains(candidate))
        .expect("the integers do not run out")
}

impl Config {
    pub fn load() -> Config {
        let Some(path) = path() else {
            return Config::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Config::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => Config {
                servers: Vec::new(),
                error: Some(format!("{}: {e}", path.display())),
            },
        }
    }

    pub fn parse(text: &str) -> Config {
        let value = match crate::json::parse(text) {
            Ok(value) => value,
            Err(e) => {
                return Config {
                    servers: Vec::new(),
                    error: Some(format!("mcp.json is not valid JSON: {e}")),
                };
            }
        };
        let servers = match value.get("mcpServers") {
            Some(Value::Object(members)) => members
                .iter()
                .map(|(name, entry)| (name.clone(), server_from(entry)))
                .collect(),
            _ => Vec::new(),
        };
        Config {
            servers,
            error: None,
        }
    }
}

fn expand(text: &str) -> String {
    match text.strip_prefix("~/") {
        Some(rest) => {
            std::env::var("HOME").map_or_else(|_| text.to_owned(), |h| format!("{h}/{rest}"))
        }
        None => text.to_owned(),
    }
}

fn server_from(entry: &Value) -> ServerConfig {
    let strings = |key: &str| -> Vec<String> {
        entry
            .get(key)
            .and_then(Value::as_array)
            .unwrap_or_default()
            .iter()
            .filter_map(Value::as_str)
            .map(expand)
            .collect()
    };
    ServerConfig {
        command: entry
            .get("command")
            .and_then(Value::as_str)
            .map(expand)
            .unwrap_or_default(),
        args: strings("args"),
        env: match entry.get("env") {
            Some(Value::Object(members)) => members
                .iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                .collect(),
            _ => Vec::new(),
        },
        cwd: entry
            .get("cwd")
            .and_then(Value::as_str)
            .map(|c| PathBuf::from(expand(c))),
        url: entry.get("url").and_then(Value::as_str).map(str::to_owned),
    }
}

/// Programs that download a package and run it in one step.
const FETCHERS: &[&str] = &["npx", "bunx", "pnpx", "uvx", "yarn-dlx"];
/// Package managers, and the first arguments that make them fetch and run.
const FETCHING_SUBCOMMANDS: &[(&str, &[&str])] = &[
    ("npm", &["exec", "x", "create", "init"]),
    ("pnpm", &["dlx", "create"]),
    ("yarn", &["dlx", "create"]),
    ("bun", &["x", "create"]),
    ("uv", &["tool", "run"]),
    ("pipx", &["run"]),
    ("deno", &["run"]),
];

impl ServerConfig {
    /// The entry as `mcp.json` writes it: only the keys that are set.
    pub fn to_json(&self) -> Value {
        let mut members = Vec::new();
        if let Some(url) = &self.url {
            members.push(("url".to_owned(), Value::String(url.clone())));
        }
        if !self.command.is_empty() {
            members.push(("command".to_owned(), Value::String(self.command.clone())));
        }
        if !self.args.is_empty() {
            members.push((
                "args".to_owned(),
                Value::Array(self.args.iter().map(|a| Value::String(a.clone())).collect()),
            ));
        }
        if !self.env.is_empty() {
            members.push((
                "env".to_owned(),
                Value::Object(
                    self.env
                        .iter()
                        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                        .collect(),
                ),
            ));
        }
        if let Some(cwd) = &self.cwd {
            members.push((
                "cwd".to_owned(),
                Value::String(cwd.to_string_lossy().into_owned()),
            ));
        }
        Value::Object(members)
    }

    /// Why this entry would run code fetched at start, if it would.
    pub fn fetches(&self) -> Option<String> {
        let program = Path::new(&self.command)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if FETCHERS.contains(&program.as_str()) {
            return Some(format!(
                "{program} downloads a package and runs it; point mcp.json at an installed, reviewed binary"
            ));
        }
        let first = self
            .args
            .iter()
            .find(|a| !a.starts_with('-'))
            .map(String::as_str);
        FETCHING_SUBCOMMANDS
            .iter()
            .find(|(name, subs)| *name == program && first.is_some_and(|f| subs.contains(&f)))
            .map(|(name, _)| {
                format!(
                    "{name} {} downloads a package and runs it; point mcp.json at an installed, reviewed binary",
                    first.unwrap_or("")
                )
            })
    }

    /// The program to run, found the way language servers are found. Refused
    /// when it would fetch code or is not installed.
    pub fn resolve(&self) -> Result<PathBuf, String> {
        if self.command.is_empty() {
            return Err("no command in mcp.json".into());
        }
        if let Some(why) = self.fetches() {
            return Err(why);
        }
        let command = Path::new(&self.command);
        if command.components().count() > 1 {
            return if command.is_file() {
                Ok(command.to_path_buf())
            } else {
                Err(format!("{} does not exist", command.display()))
            };
        }
        crate::lsp::servers::search_dirs()
            .into_iter()
            .map(|dir| dir.join(command))
            .find(|p| p.is_file())
            .ok_or_else(|| format!("{} is not installed where crc looks", self.command))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_claude_code_shaped_entries() {
        let config = Config::parse(
            r#"{"mcpServers": {
                "notes": {"command": "/opt/notes-mcp", "args": ["--root", "/tmp/n"], "env": {"LEVEL": "info", "N": 3}},
                "remote": {"url": "https://example.com/mcp"}
            }}"#,
        );
        assert_eq!(config.error, None);
        assert_eq!(config.servers.len(), 2);
        let (name, notes) = &config.servers[0];
        assert_eq!(name, "notes");
        assert_eq!(notes.command, "/opt/notes-mcp");
        assert_eq!(notes.args, ["--root", "/tmp/n"]);
        assert_eq!(notes.env, [("LEVEL".to_string(), "info".to_string())]);
        assert_eq!(
            config.servers[1].1.url.as_deref(),
            Some("https://example.com/mcp")
        );
        assert!(
            config.servers[1]
                .1
                .resolve()
                .unwrap_err()
                .contains("no command")
        );
        assert!(Config::parse("{nope").error.is_some());
        assert!(Config::parse("{}").servers.is_empty());
    }

    #[test]
    fn adding_an_entry_keeps_the_rest_of_the_file() {
        let notes = ServerConfig {
            command: "/opt/notes-mcp".into(),
            args: vec!["--root".into(), "/tmp/n".into()],
            ..ServerConfig::default()
        };
        // An empty template gets its first entry.
        let text = added(TEMPLATE, "notes", &notes).unwrap();
        assert_eq!(
            text,
            "{\n  \"mcpServers\": {\n    \"notes\": {\n      \"command\": \"/opt/notes-mcp\",\n      \"args\": [\n        \"--root\",\n        \"/tmp/n\"\n      ]\n    }\n  }\n}\n"
        );
        let parsed = Config::parse(&text);
        assert_eq!(parsed.servers, vec![("notes".to_string(), notes.clone())]);
        // Other keys and other servers stay; a same-named one is replaced.
        let remote = ServerConfig {
            url: Some("https://example.com/mcp".into()),
            ..ServerConfig::default()
        };
        let text = added(
            r#"{"other": true, "mcpServers": {"notes": {"command": "/old"}, "a": {"url": "https://a"}}}"#,
            "notes",
            &notes,
        )
        .unwrap();
        let text = added(&text, "remote", &remote).unwrap();
        let parsed = Config::parse(&text);
        assert_eq!(
            parsed
                .servers
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>(),
            ["notes", "a", "remote"]
        );
        assert_eq!(parsed.servers[0].1, notes);
        assert_eq!(parsed.servers[2].1, remote);
        assert!(text.contains("\"other\": true"));
        // A file with no mcpServers key, and one that is not JSON.
        assert_eq!(
            Config::parse(&added("{}", "x", &remote).unwrap())
                .servers
                .len(),
            1
        );
        assert_eq!(
            Config::parse(&added("", "x", &remote).unwrap())
                .servers
                .len(),
            1
        );
        assert!(
            added("{nope", "x", &remote)
                .unwrap_err()
                .contains("not valid JSON")
        );
        assert!(
            added("[]", "x", &remote)
                .unwrap_err()
                .contains("not a JSON object")
        );
    }

    #[test]
    fn suggested_names_come_from_the_program_or_host() {
        let cmd = |c: &str| ServerConfig {
            command: c.into(),
            ..ServerConfig::default()
        };
        let url = |u: &str| ServerConfig {
            url: Some(u.into()),
            ..ServerConfig::default()
        };
        assert_eq!(suggest_name(&cmd("/opt/bin/Notes-MCP"), &[]), "notes-mcp");
        assert_eq!(suggest_name(&cmd("/opt/server.py"), &[]), "server");
        assert_eq!(suggest_name(&cmd(""), &[]), "server");
        assert_eq!(
            suggest_name(&url("https://api.example.com:8443/mcp?x=1"), &[]),
            "api.example.com"
        );
        assert_eq!(
            suggest_name(&cmd("/opt/notes"), &["notes".into(), "notes-2".into()]),
            "notes-3"
        );
    }

    #[test]
    fn launchers_that_download_code_are_refused() {
        let entry = |command: &str, args: &[&str]| ServerConfig {
            command: command.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            ..ServerConfig::default()
        };
        for (command, args) in [
            ("npx", &["-y", "@scope/server"][..]),
            ("/usr/local/bin/bunx", &["server"][..]),
            ("uvx", &["mcp-server-time"][..]),
            ("npm", &["exec", "server"][..]),
            ("pnpm", &["dlx", "server"][..]),
            ("bun", &["x", "server"][..]),
            ("uv", &["tool", "run", "server"][..]),
        ] {
            let e = entry(command, args);
            assert!(e.fetches().is_some(), "{command} {args:?}");
            assert!(e.resolve().unwrap_err().contains("downloads"));
        }
        assert_eq!(entry("node", &["/opt/server/index.js"]).fetches(), None);
        assert_eq!(entry("npm", &["--version"]).fetches(), None);
        assert!(
            entry("/nowhere/server", &[])
                .resolve()
                .unwrap_err()
                .contains("does not exist")
        );
        assert_eq!(
            entry("/bin/cat", &[]).resolve(),
            Ok(PathBuf::from("/bin/cat"))
        );
    }
}
