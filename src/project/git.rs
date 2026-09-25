//! Local Git plumbing. Paths remain OS strings; display labels may be lossy.
//! Call on workers, never on the event/render thread. No network operations.
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

#[derive(Clone, Debug)]
pub struct Change {
    pub path: PathBuf,
    pub original: Option<PathBuf>,
    pub index: u8,
    pub worktree: u8,
}

impl Change {
    /// Unmerged: both sides changed the path and Git left the result to
    /// the user. Such a path is neither staged nor unstaged; `git add`
    /// after resolving it is what ends the conflict.
    pub fn conflicted(&self) -> bool {
        self.index == b'U'
            || self.worktree == b'U'
            || (self.index == b'A' && self.worktree == b'A')
            || (self.index == b'D' && self.worktree == b'D')
    }
    pub fn staged(&self) -> bool {
        self.index != b' ' && self.index != b'?' && !self.conflicted()
    }
    pub fn unstaged(&self) -> bool {
        (self.worktree != b' ' || self.index == b'?') && !self.conflicted()
    }
    pub fn label(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
    pub fn status(&self) -> String {
        String::from_utf8_lossy(&[self.index, self.worktree]).into_owned()
    }
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub root: PathBuf,
    pub branch: String,
    pub changes: Vec<Change>,
    /// A merge, rebase, cherry-pick or revert that stopped for the user.
    pub in_progress: Option<InProgress>,
}

/// An operation Git left half done, read from the files it keeps in the
/// Git directory while one is under way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InProgress {
    Merge,
    Rebase,
    CherryPick,
    Revert,
}

impl InProgress {
    /// As the status bar shows it.
    pub fn label(self) -> &'static str {
        match self {
            InProgress::Merge => "MERGING",
            InProgress::Rebase => "REBASING",
            InProgress::CherryPick => "CHERRY-PICKING",
            InProgress::Revert => "REVERTING",
        }
    }

    /// What the files in `git_dir` say is under way, if anything.
    pub fn read(git_dir: &Path) -> Option<InProgress> {
        if git_dir.join("rebase-merge").is_dir() || git_dir.join("rebase-apply").is_dir() {
            Some(InProgress::Rebase)
        } else if git_dir.join("MERGE_HEAD").is_file() {
            Some(InProgress::Merge)
        } else if git_dir.join("CHERRY_PICK_HEAD").is_file() {
            Some(InProgress::CherryPick)
        } else if git_dir.join("REVERT_HEAD").is_file() {
            Some(InProgress::Revert)
        } else {
            None
        }
    }
}

/// What a line in a rendered diff is, which decides how it is drawn.
///
/// The panel used to show `git diff` verbatim, `diff --git`, `index`, `---`
/// and `+++` included, coloured only by the first character of the text. That
/// is the output of a command, not a view of a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffKind {
    /// `STAGED` or `WORKING TREE`, when a file has both.
    Section,
    /// A hunk boundary. Drawn as a separator with its own context caption.
    Hunk,
    Added,
    Removed,
    Context,
    /// Anything git says that is not part of the text: "Binary files differ",
    /// a truncation notice, an error.
    Note,
}

#[derive(Clone, Debug)]
pub struct DiffLine {
    pub kind: DiffKind,
    /// Line number on the left (before) and right (after) side. A line that
    /// exists on only one side has only that one.
    pub old: Option<usize>,
    pub new: Option<usize>,
    /// The text without its leading `+`, `-` or space.
    pub text: String,
    /// Index into `Diff::hunks` for an actionable hunk header.
    pub hunk: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct Hunk {
    pub staged: bool,
    pub ordinal: usize,
    /// Exact text shown for this hunk, checked against a fresh Git diff before
    /// changing the index. Non-UTF-8 diffs fail closed at apply time.
    pub raw: String,
}

#[derive(Clone, Debug, Default)]
pub struct Diff {
    pub lines: Vec<DiffLine>,
    pub hunks: Vec<Hunk>,
    pub truncated: bool,
    pub added: usize,
    pub removed: usize,
}

impl Diff {
    fn push(&mut self, kind: DiffKind, old: Option<usize>, new: Option<usize>, text: &str) {
        match kind {
            DiffKind::Added => self.added += 1,
            DiffKind::Removed => self.removed += 1,
            _ => {}
        }
        self.lines.push(DiffLine {
            kind,
            old,
            new,
            text: text.to_owned(),
            hunk: None,
        });
    }

    /// Turns `git diff` output into lines that carry their own meaning and
    /// line numbers, dropping the headers that address the command rather
    /// than the reader.
    pub fn parse(text: &str) -> Diff {
        let mut diff = Diff::default();
        let (mut old, mut new) = (0usize, 0usize);
        let mut staged = false;
        let mut active_hunk = None;
        let mut ordinal = 0;
        for raw_line in text.split_terminator('\n') {
            // Preserve CRLF bytes for `git apply`, while the visible diff
            // remains the same text without the line terminator.
            let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
            if let Some(rest) = line.strip_prefix("@@") {
                // `@@ -12,7 +12,9 @@ fn name()` — the trailing text is the
                // enclosing context git found, which is worth keeping.
                let (counts, caption) = rest.split_once("@@").unwrap_or((rest, ""));
                (old, new) = hunk_start(counts);
                diff.push(DiffKind::Hunk, None, None, caption.trim());
                let index = diff.hunks.len();
                diff.lines.last_mut().expect("hunk line").hunk = Some(index);
                diff.hunks.push(Hunk {
                    staged,
                    ordinal,
                    raw: format!("{raw_line}\n"),
                });
                ordinal += 1;
                active_hunk = Some(index);
                continue;
            }
            // Headers that name the command's arguments, not the change.
            if line.starts_with("diff --git ")
                || line.starts_with("index ")
                || line.starts_with("--- ")
                || line.starts_with("+++ ")
                || line.starts_with("new file mode")
                || line.starts_with("deleted file mode")
                || line.starts_with("old mode")
                || line.starts_with("new mode")
                || line.starts_with("similarity index")
                || line.starts_with("rename from")
                || line.starts_with("rename to")
            {
                active_hunk = None;
                continue;
            }
            if line == "STAGED" || line == "WORKING TREE" {
                staged = line == "STAGED";
                ordinal = 0;
                active_hunk = None;
                diff.push(DiffKind::Section, None, None, line);
                continue;
            }
            if line.is_empty() {
                active_hunk = None;
                continue;
            }
            if line == "… Diff preview truncated at 512 KiB." {
                diff.truncated = true;
                active_hunk = None;
            }
            if let Some(index) = active_hunk {
                diff.hunks[index].raw.push_str(raw_line);
                diff.hunks[index].raw.push('\n');
            }
            match line.as_bytes()[0] {
                b'+' => {
                    new += 1;
                    diff.push(DiffKind::Added, None, Some(new), &line[1..]);
                }
                b'-' => {
                    old += 1;
                    diff.push(DiffKind::Removed, Some(old), None, &line[1..]);
                }
                b' ' => {
                    old += 1;
                    new += 1;
                    diff.push(DiffKind::Context, Some(old), Some(new), &line[1..]);
                }
                // "\ No newline at end of file", "Binary files ... differ".
                _ => diff.push(DiffKind::Note, None, None, line),
            }
        }
        diff
    }
}

/// The `-12,7 +12,9` of a hunk header, as the first line number on each side.
fn hunk_start(counts: &str) -> (usize, usize) {
    let mut old = 0;
    let mut new = 0;
    for field in counts.split_whitespace() {
        let (sign, rest) = field.split_at(1);
        let number = rest
            .split(',')
            .next()
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap_or(1);
        // The header numbers the first line; the counter is incremented
        // before use, so start one behind it.
        match sign {
            "-" => old = number.saturating_sub(1),
            "+" => new = number.saturating_sub(1),
            _ => {}
        }
    }
    (old, new)
}

fn command(root: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(root)
        .args(["-c", "core.fsmonitor=false", "-c", "color.ui=false"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LITERAL_PATHSPECS", "1");
    cmd
}

fn checked(output: Output) -> Result<Vec<u8>, String> {
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

fn run(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    checked(
        command(root)
            .args(args)
            .output()
            .map_err(|e| e.to_string())?,
    )
}

/// Files Git would show under `directory`: tracked ones and untracked
/// ones it does not ignore, joined onto `directory`. What the project
/// index reads.
pub fn project_files(directory: &Path) -> Result<Vec<PathBuf>, String> {
    let out = run(
        directory,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
    )?;
    let mut files: Vec<PathBuf> = out
        .split(|&b| b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| directory.join(std::ffi::OsStr::from_bytes(r)))
        .collect();
    files.dedup();
    Ok(files)
}

/// Untracked paths Git ignores under `directory`, joined onto it. A directory ignored as a whole is one entry, not its
/// contents: `node_modules` is one line however big it is. More than
/// `IGNORED_MAX` entries is treated as none, rather than a partial answer
/// that would dim some siblings and not others.
pub fn ignored(directory: &Path) -> Result<std::collections::HashSet<PathBuf>, String> {
    const IGNORED_MAX: usize = 200_000;
    // Run from `directory`, not the top level: Git then prints paths
    // relative to it, and joining them back onto `directory` keeps the
    // caller's spelling. The top level is the resolved path, so a project
    // opened through a symlink (/var is /private/var) never matched.
    let out = run(
        directory,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ],
    )?;
    let mut paths = std::collections::HashSet::new();
    for record in out.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let record = record.strip_suffix(b"/").unwrap_or(record);
        paths.insert(directory.join(std::ffi::OsStr::from_bytes(record)));
        if paths.len() > IGNORED_MAX {
            return Err(format!("more than {IGNORED_MAX} ignored paths"));
        }
    }
    Ok(paths)
}

/// The repository that `directory` is inside.
pub fn toplevel(directory: &Path) -> Result<PathBuf, String> {
    let root = run(directory, &["rev-parse", "--show-toplevel"])?;
    let root = root.strip_suffix(b"\n").unwrap_or(&root);
    Ok(PathBuf::from(std::ffi::OsString::from_vec(root.to_vec())))
}

/// The text of `path` as of HEAD, or `None` when HEAD has no such file:
/// a new file, or a repository with no commits yet. Line endings are
/// normalised to what the editor keeps in its rope, so a CRLF file does
/// not read as changed on every line.
pub fn head_text(root: &Path, path: &Path) -> Result<Option<String>, String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "outside the repository".to_string())?;
    let spec = format!("HEAD:{}", relative.to_string_lossy());
    let output = command(root)
        .args(["show", &spec])
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
        return Ok(Some(text));
    }
    let error = String::from_utf8_lossy(&output.stderr);
    if error.contains("does not exist")
        || error.contains("exists on disk")
        || error.contains("but not in")
        || error.contains("Needed a single revision")
        || error.contains("bad revision")
    {
        Ok(None)
    } else {
        Err(error.trim().to_owned())
    }
}

/// What a gutter mark says about a line of the current text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkKind {
    Added,
    Modified,
    /// Lines were removed just above this one. On the line count itself
    /// when they were removed from the end.
    Removed,
}

/// One line's standing against HEAD, for the editor gutter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    /// Zero-based line of the current text.
    pub line: usize,
    pub kind: MarkKind,
}

/// Gutter marks for a diff of HEAD against the current text.
///
/// A run of removed and added lines is one change: its added lines are
/// modifications when anything was removed in the same run, additions
/// otherwise, and a run with nothing added is a deletion sitting above the
/// next line that survived.
pub fn marks(diff: &Diff) -> Vec<Mark> {
    let mut out = Vec::new();
    let lines = &diff.lines;
    let mut previous_new = 0;
    let mut i = 0;
    while i < lines.len() {
        if !matches!(lines[i].kind, DiffKind::Removed | DiffKind::Added) {
            if let Some(new) = lines[i].new {
                previous_new = new;
            }
            i += 1;
            continue;
        }
        let start = i;
        while i < lines.len() && matches!(lines[i].kind, DiffKind::Removed | DiffKind::Added) {
            i += 1;
        }
        let run = &lines[start..i];
        let removed = run.iter().any(|l| l.kind == DiffKind::Removed);
        let added: Vec<usize> = run.iter().filter_map(|l| l.new).collect();
        if added.is_empty() {
            out.push(Mark {
                line: previous_new,
                kind: MarkKind::Removed,
            });
        } else {
            let kind = if removed {
                MarkKind::Modified
            } else {
                MarkKind::Added
            };
            for new in added {
                out.push(Mark {
                    line: new - 1,
                    kind,
                });
                previous_new = new;
            }
        }
    }
    out
}

pub fn snapshot(directory: &Path) -> Result<Snapshot, String> {
    // One process for both: the top level, and the Git directory, which is
    // not `<root>/.git` in a linked worktree.
    let located = run(
        directory,
        &["rev-parse", "--show-toplevel", "--absolute-git-dir"],
    )?;
    let mut lines = located.split(|&b| b == b'\n').filter(|l| !l.is_empty());
    let root = PathBuf::from(std::ffi::OsString::from_vec(
        lines.next().ok_or("Git returned no top level")?.to_vec(),
    ));
    let in_progress = lines
        .next()
        .map(|dir| PathBuf::from(std::ffi::OsString::from_vec(dir.to_vec())))
        .and_then(|dir| InProgress::read(&dir));
    let bytes = run(
        &root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--branch",
            "--untracked-files=all",
        ],
    )?;
    let mut snapshot = parse_status(root, &bytes)?;
    snapshot.in_progress = in_progress;
    Ok(snapshot)
}

fn parse_status(root: PathBuf, bytes: &[u8]) -> Result<Snapshot, String> {
    let mut fields = bytes
        .split(|&byte| byte == 0)
        .filter(|field| !field.is_empty());
    let header = fields.next().ok_or("Git returned no status")?;
    if !header.starts_with(b"## ") {
        return Err("Invalid Git status header".into());
    }
    let branch = String::from_utf8_lossy(&header[3..]).into_owned();
    let mut changes = Vec::new();
    for _ in 0..100_000 {
        let Some(field) = fields.next() else {
            break;
        };
        if field.len() < 4 || field[2] != b' ' {
            return Err("Invalid Git status record".into());
        }
        let index = field[0];
        let worktree = field[1];
        let path = PathBuf::from(std::ffi::OsString::from_vec(field[3..].to_vec()));
        let original = if [index, worktree].iter().any(|c| matches!(c, b'R' | b'C')) {
            Some(PathBuf::from(std::ffi::OsString::from_vec(
                fields.next().ok_or("Missing rename source")?.to_vec(),
            )))
        } else {
            None
        };
        changes.push(Change {
            path,
            original,
            index,
            worktree,
        });
    }
    if fields.next().is_some() {
        return Err(
            "More than 100,000 changes; narrow the repository before opening Source Control".into(),
        );
    }
    Ok(Snapshot {
        root,
        branch,
        changes,
        in_progress: None,
    })
}

pub fn diff(root: &Path, change: &Change) -> Result<String, String> {
    if change.index == b'?' {
        // Git itself handles binary files and symlinks. Exit 1 means differences.
        let mut cmd = command(root);
        cmd.args([
            "diff",
            "--no-index",
            "--no-ext-diff",
            "--no-textconv",
            "--",
            "/dev/null",
        ])
        .arg(&change.path)
        .current_dir(root);
        return diff_output(cmd, true);
    }
    let mut text = String::new();
    for (staged, title) in [(true, "STAGED"), (false, "WORKING TREE")] {
        let mut cmd = command(root);
        cmd.args(["diff", "--no-ext-diff", "--no-textconv"]);
        if staged {
            cmd.arg("--cached");
        }
        cmd.arg("--").arg(&change.path);
        if let Some(old) = &change.original {
            cmd.arg(old);
        }
        let preview = diff_output(cmd, false)?;
        if !preview.is_empty() {
            text.push_str(&format!("{title}\n\n{preview}\n"));
        }
    }
    if text.is_empty() {
        text.push_str("No textual diff. This may be a binary file, submodule, or metadata change.");
    }
    Ok(text)
}

fn diff_output(mut cmd: Command, differences_exit: bool) -> Result<String, String> {
    let (bytes, truncated) = diff_output_bytes(&mut cmd, differences_exit)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        text.push_str("\n… Diff preview truncated at 512 KiB.\n");
    }
    Ok(text)
}

fn diff_output_bytes(cmd: &mut Command, differences_exit: bool) -> Result<(Vec<u8>, bool), String> {
    const LIMIT: usize = 512 * 1024;
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stderr = child.stderr.take().ok_or("Missing Git stderr")?;
    let errors = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.by_ref().take(64 * 1024).read_to_end(&mut bytes);
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        bytes
    });
    let mut bytes = Vec::new();
    let read = child
        .stdout
        .take()
        .ok_or("Missing Git stdout")?
        .take((LIMIT + 1) as u64)
        .read_to_end(&mut bytes);
    if bytes.len() > LIMIT || read.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let errors = errors.join().unwrap_or_default();
    read.map_err(|e| e.to_string())?;
    if bytes.len() <= LIMIT && !status.success() && !(differences_exit && status.code() == Some(1))
    {
        return Err(String::from_utf8_lossy(&errors).into_owned());
    }
    let truncated = bytes.len() > LIMIT;
    bytes.truncate(LIMIT);
    Ok((bytes, truncated))
}

/// Hunk staging is available for tracked, text-only modifications. File
/// additions, deletions, renames, mode changes and binary patches keep the
/// existing whole-file actions until they can be represented safely here.
pub fn can_stage_hunk(change: &Change, staged: bool) -> bool {
    change.original.is_none()
        && if staged {
            change.index == b'M'
        } else {
            change.worktree == b'M' && change.index != b'?'
        }
}

fn split_text_patch(raw: &str) -> Result<(&str, Vec<&str>), String> {
    if !raw.starts_with("diff --git ") {
        return Err("Git returned no text patch".into());
    }
    let mut first_hunk = None;
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in raw.split_inclusive('\n') {
        if offset > 0 && line.starts_with("diff --git ") {
            return Err("Hunk action requires one file patch".into());
        }
        if line.starts_with("@@ -") {
            first_hunk.get_or_insert(offset);
            starts.push(offset);
        }
        offset += line.len();
    }
    let first = first_hunk.ok_or("No text hunk is available")?;
    let header = &raw[..first];
    if header.lines().any(|line| {
        line.starts_with("new file mode")
            || line.starts_with("deleted file mode")
            || line.starts_with("old mode")
            || line.starts_with("new mode")
            || line.starts_with("rename ")
            || line.starts_with("copy ")
            || line.starts_with("Binary files")
            || line.starts_with("GIT binary patch")
            || line == "--- /dev/null"
            || line == "+++ /dev/null"
    }) {
        return Err("Hunk action supports tracked text modifications only".into());
    }
    let hunks = starts
        .iter()
        .enumerate()
        .map(|(i, &start)| &raw[start..starts.get(i + 1).copied().unwrap_or(raw.len())])
        .collect();
    Ok((header, hunks))
}

fn apply_index_patch(root: &Path, patch: &[u8], reverse: bool, check: bool) -> Result<(), String> {
    let mut cmd = command(root);
    cmd.args(["apply", "--cached"]);
    if reverse {
        cmd.arg("--reverse");
    }
    if check {
        cmd.arg("--check");
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    child
        .stdin
        .take()
        .ok_or("Missing Git stdin")?
        .write_all(patch)
        .map_err(|e| e.to_string())?;
    checked(child.wait_with_output().map_err(|e| e.to_string())?).map(|_| ())
}

/// Apply one exact, freshly validated hunk to the index. The worktree is never
/// changed, and a stale preview cannot silently select a different hunk.
pub fn stage_hunk(root: &Path, change: &Change, hunk: &Hunk) -> Result<(), String> {
    if !can_stage_hunk(change, hunk.staged) {
        return Err("Hunk action supports tracked text modifications only".into());
    }
    let mut cmd = command(root);
    cmd.args(["diff", "--no-ext-diff", "--no-textconv"]);
    if hunk.staged {
        cmd.arg("--cached");
    }
    cmd.arg("--").arg(&change.path);
    let (bytes, truncated) = diff_output_bytes(&mut cmd, false)?;
    if truncated {
        return Err("Diff is too large for a safe hunk action".into());
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| "Hunk action requires a UTF-8 text diff".to_string())?;
    let (header, hunks) = split_text_patch(text)?;
    let fresh = hunks
        .get(hunk.ordinal)
        .ok_or("This hunk changed; refresh Source Control")?;
    if *fresh != hunk.raw {
        return Err("This hunk changed; refresh Source Control".into());
    }
    let mut patch = Vec::with_capacity(header.len() + fresh.len());
    patch.extend_from_slice(header.as_bytes());
    patch.extend_from_slice(fresh.as_bytes());
    apply_index_patch(root, &patch, hunk.staged, true)?;
    apply_index_patch(root, &patch, hunk.staged, false)
}

pub fn stage(root: &Path, change: &Change, staged: bool) -> Result<(), String> {
    let mut cmd = command(root);
    if staged {
        cmd.args(["add", "--"]);
    } else if command(root)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .map_err(|e| e.to_string())?
        .status
        .success()
    {
        cmd.args(["restore", "--staged", "--"]);
    } else {
        cmd.args(["rm", "--cached", "--"]);
    }
    cmd.arg(&change.path);
    if let Some(old) = &change.original {
        cmd.arg(old);
    }
    checked(cmd.output().map_err(|e| e.to_string())?).map(|_| ())
}

pub fn commit(root: &Path, message: &str) -> Result<(), String> {
    if message.trim().is_empty() {
        return Err("Write a commit message first".into());
    }
    let mut child = command(root)
        .args(["commit", "--file=-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    child
        .stdin
        .take()
        .ok_or("Missing Git stdin")?
        .write_all(message.as_bytes())
        .map_err(|e| e.to_string())?;
    checked(child.wait_with_output().map_err(|e| e.to_string())?).map(|_| ())
}

/// A local branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    pub current: bool,
    /// Its upstream, such as `origin/main`, when it has one.
    pub upstream: Option<String>,
}

/// Local branches, most recently committed to first.
pub fn branches(root: &Path) -> Result<Vec<Branch>, String> {
    let bytes = run(
        root,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(HEAD)%00%(refname:short)%00%(upstream:short)",
            "refs/heads",
        ],
    )?;
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\0');
            let head = fields.next()?;
            let name = fields.next()?.to_owned();
            let upstream = fields.next().filter(|u| !u.is_empty()).map(str::to_owned);
            Some(Branch {
                name,
                current: head == "*",
                upstream,
            })
        })
        .collect())
}

/// Whether `name` is a branch name Git accepts.
pub fn valid_branch_name(root: &Path, name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && run(root, &["check-ref-format", "--branch", name]).is_ok()
}

/// Switches to `name`. Git refuses when local changes would be overwritten,
/// and that refusal is the answer: nothing is stashed or forced.
pub fn switch(root: &Path, name: &str) -> Result<(), String> {
    run(root, &["switch", "--no-guess", name]).map(|_| ())
}

/// Creates `name` at HEAD and switches to it, keeping local changes.
pub fn create_branch(root: &Path, name: &str) -> Result<(), String> {
    if !valid_branch_name(root, name) {
        return Err(format!("{name:?} is not a valid branch name"));
    }
    run(root, &["switch", "-c", name]).map(|_| ())
}

/// A command that talks to a remote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Remote {
    Fetch,
    /// Fast-forward only: a pull never creates a merge commit or rebases.
    Pull,
    /// Never forced. A branch without an upstream is pushed to `origin`
    /// and tracks it.
    Push,
}

impl Remote {
    pub fn verb(self) -> &'static str {
        match self {
            Remote::Fetch => "Fetching",
            Remote::Pull => "Pulling",
            Remote::Push => "Pushing",
        }
    }
}

/// How long a remote command may take before it is stopped.
const REMOTE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Runs a remote command. `ssh_auth_sock` is the agent socket from the
/// settings: an app started from the Dock does not inherit the shell's, and
/// crc does not read the shell profile to find it. Nothing can prompt:
/// there is no terminal and `GIT_TERMINAL_PROMPT` is 0, so a credential
/// that is missing fails with Git's message instead of hanging.
pub fn remote(root: &Path, what: Remote, ssh_auth_sock: Option<&Path>) -> Result<String, String> {
    let branch = run(root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|b| String::from_utf8_lossy(&b).trim().to_owned())
        .ok();
    let upstream = run(
        root,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .is_ok();
    let mut args: Vec<String> = match what {
        Remote::Fetch => vec!["fetch".into(), "--prune".into()],
        Remote::Pull => vec!["pull".into(), "--ff-only".into()],
        Remote::Push => vec!["push".into()],
    };
    if what == Remote::Push && !upstream {
        let Some(branch) = &branch else {
            return Err("not on a branch: nothing to push".into());
        };
        let remotes = run(root, &["remote"]).unwrap_or_default();
        if !String::from_utf8_lossy(&remotes)
            .lines()
            .any(|r| r == "origin")
        {
            return Err("this branch has no upstream and there is no origin remote".into());
        }
        args.extend(["--set-upstream".into(), "origin".into(), branch.clone()]);
    }
    if what == Remote::Pull && !upstream {
        return Err("this branch has no upstream to pull from".into());
    }
    let mut cmd = command(root);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(sock) = ssh_auth_sock {
        cmd.env("SSH_AUTH_SOCK", sock);
    }
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let started = std::time::Instant::now();
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(_) => break,
            None if started.elapsed() > REMOTE_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "git {} took over two minutes and was stopped",
                    args[0]
                ));
            }
            None => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    let said = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if output.status.success() {
        Ok(said)
    } else {
        Err(if said.is_empty() {
            format!("git {} failed", args[0])
        } else {
            said
        })
    }
}

/// Who last changed a line, and when, for the status line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blame {
    /// `None` for a line not committed yet.
    pub author: Option<String>,
    /// Seconds since the epoch.
    pub time: i64,
    pub summary: String,
    pub commit: String,
}

/// Blames `line` (zero-based) of `path` as the editor has it: `contents` is
/// the document's text, so unsaved edits count as not committed yet.
pub fn blame_line(root: &Path, path: &Path, line: usize, contents: &str) -> Result<Blame, String> {
    let range = format!("{},{}", line + 1, line + 1);
    let mut child = command(root)
        .args([
            "blame",
            "--porcelain",
            "--contents",
            "-",
            "-L",
            &range,
            "--",
        ])
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().ok_or("Missing Git stdin")?;
    let text = contents.to_owned();
    // Written from its own thread: a large file fills the pipe before Git
    // starts answering, and both sides would wait for each other.
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(text.as_bytes());
    });
    let output = checked(child.wait_with_output().map_err(|e| e.to_string())?)?;
    let _ = writer.join();
    parse_blame(&String::from_utf8_lossy(&output)).ok_or_else(|| "no blame".into())
}

fn parse_blame(text: &str) -> Option<Blame> {
    let mut lines = text.lines();
    let commit = lines.next()?.split(' ').next()?.to_owned();
    let mut blame = Blame {
        author: None,
        time: 0,
        summary: String::new(),
        commit: commit.clone(),
    };
    for line in lines {
        if let Some(name) = line.strip_prefix("author ") {
            blame.author = Some(name.to_owned());
        } else if let Some(time) = line.strip_prefix("author-time ") {
            blame.time = time.parse().unwrap_or(0);
        } else if let Some(summary) = line.strip_prefix("summary ") {
            blame.summary = summary.to_owned();
        } else if line.starts_with('\t') {
            break;
        }
    }
    if commit.bytes().all(|b| b == b'0') {
        blame.author = None;
    }
    Some(blame)
}

/// "3 days ago", for a blame.
pub fn ago(time: i64, now: i64) -> String {
    let seconds = (now - time).max(0);
    let (n, unit) = match seconds {
        s if s < 60 => return "just now".into(),
        s if s < 3600 => (s / 60, "minute"),
        s if s < 86_400 => (s / 3600, "hour"),
        s if s < 86_400 * 30 => (s / 86_400, "day"),
        s if s < 86_400 * 365 => (s / (86_400 * 30), "month"),
        s => (s / (86_400 * 365), "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blame_porcelain_parses_and_uncommitted_has_no_author() {
        let committed = "a1b2c3 4 4 1\nauthor Caio\nauthor-mail <x>\nauthor-time 1700000000\nsummary feat: beds\nfilename f\n\tline\n";
        let blame = parse_blame(committed).unwrap();
        assert_eq!(blame.author.as_deref(), Some("Caio"));
        assert_eq!(blame.time, 1_700_000_000);
        assert_eq!(blame.summary, "feat: beds");
        let local = "0000000000000000000000000000000000000000 1 1 1\nauthor Not Committed Yet\nauthor-time 1\nsummary Version of f from -\n\tx\n";
        assert_eq!(parse_blame(local).unwrap().author, None);
        assert_eq!(ago(1000, 1000 + 3 * 86_400), "3 days ago");
        assert_eq!(ago(1000, 1000 + 3600), "1 hour ago");
    }

    #[test]
    fn branches_switch_create_and_blame_in_a_real_repository() {
        let dir = std::env::temp_dir().join(format!("crc-branches-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("f.txt"), "one\ntwo\n").unwrap();
        git(&["add", "f.txt"]);
        git(&["commit", "-q", "-m", "first"]);
        assert!(create_branch(&dir, "feature/x").is_ok());
        assert!(create_branch(&dir, "bad..name").is_err());
        let list = branches(&dir).unwrap();
        assert!(list.iter().any(|b| b.name == "feature/x" && b.current));
        assert!(list.iter().any(|b| b.name == "main" && !b.current));
        assert!(switch(&dir, "main").is_ok());
        assert!(switch(&dir, "nope").is_err());
        let blame = blame_line(&dir, Path::new("f.txt"), 1, "one\ntwo\n").unwrap();
        assert_eq!(blame.author.as_deref(), Some("Test"));
        assert_eq!(blame.summary, "first");
        let edited = blame_line(&dir, Path::new("f.txt"), 1, "one\nTWO\n").unwrap();
        assert_eq!(edited.author, None);
        assert_eq!(
            remote(&dir, Remote::Pull, None).unwrap_err(),
            "this branch has no upstream to pull from"
        );
        assert_eq!(
            remote(&dir, Remote::Push, None).unwrap_err(),
            "this branch has no upstream and there is no origin remote"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_merge_conflict_is_listed_apart_and_ends_with_add() {
        let dir = std::env::temp_dir().join(format!("crc-conflict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "merge.conflictStyle=zdiff3",
                ])
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        };
        assert!(git(&["init", "-q", "-b", "main"]));
        std::fs::write(dir.join("f.txt"), "a\nx = 0\nz\n").unwrap();
        assert!(git(&["add", "f.txt"]));
        assert!(git(&["commit", "-q", "-m", "base"]));
        assert!(git(&["switch", "-q", "-c", "topic"]));
        std::fs::write(dir.join("f.txt"), "a\nx = 2\nz\n").unwrap();
        assert!(git(&["commit", "-q", "-am", "topic"]));
        assert!(git(&["switch", "-q", "main"]));
        std::fs::write(dir.join("f.txt"), "a\nx = 1\nz\n").unwrap();
        assert!(git(&["commit", "-q", "-am", "main"]));
        assert!(!git(&["merge", "-q", "topic"]), "the merge conflicts");

        let s = snapshot(&dir).unwrap();
        assert_eq!(s.in_progress, Some(InProgress::Merge));
        assert_eq!(s.changes.len(), 1);
        let change = &s.changes[0];
        assert_eq!(change.status(), "UU");
        assert!(change.conflicted() && !change.staged() && !change.unstaged());
        let text = std::fs::read_to_string(dir.join("f.txt")).unwrap();
        let rope = crate::text::rope::Rope::from_text(&text);
        let found = crate::project::conflict::parse(&rope);
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].resolution(&rope, crate::project::conflict::Take::Base),
            "x = 0\n"
        );

        std::fs::write(dir.join("f.txt"), "a\nx = 2\nz\n").unwrap();
        stage(&s.root, change, true).unwrap();
        let s = snapshot(&dir).unwrap();
        assert!(s.changes[0].staged() && !s.changes[0].conflicted());
        assert_eq!(s.in_progress, Some(InProgress::Merge), "until committed");
        assert!(git(&["commit", "-q", "--no-edit"]));
        assert_eq!(snapshot(&dir).unwrap().in_progress, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn marks_between(old: &str, new: &str) -> Vec<(usize, MarkKind)> {
        marks(&crate::ide::diff::diff(old, new))
            .into_iter()
            .map(|m| (m.line, m.kind))
            .collect()
    }

    #[test]
    fn gutter_marks_name_additions_modifications_and_deletions() {
        assert_eq!(marks_between("a\nb\nc\n", "a\nb\nc\n"), []);
        assert_eq!(
            marks_between("a\nb\nc\n", "a\nb\nx\ny\nc\n"),
            [(2, MarkKind::Added), (3, MarkKind::Added)]
        );
        assert_eq!(
            marks_between("a\nb\nc\n", "a\nB\nc\n"),
            [(1, MarkKind::Modified)]
        );
        assert_eq!(
            marks_between("a\nb\nc\n", "a\nc\n"),
            [(1, MarkKind::Removed)],
            "the deletion sits above c, which is now line 1"
        );
        assert_eq!(
            marks_between("a\nb\n", "a\n"),
            [(1, MarkKind::Removed)],
            "removed from the end: on the line count"
        );
        assert_eq!(
            marks_between("", "a\nb\n"),
            [(0, MarkKind::Added), (1, MarkKind::Added)],
            "a file HEAD does not have"
        );
        assert_eq!(
            marks_between("a\nb\nc\nd\n", "a\nX\nc\n"),
            [(1, MarkKind::Modified), (3, MarkKind::Removed)],
            "two changes in one file keep their own positions"
        );
    }

    #[test]
    fn parses_spaces_newlines_non_utf8_and_renames() {
        let s = parse_status(
            "/tmp".into(),
            b"## main...origin/main [ahead 2]\0R  new name\0old\nname\0 M weird\xff\0?? -file\0",
        )
        .unwrap();
        assert_eq!(s.changes.len(), 3);
        assert_eq!(
            s.changes[0].original.as_deref(),
            Some(Path::new("old\nname"))
        );
        assert!(s.changes[0].staged());
        assert!(s.changes[1].unstaged());
        assert_eq!(s.changes[2].path, Path::new("-file"));
    }
    #[test]
    fn diff_parsing_drops_command_headers_and_numbers_both_sides() {
        let diff = Diff::parse(concat!(
            "WORKING TREE\n",
            "diff --git a/docs/roadmap.md b/docs/roadmap.md\n",
            "index 7245682..3c8adf8 100644\n",
            "--- a/docs/roadmap.md\n",
            "+++ b/docs/roadmap.md\n",
            "@@ -6,7 +6,7 @@ Originally written from the audit.\n",
            " ## Current checkpoint\n",
            "-Latest code checkpoint: FIX_COMMIT\n",
            "+Latest code checkpoint: 1d72c45\n",
            " UI pass is complete.\n",
        ));
        let kinds: Vec<_> = diff.lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![
                DiffKind::Section,
                DiffKind::Hunk,
                DiffKind::Context,
                DiffKind::Removed,
                DiffKind::Added,
                DiffKind::Context,
            ],
            "command headers should not reach the reader"
        );
        assert_eq!(diff.added, 1);
        assert_eq!(diff.removed, 1);
        // The hunk starts at line 6 on both sides, so the context line above
        // the change is 6, the removal is 7 on the left and the addition is 7
        // on the right, and the context below is 8 on both.
        let at = |i: usize| (diff.lines[i].old, diff.lines[i].new);
        assert_eq!(at(2), (Some(6), Some(6)));
        assert_eq!(at(3), (Some(7), None));
        assert_eq!(at(4), (None, Some(7)));
        assert_eq!(at(5), (Some(8), Some(8)));
        assert_eq!(diff.lines[1].text, "Originally written from the audit.");
        assert_eq!(diff.lines[3].text, "Latest code checkpoint: FIX_COMMIT");
    }

    #[test]
    fn diff_parsing_keeps_notes_and_survives_a_malformed_hunk_header() {
        let diff = Diff::parse(concat!(
            "@@ nonsense @@\n",
            "+added\n",
            "\\ No newline at end of file\n",
            "Binary files a/logo.png and b/logo.png differ\n",
        ));
        assert_eq!(diff.lines[0].kind, DiffKind::Hunk);
        assert_eq!(diff.lines[1].kind, DiffKind::Added);
        assert_eq!(diff.lines[2].kind, DiffKind::Note);
        assert_eq!(diff.lines[3].kind, DiffKind::Note);
        assert_eq!(diff.added, 1);
    }

    #[test]
    fn a_new_file_numbers_only_the_side_it_exists_on() {
        let diff = Diff::parse(concat!(
            "diff --git a/new.txt b/new.txt\n",
            "new file mode 100644\n",
            "--- /dev/null\n",
            "+++ b/new.txt\n",
            "@@ -0,0 +1,2 @@\n",
            "+one\n",
            "+two\n",
        ));
        assert_eq!(diff.lines.len(), 3, "only the hunk and its two additions");
        assert_eq!((diff.lines[1].old, diff.lines[1].new), (None, Some(1)));
        assert_eq!((diff.lines[2].old, diff.lines[2].new), (None, Some(2)));
        assert_eq!(diff.removed, 0);
    }

    #[test]
    fn ignored_paths_are_the_untracked_ones_gitignore_matches() {
        let root = std::env::temp_dir().join(format!("crc-git-ignored-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("dist/nested")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        run(&root, &["init", "--quiet"]).unwrap();
        std::fs::write(root.join(".gitignore"), "dist/\n*.log\n").unwrap();
        std::fs::write(root.join("dist/nested/out.js"), "").unwrap();
        std::fs::write(root.join("src/main.rs"), "").unwrap();
        std::fs::write(root.join("src/debug.log"), "").unwrap();
        std::fs::write(root.join("kept.log"), "").unwrap();
        // Tracked, so not ignored whatever .gitignore says.
        run(&root, &["add", "--force", "kept.log"]).unwrap();

        // Paths keep the spelling they were asked with: temp_dir is under
        // /var, which Git's top level resolves to /private/var.
        let found = ignored(&root).unwrap();
        let expected: std::collections::HashSet<PathBuf> =
            [root.join("dist"), root.join("src/debug.log")]
                .into_iter()
                .collect();
        assert_eq!(
            found, expected,
            "one entry for the folder, none for tracked or plain files"
        );
        let below = ignored(&root.join("src")).unwrap();
        let only: std::collections::HashSet<PathBuf> =
            [root.join("src/debug.log")].into_iter().collect();
        assert_eq!(below, only, "only what is under the directory asked about");
        assert!(ignored(&std::env::temp_dir().join("crc-not-a-repo-at-all")).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn local_staging_diff_and_commit_round_trip() {
        let root = std::env::temp_dir().join(format!("caio-git-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        run(&root, &["init", "--quiet"]).unwrap();
        run(&root, &["config", "user.name", "caio test"]).unwrap();
        run(&root, &["config", "user.email", "test@example.invalid"]).unwrap();
        run(&root, &["config", "commit.gpgsign", "false"]).unwrap();
        run(&root, &["config", "core.hooksPath", ".git/hooks"]).unwrap();
        std::fs::write(root.join("a file.txt"), "hello\n").unwrap();
        let s = snapshot(&root).unwrap();
        assert!(diff(&root, &s.changes[0]).unwrap().contains("+hello"));
        stage(&root, &s.changes[0], true).unwrap();
        let s = snapshot(&root).unwrap();
        assert!(s.changes[0].staged());
        stage(&root, &s.changes[0], false).unwrap();
        assert!(!snapshot(&root).unwrap().changes[0].staged());
        stage(&root, &snapshot(&root).unwrap().changes[0], true).unwrap();
        commit(&root, "Initial personal sample").unwrap();
        assert!(snapshot(&root).unwrap().changes.is_empty());
        std::fs::write(root.join("a file.txt"), "hello again\n").unwrap();
        let s = snapshot(&root).unwrap();
        assert!(diff(&root, &s.changes[0]).unwrap().contains("+hello again"));
        stage(&root, &s.changes[0], true).unwrap();
        stage(&root, &snapshot(&root).unwrap().changes[0], false).unwrap();
        assert!(!snapshot(&root).unwrap().changes[0].staged());
        // A filename that resembles Git pathspec syntax must stage only itself.
        let literal = ":(glob)*.txt";
        std::fs::write(root.join(literal), "literal\n").unwrap();
        let s = snapshot(&root).unwrap();
        let change = s
            .changes
            .iter()
            .find(|c| c.path == Path::new(literal))
            .unwrap();
        stage(&root, change, true).unwrap();
        let s = snapshot(&root).unwrap();
        let staged: Vec<_> = s.changes.iter().filter(|c| c.staged()).collect();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].path, Path::new(literal));
        let large = "line of personal sample content\n".repeat(30_000);
        std::fs::write(root.join("large.txt"), large).unwrap();
        let s = snapshot(&root).unwrap();
        let change = s
            .changes
            .iter()
            .find(|c| c.path == Path::new("large.txt"))
            .unwrap();
        let preview = diff(&root, change).unwrap();
        assert!(preview.contains("truncated at 512 KiB"));
        assert!(preview.len() < 513 * 1024);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stages_and_unstages_one_hunk_without_touching_other_edits() {
        let root = std::env::temp_dir().join(format!("caio-hunk-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        run(&root, &["init", "--quiet"]).unwrap();
        run(&root, &["config", "user.name", "caio test"]).unwrap();
        run(&root, &["config", "user.email", "test@example.invalid"]).unwrap();
        run(&root, &["config", "commit.gpgsign", "false"]).unwrap();
        let path = root.join("personal notes.txt");
        let original: Vec<String> = (1..=24).map(|n| format!("line {n}\n")).collect();
        std::fs::write(&path, original.concat()).unwrap();
        run(&root, &["add", "--", "personal notes.txt"]).unwrap();
        run(&root, &["commit", "--quiet", "-m", "Personal sample"]).unwrap();

        let mut edited = original.clone();
        edited[1] = "first edit\n".into();
        edited[20] = "second edit\n".into();
        std::fs::write(&path, edited.concat()).unwrap();
        let change = snapshot(&root).unwrap().changes.remove(0);
        let preview = Diff::parse(&diff(&root, &change).unwrap());
        assert_eq!(preview.hunks.len(), 2);
        assert!(!preview.hunks[0].staged);
        stage_hunk(&root, &change, &preview.hunks[0]).unwrap();
        let staged = String::from_utf8(run(&root, &["diff", "--cached"]).unwrap()).unwrap();
        let working = String::from_utf8(run(&root, &["diff"]).unwrap()).unwrap();
        assert!(staged.contains("+first edit"));
        assert!(!staged.contains("+second edit"));
        assert!(!working.contains("+first edit"));
        assert!(working.contains("+second edit"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited.concat());

        let change = snapshot(&root).unwrap().changes.remove(0);
        assert!(change.staged() && change.unstaged());
        let preview = Diff::parse(&diff(&root, &change).unwrap());
        let staged_hunk = preview.hunks.iter().find(|h| h.staged).unwrap();
        stage_hunk(&root, &change, staged_hunk).unwrap();
        assert!(run(&root, &["diff", "--cached"]).unwrap().is_empty());
        let remaining = String::from_utf8(run(&root, &["diff"]).unwrap()).unwrap();
        assert!(remaining.contains("+first edit"));
        assert!(remaining.contains("+second edit"));

        let change = snapshot(&root).unwrap().changes.remove(0);
        let second = Diff::parse(&diff(&root, &change).unwrap()).hunks.remove(1);
        stage_hunk(&root, &change, &second).unwrap();
        let staged = String::from_utf8(run(&root, &["diff", "--cached"]).unwrap()).unwrap();
        assert!(!staged.contains("+first edit"));
        assert!(staged.contains("+second edit"));
        let change = snapshot(&root).unwrap().changes.remove(0);
        let staged_hunk = Diff::parse(&diff(&root, &change).unwrap()).hunks.remove(0);
        stage_hunk(&root, &change, &staged_hunk).unwrap();
        assert!(run(&root, &["diff", "--cached"]).unwrap().is_empty());

        let stale = Diff::parse(&diff(&root, &snapshot(&root).unwrap().changes[0]).unwrap())
            .hunks
            .remove(0);
        edited[1] = "changed again\n".into();
        std::fs::write(&path, edited.concat()).unwrap();
        assert!(stage_hunk(&root, &change, &stale).is_err());
        assert!(run(&root, &["diff", "--cached"]).unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hunk_patch_keeps_crlf_bytes() {
        let root = std::env::temp_dir().join(format!("caio-crlf-hunk-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        run(&root, &["init", "--quiet"]).unwrap();
        run(&root, &["config", "core.autocrlf", "false"]).unwrap();
        run(&root, &["config", "user.name", "caio test"]).unwrap();
        run(&root, &["config", "user.email", "test@example.invalid"]).unwrap();
        run(&root, &["config", "commit.gpgsign", "false"]).unwrap();
        let path = root.join("crlf notes.txt");
        let mut lines: Vec<String> = (1..=20).map(|n| format!("line {n}\r\n")).collect();
        std::fs::write(&path, lines.concat()).unwrap();
        run(&root, &["add", "--", "crlf notes.txt"]).unwrap();
        run(&root, &["commit", "--quiet", "-m", "Personal CRLF sample"]).unwrap();
        lines[1] = "first edit\r\n".into();
        lines[17] = "second edit\r\n".into();
        std::fs::write(&path, lines.concat()).unwrap();
        let change = snapshot(&root).unwrap().changes.remove(0);
        let diff = Diff::parse(&diff(&root, &change).unwrap());
        assert_eq!(diff.hunks.len(), 2);
        assert!(diff.hunks[0].raw.contains("+first edit\r\n"));
        stage_hunk(&root, &change, &diff.hunks[0]).unwrap();
        let staged = run(&root, &["diff", "--cached"]).unwrap();
        assert!(
            staged
                .windows(b"+first edit\r\n".len())
                .any(|w| w == b"+first edit\r\n")
        );
        assert!(
            !staged
                .windows(b"+second edit\r\n".len())
                .any(|w| w == b"+second edit\r\n")
        );
        assert_eq!(std::fs::read(&path).unwrap(), lines.concat().as_bytes());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hunk_patch_keeps_missing_final_newline() {
        let root = std::env::temp_dir().join(format!("caio-eof-hunk-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        run(&root, &["init", "--quiet"]).unwrap();
        run(&root, &["config", "user.name", "caio test"]).unwrap();
        run(&root, &["config", "user.email", "test@example.invalid"]).unwrap();
        run(&root, &["config", "commit.gpgsign", "false"]).unwrap();
        let path = root.join("no-final-newline.txt");
        std::fs::write(&path, "alpha\nbeta").unwrap();
        run(&root, &["add", "--", "no-final-newline.txt"]).unwrap();
        run(&root, &["commit", "--quiet", "-m", "Personal EOF sample"]).unwrap();
        std::fs::write(&path, "alpha\nchanged").unwrap();
        let change = snapshot(&root).unwrap().changes.remove(0);
        let preview = Diff::parse(&diff(&root, &change).unwrap());
        assert!(
            preview.hunks[0]
                .raw
                .contains("\\ No newline at end of file\n")
        );
        stage_hunk(&root, &change, &preview.hunks[0]).unwrap();
        assert!(run(&root, &["diff"]).unwrap().is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), b"alpha\nchanged");
        std::fs::remove_dir_all(root).unwrap();
    }
}
