//! Tasks: a repository's Git worktrees, each a branch checked out in a
//! folder of its own, so several agents can work at once without editing
//! the same files.
//!
//! The list is Git's own (`git worktree list`), so worktrees made outside
//! crc show too and nothing is kept on the side. Worktrees crc makes go
//! under Application Support, out of the project's tree, its index and its
//! file watcher.

use std::path::{Path, PathBuf};
use std::process::Command;

/// One checkout of the repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// The branch checked out, without `refs/heads/`; `None` when HEAD is
    /// detached.
    pub branch: Option<String>,
    /// The repository's own checkout, which cannot be removed.
    pub main: bool,
}

impl Worktree {
    /// What the sidebar calls it: the branch, or the folder's name.
    pub fn name(&self) -> String {
        match &self.branch {
            Some(branch) => branch.clone(),
            None => self.path.file_name().map_or_else(
                || self.path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            ),
        }
    }
}

fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    cmd
}

fn run(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = git(dir).args(args).output().map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(git_said(&String::from_utf8_lossy(&output.stderr)))
    }
}

/// What Git said on stderr when it failed, without its `fatal: ` or
/// `error: `.
fn git_said(stderr: &str) -> String {
    let said = stderr.trim();
    said.strip_prefix("fatal: ")
        .or_else(|| said.strip_prefix("error: "))
        .unwrap_or(said)
        .to_owned()
}

/// Where a repository's Git runs: on this Mac, or on an ssh host through
/// the connection its terminals share (`control`, see
/// [`crate::platform::remote::control_path`]). A host's paths are its own:
/// nothing here resolves them or looks for them on this Mac.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Runner {
    Local,
    Ssh {
        host: String,
        control: Option<String>,
    },
}

/// What a read over ssh says when the host did not answer: ssh's own exit
/// status, 255, no ssh to run, or no answer in [`SSH_WAIT`].
pub const UNREACHABLE: &str = "host unreachable";

/// How long one read over ssh may take before the host counts as
/// unreachable: a hung connection must not hold the list's reads forever.
const SSH_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// After each command of a batch, on a line of its own: this byte (ASCII's
/// record separator), then the command's exit status.
const MARK: char = '\u{1e}';

/// The line a host's shell runs for a batch of Git commands in `dir`: each
/// one's output, then a line with [`MARK`] and its exit status.
pub fn batch_line(dir: &Path, commands: &[Vec<String>]) -> String {
    let mut line = format!(
        "cd {} || exit 1; export LC_ALL=C GIT_TERMINAL_PROMPT=0 GIT_OPTIONAL_LOCKS=0",
        crate::platform::remote::shell_path(&dir.to_string_lossy())
    );
    for args in commands {
        line.push_str("; git");
        for arg in args {
            line.push(' ');
            line.push_str(&crate::platform::remote::quote(arg));
        }
        line.push_str(" 2>/dev/null; printf '\\n\\036%d\\n' $?");
    }
    line
}

/// The output of [`batch_line`], per command: what it printed, or an error
/// for one that failed. `None` when it stopped before the last.
pub fn parse_batch(text: &str, count: usize) -> Option<Vec<Result<String, String>>> {
    let mut out = Vec::with_capacity(count);
    let mut rest = text;
    let mark = format!("\n{MARK}");
    while out.len() < count {
        let (said, after) = rest.split_once(&mark)?;
        let (status, after) = after.split_once('\n')?;
        out.push(match status.trim() {
            "0" => Ok(said.to_owned()),
            status => Err(format!("git exited with {status}")),
        });
        rest = after;
    }
    Some(out)
}

/// Runs `line` on `host` over ssh: what it printed, when it exits 0.
pub fn ssh_output(host: &str, control: Option<&str>, line: &str) -> Result<String, String> {
    use std::io::Read;
    // Nothing on stderr is read: a connection ssh keeps open for later
    // would hold the pipe, and the read with it.
    let mut child = Command::new(crate::platform::remote::program())
        .args(crate::platform::remote::batch_args(host, control, line))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|_| UNREACHABLE.to_owned())?;
    let mut stdout = child.stdout.take().ok_or(UNREACHABLE)?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < SSH_WAIT => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(UNREACHABLE.into());
            }
        }
    };
    let bytes = reader.join().unwrap_or_default();
    match status.code() {
        Some(0) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Some(255) | None => Err(UNREACHABLE.into()),
        Some(code) => Err(format!("exited with {code}")),
    }
}

/// Runs a batch of Git commands in `dir`: one after another here, or in one
/// ssh call on a host. Per command, its output or why it failed; `Err` when
/// none ran (the host did not answer, or `dir` is not there).
pub fn batch_on(
    runner: &Runner,
    dir: &Path,
    commands: &[Vec<String>],
) -> Result<Vec<Result<String, String>>, String> {
    match runner {
        Runner::Local => Ok(commands
            .iter()
            .map(|args| {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                run(dir, &args)
            })
            .collect()),
        Runner::Ssh { host, control } => {
            let text = ssh_output(host, control.as_deref(), &batch_line(dir, commands))?;
            parse_batch(&text, commands.len()).ok_or_else(|| UNREACHABLE.to_owned())
        }
    }
}

/// One Git command in `dir`, by `runner`.
fn run_on(runner: &Runner, dir: &Path, args: &[&str]) -> Result<String, String> {
    match runner {
        Runner::Local => run(dir, args),
        Runner::Ssh { .. } => {
            let args = args.iter().map(|a| (*a).to_owned()).collect();
            batch_on(runner, dir, &[args])?
                .pop()
                .unwrap_or_else(|| Err(UNREACHABLE.into()))
        }
    }
}

/// How long a change made over ssh may take before it is stopped: a pull
/// or a push talks to another server from the host.
const SSH_WRITE_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// The line a host's shell runs to change something with Git in `dir`: the
/// commands one after another, stopping at the first that fails, their
/// stderr left for the caller to read.
pub fn write_line(dir: &Path, commands: &[Vec<String>]) -> String {
    let mut line = format!(
        "cd {} || exit 1; export LC_ALL=C GIT_TERMINAL_PROMPT=0",
        crate::platform::remote::shell_path(&dir.to_string_lossy())
    );
    for (n, args) in commands.iter().enumerate() {
        line.push_str(if n == 0 { "; git" } else { " && git" });
        for arg in args {
            line.push(' ');
            line.push_str(&crate::platform::remote::quote(arg));
        }
    }
    line
}

/// Runs `line` on `host` over ssh to change something there: what it
/// printed on stdout and on stderr, when it exits 0; else what it said.
/// Its stderr is read, unlike a read's: the call never becomes the shared
/// connection (see [`crate::platform::remote::write_args`]), so nothing
/// outlives it holding the pipe.
pub fn ssh_write(
    host: &str,
    control: Option<&str>,
    line: &str,
) -> Result<(String, String), String> {
    use std::io::Read;
    let mut child = Command::new(crate::platform::remote::program())
        .args(crate::platform::remote::write_args(host, control, line))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|_| UNREACHABLE.to_owned())?;
    // Both pipes are read while it runs, so a chatty push never fills one.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            let _ = tx.send(String::from_utf8_lossy(&bytes).into_owned());
        });
        rx
    };
    let out = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let err = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < SSH_WRITE_WAIT => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{host} took over two minutes to answer and was stopped"
                ));
            }
        }
    };
    let wait = std::time::Duration::from_secs(2);
    let out = out.recv_timeout(wait).unwrap_or_default();
    let err = err.recv_timeout(wait).unwrap_or_default();
    match status.code() {
        Some(0) => Ok((out, err)),
        // ssh's own failure: it says why, a key refused or no route.
        Some(255) | None => Err(match err.trim() {
            "" => UNREACHABLE.to_owned(),
            said => format!("{UNREACHABLE}: {said}"),
        }),
        Some(code) => Err(match git_said(&err) {
            said if said.is_empty() => format!("git exited with {code}"),
            said => said,
        }),
    }
}

/// Runs Git commands in `dir` that change something, one after another
/// until one fails: here, or in one ssh call on a host. The last one's
/// stdout and stderr, or what the failing one said.
fn write_on(
    runner: &Runner,
    dir: &Path,
    commands: &[Vec<String>],
) -> Result<(String, String), String> {
    match runner {
        Runner::Local => {
            let mut said = (String::new(), String::new());
            for args in commands {
                let output = git(dir)
                    .args(args)
                    .stdin(std::process::Stdio::null())
                    .output()
                    .map_err(|e| e.to_string())?;
                let err = String::from_utf8_lossy(&output.stderr).into_owned();
                if !output.status.success() {
                    return Err(git_said(&err));
                }
                said = (String::from_utf8_lossy(&output.stdout).into_owned(), err);
            }
            Ok(said)
        }
        Runner::Ssh { host, control } => {
            ssh_write(host, control.as_deref(), &write_line(dir, commands))
        }
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_owned()).collect()
}

/// Every worktree of the repository `dir` is in, its own checkout first.
/// Prunable ones (their folder is gone) are left out.
pub fn list(dir: &Path) -> Result<Vec<Worktree>, String> {
    Ok(list_all(dir)?.0)
}

/// [`list`], and how many worktrees Git still records whose folder is
/// gone: what [`prune`] would clear.
pub fn list_all(dir: &Path) -> Result<(Vec<Worktree>, usize), String> {
    list_all_on(&Runner::Local, dir)
}

/// [`list_all`], by `runner`.
pub fn list_all_on(runner: &Runner, dir: &Path) -> Result<(Vec<Worktree>, usize), String> {
    let text = run_on(runner, dir, &["worktree", "list", "--porcelain"])?;
    Ok((parse_list(&text), parse_prunable(&text)))
}

/// The prunable blocks in `git worktree list --porcelain`.
pub fn parse_prunable(text: &str) -> usize {
    text.split("\n\n")
        .filter(|block| block.lines().any(|l| l.starts_with("prunable")))
        .count()
}

/// Forgets the worktrees of the repository at `main` whose folder is gone.
pub fn prune(main: &Path) -> Result<String, String> {
    let before = list_all(main)?.1;
    run(main, &["worktree", "prune"])?;
    let pruned = before.saturating_sub(list_all(main)?.1);
    Ok(match pruned {
        1 => "Pruned 1 missing worktree".to_owned(),
        n => format!("Pruned {n} missing worktrees"),
    })
}

/// `git worktree list --porcelain`: blocks separated by blank lines, the
/// first the main worktree.
pub fn parse_list(text: &str) -> Vec<Worktree> {
    let mut out = Vec::new();
    for (index, block) in text.split("\n\n").enumerate() {
        let mut path = None;
        let mut branch = None;
        let mut skip = false;
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch ") {
                branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_owned());
            } else if line == "bare" || line.starts_with("prunable") {
                // A bare repository has no checkout to work in; a prunable
                // one's folder is gone.
                skip = true;
            }
        }
        if let Some(path) = path.filter(|_| !skip) {
            out.push(Worktree {
                path,
                branch,
                main: index == 0,
            });
        }
    }
    out
}

/// A branch name from what someone typed: lower case, runs of anything
/// but letters, digits, `/`, `.` and `_` turned into one `-`.
pub fn branch_name(typed: &str) -> String {
    let mut out = String::new();
    for c in typed.trim().chars() {
        if c.is_alphanumeric() || matches!(c, '/' | '.' | '_') {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out
        .trim_matches(|c| matches!(c, '-' | '/' | '.'))
        .to_owned();
    // Git refuses `..`, `@{` and a trailing `.lock`; keep it simple.
    out.replace("..", ".")
}

/// Where crc puts the worktrees of the repository at `main`: one folder
/// per repository, named for it, with a hash so two repositories of the
/// same name do not share one.
pub fn home(main: &Path) -> Option<PathBuf> {
    let base = match std::env::var_os("CRC_WORKTREES") {
        Some(dir) => PathBuf::from(dir),
        None => crate::platform::app_support()?.join("worktrees"),
    };
    let canonical = crate::platform::canonical(main);
    let name = canonical
        .file_name()
        .map_or_else(|| "repo".into(), |n| n.to_string_lossy().into_owned());
    let hash = crate::platform::fnv1a(canonical.as_os_str().as_encoded_bytes());
    Some(base.join(format!("{name}-{:08x}", hash as u32)))
}

/// Makes a worktree for a new task: a new branch `name` from the main
/// checkout's HEAD, in its own folder. Returns the folder.
pub fn add(main: &Path, name: &str) -> Result<PathBuf, String> {
    let branch = branch_name(name);
    if branch.is_empty() {
        return Err("Type a name with a letter or digit in it".into());
    }
    let dir = home(main)
        .ok_or("No home folder to put the worktree in")?
        .join(branch.replace('/', "-"));
    if dir.exists() {
        return Err(format!("{} already exists", dir.display()));
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let target = dir.to_string_lossy().into_owned();
    run(main, &["worktree", "add", "-b", &branch, &target, "HEAD"])?;
    Ok(dir)
}

/// Whether the checkout at `dir` has changes Git would lose on removal:
/// anything modified, staged or untracked.
pub fn is_dirty(dir: &Path) -> Result<bool, String> {
    Ok(
        !run(dir, &["status", "--porcelain", "--untracked-files=normal"])?
            .trim()
            .is_empty(),
    )
}

/// Removes a task's worktree, refused while it has changes. The branch is
/// deleted too when it is merged: into `base` (the default branch), or,
/// for a squash merge Git cannot see, when it is still at the head of its
/// merged pull request (`merged_pr_head`). Otherwise `branch -d` decides,
/// as Git does, from the main checkout's HEAD; an unmerged branch is kept,
/// and the answer says so.
pub fn remove(
    main: &Path,
    tree: &Worktree,
    base: Option<&str>,
    merged_pr_head: Option<&str>,
) -> Result<String, String> {
    if tree.main {
        return Err("The repository's own checkout is not a task".into());
    }
    if is_dirty(&tree.path)? {
        return Err(format!(
            "{} has uncommitted changes; commit or discard them first",
            tree.name()
        ));
    }
    let target = tree.path.to_string_lossy().into_owned();
    run(main, &["worktree", "remove", &target])?;
    let Some(branch) = &tree.branch else {
        return Ok(format!("Removed {}", tree.name()));
    };
    // Checked now, not from what the list last read: -D deletes whatever
    // the branch holds.
    let in_base = base.filter(|b| *b != branch).is_some_and(|b| {
        git(main)
            .args(["merge-base", "--is-ancestor", branch, b])
            .output()
            .is_ok_and(|o| o.status.success())
    });
    if in_base {
        run(main, &["branch", "-D", branch])?;
        return Ok(format!("Removed {branch} and its branch"));
    }
    if let Some(head) = merged_pr_head.filter(|h| !h.is_empty()) {
        let tip = run(main, &["rev-parse", "--verify", "--quiet", branch])
            .map(|t| t.trim().to_owned())
            .unwrap_or_default();
        if tip == head {
            run(main, &["branch", "-D", branch])?;
            return Ok(format!(
                "Removed {branch} and its branch (its pull request was merged)"
            ));
        }
        return Ok(format!(
            "Removed {branch}; the branch has commits its merged pull request does not, so it was kept"
        ));
    }
    Ok(match run(main, &["branch", "-d", branch]) {
        Ok(_) => format!("Removed {branch} and its branch"),
        Err(_) => format!("Removed {branch}; the branch has unmerged commits and was kept"),
    })
}

/// The local branches of the repository at `main` whose commits are all on
/// `base`. A branch made and never committed to is on it too; see
/// [`has_own_commits`].
pub fn merged(main: &Path, base: &str) -> Vec<String> {
    merged_on(&Runner::Local, main, base).unwrap_or_default()
}

/// [`merged`], by `runner`; the failure is told.
pub fn merged_on(runner: &Runner, main: &Path, base: &str) -> Result<Vec<String>, String> {
    let flag = format!("--merged={base}");
    run_on(
        runner,
        main,
        &[
            "for-each-ref",
            &flag,
            "refs/heads",
            "--format=%(refname:short)",
        ],
    )
    .map(|text| {
        text.lines()
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect()
    })
}

/// Whether `branch` was ever committed to: its reflog has more than the
/// line for its creation. Read from the file, without asking Git; no
/// reflog counts as no.
pub fn has_own_commits(main: &Path, branch: &str) -> bool {
    std::fs::read(main.join(".git/logs/refs/heads").join(branch))
        .is_ok_and(|log| log.split(|b| *b == b'\n').filter(|l| !l.is_empty()).count() > 1)
}

/// [`has_own_commits`] for each of `branches`, by `runner`: on a host, its
/// reflog as Git shows it, every branch in one call.
pub fn own_commits_on(
    runner: &Runner,
    main: &Path,
    branches: &[String],
) -> Result<Vec<bool>, String> {
    if *runner == Runner::Local || branches.is_empty() {
        return Ok(branches.iter().map(|b| has_own_commits(main, b)).collect());
    }
    let commands: Vec<Vec<String>> = branches
        .iter()
        .map(|b| {
            ["reflog", "show", "--format=%h", &format!("refs/heads/{b}")]
                .map(str::to_owned)
                .to_vec()
        })
        .collect();
    Ok(batch_on(runner, main, &commands)?
        .into_iter()
        .map(|said| said.is_ok_and(|text| text.lines().filter(|l| !l.is_empty()).count() > 1))
        .collect())
}

/// Merges a task's branch into whatever the main checkout has out.
/// Refused while the main checkout has changes of its own; a conflict
/// leaves the merge in progress for Source Control to finish or abort.
pub fn merge(main: &Path, branch: &str) -> Result<String, String> {
    if is_dirty(main)? {
        return Err("The main checkout has uncommitted changes; commit or stash them first".into());
    }
    let into = run(main, &["branch", "--show-current"])?.trim().to_owned();
    match run(main, &["merge", "--no-edit", branch]) {
        Ok(_) => Ok(format!("Merged {branch} into {into}")),
        Err(e) if e.contains("CONFLICT") || e.contains("Automatic merge failed") => Err(format!(
            "Merging {branch} into {into} stopped on conflicts; resolve them in Source Control"
        )),
        Err(e) => Err(e),
    }
}

/// How far the task's branch is from `base` (the default branch; `None`
/// is the main checkout's HEAD): commits on it the base does not have.
pub fn ahead(main: &Path, base: Option<&str>, branch: &str) -> Result<usize, String> {
    let range = format!("{}..{branch}", base.unwrap_or("HEAD"));
    Ok(run(main, &["rev-list", "--count", &range])?
        .trim()
        .parse()
        .unwrap_or(0))
}

/// [`ahead`] for each of `branches`, by `runner`, in one batch; a branch
/// Git could not count is 0.
pub fn aheads_on(
    runner: &Runner,
    main: &Path,
    base: Option<&str>,
    branches: &[String],
) -> Result<Vec<usize>, String> {
    let commands: Vec<Vec<String>> = branches
        .iter()
        .map(|b| {
            let range = format!("{}..{b}", base.unwrap_or("HEAD"));
            vec!["rev-list".to_owned(), "--count".to_owned(), range]
        })
        .collect();
    Ok(batch_on(runner, main, &commands)?
        .into_iter()
        .map(|said| said.map_or(0, |text| text.trim().parse().unwrap_or(0)))
        .collect())
}

/// A local branch and its last commit: when (Unix seconds, the committer's
/// date) and its subject.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tip {
    pub branch: String,
    pub time: u64,
    pub subject: String,
}

/// Every local branch of the repository at `main` and its last commit, in
/// one `for-each-ref`.
pub fn tips(main: &Path) -> Result<Vec<Tip>, String> {
    tips_on(&Runner::Local, main)
}

/// [`tips`], by `runner`.
pub fn tips_on(runner: &Runner, main: &Path) -> Result<Vec<Tip>, String> {
    Ok(parse_tips(&run_on(
        runner,
        main,
        &[
            "for-each-ref",
            "refs/heads",
            "--format=%(refname:short)%09%(committerdate:unix)%09%(subject)",
        ],
    )?))
}

/// `for-each-ref` with the format [`tips`] asks for: branch, time and
/// subject, tab-separated (a branch name has no tab; a subject may).
pub fn parse_tips(text: &str) -> Vec<Tip> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let branch = parts.next().filter(|b| !b.is_empty())?;
            let time = parts.next()?.trim().parse().unwrap_or(0);
            Some(Tip {
                branch: branch.to_owned(),
                time,
                subject: parts.next().unwrap_or("").to_owned(),
            })
        })
        .collect()
}

/// The repository's default branch, which its worktrees are compared
/// with: what `origin/HEAD` points at (the local branch of that name when
/// there is one, else the remote one), else `init.defaultBranch` when that
/// branch exists, else `main` or `master`, else what the main checkout has
/// out. `branches` are the local ones, from [`tips`].
pub fn default_branch(main: &Path, branches: &[Tip], current: Option<&str>) -> Option<String> {
    default_branch_on(&Runner::Local, main, branches, current)
        .ok()
        .flatten()
}

/// [`default_branch`], by `runner`, its two questions in one batch. `Err`
/// when neither could be asked.
pub fn default_branch_on(
    runner: &Runner,
    main: &Path,
    branches: &[Tip],
    current: Option<&str>,
) -> Result<Option<String>, String> {
    let commands = [
        vec![
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
        vec!["config", "--get", "init.defaultBranch"],
    ]
    .map(|args| args.into_iter().map(str::to_owned).collect::<Vec<_>>());
    let mut said = batch_on(runner, main, &commands)?
        .into_iter()
        .map(Result::ok);
    let (origin, init) = (said.next().flatten(), said.next().flatten());
    Ok(pick_default(
        origin.as_deref(),
        init.as_deref(),
        branches,
        current,
    ))
}

/// [`default_branch`]'s choice from what Git said.
pub fn pick_default(
    origin_head: Option<&str>,
    init_default: Option<&str>,
    branches: &[Tip],
    current: Option<&str>,
) -> Option<String> {
    let has = |name: &str| branches.iter().any(|t| t.branch == name);
    if let Some(remote) = origin_head.map(str::trim).filter(|r| !r.is_empty()) {
        let local = remote.strip_prefix("origin/").unwrap_or(remote);
        return Some(if has(local) { local } else { remote }.to_owned());
    }
    init_default
        .map(str::trim)
        .filter(|b| !b.is_empty() && has(b))
        .or_else(|| ["main", "master"].into_iter().find(|b| has(b)))
        .or(current)
        .map(str::to_owned)
}

/// A task branch's pull request on GitHub, as `gh` reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    /// `OPEN`, `MERGED` or `CLOSED`.
    pub state: String,
    pub url: String,
    pub checks: Checks,
    pub title: String,
    /// The author's login.
    pub author: String,
    pub commits: usize,
    pub additions: u64,
    pub deletions: u64,
    /// The branch it merges into, and the one it merges.
    pub base: String,
    pub head: String,
    pub draft: bool,
    /// `MERGEABLE`, `CONFLICTING` or `UNKNOWN`.
    pub mergeable: String,
    /// The commit its branch was at, last GitHub saw it.
    pub head_oid: String,
    /// Each check, as GitHub names it.
    pub list: Vec<Check>,
}

/// One check on a pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub state: CheckState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckState {
    Passed,
    Failed,
    Running,
    /// Skipped or neutral: it ran and does not count either way.
    Skipped,
}

impl CheckState {
    pub fn label(self) -> &'static str {
        match self {
            CheckState::Passed => "Success",
            CheckState::Failed => "Failed",
            CheckState::Running => "Running",
            CheckState::Skipped => "Skipped",
        }
    }
}

/// Its checks, rolled up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Checks {
    #[default]
    None,
    Running,
    Passing,
    Failing,
}

/// `gh`, where a GUI app can find it: not from the shell's PATH.
fn gh() -> Option<PathBuf> {
    crate::lsp::servers::search_dirs()
        .into_iter()
        .map(|dir| dir.join("gh"))
        .find(|path| path.is_file())
}

/// The pull request for `branch`, asked of `gh` in `dir`: `None` when the
/// branch has none; an error without `gh`, offline, or when `gh` failed,
/// so a row keeps what it last said.
pub fn pull_request(dir: &Path, branch: &str) -> Result<Option<PullRequest>, String> {
    let gh = gh().ok_or("GitHub's gh is not installed")?;
    let output = Command::new(gh)
        .current_dir(dir)
        .args([
            "pr",
            "view",
            branch,
            "--json",
            "number,state,url,statusCheckRollup,title,author,commits,additions,deletions,\
             baseRefName,headRefName,headRefOid,isDraft,mergeable",
        ])
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    pull_request_answer(
        output.status.success(),
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
    )
}

/// What `gh pr view` said: the pull request, none (gh says so), or a
/// failure.
pub fn pull_request_answer(ok: bool, out: &str, err: &str) -> Result<Option<PullRequest>, String> {
    if ok {
        return parse_pull_request(out)
            .map(Some)
            .ok_or_else(|| "gh said something unreadable".to_owned());
    }
    if err.contains("no pull requests found") {
        return Ok(None);
    }
    Err(err.trim().lines().last().unwrap_or("gh failed").to_owned())
}

pub fn parse_pull_request(text: &str) -> Option<PullRequest> {
    let value = crate::json::parse(text).ok()?;
    let checks = value
        .get("statusCheckRollup")
        .and_then(|v| v.as_array())
        .unwrap_or(&[]);
    let word = |v: &crate::json::Value, key: &str| {
        v.get(key)
            .and_then(|w| w.as_str())
            .unwrap_or("")
            .to_ascii_uppercase()
    };
    let text = |v: &crate::json::Value, key: &str| {
        v.get(key).and_then(|w| w.as_str()).unwrap_or("").to_owned()
    };
    // A check run says status and conclusion; a commit status says state.
    let list: Vec<Check> = checks
        .iter()
        .map(|c| {
            let conclusion = word(c, "conclusion");
            let status = word(c, "status");
            let state = word(c, "state");
            let check = if matches!(
                conclusion.as_str(),
                "FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
            ) || matches!(state.as_str(), "FAILURE" | "ERROR")
            {
                CheckState::Failed
            } else if (!status.is_empty() && status != "COMPLETED")
                || matches!(state.as_str(), "PENDING" | "EXPECTED")
            {
                CheckState::Running
            } else if matches!(conclusion.as_str(), "SKIPPED" | "NEUTRAL" | "STALE") {
                CheckState::Skipped
            } else {
                CheckState::Passed
            };
            let name = Some(text(c, "name"))
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| text(c, "context"));
            Check { name, state: check }
        })
        .collect();
    let failed = list.iter().any(|c| c.state == CheckState::Failed);
    let running = list.iter().any(|c| c.state == CheckState::Running);
    let number = |key: &str| value.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    Some(PullRequest {
        number: value.get("number")?.as_u64()?,
        state: word(&value, "state"),
        url: text(&value, "url"),
        checks: if list.is_empty() {
            Checks::None
        } else if failed {
            Checks::Failing
        } else if running {
            Checks::Running
        } else {
            Checks::Passing
        },
        title: text(&value, "title"),
        author: value
            .get("author")
            .map(|a| text(a, "login"))
            .unwrap_or_default(),
        commits: value
            .get("commits")
            .and_then(|v| v.as_array())
            .map_or(0, <[_]>::len),
        additions: number("additions"),
        deletions: number("deletions"),
        base: text(&value, "baseRefName"),
        head: text(&value, "headRefName"),
        draft: value
            .get("isDraft")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        mergeable: word(&value, "mergeable"),
        head_oid: text(&value, "headRefOid"),
        list,
    })
}

/// Where a workspace's checkout stands: against the branch it came from,
/// against its upstream, and its own uncommitted work. Read for the
/// inspector, on a worker.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub branch: Option<String>,
    /// The branch it is compared with: the repository's default one.
    pub base: Option<String>,
    /// Commits it has that the base does not, and the other way round.
    pub ahead: usize,
    pub behind: usize,
    /// The files it changes from where it left the base, and the lines.
    pub files: usize,
    pub added: u64,
    pub removed: u64,
    pub upstream: Option<String>,
    pub to_push: usize,
    pub to_pull: usize,
    /// Uncommitted: files touched (untracked included) and their lines.
    pub dirty: usize,
    pub dirty_added: u64,
    pub dirty_removed: u64,
    /// The newest tag reachable from HEAD and the commits since it; read
    /// for the main checkout only.
    pub tag: Option<(String, usize)>,
}

/// `git rev-list --left-right --count A...B`: the commits only on A, then
/// only on B.
pub fn parse_left_right(text: &str) -> (usize, usize) {
    let mut words = text.split_whitespace().map(|w| w.parse().unwrap_or(0));
    (words.next().unwrap_or(0), words.next().unwrap_or(0))
}

/// `git diff --numstat`: files, lines added, lines removed. A binary file
/// counts as a file with no lines.
pub fn parse_numstat(text: &str) -> (usize, u64, u64) {
    text.lines().filter(|l| !l.trim().is_empty()).fold(
        (0, 0, 0),
        |(files, added, removed), line| {
            let mut words = line.split('\t');
            let a = words.next().and_then(|w| w.parse().ok()).unwrap_or(0);
            let r = words.next().and_then(|w| w.parse().ok()).unwrap_or(0);
            (files + 1, added + a, removed + r)
        },
    )
}

/// Reads `dir`'s status, comparing with `base` when it is another branch.
pub fn status(dir: &Path, base: Option<&str>, tags: bool) -> Result<Status, String> {
    run(dir, &["rev-parse", "--git-dir"])?;
    let branch = run(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|b| b.trim().to_owned())
        .filter(|b| !b.is_empty());
    let mut out = Status {
        branch: branch.clone(),
        ..Status::default()
    };
    if let Some(base) = base.filter(|b| branch.as_deref() != Some(*b)) {
        let range = format!("{base}...HEAD");
        if let Ok(counts) = run(dir, &["rev-list", "--left-right", "--count", &range]) {
            (out.behind, out.ahead) = parse_left_right(&counts);
            out.base = Some(base.to_owned());
            if let Ok(stat) = run(dir, &["diff", "--numstat", &range]) {
                (out.files, out.added, out.removed) = parse_numstat(&stat);
            }
        }
    }
    if let Ok(upstream) = run(
        dir,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    ) {
        out.upstream = Some(upstream.trim().to_owned());
        if let Ok(counts) = run(dir, &["rev-list", "--left-right", "--count", "@{u}...HEAD"]) {
            (out.to_pull, out.to_push) = parse_left_right(&counts);
        }
    }
    out.dirty = run(dir, &["status", "--porcelain"])
        .map(|s| s.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0);
    if out.dirty > 0
        && let Ok(stat) = run(dir, &["diff", "HEAD", "--numstat"])
    {
        let (_, a, r) = parse_numstat(&stat);
        (out.dirty_added, out.dirty_removed) = (a, r);
    }
    if tags && let Ok(tag) = run(dir, &["describe", "--tags", "--abbrev=0"]) {
        let tag = tag.trim().to_owned();
        let since = run(dir, &["rev-list", "--count", &format!("{tag}..HEAD")])
            .ok()
            .and_then(|n| n.trim().parse().ok())
            .unwrap_or(0);
        out.tag = Some((tag, since));
    }
    Ok(out)
}

/// The questions [`status_on`] asks a host, all at once: what [`status`]
/// asks here one after another, the base's two whether or not it turns out
/// to be the branch itself, and the latest tag with its count in one
/// `describe --long`.
pub fn status_commands(base: Option<&str>, tags: bool) -> Vec<Vec<String>> {
    let mut commands = vec![
        strings(&["rev-parse", "--git-dir"]),
        strings(&["symbolic-ref", "--quiet", "--short", "HEAD"]),
        strings(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]),
        strings(&["rev-list", "--left-right", "--count", "@{u}...HEAD"]),
        strings(&["status", "--porcelain"]),
        strings(&["diff", "HEAD", "--numstat"]),
    ];
    if let Some(base) = base {
        let range = format!("{base}...HEAD");
        commands.push(strings(&["rev-list", "--left-right", "--count", &range]));
        commands.push(strings(&["diff", "--numstat", &range]));
    }
    if tags {
        commands.push(strings(&["describe", "--tags", "--long"]));
    }
    commands
}

/// The answers to [`status_commands`], as [`status`] reads them.
pub fn parse_status(
    said: &[Result<String, String>],
    base: Option<&str>,
    tags: bool,
) -> Result<Status, String> {
    let at = |n: usize| said.get(n).and_then(|s| s.as_ref().ok());
    if let Some(Err(e)) = said.first() {
        return Err(e.clone());
    }
    let branch = at(1).map(|b| b.trim().to_owned()).filter(|b| !b.is_empty());
    let mut out = Status {
        branch: branch.clone(),
        ..Status::default()
    };
    if let Some(upstream) = at(2) {
        out.upstream = Some(upstream.trim().to_owned());
        if let Some(counts) = at(3) {
            (out.to_pull, out.to_push) = parse_left_right(counts);
        }
    }
    out.dirty = at(4).map_or(0, |s| s.lines().filter(|l| !l.is_empty()).count());
    if out.dirty > 0
        && let Some(stat) = at(5)
    {
        let (_, a, r) = parse_numstat(stat);
        (out.dirty_added, out.dirty_removed) = (a, r);
    }
    let mut next = 6;
    if let Some(base) = base {
        if branch.as_deref() != Some(base)
            && let Some(counts) = at(next)
        {
            (out.behind, out.ahead) = parse_left_right(counts);
            out.base = Some(base.to_owned());
            if let Some(stat) = at(next + 1) {
                (out.files, out.added, out.removed) = parse_numstat(stat);
            }
        }
        next += 2;
    }
    if tags {
        out.tag = at(next).and_then(|d| parse_describe(d.trim()));
    }
    Ok(out)
}

/// `git describe --tags --long`: the tag, then the commits since it (the
/// tag's own name may have dashes; the last two parts are Git's).
pub fn parse_describe(text: &str) -> Option<(String, usize)> {
    let mut parts = text.rsplitn(3, '-');
    let _hash = parts.next()?;
    let since = parts.next()?.parse().ok()?;
    let tag = parts.next().filter(|t| !t.is_empty())?;
    Some((tag.to_owned(), since))
}

/// [`status`], by `runner`: on a host, every question in one ssh call.
pub fn status_on(
    runner: &Runner,
    dir: &Path,
    base: Option<&str>,
    tags: bool,
) -> Result<Status, String> {
    if *runner == Runner::Local {
        return status(dir, base, tags);
    }
    let said = batch_on(runner, dir, &status_commands(base, tags))?;
    parse_status(&said, base, tags)
}

/// Merges `base` into the workspace's branch: the way to take main's new
/// commits without rewriting the branch. Refused with uncommitted changes;
/// on a conflict the merge is undone, so nothing is left half done.
pub fn update_from(dir: &Path, base: &str) -> Result<String, String> {
    update_from_on(&Runner::Local, dir, base)
}

/// [`update_from`], by `runner`: on a host, the checks in one read and
/// the merge in one call.
pub fn update_from_on(runner: &Runner, dir: &Path, base: &str) -> Result<String, String> {
    let mut read = batch_on(
        runner,
        dir,
        &[
            strings(&["status", "--porcelain", "--untracked-files=normal"]),
            strings(&["branch", "--show-current"]),
        ],
    )?
    .into_iter();
    let dirty = read.next().unwrap_or_else(|| Err(UNREACHABLE.into()))?;
    if !dirty.trim().is_empty() {
        return Err("Commit or stash the uncommitted changes first".into());
    }
    let branch = read
        .next()
        .unwrap_or_else(|| Err(UNREACHABLE.into()))?
        .trim()
        .to_owned();
    match write_on(runner, dir, &[strings(&["merge", "--no-edit", base])]) {
        Ok((said, _)) if said.contains("Already up to date") => {
            Ok(format!("{branch} already has everything on {base}"))
        }
        Ok(_) => Ok(format!("Merged {base} into {branch}")),
        Err(e) => {
            let conflicted = run_on(runner, dir, &["diff", "--name-only", "--diff-filter=U"])
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if conflicted.is_empty() {
                return Err(e);
            }
            let _ = write_on(runner, dir, &[strings(&["merge", "--abort"])]);
            Err(format!(
                "{base} conflicts with {branch} in {}; nothing changed. Merge it in a terminal there to resolve them",
                conflicted.join(", ")
            ))
        }
    }
}

/// Stages everything in the workspace, untracked files too, and commits
/// it with `message`.
pub fn commit_all(dir: &Path, message: &str) -> Result<String, String> {
    if message.trim().is_empty() {
        return Err("Write a commit message first".into());
    }
    let files = run(dir, &["status", "--porcelain"])?
        .lines()
        .filter(|l| !l.is_empty())
        .count();
    if files == 0 {
        return Err("Nothing to commit".into());
    }
    run(dir, &["add", "--all"])?;
    crate::project::git::commit(dir, message)?;
    Ok(committed(files))
}

fn committed(files: usize) -> String {
    match files {
        1 => "Committed 1 file".to_owned(),
        n => format!("Committed {n} files"),
    }
}

/// [`commit_all`], by `runner`. On a host the message goes as an argument
/// (`-m`, which Git cleans up as it does a message from a file) and the
/// commit is made with the host's own Git settings: one that signs with a
/// key wanting a passphrase fails, saying so, as nothing can be asked.
pub fn commit_all_on(runner: &Runner, dir: &Path, message: &str) -> Result<String, String> {
    if message.trim().is_empty() {
        return Err("Write a commit message first".into());
    }
    let files = run_on(runner, dir, &["status", "--porcelain"])?
        .lines()
        .filter(|l| !l.is_empty())
        .count();
    if files == 0 {
        return Err("Nothing to commit".into());
    }
    write_on(
        runner,
        dir,
        &[
            strings(&["add", "--all"]),
            strings(&["commit", "-q", "-m", message]),
        ],
    )?;
    Ok(committed(files))
}

/// Pull (fast-forward only) or push in `dir` by `runner`, with the flags
/// and refusals of [`crate::project::git::remote`]: on a host, the checks
/// in one read and the pull or push in one call. What Git said, as there.
pub fn remote_on(
    runner: &Runner,
    dir: &Path,
    what: crate::project::git::Remote,
) -> Result<String, String> {
    use crate::project::git::Remote;
    let mut read = batch_on(
        runner,
        dir,
        &[
            strings(&["symbolic-ref", "--quiet", "--short", "HEAD"]),
            strings(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]),
            strings(&["remote"]),
        ],
    )?
    .into_iter();
    let branch = read
        .next()
        .and_then(Result::ok)
        .map(|b| b.trim().to_owned())
        .filter(|b| !b.is_empty());
    let upstream = read.next().is_some_and(|u| u.is_ok());
    let remotes = read.next().and_then(Result::ok).unwrap_or_default();
    let args = crate::project::git::remote_args(what, branch.as_deref(), upstream, || {
        remotes.lines().any(|r| r == "origin")
    })?;
    match write_on(runner, dir, &[args]) {
        Ok((out, err)) => Ok(match err.trim() {
            "" => out.trim().to_owned(),
            err => err.to_owned(),
        }),
        Err(e) if what == Remote::Pull && e.contains("fast-forward") => {
            Err("the branches have diverged: pull with rebase or merge in a terminal there".into())
        }
        Err(e) => Err(e),
    }
}

/// `gh` in `dir`, never asking anything: what it printed, or its error.
fn run_gh(dir: &Path, args: &[&str]) -> Result<String, String> {
    let gh = gh().ok_or("GitHub's gh is not installed")?;
    let output = Command::new(gh)
        .current_dir(dir)
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        let said = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(said.lines().last().unwrap_or("gh failed").to_owned())
    }
}

/// Whether `gh` is there to ask about pull requests.
pub fn has_gh() -> bool {
    gh().is_some()
}

/// Opens a pull request for the workspace's branch, its title and body
/// from the commits. The branch must be pushed first.
pub fn create_pull_request(dir: &Path) -> Result<String, String> {
    let url = run_gh(dir, &["pr", "create", "--fill"])?;
    Ok(format!(
        "Opened {}",
        url.lines().last().unwrap_or("the pull request")
    ))
}

/// How a pull request is merged on GitHub.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeStrategy {
    Squash,
    Merge,
    Rebase,
}

impl MergeStrategy {
    fn flag(self) -> &'static str {
        match self {
            MergeStrategy::Squash => "--squash",
            MergeStrategy::Merge => "--merge",
            MergeStrategy::Rebase => "--rebase",
        }
    }
}

/// Merges pull request `number` on GitHub. The branch and its worktree
/// stay; removing them is the task menu's.
pub fn merge_pull_request(
    dir: &Path,
    number: u64,
    strategy: MergeStrategy,
) -> Result<String, String> {
    let n = number.to_string();
    run_gh(dir, &["pr", "merge", &n, strategy.flag()])?;
    Ok(format!("Merged pull request #{number}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_a_pull_requests_details_and_each_check() {
        let pr = parse_pull_request(
            r#"{"number":648,"state":"OPEN","url":"u","title":"Kill the session","author":{"login":"sam"},"commits":[{},{},{},{}],"additions":423,"deletions":42,"baseRefName":"main","headRefName":"fix/close","headRefOid":"abc123","isDraft":false,"mergeable":"MERGEABLE","statusCheckRollup":[{"__typename":"CheckRun","name":"build","status":"COMPLETED","conclusion":"SUCCESS"},{"__typename":"CheckRun","name":"lint","status":"COMPLETED","conclusion":"SKIPPED"},{"__typename":"StatusContext","context":"ci/policy","state":"PENDING"}]}"#,
        )
        .unwrap();
        assert_eq!(
            (
                pr.title.as_str(),
                pr.author.as_str(),
                pr.commits,
                pr.additions,
                pr.deletions,
                pr.base.as_str(),
                pr.head.as_str(),
                pr.mergeable.as_str(),
            ),
            (
                "Kill the session",
                "sam",
                4,
                423,
                42,
                "main",
                "fix/close",
                "MERGEABLE"
            )
        );
        let names: Vec<(&str, CheckState)> =
            pr.list.iter().map(|c| (c.name.as_str(), c.state)).collect();
        assert_eq!(
            names,
            [
                ("build", CheckState::Passed),
                ("lint", CheckState::Skipped),
                ("ci/policy", CheckState::Running)
            ]
        );
        assert_eq!(pr.checks, Checks::Running);
        assert_eq!(pr.head_oid, "abc123");
    }

    #[test]
    fn reads_counts_and_numstat() {
        assert_eq!(parse_left_right("3\t5\n"), (3, 5));
        assert_eq!(parse_left_right(""), (0, 0));
        assert_eq!(
            parse_numstat("10\t2\tsrc/a.rs\n-\t-\tlogo.png\n1\t0\tb.md\n"),
            (3, 11, 2)
        );
    }

    #[test]
    fn reads_a_pull_request_and_its_checks() {
        let pr = parse_pull_request(
            r#"{"number":12,"state":"OPEN","url":"https://github.com/o/r/pull/12","statusCheckRollup":[{"status":"COMPLETED","conclusion":"SUCCESS"},{"status":"IN_PROGRESS","conclusion":""}]}"#,
        )
        .unwrap();
        assert_eq!(
            (pr.number, pr.state.as_str(), pr.checks),
            (12, "OPEN", Checks::Running)
        );
        let failing = parse_pull_request(
            r#"{"number":3,"state":"OPEN","url":"u","statusCheckRollup":[{"status":"COMPLETED","conclusion":"FAILURE"},{"status":"IN_PROGRESS"}]}"#,
        )
        .unwrap();
        assert_eq!(failing.checks, Checks::Failing);
        let none =
            parse_pull_request(r#"{"number":4,"state":"MERGED","url":"u","statusCheckRollup":[]}"#)
                .unwrap();
        assert_eq!((none.state.as_str(), none.checks), ("MERGED", Checks::None));
        assert_eq!(parse_pull_request("not json"), None);
    }

    use super::*;

    #[test]
    fn no_pull_request_is_not_a_failure() {
        let open = pull_request_answer(true, r#"{"number":7,"state":"OPEN","url":"u"}"#, "");
        assert_eq!(open.unwrap().map(|pr| pr.number), Some(7));
        assert_eq!(
            pull_request_answer(
                false,
                "",
                "no pull requests found for branch \"fix/seed-dates\"\n"
            ),
            Ok(None)
        );
        assert_eq!(
            pull_request_answer(false, "", "warn\nerror connecting to api.github.com\n"),
            Err("error connecting to api.github.com".to_owned())
        );
        assert!(pull_request_answer(true, "not json", "").is_err());
    }

    #[test]
    fn parses_the_porcelain_list() {
        let text = "worktree /r/crc\nHEAD 1111\nbranch refs/heads/main\n\n\
                    worktree /w/fix-login\nHEAD 2222\nbranch refs/heads/fix/login\n\n\
                    worktree /w/detached\nHEAD 3333\ndetached\n\n\
                    worktree /w/gone\nHEAD 4444\nbranch refs/heads/gone\nprunable gitdir file points to non-existent location\n";
        let list = parse_list(text);
        assert_eq!(list.len(), 3);
        assert!(list[0].main);
        assert_eq!(list[0].branch.as_deref(), Some("main"));
        assert_eq!(list[1].name(), "fix/login");
        assert!(!list[1].main);
        assert_eq!(list[2].branch, None);
        assert_eq!(list[2].name(), "detached");
        assert_eq!(parse_prunable(text), 1);
        assert_eq!(
            parse_prunable("worktree /r/crc\nHEAD 1\nbranch refs/heads/main\n"),
            0
        );
    }

    #[test]
    fn reads_branch_tips() {
        let tips = parse_tips(
            "main\t1760000000\tbeds\nfix/seed-dates\t1760003600\tdates:\tmarch\tapril\n\nodd\tx\t\n",
        );
        assert_eq!(
            tips,
            [
                Tip {
                    branch: "main".into(),
                    time: 1_760_000_000,
                    subject: "beds".into()
                },
                Tip {
                    branch: "fix/seed-dates".into(),
                    time: 1_760_003_600,
                    subject: "dates:\tmarch\tapril".into()
                },
                Tip {
                    branch: "odd".into(),
                    time: 0,
                    subject: String::new()
                },
            ]
        );
    }

    #[test]
    fn a_hosts_batch_is_quoted_and_read_back() {
        let commands = [
            vec![
                "for-each-ref".to_owned(),
                "--format=%(refname:short)%09x".into(),
            ],
            vec!["log".to_owned(), "it's $HOME".into()],
        ];
        assert_eq!(
            batch_line(Path::new("/srv/my app"), &commands),
            "cd '/srv/my app' || exit 1; \
             export LC_ALL=C GIT_TERMINAL_PROMPT=0 GIT_OPTIONAL_LOCKS=0; \
             git 'for-each-ref' '--format=%(refname:short)%09x' 2>/dev/null; \
             printf '\\n\\036%d\\n' $?; \
             git 'log' 'it'\\''s $HOME' 2>/dev/null; printf '\\n\\036%d\\n' $?"
        );
        assert!(batch_line(Path::new("~/x"), &[]).starts_with("cd ~/'x' || exit 1;"));
        assert_eq!(
            parse_batch("a\nb\n\n\u{1e}0\n\n\u{1e}128\nmain\n\n\u{1e}0\n", 3),
            Some(vec![
                Ok("a\nb\n".to_owned()),
                Err("git exited with 128".to_owned()),
                Ok("main\n".to_owned()),
            ])
        );
        // Cut off before the last: the host went away partway.
        assert_eq!(parse_batch("a\n\n\u{1e}0\n", 2), None);
        assert_eq!(parse_batch("", 0), Some(Vec::new()));
    }

    #[test]
    fn a_batch_run_by_a_shell_reads_as_git_run_here() {
        // What a host's shell does with the line, done by this Mac's: the
        // same answers as running each command here.
        let tree = fixture("batch-shell", "main", &["fix/it's"]);
        let repo = &tree.0;
        let commands: Vec<Vec<String>> = [
            &["worktree", "list", "--porcelain"][..],
            &[
                "for-each-ref",
                "refs/heads",
                "--format=%(refname:short)%09%(committerdate:unix)%09%(subject)",
            ],
            &[
                "symbolic-ref",
                "--quiet",
                "--short",
                "refs/remotes/origin/HEAD",
            ],
            &["rev-list", "--count", "main..fix/it's"],
        ]
        .iter()
        .map(|args| args.iter().map(|a| (*a).to_owned()).collect())
        .collect();
        let shell = Command::new("/bin/sh")
            .arg("-c")
            .arg(batch_line(repo, &commands))
            .output()
            .unwrap();
        assert!(shell.status.success());
        let remote = parse_batch(&String::from_utf8_lossy(&shell.stdout), commands.len()).unwrap();
        let here = batch_on(&Runner::Local, repo, &commands).unwrap();
        assert_eq!(remote.len(), here.len());
        for (there, here) in remote.iter().zip(&here) {
            assert_eq!(there.is_ok(), here.is_ok(), "{there:?} {here:?}");
            if let (Ok(there), Ok(here)) = (there, here) {
                assert_eq!(there, here);
            }
        }
        assert_eq!(parse_list(remote[0].as_ref().unwrap()).len(), 1);
        assert_eq!(parse_tips(remote[1].as_ref().unwrap()).len(), 2);
        assert!(remote[2].is_err());
        // A folder that is not there: nothing ran.
        let gone = Command::new("/bin/sh")
            .arg("-c")
            .arg(batch_line(&repo.join("gone"), &commands))
            .output()
            .unwrap();
        assert!(!gone.status.success() && gone.stdout.is_empty());
    }

    #[test]
    fn a_hosts_status_reads_as_the_local_one() {
        // A main checkout with a tag, a commit since, a branch behind it as
        // the base, and a change not committed: the batch a host's shell
        // runs reads back as `status` reads it here.
        let tree = fixture("status-shell", "main", &["old"]);
        let repo = &tree.0;
        let ok = |args: &[&str]| assert!(git(repo).args(args).output().unwrap().status.success());
        ok(&["-c", "tag.gpgsign=false", "tag", "v1-rc"]);
        std::fs::write(repo.join("b.txt"), "two\nthree\n").unwrap();
        ok(&["add", "b.txt"]);
        ok(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "two",
        ]);
        std::fs::write(repo.join("a.txt"), "one\nmore\n").unwrap();
        std::fs::write(repo.join("new.txt"), "new\n").unwrap();
        for (base, tags) in [(Some("old"), true), (Some("main"), false), (None, true)] {
            let commands = status_commands(base, tags);
            let shell = Command::new("/bin/sh")
                .arg("-c")
                .arg(batch_line(repo, &commands))
                .output()
                .unwrap();
            let said =
                parse_batch(&String::from_utf8_lossy(&shell.stdout), commands.len()).unwrap();
            let there = parse_status(&said, base, tags).unwrap();
            assert_eq!(there, status(repo, base, tags).unwrap(), "{base:?} {tags}");
            if base == Some("old") {
                assert_eq!((there.ahead, there.behind, there.files), (1, 0, 1));
                assert_eq!((there.dirty, there.dirty_added), (2, 1));
                assert_eq!(there.tag, Some(("v1-rc".to_owned(), 1)));
            }
        }
        // Not a repository: the first answer says so.
        let said = [Err("git exited with 128".to_owned())];
        assert!(parse_status(&said, None, false).is_err());
    }

    #[test]
    fn reads_a_describe_with_dashes_in_the_tag() {
        assert_eq!(
            parse_describe("v1.2-rc-1-4-gabc123"),
            Some(("v1.2-rc-1".to_owned(), 4))
        );
        assert_eq!(parse_describe("v1-0-g1"), Some(("v1".to_owned(), 0)));
        assert_eq!(parse_describe("gabc123"), None);
        assert_eq!(parse_describe(""), None);
    }

    #[test]
    fn a_hosts_change_is_one_line_that_stops_at_the_first_failure() {
        let commands = [
            vec!["add".to_owned(), "--all".into()],
            vec!["commit".to_owned(), "-m".into(), "it's $HOME\nmore".into()],
        ];
        assert_eq!(
            write_line(Path::new("/srv/my app"), &commands),
            "cd '/srv/my app' || exit 1; export LC_ALL=C GIT_TERMINAL_PROMPT=0; \
             git 'add' '--all' && git 'commit' '-m' 'it'\\''s $HOME\nmore'"
        );
        // Run by a shell: the message arrives as typed, and a failing first
        // command stops the rest with Git's own error on stderr.
        let tree = fixture("write-shell", "main", &[]);
        let repo = &tree.0;
        let ok = |args: &[&str]| assert!(git(repo).args(args).output().unwrap().status.success());
        ok(&["config", "user.name", "t"]);
        ok(&["config", "user.email", "t@t"]);
        std::fs::write(repo.join("b.txt"), "b\n").unwrap();
        let run = |line: String| {
            Command::new("/bin/sh")
                .arg("-c")
                .arg(line)
                .output()
                .unwrap()
        };
        assert!(run(write_line(repo, &commands)).status.success());
        let log = git(repo)
            .args(["log", "-1", "--format=%B"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&log.stdout).trim(),
            "it's $HOME\nmore"
        );
        let failed = run(write_line(
            repo,
            &[
                vec!["merge".to_owned(), "nope".into()],
                vec![
                    "-c".to_owned(),
                    "tag.gpgsign=false".into(),
                    "tag".into(),
                    "after".into(),
                ],
            ],
        ));
        assert!(!failed.status.success());
        assert!(!String::from_utf8_lossy(&failed.stderr).is_empty());
        let tags = git(repo).args(["tag"]).output().unwrap();
        assert!(String::from_utf8_lossy(&tags.stdout).trim().is_empty());
    }

    #[test]
    fn commit_pull_and_push_by_a_runner_do_what_they_do_here() {
        // By the local runner, the same steps a host's run: committed, the
        // branch pushed to origin with its upstream set, then pulled.
        let tree = fixture("runner-ops", "main", &[]);
        let repo = &tree.0;
        let origin = repo.join("origin.git");
        let ok = |args: &[&str]| assert!(git(repo).args(args).output().unwrap().status.success());
        ok(&["config", "user.name", "t"]);
        ok(&["config", "user.email", "t@t"]);
        ok(&[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            &origin.to_string_lossy(),
        ]);
        let runner = Runner::Local;
        use crate::project::git::Remote;
        assert_eq!(
            remote_on(&runner, repo, Remote::Push),
            Err("this branch has no upstream and there is no origin remote".to_owned())
        );
        assert_eq!(
            remote_on(&runner, repo, Remote::Pull),
            Err("this branch has no upstream to pull from".to_owned())
        );
        ok(&["remote", "add", "origin", &origin.to_string_lossy()]);
        // The bare origin is inside the checkout: kept out of what is committed.
        std::fs::write(repo.join(".git/info/exclude"), "origin.git/\n").unwrap();
        assert_eq!(
            commit_all_on(&runner, repo, "  "),
            Err("Write a commit message first".to_owned())
        );
        assert_eq!(
            commit_all_on(&runner, repo, "wip"),
            Err("Nothing to commit".to_owned())
        );
        std::fs::write(repo.join("b.txt"), "b\n").unwrap();
        assert_eq!(
            commit_all_on(&runner, repo, "wip"),
            Ok("Committed 1 file".to_owned())
        );
        remote_on(&runner, repo, Remote::Push).unwrap();
        let upstream = run(repo, &["rev-parse", "--abbrev-ref", "@{u}"]).unwrap();
        assert_eq!(upstream.trim(), "origin/main");
        assert!(remote_on(&runner, repo, Remote::Pull).is_ok());
        assert_eq!(
            update_from_on(&runner, repo, "main"),
            Ok("main already has everything on main".to_owned())
        );
    }

    /// A repository on `branch` with one commit, and `others` branched
    /// from it.
    fn fixture(name: &str, branch: &str, others: &[&str]) -> crate::project::TempTree {
        let tree = crate::project::TempTree::new(name, &[("a.txt", "one\n")]);
        let repo = &tree.0;
        let ok = |args: &[&str]| assert!(git(repo).args(args).output().unwrap().status.success());
        ok(&["init", "-q", "-b", branch]);
        ok(&["config", "commit.gpgsign", "false"]);
        ok(&["add", "a.txt"]);
        ok(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "one",
        ]);
        for other in others {
            ok(&["branch", other]);
        }
        tree
    }

    fn resolved(repo: &Path) -> Option<String> {
        let tips = tips(repo).unwrap();
        let current = run(repo, &["branch", "--show-current"]).unwrap();
        default_branch(repo, &tips, Some(current.trim()))
    }

    #[test]
    fn the_default_branch_is_origin_head_then_config_then_main_or_master() {
        // origin/HEAD set: its branch, the local one when there is one,
        // whatever is checked out.
        let tree = fixture("default-origin", "work", &["trunk"]);
        let repo = &tree.0;
        let ok = |args: &[&str]| assert!(git(repo).args(args).output().unwrap().status.success());
        ok(&["update-ref", "refs/remotes/origin/trunk", "HEAD"]);
        ok(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/trunk",
        ]);
        assert_eq!(resolved(repo).as_deref(), Some("trunk"));
        ok(&["branch", "-D", "trunk"]);
        assert_eq!(resolved(repo).as_deref(), Some("origin/trunk"));

        // Only init.defaultBranch, naming a branch that exists.
        let tree = fixture("default-config", "work", &["stable", "main"]);
        let repo = &tree.0;
        assert!(
            git(repo)
                .args(["config", "init.defaultBranch", "stable"])
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(resolved(repo).as_deref(), Some("stable"));
        // Naming one that does not: main.
        assert!(
            git(repo)
                .args(["config", "init.defaultBranch", "gone"])
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(resolved(repo).as_deref(), Some("main"));

        // Only master.
        let tree = fixture("default-master", "work", &["master"]);
        assert_eq!(resolved(&tree.0).as_deref(), Some("master"));

        // None of them: what the main checkout has out.
        let tree = fixture("default-none", "work", &["other"]);
        assert_eq!(resolved(&tree.0).as_deref(), Some("work"));
    }

    #[test]
    fn branch_names_from_typing() {
        assert_eq!(branch_name("Fix the login bug!"), "fix-the-login-bug");
        assert_eq!(branch_name("  feat/Agents  mode "), "feat/agents-mode");
        assert_eq!(branch_name("a..b"), "a.b");
        assert_eq!(branch_name("!!!"), "");
    }

    #[test]
    fn makes_merges_and_removes_a_task() {
        let tree = crate::project::TempTree::new("worktree", &[("a.txt", "one\n")]);
        let repo = &tree.0;
        let ok = |args: &[&str]| assert!(git(repo).args(args).output().unwrap().status.success());
        ok(&["init", "-q", "-b", "main"]);
        // The person's own config may sign commits, which waits on them.
        ok(&["config", "commit.gpgsign", "false"]);
        ok(&["-c", "user.name=t", "-c", "user.email=t@t", "add", "a.txt"]);
        ok(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "one",
        ]);
        let home = repo.with_extension("trees");
        // SAFETY: tests in this module are the only readers of the variable.
        unsafe { std::env::set_var("CRC_WORKTREES", &home) };

        let dir = add(repo, "Try It").unwrap();
        assert!(dir.join("a.txt").is_file());
        let list = list(repo).unwrap();
        assert_eq!(list.len(), 2);
        let task = list[1].clone();
        assert_eq!(task.branch.as_deref(), Some("try-it"));

        std::fs::write(dir.join("b.txt"), "two\n").unwrap();
        assert!(is_dirty(&dir).unwrap());
        assert!(remove(repo, &task, Some("main"), None).is_err());
        let commit = |args: &[&str]| {
            assert!(git(&dir).args(args).output().unwrap().status.success());
        };
        commit(&["add", "b.txt"]);
        commit(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "two",
        ]);
        assert_eq!(ahead(repo, Some("main"), "try-it").unwrap(), 1);
        assert_eq!(ahead(repo, None, "try-it").unwrap(), 1);
        assert_eq!(merge(repo, "try-it").unwrap(), "Merged try-it into main");
        assert!(repo.join("b.txt").is_file());
        assert_eq!(
            remove(repo, &task, Some("main"), None).unwrap(),
            "Removed try-it and its branch"
        );
        assert!(!dir.exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Runs git in `dir` with a test identity, asserting it worked.
    fn ok_in(dir: &Path, args: &[&str]) {
        let out = git(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A linked worktree of `repo` on a new branch `branch`, in its own
    /// temporary folder.
    fn linked(repo: &Path, branch: &str) -> (PathBuf, Worktree) {
        let dir = repo.with_extension(branch.replace('/', "-"));
        let _ = std::fs::remove_dir_all(&dir);
        ok_in(
            repo,
            &["worktree", "add", "-q", "-b", branch, dir.to_str().unwrap()],
        );
        let tree = Worktree {
            path: dir.clone(),
            branch: Some(branch.to_owned()),
            main: false,
        };
        (dir, tree)
    }

    #[test]
    fn merged_needs_own_commits() {
        let tree = fixture("merged-own", "main", &[]);
        let repo = &tree.0;
        let (fresh_dir, _) = linked(repo, "fresh");
        let (done_dir, _) = linked(repo, "done");
        std::fs::write(done_dir.join("b.txt"), "two\n").unwrap();
        ok_in(&done_dir, &["add", "b.txt"]);
        ok_in(&done_dir, &["commit", "-qm", "two"]);
        // main moves on: both branches are behind it, so both "merged".
        ok_in(repo, &["merge", "-q", "--ff-only", "done"]);
        std::fs::write(repo.join("c.txt"), "three\n").unwrap();
        ok_in(repo, &["add", "c.txt"]);
        ok_in(repo, &["commit", "-qm", "three"]);
        let merged = merged(repo, "main");
        assert!(merged.contains(&"fresh".to_owned()) && merged.contains(&"done".to_owned()));
        assert!(!has_own_commits(repo, "fresh"));
        assert!(has_own_commits(repo, "done"));
        assert!(!has_own_commits(repo, "no-such"));
        let _ = std::fs::remove_dir_all(&fresh_dir);
        let _ = std::fs::remove_dir_all(&done_dir);
    }

    #[test]
    fn remove_deletes_a_branch_merged_into_the_default_branch() {
        // The main checkout has another branch out, so plain -d, which
        // looks at its HEAD, would refuse.
        let tree = fixture("merged-default", "main", &["elsewhere"]);
        let repo = &tree.0;
        let (dir, task) = linked(repo, "fix/seed");
        std::fs::write(dir.join("b.txt"), "two\n").unwrap();
        ok_in(&dir, &["add", "b.txt"]);
        ok_in(&dir, &["commit", "-qm", "two"]);
        ok_in(repo, &["merge", "-q", "--ff-only", "fix/seed"]);
        ok_in(repo, &["switch", "-q", "elsewhere"]);
        assert_eq!(
            remove(repo, &task, Some("main"), None).unwrap(),
            "Removed fix/seed and its branch"
        );
        assert!(!dir.exists());
        assert!(run(repo, &["rev-parse", "--verify", "--quiet", "fix/seed"]).is_err());
    }

    #[test]
    fn squash_merged_branch_is_deleted_only_at_the_pr_head() {
        let tree = fixture("merged-squash", "main", &[]);
        let repo = &tree.0;
        let commit_in = |dir: &Path, name: &str| {
            std::fs::write(dir.join(name), "x\n").unwrap();
            ok_in(dir, &["add", name]);
            ok_in(dir, &["commit", "-qm", name]);
        };
        // At the head GitHub merged: deleted though Git sees it unmerged.
        let (dir, task) = linked(repo, "squashed");
        commit_in(&dir, "b.txt");
        let head = run(repo, &["rev-parse", "squashed"]).unwrap();
        assert_eq!(
            remove(repo, &task, Some("main"), Some(head.trim())).unwrap(),
            "Removed squashed and its branch (its pull request was merged)"
        );
        assert!(run(repo, &["rev-parse", "--verify", "--quiet", "squashed"]).is_err());
        // A commit after the merge: kept.
        let (dir, task) = linked(repo, "later");
        commit_in(&dir, "c.txt");
        let head = run(repo, &["rev-parse", "later"]).unwrap();
        commit_in(&dir, "d.txt");
        assert_eq!(
            remove(repo, &task, Some("main"), Some(head.trim())).unwrap(),
            "Removed later; the branch has commits its merged pull request does not, so it was kept"
        );
        assert!(run(repo, &["rev-parse", "--verify", "--quiet", "later"]).is_ok());
    }

    #[test]
    fn prunes_a_missing_worktree() {
        let tree = fixture("prune", "main", &[]);
        let repo = &tree.0;
        let (dir, _) = linked(repo, "gone");
        std::fs::remove_dir_all(&dir).unwrap();
        let (listed, prunable) = list_all(repo).unwrap();
        assert_eq!((listed.len(), prunable), (1, 1));
        assert_eq!(prune(repo).unwrap(), "Pruned 1 missing worktree");
        assert_eq!(list_all(repo).unwrap().1, 0);
    }
}
