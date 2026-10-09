//! Agent review sessions: which files an agent in a crc terminal changed,
//! and each one as it was before the agent's first write to it.
//!
//! Every terminal crc starts gets its own folder under
//! `Application Support/crc/sessions`, named in `CRC_SESSION_DIR`. The
//! agent's hook (`crc --hook pre`, run by Claude Code before Write, Edit,
//! MultiEdit and NotebookEdit) copies the file into it before the write
//! lands, so there is no race with the watcher, and appends a line to its
//! `events` file. The window reads new lines when the terminal prints.
//!
//! Layout of a session folder:
//!
//! - `files/<key>.path`: the file's path, created exclusively, so the
//!   first write to a path claims its one checkpoint;
//! - `files/<key>`: its text before that write, or `files/<key>.absent`
//!   when it did not exist, or `files/<key>.large` when it was too big to
//!   keep;
//! - `events`: one line per hook call, appended.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The variable that tells the hook where its session folder is.
pub const SESSION_VAR: &str = "CRC_SESSION_DIR";

/// Files bigger than this are recorded but not copied: they can be kept,
/// never undone.
pub const MAX_CHECKPOINT: u64 = 8 << 20;

/// How long a session nobody reviewed stays on disk.
pub const MAX_AGE: std::time::Duration = std::time::Duration::from_secs(14 * 24 * 60 * 60);

/// Where every session folder lives.
pub fn root() -> Option<PathBuf> {
    Some(crate::platform::app_support()?.join("sessions"))
}

/// A folder name no other terminal, in this run or an earlier one, has.
pub fn new_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{nanos:x}-{}", std::process::id())
}

/// What a session kept of a file from before the agent changed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Checkpoint {
    /// Its text, in this file.
    Text(PathBuf),
    /// It did not exist: undoing deletes it.
    Absent,
    /// Over [`MAX_CHECKPOINT`]: recorded, not copied.
    TooLarge,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct File {
    pub path: PathBuf,
    pub checkpoint: Checkpoint,
}

/// One line of a session's `events` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The agent is about to write this file with this tool.
    Wrote { tool: String, path: PathBuf },
    /// The agent is waiting on the person: Claude's Notification hook.
    Asked(String),
    /// The hook could not do its job; said, never passed to the agent.
    Failed(String),
}

/// What `crc --hook <kind>` does with the hook's JSON on stdin, for the
/// session in `dir`. `pre` checkpoints the file a tool is about to write,
/// `notify` records the agent's question.
pub fn hook(kind: &str, input: &str, dir: &Path) -> Result<Event, String> {
    let value = crate::json::parse(input).map_err(|e| format!("hook input: {e}"))?;
    let event = match kind {
        "pre" => {
            let tool = value
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let named = value
                .path("tool_input.file_path")
                .or_else(|| value.path("tool_input.notebook_path"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("{tool}: no file path in the tool input"))?;
            let mut path = PathBuf::from(named);
            if path.is_relative() {
                let cwd = value.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
                path = Path::new(cwd).join(path);
            }
            let path = resolve(&path);
            checkpoint(dir, &path).map_err(|e| format!("{}: {e}", path.display()))?;
            Event::Wrote { tool, path }
        }
        "notify" => Event::Asked(
            value
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned(),
        ),
        other => return Err(format!("unknown hook {other:?}")),
    };
    append(dir, &event).map_err(|e| format!("events: {e}"))?;
    Ok(event)
}

/// `crc --hook <kind>`: reads the hook's input and does [`hook`]. Never
/// prints and never fails, so it cannot block or change the agent's call;
/// outside a crc terminal it does nothing at all.
pub fn run_hook(kind: &str) {
    let Some(dir) = std::env::var_os(SESSION_VAR).map(PathBuf::from) else {
        return;
    };
    // Only ever a folder of ours: whatever set the variable, the hook
    // writes nowhere else.
    if !root().is_some_and(|root| dir.parent() == Some(root.as_path())) {
        return;
    }
    let mut input = String::new();
    let read = std::io::Read::read_to_string(
        &mut std::io::Read::take(std::io::stdin(), 16 << 20),
        &mut input,
    );
    let result = match read {
        Ok(_) => hook(kind, &input, &dir),
        Err(e) => Err(format!("hook input: {e}")),
    };
    if let Err(e) = result {
        let _ = append(&dir, &Event::Failed(e));
    }
}

/// The path as the editor keys its documents, for a file that may not
/// exist yet: its folder resolved, its name as given.
fn resolve(path: &Path) -> PathBuf {
    if path.exists() {
        return crate::platform::canonical(path);
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => crate::platform::canonical(parent).join(name),
        _ => path.to_path_buf(),
    }
}

fn key(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    format!(
        "{:016x}",
        crate::platform::fnv1a(path.as_os_str().as_bytes())
    )
}

/// Keeps `path` as it is now, unless this session already has it.
pub fn checkpoint(dir: &Path, path: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let files = dir.join("files");
    std::fs::create_dir_all(&files)?;
    let key = key(path);
    // The claim: two hooks racing for one path both try, one wins.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(files.join(format!("{key}.path")))
    {
        Ok(mut claim) => claim.write_all(path.as_os_str().as_bytes())?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(e),
    }
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::write(files.join(format!("{key}.absent")), "")
        }
        Err(e) => Err(e),
        Ok(meta) if meta.len() > MAX_CHECKPOINT => {
            std::fs::write(files.join(format!("{key}.large")), "")
        }
        Ok(_) => {
            // Copied aside and renamed, so a reader never sees half a file.
            let partial = files.join(format!("{key}.partial"));
            std::fs::copy(path, &partial)?;
            std::fs::rename(partial, files.join(key))
        }
    }
}

/// Every file the session has a checkpoint for, in path order. A claim
/// whose copy is not finished yet is left out until it is.
pub fn files(dir: &Path) -> Vec<File> {
    let files = dir.join("files");
    let Ok(entries) = std::fs::read_dir(&files) else {
        return Vec::new();
    };
    let mut out: Vec<File> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let key = name.strip_suffix(".path")?;
            let path = crate::platform::path_from_bytes(std::fs::read(entry.path()).ok()?);
            let text = files.join(key);
            let checkpoint = if text.is_file() {
                Checkpoint::Text(text)
            } else if files.join(format!("{key}.absent")).exists() {
                Checkpoint::Absent
            } else if files.join(format!("{key}.large")).exists() {
                Checkpoint::TooLarge
            } else {
                return None;
            };
            Some(File { path, checkpoint })
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// One line per event. Tabs and line breaks in the text become spaces, so
/// a line always splits back into the same fields.
fn append(dir: &Path, event: &Event) -> std::io::Result<()> {
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    let line = match event {
        Event::Wrote { tool, path } => {
            format!(
                "wrote\t{}\t{}\n",
                clean(tool),
                clean(&path.to_string_lossy())
            )
        }
        Event::Asked(text) => format!("asked\t{}\n", clean(text)),
        Event::Failed(text) => format!("failed\t{}\n", clean(text)),
    };
    std::fs::create_dir_all(dir)?;
    // One write of a short line with O_APPEND: hooks running at once do
    // not interleave inside a line.
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("events"))?
        .write_all(line.as_bytes())
}

/// The events after byte `from`, and where the next read starts. Only
/// whole lines are taken; a line still being written waits for the next
/// read.
pub fn read_events(dir: &Path, from: u64) -> (Vec<Event>, u64) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(dir.join("events")) else {
        return (Vec::new(), from);
    };
    let len = file.metadata().map_or(0, |m| m.len());
    if len <= from || file.seek(SeekFrom::Start(from)).is_err() {
        return (Vec::new(), from);
    }
    let mut bytes = Vec::new();
    if file.take(len - from).read_to_end(&mut bytes).is_err() {
        return (Vec::new(), from);
    }
    let Some(end) = bytes.iter().rposition(|&b| b == b'\n') else {
        return (Vec::new(), from);
    };
    let text = String::from_utf8_lossy(&bytes[..end]);
    let events = text
        .lines()
        .filter_map(|line| {
            let (kind, rest) = line.split_once('\t').unwrap_or((line, ""));
            Some(match kind {
                "wrote" => {
                    let (tool, path) = rest.split_once('\t')?;
                    Event::Wrote {
                        tool: tool.to_owned(),
                        path: PathBuf::from(path),
                    }
                }
                "asked" => Event::Asked(rest.to_owned()),
                "failed" => Event::Failed(rest.to_owned()),
                _ => return None,
            })
        })
        .collect();
    (events, from + end as u64 + 1)
}

/// Removes session folders untouched for longer than `max_age`, and says
/// how many went. A folder's time is its newest event or checkpoint.
pub fn prune(root: &Path, max_age: std::time::Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let now = std::time::SystemTime::now();
    let mut removed = 0;
    for entry in entries.filter_map(Result::ok) {
        let dir = entry.path();
        let newest = [dir.join("events"), dir.join("files"), dir.clone()]
            .iter()
            .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
            .max();
        let old = newest
            .and_then(|at| now.duration_since(at).ok())
            .is_some_and(|age| age >= max_age);
        if old && std::fs::remove_dir_all(&dir).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Quoted for `sh`: the whole string, literally.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Claude Code settings that run this binary's hooks, for `claude
/// --settings`. Claude merges hooks from every settings source, so the
/// person's own hooks still run.
pub fn claude_settings(exe: &Path) -> String {
    use crate::json::{Value, object, string};
    let command = |kind: &str| {
        Value::Array(vec![object([
            ("type", string("command")),
            (
                "command",
                string(&format!(
                    "{} --hook {kind}",
                    shell_quote(&exe.to_string_lossy())
                )),
            ),
        ])])
    };
    let settings = object([(
        "hooks",
        object([
            (
                "PreToolUse",
                Value::Array(vec![object([
                    ("matcher", string("Write|Edit|MultiEdit|NotebookEdit")),
                    ("hooks", command("pre")),
                ])]),
            ),
            (
                "Notification",
                Value::Array(vec![object([("hooks", command("notify"))])]),
            ),
        ]),
    )]);
    crate::json::pretty(&settings)
}

/// Writes [`claude_settings`] for the running binary where `claude
/// --settings` reads it, and returns that path quoted for the shell.
pub fn claude_settings_arg() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let path = crate::platform::app_support()?.join("claude-hooks.json");
    let text = claude_settings(&exe);
    if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::write(&path, text).ok()?;
    }
    Some(shell_quote(&path.to_string_lossy()))
}

/// What a review compares for one file: its checkpoint and its text now.
/// A file the agent deleted reads as empty; so does one that did not
/// exist before.
pub fn texts(file: &File) -> Result<(String, String), String> {
    let old = match &file.checkpoint {
        Checkpoint::Text(kept) => std::fs::read_to_string(kept)
            .map_err(|_| "not text: keep or undo the whole file".to_owned())?,
        Checkpoint::Absent => String::new(),
        Checkpoint::TooLarge => {
            return Err("too large to review by change: keep it whole".into());
        }
    };
    let new = match std::fs::read_to_string(&file.path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return Err("not text: keep or undo the whole file".into()),
    };
    Ok((old, new))
}

/// What the review showed was made from this text. A hunk number only
/// means something against the text it was counted in, so every action
/// takes the fingerprint of what was on screen and refuses when the file
/// has moved on since.
pub fn fingerprint(text: &str) -> u64 {
    crate::platform::fnv1a(text.as_bytes())
}

fn current(file: &File, seen: u64) -> Result<(String, String), String> {
    let (old, new) = texts(file)?;
    if fingerprint(&new) != seen {
        return Err(format!(
            "{} changed since it was shown; look again",
            file.path.display()
        ));
    }
    Ok((old, new))
}

/// Keeps one change: the checkpoint takes it, so it leaves the review.
/// The last change kept takes the file out of the session.
pub fn keep(dir: &Path, file: &File, hunk: usize, seen: u64) -> Result<(), String> {
    let (old, new) = current(file, seen)?;
    let kept = crate::ide::diff::keep_hunk(&old, &new, hunk).ok_or("that change is gone")?;
    if kept == new {
        return forget(dir, file);
    }
    set_checkpoint(dir, &file.path, &kept).map_err(|e| format!("checkpoint: {e}"))
}

/// The file's text with one change undone, for the caller to put in place:
/// through the open document when there is one, so it is one undo step,
/// or with [`write_undone`] when there is not.
pub fn undone(file: &File, hunk: usize, seen: u64) -> Result<String, String> {
    let (old, new) = current(file, seen)?;
    crate::ide::diff::undo_hunk(&old, &new, hunk).ok_or_else(|| "that change is gone".into())
}

/// Writes [`undone`]'s text to the file, for a file not open in crc, and
/// takes the file out of the session when nothing is left to review.
pub fn write_undone(dir: &Path, file: &File, text: &str) -> Result<(), String> {
    std::fs::write(&file.path, text).map_err(|e| format!("{}: {e}", file.path.display()))?;
    settle(dir, file)
}

/// After the file was changed through the editor: takes it out of the
/// session when it matches its checkpoint again.
pub fn settle(dir: &Path, file: &File) -> Result<(), String> {
    match texts(file) {
        Ok((old, new)) if old == new => forget(dir, file),
        _ => Ok(()),
    }
}

/// Keeps every change in the file.
pub fn keep_all(dir: &Path, file: &File) -> Result<(), String> {
    forget(dir, file)
}

/// Puts the whole file back as it was: its checkpoint copied over it, or
/// the file removed when the agent created it.
pub fn undo_all(dir: &Path, file: &File) -> Result<(), String> {
    let said = |e: std::io::Error| format!("{}: {e}", file.path.display());
    match &file.checkpoint {
        Checkpoint::Text(kept) => {
            std::fs::copy(kept, &file.path).map_err(said)?;
        }
        Checkpoint::Absent => match std::fs::remove_file(&file.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(said(e)),
            _ => {}
        },
        Checkpoint::TooLarge => return Err("too large: no copy was kept to undo to".into()),
    }
    forget(dir, file)
}

/// A new checkpoint for a file the session already has.
fn set_checkpoint(dir: &Path, path: &Path, text: &str) -> std::io::Result<()> {
    let files = dir.join("files");
    let key = key(path);
    let partial = files.join(format!("{key}.partial"));
    std::fs::write(&partial, text)?;
    std::fs::rename(partial, files.join(&key))?;
    match std::fs::remove_file(files.join(format!("{key}.absent"))) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Takes the file out of the session: nothing about it left to review.
fn forget(dir: &Path, file: &File) -> Result<(), String> {
    let files = dir.join("files");
    let key = key(&file.path);
    // The claim goes last: until it does, the file still lists, never
    // half removed.
    for suffix in ["", ".absent", ".large", ".partial", ".path"] {
        match std::fs::remove_file(files.join(format!("{key}{suffix}"))) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(format!("forget {}: {e}", file.path.display()));
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::TempTree;

    fn pre(path: &Path) -> String {
        format!(
            r#"{{"hook_event_name":"PreToolUse","cwd":"/","tool_name":"Edit","tool_input":{{"file_path":{}}}}}"#,
            crate::json::compact(&crate::json::string(&path.to_string_lossy()))
        )
    }

    #[test]
    fn the_first_write_keeps_the_file_as_it_was() {
        let tree = TempTree::new("review-first", &[("a.txt", "one\n")]);
        let dir = tree.0.join("session");
        let a = crate::platform::canonical(&tree.0.join("a.txt"));

        hook("pre", &pre(&a), &dir).expect("hook");
        std::fs::write(&a, "two\n").expect("agent writes");
        // A second write in the same session keeps the first checkpoint.
        hook("pre", &pre(&a), &dir).expect("hook");
        std::fs::write(&a, "three\n").expect("agent writes");

        let files = files(&dir);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, a);
        let Checkpoint::Text(kept) = &files[0].checkpoint else {
            panic!("{:?}", files[0].checkpoint);
        };
        assert_eq!(std::fs::read_to_string(kept).expect("read"), "one\n");
    }

    #[test]
    fn new_and_large_files_are_recorded_without_a_copy() {
        let tree = TempTree::new("review-kinds", &[]);
        let dir = tree.0.join("session");
        let new = tree.0.join("new.txt");
        let big = tree.0.join("big.bin");
        std::fs::write(&big, vec![b'x'; MAX_CHECKPOINT as usize + 1]).expect("write");

        hook("pre", &pre(&new), &dir).expect("hook");
        hook("pre", &pre(&big), &dir).expect("hook");

        let kinds: Vec<Checkpoint> = files(&dir).into_iter().map(|f| f.checkpoint).collect();
        assert_eq!(kinds, vec![Checkpoint::TooLarge, Checkpoint::Absent]);
    }

    #[test]
    fn a_relative_path_is_taken_from_the_agents_folder() {
        let tree = TempTree::new("review-relative", &[("src/b.rs", "fn b() {}\n")]);
        let dir = tree.0.join("session");
        let input = format!(
            r#"{{"cwd":{},"tool_name":"Write","tool_input":{{"file_path":"src/b.rs"}}}}"#,
            crate::json::compact(&crate::json::string(&tree.0.to_string_lossy()))
        );
        let event = hook("pre", &input, &dir).expect("hook");
        let b = crate::platform::canonical(&tree.0.join("src/b.rs"));
        assert_eq!(
            event,
            Event::Wrote {
                tool: "Write".into(),
                path: b.clone()
            }
        );
        assert_eq!(files(&dir)[0].path, b);
    }

    #[test]
    fn events_are_read_whole_lines_at_a_time() {
        let tree = TempTree::new("review-events", &[("c.txt", "")]);
        let dir = tree.0.join("session");
        let c = crate::platform::canonical(&tree.0.join("c.txt"));
        hook("pre", &pre(&c), &dir).expect("hook");
        hook(
            "notify",
            r#"{"message":"Claude needs\tyour permission"}"#,
            &dir,
        )
        .expect("hook");

        let (events, at) = read_events(&dir, 0);
        assert_eq!(
            events,
            vec![
                Event::Wrote {
                    tool: "Edit".into(),
                    path: c
                },
                Event::Asked("Claude needs your permission".into()),
            ]
        );
        assert_eq!(read_events(&dir, at), (Vec::new(), at));

        // Half a line waits for the rest.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("events"))
            .expect("open");
        file.write_all(b"asked\tha").expect("write");
        assert_eq!(read_events(&dir, at), (Vec::new(), at));
        file.write_all(b"lf\n").expect("write");
        assert_eq!(read_events(&dir, at).0, vec![Event::Asked("half".into())]);
    }

    #[test]
    fn a_bad_input_is_an_error_not_a_checkpoint() {
        let tree = TempTree::new("review-bad", &[]);
        let dir = tree.0.join("session");
        assert!(hook("pre", "not json", &dir).is_err());
        assert!(hook("pre", r#"{"tool_name":"Edit","tool_input":{}}"#, &dir).is_err());
        assert!(hook("later", "{}", &dir).is_err());
        assert!(files(&dir).is_empty());
    }

    #[test]
    fn prune_removes_only_old_sessions() {
        let tree = TempTree::new("review-prune", &[("fresh/events", "")]);
        assert_eq!(prune(&tree.0, MAX_AGE), 0);
        assert_eq!(prune(&tree.0, std::time::Duration::ZERO), 1);
        assert!(!tree.0.join("fresh").exists());
    }

    #[test]
    fn claude_settings_quote_the_binary_for_the_shell() {
        let text = claude_settings(Path::new("/Applications/crc's.app/crc"));
        let value = crate::json::parse(&text).expect("json");
        let command = value
            .path("hooks.PreToolUse")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|m| m.path("hooks"))
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|h| h.get("command"))
            .and_then(|v| v.as_str())
            .expect("command");
        assert_eq!(command, r"'/Applications/crc'\''s.app/crc' --hook pre");
    }

    /// A session holding `name` as `before`, with the agent's `after` on
    /// disk.
    fn reviewed(tree: &TempTree, name: &str, before: &str, after: &str) -> (PathBuf, File) {
        let dir = tree.0.join("session");
        let path = tree.0.join(name);
        std::fs::write(&path, before).expect("write");
        let path = crate::platform::canonical(&path);
        hook("pre", &pre(&path), &dir).expect("hook");
        std::fs::write(&path, after).expect("agent writes");
        let file = files(&dir).remove(0);
        (dir, file)
    }

    const BEFORE: &str = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n";
    const AFTER: &str = "a\nB\nc\nd\ne\nf\ng\nh\ni\nj\nk\nL\n";

    #[test]
    fn keep_one_change_then_undo_the_other() {
        let tree = TempTree::new("review-keep-undo", &[]);
        let (dir, file) = reviewed(&tree, "f.txt", BEFORE, AFTER);

        keep(&dir, &file, 0, fingerprint(AFTER)).expect("keep");
        let file = files(&dir).remove(0);
        let (old, new) = texts(&file).expect("texts");
        assert_eq!(crate::ide::diff::diff(&old, &new).hunks.len(), 1);

        let text = undone(&file, 0, fingerprint(&new)).expect("undo");
        assert_eq!(text, "a\nB\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n");
        write_undone(&dir, &file, &text).expect("write");
        assert_eq!(std::fs::read_to_string(&file.path).expect("read"), text);
        assert!(files(&dir).is_empty(), "nothing left to review");
    }

    #[test]
    fn a_file_changed_since_it_was_shown_is_refused() {
        let tree = TempTree::new("review-stale", &[]);
        let (dir, file) = reviewed(&tree, "f.txt", BEFORE, AFTER);
        std::fs::write(&file.path, "moved on\n").expect("write");
        assert!(keep(&dir, &file, 0, fingerprint(AFTER)).is_err());
        assert!(undone(&file, 0, fingerprint(AFTER)).is_err());
        assert_eq!(files(&dir).len(), 1);
    }

    #[test]
    fn undo_all_restores_or_removes() {
        let tree = TempTree::new("review-undo-all", &[]);
        let (dir, file) = reviewed(&tree, "f.txt", BEFORE, AFTER);
        undo_all(&dir, &file).expect("undo all");
        assert_eq!(std::fs::read_to_string(&file.path).expect("read"), BEFORE);

        let created = tree.0.join("new.txt");
        hook("pre", &pre(&created), &dir).expect("hook");
        std::fs::write(&created, "made by the agent\n").expect("write");
        let file = files(&dir).remove(0);
        assert_eq!(file.checkpoint, Checkpoint::Absent);
        undo_all(&dir, &file).expect("undo all");
        assert!(!created.exists());
        assert!(files(&dir).is_empty());
    }

    #[test]
    fn keeping_a_new_file_takes_it_out_of_the_session() {
        let tree = TempTree::new("review-new-keep", &[]);
        let dir = tree.0.join("session");
        let created = tree.0.join("new.txt");
        hook("pre", &pre(&created), &dir).expect("hook");
        std::fs::write(&created, AFTER).expect("write");
        let file = files(&dir).remove(0);
        // One hunk from empty: keeping it is keeping the whole file.
        keep(&dir, &file, 0, fingerprint(AFTER)).expect("keep");
        assert!(files(&dir).is_empty());
    }

    #[test]
    fn keep_all_forgets_and_leaves_the_file() {
        let tree = TempTree::new("review-keep-all", &[]);
        let (dir, file) = reviewed(&tree, "f.txt", BEFORE, AFTER);
        keep_all(&dir, &file).expect("keep all");
        assert!(files(&dir).is_empty());
        assert_eq!(std::fs::read_to_string(&file.path).expect("read"), AFTER);
        assert!(
            std::fs::read_dir(dir.join("files"))
                .expect("dir")
                .next()
                .is_none()
        );
    }
}
