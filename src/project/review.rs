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
//! it, which is what Undo checks before it puts anything back, and keeps
//! that text in local history as the session's version.
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
//! Its `events` and `about` stay, as the workspace's ledger: [`archive`]
//! moves them to `ledgers/<workspace>/<id>` first, outside `sessions`, and
//! [`prune_ledgers`] drops them after [`LEDGER_AGE`].

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

/// How long the ledger of a closed session stays on disk.
pub const LEDGER_AGE: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 60 * 60);

/// Where every session folder lives.
pub fn root() -> Option<PathBuf> {
    Some(crate::platform::app_support()?.join("sessions"))
}

/// Where the ledgers of closed sessions live, a folder per workspace.
pub fn ledgers_root() -> Option<PathBuf> {
    Some(crate::platform::app_support()?.join("ledgers"))
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
    /// What the agent said as its turn ended, in sentences: Claude's Stop
    /// hook, so the receipt can check its claims.
    Said(Vec<String>),
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
/// the session's `events`, when the hook is one that is. With `history`,
/// `post` also keeps the file's new text in local history, as the
/// session's version of it.
pub fn hook(
    kind: &str,
    input: &str,
    dir: &Path,
    history: Option<&crate::history::recorder::AgentHistory>,
) -> Result<Option<Event>, String> {
    let value = crate::json::parse(input).map_err(|e| format!("hook input: {e}"))?;
    note_claude_session(dir, &value);
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
            let kept = keep_version(history, dir, &path);
            let holder = holder(dir, &path);
            if has(&holder, &path) {
                record_after(&holder, &path).map_err(|e| format!("{}: {e}", path.display()))?;
            }
            kept?;
            return Ok(None);
        }
        "notify" => Event::Asked(
            value
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned(),
        ),
        // What the agent said goes first, so the receipt has checked it
        // by the time the turn shows as finished.
        "stop" => {
            let said = value
                .get("last_assistant_message")
                .and_then(|v| v.as_str())
                .map(crate::project::receipt::sentences)
                .unwrap_or_default();
            if !said.is_empty() {
                append(dir, &Event::Said(said)).map_err(|e| format!("events: {e}"))?;
            }
            Event::Asked(FINISHED.into())
        }
        "stop-failure" => {
            let text = |name: &str| {
                value
                    .get(name)
                    .and_then(|v| v.as_str())
                    .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
                    .filter(|s| !s.is_empty())
            };
            // Without details, what the agent last said stands in only
            // when it is the error itself ("API Error: ..."): its own
            // prose about a lost connection in the code it was working
            // on is not a reason to try again.
            Event::ApiError {
                error: text("error").unwrap_or_else(|| "unknown".into()),
                details: text("error_details")
                    .or_else(|| {
                        text("last_assistant_message")
                            .filter(|s| s.to_lowercase().starts_with("api error"))
                    })
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
            use crate::project::receipt::{
                Kind, Outcome, Ran, clean_command, failure_outcome, kinds_of, test_counts,
            };
            let command = value
                .path("tool_input.command")
                .and_then(|v| v.as_str())
                .ok_or("no command in the tool input")?;
            let text = |name: &str| value.path(name).and_then(|v| v.as_str()).unwrap_or("");
            // What the command printed, read for its test counts and then
            // dropped: output can hold secrets, so it is never kept.
            let output = if kind == "ran-failed" {
                text("error").to_owned()
            } else {
                format!(
                    "{}\n{}",
                    text("tool_response.stdout"),
                    text("tool_response.stderr")
                )
            };
            let outcome = if kind == "ran-failed" {
                let interrupted = value.get("is_interrupt").and_then(|v| v.as_bool()) == Some(true);
                failure_outcome(&output, interrupted)
            } else if value
                .path("tool_input.run_in_background")
                .and_then(|v| v.as_bool())
                == Some(true)
            {
                Outcome::Background
            } else {
                Outcome::Passed
            };
            // A run stopped part way, or still going, has no counts to
            // stand on.
            let finished = !matches!(outcome, Outcome::Background | Outcome::Interrupted);
            let tests = (finished && kinds_of(command).contains(&Kind::Tests))
                .then(|| test_counts(&output))
                .flatten();
            Event::Ran(Ran {
                at: crate::platform::unix_seconds(),
                outcome,
                command: clean_command(command),
                tests,
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
    // Only a write done has a version to keep, so only then the settings
    // are read.
    let history = (kind == "post")
        .then(crate::history::recorder::AgentHistory::from_settings)
        .flatten();
    let result = match read {
        Ok(_) => hook(kind, &input, &dir, history.as_ref()),
        Err(e) => Err(format!("hook input: {e}")),
    };
    if let Err(e) = result {
        let _ = append(&dir, &Event::Failed(e));
    }
}

/// Keeps `path` as the agent just left it in local history, as a version
/// of the crc session in `dir`: its folder's name, the id a ledger keeps
/// too, so the session's turns can find their versions. Claude's own id is
/// not used: a crc session may hold several of Claude's, and the turns are
/// the crc session's. The session that wrote is named even when another
/// holds the file's checkpoint.
fn keep_version(
    history: Option<&crate::history::recorder::AgentHistory>,
    dir: &Path,
    path: &Path,
) -> Result<(), String> {
    let (Some(history), Some(session)) = (history, dir.file_name().and_then(|n| n.to_str())) else {
        return Ok(());
    };
    if git_metadata(path) {
        return Ok(());
    }
    match history.wrote(path, session) {
        // A history from a newer crc is read-only, which is not a failure.
        Ok(_) | Err(crate::history::store::Error::ReadOnly) => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
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
/// a line always splits back into the same fields. Wrote, prompted, asked
/// and apierror lines start with the time they were written, in Unix
/// seconds, as ran lines do, for [`turns`]; the window's events go without
/// it.
fn append(dir: &Path, event: &Event) -> std::io::Result<()> {
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    let at = crate::platform::unix_seconds();
    let line = match event {
        Event::Wrote { tool, path } => {
            format!(
                "wrote\t{at}\t{}\t{}\n",
                clean(tool),
                clean(&path.to_string_lossy())
            )
        }
        Event::Asked(text) => format!("asked\t{at}\t{}\n", clean(text)),
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
            format!("apierror\t{at}\t{}\t{}\n", clean(error), clean(details))
        }
        Event::Prompted(text) => format!("prompted\t{at}\t{}\n", clean(text)),
        Event::Said(sentences) => {
            let sentences: Vec<String> = sentences.iter().map(|s| clean(s)).collect();
            format!("said\t{}\n", sentences.join("\t"))
        }
        // The test counts, when there are any, after the command: a line
        // without them reads as it always has.
        Event::Ran(ran) => format!(
            "ran\t{}\t{}\t{}{}\n",
            ran.at,
            ran.outcome.code(),
            clean(&ran.command),
            ran.tests
                .map_or_else(String::new, |t| format!("\t{}", t.code()))
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
    timed_event(line).map(|(_, event)| event)
}

/// The time a line of `fields` fields after its kind starts with, and the
/// fields. A line written before it had a time has one field fewer, and its
/// text no tab, so the count tells the two apart.
fn stamped(rest: &str, fields: usize) -> (Option<u64>, &str) {
    if rest.matches('\t').count() >= fields
        && let Some((at, fields)) = rest.split_once('\t')
        && let Ok(at) = at.parse()
    {
        return (Some(at), fields);
    }
    (None, rest)
}

/// [`parse_event`] with the time the line was written, when it has one:
/// wrote, prompted, asked and apierror lines from before times were kept
/// do not.
fn timed_event(line: &str) -> Option<(Option<u64>, Event)> {
    let (kind, rest) = line.split_once('\t').unwrap_or((line, ""));
    let mut at = None;
    let event = match kind {
        "wrote" => {
            let (time, rest) = stamped(rest, 2);
            at = time;
            let (tool, path) = rest.split_once('\t')?;
            Event::Wrote {
                tool: tool.to_owned(),
                path: PathBuf::from(path),
            }
        }
        "asked" => {
            let (time, text) = stamped(rest, 1);
            at = time;
            Event::Asked(text.to_owned())
        }
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
        "prompted" => {
            let (time, text) = stamped(rest, 1);
            at = time;
            Event::Prompted(text.to_owned())
        }
        "said" => Event::Said(rest.split('\t').map(str::to_owned).collect()),
        "ran" => {
            use crate::project::receipt::{Outcome, Ran, TestCounts};
            let mut parts = rest.splitn(4, '\t');
            let time = parts.next()?.parse().ok()?;
            at = Some(time);
            let outcome = Outcome::from_code(parts.next()?)?;
            Event::Ran(Ran {
                at: time,
                outcome,
                command: parts.next()?.to_owned(),
                tests: parts.next().and_then(TestCounts::from_code),
            })
        }
        "apierror" => {
            let (time, rest) = stamped(rest, 2);
            at = time;
            let (error, details) = rest.split_once('\t').unwrap_or((rest, ""));
            Event::ApiError {
                error: error.to_owned(),
                details: details.to_owned(),
            }
        }
        _ => return None,
    };
    Some((at, event))
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
        match parse_event(line) {
            Some(Event::Ran(ran)) => receipt.note(ran),
            Some(Event::Prompted(_)) => receipt.note_prompt(),
            Some(Event::Wrote { .. }) => receipt.note_wrote(),
            Some(Event::Said(sentences)) => receipt.note_said(&sentences),
            _ => {}
        }
    }
    receipt
}

/// The suffix of a session folder moved aside to be removed.
const GONE: &str = ".gone";

/// Removes session folders untouched for longer than `max_age`, except
/// those in `keep` (the sessions of terminals still held), and whatever
/// [`remove`] moved aside and did not finish; says how many went. A
/// folder's time is its newest event or checkpoint. With `ledgers`, an
/// aged session that says its folder keeps its ledger under that folder,
/// as a tab outside any workspace does: which workspace it was started in
/// is not on disk.
pub fn prune(
    root: &Path,
    max_age: std::time::Duration,
    keep: &[PathBuf],
    ledgers: Option<&Path>,
) -> usize {
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
        if old && let Some(ledgers) = ledgers {
            keep_ledger(&dir, ledgers, &about(&dir).1.to_string_lossy());
        }
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

/// The folder of a workspace's ledger: named by a hash of its id, which
/// may be a path or `host:path`.
fn ledger_dir(ledgers: &Path, workspace: &str) -> PathBuf {
    ledgers.join(format!("{:016x}", fingerprint(workspace)))
}

/// Moves the session's `events`, `about` and `files` (its copies from
/// before each first write, which a restore to a turn goes back to) into
/// the ledger of `workspace`; a session with no events, or no workspace to
/// file it under, keeps nothing.
fn keep_ledger(dir: &Path, ledgers: &Path, workspace: &str) {
    let Some(id) = dir.file_name() else {
        return;
    };
    if workspace.is_empty() || is_gone(dir) || !dir.join("events").is_file() {
        return;
    }
    let to = ledger_dir(ledgers, workspace).join(id);
    if std::fs::create_dir_all(&to).is_err() {
        return;
    }
    for name in ["events", "about", "files"] {
        let _ = std::fs::rename(dir.join(name), to.join(name));
    }
    LEDGERS_CHANGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Bumped whenever a ledger is archived or pruned, so the card reads the
/// ledgers again only then and never touches the disk on a plain frame.
/// Only this process writes ledgers: hooks write a session's events.
static LEDGERS_CHANGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Removes the session as [`remove`] does, its terminal closed, keeping
/// its ledger for `workspace` (a [`crate::platform::agents::Workspace::id`],
/// or the tab's folder when it was started in none).
pub fn archive(dir: &Path, workspace: &str) {
    if let Some(ledgers) = ledgers_root() {
        archive_in(dir, &ledgers, workspace);
    } else {
        remove(dir);
    }
}

/// [`archive`] into the ledgers under `ledgers`.
pub fn archive_in(dir: &Path, ledgers: &Path, workspace: &str) {
    keep_ledger(dir, ledgers, workspace);
    remove(dir);
}

/// Removes the ledgers of sessions closed longer than `max_age` ago, and
/// the folders of workspaces left with none; says how many went. A
/// ledger's time is when it was archived.
pub fn prune_ledgers(ledgers: &Path, max_age: std::time::Duration) -> usize {
    let Ok(workspaces) = std::fs::read_dir(ledgers) else {
        return 0;
    };
    let now = std::time::SystemTime::now();
    let mut removed = 0;
    for workspace in workspaces.filter_map(Result::ok).map(|e| e.path()) {
        let Ok(sessions) = std::fs::read_dir(&workspace) else {
            continue;
        };
        for dir in sessions.filter_map(Result::ok).map(|e| e.path()) {
            let old = std::fs::metadata(&dir)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|at| now.duration_since(at).ok())
                .is_some_and(|age| age >= max_age);
            if old && std::fs::remove_dir_all(&dir).is_ok() {
                removed += 1;
            }
        }
        // Only when empty.
        let _ = std::fs::remove_dir(&workspace);
    }
    if removed > 0 {
        LEDGERS_CHANGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    removed
}

/// Every command the closed sessions of `workspace` ran, in the order they
/// ran, with the claims of the last session that made any.
pub fn ledger(workspace: &str) -> crate::project::receipt::Receipt {
    ledgers_root().map_or_else(Default::default, |ledgers| ledger_in(&ledgers, workspace))
}

/// [`ledger`] from the ledgers under `ledgers`.
pub fn ledger_in(ledgers: &Path, workspace: &str) -> crate::project::receipt::Receipt {
    ledger_of_keys(ledgers, &[workspace.to_owned()])
}

/// The ledger filed under every key in `keys` (a workspace's id and
/// folders, [`ledger_keys`]), merged as one.
fn ledger_of_keys(ledgers: &Path, keys: &[String]) -> crate::project::receipt::Receipt {
    let mut receipt = crate::project::receipt::Receipt::default();
    for dir in ledger_sessions(ledgers, keys) {
        receipt.merge(receipt_until(&dir, u64::MAX));
    }
    receipt
}

/// The closed sessions filed under every key in `keys`, oldest archived
/// first, so a later session's claims stand.
fn ledger_sessions(ledgers: &Path, keys: &[String]) -> Vec<PathBuf> {
    let mut sessions: Vec<(std::time::SystemTime, PathBuf)> = keys
        .iter()
        .filter_map(|key| std::fs::read_dir(ledger_dir(ledgers, key)).ok())
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .map(|p| {
            let at = std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            (at, p)
        })
        .collect();
    sessions.sort();
    sessions.into_iter().map(|(_, dir)| dir).collect()
}

/// What a handoff reads from the sessions beside their receipt: the files
/// their agents wrote, each once, in the order first written, and when the
/// first of them started, in Unix seconds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Trail {
    pub wrote: Vec<PathBuf>,
    pub started: Option<u64>,
}

/// The [`Trail`] of the sessions in `dirs`. A session started when its
/// folder's name says ([`new_id`]), or failing that at its first command.
pub fn trail(dirs: &[PathBuf]) -> Trail {
    let mut trail = Trail::default();
    for dir in dirs {
        let mut first_ran = None;
        let text = std::fs::read(dir.join("events")).unwrap_or_default();
        for line in String::from_utf8_lossy(&text).lines() {
            match parse_event(line) {
                Some(Event::Wrote { path, .. }) if !trail.wrote.contains(&path) => {
                    trail.wrote.push(path);
                }
                Some(Event::Ran(ran)) if first_ran.is_none() => first_ran = Some(ran.at),
                _ => {}
            }
        }
        if let Some(at) = started_by_name(dir).or(first_ran) {
            trail.started = Some(trail.started.map_or(at, |s| s.min(at)));
        }
    }
    trail
}

/// When the session in `dir` started, by its name: [`new_id`]'s
/// nanoseconds, in hex, before the process and the count.
fn started_by_name(dir: &Path) -> Option<u64> {
    let name = dir.file_name()?.to_str()?;
    let [nanos, pid, n] = name.split('-').collect::<Vec<_>>()[..] else {
        return None;
    };
    if pid.parse::<u32>().is_err() || n.parse::<u64>().is_err() {
        return None;
    }
    let nanos = u128::from_str_radix(nanos, 16).ok()?;
    u64::try_from(nanos / 1_000_000_000).ok()
}

/// One turn of a session: what its agent did from a prompt to the end of
/// its answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Turn {
    /// 1 for the first prompt's turn, and so on; 0 only for what came
    /// before any prompt.
    pub index: usize,
    /// The prompt's first line, as the prompt hook kept it; `None` for
    /// turn 0.
    pub prompt: Option<String>,
    /// When it started and ended, in Unix seconds, 0 for a line written
    /// before times were kept. `ended` is `None` while it is still running.
    pub started: u64,
    pub ended: Option<u64>,
    /// The files the agent wrote, each once, in the order first written.
    pub wrote: Vec<PathBuf>,
    /// The commands it ran, in the order they ran.
    pub ran: Vec<crate::project::receipt::Ran>,
}

/// The turns of the session in `dir`, live or archived as a ledger, in
/// order. A prompt starts a turn; Stop or an API error ends it, and so
/// does the next prompt when neither came. What the agent wrote or ran before the first
/// prompt (a session resumed, or hooks from before the prompt hook) is
/// kept as turn 0, not dropped, since a restore must know of it. What it
/// did after a Stop, before the next prompt, stays with the turn that
/// ended.
pub fn turns(dir: &Path) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    let text = std::fs::read(dir.join("events")).unwrap_or_default();
    for line in String::from_utf8_lossy(&text).lines() {
        let Some((at, event)) = timed_event(line) else {
            continue;
        };
        let at = at.unwrap_or(0);
        if let Event::Prompted(prompt) = event {
            if let Some(last) = turns.last_mut()
                && last.ended.is_none()
            {
                last.ended = Some(at);
            }
            turns.push(Turn {
                index: turns.last().map_or(1, |t| t.index + 1),
                prompt: Some(prompt),
                started: at,
                ended: None,
                wrote: Vec::new(),
                ran: Vec::new(),
            });
            continue;
        }
        // An API error ends the turn as Stop does; a question mid-turn is
        // not its end.
        let ends = match &event {
            Event::Asked(text) => Some(text == FINISHED),
            Event::ApiError { .. } => Some(true),
            _ => None,
        };
        if let Some(ends) = ends {
            if ends && let Some(last) = turns.last_mut() {
                last.ended = Some(at);
            }
            continue;
        }
        if !matches!(event, Event::Wrote { .. } | Event::Ran(_)) {
            continue;
        }
        if turns.is_empty() {
            turns.push(Turn {
                index: 0,
                prompt: None,
                started: at,
                ended: None,
                wrote: Vec::new(),
                ran: Vec::new(),
            });
        }
        let Some(turn) = turns.last_mut() else {
            continue;
        };
        match event {
            Event::Wrote { path, .. } if !turn.wrote.contains(&path) => turn.wrote.push(path),
            Event::Ran(ran) => turn.ran.push(ran),
            _ => {}
        }
    }
    turns
}

/// The [`Trail`] of the workspace's Receipt card, read now: its closed
/// sessions, as [`workspace_receipt`] files them, and `live`, the session
/// of the terminal showing.
pub fn workspace_trail(
    workspace: Option<&str>,
    folder: Option<&Path>,
    live: Option<&Path>,
) -> Trail {
    let mut dirs = workspace_sessions(workspace, folder, live);
    // Oldest first, so the files read in the order first written.
    dirs.reverse();
    trail(&dirs)
}

/// The sessions of the workspace's Receipt card: its closed sessions, as
/// [`workspace_receipt`] files them, and `live`, the session of the
/// terminal showing; newest first, by when each started.
pub fn workspace_sessions(
    workspace: Option<&str>,
    folder: Option<&Path>,
    live: Option<&Path>,
) -> Vec<PathBuf> {
    workspace_sessions_in(ledgers_root().as_deref(), workspace, folder, live)
}

/// [`workspace_sessions`] from the ledgers under `ledgers`.
pub fn workspace_sessions_in(
    ledgers: Option<&Path>,
    workspace: Option<&str>,
    folder: Option<&Path>,
    live: Option<&Path>,
) -> Vec<PathBuf> {
    let mut dirs = match (workspace, ledgers) {
        (Some(id), Some(ledgers)) => ledger_sessions(ledgers, &ledger_keys(id, folder)),
        _ => Vec::new(),
    };
    if let Some(live) = live.filter(|d| !d.as_os_str().is_empty())
        && !dirs.iter().any(|d| d.file_name() == live.file_name())
    {
        dirs.push(live.to_path_buf());
    }
    // A name that says no start goes last; one archived later than another
    // with the same start stays ahead of it, as the ledgers have them.
    dirs.reverse();
    dirs.sort_by_key(|dir| std::cmp::Reverse(started_by_name(dir).unwrap_or(0)));
    dirs
}

/// When the session in `dir` started, in Unix seconds, by its name.
pub fn session_started(dir: &Path) -> Option<u64> {
    started_by_name(dir)
}

/// Counts every ledger archived or pruned in this process: what was read
/// from the ledgers is good while it says the same.
pub fn ledgers_changed() -> u64 {
    LEDGERS_CHANGED.load(std::sync::atomic::Ordering::Relaxed)
}

/// One session of a workspace's Rewind: its folder, whether it is the
/// terminal showing, and its turns, oldest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rewound {
    pub dir: PathBuf,
    pub live: bool,
    pub turns: Vec<Turn>,
}

/// What a workspace's Rewind shows: its sessions with their turns, newest
/// first, and its folder resolved, as written files are kept.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rewind {
    pub folder: Option<PathBuf>,
    pub sessions: Vec<Rewound>,
}

/// The sessions of [`workspace_sessions`] with their [`turns`], newest
/// first; a session with none is left out. The closed ones are read once,
/// and again only when a ledger was archived or pruned or another
/// workspace shows; the live one when another terminal shows or the window
/// read more of its events (`events_read`). So a plain frame never touches
/// the disk, and an agent at work has only its own session read again.
pub fn workspace_rewind(
    workspace: Option<&str>,
    folder: Option<&Path>,
    live: Option<&Path>,
    events_read: u64,
) -> std::sync::Arc<Rewind> {
    type Closed = (Option<String>, Option<PathBuf>, u64);
    type Key = (Closed, Option<PathBuf>, u64);
    type Read = (Key, std::sync::Arc<Rewind>);
    static READ: std::sync::Mutex<Option<Read>> = std::sync::Mutex::new(None);
    let live = live.filter(|d| !d.as_os_str().is_empty());
    let key: Key = (
        (
            workspace.map(str::to_owned),
            folder.map(Path::to_path_buf),
            ledgers_changed(),
        ),
        live.map(Path::to_path_buf),
        events_read,
    );
    let mut read = READ
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let last = read.take();
    if let Some((at, rewind)) = &last
        && *at == key
    {
        let rewind = rewind.clone();
        *read = last;
        return rewind;
    }
    let not_live = |dir: &Path| live.is_none_or(|l| dir.file_name() != l.file_name());
    let mut sessions: Vec<Rewound> = match last {
        Some((at, rewind)) if at.0 == key.0 => rewind
            .sessions
            .iter()
            .filter(|s| !s.live && not_live(&s.dir))
            .cloned()
            .collect(),
        _ => {
            let dirs: Vec<PathBuf> = workspace_sessions(workspace, folder, None)
                .into_iter()
                .filter(|d| not_live(d))
                .collect();
            rewind_of(&dirs, None)
        }
    };
    if let Some(live) = live {
        let turns = turns(live);
        if !turns.is_empty() {
            insert_live(
                &mut sessions,
                Rewound {
                    dir: live.to_path_buf(),
                    live: true,
                    turns,
                },
            );
        }
    }
    let rewind = std::sync::Arc::new(Rewind {
        folder: folder
            .filter(|f| !f.as_os_str().is_empty())
            .map(crate::platform::canonical),
        sessions,
    });
    *read = Some((key, rewind.clone()));
    rewind
}

/// `live` among `sessions` (newest first) where [`workspace_sessions`]
/// puts it: ahead of any that started no later.
fn insert_live(sessions: &mut Vec<Rewound>, live: Rewound) {
    let started = started_by_name(&live.dir).unwrap_or(0);
    let at = sessions
        .iter()
        .position(|s| started_by_name(&s.dir).unwrap_or(0) <= started)
        .unwrap_or(sessions.len());
    sessions.insert(at, live);
}

/// The turns of each of `dirs`, in their order, leaving out a session
/// with none; `live` is marked.
pub fn rewind_of(dirs: &[PathBuf], live: Option<&Path>) -> Vec<Rewound> {
    dirs.iter()
        .map(|dir| Rewound {
            dir: dir.clone(),
            live: live.is_some_and(|l| l == dir),
            turns: turns(dir),
        })
        .filter(|s| !s.turns.is_empty())
        .collect()
}

/// The local history version an agent kept of a file in the turn that ran
/// from `from` until `until` (the next turn's start, `None` for the last),
/// in Unix seconds: the newest of `entries` (newest first, as
/// [`crate::history::store::Store::list`] gives them) kept by the crc
/// session `session` in that time. `None` when the turn kept none.
pub fn turn_version(
    entries: &[crate::history::store::Entry],
    session: &str,
    from: u64,
    until: Option<u64>,
) -> Option<crate::history::store::SnapshotId> {
    entries
        .iter()
        .find(|e| {
            let at = u64::try_from(e.at.div_euclid(1000)).unwrap_or(0);
            e.reason == crate::history::store::Reason::Agent
                && e.session.as_deref() == Some(session)
                && at >= from
                && until.is_none_or(|until| at < until)
        })
        .map(|e| e.id)
}

/// What a file goes back to in a restore.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Back {
    /// An agent's version from local history, written in the file's
    /// current encoding and line endings.
    Text(String),
    /// The session's copy from before its first write, byte for byte.
    Bytes(Vec<u8>),
    /// The agent created it: it goes.
    Absent,
}

/// One file of a [`RestorePlan`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restoring {
    pub path: PathBuf,
    pub to: Back,
    /// Not as the session last left it: someone, or something, changed it
    /// since. Restoring drops that change, so it is asked about.
    pub changed: bool,
    /// The file's size and hash when the plan was read; a restore refuses a
    /// file that moved since.
    pub seen: String,
}

/// What restoring a session to the end of one of its turns would do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestorePlan {
    /// The turn's index, as the Rewind view numbers it.
    pub turn: usize,
    /// The files to put back, in the order the session first wrote them.
    pub files: Vec<Restoring>,
    /// Files already as they were at the turn's end.
    pub same: usize,
    /// Files with nothing known to put back, and why: left as they are.
    pub refused: Vec<(PathBuf, String)>,
    /// The session's last turn has not ended: its agent may still write.
    pub running: bool,
}

/// What restoring the session in `dir` to the end of `turns[at]` would do:
/// every file the session wrote goes back to how it was then. A file the
/// session wrote by then takes the version local history kept of it in
/// the last turn that wrote it, looked up in that turn alone, so a missing
/// version never brings back an older one. A file first written after
/// goes back to the session's copy from before it wrote it, or away when
/// the agent created it. `live` says the session is the terminal showing.
pub fn restore_plan(
    dir: &Path,
    turns: &[Turn],
    at: usize,
    live: bool,
    store: Option<&crate::history::store::Store>,
) -> RestorePlan {
    let mut plan = RestorePlan {
        turn: turns.get(at).map_or(0, |t| t.index),
        running: live && turns.last().is_some_and(|t| t.ended.is_none()),
        ..RestorePlan::default()
    };
    let Some(session) = dir.file_name().and_then(|n| n.to_str()) else {
        return plan;
    };
    let mut paths: Vec<&PathBuf> = Vec::new();
    for turn in turns {
        for path in &turn.wrote {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    let checkpoints = files(dir);
    for path in paths {
        let entries = store
            .and_then(|s| s.list(&crate::platform::canonical(path)).ok())
            .unwrap_or_default();
        let last = turns
            .iter()
            .enumerate()
            .take(at + 1)
            .rfind(|(_, t)| t.wrote.contains(path));
        let to = match last {
            Some((n, turn)) => {
                let until = turns.get(n + 1).map(|t| t.started);
                let text = turn_version(&entries, session, turn.started, until)
                    .and_then(|id| store?.read(id).ok().flatten());
                match text {
                    Some(text) => Ok(Back::Text(text)),
                    None => Err(format!("no saved version from turn {}", turn.index)),
                }
            }
            None => baseline(&checkpoints, path),
        };
        let to = match to {
            Ok(to) => to,
            Err(why) => {
                plan.refused.push((path.clone(), why));
                continue;
            }
        };
        let now = std::fs::read(path).ok();
        if now.as_deref().is_some_and(|bytes| is_back(bytes, &to))
            || (now.is_none() && to == Back::Absent)
        {
            plan.same += 1;
            continue;
        }
        let seen = file_fingerprint(path).unwrap_or_default();
        plan.files.push(Restoring {
            changed: !left_so(dir, path, session, &entries, store, now.as_deref(), &seen),
            path: path.clone(),
            to,
            seen,
        });
    }
    plan
}

/// The session's copy of `path` from before its first write, as a
/// restore puts it back.
fn baseline(checkpoints: &[File], path: &Path) -> Result<Back, String> {
    let file = checkpoints.iter().find(|f| {
        f.path == path || crate::platform::canonical(&f.path) == crate::platform::canonical(path)
    });
    match file.map(|f| &f.checkpoint) {
        Some(Checkpoint::Text(kept)) => std::fs::read(kept)
            .map(Back::Bytes)
            .map_err(|e| format!("the copy from before the session: {e}")),
        Some(Checkpoint::Absent) => Ok(Back::Absent),
        Some(Checkpoint::TooLarge) => Err("too large: no copy from before the session".into()),
        Some(Checkpoint::Unhooked(_)) | None => Err("no copy from before the session".into()),
    }
}

/// Whether the file's `bytes` are what `to` puts back.
fn is_back(bytes: &[u8], to: &Back) -> bool {
    match to {
        Back::Bytes(kept) => bytes == kept.as_slice(),
        Back::Text(text) => {
            crate::text::file_format::decode(bytes).is_ok_and(|(now, _)| now == *text)
        }
        Back::Absent => false,
    }
}

/// Whether `path` is as the session last left it: as its post hook
/// recorded it, or as its newest version in local history.
fn left_so(
    dir: &Path,
    path: &Path,
    session: &str,
    entries: &[crate::history::store::Entry],
    store: Option<&crate::history::store::Store>,
    now: Option<&[u8]>,
    seen: &str,
) -> bool {
    let after = std::fs::read_to_string(dir.join("files").join(format!("{}.after", key(path))));
    if after.is_ok_and(|after| after == seen) {
        return true;
    }
    let newest = entries.iter().find(|e| {
        e.reason == crate::history::store::Reason::Agent && e.session.as_deref() == Some(session)
    });
    let text = newest.and_then(|e| store?.read(e.id).ok().flatten());
    match (text, now) {
        (Some(text), Some(now)) => is_back(now, &Back::Text(text)),
        _ => false,
    }
}

/// Puts one file of a [`RestorePlan`] back, unless it moved since the plan
/// was read. Answers what it held, for local history to keep: `None` when
/// it was not there.
pub fn restore_file(file: &Restoring) -> Result<Option<Vec<u8>>, String> {
    let name = file.path.file_name().map_or_else(
        || file.path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let said = |e: std::io::Error| format!("{name}: {e}");
    let now = file_fingerprint(&file.path).map_err(said)?;
    if now != file.seen {
        return Err(format!("{name} changed while the restore was asked about"));
    }
    let before = match std::fs::read(&file.path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(said(e)),
    };
    let bytes = match &file.to {
        Back::Absent => {
            match std::fs::remove_file(&file.path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(said(e)),
                _ => {}
            }
            return Ok(before);
        }
        Back::Bytes(bytes) => bytes.clone(),
        Back::Text(text) => {
            // The file's own encoding and dominant line ending, as a save
            // of the open document would keep them.
            let format = before
                .as_deref()
                .and_then(|b| crate::text::file_format::decode(b).ok())
                .map(|(_, format)| crate::text::file_format::DiskFormat {
                    endings: Vec::new(),
                    ..format
                })
                .unwrap_or_default();
            let mut out = Vec::new();
            let rope = crate::text::rope::Rope::from_text(text);
            crate::text::file_format::write(&mut out, &rope, &format).map_err(said)?;
            out
        }
    };
    if let Some(parent) = file.path.parent() {
        std::fs::create_dir_all(parent).map_err(said)?;
    }
    // Written in place, so the file keeps its mode, as Undo does.
    std::fs::write(&file.path, bytes).map_err(said)?;
    Ok(before)
}

/// What the workspace's Receipt card shows: its ledger with the receipt of
/// its terminal showing, `live`, after it. The live session's claims are
/// the ones that stand while it is there.
pub fn card<'a>(
    ledger: Option<&crate::project::receipt::Receipt>,
    live: Option<&'a crate::project::receipt::Receipt>,
) -> Option<std::borrow::Cow<'a, crate::project::receipt::Receipt>> {
    let Some(ledger) = ledger.filter(|l| l.shown()) else {
        return live.map(std::borrow::Cow::Borrowed);
    };
    let mut merged = ledger.clone();
    // A terminal that has run and claimed nothing yet leaves the last
    // session's claim check standing.
    if let Some(live) = live.filter(|l| l.shown()) {
        merged.merge(live.clone());
        merged.claims.clone_from(&live.claims);
    }
    Some(std::borrow::Cow::Owned(merged))
}

/// [`card`] for the workspace with id `workspace` and folder `folder`, its
/// ledger read once and again only when a session is archived into it or
/// pruned from it.
pub fn workspace_receipt<'a>(
    workspace: Option<&str>,
    folder: Option<&Path>,
    live: Option<&'a crate::project::receipt::Receipt>,
) -> Option<std::borrow::Cow<'a, crate::project::receipt::Receipt>> {
    let ledger = workspace.and_then(|id| ledger_cached(id, folder));
    card(ledger.as_deref(), live)
}

/// The keys a workspace's sessions are filed under: its id, and for a tab
/// started outside any workspace its folder, as given and resolved.
fn ledger_keys(workspace: &str, folder: Option<&Path>) -> Vec<String> {
    let mut keys = vec![workspace.to_owned()];
    if let Some(folder) = folder.filter(|f| !f.as_os_str().is_empty()) {
        let resolved = std::fs::canonicalize(folder).ok();
        for key in std::iter::once(folder).chain(resolved.as_deref()) {
            let key = key.to_string_lossy().into_owned();
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    keys
}

/// The ledger of a workspace, read once and again only after a ledger was
/// archived or pruned ([`LEDGERS_CHANGED`]); `None` when it has none.
fn ledger_cached(
    workspace: &str,
    folder: Option<&Path>,
) -> Option<std::sync::Arc<crate::project::receipt::Receipt>> {
    type Read = (
        String,
        u64,
        Option<std::sync::Arc<crate::project::receipt::Receipt>>,
    );
    static READ: std::sync::Mutex<Vec<Read>> = std::sync::Mutex::new(Vec::new());
    let changed = ledgers_changed();
    let mut read = READ
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((_, at, receipt)) = read.iter().find(|(id, ..)| id == workspace)
        && *at == changed
    {
        return receipt.clone();
    }
    let receipt = ledgers_root()
        .map(|ledgers| ledger_of_keys(&ledgers, &ledger_keys(workspace, folder)))
        .filter(crate::project::receipt::Receipt::shown)
        .map(std::sync::Arc::new);
    read.retain(|(id, ..)| id != workspace);
    read.push((workspace.to_owned(), changed, receipt.clone()));
    receipt
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
        // Whole or not at all: a second instance starting a session at the
        // same moment reads the old file or the new, never half of one.
        crate::platform::write_atomically(&path, text.as_bytes()).ok()?;
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
/// can name it after the tab is gone. A hook may have started the file
/// with [`CLAUDE_SESSION`]; this adds to it.
pub fn describe(dir: &Path, title: &str, folder: &Path) {
    if !dir.is_dir() || about_field(&about_text(dir), "title").is_some() {
        return;
    }
    let clean = |s: &str| s.replace(['\n', '\r'], " ");
    let _ = add_about(
        dir,
        &format!(
            "title={}\nfolder={}\n",
            clean(title),
            // Resolved, as the file paths are, so one is a prefix of the other.
            clean(&crate::platform::canonical(folder).to_string_lossy())
        ),
    );
}

/// The key of Claude's own id for the session in `about`.
const CLAUDE_SESSION: &str = "claude_session";

/// Records Claude's own id for the session, the `session_id` of a hook's
/// input, in its `about`, the first time a hook has one; an id already
/// there stays. Two hooks at once may both add it, the same id twice, and
/// [`claude_session`] reads the first.
fn note_claude_session(dir: &Path, input: &crate::json::Value) {
    let Some(id) = input.get("session_id").and_then(|v| v.as_str()) else {
        return;
    };
    // An id, not text: anything else could break the file's lines.
    let id_like = !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if id_like && claude_session(dir).is_none() {
        let _ = add_about(dir, &format!("{CLAUDE_SESSION}={id}\n"));
    }
}

/// Claude's own id for the session in `dir`, when a hook recorded it.
pub fn claude_session(dir: &Path) -> Option<String> {
    about_field(&about_text(dir), CLAUDE_SESSION)
}

/// Adds lines to the session's `about`, in one append: the window and a
/// hook may both write it.
fn add_about(dir: &Path, lines: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("about"))?
        .write_all(lines.as_bytes())
}

fn about_text(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("about")).unwrap_or_default()
}

/// The first `key=value` of an `about` file's text.
fn about_field(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
        .map(str::to_owned)
}

/// The session's tab title and its folder, as [`describe`] wrote them;
/// "Terminal" and an empty path when it never did.
pub fn about(dir: &Path) -> (String, PathBuf) {
    let text = about_text(dir);
    let title = about_field(&text, "title").unwrap_or_else(|| "Terminal".into());
    (
        title,
        about_field(&text, "folder")
            .map(PathBuf::from)
            .unwrap_or_default(),
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
    use crate::history::recorder::AgentHistory;
    use crate::history::store::{Reason, Store};

    /// [`super::hook`] with no local history: a test never writes to the
    /// person's own.
    fn hook(kind: &str, input: &str, dir: &Path) -> Result<Option<Event>, String> {
        super::hook(kind, input, dir, None)
    }

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

    /// Local history in a history file of the test's own, as
    /// `CRC_HISTORY_DB` points a self-test at one.
    fn history_in(tree: &TempTree, exclude: &str) -> AgentHistory {
        AgentHistory {
            db: tree.0.join("history").join("history.db"),
            on: true,
            exclude: exclude.into(),
        }
    }

    /// `path`'s versions in `history`, newest first, with their text.
    fn versions(history: &AgentHistory, path: &Path) -> Vec<(Reason, Option<String>, String)> {
        let Some(store) = Store::open_to_read(&history.db).expect("open") else {
            return Vec::new();
        };
        store
            .list(path)
            .expect("list")
            .into_iter()
            .map(|e| {
                let text = store.read(e.id).expect("read").unwrap_or_default();
                (e.reason, e.session, text)
            })
            .collect()
    }

    #[test]
    fn a_write_done_is_kept_in_local_history_as_the_sessions_version() {
        let tree = TempTree::new("review-history", &[("a.txt", "one\n")]);
        let dir = tree.0.join("5f3a-77-0");
        let history = history_in(&tree, "");
        let a = crate::platform::canonical(&tree.0.join("a.txt"));
        let post = |dir: &Path| super::hook("post", &pre(&a), dir, Some(&history));

        hook("pre", &pre(&a), &dir).expect("hook");
        std::fs::write(&a, "two\r\nlines\r\n").expect("agent writes");
        assert_eq!(post(&dir).expect("hook"), None);
        // Kept as a document opened on it holds it: LF line endings.
        assert_eq!(
            versions(&history, &a),
            [(
                Reason::Agent,
                Some("5f3a-77-0".to_owned()),
                "two\nlines\n".to_owned()
            )]
        );
        // The same text again in the same session is the same version; a
        // new text is a new one.
        post(&dir).expect("hook");
        std::fs::write(&a, "three\n").expect("agent writes");
        post(&dir).expect("hook");
        let texts: Vec<String> = versions(&history, &a).into_iter().map(|v| v.2).collect();
        assert_eq!(texts, ["three\n", "two\nlines\n"]);
        // Undo's record of the file is kept as before.
        let file = files(&dir).remove(0);
        assert_eq!(undoable_now(&dir, &file), Ok(()));

        // Another session leaving the same text has its own version, even
        // though the first session holds the file's checkpoint.
        let other = tree.0.join("5f3a-78-0");
        super::hook("post", &pre(&a), &other, Some(&history)).expect("hook");
        let sessions: Vec<Option<String>> =
            versions(&history, &a).into_iter().map(|v| v.1).collect();
        assert_eq!(
            sessions,
            [
                Some("5f3a-78-0".to_owned()),
                Some("5f3a-77-0".to_owned()),
                Some("5f3a-77-0".to_owned())
            ]
        );
    }

    #[test]
    fn local_historys_rules_hold_for_agent_writes() {
        let tree = TempTree::new(
            "review-history-rules",
            &[
                (".env", "TOKEN=1\n"),
                ("logs/run.log", "log\n"),
                ("kept.txt", "kept\n"),
                ("repo/.git/config", "[core]\n"),
            ],
        );
        let dir = tree.0.join("session");
        let history = history_in(&tree, "*.log");
        let post = |path: &Path| {
            let path = crate::platform::canonical(path);
            super::hook("post", &pre(&path), &dir, Some(&history)).expect("hook");
            versions(&history, &path).len()
        };
        // Secrets by name, the user's globs, Git's own files.
        assert_eq!(post(&tree.0.join(".env")), 0);
        assert_eq!(post(&tree.0.join("logs/run.log")), 0);
        assert_eq!(post(&tree.0.join("repo/.git/config")), 0);
        // Too big, and not text.
        let big = tree.0.join("big.txt");
        std::fs::write(&big, "x".repeat(crate::history::recorder::MAX_BYTES + 1)).expect("write");
        assert_eq!(post(&big), 0);
        let binary = tree.0.join("blob.bin");
        std::fs::write(&binary, [0u8, 1, 2, 0, 255]).expect("write");
        assert_eq!(post(&binary), 0);
        // Not on this Mac's disk, as a file on another machine is: nothing,
        // and no failure for the agent.
        assert_eq!(post(&tree.0.join("elsewhere/gone.txt")), 0);
        assert!(!dir.join("events").exists(), "nothing failed");
        // Local history off.
        let off = AgentHistory {
            on: false,
            ..history_in(&tree, "")
        };
        let kept = crate::platform::canonical(&tree.0.join("kept.txt"));
        super::hook("post", &pre(&kept), &dir, Some(&off)).expect("hook");
        assert_eq!(versions(&history, &kept).len(), 0);
        // And what the rules allow is kept.
        assert_eq!(post(&tree.0.join("kept.txt")), 1);
    }

    /// A history the hook cannot write is the hook's failure, said the way
    /// its others are; one from a newer crc is read-only, which is not one.
    #[test]
    fn a_history_that_cannot_be_written_never_fails_the_agent() {
        let tree = TempTree::new("review-history-broken", &[("a.txt", "one\n")]);
        let dir = tree.0.join("session");
        let a = crate::platform::canonical(&tree.0.join("a.txt"));

        let newer = history_in(&tree, "");
        std::fs::create_dir_all(newer.db.parent().unwrap()).expect("mkdir");
        crate::index::db::Db::open(&newer.db)
            .expect("db")
            .execute("PRAGMA user_version = 99")
            .expect("version");
        super::hook("post", &pre(&a), &dir, Some(&newer)).expect("hook");
        assert!(!dir.join("events").exists(), "read-only is not a failure");

        // A history file that is a folder cannot be opened at all.
        let broken = AgentHistory {
            db: tree.0.join("folder.db"),
            on: true,
            exclude: String::new(),
        };
        std::fs::create_dir_all(&broken.db).expect("mkdir");
        // An error, which run_hook appends as a failure and never returns
        // to the agent.
        let failed = super::hook("post", &pre(&a), &dir, Some(&broken));
        assert!(
            failed.as_ref().is_err_and(|e| e.contains("a.txt")),
            "{failed:?}"
        );
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
                ("fresh/events", "ran\t1\t0\tcargo test\n"),
                ("fresh/about", "title=t\nfolder=/w\n"),
                ("held/events", ""),
                ("left.gone/events", ""),
            ],
        );
        let ledgers = TempTree::new("review-prune-ledgers", &[]);
        let held = tree.0.join("held");
        assert_eq!(
            prune(&tree.0, MAX_AGE, &[], Some(&ledgers.0)),
            1,
            "a removal left behind goes"
        );
        assert!(!tree.0.join("left.gone").exists());
        // A session whose terminal is still held is never aged out.
        assert_eq!(
            prune(
                &tree.0,
                std::time::Duration::ZERO,
                std::slice::from_ref(&held),
                Some(&ledgers.0)
            ),
            1
        );
        assert!(!tree.0.join("fresh").exists());
        assert!(held.is_dir());
        // The aged session's commands stay, under its folder.
        assert_eq!(ledger_in(&ledgers.0, "/w").total, 1);
    }

    /// Hooks write a receipt into a session, which the session's tab
    /// closing archives: the ledger has its commands, not its files.
    #[test]
    fn archive_keeps_events_and_checkpoints() {
        let tree = TempTree::new("review-archive", &[("work/a.txt", "one\n")]);
        let ledgers = tree.0.join("ledgers");
        let dir = tree.0.join("sessions").join("s1");
        let file = tree.0.join("work/a.txt");
        hook("pre", &pre(&file), &dir).expect("pre");
        assert!(dir.join("files").is_dir());
        describe(&dir, "Agent", &tree.0.join("work"));
        for command in ["cargo test", "cargo build"] {
            let input = format!(r#"{{"tool_input":{{"command":"{command}"}}}}"#);
            hook("ran", &input, &dir).expect("ran");
        }
        archive_in(&dir, &ledgers, "ws");
        assert!(!dir.exists(), "the session folder goes");
        let kept = ledger_dir(&ledgers, "ws").join("s1");
        assert!(kept.join("events").is_file());
        assert!(kept.join("about").is_file());
        // The copy from before the write, for a restore to a turn.
        assert_eq!(files(&kept).len(), 1);
        assert!(matches!(files(&kept)[0].checkpoint, Checkpoint::Text(_)));
        assert_eq!(receipt_until(&kept, u64::MAX).total, 2);
        assert_eq!(ledger_in(&ledgers, "ws").total, 2);
        assert_eq!(about(&kept).0, "Agent");
        // Nothing to keep: a session with no events, or no workspace.
        let quiet = tree.0.join("sessions").join("s2");
        std::fs::create_dir_all(quiet.join("files")).expect("mkdir");
        archive_in(&quiet, &ledgers, "ws");
        assert!(!quiet.exists());
        assert!(!ledger_dir(&ledgers, "ws").join("s2").exists());
    }

    /// A session wrote a.txt in turn 1 and again in turn 2, created b.txt
    /// in turn 2, and wrote c.txt in turn 1 with no version kept.
    /// Restoring to turn 1 puts a.txt back to turn 1's version and removes
    /// b.txt; back to turn 2 brings both again, said changed since the
    /// session left them. A file that moved after the plan is refused.
    #[test]
    fn restore_puts_files_back_as_a_turn_left_them() {
        let tree = TempTree::new(
            "review-restore",
            &[("a.txt", "one\n"), ("c.txt", "c\n"), ("crlf.txt", "x\r\n")],
        );
        let root = crate::platform::canonical(&tree.0);
        let dir = root.join("5f3a-1-0");
        let session = "5f3a-1-0";
        let (a, b, c, crlf) = (
            root.join("a.txt"),
            root.join("b.txt"),
            root.join("c.txt"),
            root.join("crlf.txt"),
        );
        for path in [&a, &b, &c, &crlf] {
            checkpoint(&dir, path).expect("checkpoint");
        }
        let store = Store::open(&root.join("history.db")).expect("store");
        let write = |path: &Path, text: &str, at: i64| {
            std::fs::write(path, text).expect("write");
            store
                .put_agent_snapshot(path, &text.replace('\r', ""), session, at)
                .expect("snapshot");
        };
        write(&a, "two\n", 150_000);
        std::fs::write(&c, "c turn 1\n").expect("write");
        write(&crlf, "y\r\n", 160_000);
        write(&a, "three\n", 250_000);
        write(&b, "bee\n", 260_000);
        let turn = |index: usize, started: u64, wrote: &[&PathBuf]| Turn {
            index,
            prompt: None,
            started,
            ended: Some(started + 50),
            wrote: wrote.iter().map(|p| (*p).clone()).collect(),
            ran: Vec::new(),
        };
        let turns = [turn(1, 100, &[&a, &c, &crlf]), turn(2, 200, &[&a, &b])];

        // As the session left them: nothing to go back to for turn 2.
        let plan = restore_plan(&dir, &turns, 1, true, Some(&store));
        assert_eq!((plan.turn, plan.files.len(), plan.same), (2, 0, 3));
        assert_eq!(
            plan.refused,
            [(c.clone(), "no saved version from turn 1".into())]
        );
        assert!(!plan.running);

        // Back to turn 1: a.txt's first version, b.txt gone, crlf.txt as it
        // was then. None was changed since the session left it.
        let plan = restore_plan(&dir, &turns, 0, true, Some(&store));
        let to: Vec<(&Path, &Back, bool)> = plan
            .files
            .iter()
            .map(|f| (f.path.as_path(), &f.to, f.changed))
            .collect();
        assert_eq!(
            to,
            [
                (a.as_path(), &Back::Text("two\n".into()), false),
                (b.as_path(), &Back::Absent, false),
            ]
        );
        assert_eq!(plan.same, 1, "crlf.txt is as turn 1 left it");
        for file in &plan.files {
            restore_file(file).expect("restore");
        }
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "two\n");
        assert!(!b.exists());

        // Forward to turn 2 again: both said changed, since the restore,
        // not the session, left them so.
        let plan = restore_plan(&dir, &turns, 1, true, Some(&store));
        assert_eq!(plan.files.len(), 2);
        assert!(plan.files.iter().all(|f| f.changed));
        // a.txt moves after the plan was read: refused, left as it is.
        std::fs::write(&a, "mine\n").unwrap();
        assert!(restore_file(&plan.files[0]).is_err());
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "mine\n");
        assert_eq!(restore_file(&plan.files[1]).expect("b"), None);
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "bee\n");

        // A version kept as text goes back in the file's own line endings.
        std::fs::write(&crlf, "z\r\n").unwrap();
        let plan = restore_plan(&dir, &turns, 0, true, Some(&store));
        let crlf_back = plan.files.iter().find(|f| f.path == crlf).expect("crlf");
        restore_file(crlf_back).expect("crlf");
        assert_eq!(std::fs::read(&crlf).unwrap(), b"y\r\n");

        // A live session still in its last turn is said to be running.
        let mut open = turns.clone();
        open[1].ended = None;
        assert!(restore_plan(&dir, &open, 0, true, Some(&store)).running);
        assert!(!restore_plan(&dir, &open, 0, false, Some(&store)).running);
    }

    /// A turn whose write kept no version never falls back to an earlier
    /// turn's version of the file.
    #[test]
    fn restore_refuses_a_file_whose_turn_kept_no_version() {
        let tree = TempTree::new("review-restore-gap", &[("a.txt", "one\n")]);
        let root = crate::platform::canonical(&tree.0);
        let dir = root.join("5f3a-2-0");
        let a = root.join("a.txt");
        checkpoint(&dir, &a).expect("checkpoint");
        let store = Store::open(&root.join("history.db")).expect("store");
        store
            .put_agent_snapshot(&a, "two\n", "5f3a-2-0", 150_000)
            .expect("snapshot");
        std::fs::write(&a, "three\n").unwrap();
        let turns = [
            Turn {
                index: 1,
                prompt: None,
                started: 100,
                ended: Some(140),
                wrote: vec![a.clone()],
                ran: Vec::new(),
            },
            Turn {
                index: 2,
                prompt: None,
                started: 200,
                ended: Some(240),
                wrote: vec![a.clone()],
                ran: Vec::new(),
            },
        ];
        let plan = restore_plan(&dir, &turns, 1, false, Some(&store));
        assert!(plan.files.is_empty());
        assert_eq!(
            plan.refused,
            [(a.clone(), "no saved version from turn 2".into())]
        );
        // Without local history at all, nothing is known either.
        let plan = restore_plan(&dir, &turns, 0, false, None);
        assert_eq!(plan.refused, [(a, "no saved version from turn 1".into())]);
    }

    #[test]
    fn ledgers_older_than_30_days_are_pruned() {
        let tree = TempTree::new(
            "review-ledger-age",
            &[
                ("old/a/events", "ran\t1\t0\tls\n"),
                ("old/b/events", "ran\t2\t0\tls\n"),
                ("both/c/events", "ran\t3\t0\tls\n"),
                ("both/d/events", "ran\t4\t0\tls\n"),
            ],
        );
        let aged = std::time::SystemTime::now() - LEDGER_AGE - std::time::Duration::from_secs(60);
        for session in ["old/a", "old/b", "both/c"] {
            std::fs::File::open(tree.0.join(session))
                .and_then(|f| f.set_modified(aged))
                .expect("age");
        }
        assert_eq!(prune_ledgers(&tree.0, LEDGER_AGE), 3);
        assert!(!tree.0.join("old").exists(), "an emptied workspace goes");
        assert!(!tree.0.join("both/c").exists());
        assert!(tree.0.join("both/d/events").is_file());
    }

    /// Sessions archived into a workspace read as one receipt, commands in
    /// the order they ran; the card puts the live terminal's after them.
    #[test]
    fn ledger_merges_archived_sessions() {
        use crate::project::receipt::Outcome;
        let tree = TempTree::new("review-ledger-merge", &[]);
        let ledgers = tree.0.join("ledgers");
        let session = |name: &str, workspace: &str, events: &str| {
            let dir = tree.0.join("sessions").join(name);
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(dir.join("events"), events).expect("events");
            archive_in(&dir, &ledgers, workspace);
        };
        session(
            "one",
            "/repo",
            "prompted\tfix it\nran\t100\t0\tcargo build\nran\t300\t101\tcargo test\n",
        );
        session(
            "two",
            "/repo",
            "ran\t200\t0\tcargo fmt\nsaid\tThe tests pass.\n",
        );
        session("other", "/elsewhere", "ran\t150\t0\tls\n");
        let merged = ledger_in(&ledgers, "/repo");
        assert_eq!((merged.total, merged.failed), (3, 1));
        let commands: Vec<&str> = merged.ran.iter().map(|r| r.command.as_str()).collect();
        assert_eq!(commands, ["cargo build", "cargo fmt", "cargo test"]);
        assert_eq!(merged.claims.len(), 1, "the claims of the last session");
        assert!(ledger_in(&ledgers, "/nowhere").is_empty());

        // The card: the ledger alone, or with the live terminal's after it.
        assert!(card(None, None).is_none());
        assert_eq!(card(Some(&merged), None).expect("card").total, 3);
        let mut live = crate::project::receipt::Receipt::default();
        live.note(crate::project::receipt::Ran {
            at: 400,
            outcome: Outcome::Passed,
            command: "git status".into(),
            tests: None,
        });
        let shown = card(Some(&merged), Some(&live)).expect("card");
        assert_eq!(shown.total, 4);
        assert_eq!(
            shown.latest(1).next().map(|r| r.command.as_str()),
            Some("git status")
        );
        assert!(shown.claims.is_empty(), "the live session's claims stand");
        // Regression: a fresh terminal, nothing run or claimed yet, wiped
        // the closed session's claim check off the card.
        let fresh = crate::project::receipt::Receipt::default();
        let shown = card(Some(&merged), Some(&fresh)).expect("card");
        assert_eq!(shown.total, 3);
        assert_eq!(shown.claims.len(), 1, "the last session's claims stand");
        assert!(matches!(
            card(Some(&Default::default()), Some(&live)),
            Some(std::borrow::Cow::Borrowed(_))
        ));
    }

    /// A handoff's files and start: what the sessions wrote, each once,
    /// from when the earliest began, by its name or its first command.
    #[test]
    fn a_trail_has_the_files_written_and_the_earliest_start() {
        let tree = TempTree::new("review-trail", &[]);
        // 2023-11-14T22:13:20Z, as new_id names a folder.
        let named = format!("{:x}-123-0", 1_700_000_000_000_000_000u128);
        let events = [
            (
                named.as_str(),
                "wrote\tWrite\t/w/a.rs\nran\t1700000100\t0\tls\nwrote\tEdit\t/w/b.rs\n",
            ),
            (
                "ledger-copy",
                "ran\t1700000050\t0\tls\nwrote\tEdit\t/w/a.rs\n",
            ),
            ("quiet", "wrote\tWrite\t/w/c.rs\n"),
        ];
        let dirs: Vec<PathBuf> = events
            .iter()
            .map(|(name, text)| {
                let dir = tree.0.join(name);
                std::fs::create_dir_all(&dir).expect("mkdir");
                std::fs::write(dir.join("events"), text).expect("events");
                dir
            })
            .collect();
        let trail = trail(&dirs);
        assert_eq!(
            trail.wrote,
            [
                PathBuf::from("/w/a.rs"),
                PathBuf::from("/w/b.rs"),
                PathBuf::from("/w/c.rs")
            ]
        );
        assert_eq!(trail.started, Some(1_700_000_000));
        // Without a name to go by, its first command.
        assert_eq!(super::trail(&dirs[1..]).started, Some(1_700_000_050));
        assert_eq!(super::trail(&dirs[2..]).started, None);
        assert_eq!(
            started_by_name(Path::new(&new_id())).map(|s| s > 0),
            Some(true)
        );
    }

    /// Hooks write a session's turns as they happen: each prompt starts
    /// one, Stop ends it, and the files and commands between are its own.
    /// The ledger the session's tab leaves reads the same.
    #[test]
    fn turns_group_writes_between_prompt_and_stop() {
        let tree = TempTree::new("review-turns", &[("work/a.rs", ""), ("work/b.rs", "")]);
        let dir = tree.0.join("sessions").join("s1");
        let a = crate::platform::canonical(&tree.0.join("work/a.rs"));
        let b = crate::platform::canonical(&tree.0.join("work/b.rs"));
        let c = crate::platform::canonical(&tree.0.join("work")).join("c.rs");
        let before = crate::platform::unix_seconds();
        hook("prompt", r#"{"prompt":"fix the parser\nand more"}"#, &dir).expect("prompt");
        hook("pre", &pre(&a), &dir).expect("pre");
        hook("ran", r#"{"tool_input":{"command":"cargo test"}}"#, &dir).expect("ran");
        hook("pre", &pre(&b), &dir).expect("pre");
        hook("pre", &pre(&a), &dir).expect("pre");
        hook("stop", r#"{"last_assistant_message":"Done."}"#, &dir).expect("stop");
        hook("prompt", r#"{"prompt":"now c"}"#, &dir).expect("prompt");
        hook("pre", &pre(&c), &dir).expect("pre");
        let after = crate::platform::unix_seconds();

        let turns = turns(&dir);
        assert_eq!(turns.len(), 2, "{turns:?}");
        let (first, second) = (&turns[0], &turns[1]);
        assert_eq!(first.index, 1);
        assert_eq!(first.prompt.as_deref(), Some("fix the parser"));
        assert_eq!(first.wrote, [a.clone(), b.clone()]);
        let commands: Vec<&str> = first.ran.iter().map(|r| r.command.as_str()).collect();
        assert_eq!(commands, ["cargo test"]);
        assert!(before <= first.started && first.started <= after);
        let ended = first.ended.expect("the first turn ended");
        assert!(first.started <= ended && ended <= second.started);
        assert_eq!(second.index, 2);
        assert_eq!(second.prompt.as_deref(), Some("now c"));
        assert_eq!(second.wrote, [c]);
        assert!(second.ran.is_empty());
        assert_eq!(second.ended, None, "still running");

        // The archived ledger keeps the times, so its turns are the same.
        let ledgers = tree.0.join("ledgers");
        archive_in(&dir, &ledgers, "ws");
        assert_eq!(super::turns(&ledger_dir(&ledgers, "ws").join("s1")), turns);
    }

    /// What came before the first prompt is turn 0, with no prompt; a
    /// prompt with no Stop ends at the next prompt; a write after Stop
    /// stays with the turn that just ended; a question is not the end, an
    /// API error is.
    #[test]
    fn turns_keep_what_came_before_the_first_prompt_as_turn_0() {
        let tree = TempTree::new(
            "review-turn-0",
            &[(
                "s/events",
                "ran\t100\t0\tls\n\
                 wrote\t101\tWrite\t/w/early.rs\n\
                 prompted\t110\tfirst\n\
                 wrote\t111\tEdit\t/w/a.rs\n\
                 prompted\t120\tsecond\n\
                 wrote\t121\tEdit\t/w/b.rs\n\
                 asked\t125\tClaude needs your permission\n\
                 wrote\t126\tEdit\t/w/b.rs\n\
                 asked\t130\tfinished\n\
                 wrote\t131\tEdit\t/w/late.rs\n\
                 prompted\t140\tthird\n\
                 wrote\t141\tEdit\t/w/c.rs\n\
                 apierror\t145\toverloaded\tAPI Error: 529\n\
                 prompted\t150\tfourth\n",
            )],
        );
        let turns = turns(&tree.0.join("s"));
        // Index, prompt, start, end, files written, commands run.
        type Row<'a> = (
            usize,
            Option<&'a str>,
            u64,
            Option<u64>,
            Vec<&'a Path>,
            usize,
        );
        let summary: Vec<Row> = turns
            .iter()
            .map(|t| {
                (
                    t.index,
                    t.prompt.as_deref(),
                    t.started,
                    t.ended,
                    t.wrote.iter().map(PathBuf::as_path).collect(),
                    t.ran.len(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (0, None, 100, Some(110), vec![Path::new("/w/early.rs")], 1),
                (
                    1,
                    Some("first"),
                    110,
                    Some(120),
                    vec![Path::new("/w/a.rs")],
                    0
                ),
                (
                    2,
                    Some("second"),
                    120,
                    Some(130),
                    vec![Path::new("/w/b.rs"), Path::new("/w/late.rs")],
                    0
                ),
                (
                    3,
                    Some("third"),
                    140,
                    Some(145),
                    vec![Path::new("/w/c.rs")],
                    0
                ),
                (4, Some("fourth"), 150, None, Vec::new(), 0),
            ]
        );
        // Prompted from the start: no turn 0.
        let tree = TempTree::new(
            "review-turn-1",
            &[("s/events", "prompted\t5\tgo\nwrote\t6\tWrite\t/w/a.rs\n")],
        );
        let turns = super::turns(&tree.0.join("s"));
        assert_eq!(turns.len(), 1);
        assert_eq!((turns[0].index, turns[0].ended), (1, None));
        assert!(super::turns(&tree.0.join("none")).is_empty());
    }

    /// A workspace's Rewind lists its sessions newest first by when each
    /// started, as its name says, whatever order they were archived in:
    /// the live one among them, once, and a name with no start last.
    #[test]
    fn workspace_sessions_are_newest_first_by_start() {
        let tree = TempTree::new("review-sessions", &[]);
        let ledgers = tree.0.join("ledgers");
        let named = |secs: u128| format!("{:x}-123-0", secs * 1_000_000_000);
        let (old, middle, new) = (named(1_000), named(2_000), named(3_000));
        // Archived newest first, then the oldest, then one with no start.
        for name in [new.as_str(), old.as_str(), "plain"] {
            let dir = tree.0.join("sessions").join(name);
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(dir.join("events"), "prompted\t5\tgo\n").expect("events");
            archive_in(&dir, &ledgers, "ws");
        }
        let live = tree.0.join("sessions").join(&middle);
        std::fs::create_dir_all(&live).expect("mkdir");
        std::fs::write(live.join("events"), "wrote\t6\tWrite\t/w/a.rs\n").expect("events");
        let names = |dirs: Vec<PathBuf>| -> Vec<String> {
            dirs.iter()
                .map(|d| d.file_name().unwrap().to_string_lossy().into_owned())
                .collect()
        };
        let all = workspace_sessions_in(Some(&ledgers), Some("ws"), None, Some(&live));
        assert_eq!(names(all.clone()), [&new, &middle, &old, "plain"]);
        assert_eq!(all[1], live);
        // No live terminal; another workspace has none.
        let closed = workspace_sessions_in(Some(&ledgers), Some("ws"), None, None);
        assert_eq!(names(closed), [&new, &old, "plain"]);
        assert!(workspace_sessions_in(Some(&ledgers), Some("other"), None, None).is_empty());
        // A live session already archived is not listed twice.
        let again = ledger_dir(&ledgers, "ws").join(&new);
        let once = workspace_sessions_in(Some(&ledgers), Some("ws"), None, Some(&again));
        assert_eq!(once.len(), 3);

        // Their turns: the live one marked; one with none left out.
        std::fs::create_dir_all(tree.0.join("quiet")).expect("mkdir");
        let mut dirs = all;
        dirs.push(tree.0.join("quiet"));
        let rewound = rewind_of(&dirs, Some(&live));
        assert_eq!(rewound.len(), 4);
        assert_eq!(
            rewound.iter().map(|s| s.live).collect::<Vec<_>>(),
            [false, true, false, false]
        );
        assert_eq!(rewound[1].turns[0].index, 0);
        assert_eq!(session_started(&live), Some(2_000));

        // The live one read on its own goes where the list has it.
        let mut closed: Vec<Rewound> = rewound.iter().filter(|s| !s.live).cloned().collect();
        insert_live(&mut closed, rewound[1].clone());
        assert_eq!(closed, rewound);
    }

    /// A turn's version of a file: the newest the turn's own session kept
    /// by agent between the turn's start and the next turn's, not another
    /// session's, not one kept by hand, and none when it kept nothing.
    #[test]
    fn a_turn_finds_its_session_s_agent_version() {
        use crate::history::store::{Entry, Reason, SnapshotId};
        let entry = |id: i64, secs: i64, reason: Reason, session: Option<&str>| Entry {
            id: SnapshotId(id),
            at: secs * 1000 + 400,
            reason,
            size: 1,
            session: session.map(str::to_owned),
        };
        // Newest first, as the store lists them.
        let entries = [
            entry(7, 300, Reason::Agent, Some("s1")),
            entry(6, 250, Reason::Saved, None),
            entry(5, 210, Reason::Agent, Some("s2")),
            entry(4, 205, Reason::Agent, Some("s1")),
            entry(3, 201, Reason::Agent, Some("s1")),
            entry(2, 150, Reason::Agent, Some("s1")),
            entry(1, 100, Reason::Opened, None),
        ];
        // The first turn, 140 until 200; the second from 200 until 300;
        // the third from 300 on.
        assert_eq!(
            turn_version(&entries, "s1", 140, Some(200)),
            Some(SnapshotId(2))
        );
        assert_eq!(
            turn_version(&entries, "s1", 200, Some(300)),
            Some(SnapshotId(4))
        );
        assert_eq!(turn_version(&entries, "s1", 300, None), Some(SnapshotId(7)));
        assert_eq!(turn_version(&entries, "s2", 200, None), Some(SnapshotId(5)));
        // Kept nothing then, or not this session.
        assert_eq!(turn_version(&entries, "s1", 90, Some(140)), None);
        assert_eq!(turn_version(&entries, "s3", 0, None), None);
        assert_eq!(turn_version(&[], "s1", 0, None), None);
    }

    /// Lines written before wrote, prompted, asked and apierror lines had a time
    /// still read, as turns with no time, and in the trail.
    #[test]
    fn event_lines_without_a_time_still_parse() {
        let old = |line: &str| timed_event(line).expect(line);
        assert_eq!(
            old("wrote\tEdit\t/w/a.rs"),
            (
                None,
                Event::Wrote {
                    tool: "Edit".into(),
                    path: PathBuf::from("/w/a.rs")
                }
            )
        );
        assert_eq!(
            old("wrote\t300\tEdit\t/w/a.rs"),
            (
                Some(300),
                Event::Wrote {
                    tool: "Edit".into(),
                    path: PathBuf::from("/w/a.rs")
                }
            )
        );
        assert_eq!(
            old("prompted\t12 apples"),
            (None, Event::Prompted("12 apples".into()))
        );
        assert_eq!(
            old("prompted\t300\t12 apples"),
            (Some(300), Event::Prompted("12 apples".into()))
        );
        assert_eq!(
            old("asked\tfinished"),
            (None, Event::Asked(FINISHED.into()))
        );
        assert_eq!(
            old("asked\t300\tfinished"),
            (Some(300), Event::Asked(FINISHED.into()))
        );
        assert_eq!(
            old("asked\t300\t"),
            (Some(300), Event::Asked(String::new()))
        );
        let overloaded = |details: &str| Event::ApiError {
            error: "overloaded".into(),
            details: details.into(),
        };
        assert_eq!(
            old("apierror\toverloaded\tAPI Error: 529"),
            (None, overloaded("API Error: 529"))
        );
        assert_eq!(old("apierror\toverloaded\t"), (None, overloaded("")));
        assert_eq!(
            old("apierror\t300\toverloaded\tAPI Error: 529"),
            (Some(300), overloaded("API Error: 529"))
        );
        assert_eq!(
            old("apierror\t300\toverloaded\t"),
            (Some(300), overloaded(""))
        );

        let tree = TempTree::new(
            "review-old-lines",
            &[(
                "s/events",
                "prompted\tfirst\nwrote\tWrite\t/w/a.rs\nasked\tfinished\n\
                 prompted\t50\tsecond\nwrote\t51\tEdit\t/w/b.rs\n",
            )],
        );
        let dir = tree.0.join("s");
        let turns = turns(&dir);
        assert_eq!(turns.len(), 2);
        assert_eq!(
            (turns[0].started, turns[0].ended, turns[0].wrote.len()),
            (0, Some(0), 1)
        );
        assert_eq!((turns[1].started, turns[1].ended), (50, None));
        assert_eq!(
            trail(std::slice::from_ref(&dir)).wrote,
            [PathBuf::from("/w/a.rs"), PathBuf::from("/w/b.rs")]
        );
    }

    /// Claude's own id for the session goes in `about` once, from the
    /// first hook that has one, beside the tab's title and folder.
    #[test]
    fn the_claude_session_id_is_recorded_once() {
        let tree = TempTree::new("review-claude-session", &[]);
        let dir = tree.0.join("s");
        hook("prompt", r#"{"prompt":"a"}"#, &dir).expect("prompt");
        assert_eq!(claude_session(&dir), None);
        let id = "3f2a9c1e-7b4d-4e8a-9c0f-1a2b3c4d5e6f";
        hook(
            "prompt",
            &format!(r#"{{"session_id":"{id}","prompt":"b"}}"#),
            &dir,
        )
        .expect("prompt");
        hook("stop", r#"{"session_id":"another"}"#, &dir).expect("stop");
        assert_eq!(claude_session(&dir).as_deref(), Some(id));
        describe(&dir, "Agent", Path::new("/work/garden-log"));
        let text = std::fs::read_to_string(dir.join("about")).expect("about");
        assert_eq!(text.matches("claude_session=").count(), 1, "{text}");
        assert_eq!(
            about(&dir),
            ("Agent".into(), PathBuf::from("/work/garden-log"))
        );
        // Anything that is not an id is not kept.
        let other = tree.0.join("t");
        hook(
            "prompt",
            r#"{"session_id":"a\nfolder=/x","prompt":"a"}"#,
            &other,
        )
        .expect("hook");
        assert_eq!(claude_session(&other), None);
        assert!(!other.join("about").exists());
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
        // No details: the last message says it when it is the error, not
        // when it is the agent's own words. No error named: unknown.
        hook(
            "stop-failure",
            r#"{"last_assistant_message":"API Error: Connection dropped"}"#,
            &dir,
        )
        .expect("hook");
        hook(
            "stop-failure",
            r#"{"error":"unknown","last_assistant_message":"The socket test lost its connection."}"#,
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
                    details: "API Error: Connection dropped".into(),
                },
                Event::ApiError {
                    error: "unknown".into(),
                    details: String::new(),
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

    /// A test command's counts come from its output, on a success from
    /// stdout and stderr (jest prints to stderr), on a failure from the
    /// error; only the counts reach the events file, never the output.
    #[test]
    fn a_test_run_keeps_its_counts_and_not_its_output() {
        use crate::project::receipt::TestCounts;
        let tree = TempTree::new("review-counts", &[]);
        let dir = tree.0.join("session");
        let tests = |kind: &str, input: &str| match hook(kind, input, &dir).expect("hook") {
            Some(Event::Ran(ran)) => ran.tests,
            other => panic!("{other:?}"),
        };
        let counts = |passed, failed| Some(TestCounts { passed, failed });
        assert_eq!(
            tests(
                "ran-failed",
                r#"{"tool_input":{"command":"cargo test"},"error":"Exit code 101\ntest result: FAILED. 12 passed; 1 failed; 0 ignored\nSECRET_TOKEN=abc123","is_interrupt":false}"#
            ),
            counts(12, 1)
        );
        assert_eq!(
            tests(
                "ran",
                r#"{"tool_input":{"command":"npx jest"},"tool_response":{"stdout":"","stderr":"Tests:       20 passed, 20 total\nSECRET_TOKEN=abc123"}}"#
            ),
            counts(20, 0)
        );
        // Not a test command: its output is not read.
        assert_eq!(
            tests(
                "ran",
                r#"{"tool_input":{"command":"cat log.txt"},"tool_response":{"stdout":"test result: ok. 3 passed; 0 failed"}}"#
            ),
            None
        );
        // Stopped part way: no counts.
        assert_eq!(
            tests(
                "ran-failed",
                r#"{"tool_input":{"command":"cargo test"},"error":"test result: ok. 3 passed; 0 failed","is_interrupt":true}"#
            ),
            None
        );
        let events = std::fs::read_to_string(dir.join("events")).expect("events");
        assert!(!events.contains("SECRET"), "{events}");
        let ends: Vec<&str> = events
            .lines()
            .map(|l| l.rsplit('\t').next().unwrap_or(""))
            .collect();
        assert_eq!(ends, ["12/1", "20/0", "cat log.txt", "cargo test"]);
        let receipt = receipt_until(&dir, u64::MAX);
        let read: Vec<_> = receipt.ran.iter().map(|r| r.tests).collect();
        assert_eq!(read, [counts(12, 1), counts(20, 0), None, None]);
    }

    /// Lines written before test counts were kept, and lines whose counts
    /// this build cannot read, still give their command.
    #[test]
    fn ran_lines_without_test_counts_still_parse() {
        use crate::project::receipt::{Outcome, Ran, TestCounts};
        let parsed = |line: &str| match parse_event(line) {
            Some(Event::Ran(ran)) => ran,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            parsed("ran\t300\t101\tcargo test --offline"),
            Ran {
                at: 300,
                outcome: Outcome::Failed(101),
                command: "cargo test --offline".into(),
                tests: None,
            }
        );
        assert_eq!(
            parsed("ran\t300\t101\tcargo test\t12/1").tests,
            Some(TestCounts {
                passed: 12,
                failed: 1
            })
        );
        let odd = parsed("ran\t300\t0\tcargo test\tsomething new");
        assert_eq!((odd.command.as_str(), odd.tests), ("cargo test", None));
    }

    /// The Stop input as Claude Code sent it (2.1.316, probed
    /// 2026-10-31): the agent's last words in `last_assistant_message`,
    /// kept as sentences ahead of the turn's end, and checked against the
    /// turn's commands when a held terminal comes back.
    #[test]
    fn stop_keeps_what_the_agent_said_for_the_receipt() {
        use crate::project::receipt::{Kind, Verdict};
        let tree = TempTree::new("review-said", &[]);
        let dir = tree.0.join("session");
        hook("prompt", r#"{"prompt":"fix the parser"}"#, &dir).expect("prompt");
        hook(
            "ran-failed",
            r#"{"tool_input":{"command":"cargo test"},"error":"Exit code 101","is_interrupt":false}"#,
            &dir,
        )
        .expect("ran");
        let stop = r#"{"session_id":"s","hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"Fixed the parser.\n\nAll tests pass\tnow. Want me to commit?","background_tasks":[],"session_crons":[]}"#;
        assert_eq!(
            hook("stop", stop, &dir).expect("stop"),
            Some(Event::Asked(FINISHED.into()))
        );
        let (events, read) = read_events(&dir, 0);
        assert_eq!(
            events[2],
            Event::Said(vec![
                "Fixed the parser.".into(),
                "All tests pass now.".into(),
                "Want me to commit?".into(),
            ])
        );
        assert_eq!(events[3], Event::Asked(FINISHED.into()));
        let receipt = receipt_until(&dir, read);
        assert_eq!(receipt.claims.len(), 1);
        assert_eq!(receipt.claims[0].kind, Kind::Tests);
        assert!(matches!(
            receipt.claims[0].verdict,
            Verdict::Contradicted { .. }
        ));
        // Nothing said, nothing kept: only the turn's end.
        hook("stop", r#"{"last_assistant_message":""}"#, &dir).expect("stop");
        let (events, _) = read_events(&dir, read);
        assert_eq!(events, [Event::Asked(FINISHED.into())]);
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
