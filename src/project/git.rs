//! Local Git plumbing. Paths remain OS strings; display labels may be lossy.
//! Call on workers, never on the event/render thread. No network operations.
use crate::platform::path_from_bytes;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
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
    /// HEAD's full message, for Amend to start from; `None` before the
    /// first commit, or until [`read_head`] has asked.
    pub head_message: Option<String>,
    /// Whether HEAD is already on its upstream (see [`head_is_pushed`]).
    pub head_pushed: bool,
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

    /// Its command: `git merge`, `git rebase`, ...
    pub fn command(self) -> &'static str {
        match self {
            InProgress::Merge => "merge",
            InProgress::Rebase => "rebase",
            InProgress::CherryPick => "cherry-pick",
            InProgress::Revert => "revert",
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
        // Lines still to come in the current hunk, old and new side, from
        // its header. Inside a hunk every line is content, even one that
        // reads `--- a comment` or `diff --git`.
        let mut remaining = (0usize, 0usize);
        // Prefix columns: one, or one per parent in a combined (`@@@`)
        // diff of an unmerged path.
        let mut columns = 1;
        // A header without counts: the hunk ends at the first line that
        // does not look like content.
        let mut uncounted = false;
        for raw_line in text.split_terminator('\n') {
            // Preserve CRLF bytes for `git apply`, while the visible diff
            // remains the same text without the line terminator.
            let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
            if line == "… Diff preview truncated at 512 KiB." {
                diff.truncated = true;
                active_hunk = None;
                remaining = (0, 0);
                diff.push(DiffKind::Note, None, None, line);
                continue;
            }
            if uncounted && !line.starts_with([' ', '+', '-', '\\']) {
                remaining = (0, 0);
                uncounted = false;
            }
            if remaining != (0, 0) {
                let prefix: String = line.chars().take(columns).collect();
                let body = line.get(prefix.len()..).unwrap_or("");
                if let Some(index) = active_hunk {
                    let hunk: &mut Hunk = &mut diff.hunks[index];
                    hunk.raw.push_str(raw_line);
                    hunk.raw.push('\n');
                }
                if line.starts_with('\\') {
                    diff.push(DiffKind::Note, None, None, line);
                } else if prefix.contains('-') {
                    remaining.0 = remaining.0.saturating_sub(1);
                    old += 1;
                    diff.push(DiffKind::Removed, Some(old), None, body);
                } else if prefix.contains('+') {
                    remaining.1 = remaining.1.saturating_sub(1);
                    new += 1;
                    diff.push(DiffKind::Added, None, Some(new), body);
                } else {
                    // A space, or nothing where an editor stripped it.
                    remaining = (remaining.0.saturating_sub(1), remaining.1.saturating_sub(1));
                    old += 1;
                    new += 1;
                    diff.push(DiffKind::Context, Some(old), Some(new), body);
                }
                continue;
            }
            if let Some(after) = line.strip_prefix("@@") {
                // `@@ -12,7 +12,9 @@ fn name()` — the trailing text is the
                // enclosing context git found, which is worth keeping. A
                // combined diff has one more `@` per extra parent.
                let extra = after.len() - after.trim_start_matches('@').len();
                let rest = &after[extra..];
                let fence = "@".repeat(2 + extra);
                let (counts, caption) = rest.split_once(fence.as_str()).unwrap_or((rest, ""));
                columns = 1 + extra;
                (old, new) = hunk_start(counts);
                (remaining, uncounted) = match hunk_lengths(counts) {
                    Some(lengths) => (lengths, false),
                    None => ((usize::MAX, usize::MAX), true),
                };
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
            if COMMAND_HEADERS.iter().any(|h| line.starts_with(h)) {
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
            // "\ No newline at end of file" after a hunk, "Binary files
            // ... differ", git's own remarks.
            if line.starts_with('\\')
                && let Some(index) = active_hunk
            {
                diff.hunks[index].raw.push_str(raw_line);
                diff.hunks[index].raw.push('\n');
            }
            diff.push(DiffKind::Note, None, None, line);
        }
        diff
    }
}

/// The `-12,7 +12,9` of a hunk header, as the number of lines on each side
/// (a missing count is one line). The old side of a combined diff is the
/// longest of its parents.
fn hunk_lengths(counts: &str) -> Option<(usize, usize)> {
    let mut old = None;
    let mut new = None;
    for field in counts.split_whitespace() {
        let Some(rest) = field.strip_prefix(['-', '+']) else {
            continue;
        };
        let length = match rest.split_once(',') {
            Some((start, n)) => start.parse::<usize>().and(n.parse::<usize>()).ok(),
            None => rest.parse::<usize>().ok().map(|_| 1),
        };
        let Some(length) = length else {
            continue;
        };
        if field.starts_with('-') {
            old = Some(old.unwrap_or(0).max(length));
        } else {
            new = Some(length);
        }
    }
    Some((old?, new?))
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

pub(super) fn command(root: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(root)
        .args(["-c", "core.fsmonitor=false", "-c", "color.ui=false"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        // Messages are shown, not parsed, but keep them the same everywhere.
        .env("LC_ALL", "C")
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

/// One line a commit would add: the file, its line number there, and
/// the text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Added {
    pub path: String,
    pub line: usize,
    pub text: String,
}

/// The lines the index adds over HEAD: what committing now would add.
pub fn staged_additions(root: &Path) -> Result<Vec<Added>, String> {
    let out = run(
        root,
        &[
            "diff",
            "--cached",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
            "-U0",
        ],
    )?;
    Ok(parse_additions(&String::from_utf8_lossy(&out)))
}

fn parse_additions(diff: &str) -> Vec<Added> {
    let mut added = Vec::new();
    let mut path = String::new();
    let mut line = 0;
    for row in diff.lines() {
        if let Some(name) = row.strip_prefix("+++ ") {
            path = name.strip_prefix("b/").unwrap_or(name).to_owned();
        } else if let Some(counts) = row.strip_prefix("@@ ") {
            line = counts
                .split(' ')
                .find_map(|c| c.strip_prefix('+'))
                .and_then(|c| c.split(',').next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
        } else if let Some(text) = row.strip_prefix('+') {
            added.push(Added {
                path: path.clone(),
                line,
                text: text.to_owned(),
            });
            line += 1;
        }
    }
    added
}

/// Subjects of the commits on HEAD made after `since` (Unix seconds),
/// newest first, at most `limit`.
pub fn commits_since(root: &Path, since: u64, limit: usize) -> Result<Vec<String>, String> {
    // Git counts a commit made in the same second as `since`; it is not
    // after it.
    let since = format!("--since=@{}", since.saturating_add(1));
    let limit = format!("--max-count={limit}");
    let out = run(root, &["log", &since, &limit, "--format=%s"])?;
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect())
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
    if root.is_empty() {
        return Err("Git returned no path".into());
    }
    Ok(path_from_bytes(root))
}

/// The Git directories of the repository `directory` is in: its own and,
/// for a linked worktree, the common one. Empty outside a repository.
pub fn git_dirs(directory: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for args in [
        &["rev-parse", "--absolute-git-dir"][..],
        &["rev-parse", "--path-format=absolute", "--git-common-dir"][..],
    ] {
        if let Ok(mut bytes) = run(directory, args) {
            if bytes.last() == Some(&b'\n') {
                bytes.pop();
            }
            let dir = path_from_bytes(bytes);
            if !dir.as_os_str().is_empty() && !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    dirs
}

/// The text of `path` as of HEAD, or `None` when HEAD has no such file:
/// a new file, or a repository with no commits yet. Line endings are
/// normalised to what the editor keeps in its rope, so a CRLF file does
/// not read as changed on every line.
pub fn head_text(root: &Path, path: &Path) -> Result<Option<String>, String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "outside the repository".to_string())?;
    Ok(rev_bytes(root, "HEAD", relative)?.map(|bytes| {
        // Line breaks as the buffer has them: CRLF and a lone CR are LF.
        let text = String::from_utf8_lossy(&bytes);
        crate::text::buffer::normalize_newlines(&text).into_owned()
    }))
}

/// The bytes of `relative` as of `rev`, or `None` when that commit has no
/// such file or there is no such commit (a repository with no commits yet
/// included).
pub fn rev_bytes(root: &Path, rev: &str, relative: &Path) -> Result<Option<Vec<u8>>, String> {
    let spec = format!("{rev}:{}", relative.to_string_lossy());
    let output = command(root)
        .args(["show", &spec])
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        return Ok(Some(output.stdout));
    }
    // Why it failed, from exit statuses rather than from git's wording,
    // which changes between versions and languages. No commit yet, or no
    // such file in it: the whole file is new.
    let succeeds = |args: &[&str]| {
        command(root)
            .args(args)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if !succeeds(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("{rev}^{{commit}}"),
    ]) || !succeeds(&["cat-file", "-e", &spec])
    {
        return Ok(None);
    }
    Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
}

/// The commit `tree`'s HEAD and `base` last had in common.
pub fn merge_base(tree: &Path, base: &str) -> Result<String, String> {
    let out = run(tree, &["merge-base", base, "HEAD"])?;
    let id = String::from_utf8_lossy(&out).trim().to_owned();
    if id.is_empty() {
        return Err(format!("no commit in common with {base}"));
    }
    Ok(id)
}

/// A file that differs from a commit, as `git diff --name-status` says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changed {
    /// Relative to the repository.
    pub path: PathBuf,
    /// `A`, `M`, `D`, `R`, `C`, `T`, or `?` for a file Git does not track.
    pub kind: char,
    /// Where a renamed or copied file came from.
    pub from: Option<PathBuf>,
}

/// Every file in `tree` that differs from `rev`: what is committed since,
/// staged, and not staged yet, and the files Git does not track. Sorted
/// by path.
pub fn changed_since(tree: &Path, rev: &str) -> Result<Vec<Changed>, String> {
    let tracked = run(
        tree,
        &["diff", "--name-status", "-z", "-M", "--no-ext-diff", rev],
    )?;
    let mut changed = parse_name_status(&tracked);
    let untracked = run(tree, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    changed.extend(
        untracked
            .split(|b| *b == 0)
            .filter(|r| !r.is_empty())
            .map(|r| Changed {
                path: path_from_bytes(r),
                kind: '?',
                from: None,
            }),
    );
    changed.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(changed)
}

/// `git diff --name-status -z` output: `X\0path\0`, and for a rename or
/// copy `R087\0old\0new\0`.
pub fn parse_name_status(bytes: &[u8]) -> Vec<Changed> {
    let mut fields = bytes.split(|b| *b == 0);
    let mut changed = Vec::new();
    while let Some(status) = fields.next() {
        let Some(&kind) = status.first() else {
            continue;
        };
        let kind = kind as char;
        let Some(first) = fields.next() else {
            break;
        };
        if matches!(kind, 'R' | 'C') {
            let Some(second) = fields.next() else {
                break;
            };
            changed.push(Changed {
                path: path_from_bytes(second),
                kind,
                from: Some(path_from_bytes(first)),
            });
        } else {
            changed.push(Changed {
                path: path_from_bytes(first),
                kind,
                from: None,
            });
        }
    }
    changed
}

/// What Git holds for a file in the index.
#[derive(Debug, PartialEq, Eq)]
pub enum Indexed {
    /// Its staged bytes.
    Tracked(Vec<u8>),
    /// Not in the index and not ignored: a new file.
    Untracked,
    /// Matched by a .gitignore: build output and the like.
    Ignored,
}

/// The staged copy of `path`, or why there is none. `Err` outside a
/// repository.
pub fn index_bytes(path: &Path) -> Result<Indexed, String> {
    let dir = path.parent().ok_or("no folder")?;
    let root = crate::platform::canonical(&toplevel(dir)?);
    let relative = path
        .strip_prefix(&root)
        .map_err(|_| "outside the repository".to_string())?;
    let relative = relative.to_string_lossy();
    let output = command(&root)
        .args(["show", &format!(":{relative}")])
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        return Ok(Indexed::Tracked(output.stdout));
    }
    let ignored = command(&root)
        .args(["check-ignore", "-q", "--", &relative])
        .output()
        .is_ok_and(|o| o.status.success());
    Ok(if ignored {
        Indexed::Ignored
    } else {
        Indexed::Untracked
    })
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

/// Gutter marks for the changes from HEAD to the current text.
///
/// Each line a change holds is marked with its kind: modified when the
/// change removed anything, added otherwise. A change with nothing added is
/// one deletion mark, on the next line that survived.
pub fn marks(changes: &[crate::ide::diff::Change]) -> Vec<Mark> {
    let mut out = Vec::new();
    for change in changes {
        if change.kind == MarkKind::Removed {
            out.push(Mark {
                line: change.new_start,
                kind: MarkKind::Removed,
            });
        } else {
            out.extend((change.new_start..change.new_end).map(|line| Mark {
                line,
                kind: change.kind,
            }));
        }
    }
    out
}

pub fn snapshot(directory: &Path) -> Result<Snapshot, String> {
    // The Git directory, which is not `<root>/.git` in a linked worktree.
    // One question per call: a path may itself contain a newline, so two
    // answers on two lines cannot be told apart.
    let answer = |args: &[&str]| -> Result<PathBuf, String> {
        let mut bytes = run(directory, args)?;
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        if bytes.is_empty() {
            return Err("Git returned no path".into());
        }
        Ok(path_from_bytes(bytes))
    };
    let root = toplevel(directory)?;
    let in_progress = answer(&["rev-parse", "--absolute-git-dir"])
        .ok()
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
        let path = path_from_bytes(&field[3..]);
        let original = if [index, worktree].iter().any(|c| matches!(c, b'R' | b'C')) {
            Some(path_from_bytes(
                fields.next().ok_or("Missing rename source")?,
            ))
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
        head_message: None,
        head_pushed: false,
    })
}

/// Fills in what Amend needs to know about HEAD: its message, and whether
/// it is already pushed. Asked only where Source Control refreshes, and
/// only what the status header leaves open: no HEAD commit means no
/// message, no live upstream means not pushed.
pub fn read_head(snapshot: &mut Snapshot) {
    let header = &snapshot.branch;
    if header.starts_with("No commits yet on ") || header.starts_with("Initial commit on ") {
        return;
    }
    snapshot.head_message = head_message(&snapshot.root).ok();
    snapshot.head_pushed =
        header.contains("...") && !header.ends_with("[gone]") && head_is_pushed(&snapshot.root);
}

/// HEAD's full message, without the newline Git ends it with.
pub fn head_message(root: &Path) -> Result<String, String> {
    let out = run(root, &["log", "-1", "--format=%B"])?;
    Ok(String::from_utf8_lossy(&out).trim_end().to_owned())
}

/// Whether HEAD is already in its upstream, so amending it would rewrite a
/// published commit. No upstream, or one whose branch is gone, is not.
pub fn head_is_pushed(root: &Path) -> bool {
    run(root, &["merge-base", "--is-ancestor", "HEAD", "@{u}"]).is_ok()
}

/// Lines of a diff that name the command's arguments, not the change.
const COMMAND_HEADERS: [&str; 11] = [
    "diff --git ",
    "index ",
    "--- ",
    "+++ ",
    "new file mode",
    "deleted file mode",
    "old mode",
    "new mode",
    "similarity index",
    "rename from",
    "rename to",
];

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
    if change.conflicted() {
        // `git diff` of an unmerged path is a combined diff and `--cached`
        // only says "Unmerged path". Against HEAD it is an ordinary diff of
        // the file as it stands, markers and all.
        let mut cmd = command(root);
        cmd.args(["diff", "--no-ext-diff", "--no-textconv", "HEAD", "--"])
            .arg(&change.path);
        let preview = diff_output(cmd, false)?;
        return Ok(format!("WORKING TREE\n\n{preview}\n"));
    }
    let mut text = String::new();
    for (staged, title) in [(true, "STAGED"), (false, "WORKING TREE")] {
        let mut cmd = diff_command(root, &change.path, staged);
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

/// `git diff` of one path, of the index against HEAD when `staged`, else of
/// the worktree against the index. More paths may follow.
fn diff_command(root: &Path, path: &Path, staged: bool) -> Command {
    let mut cmd = command(root);
    cmd.args(["diff", "--no-ext-diff", "--no-textconv"]);
    if staged {
        cmd.arg("--cached");
    }
    cmd.arg("--").arg(path);
    cmd
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
    run_with_stdin(&mut cmd, patch.to_vec()).map(|_| ())
}

/// Runs `cmd` with `input` on its standard input and answers its output, or
/// Git's own error. The input is written from its own thread: a large one
/// fills the pipe before Git starts answering, and both sides would wait
/// for each other. A write cut short because Git exited is Git's error to
/// report, not the pipe's.
fn run_with_stdin(cmd: &mut Command, input: Vec<u8>) -> Result<Vec<u8>, String> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().ok_or("Missing Git stdin")?;
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = checked(child.wait_with_output().map_err(|e| e.to_string())?)?;
    match writer.join() {
        Ok(Ok(())) => Ok(output),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("the Git input thread stopped".into()),
    }
}

/// Apply one exact, freshly validated hunk to the index. The worktree is never
/// changed, and a stale preview cannot silently select a different hunk.
pub fn stage_hunk(root: &Path, change: &Change, hunk: &Hunk) -> Result<(), String> {
    if !can_stage_hunk(change, hunk.staged) {
        return Err("Hunk action supports tracked text modifications only".into());
    }
    let mut cmd = diff_command(root, &change.path, hunk.staged);
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

/// Applies chosen lines of hunks of one file to the index, as `git add -p`
/// does with an edited hunk: each hunk as the preview showed it, and the
/// indexes of the chosen lines among the lines under its header. Staging
/// drops the additions not chosen and keeps the removals not chosen as
/// context; unstaging (the hunks are staged ones) does the opposite. Every
/// hunk is checked against a fresh diff first, as [`stage_hunk`] does, and
/// the worktree is never changed.
pub fn stage_hunk_lines(
    root: &Path,
    change: &Change,
    picks: &[(Hunk, Vec<usize>)],
) -> Result<(), String> {
    let staged = picks.first().ok_or("No lines are selected")?.0.staged;
    if picks.iter().any(|(hunk, _)| hunk.staged != staged) {
        return Err("Select lines from the staged or the unstaged change, not both".into());
    }
    if !can_stage_hunk(change, staged) {
        return Err("Hunk action supports tracked text modifications only".into());
    }
    let mut cmd = diff_command(root, &change.path, staged);
    let (bytes, truncated) = diff_output_bytes(&mut cmd, false)?;
    if truncated {
        return Err("Diff is too large for a safe hunk action".into());
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| "Hunk action requires a UTF-8 text diff".to_string())?;
    let (header, hunks) = split_text_patch(text)?;
    let mut sorted: Vec<&(Hunk, Vec<usize>)> = picks.iter().collect();
    sorted.sort_by_key(|(hunk, _)| hunk.ordinal);
    sorted.dedup_by_key(|(hunk, _)| hunk.ordinal);
    let mut patch = header.to_owned();
    let mut chosen = false;
    for (hunk, selected) in sorted {
        let fresh = hunks
            .get(hunk.ordinal)
            .ok_or("This hunk changed; refresh Source Control")?;
        if *fresh != hunk.raw {
            return Err("This hunk changed; refresh Source Control".into());
        }
        let (part, any) = partial_hunk(fresh, selected, staged)?;
        if any {
            patch.push_str(&part);
            chosen = true;
        }
    }
    if !chosen {
        return Err("No added or removed line is selected".into());
    }
    apply_index_patch(root, patch.as_bytes(), staged, true)?;
    apply_index_patch(root, patch.as_bytes(), staged, false)
}

/// One hunk cut down to the `selected` lines (indexes among the lines under
/// its header), with its header counted again, and whether any added or
/// removed line is left. `reverse` is for a patch applied backwards, to
/// unstage: there the additions not chosen stay as context instead of the
/// removals. A `\ No newline at end of file` goes with the line before it.
fn partial_hunk(raw: &str, selected: &[usize], reverse: bool) -> Result<(String, bool), String> {
    const STALE: &str = "This hunk changed; refresh Source Control";
    let mut lines = raw.split_inclusive('\n');
    let header = lines.next().ok_or(STALE)?;
    let counts = header.strip_prefix("@@ -").ok_or(STALE)?;
    let (counts, caption) = counts.split_once(" @@").ok_or(STALE)?;
    let (old, new) = counts.split_once(" +").ok_or(STALE)?;
    let old_start = old.split(',').next().unwrap_or(old);
    let new_start = new.split(',').next().unwrap_or(new);
    let mut selected = selected.to_vec();
    selected.sort_unstable();
    let mut body = String::new();
    let (mut old_len, mut new_len) = (0usize, 0usize);
    let mut any = false;
    // The prefix of the line last written, and whether a `\` line said
    // it ends its side without a newline: then only an addition may follow
    // a removal, and nothing may follow anything else.
    let mut last = None;
    let mut ended = false;
    for (index, line) in lines.enumerate() {
        let first = line.as_bytes().first().copied();
        if first == Some(b'\\') {
            // With the line before it, written or dropped.
            if last.is_some() {
                body.push_str(line);
                ended = true;
            }
            continue;
        }
        let chosen = selected.binary_search(&index).is_ok();
        let prefix = match first {
            Some(b'+' | b'-') if chosen => first,
            // Not chosen: the side the patch starts from keeps the line.
            Some(b'+') if reverse => Some(b' '),
            Some(b'-') if !reverse => Some(b' '),
            Some(b'+' | b'-') => None,
            _ => Some(b' '),
        };
        let Some(prefix) = prefix else {
            last = None;
            continue;
        };
        if ended && !(last == Some(b'-') && prefix == b'+') {
            return Err(
                "These lines cannot be split from the end of the file; stage the whole hunk".into(),
            );
        }
        ended = false;
        match prefix {
            b'+' => new_len += 1,
            b'-' => old_len += 1,
            _ => {
                old_len += 1;
                new_len += 1;
            }
        }
        any |= prefix != b' ';
        if first == Some(prefix) {
            body.push_str(line);
        } else {
            // A changed line kept as context, or one written without its
            // space by something that stripped it.
            body.push(' ');
            body.push_str(
                line.get(usize::from(matches!(first, Some(b'+' | b'-')))..)
                    .unwrap_or(""),
            );
        }
        last = Some(prefix);
    }
    Ok((
        format!("@@ -{old_start},{old_len} +{new_start},{new_len} @@{caption}{body}"),
        any,
    ))
}

/// Lines of context around a change staged from the editor, as `git diff`
/// writes them.
const TEXT_HUNK_CONTEXT: usize = 3;

/// Stages one change the gutter marks: `change` from `head`, the file as of
/// HEAD, to `text`, the editor's text of it, applied to the index alone.
/// The patch is built from those two texts, so what stages is what the
/// editor shows, unsaved lines included. The index must still have HEAD's
/// lines around the change; when it does not, nothing is staged.
pub fn stage_text_hunk(
    root: &Path,
    relative: &Path,
    head: &str,
    text: &str,
    change: &crate::ide::diff::Change,
) -> Result<(), String> {
    const ELSEWHERE: &str = "stage from Source Control";
    // The editor's texts have LF line breaks; the bytes in Git may not.
    if rev_bytes(root, "HEAD", relative)?.is_some_and(|bytes| bytes.contains(&b'\r')) {
        return Err(format!(
            "cannot stage one change of a file with CRLF line breaks; {ELSEWHERE}"
        ));
    }
    let path = relative
        .to_str()
        .ok_or("cannot stage a change in a file whose name is not UTF-8")?;
    let patch = text_hunk_patch(path, head, text, change)
        .ok_or("cannot stage: the change no longer fits the text")?;
    if let Err(error) = apply_index_patch(root, patch.as_bytes(), false, true) {
        // Why it does not apply, in words that say what to do instead.
        let succeeds = |args: &[&str]| {
            command(root)
                .args(args)
                .arg(relative)
                .output()
                .is_ok_and(|o| o.status.success())
        };
        if !succeeds(&["ls-files", "--error-unmatch", "--"]) {
            return Err(format!(
                "cannot stage: this file is not tracked; {ELSEWHERE}"
            ));
        }
        if !succeeds(&["diff", "--cached", "--quiet", "--"]) {
            return Err(format!(
                "cannot stage: the index differs from HEAD for this file; {ELSEWHERE}"
            ));
        }
        return Err(error);
    }
    apply_index_patch(root, patch.as_bytes(), false, false)
}

/// The patch [`stage_text_hunk`] applies: one hunk, with HEAD's lines as
/// context, numbered for a file that has only this change. `None` when the
/// change does not fit the texts.
fn text_hunk_patch(
    path: &str,
    head: &str,
    text: &str,
    change: &crate::ide::diff::Change,
) -> Option<String> {
    let old: Vec<&str> = head.split_inclusive('\n').collect();
    let new: Vec<&str> = text.split_inclusive('\n').collect();
    if change.old_start > change.old_end
        || change.old_end > old.len()
        || change.new_start > change.new_end
        || change.new_end > new.len()
    {
        return None;
    }
    let before = change.old_start.min(TEXT_HUNK_CONTEXT);
    let after = (old.len() - change.old_end).min(TEXT_HUNK_CONTEXT);
    let first = change.old_start - before;
    let old_count = before + (change.old_end - change.old_start) + after;
    let new_count = before + (change.new_end - change.new_start) + after;
    // A side with no lines names the line before it, `0` at the top.
    let range = |count: usize| match count {
        0 => format!("{first},0"),
        1 => format!("{}", first + 1),
        n => format!("{},{n}", first + 1),
    };
    let (a, b) = (
        quoted_path(&format!("a/{path}")),
        quoted_path(&format!("b/{path}")),
    );
    let mut patch = format!(
        "diff --git {a} {b}\n--- {a}\n+++ {b}\n@@ -{} +{} @@\n",
        range(old_count),
        range(new_count)
    );
    let mut push = |sign: char, line: &str| {
        patch.push(sign);
        patch.push_str(line);
        if !line.ends_with('\n') {
            patch.push_str("\n\\ No newline at end of file\n");
        }
    };
    old[first..change.old_start]
        .iter()
        .for_each(|l| push(' ', l));
    old[change.old_start..change.old_end]
        .iter()
        .for_each(|l| push('-', l));
    new[change.new_start..change.new_end]
        .iter()
        .for_each(|l| push('+', l));
    old[change.old_end..change.old_end + after]
        .iter()
        .for_each(|l| push(' ', l));
    Some(patch)
}

/// A path as a patch header writes it: as is, or in double quotes with C
/// escapes when it holds a quote, a backslash or a control character.
fn quoted_path(path: &str) -> String {
    if !path
        .chars()
        .any(|c| c == '"' || c == '\\' || c.is_ascii_control())
    {
        return path.to_owned();
    }
    let mut out = String::from("\"");
    for c in path.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            c if c.is_ascii_control() => out.push_str(&format!("\\{:03o}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
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
        // No commit yet: out of the index. `-f`, or git refuses a file
        // changed since it was added; with `--cached` the file is untouched.
        cmd.args(["rm", "--cached", "-f", "--"]);
    }
    cmd.arg(&change.path);
    // Unstaging a rename restores both names. Staging one needs the old
    // name only while it is still on disk: after `git mv` it is in neither
    // the tree nor the index, and `git add` refuses the whole command.
    if let Some(old) = &change.original
        && (!staged || root.join(old).exists())
    {
        cmd.arg(old);
    }
    checked(cmd.output().map_err(|e| e.to_string())?).map(|_| ())
}

/// Commits what is staged with `message`; with `amend`, replaces HEAD
/// instead, which with nothing staged rewords it.
pub fn commit(root: &Path, message: &str, amend: bool) -> Result<(), String> {
    if message.trim().is_empty() {
        return Err("Write a commit message first".into());
    }
    let mut cmd = command(root);
    cmd.args(["commit", "--file=-"]);
    if amend {
        cmd.arg("--amend");
    }
    run_with_stdin(&mut cmd, message.as_bytes().to_vec()).map(|_| ())
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

/// Branches with commits HEAD does not have: deleting one loses them.
pub fn unmerged_branches(root: &Path) -> Result<Vec<String>, String> {
    let bytes = run(
        root,
        &[
            "for-each-ref",
            "--no-merged=HEAD",
            "--format=%(refname:short)",
            "refs/heads",
        ],
    )?;
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .map(str::to_owned)
        .collect())
}

/// Deletes the branch `name`. Without `force` Git refuses one whose commits
/// are on neither its upstream nor HEAD, and that refusal is the answer.
pub fn delete_branch(root: &Path, name: &str, force: bool) -> Result<(), String> {
    let flag = if force { "-D" } else { "-d" };
    run(root, &["branch", flag, "--", name]).map(|_| ())
}

/// Renames the branch `old` to `new`, keeping its upstream and reflog. Git
/// refuses a name another branch has.
pub fn rename_branch(root: &Path, old: &str, new: &str) -> Result<(), String> {
    if !valid_branch_name(root, new) {
        return Err(format!("{new:?} is not a valid branch name"));
    }
    run(root, &["branch", "-m", "--", old, new]).map(|_| ())
}

/// Gives up the merge, rebase, cherry-pick or revert under way, putting
/// the branch and files back as they were before it.
pub fn abort(root: &Path, what: InProgress) -> Result<(), String> {
    run(root, &[what.command(), "--abort"]).map(|_| ())
}

/// Goes on with a rebase whose conflicts are resolved and added. The
/// commit keeps the message it had; no editor opens.
pub fn continue_rebase(root: &Path) -> Result<(), String> {
    checked(
        command(root)
            .env("GIT_EDITOR", "true")
            .args(["rebase", "--continue"])
            .output()
            .map_err(|e| e.to_string())?,
    )
    .map(|_| ())
}

/// A command that talks to a remote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Remote {
    Fetch,
    /// Fast-forward only: a pull never creates a merge commit or rebases.
    Pull,
    /// For branches that have diverged: your commits replayed on top.
    PullRebase,
    /// For branches that have diverged: a merge commit, its message Git's.
    PullMerge,
    /// Never forced. A branch without an upstream is pushed to `origin`
    /// and tracks it.
    Push,
}

impl Remote {
    pub fn verb(self) -> &'static str {
        match self {
            Remote::Fetch => "Fetching",
            Remote::Pull | Remote::PullRebase | Remote::PullMerge => "Pulling",
            Remote::Push => "Pushing",
        }
    }
}

/// The arguments for Git that do `what` on the branch checked out
/// (`branch`, `None` when HEAD is detached), which has an upstream or not.
/// A push without one goes to `origin` and tracks it, when `origin` says
/// there is one; a pull without one is refused.
pub fn remote_args(
    what: Remote,
    branch: Option<&str>,
    upstream: bool,
    origin: impl FnOnce() -> bool,
) -> Result<Vec<String>, String> {
    let mut args: Vec<String> = match what {
        Remote::Fetch => vec!["fetch".into(), "--prune".into()],
        Remote::Pull => vec!["pull".into(), "--ff-only".into()],
        Remote::PullRebase => vec!["pull".into(), "--rebase".into()],
        Remote::PullMerge => vec!["pull".into(), "--no-rebase".into(), "--no-edit".into()],
        Remote::Push => vec!["push".into()],
    };
    if what == Remote::Push && !upstream {
        let Some(branch) = branch else {
            return Err("not on a branch: nothing to push".into());
        };
        if !origin() {
            return Err("this branch has no upstream and there is no origin remote".into());
        }
        args.extend(["--set-upstream".into(), "origin".into(), branch.to_owned()]);
    }
    let pull = matches!(what, Remote::Pull | Remote::PullRebase | Remote::PullMerge);
    if pull && !upstream {
        return Err("this branch has no upstream to pull from".into());
    }
    Ok(args)
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
    let args = remote_args(what, branch.as_deref(), upstream, || {
        let remotes = run(root, &["remote"]).unwrap_or_default();
        String::from_utf8_lossy(&remotes)
            .lines()
            .any(|r| r == "origin")
    })?;
    let mut cmd = command(root);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(sock) = ssh_auth_sock {
        cmd.env("SSH_AUTH_SOCK", sock);
    }
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    // Both pipes are read while it runs: a fetch of many refs or a push
    // whose server talks a lot fills a pipe's 64 KiB and would wait on us
    // for ever. Only the end of stderr is kept; that is where Git says
    // what happened.
    // Handed back over a channel: a helper Git starts (a credential cache
    // daemon) can keep a pipe open after Git itself has exited.
    let tail = |pipe: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            const KEEP: usize = 64 * 1024;
            let mut kept = Vec::new();
            let Some(mut pipe) = pipe else {
                let _ = tx.send(kept);
                return;
            };
            let mut chunk = [0u8; 8192];
            while let Ok(n) = pipe.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                kept.extend_from_slice(&chunk[..n]);
                if kept.len() > 2 * KEEP {
                    kept.drain(..kept.len() - KEEP);
                }
            }
            if kept.len() > KEEP {
                kept.drain(..kept.len() - KEEP);
            }
            let _ = tx.send(kept);
        });
        rx
    };
    let out = tail(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let err = tail(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
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
    };
    drop(out);
    let stderr = err
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap_or_default();
    let said = String::from_utf8_lossy(&stderr).trim().to_owned();
    if status.success() {
        Ok(said)
    } else if what == Remote::Pull && said.contains("fast-forward") {
        // Git's own hint names `git config` and command-line flags.
        Err("the branches have diverged: use Git > Pull with Rebase or Pull with Merge".into())
    } else {
        Err(if said.is_empty() {
            format!("git {} failed", args[0])
        } else if let Some(why) = explain_ssh_failure(&said) {
            format!("error: {why}\n\n{said}")
        } else {
            said
        })
    }
}

/// What Git's output does not say when ssh failed on a key agent or key
/// file inside another app's container (Secretive keeps its socket and
/// public keys in `~/Library/Containers/...`): macOS keeps crc out of
/// that folder until crc has Full Disk Access. The shell works because
/// the terminal app has it. `None` when the output shows nothing of the
/// kind.
pub fn explain_ssh_failure(stderr: &str) -> Option<String> {
    let line = stderr.lines().map(str::trim).find(|l| {
        l.contains("/Library/Containers/")
            && (l.starts_with("Load key")
                || l.contains("Permission denied")
                || l.contains("Operation not permitted")
                || l.contains("No such file"))
    })?;
    let bundle = line
        .split("/Library/Containers/")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("");
    let app = if bundle.contains("Secretive") {
        "Secretive".to_owned()
    } else if bundle.is_empty() {
        "another app".to_owned()
    } else {
        bundle.to_owned()
    };
    Some(format!(
        "Full Disk Access is needed: ssh's key agent lives in {app}'s container, which macOS keeps crc out of. System Settings > Privacy & Security > Full Disk Access, add crc, then try again. The shell works because the terminal app has it."
    ))
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
    let output = run_with_stdin(
        command(root)
            .args([
                "blame",
                "--porcelain",
                "--contents",
                "-",
                "-L",
                &range,
                "--",
            ])
            .arg(path),
        contents.as_bytes().to_vec(),
    )?;
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

/// A commit some line of a blamed file was last changed in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitInfo {
    pub sha: String,
    pub author: String,
    /// Seconds since the epoch.
    pub time: i64,
    pub summary: String,
    /// Whether the commit object carries a signature (`gpgsig` or
    /// `gpgsig-sha256`), whoever made it; `None` when that could not be
    /// read. Says nothing of whether the signature checks out.
    pub signed: Option<bool>,
}

/// In [`BlameMap::lines`], a line not committed yet.
pub const NOT_COMMITTED: u32 = u32::MAX;

/// Who last changed every line of a file: each commit once, and each
/// line's index into them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlameMap {
    pub commits: Vec<CommitInfo>,
    /// Per line, zero-based: an index into `commits`, or [`NOT_COMMITTED`].
    pub lines: Vec<u32>,
    /// Per line, zero-based: where the line was in the file as its commit
    /// left it.
    pub origins: Vec<u32>,
}

/// Blames every line of `path` as the editor has it: `contents` is the
/// document's text, so unsaved edits count as not committed yet.
pub fn blame_file(root: &Path, path: &Path, contents: &str) -> Result<BlameMap, String> {
    let output = run_with_stdin(
        command(root)
            .args(["blame", "--porcelain", "--contents", "-", "--"])
            .arg(path),
        contents.as_bytes().to_vec(),
    )?;
    let mut map = parse_blame_file(&String::from_utf8_lossy(&output));
    // Git answers for the lines `contents` has; a final line break leaves
    // the editor one more, empty, line, which no commit changed.
    let lines = contents.bytes().filter(|&b| b == b'\n').count() + 1;
    map.lines.truncate(lines);
    map.origins.truncate(lines);
    // Which commits are signed: one read of every commit object. Without
    // it the strip still draws, saying nothing of signatures.
    if !map.commits.is_empty() {
        let shas: Vec<&str> = map.commits.iter().map(|c| c.sha.as_str()).collect();
        if let Ok(signed) = commit_signatures(root, &shas) {
            for (commit, signed) in map.commits.iter_mut().zip(signed) {
                commit.signed = signed;
            }
        }
    }
    Ok(map)
}

/// Whether each of `shas` is a signed commit, read through one
/// `git cat-file --batch`; `None` for one Git does not have.
pub fn commit_signatures(root: &Path, shas: &[&str]) -> Result<Vec<Option<bool>>, String> {
    // Anything but an object id would be read as another request.
    if shas
        .iter()
        .any(|sha| sha.is_empty() || !sha.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err("not an object id".into());
    }
    let mut input = shas.join("\n");
    input.push('\n');
    let output = run_with_stdin(
        command(root).args(["cat-file", "--batch"]),
        input.into_bytes(),
    )?;
    Ok(parse_signatures(&output, shas.len()))
}

/// `git cat-file --batch` for `count` commits: per object, whether its
/// header has a signature. The header is everything before the first
/// blank line; a `gpgsig` further down, in the message, is not one.
pub fn parse_signatures(output: &[u8], count: usize) -> Vec<Option<bool>> {
    let mut out = Vec::with_capacity(count);
    let mut rest = output;
    while out.len() < count {
        let Some(end) = rest.iter().position(|&b| b == b'\n') else {
            break;
        };
        let header = String::from_utf8_lossy(&rest[..end]).into_owned();
        rest = &rest[end + 1..];
        let mut words = header.split(' ');
        let (kind, size) = (
            words.nth(1),
            words.next().and_then(|n| n.parse::<usize>().ok()),
        );
        let Some(size) = size else {
            // `<sha> missing`, or worse: nothing to skip.
            out.push(None);
            continue;
        };
        let Some(body) = rest.get(..size) else {
            break;
        };
        rest = rest.get(size + 1..).unwrap_or(&[]);
        if kind != Some("commit") {
            out.push(None);
            continue;
        }
        let signed = body
            .split(|&b| b == b'\n')
            .take_while(|line| !line.is_empty())
            .any(|line| line.starts_with(b"gpgsig ") || line.starts_with(b"gpgsig-sha256 "));
        out.push(Some(signed));
    }
    out.resize(count, None);
    out
}

/// Git's `%G?` for `sha`: one letter saying whether its signature checks
/// out here. Runs the signature's checker, so only on demand.
pub fn signature_check(root: &Path, sha: &str) -> Result<char, String> {
    if sha.is_empty() || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("not an object id".into());
    }
    let output = run(root, &["log", "-1", "--format=%G?", sha])?;
    String::from_utf8_lossy(&output)
        .trim()
        .chars()
        .next()
        .ok_or_else(|| "no answer".into())
}

/// What a tooltip says of a commit's signature: whether it has one, and
/// once `check` (Git's `%G?`) is in, whether it holds. A signed commit
/// that cannot be checked here is never called unsigned.
pub fn signature_note(signed: Option<bool>, check: Option<char>) -> Option<&'static str> {
    match (signed, check) {
        (Some(true), Some('G')) => Some("signed, verified"),
        (Some(true), Some('B')) => Some("signature bad"),
        (Some(true), Some(_)) => Some("signed, cannot verify here"),
        (Some(true), None) => Some("signed"),
        (Some(false), _) => Some("unsigned"),
        (None, _) => None,
    }
}

/// `git blame --porcelain` for a whole file. A commit's details come with
/// its first line only; the lines after name it by its hash alone.
pub fn parse_blame_file(text: &str) -> BlameMap {
    let mut map = BlameMap::default();
    let mut index: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    // Every line of the file comes with a line of its own, so no answer
    // has more lines than that.
    let most = text.bytes().filter(|&b| b == b'\n').count();
    // The commit the header just read names, while its details follow.
    let mut current: Option<u32> = None;
    for line in text.lines() {
        if line.starts_with('\t') {
            current = None;
            continue;
        }
        let mut words = line.split(' ');
        let first = words.next().unwrap_or("");
        let is_header = first.len() >= 40 && first.bytes().all(|b| b.is_ascii_hexdigit());
        if is_header {
            let origin = words.next().and_then(|n| n.parse::<u32>().ok());
            let Some(final_line) = words
                .next()
                .and_then(|n| n.parse::<usize>().ok())
                .filter(|n| (1..=most).contains(n))
            else {
                current = None;
                continue;
            };
            let at = if first.bytes().all(|b| b == b'0') {
                current = None;
                NOT_COMMITTED
            } else {
                let next = map.commits.len() as u32;
                let at = *index.entry(first).or_insert(next);
                if at == next {
                    map.commits.push(CommitInfo {
                        sha: first.to_owned(),
                        author: String::new(),
                        time: 0,
                        summary: String::new(),
                        signed: None,
                    });
                }
                current = Some(at);
                at
            };
            if map.lines.len() < final_line {
                map.lines.resize(final_line, NOT_COMMITTED);
                map.origins.resize(final_line, 0);
            }
            map.lines[final_line - 1] = at;
            map.origins[final_line - 1] =
                origin.map_or(final_line as u32 - 1, |n| n.saturating_sub(1));
            continue;
        }
        let Some(commit) = current.and_then(|at| map.commits.get_mut(at as usize)) else {
            continue;
        };
        if let Some(name) = line.strip_prefix("author ") {
            commit.author = super::worktree::clean_line(name);
        } else if let Some(time) = line.strip_prefix("author-time ") {
            commit.time = time.parse().unwrap_or(0);
        } else if let Some(summary) = line.strip_prefix("summary ") {
            commit.summary = super::worktree::clean_line(summary);
        }
    }
    map
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
    fn a_key_agent_in_another_apps_container_is_explained() {
        let said = "Load key \"/Users/me/Library/Containers/com.maxgoedjen.Secretive.SecretAgent/Data/PublicKeys/abc.pub\": invalid format\ngit@github.com: Permission denied (publickey).\nfatal: Could not read from remote repository.";
        let why = explain_ssh_failure(said).unwrap();
        assert!(why.starts_with("Full Disk Access is needed"), "{why}");
        assert!(why.contains("Secretive's container"), "{why}");
        assert_eq!(
            explain_ssh_failure("fatal: Could not read from remote repository."),
            None
        );
        assert_eq!(
            explain_ssh_failure("Load key \"/Users/me/.ssh/id\": bad"),
            None
        );
    }

    /// A repository in a temporary folder, with a test identity and no
    /// signing, removed when dropped (a failed test included).
    struct TempRepo(PathBuf);

    impl TempRepo {
        fn new(name: &str) -> TempRepo {
            let dir = std::env::temp_dir().join(format!("crc-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            run(&dir, &["init", "--quiet"]).unwrap();
            for (key, value) in [
                ("user.name", "caio test"),
                ("user.email", "test@example.invalid"),
                ("commit.gpgsign", "false"),
            ] {
                run(&dir, &["config", key, value]).unwrap();
            }
            TempRepo(std::fs::canonicalize(&dir).unwrap())
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn additions_carry_their_file_and_line() {
        let diff = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n\
                    @@ -3,0 +4,2 @@ fn x()\n+four\n+five\n\
                    diff --git a/new.md b/new.md\nnew file mode 100644\n\
                    --- /dev/null\n+++ b/new.md\n@@ -0,0 +1 @@\n+only\n";
        let got: Vec<(String, usize, String)> = parse_additions(diff)
            .into_iter()
            .map(|a| (a.path, a.line, a.text))
            .collect();
        assert_eq!(
            got,
            [
                ("a.txt".into(), 4, "four".into()),
                ("a.txt".into(), 5, "five".into()),
                ("new.md".into(), 1, "only".into()),
            ]
        );
    }

    #[test]
    fn commits_since_are_strictly_after_the_visit() {
        let base = TempRepo::new("since");
        let dir = base.path();
        run(dir, &["init", "--quiet"]).unwrap();
        for (key, value) in [
            ("user.name", "test"),
            ("user.email", "test@example.invalid"),
            ("commit.gpgsign", "false"),
        ] {
            run(dir, &["config", key, value]).unwrap();
        }
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        run(dir, &["add", "a.txt"]).unwrap();
        run(dir, &["commit", "--quiet", "-m", "plant the beans"]).unwrap();
        let at: u64 = String::from_utf8(run(dir, &["log", "-1", "--format=%ct"]).unwrap())
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(commits_since(dir, at - 1, 5).unwrap(), ["plant the beans"]);
        // A visit in the same second as the commit came after it.
        assert!(commits_since(dir, at, 5).unwrap().is_empty());
    }

    fn count(dir: &Path) -> String {
        String::from_utf8(run(dir, &["rev-list", "--count", "HEAD"]).unwrap())
            .unwrap()
            .trim()
            .to_owned()
    }

    #[test]
    fn amend_rewrites_head_without_a_new_commit() {
        let base = TempRepo::new("amend");
        let dir = base.path();
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        run(dir, &["add", "a.txt"]).unwrap();
        commit(dir, "plant the beans\n\nA body line.", false).unwrap();
        assert_eq!(
            head_message(dir).unwrap(),
            "plant the beans\n\nA body line."
        );
        std::fs::write(dir.join("b.txt"), "b\n").unwrap();
        run(dir, &["add", "b.txt"]).unwrap();
        commit(dir, "plant the beans and water them", true).unwrap();
        assert_eq!(count(dir), "1", "amended, not added");
        assert_eq!(head_message(dir).unwrap(), "plant the beans and water them");
        let files = run(dir, &["show", "--name-only", "--format=", "HEAD"]).unwrap();
        assert_eq!(String::from_utf8_lossy(&files).trim(), "a.txt\nb.txt");
        // Nothing staged: a reword.
        commit(dir, "water the beans", true).unwrap();
        assert_eq!(count(dir), "1");
        assert_eq!(head_message(dir).unwrap(), "water the beans");
    }

    #[test]
    fn head_is_pushed_only_when_its_upstream_has_it() {
        let base = TempRepo::new("pushed");
        let dir = base.path();
        run(dir, &["init", "--quiet", "--bare", "origin.git"]).unwrap();
        let work = dir.join("work");
        run(dir, &["clone", "--quiet", "origin.git", "work"]).unwrap_or_default();
        for (key, value) in [
            ("user.name", "caio test"),
            ("user.email", "test@example.invalid"),
            ("commit.gpgsign", "false"),
        ] {
            run(&work, &["config", key, value]).unwrap();
        }
        let add = |file: &str| {
            std::fs::write(work.join(file), file).unwrap();
            run(&work, &["add", file]).unwrap();
            commit(&work, file, false).unwrap();
        };
        let read = || {
            let mut snapshot = snapshot(&work).unwrap();
            read_head(&mut snapshot);
            snapshot
        };
        assert_eq!(read().head_message, None, "no commits yet");
        add("one.txt");
        assert!(!head_is_pushed(&work), "no upstream");
        assert!(!read().head_pushed);
        run(&work, &["push", "--quiet", "-u", "origin", "HEAD"]).unwrap();
        assert!(head_is_pushed(&work), "level with its upstream");
        let level = read();
        assert!(level.head_pushed);
        assert_eq!(level.head_message.as_deref(), Some("one.txt"));
        add("two.txt");
        assert!(!head_is_pushed(&work), "ahead of its upstream");
        assert!(!read().head_pushed);
        run(&work, &["push", "--quiet"]).unwrap();
        assert!(read().head_pushed);
        // The upstream's branch deleted on the remote, then pruned.
        let origin = dir.join("origin.git");
        run(&origin, &["config", "receive.denyDeleteCurrent", "ignore"]).unwrap();
        let branch = String::from_utf8(run(&work, &["branch", "--show-current"]).unwrap())
            .unwrap()
            .trim()
            .to_owned();
        run(&work, &["push", "--quiet", "origin", "--delete", &branch]).unwrap();
        run(&work, &["fetch", "--quiet", "--prune"]).unwrap();
        let gone = read();
        assert!(gone.branch.ends_with("[gone]"), "{}", gone.branch);
        assert!(!head_is_pushed(&work), "gone upstream");
        assert!(!gone.head_pushed);
    }

    #[test]
    fn diverged_pulls_and_aborting_or_continuing_what_git_left() {
        // An origin with two clones; each commits, so they diverge.
        let base = TempRepo::new("diverge");
        let dir = base.path();
        let origin = dir.join("origin.git");
        run(dir, &["init", "--quiet", "--bare", "origin.git"]).unwrap();
        let clone = |name: &str| {
            run(dir, &["clone", "--quiet", "origin.git", name]).unwrap_or_default();
            let path = dir.join(name);
            for (key, value) in [
                ("user.name", "caio test"),
                ("user.email", "test@example.invalid"),
                ("commit.gpgsign", "false"),
            ] {
                run(&path, &["config", key, value]).unwrap();
            }
            path
        };
        let commit = |path: &Path, file: &str, text: &str| {
            std::fs::write(path.join(file), text).unwrap();
            run(path, &["add", file]).unwrap();
            run(path, &["commit", "--quiet", "-m", file]).unwrap();
        };
        let mine = clone("mine");
        commit(&mine, "shared.txt", "one\n");
        run(&mine, &["push", "--quiet", "-u", "origin", "HEAD"]).unwrap();
        let theirs = clone("theirs");
        commit(&theirs, "theirs.txt", "theirs\n");
        run(&theirs, &["push", "--quiet"]).unwrap();
        commit(&mine, "mine.txt", "mine\n");

        let error = remote(&mine, Remote::Pull, None).unwrap_err();
        assert!(error.contains("Pull with Rebase"), "{error}");
        remote(&mine, Remote::PullRebase, None).unwrap();
        assert!(mine.join("theirs.txt").exists());

        commit(&theirs, "again.txt", "again\n");
        run(&theirs, &["push", "--quiet"]).unwrap();
        commit(&mine, "more.txt", "more\n");
        remote(&mine, Remote::PullMerge, None).unwrap();
        assert!(mine.join("again.txt").exists());
        let parents = run(&mine, &["rev-list", "--parents", "-n", "1", "HEAD"]).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&parents).split_whitespace().count(),
            3,
            "a merge commit"
        );

        // A conflicting merge, given up.
        let git_dir = mine.join(".git");
        run(&mine, &["switch", "--quiet", "-c", "side"]).unwrap();
        commit(&mine, "shared.txt", "side\n");
        run(&mine, &["switch", "--quiet", "-"]).unwrap();
        commit(&mine, "shared.txt", "main\n");
        assert!(run(&mine, &["merge", "side"]).is_err());
        assert_eq!(InProgress::read(&git_dir), Some(InProgress::Merge));
        abort(&mine, InProgress::Merge).unwrap();
        assert_eq!(InProgress::read(&git_dir), None);
        assert_eq!(
            std::fs::read_to_string(mine.join("shared.txt")).unwrap(),
            "main\n"
        );

        // A conflicting rebase, resolved and continued without an editor.
        assert!(run(&mine, &["rebase", "side"]).is_err());
        assert_eq!(InProgress::read(&git_dir), Some(InProgress::Rebase));
        std::fs::write(mine.join("shared.txt"), "both\n").unwrap();
        run(&mine, &["add", "shared.txt"]).unwrap();
        continue_rebase(&mine).unwrap();
        assert_eq!(InProgress::read(&git_dir), None);
        assert!(origin.exists());
    }

    #[test]
    fn branches_rename_and_delete_with_their_refusals() {
        let repo = TempRepo::new("branch-edit");
        let dir = repo.path();
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        run(dir, &["add", "a.txt"]).unwrap();
        run(dir, &["commit", "--quiet", "-m", "one"]).unwrap();
        let main = run(dir, &["symbolic-ref", "--short", "HEAD"]).unwrap();
        let main = String::from_utf8_lossy(&main).trim().to_owned();
        // A merged branch, and one with a commit of its own.
        run(dir, &["branch", "merged"]).unwrap();
        run(dir, &["switch", "--quiet", "-c", "ahead"]).unwrap();
        std::fs::write(dir.join("b.txt"), "b\n").unwrap();
        run(dir, &["add", "b.txt"]).unwrap();
        run(dir, &["commit", "--quiet", "-m", "two"]).unwrap();
        run(dir, &["switch", "--quiet", &main]).unwrap();

        assert_eq!(unmerged_branches(dir).unwrap(), vec!["ahead".to_owned()]);
        assert!(rename_branch(dir, "merged", "bad..name").is_err());
        assert!(
            rename_branch(dir, "merged", "ahead").is_err(),
            "a taken name"
        );
        rename_branch(dir, "merged", "done").unwrap();
        // A name that looks like an option is a branch, not a flag.
        assert!(delete_branch(dir, "-D", false).is_err());
        assert!(delete_branch(dir, "ahead", false).is_err(), "not merged");
        delete_branch(dir, "ahead", true).unwrap();
        delete_branch(dir, "done", false).unwrap();
        let names: Vec<String> = branches(dir).unwrap().into_iter().map(|b| b.name).collect();
        assert_eq!(names, vec![main]);
    }

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
    fn a_whole_file_blame_names_each_commit_once() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let zero = "0".repeat(40);
        let porcelain = format!(
            "{a} 1 1 2\nauthor Ana\nauthor-mail <a@example.com>\nauthor-time 1700000000\nauthor-tz +0000\nsummary first\x1b[31m words\nfilename f.txt\n\tone\n\
             {a} 2 2\n\ttwo\n\
             {zero} 3 3 1\nauthor Not Committed Yet\nauthor-time 1\nsummary Version of f.txt from -\nfilename f.txt\n\tTHREE\n\
             {b} 3 4 1\nauthor Bo\tBloggs\nauthor-time 1600000000\nsummary second\nprevious {a} f.txt\nfilename f.txt\n\tfour\n\
             {a} 4 5 1\nfilename f.txt\n\tfive\n"
        );
        let map = parse_blame_file(&porcelain);
        assert_eq!(map.commits.len(), 2);
        assert_eq!(map.commits[0].sha, a);
        assert_eq!(map.commits[0].author, "Ana");
        assert_eq!(map.commits[0].time, 1_700_000_000);
        assert_eq!(
            map.commits[0].summary, "first words",
            "no escape gets through"
        );
        assert_eq!(map.commits[1].author, "Bo Bloggs");
        assert_eq!(map.commits[1].summary, "second");
        assert_eq!(map.lines, vec![0, 0, NOT_COMMITTED, 1, 0]);
        assert_eq!(
            map.origins,
            vec![0, 1, 2, 2, 3],
            "where each line came from"
        );
        assert!(map.commits.iter().all(|c| c.signed.is_none()));
        // A line number past what the answer holds is not believed.
        let wild = format!("{a} 1 9999999 1\nauthor X\n\tone\n");
        assert!(parse_blame_file(&wild).lines.is_empty());
        assert_eq!(parse_blame_file(""), BlameMap::default());
    }

    #[test]
    fn a_signature_is_read_from_the_commit_header_only() {
        let object = |body: &str| format!("{} commit {}\n{body}\n", "c".repeat(40), body.len());
        let signed = "tree t\nparent p\nauthor A <a@x> 1 +0000\ncommitter A <a@x> 1 +0000\ngpgsig -----BEGIN SSH SIGNATURE-----\n U1NIU0lH\n -----END SSH SIGNATURE-----\n\nsigned work\n";
        let sha256 = "tree t\nauthor A <a@x> 1 +0000\ngpgsig-sha256 -----BEGIN PGP SIGNATURE-----\n \n iQ\n -----END PGP SIGNATURE-----\n\nnewer hash\n";
        // Only in the message: not a signature.
        let plain = "tree t\nauthor A <a@x> 1 +0000\n\nmention\ngpgsig in the message\n";
        let output = format!(
            "{}{}{}{} missing\n{}",
            object(signed),
            object(plain),
            object(sha256),
            "d".repeat(40),
            format_args!("{} tree 4\nabcd\n", "e".repeat(40)),
        );
        assert_eq!(
            parse_signatures(output.as_bytes(), 5),
            vec![Some(true), Some(false), Some(true), None, None]
        );
        // Cut short: what is missing is not known.
        let cut = object(plain);
        assert_eq!(
            parse_signatures(&cut.as_bytes()[..cut.len() - 5], 2),
            vec![None, None]
        );
        assert_eq!(parse_signatures(b"", 1), vec![None]);
    }

    #[test]
    fn a_signature_check_never_calls_a_signed_commit_unsigned() {
        assert_eq!(
            signature_note(Some(true), Some('G')),
            Some("signed, verified")
        );
        assert_eq!(signature_note(Some(true), Some('B')), Some("signature bad"));
        for code in ['E', 'U', 'X', 'Y', 'R', 'N', '?'] {
            assert_eq!(
                signature_note(Some(true), Some(code)),
                Some("signed, cannot verify here"),
                "{code}"
            );
        }
        assert_eq!(signature_note(Some(true), None), Some("signed"));
        assert_eq!(signature_note(Some(false), None), Some("unsigned"));
        assert_eq!(signature_note(Some(false), Some('N')), Some("unsigned"));
        assert_eq!(signature_note(None, Some('G')), None);
    }

    #[test]
    fn a_whole_file_is_blamed_with_its_unsaved_lines() {
        let dir = std::env::temp_dir().join(format!("crc-blame-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str], date: &str| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args([
                    "-c",
                    "user.name=Robin Example",
                    "-c",
                    "user.email=r@example.com",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_DATE", date)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        let date = "2020-01-01T00:00:00Z";
        git(&["init", "-q", "-b", "main"], date);
        std::fs::write(dir.join("garden.txt"), "beans\npeas\n").unwrap();
        git(&["add", "garden.txt"], date);
        git(&["commit", "-q", "-m", "plant beans"], date);
        std::fs::write(dir.join("garden.txt"), "beans\npeas\nkale\n").unwrap();
        git(&["commit", "-q", "-am", "add kale"], "2024-06-01T00:00:00Z");
        let map = blame_file(&dir, Path::new("garden.txt"), "beans\nPEAS\nkale\n").unwrap();
        assert_eq!(
            map.lines.len(),
            3,
            "no line for the empty one after the last break"
        );
        assert_eq!(map.commits.len(), 2);
        let summary = |line: usize| &map.commits[map.lines[line] as usize].summary;
        assert_eq!(summary(0), "plant beans");
        assert_eq!(map.lines[1], NOT_COMMITTED, "the edited line");
        assert_eq!(summary(2), "add kale");
        assert_eq!(map.commits[map.lines[2] as usize].time, 1_717_200_000);
        assert_eq!(map.commits[0].author, "Robin Example");
        assert_eq!(map.origins, vec![0, 1, 2]);
        assert!(
            map.commits.iter().all(|c| c.signed == Some(false)),
            "made without signing"
        );
        assert!(blame_file(&dir, Path::new("missing.txt"), "x\n").is_err());
        let _ = std::fs::remove_dir_all(&dir);
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
        // The unmerged file reads as an ordinary diff against HEAD.
        let shown = Diff::parse(&diff(&s.root, change).unwrap());
        assert!(
            shown
                .lines
                .iter()
                .any(|l| l.kind == DiffKind::Added && l.text.starts_with("<<<<<<<"))
        );
        assert!(
            shown
                .lines
                .iter()
                .any(|l| l.kind == DiffKind::Context && l.text == "x = 1" && l.old == Some(2))
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
        marks(&crate::ide::diff::changes(old, new))
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
    fn content_that_looks_like_a_header_is_content() {
        // A removed `-- comment` line comes out as `--- comment`.
        let diff = Diff::parse(concat!(
            "diff --git a/q.sql b/q.sql\n",
            "--- a/q.sql\n",
            "+++ b/q.sql\n",
            "@@ -1,3 +1,3 @@\n",
            "--- old comment\n",
            "+++ new comment\n",
            " select 1;\n",
            "-diff --git looks like a header\n",
            "+x\n",
        ));
        let kinds: Vec<_> = diff.lines.iter().map(|l| (l.kind, l.old, l.new)).collect();
        assert_eq!(
            kinds,
            [
                (DiffKind::Hunk, None, None),
                (DiffKind::Removed, Some(1), None),
                (DiffKind::Added, None, Some(1)),
                (DiffKind::Context, Some(2), Some(2)),
                (DiffKind::Removed, Some(3), None),
                (DiffKind::Added, None, Some(3)),
            ]
        );
        assert_eq!(diff.lines[1].text, "-- old comment");
        assert_eq!(diff.hunks[0].raw.lines().count(), 6, "the whole hunk");
    }

    #[test]
    fn combined_diffs_strip_one_column_per_parent() {
        let diff = Diff::parse(concat!(
            "@@@ -1,2 -1,2 +1,3 @@@\n",
            "  a\n",
            "++<<<<<<< ours\n",
            " -x\n",
        ));
        assert_eq!(diff.lines[1].text, "a");
        assert_eq!(diff.lines[2].kind, DiffKind::Added);
        assert_eq!(diff.lines[2].text, "<<<<<<< ours");
        assert_eq!(diff.lines[3].kind, DiffKind::Removed);
        assert_eq!(diff.lines[3].text, "x");
    }

    #[test]
    fn head_text_of_a_repository_without_commits_is_a_new_file() {
        let repo = TempRepo::new("unborn");
        let root = repo.path().to_path_buf();
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        assert_eq!(head_text(&root, &root.join("a.txt")), Ok(None));
    }

    #[test]
    fn parses_name_status_with_renames() {
        let raw = b"M\0src/a.rs\0A\0new.txt\0D\0gone.txt\0R087\0old.md\0moved.md\0T\0link\0";
        let got = parse_name_status(raw);
        let short: Vec<(char, &str, Option<&str>)> = got
            .iter()
            .map(|c| {
                (
                    c.kind,
                    c.path.to_str().unwrap(),
                    c.from.as_deref().and_then(Path::to_str),
                )
            })
            .collect();
        assert_eq!(
            short,
            [
                ('M', "src/a.rs", None),
                ('A', "new.txt", None),
                ('D', "gone.txt", None),
                ('R', "moved.md", Some("old.md")),
                ('T', "link", None),
            ]
        );
        assert!(parse_name_status(b"").is_empty());
    }

    #[test]
    fn rev_bytes_reads_any_rev() {
        let repo = TempRepo::new("rev-bytes");
        let root = repo.path().to_path_buf();
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        run(&root, &["add", "a.txt"]).unwrap();
        run(&root, &["commit", "-q", "-m", "one"]).unwrap();
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        run(&root, &["commit", "-q", "-am", "two"]).unwrap();
        let a = Path::new("a.txt");
        assert_eq!(rev_bytes(&root, "HEAD~1", a), Ok(Some(b"one\n".to_vec())));
        assert_eq!(rev_bytes(&root, "HEAD", a), Ok(Some(b"two\n".to_vec())));
        assert_eq!(rev_bytes(&root, "HEAD", Path::new("b.txt")), Ok(None));
        assert_eq!(rev_bytes(&root, "no-such-branch", a), Ok(None));
    }

    #[test]
    fn changed_since_the_merge_base_includes_uncommitted() {
        let repo = TempRepo::new("changed-since");
        let root = repo.path().to_path_buf();
        let body = "a line long enough that a rename is found\n".repeat(8);
        for name in ["kept.txt", "edited.txt", "staged-gone.txt", "old-name.txt"] {
            std::fs::write(root.join(name), &body).unwrap();
        }
        run(&root, &["add", "."]).unwrap();
        run(&root, &["commit", "-q", "-m", "base"]).unwrap();
        run(&root, &["branch", "-q", "-M", "main"]).unwrap();
        run(&root, &["switch", "-q", "-c", "topic"]).unwrap();
        std::fs::write(root.join("added.txt"), "new\n").unwrap();
        run(&root, &["add", "added.txt"]).unwrap();
        run(&root, &["mv", "old-name.txt", "new-name.txt"]).unwrap();
        run(&root, &["commit", "-q", "-m", "topic"]).unwrap();
        std::fs::write(root.join("edited.txt"), "changed\n").unwrap();
        run(&root, &["rm", "-q", "staged-gone.txt"]).unwrap();
        std::fs::write(root.join("loose.txt"), "untracked\n").unwrap();
        let base = merge_base(&root, "main").unwrap();
        let got: Vec<(char, String)> = changed_since(&root, &base)
            .unwrap()
            .into_iter()
            .map(|c| (c.kind, c.path.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(
            got,
            [
                ('A', "added.txt".into()),
                ('M', "edited.txt".into()),
                ('?', "loose.txt".into()),
                ('R', "new-name.txt".into()),
                ('D', "staged-gone.txt".into()),
            ]
        );
    }

    #[test]
    fn unstaging_before_the_first_commit_keeps_the_edited_file() {
        let repo = TempRepo::new("unborn-unstage");
        let root = repo.path().to_path_buf();
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        run(&root, &["add", "a.txt"]).unwrap();
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        let change = snapshot(&root)
            .unwrap()
            .changes
            .into_iter()
            .find(|c| c.path.ends_with("a.txt"))
            .expect("a.txt is a change");
        stage(&root, &change, false).expect("unstage");
        let status = String::from_utf8(run(&root, &["status", "--porcelain"]).unwrap()).unwrap();
        assert_eq!(status, "?? a.txt\n");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "two\n"
        );
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
        let repo = TempRepo::new("git-test");
        let root = repo.path().to_path_buf();
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
        commit(&root, "Initial personal sample", false).unwrap();
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

        // A staged rename edited again stages its new edit.
        run(&root, &["add", "-A"]).unwrap();
        // A message several times a pipe's buffer goes in whole.
        let body = "a long commit message line\n".repeat(12_000);
        commit(&root, &format!("Everything\n\n{body}"), false).unwrap();
        let logged = run(&root, &["log", "-1", "--format=%B"]).unwrap();
        assert!(logged.len() > body.len(), "{} bytes", logged.len());
        // Nothing staged: Git's own refusal, not a broken pipe.
        let error = commit(&root, &body, false).unwrap_err();
        assert!(!error.to_lowercase().contains("pipe"), "{error}");
        run(&root, &["mv", "a file.txt", "moved.txt"]).unwrap();
        std::fs::write(root.join("moved.txt"), "moved and edited\n").unwrap();
        let s = snapshot(&root).unwrap();
        let moved = s
            .changes
            .iter()
            .find(|c| c.path == Path::new("moved.txt"))
            .unwrap();
        assert!(moved.original.is_some());
        stage(&root, moved, true).unwrap();
        assert!(
            run(&root, &["diff", "--quiet", "--", "moved.txt"]).is_ok(),
            "the edit is in the index"
        );
        let staged = run(&root, &["diff", "--cached", "--", "moved.txt"]).unwrap();
        assert!(String::from_utf8_lossy(&staged).contains("+moved and edited"));
    }

    #[test]
    fn stages_and_unstages_one_hunk_without_touching_other_edits() {
        let repo = TempRepo::new("hunk-test");
        let root = repo.path().to_path_buf();
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
    }

    #[test]
    fn hunk_patch_keeps_crlf_bytes() {
        let repo = TempRepo::new("crlf-hunk");
        let root = repo.path().to_path_buf();
        run(&root, &["config", "core.autocrlf", "false"]).unwrap();
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
    }

    #[test]
    fn hunk_patch_keeps_missing_final_newline() {
        let repo = TempRepo::new("eof-hunk");
        let root = repo.path().to_path_buf();
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
    }

    /// The lines under a hunk's header in the preview, by their text with
    /// the `+`, `-` or space it starts with.
    fn pick(diff: &Diff, hunk: usize, wanted: &[&str]) -> (Hunk, Vec<usize>) {
        let hunk = diff.hunks[hunk].clone();
        let lines = hunk
            .raw
            .split_inclusive('\n')
            .skip(1)
            .enumerate()
            .filter(|(_, line)| wanted.contains(&line.trim_end_matches(['\n', '\r'])))
            .map(|(index, _)| index)
            .collect();
        (hunk, lines)
    }

    #[test]
    fn stages_selected_lines_of_one_hunk() {
        let (repo, root) = committed("line-pick", "garden.txt", "beans\npeas\nmint\n");
        let path = root.join("garden.txt");
        std::fs::write(&path, "beans\ncarrots\npeas\nleeks\nchard\n").unwrap();
        let change = snapshot(&root).unwrap().changes.remove(0);
        let preview = Diff::parse(&diff(&root, &change).unwrap());
        assert_eq!(preview.hunks.len(), 1);
        stage_hunk_lines(
            &root,
            &change,
            &[pick(&preview, 0, &["+carrots", "+chard"])],
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(run(&root, &["show", ":garden.txt"]).unwrap()).unwrap(),
            "beans\ncarrots\npeas\nmint\nchard\n"
        );
        let staged = cached(&root);
        assert!(staged.contains("+carrots") && staged.contains("+chard"));
        assert!(!staged.contains("leeks") && !staged.contains("-mint"));
        let working = String::from_utf8(run(&root, &["diff"]).unwrap()).unwrap();
        assert!(working.contains("+leeks") && working.contains("-mint"));
        assert!(!working.contains("+carrots") && !working.contains("+chard"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "beans\ncarrots\npeas\nleeks\nchard\n"
        );

        // One line back out of the index.
        let change = snapshot(&root).unwrap().changes.remove(0);
        let preview = Diff::parse(&diff(&root, &change).unwrap());
        let index = preview.hunks.iter().position(|h| h.staged).unwrap();
        stage_hunk_lines(&root, &change, &[pick(&preview, index, &["+carrots"])]).unwrap();
        assert_eq!(
            String::from_utf8(run(&root, &["show", ":garden.txt"]).unwrap()).unwrap(),
            "beans\npeas\nmint\nchard\n"
        );

        // A removal on its own, and nothing chosen is refused.
        let change = snapshot(&root).unwrap().changes.remove(0);
        let preview = Diff::parse(&diff(&root, &change).unwrap());
        let index = preview.hunks.iter().position(|h| !h.staged).unwrap();
        stage_hunk_lines(&root, &change, &[pick(&preview, index, &["-mint"])]).unwrap();
        assert_eq!(
            String::from_utf8(run(&root, &["show", ":garden.txt"]).unwrap()).unwrap(),
            "beans\npeas\nchard\n"
        );
        let change = snapshot(&root).unwrap().changes.remove(0);
        let preview = Diff::parse(&diff(&root, &change).unwrap());
        let index = preview.hunks.iter().position(|h| !h.staged).unwrap();
        assert!(stage_hunk_lines(&root, &change, &[pick(&preview, index, &[" peas"])]).is_err());
        drop(repo);
    }

    #[test]
    fn selected_lines_of_a_stale_hunk_stage_nothing() {
        let (_repo, root) = committed("line-stale", "garden.txt", "beans\npeas\n");
        let path = root.join("garden.txt");
        std::fs::write(&path, "beans\ncarrots\npeas\n").unwrap();
        let change = snapshot(&root).unwrap().changes.remove(0);
        let preview = Diff::parse(&diff(&root, &change).unwrap());
        std::fs::write(&path, "beans\nradish\npeas\n").unwrap();
        assert!(stage_hunk_lines(&root, &change, &[pick(&preview, 0, &["+carrots"])]).is_err());
        assert!(cached(&root).is_empty());
    }

    #[test]
    fn selected_lines_keep_crlf_bytes() {
        let (_repo, root) = committed("line-crlf", "crlf.txt", "one\r\ntwo\r\n");
        run(&root, &["config", "core.autocrlf", "false"]).unwrap();
        let path = root.join("crlf.txt");
        std::fs::write(&path, "one\r\nleft\r\ntwo\r\nright\r\n").unwrap();
        let change = snapshot(&root).unwrap().changes.remove(0);
        let preview = Diff::parse(&diff(&root, &change).unwrap());
        stage_hunk_lines(&root, &change, &[pick(&preview, 0, &["+right"])]).unwrap();
        assert_eq!(
            run(&root, &["show", ":crlf.txt"]).unwrap(),
            b"one\r\ntwo\r\nright\r\n"
        );
    }

    #[test]
    fn selected_lines_keep_missing_final_newline() {
        let (_repo, root) = committed("line-eof", "eof.txt", "alpha\nbeta\n");
        let path = root.join("eof.txt");
        std::fs::write(&path, "alpha\nbeta\ngamma\ndelta").unwrap();
        let change = snapshot(&root).unwrap().changes.remove(0);
        let preview = Diff::parse(&diff(&root, &change).unwrap());
        stage_hunk_lines(&root, &change, &[pick(&preview, 0, &["+delta"])]).unwrap();
        assert_eq!(
            run(&root, &["show", ":eof.txt"]).unwrap(),
            b"alpha\nbeta\ndelta"
        );
        // The other way round: the line without a newline stays behind.
        run(&root, &["reset", "--quiet"]).unwrap();
        let change = snapshot(&root).unwrap().changes.remove(0);
        let preview = Diff::parse(&diff(&root, &change).unwrap());
        stage_hunk_lines(&root, &change, &[pick(&preview, 0, &["+gamma"])]).unwrap();
        assert_eq!(
            run(&root, &["show", ":eof.txt"]).unwrap(),
            b"alpha\nbeta\ngamma\n"
        );

        // A last line without a newline cannot be kept as context with
        // lines after it: refused, nothing staged.
        let (_repo, root) = committed("line-eof-old", "eof.txt", "alpha\nbeta");
        std::fs::write(root.join("eof.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let change = snapshot(&root).unwrap().changes.remove(0);
        let preview = Diff::parse(&diff(&root, &change).unwrap());
        assert!(stage_hunk_lines(&root, &change, &[pick(&preview, 0, &["+gamma"])]).is_err());
        assert!(cached(&root).is_empty());
        stage_hunk_lines(&root, &change, &[pick(&preview, 0, &["-beta", "+beta"])]).unwrap();
        assert_eq!(run(&root, &["show", ":eof.txt"]).unwrap(), b"alpha\nbeta\n");
    }

    /// A repository with `file` committed as `head`, and its root.
    fn committed(name: &str, file: &str, head: &str) -> (TempRepo, PathBuf) {
        let repo = TempRepo::new(name);
        let root = repo.path().to_path_buf();
        std::fs::write(root.join(file), head).unwrap();
        run(&root, &["add", "--", file]).unwrap();
        run(&root, &["commit", "--quiet", "-m", "Personal sample"]).unwrap();
        (repo, root)
    }

    fn cached(root: &Path) -> String {
        String::from_utf8(run(root, &["diff", "--cached"]).unwrap()).unwrap()
    }

    #[test]
    fn text_hunk_patches_count_lines_as_git_does() {
        let changes = crate::ide::diff::changes;
        // A modification with context on both sides.
        let head = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let text = "a\nb\nc\nd\nE\nf\ng\nh\n";
        let patch = text_hunk_patch("p.txt", head, text, &changes(head, text)[0]).unwrap();
        assert_eq!(
            patch,
            "diff --git a/p.txt b/p.txt\n--- a/p.txt\n+++ b/p.txt\n\
             @@ -2,7 +2,7 @@\n b\n c\n d\n-e\n+E\n f\n g\n h\n"
        );
        // Everything added to an empty file: the old side is `-0,0`.
        let patch = text_hunk_patch("p.txt", "", "new\n", &changes("", "new\n")[0]).unwrap();
        assert!(patch.ends_with("@@ -0,0 +1 @@\n+new\n"), "{patch}");
        // Everything removed: the new side is `+0,0`.
        let patch = text_hunk_patch("p.txt", "x\ny", "", &changes("x\ny", "")[0]).unwrap();
        assert!(
            patch.ends_with("@@ -1,2 +0,0 @@\n-x\n-y\n\\ No newline at end of file\n"),
            "{patch}"
        );
        // A path git would quote.
        let patch = text_hunk_patch("a\"b.txt", "x\n", "y\n", &changes("x\n", "y\n")[0]).unwrap();
        assert!(patch.starts_with("diff --git \"a/a\\\"b.txt\" \"b/a\\\"b.txt\"\n"));
        // A change that does not fit the texts.
        let mut stale = changes(head, text)[0];
        stale.new_end = 40;
        assert_eq!(text_hunk_patch("p.txt", head, text, &stale), None);
    }

    #[test]
    fn stages_one_change_of_the_buffer_text() {
        let head: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        let (_repo, root) = committed("text-hunk", "notes.txt", &head);
        let text = head
            .replace("line 2\n", "first edit\n")
            .replace("line 18\n", "second edit\n");
        // On disk the file is unchanged: the buffer's text is what stages.
        let changes = crate::ide::diff::changes(&head, &text);
        assert_eq!(changes.len(), 2);
        stage_text_hunk(&root, Path::new("notes.txt"), &head, &text, &changes[1]).unwrap();
        let staged = cached(&root);
        assert!(staged.contains("+second edit"), "{staged}");
        assert!(!staged.contains("first edit"), "{staged}");
        assert_eq!(
            std::fs::read_to_string(root.join("notes.txt")).unwrap(),
            head
        );
    }

    #[test]
    fn stages_an_addition_at_the_end_without_a_final_newline() {
        let (_repo, root) = committed("text-hunk-eof", "eof.txt", "alpha\nbeta");
        let (head, text) = ("alpha\nbeta", "alpha\nbeta\ngamma");
        let changes = crate::ide::diff::changes(head, text);
        assert_eq!(changes.len(), 1);
        stage_text_hunk(&root, Path::new("eof.txt"), head, text, &changes[0]).unwrap();
        let index = run(&root, &["show", ":eof.txt"]).unwrap();
        assert_eq!(index, b"alpha\nbeta\ngamma");
    }

    #[test]
    fn stages_a_pure_deletion() {
        let head = "one\ntwo\nthree\nfour\n";
        let (_repo, root) = committed("text-hunk-del", "del.txt", head);
        let text = "one\nfour\n";
        let changes = crate::ide::diff::changes(head, text);
        assert_eq!(changes[0].kind, MarkKind::Removed);
        stage_text_hunk(&root, Path::new("del.txt"), head, text, &changes[0]).unwrap();
        assert_eq!(run(&root, &["show", ":del.txt"]).unwrap(), text.as_bytes());
    }

    #[test]
    fn refuses_when_the_index_differs_from_head() {
        let head = "one\ntwo\nthree\n";
        let (_repo, root) = committed("text-hunk-index", "idx.txt", head);
        // Someone staged a different edit of the same line.
        std::fs::write(root.join("idx.txt"), "one\nTWO staged\nthree\n").unwrap();
        run(&root, &["add", "--", "idx.txt"]).unwrap();
        let text = "one\nTWO typed\nthree\n";
        let changes = crate::ide::diff::changes(head, text);
        let error =
            stage_text_hunk(&root, Path::new("idx.txt"), head, text, &changes[0]).unwrap_err();
        assert!(error.contains("the index differs from HEAD"), "{error}");
        assert!(cached(&root).contains("+TWO staged"));
    }

    #[test]
    fn refuses_a_crlf_file() {
        let (_repo, root) = committed("text-hunk-crlf", "crlf.txt", "one\r\ntwo\r\n");
        run(&root, &["config", "core.autocrlf", "false"]).unwrap();
        let (head, text) = ("one\ntwo\n", "one\nTWO\n");
        let changes = crate::ide::diff::changes(head, text);
        let error =
            stage_text_hunk(&root, Path::new("crlf.txt"), head, text, &changes[0]).unwrap_err();
        assert!(error.contains("CRLF"), "{error}");
        assert!(cached(&root).is_empty());
    }
}
