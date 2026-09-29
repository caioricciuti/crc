//! Reading what a language server sends: positions, diagnostics, code
//! actions, completions, locations, edits, signatures and hovers, from the
//! protocol's JSON into the editor's types.

use super::client::ActionSteps;
use super::{
    CodeAction, Completion, Diagnostic, FileEdits, Location, Position, Severity, Signature,
    TextEdit, offset_of, path_for,
};
use crate::json::{Value, compact, object, string};

pub(super) fn parse_position(value: &Value) -> Option<Position> {
    Some(Position {
        line: value.get("line")?.as_u64()? as u32,
        character: value.get("character")?.as_u64()? as u32,
    })
}

pub(super) fn parse_diagnostic(value: &Value) -> Option<Diagnostic> {
    let range = value.get("range")?;
    Some(Diagnostic {
        start: parse_position(range.get("start")?)?,
        end: parse_position(range.get("end")?)?,
        severity: Severity::from_wire(value.get("severity").and_then(Value::as_u64)),
        message: value.get("message")?.as_str()?.to_owned(),
        source: value
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_owned),
        raw: compact(value),
    })
}

/// What a code action's reply lists: a CodeAction, or a bare Command.
pub(super) fn parse_code_action(value: &Value) -> Option<CodeAction> {
    Some(CodeAction {
        title: value.get("title")?.as_str()?.to_owned(),
        kind: value
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        preferred: value
            .get("isPreferred")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        disabled: value
            .path("disabled.reason")
            .and_then(Value::as_str)
            .map(str::to_owned),
        raw: compact(value),
        origin: None,
    })
}

/// A CodeAction's edit and command, or a bare Command as the command.
pub(super) fn steps_of(title: &str, value: &Value) -> ActionSteps {
    let command = match value.get("command") {
        Some(Value::String(_)) => Some(compact(value)),
        Some(command @ Value::Object(_)) => Some(compact(command)),
        _ => None,
    };
    ActionSteps {
        title: title.to_owned(),
        edits: value
            .get("edit")
            .map(parse_workspace_edit)
            .unwrap_or_default(),
        command,
    }
}

/// What the client says about code actions in `initialize`: the literal
/// form, the kinds it knows, and that the edit may come later.
pub(super) fn code_action_capability() -> Value {
    let kinds = [
        "",
        "quickfix",
        "refactor",
        "refactor.extract",
        "refactor.inline",
        "refactor.rewrite",
        "source",
        "source.organizeImports",
        "source.fixAll",
    ];
    object([
        (
            "codeActionLiteralSupport",
            object([(
                "codeActionKind",
                object([(
                    "valueSet",
                    Value::Array(kinds.iter().map(|k| string(k)).collect()),
                )]),
            )]),
        ),
        ("isPreferredSupport", Value::Bool(true)),
        ("disabledSupport", Value::Bool(true)),
        ("dataSupport", Value::Bool(true)),
        (
            "resolveSupport",
            object([("properties", Value::Array(vec![string("edit")]))]),
        ),
    ])
}

pub(super) fn parse_completion(value: &Value) -> Option<Completion> {
    let label = value.get("label")?.as_str()?.to_owned();
    let edit = value.get("textEdit").and_then(|edit| {
        // A plain edit has `range`; an insert/replace edit has both, and
        // `replace` is what a typed prefix wants.
        let range = edit.get("range").or_else(|| edit.get("replace"))?;
        Some((
            parse_position(range.get("start")?)?,
            parse_position(range.get("end")?)?,
            edit.get("newText")?.as_str()?.to_owned(),
        ))
    });
    let insert_text = value
        .get("insertText")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| label.clone());
    Some(Completion {
        sort_text: value
            .get("sortText")
            .and_then(Value::as_str)
            .unwrap_or(&label)
            .to_owned(),
        filter_text: value
            .get("filterText")
            .and_then(Value::as_str)
            .unwrap_or(&label)
            .to_owned(),
        detail: value
            .get("detail")
            .and_then(Value::as_str)
            .map(str::to_owned),
        kind: value.get("kind").and_then(Value::as_u64).unwrap_or(0),
        edit,
        insert_text,
        label,
    })
}

pub(super) fn parse_location(value: &Value) -> Option<Location> {
    // Location has uri + range; LocationLink has targetUri + targetRange.
    let uri = value
        .get("uri")
        .or_else(|| value.get("targetUri"))?
        .as_str()?;
    let range = value
        .get("targetSelectionRange")
        .or_else(|| value.get("range"))
        .or_else(|| value.get("targetRange"))?;
    Some(Location {
        path: path_for(uri)?,
        start: parse_position(range.get("start")?)?,
        end: parse_position(range.get("end")?)?,
    })
}

/// Whether a capability is on: `true` or an options object.
pub(super) fn provides(capabilities: &Value, name: &str) -> bool {
    match capabilities.get(name) {
        Some(Value::Bool(on)) => *on,
        Some(Value::Object(_)) => true,
        _ => false,
    }
}

pub(super) fn parse_text_edit(value: &Value) -> Option<TextEdit> {
    let range = value.get("range")?;
    Some(TextEdit {
        start: parse_position(range.get("start")?)?,
        end: parse_position(range.get("end")?)?,
        text: value.get("newText")?.as_str()?.to_owned(),
    })
}

/// A WorkspaceEdit's text edits by file, from `documentChanges` when the
/// server sent them, else `changes`. File creations, renames and deletions
/// are not applied, and are left out.
pub(super) fn parse_workspace_edit(value: &Value) -> Vec<FileEdits> {
    let edits = |items: Option<&Value>| -> Vec<TextEdit> {
        items
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(parse_text_edit).collect())
            .unwrap_or_default()
    };
    if let Some(changes) = value.get("documentChanges").and_then(Value::as_array) {
        return changes
            .iter()
            .filter_map(|change| {
                let uri = change.path("textDocument.uri")?.as_str()?;
                Some(FileEdits {
                    path: path_for(uri)?,
                    // The protocol's guard that the edits still apply; null
                    // for a file the server does not have open.
                    version: change.path("textDocument.version").and_then(Value::as_u64),
                    edits: edits(change.get("edits")),
                })
            })
            .collect();
    }
    match value.get("changes") {
        Some(Value::Object(files)) => files
            .iter()
            .filter_map(|(uri, list)| {
                Some(FileEdits {
                    path: path_for(uri)?,
                    version: None,
                    edits: edits(Some(list)),
                })
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The active signature and parameter, from a SignatureHelp.
pub(super) fn parse_signature(value: &Value) -> Option<Signature> {
    let signatures = value.get("signatures")?.as_array()?;
    let index = value
        .get("activeSignature")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let signature = signatures.get(index).or_else(|| signatures.first())?;
    let label = signature.get("label")?.as_str()?.to_owned();
    let parameter = signature
        .get("activeParameter")
        .or_else(|| value.get("activeParameter"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let active = signature
        .get("parameters")
        .and_then(Value::as_array)
        .and_then(|parameters| parameters.get(parameter))
        .and_then(|p| p.get("label"))
        .and_then(|l| match l {
            // A substring of the label, or [start, end] in UTF-16 units.
            Value::String(text) => label.find(text.as_str()).map(|at| at..at + text.len()),
            Value::Array(bounds) => {
                let unit = |i: usize| bounds.get(i)?.as_u64().map(|n| n as usize);
                let (start, end) = (unit(0)?, unit(1)?);
                let byte = |units: usize| {
                    let mut seen = 0;
                    for (at, c) in label.char_indices() {
                        if seen >= units {
                            return at;
                        }
                        seen += c.len_utf16();
                    }
                    label.len()
                };
                // Reversed bounds would be sliced as-is by the drawing code.
                let (start, end) = (byte(start), byte(end));
                (start <= end).then_some(start..end)
            }
            _ => None,
        });
    Some(Signature { label, active })
}

/// Hover contents come in four shapes; all of them become plain lines.
pub(super) fn hover_text(contents: Option<&Value>) -> String {
    fn one(value: &Value) -> String {
        match value {
            Value::String(s) => s.clone(),
            Value::Object(_) => value
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            Value::Array(items) => items.iter().map(one).collect::<Vec<_>>().join("\n"),
            _ => String::new(),
        }
    }
    contents.map(one).unwrap_or_default().trim().to_owned()
}

/// The byte range a completion replaces, given the text it was asked in.
pub fn completion_range(
    item: &Completion,
    rope: &crate::text::rope::Rope,
    caret: usize,
) -> std::ops::Range<usize> {
    if let Some((start, end, _)) = &item.edit {
        let (start, end) = (offset_of(rope, *start), offset_of(rope, *end));
        return start.min(end)..start.max(end);
    }
    // No edit given: the word before the caret.
    let line = rope.byte_to_line(caret);
    let line_start = rope.line_to_byte(line);
    let before = rope.slice_to_string(line_start..caret);
    caret - crate::complete::word_len(&before)..caret
}

/// What a completion inserts.
pub fn completion_text(item: &Completion) -> &str {
    match &item.edit {
        Some((_, _, text)) => text,
        None => &item.insert_text,
    }
}
