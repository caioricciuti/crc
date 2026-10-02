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
