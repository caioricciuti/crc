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
        // `echo` ends the line: a backslash before a quote would not read
        // the same in every login shell (see `remote::for_any_shell`).
        line.push_str(" 2>/dev/null; printf '\\n\\036%d' $?; echo");
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
    prune_on(&Runner::Local, main)
}

/// [`prune`], by `runner`.
pub fn prune_on(runner: &Runner, main: &Path) -> Result<String, String> {
    let before = list_all_on(runner, main)?.1;
    write_on(runner, main, &[strings(&["worktree", "prune"])])?;
    let pruned = before.saturating_sub(list_all_on(runner, main)?.1);
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

/// Where a new task's worktree goes on a host, for the repository whose
/// main checkout is `main` there: beside it, in `<main>-worktrees`, out of
/// its tree as Application Support's folder is here (a host has none),
/// and named for it, so two repositories never share one.
pub fn remote_home(main: &Path) -> PathBuf {
    let name = main
        .file_name()
        .map_or_else(|| "repo".into(), |n| n.to_string_lossy().into_owned());
    main.with_file_name(format!("{name}-worktrees"))
}

/// [`add`], by `runner`: on a host, in [`remote_home`], with the same new
/// branch from HEAD. Git makes the folders and refuses one that is there
/// with something in it.
pub fn add_on(runner: &Runner, main: &Path, name: &str) -> Result<PathBuf, String> {
    if *runner == Runner::Local {
        return add(main, name);
    }
    let branch = branch_name(name);
    if branch.is_empty() {
        return Err("Type a name with a letter or digit in it".into());
    }
    let dir = remote_home(main).join(branch.replace('/', "-"));
    let target = dir.to_string_lossy().into_owned();
    write_on(
        runner,
        main,
        &[strings(&[
            "worktree", "add", "-b", &branch, &target, "HEAD",
        ])],
    )?;
    Ok(dir)
}

/// Whether the checkout at `dir` has changes Git would lose on removal:
/// anything modified, staged or untracked.
pub fn is_dirty(dir: &Path) -> Result<bool, String> {
    is_dirty_on(&Runner::Local, dir)
}

/// [`is_dirty`], by `runner`.
pub fn is_dirty_on(runner: &Runner, dir: &Path) -> Result<bool, String> {
    Ok(!run_on(
        runner,
        dir,
        &["status", "--porcelain", "--untracked-files=normal"],
    )?
    .trim()
    .is_empty())
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
    remove_on(&Runner::Local, main, tree, base, merged_pr_head)
}

/// [`remove`], by `runner`: on a host, each check and change over ssh.
pub fn remove_on(
    runner: &Runner,
    main: &Path,
    tree: &Worktree,
    base: Option<&str>,
    merged_pr_head: Option<&str>,
) -> Result<String, String> {
    if tree.main {
        return Err("The repository's own checkout is not a task".into());
    }
    if is_dirty_on(runner, &tree.path)? {
        return Err(format!(
            "{} has uncommitted changes; commit or discard them first",
            tree.name()
        ));
    }
    let target = tree.path.to_string_lossy().into_owned();
    write_on(runner, main, &[strings(&["worktree", "remove", &target])])?;
    let Some(branch) = &tree.branch else {
        return Ok(format!("Removed {}", tree.name()));
    };
    let delete = || write_on(runner, main, &[strings(&["branch", "-D", branch])]);
    // Checked now, not from what the list last read: -D deletes whatever
    // the branch holds.
    let in_base = match base.filter(|b| *b != branch) {
        Some(b) => match run_on(runner, main, &["merge-base", "--is-ancestor", branch, b]) {
            Err(e) if e == UNREACHABLE => return Err(e),
            said => said.is_ok(),
        },
        None => false,
    };
    if in_base {
        delete()?;
        return Ok(format!("Removed {branch} and its branch"));
    }
    if let Some(head) = merged_pr_head.filter(|h| !h.is_empty()) {
        let tip = run_on(runner, main, &["rev-parse", "--verify", "--quiet", branch])
            .map(|t| t.trim().to_owned())
            .unwrap_or_default();
        if tip == head {
            delete()?;
            return Ok(format!(
                "Removed {branch} and its branch (its pull request was merged)"
            ));
        }
        return Ok(format!(
            "Removed {branch}; the branch has commits its merged pull request does not, so it was kept"
        ));
    }
    Ok(
        match write_on(runner, main, &[strings(&["branch", "-d", branch])]) {
            Ok(_) => format!("Removed {branch} and its branch"),
            Err(_) => format!("Removed {branch}; the branch has unmerged commits and was kept"),
        },
    )
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
    Ok(default_and_origin_on(runner, main, branches, current, false)?.0)
}

/// [`default_branch_on`], and with `origin`, in the same batch, the URL of
/// the `origin` remote (`None` without one).
pub fn default_and_origin_on(
    runner: &Runner,
    main: &Path,
    branches: &[Tip],
    current: Option<&str>,
    origin: bool,
) -> Result<(Option<String>, Option<String>), String> {
    let mut commands = vec![
        strings(&[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ]),
        strings(&["config", "--get", "init.defaultBranch"]),
    ];
    if origin {
        commands.push(strings(&["remote", "get-url", "origin"]));
    }
    let mut said = batch_on(runner, main, &commands)?
        .into_iter()
        .map(Result::ok);
    let (head, init) = (said.next().flatten(), said.next().flatten());
    let url = said
        .next()
        .flatten()
        .map(|u| u.trim().to_owned())
        .filter(|u| !u.is_empty());
    Ok((
        pick_default(head.as_deref(), init.as_deref(), branches, current),
        url,
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

/// Where `gh` finds a repository: its checkout on this Mac, or for one on
/// a host, its GitHub name (`owner/name`, from its origin; see
/// [`github_repo`]), `gh` still running here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitHub {
    Dir(PathBuf),
    Repo(String),
}

impl GitHub {
    /// `gh`, never asking anything, in the checkout or told the repository.
    /// The repository goes after the subcommand's own arguments.
    fn command(&self, args: &[&str]) -> Result<Command, String> {
        let gh = gh().ok_or("GitHub's gh is not installed")?;
        let mut cmd = Command::new(gh);
        cmd.args(args)
            .env("GH_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1")
            .stdin(std::process::Stdio::null());
        match self {
            GitHub::Dir(dir) => {
                cmd.current_dir(dir);
            }
            // Run away from any checkout, so none but the one named is
            // taken for it.
            GitHub::Repo(repo) => {
                cmd.args(["--repo", repo]).current_dir(std::env::temp_dir());
            }
        }
        Ok(cmd)
    }
}

/// The GitHub repository (`owner/name`) a remote's URL points at: the
/// https and ssh forms of github.com. `None` for any other host, which
/// `gh` is not asked about.
pub fn github_repo(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("ssh://"))
        .or_else(|| url.strip_prefix("git://"))
    {
        // `user@` and a port, as `ssh://git@github.com:22/o/r` has them.
        let (host, path) = rest.split_once('/')?;
        let host = host.rsplit('@').next()?;
        let host = host.split(':').next()?;
        (host.eq_ignore_ascii_case("github.com") || host.eq_ignore_ascii_case("www.github.com"))
            .then_some(path)?
    } else {
        // scp-like: `git@github.com:o/r.git`.
        let (host, path) = url.split_once(':')?;
        let host = host.rsplit('@').next()?;
        host.eq_ignore_ascii_case("github.com").then_some(path)?
    };
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, name) = rest.split_once('/')?;
    let fine = |part: &str| {
        !part.is_empty()
            && !part.starts_with('.')
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (fine(owner) && fine(name)).then(|| format!("{owner}/{name}"))
}

/// The pull request for `branch`, asked of `gh` in `dir`: `None` when the
/// branch has none; an error without `gh`, offline, or when `gh` failed,
/// so a row keeps what it last said.
pub fn pull_request(dir: &Path, branch: &str) -> Result<Option<PullRequest>, String> {
    pull_request_in(&GitHub::Dir(dir.to_path_buf()), branch)
}

/// [`pull_request`], of the repository `where_`.
pub fn pull_request_in(where_: &GitHub, branch: &str) -> Result<Option<PullRequest>, String> {
    let output = where_
        .command(&[
            "pr",
            "view",
            branch,
            "--json",
            "number,state,url,statusCheckRollup,title,author,commits,additions,deletions,\
             baseRefName,headRefName,headRefOid,isDraft,mergeable",
        ])?
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

/// What `git status --porcelain=v2 --branch -z` says of a checkout: its
/// branch (`None` detached), its HEAD commit (`None` before the first), its
/// upstream with the commits to push and to pull when that branch is there
/// to compare with, and how many files are touched, untracked ones too.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Porcelain {
    pub branch: Option<String>,
    pub head: Option<String>,
    pub upstream: Option<(String, usize, usize)>,
    pub touched: usize,
}

/// Reads [`Porcelain`] from what `git status --porcelain=v2 --branch -z`
/// printed: the `# branch.*` headers, then a record per file, a rename's
/// with its old path as the record after it.
pub fn parse_porcelain(text: &str) -> Porcelain {
    let mut out = Porcelain::default();
    let mut upstream = None;
    let mut records = text.split('\0').filter(|r| !r.is_empty());
    while let Some(record) = records.next() {
        if let Some(header) = record.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.oid" if value != "(initial)" => out.head = Some(value.to_owned()),
                "branch.head" if value != "(detached)" => out.branch = Some(value.to_owned()),
                "branch.upstream" => upstream = Some(value.to_owned()),
                "branch.ab" => {
                    let mut counts = value
                        .split_whitespace()
                        .map(|w| w.get(1..).and_then(|n| n.parse().ok()).unwrap_or(0));
                    let (push, pull) = (counts.next().unwrap_or(0), counts.next().unwrap_or(0));
                    out.upstream = upstream.take().map(|u| (u, push, pull));
                }
                _ => {}
            }
            continue;
        }
        match record.as_bytes().first() {
            Some(b'1' | b'u' | b'?') => out.touched += 1,
            Some(b'2') => {
                out.touched += 1;
                records.next();
            }
            _ => {}
        }
    }
    out
}

/// The newest tag reachable from HEAD and the commits since it, per
/// repository, with what it was read for: the HEAD commit and when the
/// tags last changed. A read with the same key is not asked of Git again.
type TagRead = (
    (String, Option<std::time::SystemTime>),
    Option<(String, usize)>,
);

static TAGS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<PathBuf, TagRead>>> =
    std::sync::LazyLock::new(Default::default);

/// When the tags of the repository checked out at `dir` last changed, as
/// far as its folders say: a tag made or deleted touches `refs/tags`, a
/// pack rewrites `packed-refs`. `None` when its Git directory is elsewhere.
fn tags_changed(dir: &Path) -> Option<std::time::SystemTime> {
    let git_dir = dir.join(".git");
    let at = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    [
        at(&git_dir.join("refs/tags")),
        at(&git_dir.join("packed-refs")),
    ]
    .into_iter()
    .flatten()
    .max()
}

/// The newest tag reachable from `head`, `dir`'s HEAD commit, and the
/// commits since it; read again only when HEAD moved or the tags changed.
fn latest_tag(dir: &Path, head: Option<&str>) -> Option<(String, usize)> {
    let key = (head?.to_owned(), tags_changed(dir));
    if let Some((had, tag)) = TAGS.lock().ok()?.get(dir)
        && *had == key
    {
        return tag.clone();
    }
    let tag = run(dir, &["describe", "--tags", "--long"])
        .ok()
        .and_then(|d| parse_describe(d.trim()));
    if let Ok(mut tags) = TAGS.lock() {
        tags.insert(dir.to_path_buf(), (key, tag.clone()));
    }
    tag
}

/// Reads `dir`'s status, comparing with `base` when it is another branch:
/// the branch, its upstream and what is uncommitted in one `git status`,
/// then the base's comparison, the uncommitted lines when there are any,
/// and the latest tag when HEAD or the tags moved since it was last read.
pub fn status(dir: &Path, base: Option<&str>, tags: bool) -> Result<Status, String> {
    let said = run(
        dir,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "--ahead-behind",
            "-z",
        ],
    )?;
    let read = parse_porcelain(&said);
    let mut out = Status {
        branch: read.branch.clone(),
        dirty: read.touched,
        ..Status::default()
    };
    if let Some(base) = base.filter(|b| read.branch.as_deref() != Some(*b)) {
        let range = format!("{base}...HEAD");
        if let Ok(counts) = run(dir, &["rev-list", "--left-right", "--count", &range]) {
            (out.behind, out.ahead) = parse_left_right(&counts);
            out.base = Some(base.to_owned());
            if let Ok(stat) = run(dir, &["diff", "--numstat", &range]) {
                (out.files, out.added, out.removed) = parse_numstat(&stat);
            }
        }
    }
    if let Some((upstream, push, pull)) = read.upstream {
        (out.upstream, out.to_push, out.to_pull) = (Some(upstream), push, pull);
    }
    if out.dirty > 0
        && let Ok(stat) = run(dir, &["diff", "HEAD", "--numstat"])
    {
        let (_, a, r) = parse_numstat(&stat);
        (out.dirty_added, out.dirty_removed) = (a, r);
    }
    if tags {
        out.tag = latest_tag(dir, read.head.as_deref());
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

/// `gh` on the repository `where_`, never asking anything: what it
/// printed, or its error.
fn run_gh(where_: &GitHub, args: &[&str]) -> Result<String, String> {
    let output = where_.command(args)?.output().map_err(|e| e.to_string())?;
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
    let url = run_gh(&GitHub::Dir(dir.to_path_buf()), &["pr", "create", "--fill"])?;
    Ok(opened(&url))
}

fn opened(url: &str) -> String {
    format!(
        "Opened {}",
        url.lines().last().unwrap_or("the pull request")
    )
}

/// A pull request's title and body from the commits `dir`'s branch has
/// that `base` does not, by `runner`, as `gh`'s `--fill` makes them: one
/// commit's subject and body, else the branch's name and a line per
/// commit. For a branch on a host, whose commits `gh` here cannot read.
pub fn pull_request_text_on(
    runner: &Runner,
    dir: &Path,
    base: Option<&str>,
    branch: &str,
) -> Result<(String, String), String> {
    let range = format!("{}..HEAD", base.unwrap_or("@{u}"));
    let mut said = batch_on(
        runner,
        dir,
        &[
            strings(&["log", "--reverse", "--format=%s", &range]),
            strings(&["log", "-1", "--format=%b"]),
        ],
    )?
    .into_iter();
    let subjects: Vec<String> = said
        .next()
        .and_then(Result::ok)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_owned)
        .collect();
    let body = said.next().and_then(Result::ok).unwrap_or_default();
    Ok(match subjects.as_slice() {
        [one] => (one.clone(), body.trim().to_owned()),
        many => (
            branch.replace(['-', '_'], " "),
            many.iter()
                .map(|s| format!("- {s}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    })
}

/// Opens a pull request on the GitHub repository `repo` for `branch`,
/// pushed there already, with `title` and `body`: no checkout here.
pub fn create_pull_request_in(
    repo: &str,
    branch: &str,
    title: &str,
    body: &str,
) -> Result<String, String> {
    let url = run_gh(
        &GitHub::Repo(repo.to_owned()),
        &[
            "pr", "create", "--head", branch, "--title", title, "--body", body,
        ],
    )?;
    Ok(opened(&url))
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
    merge_pull_request_in(&GitHub::Dir(dir.to_path_buf()), number, strategy)
}

/// [`merge_pull_request`], on the repository `where_`.
pub fn merge_pull_request_in(
    where_: &GitHub,
    number: u64,
    strategy: MergeStrategy,
) -> Result<String, String> {
    let n = number.to_string();
    run_gh(where_, &["pr", "merge", &n, strategy.flag()])?;
    Ok(format!("Merged pull request #{number}"))
}

/// How many open issues a repository's list reads; more is said as
/// "100+".
pub const ISSUE_LIMIT: usize = 100;
/// The most of an issue's body kept; GitHub has the rest.
const BODY_CAP: usize = 200 * 1024;
/// The most comments kept, and the most of their text in all.
const COMMENTS_CAP: usize = 50;
const COMMENTS_BYTES: usize = 1024 * 1024;
const CUT: &str = "(truncated, see GitHub)";

/// An issue on GitHub, as `gh` reports it. Read only: crc never writes
/// one. A list read leaves the body and comments empty; [`issue_in`]
/// reads them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    /// `OPEN` or `CLOSED`.
    pub state: String,
    pub url: String,
    /// The author's login.
    pub author: String,
    pub labels: Vec<String>,
    /// ISO 8601, as GitHub has them (see [`age`]).
    pub created_at: String,
    pub updated_at: String,
    pub body: String,
    pub comments: Vec<IssueComment>,
    /// Comments were left out, or one was cut short: GitHub has them all.
    pub comments_cut: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IssueComment {
    pub author: String,
    pub created_at: String,
    pub body: String,
}

/// Why a repository's issues could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IssueError {
    /// `gh` is not signed in to GitHub.
    NeedsAuth,
    /// The repository has no GitHub remote, or GitHub does not know it.
    NotGitHub,
    /// Anything else: what `gh` said last.
    Other(String),
}

impl IssueError {
    /// A line to show where the issues would be.
    pub fn message(&self) -> String {
        match self {
            IssueError::NeedsAuth => "Sign in to GitHub: run gh auth login in a terminal".into(),
            IssueError::NotGitHub => "Not a GitHub repository".into(),
            IssueError::Other(said) => said.clone(),
        }
    }
}

/// What a failed `gh` said, sorted: not signed in, not a GitHub
/// repository, or its last line.
pub fn issue_error(err: &str) -> IssueError {
    let low = err.to_ascii_lowercase();
    if low.contains("gh auth login")
        || low.contains("not logged in")
        || low.contains("not logged into")
        || low.contains("http 401")
        || low.contains("bad credentials")
    {
        IssueError::NeedsAuth
    } else if low.contains("no git remotes")
        || low.contains("could not resolve")
        || low.contains("point to a known github host")
        || low.contains("not a git repository")
        || low.contains("failed to determine base repo")
    {
        IssueError::NotGitHub
    } else {
        IssueError::Other(clean_line(err.trim().lines().last().unwrap_or("gh failed")))
    }
}

/// The open issues of the repository `where_`, newest first as `gh` lists
/// them, at most `limit`: as many means there may be more.
pub fn issues_in(where_: &GitHub, limit: usize) -> Result<Vec<Issue>, IssueError> {
    let limit = limit.to_string();
    let output = where_
        .command(&[
            "issue",
            "list",
            "--state",
            "open",
            "--limit",
            &limit,
            "--json",
            "number,title,labels,createdAt,updatedAt,author,url",
        ])
        .map_err(IssueError::Other)?
        .output()
        .map_err(|e| IssueError::Other(e.to_string()))?;
    issues_answer(
        output.status.success(),
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
    )
}

/// What `gh issue list` said.
pub fn issues_answer(ok: bool, out: &str, err: &str) -> Result<Vec<Issue>, IssueError> {
    if !ok {
        return Err(issue_error(err));
    }
    parse_issues(out).ok_or_else(|| IssueError::Other("gh said something unreadable".into()))
}

/// Issue `number` of the repository `where_`, with its body and comments.
pub fn issue_in(where_: &GitHub, number: u64) -> Result<Issue, IssueError> {
    let n = number.to_string();
    let output = where_
        .command(&[
            "issue",
            "view",
            &n,
            "--json",
            "number,title,state,url,author,labels,body,comments,createdAt,updatedAt",
        ])
        .map_err(IssueError::Other)?
        .output()
        .map_err(|e| IssueError::Other(e.to_string()))?;
    issue_answer(
        output.status.success(),
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
    )
}

/// What `gh issue view` said.
pub fn issue_answer(ok: bool, out: &str, err: &str) -> Result<Issue, IssueError> {
    if !ok {
        return Err(issue_error(err));
    }
    crate::json::parse(out)
        .ok()
        .as_ref()
        .and_then(read_issue)
        .ok_or_else(|| IssueError::Other("gh said something unreadable".into()))
}

/// `gh issue list`'s JSON: an array of issues.
pub fn parse_issues(text: &str) -> Option<Vec<Issue>> {
    let value = crate::json::parse(text).ok()?;
    Some(value.as_array()?.iter().filter_map(read_issue).collect())
}

/// One issue of `gh`'s JSON; its text cleaned of anything a terminal or
/// the screen would take for more than text, and cut to size.
fn read_issue(value: &crate::json::Value) -> Option<Issue> {
    let text = |v: &crate::json::Value, key: &str| {
        v.get(key).and_then(|w| w.as_str()).unwrap_or("").to_owned()
    };
    let login = |v: &crate::json::Value| {
        v.get("author")
            .map(|a| clean_line(&text(a, "login")))
            .unwrap_or_default()
    };
    let labels = value
        .get("labels")
        .and_then(|v| v.as_array())
        .unwrap_or(&[])
        .iter()
        .map(|l| clean_line(&text(l, "name")))
        .filter(|l| !l.is_empty())
        .collect();
    let mut comments = Vec::new();
    let mut comments_cut = false;
    let mut budget = COMMENTS_BYTES;
    let all = value
        .get("comments")
        .and_then(|v| v.as_array())
        .unwrap_or(&[]);
    for c in all {
        if comments.len() == COMMENTS_CAP || budget == 0 {
            comments_cut = true;
            break;
        }
        let mut body = clean_text(&text(c, "body"));
        if body.len() > budget {
            body.truncate(char_floor(&body, budget));
            comments_cut = true;
        }
        budget -= body.len();
        comments.push(IssueComment {
            author: login(c),
            created_at: clean_line(&text(c, "createdAt")),
            body,
        });
    }
    let mut body = clean_text(&text(value, "body"));
    if body.len() > BODY_CAP {
        body.truncate(char_floor(&body, BODY_CAP));
        body.push_str("\n\n");
        body.push_str(CUT);
    }
    let state = clean_line(&text(value, "state")).to_ascii_uppercase();
    Some(Issue {
        number: value.get("number")?.as_u64()?,
        title: clean_line(&text(value, "title")),
        // A list read asks for open ones only, and not their state.
        state: if state.is_empty() {
            "OPEN".into()
        } else {
            state
        },
        url: clean_line(&text(value, "url")),
        author: login(value),
        labels,
        created_at: clean_line(&text(value, "createdAt")),
        updated_at: clean_line(&text(value, "updatedAt")),
        body,
        comments,
        comments_cut,
    })
}

/// The largest index at most `at` that starts a character of `s`.
fn char_floor(s: &str, at: usize) -> usize {
    let mut at = at.min(s.len());
    while !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// `s` without terminal escapes (CSI, OSC and the two-byte ones), control
/// characters or the marks that reorder text on screen; `keep` says which
/// control characters stay.
fn strip(s: &str, keep: impl Fn(char) -> bool) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.next() {
                // CSI: parameters, then one final byte.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC and the other strings: to BEL or ESC \.
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let bidi = matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
        if bidi || (c.is_control() && !keep(c)) {
            continue;
        }
        out.push(c);
    }
    out
}

/// One line of untrusted text (a title, a label, a login): no control
/// characters at all, a line break or tab made a space.
pub(super) fn clean_line(s: &str) -> String {
    let spaced: String = s
        .chars()
        .map(|c| {
            if matches!(c, '\n' | '\r' | '\t') {
                ' '
            } else {
                c
            }
        })
        .collect();
    strip(&spaced, |_| false).trim().to_owned()
}

/// Untrusted text of many lines (a body, a comment): line breaks and tabs
/// stay, `\r\n` read as `\n`.
fn clean_text(s: &str) -> String {
    strip(&s.replace("\r\n", "\n"), |c| matches!(c, '\n' | '\t'))
}

/// The branch Start Task makes for issue `number`: `issue-42-fix-login`,
/// its title cut at a word near 40 characters; `issue-42` when the title
/// leaves nothing. One level: a `/` in the title is a word break.
pub fn issue_branch(number: u64, title: &str) -> String {
    let slug = branch_name(&title.replace('/', " "));
    let slug = if slug.chars().count() > 40 {
        let head: String = slug.chars().take(40).collect();
        match head.rfind('-') {
            Some(at) if at > 0 => head[..at].to_owned(),
            _ => head,
        }
    } else {
        slug
    };
    let slug = slug.trim_matches(|c| matches!(c, '-' | '.'));
    if slug.is_empty() {
        format!("issue-{number}")
    } else {
        format!("issue-{number}-{slug}")
    }
}

/// The read-only tab of an issue: its title, a line of what it is, its
/// description and its comments, as Markdown.
pub fn issue_markdown(issue: &Issue) -> String {
    let mut meta = vec![issue.state.clone()];
    if !issue.author.is_empty() {
        meta.push(format!("@{}", issue.author));
    }
    if !issue.labels.is_empty() {
        meta.push(issue.labels.join(", "));
    }
    if let Some(day) = day(&issue.created_at) {
        meta.push(format!("opened {day}"));
    }
    if !issue.url.is_empty() {
        meta.push(issue.url.clone());
    }
    let mut out = format!(
        "# #{} {}\n\n{}\n\n## Description\n\n",
        issue.number,
        issue.title,
        meta.join(" · ")
    );
    let body = issue.body.trim();
    out.push_str(if body.is_empty() {
        "No description."
    } else {
        body
    });
    out.push_str(&format!("\n\n## Comments ({})\n", issue.comments.len()));
    for c in &issue.comments {
        let who = if c.author.is_empty() {
            "someone"
        } else {
            &c.author
        };
        match day(&c.created_at) {
            Some(day) => out.push_str(&format!("\n### {who}, {day}\n\n")),
            None => out.push_str(&format!("\n### {who}\n\n")),
        }
        out.push_str(c.body.trim());
        out.push('\n');
    }
    if issue.comments_cut {
        out.push('\n');
        out.push_str(CUT);
        out.push('\n');
    }
    out
}

/// The date of an ISO 8601 time: `2026-10-12`.
fn day(iso: &str) -> Option<&str> {
    let day = iso.get(..10)?;
    unix_time(iso).is_some().then_some(day)
}

/// An ISO 8601 time (`2026-10-12T08:30:00Z`, a fraction or an offset
/// allowed) in Unix seconds; `None` when it is not one.
pub fn unix_time(iso: &str) -> Option<i64> {
    let iso = iso.trim();
    let num = |from: usize, to: usize| -> Option<i64> {
        let part = iso.get(from..to)?;
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    let b = iso.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, s) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    let mut rest = &iso[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        rest = frac.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let rest = rest[1..].replace(':', "");
            if rest.len() != 4 || !rest.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            sign * (rest[..2].parse::<i64>().ok()? * 3600 + rest[2..].parse::<i64>().ok()? * 60)
        }
    };
    // Days since 1970 from the civil date (Howard Hinnant's algorithm).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + s - offset)
}

/// How long ago the ISO 8601 time `iso` was, at `now` (Unix seconds), in
/// a few characters: `now`, `3m`, `2h`, `5d`, `3w`, `4mo`, `2y`. Empty
/// when `iso` is not a time.
pub fn age(iso: &str, now: u64) -> String {
    let Some(then) = unix_time(iso) else {
        return String::new();
    };
    let s = (now as i64 - then).max(0);
    let day = 86_400;
    match s {
        s if s < 60 => "now".into(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < day => format!("{}h", s / 3600),
        s if s < 7 * day => format!("{}d", s / day),
        s if s < 30 * day => format!("{}w", s / (7 * day)),
        s if s < 365 * day => format!("{}mo", s / (30 * day)),
        s => format!("{}y", s / (365 * day)),
    }
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
    fn reads_an_issue_list() {
        let list = issues_answer(
            true,
            r#"[{"number":42,"title":"Login fails on Safari","labels":[{"id":"x","name":"bug","color":"d73a4a"},{"name":"ui"}],"createdAt":"2026-10-01T08:00:00Z","updatedAt":"2026-10-12T09:30:00Z","author":{"login":"sam","is_bot":false},"url":"https://github.com/o/r/issues/42"},{"number":7,"title":"Docs","labels":[],"author":null,"url":"u"}]"#,
            "",
        )
        .unwrap();
        assert_eq!(list.len(), 2);
        let first = &list[0];
        assert_eq!(
            (
                first.number,
                first.title.as_str(),
                first.state.as_str(),
                first.author.as_str(),
                first.labels.clone(),
                first.updated_at.as_str(),
                first.url.as_str(),
            ),
            (
                42,
                "Login fails on Safari",
                "OPEN",
                "sam",
                vec!["bug".to_owned(), "ui".to_owned()],
                "2026-10-12T09:30:00Z",
                "https://github.com/o/r/issues/42",
            )
        );
        assert!(first.body.is_empty() && first.comments.is_empty());
        // No labels and no author (a deleted account) read as empty.
        assert_eq!((list[1].author.as_str(), list[1].labels.len()), ("", 0));
        assert_eq!(issues_answer(true, "[]", ""), Ok(Vec::new()));
        assert!(matches!(
            issues_answer(true, "not json", ""),
            Err(IssueError::Other(_))
        ));
    }

    #[test]
    fn reads_an_issue_with_its_comments() {
        let issue = issue_answer(
            true,
            r#"{"number":42,"title":"Login fails","state":"OPEN","url":"https://github.com/o/r/issues/42","author":{"login":"sam"},"labels":[{"name":"bug"}],"body":"Steps:\r\n1. open\n\t2. click","comments":[{"author":{"login":"kim"},"body":"Same here","createdAt":"2026-10-02T10:00:00Z"},{"author":{"login":"sam"},"body":"Fixed?","createdAt":"2026-10-03T10:00:00Z"}],"createdAt":"2026-10-01T08:00:00Z","updatedAt":"2026-10-03T10:00:00Z"}"#,
            "",
        )
        .unwrap();
        assert_eq!(issue.body, "Steps:\n1. open\n\t2. click");
        assert_eq!(
            issue.comments,
            vec![
                IssueComment {
                    author: "kim".into(),
                    created_at: "2026-10-02T10:00:00Z".into(),
                    body: "Same here".into(),
                },
                IssueComment {
                    author: "sam".into(),
                    created_at: "2026-10-03T10:00:00Z".into(),
                    body: "Fixed?".into(),
                },
            ]
        );
        assert!(!issue.comments_cut);
        let closed = issue_answer(true, r#"{"number":3,"state":"closed"}"#, "").unwrap();
        assert_eq!(
            (closed.state.as_str(), closed.title.as_str()),
            ("CLOSED", "")
        );
    }

    #[test]
    fn sorts_what_gh_said_when_issues_cannot_be_read() {
        let cases = [
            (
                "To get started with GitHub CLI, please run:  gh auth login\n",
                IssueError::NeedsAuth,
            ),
            (
                "You are not logged into any GitHub hosts. To log in, run: gh auth login",
                IssueError::NeedsAuth,
            ),
            (
                "HTTP 401: Bad credentials (https://api.github.com/graphql)",
                IssueError::NeedsAuth,
            ),
            ("no git remotes found", IssueError::NotGitHub),
            (
                "none of the git remotes configured for this repository point to a known GitHub host.",
                IssueError::NotGitHub,
            ),
            (
                "GraphQL: Could not resolve to a Repository with the name 'o/gone'. (repository)",
                IssueError::NotGitHub,
            ),
            (
                "warn\nerror connecting to api.github.com\n",
                IssueError::Other("error connecting to api.github.com".into()),
            ),
            (
                "the 'o/r' repository has disabled issues\u{1b}[31m!",
                IssueError::Other("the 'o/r' repository has disabled issues!".into()),
            ),
        ];
        for (said, want) in cases {
            assert_eq!(issue_error(said), want, "{said}");
            assert_eq!(issues_answer(false, "", said), Err(want.clone()));
            assert_eq!(issue_answer(false, "", said), Err(want));
        }
        assert_eq!(issue_error(""), IssueError::Other("gh failed".into()));
    }

    #[test]
    fn a_hostile_issue_is_only_text() {
        let title = "Fix\u{1b}[2J\u{1b}]0;owned\u{7} it\u{0}\nnow\u{202e}txt.exe";
        let body = format!(
            "line\u{1b}[31mred\u{1b}[0m\n\ttab\u{0}\u{7}\u{8}{}",
            "x".repeat(BODY_CAP + 10)
        );
        let json = crate::json::compact(&crate::json::object([
            ("number", crate::json::number(1)),
            ("title", crate::json::string(title)),
            ("body", crate::json::string(&body)),
            (
                "author",
                crate::json::object([("login", crate::json::string("ev\u{1b}[1mil\r"))]),
            ),
            (
                "labels",
                crate::json::Value::Array(vec![crate::json::object([(
                    "name",
                    crate::json::string("bu\u{9b}g\u{7f}"),
                )])]),
            ),
        ]));
        let issue = issue_answer(true, &json, "").unwrap();
        assert_eq!(issue.title, "Fix it nowtxt.exe");
        assert_eq!(issue.author, "evil");
        assert_eq!(issue.labels, vec!["bug".to_owned()]);
        assert!(
            issue.body.starts_with("linered\n\ttab"),
            "{:?}",
            &issue.body[..20]
        );
        assert!(issue.body.ends_with(CUT));
        assert!(issue.body.len() <= BODY_CAP + CUT.len() + 2);
        // Too many comments, and too much of them: cut, and said so.
        let comment = |i: usize, size: usize| {
            crate::json::object([
                (
                    "author",
                    crate::json::object([("login", crate::json::string("a"))]),
                ),
                (
                    "body",
                    crate::json::string(&format!("{i}{}", "é".repeat(size))),
                ),
            ])
        };
        let many = |n: usize, size: usize| {
            crate::json::compact(&crate::json::object([
                ("number", crate::json::number(2)),
                (
                    "comments",
                    crate::json::Value::Array((0..n).map(|i| comment(i, size)).collect()),
                ),
            ]))
        };
        let issue = issue_answer(true, &many(80, 10), "").unwrap();
        assert_eq!(
            (issue.comments.len(), issue.comments_cut),
            (COMMENTS_CAP, true)
        );
        let issue = issue_answer(true, &many(40, 15_000), "").unwrap();
        let total: usize = issue.comments.iter().map(|c| c.body.len()).sum();
        assert!(issue.comments_cut && total <= COMMENTS_BYTES, "{total}");
        assert!(issue.comments.len() < 40);
        assert!(issue_markdown(&issue).trim_end().ends_with(CUT));
    }

    #[test]
    fn an_issue_names_its_branch() {
        assert_eq!(
            issue_branch(42, "Login fails on Safari"),
            "issue-42-login-fails-on-safari"
        );
        assert_eq!(issue_branch(7, ""), "issue-7");
        assert_eq!(issue_branch(7, "!!! ??? ..."), "issue-7");
        // Cut at a word, near 40 characters.
        let long = issue_branch(
            9,
            "The sidebar flickers when a workspace with many sessions is selected",
        );
        assert_eq!(long, "issue-9-the-sidebar-flickers-when-a-workspace");
        // One word longer than the cut: cut inside it.
        let word = issue_branch(1, &"a".repeat(60));
        assert_eq!(word, format!("issue-1-{}", "a".repeat(40)));
        assert_eq!(
            issue_branch(3, "Crash in src/app.rs: fix/it"),
            "issue-3-crash-in-src-app.rs-fix-it"
        );
        assert_eq!(
            issue_branch(5, "Écran noir à l'ouverture"),
            "issue-5-écran-noir-à-l-ouverture"
        );
        assert_eq!(issue_branch(6, "..hidden.."), "issue-6-hidden");
    }

    #[test]
    fn an_issue_reads_as_markdown() {
        let mut issue = Issue {
            number: 42,
            title: "Login fails".into(),
            state: "OPEN".into(),
            url: "https://github.com/o/r/issues/42".into(),
            author: "sam".into(),
            labels: vec!["bug".into(), "ui".into()],
            created_at: "2026-10-01T08:00:00Z".into(),
            updated_at: String::new(),
            body: "It fails.\n".into(),
            comments: vec![IssueComment {
                author: "kim".into(),
                created_at: "2026-10-02T10:00:00Z".into(),
                body: "Same here".into(),
            }],
            comments_cut: false,
        };
        assert_eq!(
            issue_markdown(&issue),
            "# #42 Login fails\n\n\
             OPEN · @sam · bug, ui · opened 2026-10-01 · https://github.com/o/r/issues/42\n\n\
             ## Description\n\nIt fails.\n\n\
             ## Comments (1)\n\n### kim, 2026-10-02\n\nSame here\n"
        );
        issue.body = "  ".into();
        issue.comments.clear();
        issue.labels.clear();
        issue.author.clear();
        assert_eq!(
            issue_markdown(&issue),
            "# #42 Login fails\n\n\
             OPEN · opened 2026-10-01 · https://github.com/o/r/issues/42\n\n\
             ## Description\n\nNo description.\n\n## Comments (0)\n"
        );
    }

    #[test]
    fn says_how_old_a_time_is() {
        let at = unix_time("2026-10-13T12:00:00Z").unwrap() as u64;
        assert_eq!(unix_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_time("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(unix_time("2026-10-13T14:00:00.123+02:00"), Some(at as i64));
        let cases = [
            ("2026-10-13T11:59:30Z", "now"),
            ("2026-10-13T11:57:00Z", "3m"),
            ("2026-10-13T10:00:00Z", "2h"),
            ("2026-10-08T12:00:00Z", "5d"),
            ("2026-09-22T12:00:00Z", "3w"),
            ("2026-06-10T12:00:00Z", "4mo"),
            ("2024-10-01T12:00:00Z", "2y"),
            // A clock behind GitHub's says now, not a negative age.
            ("2026-10-13T12:05:00Z", "now"),
            ("yesterday", ""),
            ("2026-13-01T00:00:00Z", ""),
        ];
        for (iso, want) in cases {
            assert_eq!(age(iso, at), want, "{iso}");
        }
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
             printf '\\n\\036%d' $?; echo; \
             git 'log' 'it'\"'\"'s $HOME' 2>/dev/null; printf '\\n\\036%d' $?; echo"
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

    /// [`status`] as it was before it read `git status --porcelain=v2`:
    /// a Git command per fact, the latest tag read every time.
    fn status_one_by_one(dir: &Path, base: Option<&str>, tags: bool) -> Result<Status, String> {
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

    #[test]
    fn status_in_one_read_says_what_a_command_per_fact_said() {
        let tree = fixture("status-porcelain", "main", &["old"]);
        let repo = &tree.0;
        let commit = |message: &str| ok_in(repo, &["commit", "-qm", message]);
        let same = |what: &str| {
            for base in [None, Some("old"), Some("main"), Some("nowhere")] {
                for tags in [false, true] {
                    assert_eq!(
                        status(repo, base, tags),
                        status_one_by_one(repo, base, tags),
                        "{what}, base {base:?}, tags {tags}"
                    );
                }
            }
        };
        // Clean, no upstream, no tag.
        same("clean");
        // An upstream on a remote that is never fetched: its branch set by
        // hand, one commit behind HEAD, then ahead of it too.
        ok_in(repo, &["remote", "add", "origin", "/nowhere/garden.git"]);
        ok_in(repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        ok_in(repo, &["branch", "-q", "--set-upstream-to=origin/main"]);
        same("level with its upstream");
        std::fs::write(repo.join("b.txt"), "two\nthree\n").unwrap();
        std::fs::write(repo.join("d.txt"), "four\n").unwrap();
        ok_in(repo, &["add", "b.txt", "d.txt"]);
        commit("two");
        same("ahead of its upstream");
        ok_in(repo, &["checkout", "-q", "-b", "elsewhere", "old"]);
        std::fs::write(repo.join("c.txt"), "seed\n").unwrap();
        ok_in(repo, &["add", "c.txt"]);
        commit("seed");
        let theirs = run(repo, &["rev-parse", "HEAD"]).unwrap();
        ok_in(repo, &["checkout", "-q", "main"]);
        ok_in(
            repo,
            &["update-ref", "refs/remotes/origin/main", theirs.trim()],
        );
        same("ahead and behind");
        let read = status(repo, None, false).unwrap();
        assert_eq!(
            (read.upstream.as_deref(), read.to_push, read.to_pull),
            (Some("origin/main"), 1, 1)
        );
        // Tags: one two commits back, then one on HEAD made after a read.
        ok_in(repo, &["-c", "tag.gpgsign=false", "tag", "v0.1-rc"]);
        commit_empty(repo, "three");
        commit_empty(repo, "four");
        same("a tag two commits back");
        ok_in(
            repo,
            &["-c", "tag.gpgsign=false", "tag", "-a", "v0.2", "-m", "v0.2"],
        );
        same("a tag made on HEAD");
        assert_eq!(
            status(repo, None, true).unwrap().tag,
            Some(("v0.2".to_owned(), 0))
        );
        // Uncommitted: staged, unstaged, untracked, a rename and a
        // deletion.
        std::fs::write(repo.join("a.txt"), "one\nchanged\n").unwrap();
        std::fs::write(repo.join("staged.txt"), "staged\n").unwrap();
        ok_in(repo, &["add", "staged.txt"]);
        std::fs::write(repo.join("new notes.txt"), "untracked\n").unwrap();
        std::fs::create_dir_all(repo.join("sketches")).unwrap();
        std::fs::write(repo.join("sketches/kite.txt"), "kite\n").unwrap();
        ok_in(repo, &["mv", "b.txt", "moved.txt"]);
        ok_in(repo, &["rm", "-q", "d.txt"]);
        same("dirty");
        assert_eq!(status(repo, None, false).unwrap().dirty, 6);
        ok_in(repo, &["add", "-A"]);
        commit("five");
        // An upstream whose branch is gone from the remote.
        ok_in(repo, &["config", "branch.main.merge", "refs/heads/gone"]);
        same("upstream gone");
        assert_eq!(status(repo, None, false).unwrap().upstream, None);
        ok_in(repo, &["branch", "-q", "--unset-upstream"]);
        same("no upstream");
        // Detached HEAD, clean and then dirty.
        ok_in(repo, &["checkout", "-q", "--detach", "HEAD~1"]);
        same("detached");
        std::fs::write(repo.join("a.txt"), "detached\n").unwrap();
        same("detached and dirty");
        // A folder that is not a repository.
        let away = repo.with_extension("not-a-repo");
        std::fs::create_dir_all(&away).unwrap();
        assert_eq!(
            status(&away, None, true).is_err(),
            status_one_by_one(&away, None, true).is_err()
        );
        let _ = std::fs::remove_dir_all(&away);
    }

    fn commit_empty(repo: &Path, message: &str) {
        ok_in(repo, &["commit", "-q", "--allow-empty", "-m", message]);
    }

    #[test]
    fn reads_porcelain_v2_records() {
        let said = "# branch.oid 1234\0# branch.head feat/kite\0\
                    # branch.upstream origin/feat/kite\0# branch.ab +2 -3\0\
                    1 .M N... 100644 100644 100644 aa bb a.txt\0\
                    2 R. N... 100644 100644 100644 aa bb R100 new.txt\0old.txt\0\
                    ? notes/\0";
        assert_eq!(
            parse_porcelain(said),
            Porcelain {
                branch: Some("feat/kite".into()),
                head: Some("1234".into()),
                upstream: Some(("origin/feat/kite".into(), 2, 3)),
                touched: 3,
            }
        );
        // Detached, before the first commit, an upstream gone.
        let said = "# branch.oid (initial)\0# branch.head (detached)\0\
                    # branch.upstream origin/gone\0";
        assert_eq!(parse_porcelain(said), Porcelain::default());
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
             git 'add' '--all' && git 'commit' '-m' 'it'\"'\"'s $HOME\nmore'"
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
    fn a_remote_names_its_github_repository() {
        for url in [
            "https://github.com/me/garden-log.git",
            "https://github.com/me/garden-log",
            "https://token@github.com/me/garden-log/",
            "git@github.com:me/garden-log.git",
            "ssh://git@github.com/me/garden-log.git",
            "ssh://git@github.com:22/me/garden-log",
            "git://github.com/me/garden-log.git",
            " https://www.github.com/me/garden-log \n",
        ] {
            assert_eq!(github_repo(url).as_deref(), Some("me/garden-log"), "{url}");
        }
        for url in [
            "https://gitlab.com/me/garden-log.git",
            "git@example.com:me/garden-log.git",
            "/srv/git/garden-log.git",
            "../origin.git",
            "https://github.com/me",
            "https://github.com/me/x/y",
            "https://github.com/me/$(x)",
            "",
        ] {
            assert_eq!(github_repo(url), None, "{url}");
        }
    }

    #[test]
    fn a_task_is_made_committed_to_removed_and_pruned_on_a_host() {
        // The host is this Mac, its login shell csh: a stand-in ssh runs
        // what it is given with `/bin/csh -c`, as ssh has the login shell
        // do. Every step goes through it, the commit's message with a line
        // break and a `!` in it.
        if !Path::new("/bin/csh").exists() {
            return;
        }
        let tree = fixture("remote-tasks", "main", &[]);
        let repo = &tree.0;
        ok_in(repo, &["config", "user.name", "t"]);
        ok_in(repo, &["config", "user.email", "t@t"]);
        let stub = repo.with_extension("ssh");
        std::fs::write(
            &stub,
            "#!/bin/sh\nfor last; do :; done\nexec /bin/csh -c \"$last\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        // SAFETY: tests in this module are the only readers of the variable.
        unsafe { std::env::set_var("CRC_SSH_COMMAND", &stub) };
        let runner = Runner::Ssh {
            host: "box".into(),
            control: None,
        };
        let home = remote_home(repo);
        assert_eq!(
            home.file_name().map(|n| n.to_string_lossy().into_owned()),
            Some(format!(
                "{}-worktrees",
                repo.file_name().unwrap().to_string_lossy()
            ))
        );
        let dir = add_on(&runner, repo, "Try It").unwrap();
        assert_eq!(dir, home.join("try-it"));
        assert!(dir.join("a.txt").is_file());
        assert_eq!(list_all_on(&runner, repo).unwrap().0.len(), 2);
        // Made again: Git refuses, and says why.
        let again = add_on(&runner, repo, "Try It").unwrap_err();
        assert!(again.contains("already exists"), "{again}");

        std::fs::write(dir.join("b.txt"), "two\n").unwrap();
        assert_eq!(is_dirty_on(&runner, &dir), Ok(true));
        let task = Worktree {
            path: dir.clone(),
            branch: Some("try-it".into()),
            main: false,
        };
        assert!(remove_on(&runner, repo, &task, Some("main"), None).is_err());
        assert!(dir.is_dir());
        assert_eq!(
            commit_all_on(&runner, &dir, "two\nlines! and \\ a backslash"),
            Ok("Committed 1 file".to_owned())
        );
        let log = run(&dir, &["log", "-1", "--format=%B"]).unwrap();
        assert_eq!(log.trim(), "two\nlines! and \\ a backslash");
        ok_in(repo, &["merge", "-q", "--ff-only", "try-it"]);
        assert_eq!(
            remove_on(&runner, repo, &task, Some("main"), None),
            Ok("Removed try-it and its branch".to_owned())
        );
        assert!(!dir.exists());
        assert!(run(repo, &["rev-parse", "--verify", "--quiet", "try-it"]).is_err());

        // One whose folder went: pruned there.
        let gone = add_on(&runner, repo, "gone").unwrap();
        std::fs::remove_dir_all(&gone).unwrap();
        assert_eq!(list_all_on(&runner, repo).unwrap().1, 1);
        assert_eq!(
            prune_on(&runner, repo),
            Ok("Pruned 1 missing worktree".to_owned())
        );
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_file(&stub);
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
