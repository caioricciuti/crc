//! Agent review sessions: which files an agent in a crc terminal changed,
//! and each one as it was before the agent's first write to it.
//!
//! Every terminal crc starts gets its own folder under
//! `Application Support/crc/sessions`, named in `CRC_SESSION_DIR`. The
//! agent's hook (`crc --hook pre`, run by Claude Code before Write, Edit,
//! MultiEdit and NotebookEdit) copies the file into it before the write
//! lands, so there is no race with the watcher, and appends a line to its
//! `events` file. The window reads new lines when the terminal prints.
//! After the write, `crc --hook post` records the file as the agent left
//! it, which is what Undo checks before it puts anything back.
//!
//! Layout of a session folder:
//!
//! - `files/<key>.path`: the file's path, created exclusively, so the
//!   first write to a path claims its one checkpoint;
//! - `files/<key>`: its text before that write, or `files/<key>.absent`
//!   when it did not exist, or `files/<key>.large` when it was too big to
//!   keep;
//! - `files/<key>.unhooked`: for a file changed without the hook (a shell
//!   command) while the session was working, Git's staged copy as the only
//!   baseline, or `files/<key>.untracked` when Git has none. No copy from
//!   before the change exists, so such a file can be kept, never undone;
//! - `files/<key>.after`: the file's size and a hash of its bytes as the
//!   agent last left it, from the post hook; `files/<key>.pending` while a
//!   write is between the two hooks. Undo refuses a file that does not
//!   match its `.after`, or has none: the person changed it since, or the
//!   agent may still be writing it;
//! - `events`: one line per hook call, appended.
//!
//! One file, two sessions: a hook that finds the path claimed by another
//! session under the same root writes nothing of its own for it. The file
//! is listed once, under the session that first changed it, with the one
//! copy from before any agent touched it.
//!
//! A session's folder goes when its terminal closes, moved aside as
//! `<id>.gone` and removed on a thread; a terminal a helper holds across
//! windows keeps its folder. [`prune`] at launch removes what that missed.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The variable that tells the hook where its session folder is.
pub const SESSION_VAR: &str = "CRC_SESSION_DIR";

/// Files bigger than this are recorded but not copied: they can be kept,
/// never undone.
pub const MAX_CHECKPOINT: u64 = 8 << 20;

/// How long a session whose terminal is gone (a crash, a window that
/// never came back) stays on disk.
pub const MAX_AGE: std::time::Duration = std::time::Duration::from_secs(2 * 24 * 60 * 60);

/// Where every session folder lives.
pub fn root() -> Option<PathBuf> {
    Some(crate::platform::app_support()?.join("sessions"))
}

/// A folder name no other terminal, in this run or an earlier one, has.
pub fn new_id() -> String {
    // The clock reads in whole microseconds here, so two ids made in the
    // same one (two sessions started together, two tests in parallel)
    // were the same id, and the second session's helper took the first's
    // socket. A count in this process tells them apart.
    static MADE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = MADE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{nanos:x}-{}-{n}", std::process::id())
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
    /// Changed without the hook: Git's staged copy to compare with, when
    /// there is one. Never undone, since it is not the file as it was.
    Unhooked(Option<PathBuf>),
}

impl Checkpoint {
    /// Whether Undo may put the file back from it.
    pub fn undoable(&self) -> bool {
        matches!(self, Checkpoint::Text(_) | Checkpoint::Absent)
    }
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
    /// A task on the session's shared list (agent teams), made or
    /// finished, with the teammate that did it when one is named.
    Task {
        id: String,
        subject: String,
        done: bool,
        teammate: Option<String>,
    },
    /// A teammate has nothing left to do.
    TeammateIdle(String),
    /// An API error ended the turn: Claude's StopFailure hook, with the
    /// error's kind (`overloaded`, `rate_limit`, ...) and what it said.
    ApiError { error: String, details: String },
    /// The person sent the agent a prompt: Claude's UserPromptSubmit
    /// hook, with the prompt's first line, for the terminal's marks.
    Prompted(String),
    /// The agent ran a shell command, and this is how it ended: Claude's
    /// PostToolUse or PostToolUseFailure hook on Bash, for the receipt.
    Ran(crate::project::receipt::Ran),
}

/// The most of a prompt's first line an event keeps.
const PROMPT_CHARS: usize = 200;

/// What the Stop hook asks: the turn is over, the agent waits.
pub const FINISHED: &str = "finished";

/// What `crc --hook <kind>` does with the hook's JSON on stdin, for the
/// session in `dir`. `pre` checkpoints the file a tool is about to write,
/// `post` records the file as the tool left it, `notify` records the
/// agent's question, `stop-failure` the API error that ended the turn,
/// `prompt` the first line of what the person sent. The event appended to
/// the session's `events`, when the hook is one that is.
pub fn hook(kind: &str, input: &str, dir: &Path) -> Result<Option<Event>, String> {
    let value = crate::json::parse(input).map_err(|e| format!("hook input: {e}"))?;
    let written = || -> Result<(String, PathBuf), String> {
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
        Ok((tool, resolve(&path)))
    };
    let event = match kind {
        "pre" => {
            let (tool, path) = written()?;
            let said = |e: std::io::Error| format!("{}: {e}", path.display());
            let holder = holder(dir, &path);
            checkpoint(&holder, &path).map_err(said)?;
            mark_pending(&holder, &path).map_err(said)?;
            Event::Wrote { tool, path }
        }
        "post" => {
            let (_, path) = written()?;
            let holder = holder(dir, &path);
            if has(&holder, &path) {
                record_after(&holder, &path).map_err(|e| format!("{}: {e}", path.display()))?;
            }
            return Ok(None);
        }
        "notify" => Event::Asked(
            value
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned(),
        ),
        "stop" => Event::Asked(FINISHED.into()),
        "stop-failure" => {
            let text = |name: &str| {
                value
                    .get(name)
                    .and_then(|v| v.as_str())
                    .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
                    .filter(|s| !s.is_empty())
            };
            Event::ApiError {
                error: text("error").unwrap_or_else(|| "unknown".into()),
                details: text("error_details")
                    .or_else(|| text("last_assistant_message"))
                    .unwrap_or_default(),
            }
        }
        "task-created" | "task-completed" => {
            let text = |name: &str| {
                value
                    .get(name)
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            };
            Event::Task {
                id: text("task_id").ok_or("no task_id in the hook input")?,
                subject: text("task_subject").unwrap_or_default(),
                done: kind == "task-completed",
                teammate: text("teammate_name"),
            }
        }
        // Its first line, taken before the line breaks become spaces.
        "prompt" => Event::Prompted(
            value
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("")
                .chars()
                .take(PROMPT_CHARS)
                .collect(),
        ),
        "ran" | "ran-failed" => {
            use crate::project::receipt::{Outcome, Ran, clean_command, failure_outcome};
            let command = value
                .path("tool_input.command")
                .and_then(|v| v.as_str())
                .ok_or("no command in the tool input")?;
            let outcome = if kind == "ran-failed" {
                let error = value.get("error").and_then(|v| v.as_str()).unwrap_or("");
                let interrupted = value.get("is_interrupt").and_then(|v| v.as_bool()) == Some(true);
                failure_outcome(error, interrupted)
            } else if value
                .path("tool_input.run_in_background")
                .and_then(|v| v.as_bool())
                == Some(true)
            {
                Outcome::Background
            } else {
                Outcome::Passed
            };
            Event::Ran(Ran {
                at: crate::platform::unix_seconds(),
                outcome,
                command: clean_command(command),
            })
        }
        "teammate-idle" => Event::TeammateIdle(
            value
                .get("teammate_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned(),
        ),
        other => return Err(format!("unknown hook {other:?}")),
    };
    append(dir, &event).map_err(|e| format!("events: {e}"))?;
    Ok(Some(event))
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

/// The session that holds `path`: `dir` when it has the file or no other
/// session under the same root does, else that other session. Two agents
/// on one file share the one checkpoint, from before either touched it.
fn holder(dir: &Path, path: &Path) -> PathBuf {
    if has(dir, path) {
        return dir.to_path_buf();
    }
    dir.parent()
        .map(sessions)
        .unwrap_or_default()
        .into_iter()
        .find(|other| other != dir && has(other, path))
        .unwrap_or_else(|| dir.to_path_buf())
}

/// Git metadata is never an agent working-file change, including in child
/// repositories and linked-worktree .git files.
fn git_metadata(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == ".git")
}

/// Keeps `path` as it is now, unless this session already has it.
pub fn checkpoint(dir: &Path, path: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    if git_metadata(path) {
        return Ok(());
    }
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

/// Records `path` as changed without the hook, with Git's staged copy as
/// its baseline (`None` for a file Git does not track). False when the
/// session already has the file, hooked or not.
pub fn note_unhooked(dir: &Path, path: &Path, staged: Option<&[u8]>) -> std::io::Result<bool> {
    use std::os::unix::ffi::OsStrExt;
    if git_metadata(path) {
        return Ok(false);
    }
    let files = dir.join("files");
    std::fs::create_dir_all(&files)?;
    let key = key(path);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(files.join(format!("{key}.path")))
    {
        Ok(mut claim) => claim.write_all(path.as_os_str().as_bytes())?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(e),
    }
    match staged {
        Some(bytes) => {
            let partial = files.join(format!("{key}.partial"));
            std::fs::write(&partial, bytes)?;
            std::fs::rename(partial, files.join(format!("{key}.unhooked")))?;
        }
        None => std::fs::write(files.join(format!("{key}.untracked")), "")?,
    }
    Ok(true)
}

/// Whether the session has `path` at all.
pub fn has(dir: &Path, path: &Path) -> bool {
    dir.join("files")
        .join(format!("{}.path", key(path)))
        .exists()
}

/// When the agent last wrote `file`, as the store saw it: the post hook's
/// record, or the claim for a file with none (changed without the hook).
/// `None` for a file the session does not have.
pub fn wrote_at(dir: &Path, file: &File) -> Option<std::time::SystemTime> {
    let files = dir.join("files");
    let key = key(&file.path);
    [format!("{key}.after"), format!("{key}.path")]
        .iter()
        .filter_map(|name| std::fs::metadata(files.join(name)).ok()?.modified().ok())
        .max()
}

/// The file's size and a hash of its bytes, as the post hook records them
/// and Undo checks them; `absent` for a file that is not there.
fn file_fingerprint(path: &Path) -> std::io::Result<String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(format!(
            "{} {:016x}",
            bytes.len(),
            crate::platform::fnv1a(&bytes)
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("absent".into()),
        Err(e) => Err(e),
    }
}

/// The agent is about to write `path`: what the file is until the post
/// hook runs is nobody's to undo.
fn mark_pending(dir: &Path, path: &Path) -> std::io::Result<()> {
    std::fs::write(dir.join("files").join(format!("{}.pending", key(path))), "")
}

/// Records `path` as the agent left it, in place of the pending mark.
fn record_after(dir: &Path, path: &Path) -> std::io::Result<()> {
    let files = dir.join("files");
    let key = key(path);
    let after = file_fingerprint(path)?;
    crate::platform::write_atomically(&files.join(format!("{key}.after")), after.as_bytes())?;
    match std::fs::remove_file(files.join(format!("{key}.pending"))) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Whether [`undo_all`] would put the file back now, before anything is:
/// for a session undone whole, where one file refused must stop the rest.
pub fn undoable_now(dir: &Path, file: &File) -> Result<(), String> {
    if file.checkpoint.undoable() {
        as_agent_left(dir, file)
    } else {
        Ok(())
    }
}

/// Whether the file is as the agent left it, so Undo may put it back:
/// an error naming why not when it is not, or when nothing says.
fn as_agent_left(dir: &Path, file: &File) -> Result<(), String> {
    let name = file
        .path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let files = dir.join("files");
    let key = key(&file.path);
    // Between the two hooks, or a `claude` started with settings from
    // before the post hook existed: either way, what the agent left is not
    // known.
    if files.join(format!("{key}.pending")).exists() {
        return Err(format!(
            "{name}: the agent's write was not seen to finish: undo by hand"
        ));
    }
    let Ok(after) = std::fs::read_to_string(files.join(format!("{key}.after"))) else {
        return Err(format!(
            "{name}: how the agent left it was not recorded: undo by hand"
        ));
    };
    let now = file_fingerprint(&file.path).map_err(|e| format!("{name}: {e}"))?;
    if now != after {
        return Err(format!(
            "{name} changed since the agent wrote it: undo by hand"
        ));
    }
    Ok(())
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
            if git_metadata(&path) {
                return None;
            }
            let text = files.join(key);
            let unhooked = files.join(format!("{key}.unhooked"));
            let checkpoint = if text.is_file() {
                Checkpoint::Text(text)
            } else if unhooked.is_file() {
                Checkpoint::Unhooked(Some(unhooked))
            } else if files.join(format!("{key}.untracked")).exists() {
                Checkpoint::Unhooked(None)
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

/// How many files [`files`] lists, from one listing of the folder: a claim
/// counts when its copy or one of its markers is there by name. No checkpoint
/// contents are read. Claims are read to exclude legacy Git metadata
/// without deleting any persisted records.
pub fn count(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir.join("files")) else {
        return 0;
    };
    let names: std::collections::HashSet<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names
        .iter()
        .filter_map(|name| name.strip_suffix(".path"))
        .filter(|key| {
            let Ok(bytes) = std::fs::read(dir.join("files").join(format!("{key}.path"))) else {
                return false;
            };
            if git_metadata(&crate::platform::path_from_bytes(bytes)) {
                return false;
            }
            ["", ".unhooked", ".untracked", ".absent", ".large"]
                .iter()
                .any(|marker| names.contains(&format!("{key}{marker}")))
        })
        .count()
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
        Event::Task {
            id,
            subject,
            done,
            teammate,
        } => format!(
            "task\t{}\t{}\t{}\t{}\n",
            clean(id),
            if *done { "done" } else { "open" },
            clean(teammate.as_deref().unwrap_or("")),
            clean(subject)
        ),
        Event::TeammateIdle(name) => format!("idle\t{}\n", clean(name)),
        Event::ApiError { error, details } => {
            format!("apierror\t{}\t{}\n", clean(error), clean(details))
        }
        Event::Prompted(text) => format!("prompted\t{}\n", clean(text)),
        Event::Ran(ran) => format!(
            "ran\t{}\t{}\t{}\n",
            ran.at,
            ran.outcome.code(),
            clean(&ran.command)
        ),
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
    let events = text.lines().filter_map(parse_event).collect();
    (events, from + end as u64 + 1)
}

/// One line of the events file; `None` for a line this build cannot read.
fn parse_event(line: &str) -> Option<Event> {
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
        "task" => {
            let mut parts = rest.splitn(4, '\t');
            let id = parts.next()?.to_owned();
            let done = parts.next()? == "done";
            let teammate = parts.next().filter(|t| !t.is_empty()).map(str::to_owned);
            let subject = parts.next().unwrap_or("").to_owned();
            Event::Task {
                id,
                subject,
                done,
                teammate,
            }
        }
        "idle" => Event::TeammateIdle(rest.to_owned()),
        "prompted" => Event::Prompted(rest.to_owned()),
        "ran" => {
            let mut parts = rest.splitn(3, '\t');
            let at = parts.next()?.parse().ok()?;
            let outcome = crate::project::receipt::Outcome::from_code(parts.next()?)?;
            Event::Ran(crate::project::receipt::Ran {
                at,
                outcome,
                command: parts.next()?.to_owned(),
            })
        }
        "apierror" => {
            let (error, details) = rest.split_once('\t').unwrap_or((rest, ""));
            Event::ApiError {
                error: error.to_owned(),
                details: details.to_owned(),
            }
        }
        _ => return None,
    })
}

/// The commands a session's events record before byte `until`: the
/// receipt of a terminal that comes back with its events already read.
pub fn receipt_until(dir: &Path, until: u64) -> crate::project::receipt::Receipt {
    let mut receipt = crate::project::receipt::Receipt::default();
    let Ok(bytes) = std::fs::read(dir.join("events")) else {
        return receipt;
    };
    let end = usize::try_from(until)
        .unwrap_or(usize::MAX)
        .min(bytes.len());
    for line in String::from_utf8_lossy(&bytes[..end]).lines() {
        if let Some(Event::Ran(ran)) = parse_event(line) {
            receipt.note(ran);
        }
    }
    receipt
}

/// The suffix of a session folder moved aside to be removed.
const GONE: &str = ".gone";

/// Removes session folders untouched for longer than `max_age`, except
/// those in `keep` (the sessions of terminals still held), and whatever
/// [`remove`] moved aside and did not finish; says how many went. A
/// folder's time is its newest event or checkpoint.
pub fn prune(root: &Path, max_age: std::time::Duration, keep: &[PathBuf]) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let now = std::time::SystemTime::now();
    let mut removed = 0;
    for entry in entries.filter_map(Result::ok) {
        let dir = entry.path();
        if keep.contains(&dir) {
            continue;
        }
        let newest = [dir.join("events"), dir.join("files"), dir.clone()]
            .iter()
            .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
            .max();
        let old = newest
            .and_then(|at| now.duration_since(at).ok())
            .is_some_and(|age| age >= max_age);
        if (old || is_gone(&dir)) && std::fs::remove_dir_all(&dir).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Removes every session folder but those in `keep`, whatever its age:
/// for the sessions of terminals closed before closing one removed its
/// folder. Says how many went.
pub fn prune_closed(root: &Path, keep: &[PathBuf]) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|dir| dir.is_dir() && !keep.contains(dir))
        .filter(|dir| std::fs::remove_dir_all(dir).is_ok())
        .count()
}

/// Whether a folder is one [`remove`] moved aside.
fn is_gone(dir: &Path) -> bool {
    dir.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(GONE))
}

/// Removes the session: its terminal closed. Moved aside first, so it
/// leaves every listing at once, then removed on a thread, since a session
/// can hold thousands of files.
pub fn remove(dir: &Path) {
    let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    if is_gone(dir) || !dir.is_dir() {
        return;
    }
    let gone = dir.with_file_name(format!("{name}{GONE}"));
    if std::fs::rename(dir, &gone).is_err() {
        return;
    }
    std::thread::spawn(move || {
        let _ = std::fs::remove_dir_all(gone);
    });
}

/// Quoted for `sh`: the whole string, literally.
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Claude Code settings that run this binary's hooks, for `claude
/// --settings`: with `review` every hook, without it only StopFailure, for
/// auto-continue, so no file is checkpointed. Claude merges hooks from
/// every settings source, so the person's own hooks still run.
pub fn claude_settings(exe: &Path, review: bool) -> String {
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
    // An API error that ended the turn, of any kind: which are worth
    // another try is decided here, not by a matcher.
    let failure = (
        "StopFailure",
        Value::Array(vec![object([("hooks", command("stop-failure"))])]),
    );
    if !review {
        return crate::json::pretty(&object([("hooks", object([failure]))]));
    }
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
            // The write done: the file as the agent left it, which is what
            // Undo checks against. And every shell command and how it
            // ended, for the receipt: a success has no exit code, a
            // failure says it in its error.
            (
                "PostToolUse",
                Value::Array(vec![
                    object([
                        ("matcher", string("Write|Edit|MultiEdit|NotebookEdit")),
                        ("hooks", command("post")),
                    ]),
                    object([("matcher", string("Bash")), ("hooks", command("ran"))]),
                ]),
            ),
            (
                "PostToolUseFailure",
                Value::Array(vec![object([
                    ("matcher", string("Bash")),
                    ("hooks", command("ran-failed")),
                ])]),
            ),
            (
                "Notification",
                Value::Array(vec![object([("hooks", command("notify"))])]),
            ),
            // The end of a turn is the agent waiting on the person too: the
            // Agents sidebar marks it until someone types.
            (
                "Stop",
                Value::Array(vec![object([("hooks", command("stop"))])]),
            ),
            // Agent teams (CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS): the
            // lead's task list and its teammates, for the Agents sidebar.
            (
                "TaskCreated",
                Value::Array(vec![object([("hooks", command("task-created"))])]),
            ),
            (
                "TaskCompleted",
                Value::Array(vec![object([("hooks", command("task-completed"))])]),
            ),
            (
                "TeammateIdle",
                Value::Array(vec![object([("hooks", command("teammate-idle"))])]),
            ),
            // What the person sent, for the terminal's prompt marks.
            (
                "UserPromptSubmit",
                Value::Array(vec![object([("hooks", command("prompt"))])]),
            ),
            failure,
        ]),
    )]);
    crate::json::pretty(&settings)
}

/// Writes [`claude_settings`] for the running binary where `claude
/// --settings` reads it, and returns that path quoted for the shell. Each
/// kind has its own file: a session started with one keeps it when the
/// setting changes.
pub fn claude_settings_arg(review: bool) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let name = if review {
        "claude-hooks.json"
    } else {
        "claude-hooks-continue.json"
    };
    let path = crate::platform::app_support()?.join(name);
    let text = claude_settings(&exe, review);
    if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::write(&path, text).ok()?;
    }
    Some(shell_quote(&path.to_string_lossy()))
}

/// Every session folder under `root`, less any being removed.
pub fn sessions(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir() && !is_gone(p))
        .collect()
}

/// How many files each session with something to review holds, with its
/// title: what Home lists, without reading any file's text.
pub fn summary(root: &Path) -> Vec<(PathBuf, String, usize)> {
    let mut out: Vec<(PathBuf, String, usize)> = sessions(root)
        .into_iter()
        .filter_map(|dir| {
            let n = count(&dir);
            (n > 0).then(|| {
                let title = about(&dir).0;
                (dir, title, n)
            })
        })
        .collect();
    out.sort();
    out
}

/// Records which tab and folder a session belongs to, once, so the review
/// can name it after the tab is gone.
pub fn describe(dir: &Path, title: &str, folder: &Path) {
    let path = dir.join("about");
    if path.exists() || !dir.is_dir() {
        return;
    }
    let clean = |s: &str| s.replace(['\n', '\r'], " ");
    let _ = std::fs::write(
        path,
        format!(
            "title={}\nfolder={}\n",
            clean(title),
            // Resolved, as the file paths are, so one is a prefix of the other.
            clean(&crate::platform::canonical(folder).to_string_lossy())
        ),
    );
}

/// The session's tab title and its folder, as [`describe`] wrote them;
/// "Terminal" and an empty path when it never did.
pub fn about(dir: &Path) -> (String, PathBuf) {
    let text = std::fs::read_to_string(dir.join("about")).unwrap_or_default();
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(str::to_owned)
    };
    let title = field("title").unwrap_or_else(|| "Terminal".into());
    (
        title,
        field("folder").map(PathBuf::from).unwrap_or_default(),
    )
}

/// What a review compares for one file: its checkpoint and its text now.
/// A file the agent deleted reads as empty; so does one that did not
/// exist before. Nothing over [`MAX_CHECKPOINT`] is read: a file that
/// grew past it since, or a staged copy that size, is kept or undone
/// whole, the way one too large to copy is.
pub fn texts(file: &File) -> Result<(String, String), String> {
    let too_large = "too large to review by change: keep or undo it whole";
    let read = |path: &Path, not_text: &str| match std::fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_CHECKPOINT => Err(too_large.to_owned()),
        Ok(_) => std::fs::read_to_string(path).map_err(|_| not_text.to_owned()),
        Err(e) => Err(e.to_string()),
    };
    let old = match &file.checkpoint {
        Checkpoint::Text(kept) => read(kept, "not text: keep or undo the whole file")?,
        Checkpoint::Absent | Checkpoint::Unhooked(None) => String::new(),
        Checkpoint::Unhooked(Some(staged)) => read(staged, "not text: keep the whole file")?,
        Checkpoint::TooLarge => {
            return Err("too large to review by change: keep it whole".into());
        }
    };
    let new = match std::fs::metadata(&file.path) {
        Ok(meta) if meta.len() > MAX_CHECKPOINT => return Err(too_large.into()),
        Ok(_) => std::fs::read_to_string(&file.path)
            .map_err(|_| "not text: keep or undo the whole file".to_owned())?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.to_string()),
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
    if !file.checkpoint.undoable() {
        return Err("this file can only be kept whole".into());
    }
    let (old, new) = current(file, seen)?;
    let kept = crate::ide::diff::keep_hunk(&old, &new, hunk).ok_or("that change is gone")?;
    if kept == new {
        return forget(dir, file);
    }
    set_checkpoint(dir, &file.path, &kept).map_err(|e| format!("checkpoint: {e}"))
}

/// The file's text with one change undone, for the caller to put in place:
/// through the open document when there is one, so it is one undo step,
/// or with [`write_undone`] when there is not. Refused, like [`undo_all`],
/// for a file that is not as the agent left it.
pub fn undone(dir: &Path, file: &File, hunk: usize, seen: u64) -> Result<String, String> {
    if !file.checkpoint.undoable() {
        return Err("no copy from before the change was kept to undo to".into());
    }
    as_agent_left(dir, file)?;
    let (old, new) = current(file, seen)?;
    crate::ide::diff::undo_hunk(&old, &new, hunk).ok_or_else(|| "that change is gone".into())
}

/// Writes [`undone`]'s text to the file, for a file not open in crc, and
/// takes the file out of the session when nothing is left to review. What
/// it wrote is the state the next undo checks against.
pub fn write_undone(dir: &Path, file: &File, text: &str) -> Result<(), String> {
    let said = |e: std::io::Error| format!("{}: {e}", file.path.display());
    std::fs::write(&file.path, text).map_err(said)?;
    record_after(dir, &file.path).map_err(said)?;
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
/// the file removed when the agent created it. Refused for a file that is
/// not as the agent left it: the person's edits since are not undone.
pub fn undo_all(dir: &Path, file: &File) -> Result<(), String> {
    let said = |e: std::io::Error| format!("{}: {e}", file.path.display());
    if file.checkpoint.undoable() {
        as_agent_left(dir, file)?;
    }
    match &file.checkpoint {
        Checkpoint::Text(kept) => {
            std::fs::copy(kept, &file.path).map_err(said)?;
        }
        Checkpoint::Absent => match std::fs::remove_file(&file.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(said(e)),
            _ => {}
        },
        Checkpoint::TooLarge => return Err("too large: no copy was kept to undo to".into()),
        Checkpoint::Unhooked(_) => {
            return Err("no copy from before the change was kept to undo to".into());
        }
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
    for suffix in [
        "",
        ".absent",
        ".large",
        ".unhooked",
        ".untracked",
        ".partial",
        ".after",
        ".pending",
        ".path",
    ] {
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

    #[test]
    fn ids_made_at_once_differ() {
        let ids: Vec<String> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..4)
                .map(|_| scope.spawn(|| (0..500).map(|_| new_id()).collect::<Vec<_>>()))
                .collect();
            workers
                .into_iter()
                .flat_map(|w| w.join().unwrap())
                .collect()
        });
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
    }
    use crate::project::TempTree;

    #[test]
    fn metadata_is_excluded_from_new_and_persisted_reviews() {
        use std::os::unix::ffi::OsStrExt;
        let tree = TempTree::new("review-metadata", &[("repo/notes.txt", "notes")]);
        let dir = tree.0.join("session");
        let normal = tree.0.join("repo/notes.txt");
        assert!(note_unhooked(&dir, &normal, None).unwrap());
        for name in [
            "repo/.git",
            "repo/.git/index",
            "nested/repo/.git/objects/ab/cd",
        ] {
            let path = tree.0.join(name);
            assert!(!note_unhooked(&dir, &path, None).unwrap());
            checkpoint(&dir, &path).unwrap();
            assert!(!has(&dir, &path));
            // Simulate records collected by an older installed build.
            let files = dir.join("files");
            let key = key(&path);
            std::fs::write(
                files.join(format!("{key}.path")),
                path.as_os_str().as_bytes(),
            )
            .unwrap();
            std::fs::write(files.join(format!("{key}.untracked")), "").unwrap();
        }
        assert_eq!(files(&dir).len(), 1);
        assert_eq!(files(&dir)[0].path, normal);
        assert_eq!(count(&dir), 1);
        assert_eq!(std::fs::read_dir(dir.join("files")).unwrap().count(), 8);
    }

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
    fn a_file_grown_past_the_limit_is_not_read() {
        let tree = TempTree::new("review-grown", &[("small.txt", "one\n")]);
        let dir = tree.0.join("session");
        let path = crate::platform::canonical(&tree.0.join("small.txt"));
        hook("pre", &pre(&path), &dir).expect("hook");
        let file = files(&dir).remove(0);
        assert_eq!(texts(&file), Ok(("one\n".to_owned(), "one\n".to_owned())));
        std::fs::write(&path, vec![b'x'; MAX_CHECKPOINT as usize + 1]).expect("grow");
        hook("post", &pre(&path), &dir).expect("hook");
        assert_eq!(
            texts(&file),
            Err("too large to review by change: keep or undo it whole".to_owned())
        );
        // What was kept still puts it back whole.
        undo_all(&dir, &file).expect("undo");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "one\n");
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
            Some(Event::Wrote {
                tool: "Write".into(),
                path: b.clone()
            })
        );
        // The write done, the post hook records the file as the agent left
        // it, and says nothing in the events.
        assert_eq!(hook("post", &input, &dir).expect("hook"), None);
        assert_eq!(read_events(&dir, 0).0.len(), 1);
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
    fn team_hooks_record_tasks_and_idle_teammates() {
        let tree = TempTree::new("review-team", &[]);
        let dir = tree.0.join("session");
        hook(
            "task-created",
            r#"{"task_id":"1","task_subject":"Add\tlogin","teammate_name":"impl"}"#,
            &dir,
        )
        .expect("hook");
        hook(
            "task-completed",
            r#"{"task_id":"1","task_subject":"Add login"}"#,
            &dir,
        )
        .expect("hook");
        hook("teammate-idle", r#"{"teammate_name":"impl"}"#, &dir).expect("hook");
        assert!(hook("task-created", r#"{"task_subject":"no id"}"#, &dir).is_err());
        let (events, _) = read_events(&dir, 0);
        assert_eq!(
            events,
            vec![
                Event::Task {
                    id: "1".into(),
                    subject: "Add login".into(),
                    done: false,
                    teammate: Some("impl".into()),
                },
                Event::Task {
                    id: "1".into(),
                    subject: "Add login".into(),
                    done: true,
                    teammate: None,
                },
                Event::TeammateIdle("impl".into()),
            ]
        );
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
        let tree = TempTree::new(
            "review-prune",
            &[
                ("fresh/events", ""),
                ("held/events", ""),
                ("left.gone/events", ""),
            ],
        );
        let held = tree.0.join("held");
        assert_eq!(
            prune(&tree.0, MAX_AGE, &[]),
            1,
            "a removal left behind goes"
        );
        assert!(!tree.0.join("left.gone").exists());
        // A session whose terminal is still held is never aged out.
        assert_eq!(
            prune(
                &tree.0,
                std::time::Duration::ZERO,
                std::slice::from_ref(&held)
            ),
            1
        );
        assert!(!tree.0.join("fresh").exists());
        assert!(held.is_dir());
    }

    #[test]
    fn claude_settings_quote_the_binary_for_the_shell() {
        let text = claude_settings(Path::new("/Applications/crc's.app/crc"), true);
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
        for (event, kind) in [
            ("PostToolUse", "post"),
            ("Notification", "notify"),
            ("Stop", "stop"),
            ("TaskCreated", "task-created"),
            ("TaskCompleted", "task-completed"),
            ("TeammateIdle", "teammate-idle"),
            ("UserPromptSubmit", "prompt"),
            ("StopFailure", "stop-failure"),
        ] {
            let command = value
                .path(&format!("hooks.{event}"))
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|m| m.path("hooks"))
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|h| h.get("command"))
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("{event}"));
            assert!(
                command.ends_with(&format!(" --hook {kind}")),
                "{event}: {command}"
            );
        }
    }

    #[test]
    fn without_review_only_the_failure_hook_is_installed() {
        let text = claude_settings(Path::new("/usr/local/bin/crc"), false);
        let value = crate::json::parse(&text).expect("json");
        let hooks = value.get("hooks").expect("hooks");
        assert!(hooks.get("PreToolUse").is_none());
        assert!(hooks.get("Stop").is_none());
        assert!(hooks.get("UserPromptSubmit").is_none());
        let failure = hooks
            .get("StopFailure")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .expect("StopFailure");
        assert!(failure.get("matcher").is_none(), "every error, sorted here");
        let command = failure
            .path("hooks")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|h| h.get("command"))
            .and_then(|v| v.as_str())
            .expect("command");
        assert_eq!(command, "'/usr/local/bin/crc' --hook stop-failure");
    }

    #[test]
    fn the_failure_hook_records_the_api_error() {
        let tree = TempTree::new("review-stop-failure", &[]);
        let dir = tree.0.join("session");
        let event = hook(
            "stop-failure",
            r#"{"hook_event_name":"StopFailure","error":"server_error","error_details":"API Error: 500\n\t{\"type\":\"api_error\"}","last_assistant_message":"ignored"}"#,
            &dir,
        )
        .expect("hook");
        let said = Event::ApiError {
            error: "server_error".into(),
            details: r#"API Error: 500 {"type":"api_error"}"#.into(),
        };
        assert_eq!(event, Some(said.clone()));
        // No details: the last message says it. No error named: unknown.
        hook(
            "stop-failure",
            r#"{"last_assistant_message":"Connection dropped"}"#,
            &dir,
        )
        .expect("hook");
        hook("stop-failure", r#"{"error":"rate_limit"}"#, &dir).expect("hook");
        let (events, _) = read_events(&dir, 0);
        assert_eq!(
            events,
            vec![
                said,
                Event::ApiError {
                    error: "unknown".into(),
                    details: "Connection dropped".into(),
                },
                Event::ApiError {
                    error: "rate_limit".into(),
                    details: String::new(),
                },
            ]
        );
    }

    #[test]
    fn the_prompt_hook_records_the_first_line() {
        let tree = TempTree::new("review-prompt", &[]);
        let dir = tree.0.join("session");
        let event = hook("prompt", r#"{"prompt":"a\nb"}"#, &dir).expect("hook");
        assert_eq!(event, Some(Event::Prompted("a".into())));
        // Blank lines first are not the prompt; a tab is kept as a space.
        hook("prompt", r#"{"prompt":"\n  fix\tit  \nmore"}"#, &dir).expect("hook");
        hook(
            "prompt",
            &format!(r#"{{"prompt":"{}"}}"#, "x".repeat(500)),
            &dir,
        )
        .expect("hook");
        hook("prompt", "{}", &dir).expect("hook");
        let (events, _) = read_events(&dir, 0);
        assert_eq!(
            events,
            vec![
                Event::Prompted("a".into()),
                Event::Prompted("fix it".into()),
                Event::Prompted("x".repeat(PROMPT_CHARS)),
                Event::Prompted(String::new()),
            ]
        );
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
        hook("post", &pre(&path), &dir).expect("hook");
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

        let text = undone(&dir, &file, 0, fingerprint(&new)).expect("undo");
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
        assert!(undone(&dir, &file, 0, fingerprint(AFTER)).is_err());
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
        hook("post", &pre(&created), &dir).expect("hook");
        let file = files(&dir).remove(0);
        assert_eq!(file.checkpoint, Checkpoint::Absent);
        undo_all(&dir, &file).expect("undo all");
        assert!(!created.exists());
        assert!(files(&dir).is_empty());
    }

    #[test]
    fn undo_refuses_a_file_changed_since_the_agent_wrote_it() {
        let tree = TempTree::new("review-undo-guard", &[]);
        let (dir, file) = reviewed(&tree, "f.txt", BEFORE, AFTER);
        // The person edits the file after the agent: undo would lose that.
        let edited = "a\nB\nc\nd\ne\nf\ng\nh\ni\nj\nk\nL\nmine\n";
        std::fs::write(&file.path, edited).expect("edit");
        let said = undo_all(&dir, &file).expect_err("refused");
        assert!(said.contains("changed since the agent wrote it"), "{said}");
        assert!(
            undone(&dir, &file, 0, fingerprint(edited)).is_err(),
            "one change neither"
        );
        assert_eq!(std::fs::read_to_string(&file.path).expect("read"), edited);
        assert_eq!(files(&dir).len(), 1, "still under review");

        // A file the hook saw before a write but never after is unknown,
        // which is as unsafe: the agent may still be writing it.
        let g = tree.0.join("g.txt");
        std::fs::write(&g, BEFORE).expect("write");
        let g = crate::platform::canonical(&g);
        hook("pre", &pre(&g), &dir).expect("hook");
        std::fs::write(&g, AFTER).expect("agent writes");
        let file = files(&dir).into_iter().find(|f| f.path == g).expect("g");
        let said = undo_all(&dir, &file).expect_err("refused");
        assert!(said.contains("undo by hand"), "{said}");
        assert_eq!(std::fs::read_to_string(&g).expect("read"), AFTER);
    }

    #[test]
    fn an_unchanged_file_still_undoes() {
        let tree = TempTree::new("review-undo-same", &[]);
        let (dir, file) = reviewed(&tree, "f.txt", BEFORE, AFTER);
        // One change undone through the page, then the rest: what the page
        // wrote counts as the agent's state for the next undo.
        let text = undone(&dir, &file, 1, fingerprint(AFTER)).expect("undo one");
        write_undone(&dir, &file, &text).expect("write");
        assert_eq!(std::fs::read_to_string(&file.path).expect("read"), text);
        undo_all(&dir, &file).expect("undo all");
        assert_eq!(std::fs::read_to_string(&file.path).expect("read"), BEFORE);
        assert!(files(&dir).is_empty());

        // The agent writes the same file again in a later turn: the hooks
        // move the fingerprint along with it.
        let path = crate::platform::canonical(&tree.0.join("f.txt"));
        hook("pre", &pre(&path), &dir).expect("hook");
        std::fs::write(&path, AFTER).expect("agent writes");
        hook("post", &pre(&path), &dir).expect("hook");
        hook("pre", &pre(&path), &dir).expect("hook");
        std::fs::write(&path, "third\n").expect("agent writes");
        hook("post", &pre(&path), &dir).expect("hook");
        let file = files(&dir).remove(0);
        undo_all(&dir, &file).expect("undo all");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), BEFORE);
    }

    #[test]
    fn a_second_session_leaves_the_file_to_the_first() {
        let tree = TempTree::new("review-two-agents", &[("f.txt", BEFORE)]);
        let first = tree.0.join("session-a");
        let second = tree.0.join("session-b");
        let path = crate::platform::canonical(&tree.0.join("f.txt"));
        hook("pre", &pre(&path), &first).expect("hook");
        std::fs::write(&path, AFTER).expect("first agent writes");
        hook("post", &pre(&path), &first).expect("hook");
        // The second agent's hooks find the first's claim: no new baseline
        // from after the first agent's change, and its fingerprint lands
        // with the first session.
        hook("pre", &pre(&path), &second).expect("hook");
        std::fs::write(&path, "both\n").expect("second agent writes");
        hook("post", &pre(&path), &second).expect("hook");
        assert!(files(&second).is_empty(), "listed once, under the first");
        let file = files(&first).remove(0);
        let Checkpoint::Text(kept) = &file.checkpoint else {
            panic!("{:?}", file.checkpoint);
        };
        assert_eq!(std::fs::read_to_string(kept).expect("read"), BEFORE);
        undo_all(&first, &file).expect("undo all");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), BEFORE);
        // The second session's events still say it wrote.
        assert_eq!(read_events(&second, 0).0.len(), 1);
    }

    #[test]
    fn a_closed_session_is_removed_and_a_live_one_kept() {
        let tree = TempTree::new(
            "review-prune-closed",
            &[
                ("live/files/", ""),
                ("gone/files/", ""),
                ("old.gone/files/", ""),
            ],
        );
        let live = tree.0.join("live");
        assert_eq!(
            sessions(&tree.0).len(),
            2,
            "a folder being removed is not listed"
        );
        assert_eq!(prune_closed(&tree.0, std::slice::from_ref(&live)), 2);
        assert!(live.is_dir());
        assert!(!tree.0.join("gone").exists());
        assert!(!tree.0.join("old.gone").exists());
        // Removing a session takes it out of the listing at once.
        remove(&live);
        assert!(!live.exists());
        assert!(sessions(&tree.0).is_empty());
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while tree.0.join("live.gone").exists() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!tree.0.join("live.gone").exists());
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

    #[test]
    fn a_session_is_named_after_its_tab() {
        let tree = TempTree::new("review-about", &[("s/files/", "")]);
        let dir = tree.0.join("s");
        assert_eq!(about(&dir), ("Terminal".into(), PathBuf::new()));
        describe(&dir, "\u{273b} Claude", Path::new("/work/garden-log"));
        describe(&dir, "later", Path::new("/elsewhere"));
        assert_eq!(
            about(&dir),
            ("\u{273b} Claude".into(), PathBuf::from("/work/garden-log"))
        );
        assert!(summary(&tree.0).is_empty(), "no files, nothing to list");
    }

    #[test]
    fn a_file_changed_without_the_hook_can_only_be_kept() {
        let tree = TempTree::new("review-unhooked", &[("f.txt", AFTER), ("n.txt", "new\n")]);
        let dir = tree.0.join("session");
        let f = crate::platform::canonical(&tree.0.join("f.txt"));
        let n = crate::platform::canonical(&tree.0.join("n.txt"));
        assert!(note_unhooked(&dir, &f, Some(BEFORE.as_bytes())).expect("note"));
        assert!(note_unhooked(&dir, &n, None).expect("note"));
        assert!(
            !note_unhooked(&dir, &f, None).expect("note"),
            "once per file"
        );
        // A hooked file is never noted over.
        let h = tree.0.join("h.txt");
        hook("pre", &pre(&h), &dir).expect("hook");
        assert!(!note_unhooked(&dir, &resolve(&h), None).expect("note"));

        let files = files(&dir);
        let f = files.iter().find(|x| x.path == f).expect("f");
        let (old, new) = texts(f).expect("texts");
        assert_eq!((old.as_str(), new.as_str()), (BEFORE, AFTER));
        assert!(!f.checkpoint.undoable());
        assert!(undone(&dir, f, 0, fingerprint(AFTER)).is_err());
        assert!(undo_all(&dir, f).is_err());
        assert!(keep(&dir, f, 0, fingerprint(AFTER)).is_err());
        assert_eq!(std::fs::read_to_string(&f.path).expect("read"), AFTER);
        keep_all(&dir, f).expect("keep all");
        let n = files.iter().find(|x| x.path == n).expect("n");
        assert_eq!(n.checkpoint, Checkpoint::Unhooked(None));
        keep_all(&dir, n).expect("keep all");
        assert_eq!(super::files(&dir).len(), 1, "the hooked one is left");
    }

    /// A session folder with `n` claims of each kind, written straight,
    /// as the hook and the watcher leave them.
    fn mixed_session(root: &Path, n: usize) -> PathBuf {
        let dir = root.join("session");
        let files = dir.join("files");
        std::fs::create_dir_all(&files).expect("files");
        // A finished copy, a copy still being written, a file that did
        // not exist, one too large, and the two kinds the watcher notes.
        let kinds = [
            Some(""),
            None,
            Some(".absent"),
            Some(".large"),
            Some(".untracked"),
            Some(".unhooked"),
        ];
        for i in 0..n {
            for (k, suffix) in kinds.iter().enumerate() {
                let path = root.join(format!("f{k}-{i}.txt"));
                let key = key(&path);
                std::fs::write(
                    files.join(format!("{key}.path")),
                    path.to_string_lossy().as_bytes(),
                )
                .expect("claim");
                match suffix {
                    Some(s) => std::fs::write(files.join(format!("{key}{s}")), "x").expect("copy"),
                    None => {
                        std::fs::write(files.join(format!("{key}.partial")), "x").expect("partial")
                    }
                }
            }
        }
        dir
    }

    #[test]
    fn the_count_matches_the_files_listed() {
        let tree = TempTree::new("review-count", &[]);
        let dir = mixed_session(&tree.0, 40);
        assert_eq!(files(&dir).len(), 200, "the unfinished copies are left out");
        assert_eq!(count(&dir), files(&dir).len());
        assert_eq!(count(&tree.0.join("none")), 0);
        let summary = summary(&tree.0);
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].2, 200);
    }

    #[test]
    fn the_count_reads_no_checkpoint_contents() {
        use std::os::unix::fs::PermissionsExt;
        let tree = TempTree::new("review-count-unread", &[]);
        let dir = mixed_session(&tree.0, 1000);
        // Checkpoint contents need not be readable; only path claims are read.
        for entry in std::fs::read_dir(dir.join("files")).expect("list") {
            let path = entry.expect("entry").path();
            if path.extension().is_none_or(|e| e != "path") {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
                    .expect("chmod");
            }
        }
        let started = std::time::Instant::now();
        assert_eq!(count(&dir), 5000);
        assert_eq!(summary(&tree.0)[0].2, 5000);
        // Generous: a CI runner busy with the whole suite took over 500 ms.
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    /// The hook inputs as Claude Code sent them (2.1.314, probed
    /// 2026-10-30): a success through PostToolUse, with no exit code; a
    /// failure through PostToolUseFailure, its code in the error.
    #[test]
    fn the_bash_hooks_record_each_command_and_how_it_ended() {
        use crate::project::receipt::Outcome;
        let tree = TempTree::new("review-ran", &[]);
        let dir = tree.0.join("session");
        let ran = |kind: &str, input: &str| match hook(kind, input, &dir).expect("hook") {
            Some(Event::Ran(ran)) => (ran.outcome, ran.command),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            ran(
                "ran",
                r#"{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"echo ok","description":"Print ok"},"tool_response":{"stdout":"ok","stderr":"","interrupted":false}}"#
            ),
            (Outcome::Passed, "echo ok".into())
        );
        assert_eq!(
            ran(
                "ran-failed",
                r#"{"hook_event_name":"PostToolUseFailure","tool_name":"Bash","tool_input":{"command":"cargo test\n  --offline"},"error":"Exit code 101\nerror: 1 test failed","is_interrupt":false}"#
            ),
            (Outcome::Failed(101), "cargo test --offline".into())
        );
        assert_eq!(
            ran(
                "ran-failed",
                r#"{"tool_input":{"command":"sleep 99"},"error":"Interrupted","is_interrupt":true}"#
            )
            .0,
            Outcome::Interrupted
        );
        assert_eq!(
            ran(
                "ran",
                r#"{"tool_input":{"command":"npm run dev","run_in_background":true},"tool_response":{"backgroundTaskId":"b1"}}"#
            )
            .0,
            Outcome::Background
        );
        // No command: the hook says so, and records nothing.
        assert!(hook("ran", r#"{"tool_input":{}}"#, &dir).is_err());
        let (events, read) = read_events(&dir, 0);
        let outcomes: Vec<_> = events
            .iter()
            .map(|e| match e {
                Event::Ran(r) => (r.outcome, r.command.as_str()),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            outcomes,
            [
                (Outcome::Passed, "echo ok"),
                (Outcome::Failed(101), "cargo test --offline"),
                (Outcome::Interrupted, "sleep 99"),
                (Outcome::Background, "npm run dev"),
            ]
        );
        // A terminal back from its helper had read the first two.
        let first_two = std::fs::read_to_string(dir.join("events"))
            .expect("events")
            .lines()
            .take(2)
            .map(|l| l.len() as u64 + 1)
            .sum();
        let receipt = receipt_until(&dir, first_two);
        assert_eq!((receipt.total, receipt.failed), (2, 1));
        assert_eq!(receipt_until(&dir, read).total, 4);
    }

    #[test]
    fn the_review_settings_hook_every_shell_command() {
        let text = claude_settings(Path::new("/usr/local/bin/crc"), true);
        let value = crate::json::parse(&text).expect("json");
        let entries = |event: &str| -> Vec<(String, String)> {
            value
                .path(&format!("hooks.{event}"))
                .and_then(|v| v.as_array())
                .expect(event)
                .iter()
                .map(|m| {
                    let matcher = m.get("matcher").and_then(|v| v.as_str()).unwrap_or("");
                    let command = m
                        .path("hooks")
                        .and_then(|v| v.as_array())
                        .and_then(|a| a.first())
                        .and_then(|h| h.get("command"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    (matcher.to_owned(), command.to_owned())
                })
                .collect()
        };
        let crc = "'/usr/local/bin/crc' --hook";
        assert_eq!(
            entries("PostToolUse"),
            [
                (
                    "Write|Edit|MultiEdit|NotebookEdit".into(),
                    format!("{crc} post")
                ),
                ("Bash".into(), format!("{crc} ran")),
            ]
        );
        assert_eq!(
            entries("PostToolUseFailure"),
            [("Bash".into(), format!("{crc} ran-failed"))]
        );
    }
}
