//! `manifest.json`: what an extension is, what it asks for, and its
//! commands. The same rules as the registry build in crc-extensions, so an
//! extension installed from a folder is held to what CI holds official ones
//! to.

use crate::json::Value;

/// The extension interface this crc speaks.
pub const API: u64 = 1;

/// What an extension may be granted. Anything else in a manifest refuses
/// the install.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    SelectionRead,
    SelectionReplace,
    DocumentRead,
    DocumentEdit,
}

impl Capability {
    pub fn parse(name: &str) -> Option<Capability> {
        Some(match name {
            "selection.read" => Capability::SelectionRead,
            "selection.replace" => Capability::SelectionReplace,
            "document.read" => Capability::DocumentRead,
            "document.edit" => Capability::DocumentEdit,
            _ => return None,
        })
    }

    /// What the install prompt says it grants.
    pub fn describe(self) -> &'static str {
        match self {
            Capability::SelectionRead => "Read the selected text",
            Capability::SelectionReplace => "Replace the selected text",
            Capability::DocumentRead => "Read the whole document",
            Capability::DocumentEdit => "Replace the whole document",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    /// The export that runs it.
    pub id: String,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub authors: Vec<String>,
    pub license: String,
    pub entry: String,
    pub capabilities: Vec<Capability>,
    pub commands: Vec<Command>,
}

impl Manifest {
    /// Whether a command given a selection may see it, and one given no
    /// selection may see the whole document.
    pub fn may_read(&self, selection: bool) -> bool {
        self.capabilities.contains(&if selection {
            Capability::SelectionRead
        } else {
            Capability::DocumentRead
        })
    }

    pub fn may_replace(&self, selection: bool) -> bool {
        self.capabilities.contains(&if selection {
            Capability::SelectionReplace
        } else {
            Capability::DocumentEdit
        })
    }
}

fn is_id(id: &str) -> bool {
    let word = |w: &str| {
        !w.is_empty()
            && !w.starts_with('-')
            && !w.ends_with('-')
            && !w.contains("--")
            && w.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    let parts: Vec<&str> = id.split('.').collect();
    parts.len() >= 2 && parts.iter().all(|p| word(p))
}

fn is_version(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

fn is_command(id: &str) -> bool {
    id.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Reads and checks a manifest. The error says what is wrong, for the
/// install prompt.
pub fn parse(value: &Value) -> Result<Manifest, String> {
    let text = |key: &str| -> Result<String, String> {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("the manifest has no {key}"))
    };
    let id = text("id")?;
    if !is_id(&id) {
        return Err(format!("the id {id:?} is not like author.name"));
    }
    let version = text("version")?;
    if !is_version(&version) {
        return Err(format!("the version {version:?} is not x.y.z"));
    }
    match value.get("api").and_then(Value::as_u64) {
        Some(API) => {}
        Some(other) => {
            return Err(format!(
                "it needs extension API {other}; this crc speaks {API}"
            ));
        }
        None => return Err("the manifest has no api".into()),
    }
    let entry = text("entry")?;
    if entry.contains('/') || entry.contains('\\') || !entry.ends_with(".wasm") {
        return Err(format!("the entry {entry:?} is not a .wasm file name"));
    }
    let mut capabilities = Vec::new();
    for item in value
        .get("capabilities")
        .and_then(Value::as_array)
        .ok_or("the manifest has no capabilities")?
    {
        let name = item.as_str().unwrap_or("");
        let capability =
            Capability::parse(name).ok_or_else(|| format!("unknown capability {name:?}"))?;
        if !capabilities.contains(&capability) {
            capabilities.push(capability);
        }
    }
    let mut commands = Vec::new();
    for item in value
        .get("commands")
        .and_then(Value::as_array)
        .ok_or("the manifest has no commands")?
    {
        let id = item.get("id").and_then(Value::as_str).unwrap_or("");
        let title = item.get("title").and_then(Value::as_str).unwrap_or("");
        if !is_command(id) || title.trim().is_empty() {
            return Err(format!(
                "a command needs an id like sort_lines and a title, got {id:?}"
            ));
        }
        commands.push(Command {
            id: id.to_owned(),
            title: title.trim().to_owned(),
        });
    }
    if commands.is_empty() {
        return Err("the manifest has no commands".into());
    }
    Ok(Manifest {
        name: text("name")?,
        description: text("description")?,
        license: text("license")?,
        authors: value
            .get("authors")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        id,
        version,
        entry,
        capabilities,
        commands,
    })
}

/// Checks a module against its manifest: every command exported, the
/// memory and allocator exports present, and nothing imported but crc's own
/// host functions.
pub fn check_module(manifest: &Manifest, module: &super::wasm::Module) -> Result<(), String> {
    let exports: Vec<&str> = module.export_names().collect();
    for name in ["memory", "crc_alloc", "crc_free"] {
        if !exports.contains(&name) {
            return Err(format!("{} does not export {name}", manifest.entry));
        }
    }
    for command in &manifest.commands {
        if !exports.contains(&command.id.as_str()) {
            return Err(format!(
                "the command {} is not in {}",
                command.id, manifest.entry
            ));
        }
    }
    for (module, name) in module.import_names() {
        if module != "crc" || !super::run::HOST_FUNCTIONS.contains(&name) {
            return Err(format!(
                "it imports {module}.{name}, which crc does not provide"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SORT: &str = r#"{
        "id": "crc.sort-lines", "name": "Sort Lines", "version": "0.1.0",
        "description": "Sort.", "authors": ["crc"], "license": "MIT OR Apache-2.0",
        "api": 1, "entry": "sort_lines.wasm",
        "capabilities": ["selection.read", "selection.replace"],
        "commands": [{ "id": "sort", "title": "Sort Lines" }]
    }"#;

    #[test]
    fn reads_a_manifest() {
        let m = parse(&crate::json::parse(SORT).unwrap()).unwrap();
        assert_eq!(m.id, "crc.sort-lines");
        assert_eq!(m.commands[0].title, "Sort Lines");
        assert!(m.may_read(true) && m.may_replace(true));
        assert!(!m.may_read(false) && !m.may_replace(false));
    }

    #[test]
    fn refuses_what_crc_would_not_enforce() {
        let with = |from: &str, to: &str| {
            parse(&crate::json::parse(&SORT.replace(from, to)).unwrap()).unwrap_err()
        };
        assert!(with("\"selection.read\"", "\"network\"").contains("unknown capability"));
        assert!(with("\"api\": 1", "\"api\": 2").contains("API 2"));
        assert!(with("crc.sort-lines", "Sort Lines!").contains("author.name"));
        assert!(with("sort_lines.wasm", "../x.wasm").contains(".wasm file name"));
        assert!(with("\"0.1.0\"", "\"1\"").contains("x.y.z"));
        assert!(with("\"id\": \"sort\"", "\"id\": \"Sort-It\"").contains("command"));
    }
}
