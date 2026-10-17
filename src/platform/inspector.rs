//! The workspace inspector: a column at the right of Agents mode that says
//! where each repository of the selected workspace stands (its branch
//! against main and its upstream, its uncommitted work), with the buttons to
//! commit, pull, push and take main's new commits; for a worktree, its pull
//! request with each check, and the buttons to open or merge it. A folder
//! workspace shows every repository in it: the folder itself and its direct
//! children, each with a strip per worktree of it: where its branch stands
//! against the base, and buttons to open it or start a shell in it. Under
//! each repository's card, its open issues on GitHub, once you load them;
//! a repository's name row folds its card away. The Git status is
//! read on a worker; the pull request comes with the workspace list's own
//! read, the issues with the panel's.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::platform::agents::{IssueSet, Pace, RepoKey, Workspace};
use crate::project::icons;
use crate::project::worktree::{CheckState, PullRequest, Status};
use crate::render::{
    font::Atlas,
    layout::{self, Theme, Viewport},
    metal::GlyphInstance,
};

/// Its width, when the window has room for it beside the terminal.
pub const WIDTH: f32 = 300.0;
/// The terminal keeps at least this much beside it.
pub const TERMINAL_MIN: f32 = 420.0;
const PAD: f32 = 12.0;
pub const ROW: f32 = 26.0;
/// A status read this old is read again when the panel is drawn. Sooner
/// when something it shows was written to (see [`Inspector::touch`]).
const STALE: Duration = Duration::from_secs(60);
/// A status read is at least this old before a write to what it shows
/// reads it again: an agent writing all the time reads it every few
/// seconds, not after every file.
const TOUCHED_WAIT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Close,
    Refresh,
    CommitAll,
    Pull,
    Push,
    /// Merge the base branch into the workspace's.
    UpdateFromBase,
    CreatePullRequest,
    MergePullRequest,
    OpenPullRequest,
    /// A worktree strip's branch name: that workspace, selected.
    OpenWorktree,
    /// A worktree strip's Terminal: a new shell in it.
    WorktreeTerminal,
    /// A worktree strip's Review: its branch against the base, on the
    /// Review page.
    ReviewWorktree,
    /// A merged worktree strip's Clean up: the worktree and its branch
    /// removed.
    CleanUpWorktree,
    /// An issue row: that issue, read whole.
    OpenIssue(u64),
    /// Under a repository's issues: all of them, or the first few again.
    ToggleIssues,
    /// Load Issues, where a repository's issues go until you ask for them.
    LoadIssues,
    /// Beside a repository's issues: hidden again, no longer read.
    HideIssues,
    /// A repository's name row: its card folded to that row, or opened.
    FoldRepo,
    /// Check: a fetch from the repository's remote, so ahead and behind
    /// say what the remote has now.
    CheckRemote,
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Action::Close => "close",
            Action::Refresh => "refresh",
            Action::CommitAll => "commit",
            Action::Pull => "pull",
            Action::Push => "push",
            Action::UpdateFromBase => "update",
            Action::CreatePullRequest => "create_pr",
            Action::MergePullRequest => "merge",
            Action::OpenPullRequest => "open",
            Action::OpenWorktree => "worktree_open",
            Action::WorktreeTerminal => "worktree_terminal",
            Action::ReviewWorktree => "worktree_review",
            Action::CleanUpWorktree => "worktree_cleanup",
            Action::OpenIssue(_) => "issue_open",
            Action::ToggleIssues => "issues_toggle",
            Action::LoadIssues => "issues_load",
            Action::HideIssues => "issues_hide",
            Action::FoldRepo => "repo_fold",
            Action::CheckRemote => "check",
        }
    }
}

/// How many issues a repository shows until Show all.
const ISSUES_SHOWN: usize = 10;
/// The least an issue's title keeps beside its labels: they go first.
const TITLE_ROOM: f32 = 140.0;

/// What the issue lists are drawn from: what was read per repository,
/// whether `gh` is asked at all (a list not read yet is on its way), the
/// repositories whose issues you loaded (the others have Load Issues), and
/// now, in Unix seconds, for their ages.
#[derive(Clone, Copy)]
pub struct Issues<'a> {
    pub read: &'a HashMap<RepoKey, IssueSet>,
    pub asked: bool,
    pub loaded: &'a HashSet<RepoKey>,
    pub now: u64,
}

type Read = (PathBuf, Result<Status, String>);

/// A repository to read: its folder, the branch to compare with, and
/// whether to look up its latest tag.
pub type Want = (PathBuf, Option<String>, bool);

/// A card row: its label, and its values with their colours.
type Fact<'a> = (&'a str, Vec<(String, [f32; 4])>);

/// A worktree strip's height: its branch, where it stands, and its last
/// commit.
const STRIP: f32 = 64.0;

/// A repository's Check: a fetch from its remote. The status compares
/// HEAD with the remote's refs as last fetched, so until one is checked
/// "up to date" only says nothing came in by the last fetch.
pub enum Check {
    /// The fetch, on a worker of its own: each repository's runs beside
    /// the others', and beside the panel's one operation.
    Running(mpsc::Receiver<Result<String, String>>),
    /// When it answered, in Unix seconds.
    Done(u64),
    /// What Git said.
    Failed(String),
}

/// How a card row's value is inked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ink {
    Text,
    Faint,
    Removed,
}

impl Ink {
    fn colour(self, theme: &Theme) -> [f32; 4] {
        match self {
            Ink::Text => theme.text,
            Ink::Faint => theme.gutter_text,
            Ink::Removed => theme.diff_removed,
        }
    }
}

/// The Remote row's values: where the branch stands against its upstream
/// as last fetched, then, faintly, when that was checked (`now` in Unix
/// seconds, for the clock).
fn remote_values(status: &Status, check: Option<&Check>, now: u64) -> Vec<(String, Ink)> {
    if status.upstream.is_none() {
        return vec![("not pushed".into(), Ink::Faint)];
    }
    let mut parts = Vec::new();
    if status.to_push > 0 {
        parts.push(format!("{} to push", status.to_push));
    }
    if status.to_pull > 0 {
        parts.push(format!("{} to pull", status.to_pull));
    }
    let counts = if parts.is_empty() {
        "up to date".to_owned()
    } else {
        parts.join(", ")
    };
    let checked = match check {
        None => ("not checked".to_owned(), Ink::Faint),
        Some(Check::Running(_)) => ("checking\u{2026}".to_owned(), Ink::Faint),
        Some(Check::Done(at)) => (
            format!(
                "checked {}",
                crate::platform::clock_time(*at as i64, now as i64)
            ),
            Ink::Faint,
        ),
        Some(Check::Failed(_)) => ("check failed".to_owned(), Ink::Removed),
    };
    vec![(counts, Ink::Text), checked]
}

#[derive(Default)]
pub struct Inspector {
    /// Closed by its button or View > Workspace Inspector; open otherwise.
    pub hidden: bool,
    /// The last statuses read, per repository.
    pub status: Vec<Read>,
    /// The repositories the statuses are for, in order, and the host they
    /// are on (`None` for this Mac).
    read_for: Vec<PathBuf>,
    read_host: Option<String>,
    read_at: Option<Instant>,
    /// Something the statuses show was written to since they were read.
    touched: bool,
    rx: Option<mpsc::Receiver<(Option<String>, Vec<Read>)>>,
    /// The repositories being read, and their host.
    reading: Option<(Option<String>, Vec<PathBuf>)>,
    /// Each button, with the repository it acts on.
    pub hits: Vec<(Viewport, Action, Option<PathBuf>)>,
    /// How far the body is scrolled, and how far it can be, in points.
    pub scroll: f32,
    max_scroll: f32,
    /// The ssh session shown last frame, for the self-test.
    shown_remote: Option<String>,
    /// The remote workspace shown last frame, `host:path`, for the
    /// self-test.
    shown_host: Option<String>,
    /// The worktree strips shown last frame, per repository, and whether
    /// each was merged, for the self-test.
    shown_worktrees: Vec<(PathBuf, PathBuf, String, bool)>,
    /// The repositories (main checkouts) whose issues all show, past the
    /// first few.
    pub issues_open: HashSet<PathBuf>,
    /// What each repository card's issues said last frame, for the
    /// self-test.
    shown_issues: Vec<(PathBuf, String)>,
    /// The folder workspace shown is on a host that did not answer, so its
    /// repositories are not known: set before each draw. Its body says so
    /// instead of waiting.
    pub unreachable: bool,
    /// Per repository, by host and folder, its branch and uncommitted
    /// file count when last read: a folded card shows them, unread.
    known: HashMap<RepoKey, (Option<String>, usize)>,
    /// Per repository, by host and folder, how many times its status was
    /// asked for this run, for the self-test.
    reads: HashMap<RepoKey, usize>,
    /// The repositories whose cards were folded last frame, for the
    /// self-test.
    shown_folded: Vec<PathBuf>,
    /// Per repository, by host and folder, its Check, once asked for.
    checks: HashMap<RepoKey, Check>,
    /// A Check answered while a status read ran: what that read brings
    /// may be from before the fetch, so it is read again after.
    reread: bool,
    /// The Checks the header's refresh started, while any of them runs.
    checks_all: Vec<RepoKey>,
}

impl Inspector {
    /// Reads each repository's status on a worker, unless fresh ones are in
    /// hand or on their way: on `host` over ssh, a call per repository, or
    /// on this Mac for `None`.
    pub fn want(&mut self, host: Option<&str>, wanted: &[Want]) {
        let paths: Vec<PathBuf> = wanted.iter().map(|w| w.0.clone()).collect();
        let host = host.map(str::to_owned);
        let same = self.read_for == paths && self.read_host == host;
        let fresh = if self.touched { TOUCHED_WAIT } else { STALE };
        let have = same && self.read_at.is_some_and(|at| at.elapsed() < fresh);
        let key = (host.clone(), paths.clone());
        if have || self.reading.as_ref() == Some(&key) {
            return;
        }
        self.touched = false;
        // Another machine's paths are other folders, even named the same.
        if self.read_host != host {
            self.status.clear();
            self.read_for.clear();
            self.read_host.clone_from(&host);
        }
        for path in &paths {
            *self.reads.entry((host.clone(), path.clone())).or_default() += 1;
        }
        // A card folded or opened keeps the place you scrolled to; another
        // workspace starts at the top.
        if !paths.iter().any(|p| self.read_for.contains(p)) {
            self.scroll = 0.0;
        }
        let (tx, rx) = mpsc::channel();
        let wanted = wanted.to_vec();
        std::thread::spawn(move || {
            let runner = crate::platform::agents::runner(host.as_deref());
            let read = wanted
                .into_iter()
                .map(|(dir, base, tags)| {
                    let status =
                        crate::project::worktree::status_on(&runner, &dir, base.as_deref(), tags);
                    (dir, status)
                })
                .collect();
            let _ = tx.send((host, read));
        });
        self.rx = Some(rx);
        self.reading = Some(key);
    }

    /// The next read reads again, now.
    pub fn invalidate(&mut self) {
        self.read_at = None;
    }

    /// A folder whose status is shown was written to, or its repository's
    /// refs moved: it is read again once the last read is
    /// [`TOUCHED_WAIT`] old. Returns whether `path` is shown.
    pub fn touch(&mut self, path: &Path) -> bool {
        let shown = self.read_for.iter().any(|p| p == path);
        self.touched |= shown;
        shown
    }

    /// Every status shown is read again soon, as if written to: crc is back
    /// in front after a while behind other apps.
    pub fn touch_all(&mut self) {
        self.touched = true;
    }

    /// Check: fetches `repo` (on its host, over ssh, for a host's) on a
    /// worker, then its status is read again. Nothing can prompt: the
    /// fetch has no terminal, and a missing credential fails. A Check of
    /// a repository already checking is not started again.
    pub fn check_remote(&mut self, repo: RepoKey, ssh_auth_sock: Option<PathBuf>) {
        use crate::project::git::Remote;
        if self.checking(&repo) {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let (host, dir) = repo.clone();
        std::thread::spawn(move || {
            let answer = match host {
                Some(host) => {
                    let runner = crate::platform::agents::runner(Some(&host));
                    crate::project::worktree::remote_on(&runner, &dir, Remote::Fetch)
                }
                None => crate::project::git::remote(&dir, Remote::Fetch, ssh_auth_sock.as_deref()),
            };
            let _ = tx.send(answer);
        });
        self.checks.insert(repo, Check::Running(rx));
    }

    /// The header's refresh: a Check, each on its own worker, of every
    /// one of `repos` (on `host`) that has an upstream and is not checking
    /// already. A repository not read yet, or read for another host, has
    /// no upstream known and is left. Returns how many were started.
    pub fn check_all(
        &mut self,
        host: Option<&str>,
        repos: &[PathBuf],
        ssh_auth_sock: Option<PathBuf>,
    ) -> usize {
        if self.read_host.as_deref() != host {
            return 0;
        }
        let due: Vec<RepoKey> = repos
            .iter()
            .filter(|path| matches!(self.status_of(path), Some(Ok(s)) if s.upstream.is_some()))
            .map(|path| (self.read_host.clone(), path.clone()))
            .filter(|key| !self.checking(key))
            .collect();
        for key in &due {
            self.check_remote(key.clone(), ssh_auth_sock.clone());
        }
        self.checks_all.retain(|key| {
            self.checks
                .get(key)
                .is_some_and(|c| matches!(c, Check::Running(_)))
        });
        self.checks_all.extend(due.iter().cloned());
        due.len()
    }

    /// Whether a Check the header's refresh started is running.
    pub fn checking_all(&self) -> bool {
        self.checks_all.iter().any(|key| self.checking(key))
    }

    /// Whether `repo`'s Check is running.
    pub fn checking(&self, repo: &RepoKey) -> bool {
        matches!(self.checks.get(repo), Some(Check::Running(_)))
    }

    /// Whether any repository's Check is running.
    pub fn checking_any(&self) -> bool {
        self.checks.values().any(|c| matches!(c, Check::Running(_)))
    }

    /// `repo`'s Check, once asked for.
    pub fn check_of(&self, repo: &RepoKey) -> Option<&Check> {
        self.checks.get(repo)
    }

    /// Takes the Checks that answered: the statuses are read again, as
    /// the remote's refs moved. Returns whether any did.
    fn poll_checks(&mut self) -> bool {
        let mut answered = false;
        for check in self.checks.values_mut() {
            let Check::Running(rx) = check else {
                continue;
            };
            let answer = match rx.try_recv() {
                Ok(answer) => answer,
                Err(mpsc::TryRecvError::Empty) => continue,
                Err(mpsc::TryRecvError::Disconnected) => Err("the fetch stopped".into()),
            };
            *check = match answer {
                Ok(_) => Check::Done(crate::platform::unix_seconds()),
                Err(e) => Check::Failed(e),
            };
            answered = true;
        }
        if answered {
            self.invalidate();
            self.reread |= self.rx.is_some();
            if !self.checking_all() {
                self.checks_all.clear();
            }
        }
        answered
    }

    /// Takes a finished read or Check. Returns whether anything shown
    /// changed: a read that says what the last one did is taken without a
    /// redraw.
    pub fn poll(&mut self) -> bool {
        let checked = self.poll_checks();
        checked | self.poll_read()
    }

    fn poll_read(&mut self) -> bool {
        let Some(rx) = &self.rx else {
            return false;
        };
        let mut changed = true;
        match rx.try_recv() {
            Ok((host, read)) => {
                changed = host != self.read_host || read != self.status;
                // A read for a workspace no longer selected still counts:
                // the next `want` reads the one that is.
                self.read_for = read.iter().map(|(p, _)| p.clone()).collect();
                for (path, status) in &read {
                    if let Ok(s) = status {
                        self.known
                            .insert((host.clone(), path.clone()), (s.branch.clone(), s.dirty));
                    }
                }
                self.read_host = host;
                self.status = read;
                // Begun before a Check answered: read again.
                self.read_at = (!std::mem::take(&mut self.reread)).then(Instant::now);
            }
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => {}
        }
        self.rx = None;
        self.reading = None;
        changed
    }

    pub fn busy(&self) -> bool {
        self.rx.is_some()
    }

    /// The status of `path`, once read.
    pub fn status_of(&self, path: &Path) -> Option<&Result<Status, String>> {
        self.status.iter().find(|(p, _)| p == path).map(|(_, s)| s)
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<(Action, Option<PathBuf>)> {
        self.hits
            .iter()
            .find(|(rect, ..)| rect.contains(x, y))
            .map(|(_, a, repo)| (*a, repo.clone()))
    }

    /// A button by the name a self-test script uses: `inspector.push`, or
    /// `inspector.push.1` for the second repository's.
    pub fn named(&self, name: &str) -> Option<Viewport> {
        let rest = name.strip_prefix("inspector.")?;
        let (rest, n) = match rest.rsplit_once('.') {
            Some((rest, n)) if n.parse::<usize>().is_ok() => (rest, n.parse().ok()),
            _ => (rest, None),
        };
        // A worktree strip's button, or an issue row, by its place: `.1`
        // the second.
        if rest.starts_with("worktree_") || rest.starts_with("issue") || rest.starts_with("repo_") {
            return self
                .hits
                .iter()
                .filter(|(_, a, _)| a.name() == rest)
                .nth(n.unwrap_or(0))
                .map(|(rect, ..)| *rect);
        }
        let repo = n.and_then(|n: usize| self.read_for.get(n).cloned());
        self.hits
            .iter()
            .find(|(_, a, r)| a.name() == rest && (repo.is_none() || *r == repo))
            .map(|(rect, ..)| *rect)
    }

    /// For the self-test: what the status says for each of `repos`, and
    /// which buttons are live; with several, each as `name: ...`, joined
    /// by ` | `.
    pub fn report(&self, repos: Option<&[PathBuf]>) -> String {
        if self.hidden {
            return "hidden".into();
        }
        if let Some(login) = &self.shown_remote {
            return format!("ssh={login}");
        }
        // A remote workspace: where it is, then its Git as a local one's.
        let at = self.shown_host.as_ref().map(|at| format!("remote={at}"));
        let Some(repos) = repos else {
            return match at {
                Some(at) if self.unreachable => format!("{at} unreachable"),
                Some(at) => at,
                None => "-".into(),
            };
        };
        if repos.is_empty() {
            return at.map_or_else(|| "none".into(), |at| format!("{at} none"));
        }
        let local = self.report_repos(repos);
        match at {
            Some(at) => format!("{at} {local}"),
            None => local,
        }
    }

    /// [`Inspector::report`] for the repositories shown.
    fn report_repos(&self, repos: &[PathBuf]) -> String {
        let one = |path: &Path| {
            // Folded: how often its status was read this run, which stays
            // put while it is.
            if self.shown_folded.iter().any(|p| p == path) {
                let reads = self
                    .reads
                    .get(&(self.read_host.clone(), path.to_path_buf()))
                    .copied()
                    .unwrap_or(0);
                return format!("folded reads={reads}");
            }
            let status = match self.status_of(path) {
                None => "reading".to_owned(),
                Some(Err(e)) => format!("error={e}"),
                Some(Ok(s)) => format!(
                    "branch={} base={} ahead={} behind={} upstream={} push={} pull={} dirty={} changes=+{}-{} tag={}",
                    s.branch.as_deref().unwrap_or("-"),
                    s.base.as_deref().unwrap_or("-"),
                    s.ahead,
                    s.behind,
                    s.upstream.as_deref().unwrap_or("-"),
                    s.to_push,
                    s.to_pull,
                    s.dirty,
                    s.added,
                    s.removed,
                    s.tag
                        .as_ref()
                        .map_or("-".to_owned(), |(t, n)| format!("{t}+{n}")),
                ),
            };
            // With an upstream: whether it was checked, which nothing else
            // does for it.
            let check = match self.status_of(path) {
                Some(Ok(s)) if s.upstream.is_some() => {
                    let key = (self.read_host.clone(), path.to_path_buf());
                    match self.checks.get(&key) {
                        None => " check=none",
                        Some(Check::Running(_)) => " check=running",
                        Some(Check::Done(_)) => " check=ok",
                        Some(Check::Failed(_)) => " check=err",
                    }
                }
                _ => "",
            };
            let status = format!("{status}{check}");
            let live: Vec<&str> = self
                .hits
                .iter()
                .filter(|(_, a, r)| {
                    !matches!(
                        a,
                        Action::Close
                            | Action::Refresh
                            | Action::OpenIssue(_)
                            | Action::ToggleIssues
                            | Action::LoadIssues
                            | Action::HideIssues
                            | Action::FoldRepo
                            | Action::CheckRemote
                    ) && r.as_deref().is_none_or(|r| r == path)
                })
                .map(|(_, a, _)| a.name())
                .collect();
            let strips: Vec<String> = self
                .shown_worktrees
                .iter()
                .filter(|(repo, ..)| repo == path)
                .map(|(_, tree, name, merged)| {
                    let state = match self.status_of(tree) {
                        Some(Ok(s)) => format!(
                            "{name}:ahead={}:+{}-{}:files={}",
                            s.ahead, s.added, s.removed, s.files
                        ),
                        Some(Err(_)) => format!("{name}:error"),
                        None => format!("{name}:reading"),
                    };
                    if *merged {
                        format!("{state}:merged")
                    } else {
                        state
                    }
                })
                .collect();
            let mut line = if strips.is_empty() {
                format!("{status} buttons={}", live.join(","))
            } else {
                format!(
                    "{status} buttons={} worktrees={}",
                    live.join(","),
                    strips.join(",")
                )
            };
            if let Some((_, issues)) = self.shown_issues.iter().find(|(p, _)| p == path) {
                line.push_str(&format!(" issues={issues}"));
            }
            line
        };
        if let [only] = repos {
            return one(only);
        }
        repos
            .iter()
            .map(|path| {
                let name = path.file_name().map_or_else(
                    || path.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                format!("{name}: {}", one(path))
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }

    /// `worktrees`: for a folder workspace, the worktrees of its
    /// repositories, each with the name its row has, what it has been
    /// doing, and whether it is merged. `folded`: the repositories whose
    /// cards are folded to their name row.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        workspace: Option<&Workspace>,
        repos: Option<&[PathBuf]>,
        worktrees: &[(Workspace, String, Pace, bool)],
        remote: Option<&crate::platform::remote::Remote>,
        pull_request: Option<Option<&PullRequest>>,
        github: bool,
        issues: Issues,
        folded: &HashSet<RepoKey>,
        atlas: &mut Atlas,
        rect: Viewport,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) {
        self.hits.clear();
        self.shown_issues.clear();
        self.shown_folded.clear();
        self.shown_remote = remote.map(|r| r.login());
        self.shown_host = workspace
            .filter(|_| remote.is_none())
            .and_then(|w| Some(format!("{}:{}", w.host.as_ref()?, w.path.display())));
        self.shown_worktrees.clear();
        layout::push_rect(
            out,
            atlas,
            [rect.x, rect.y],
            [rect.width, rect.height],
            theme.sidebar_background,
        );
        layout::push_rect(
            out,
            atlas,
            [rect.x, rect.y],
            [1.0, rect.height],
            theme.hairline,
        );
        layout::push_sidebar_title(out, atlas, rect, "WORKSPACE", 2, theme);
        let [refresh, close] = layout::sidebar_header_buttons::<2>(rect);
        // While the Checks it started run, refresh waits, and a bar slides
        // under the header.
        let checking = self.checking_all();
        for (button, icon, tip, action, enabled) in [
            (
                refresh,
                icons::REFRESH,
                "Check Every Repo's Remote and Refresh",
                Action::Refresh,
                !checking,
            ),
            (
                close,
                icons::CLOSE,
                "Hide Inspector  \u{2325}\u{2318}I",
                Action::Close,
                true,
            ),
        ] {
            layout::Button::new(button)
                .icon(icon)
                .tone(layout::Tone::Ghost)
                .enabled(enabled)
                .tip(tip)
                .draw(out, atlas, theme);
            if enabled {
                self.hits.push((button, action, None));
            }
        }
        if checking {
            layout::push_progress(
                out,
                atlas,
                Viewport {
                    x: rect.x + 1.0,
                    y: rect.y + layout::SIDEBAR_HEADER_HEIGHT - 2.0,
                    width: rect.width - 1.0,
                    height: 2.0,
                },
                theme,
            );
        }
        // The body scrolls under the header: drawn moved up by `scroll`,
        // then cut to the room under the header, its buttons with it.
        let top = rect.y + layout::SIDEBAR_HEADER_HEIGHT;
        let bottom = rect.y + rect.height;
        let first_hit = self.hits.len();
        let mut body = Vec::new();
        self.draw_body(
            workspace,
            repos,
            worktrees,
            remote,
            pull_request,
            github,
            issues,
            folded,
            atlas,
            rect,
            top - self.scroll,
            theme,
            &mut body,
        );
        let end = body
            .iter()
            .map(|q| q.pos[1] + q.size[1])
            .fold(top - self.scroll, f32::max)
            + self.scroll
            + PAD;
        self.max_scroll = (end - bottom).max(0.0);
        let shift = self.scroll.min(self.max_scroll) - self.scroll;
        self.scroll += shift;
        for mut quad in body {
            quad.pos[1] -= shift;
            if quad.pos[1] >= bottom || quad.pos[1] + quad.size[1] <= top {
                continue;
            }
            layout::clip_vertical(&mut quad, top, bottom);
            out.push(quad);
        }
        let mut hits = self.hits.split_off(first_hit);
        hits.retain_mut(|(b, ..)| {
            b.y -= shift;
            let (from, to) = (b.y.max(top), (b.y + b.height).min(bottom));
            b.y = from;
            b.height = to - from;
            b.height > 0.0
        });
        self.hits.extend(hits);
    }

    /// Scrolls the body by `points`, down the content positive.
    pub fn scroll_by(&mut self, points: f32) {
        self.scroll = (self.scroll + points).clamp(0.0, self.max_scroll);
    }

    /// Everything under the header, from `y` down, unclipped.
    #[allow(clippy::too_many_arguments)]
    fn draw_body(
        &mut self,
        workspace: Option<&Workspace>,
        repos: Option<&[PathBuf]>,
        worktrees: &[(Workspace, String, Pace, bool)],
        remote: Option<&crate::platform::remote::Remote>,
        pull_request: Option<Option<&PullRequest>>,
        github: bool,
        issues: Issues,
        folded: &HashSet<RepoKey>,
        atlas: &mut Atlas,
        rect: Viewport,
        y: f32,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) {
        let x = rect.x + PAD;
        let width = (rect.width - 2.0 * PAD).max(0.0);
        // Nothing is left out for want of room: the body scrolls.
        let bottom = f32::INFINITY;
        let mut y = y;
        let Some(workspace) = workspace else {
            note(out, atlas, x, y, width, "Select a workspace", theme);
            return;
        };
        let name = workspace.name();
        let name = fit(atlas, &name, width);
        layout::push_title(out, atlas, x, y, 15.0, &name, theme.text, width);
        y += 24.0;
        // No folder: no Git to show, only where its terminals start.
        if workspace.folderless() {
            text(out, atlas, x, y, width, "No folder", theme.gutter_text);
            y += ROW;
            let said = wrap(
                atlas,
                "Its terminals and Claude sessions start in your home folder.",
                width,
            );
            for line in said {
                note(out, atlas, x, y, width, &line, theme);
                y += ROW;
            }
            return;
        }
        let shown = crate::platform::agents::shown_folder(workspace);
        let shown = fit_start(atlas, &shown, width);
        text(out, atlas, x, y, width, &shown, theme.gutter_text);
        y += ROW;
        // The terminal showing is in ssh: where it is, not this Mac's Git.
        if let Some(remote) = remote {
            text(out, atlas, x, y, width, "SSH SESSION", theme.status_text);
            y += ROW;
            let mut facts: Vec<Fact> = vec![("Host", vec![(remote.host.clone(), theme.text)])];
            if let Some(user) = &remote.user {
                facts.push(("User", vec![(user.clone(), theme.text)]));
            }
            if let Some(port) = remote.port {
                facts.push(("Port", vec![(port.to_string(), theme.text)]));
            }
            if remote.destination != remote.login() {
                facts.push((
                    "As typed",
                    vec![(remote.destination.clone(), theme.gutter_text)],
                ));
            }
            facts.push(("Connected", vec![(remote.connected_for(), theme.text)]));
            card(out, atlas, x, y, width, &facts, theme);
            return;
        }
        // A remote workspace: where its terminals go, then its Git as a
        // local one's, read on the host.
        if let Some(host) = &workspace.host {
            text(out, atlas, x, y, width, "REMOTE FOLDER", theme.status_text);
            y += ROW;
            let mut facts: Vec<Fact> = vec![
                ("Host", vec![(host.clone(), theme.text)]),
                (
                    "Folder",
                    vec![(workspace.path.display().to_string(), theme.text)],
                ),
            ];
            // A worktree there: its branch, as its row has it.
            if let Some(branch) = &workspace.branch {
                facts.push(("Branch", vec![(branch.clone(), theme.text)]));
            }
            facts.push(("Over", vec![("ssh".to_owned(), theme.gutter_text)]));
            y = card(out, atlas, x, y, width, &facts, theme) + 14.0;
        }
        let skeleton = |out: &mut Vec<GlyphInstance>, y: f32, rows: usize| {
            for n in 0..rows {
                layout::push_skeleton_row(
                    out,
                    Viewport {
                        x,
                        y: y + n as f32 * ROW,
                        width,
                        height: ROW,
                    },
                    0.0,
                    (0.8, 0.0),
                    false,
                    theme,
                );
            }
        };
        // Skeletons while the folder's repositories are first read; a host
        // that did not answer is said.
        let Some(repos) = repos else {
            if self.unreachable {
                note(out, atlas, x, y, width, "Host unreachable", theme);
            } else {
                skeleton(out, y, 4);
            }
            return;
        };
        if repos.is_empty() {
            note(
                out,
                atlas,
                x,
                y,
                width,
                "No Git repository in this folder or the folders in it",
                theme,
            );
            return;
        }
        let named = names_repos(workspace, repos);
        let mut single = None;
        for (n, repo) in repos.iter().enumerate() {
            if y + ROW > bottom {
                note(
                    out,
                    atlas,
                    x,
                    y,
                    width,
                    &format!("and {} more", plural(repos.len() - n, "repository")),
                    theme,
                );
                return;
            }
            if named {
                // Under the folder: the path from it; the folder itself, or
                // the repository it is inside: that one's name.
                let label = match repo.strip_prefix(&workspace.path) {
                    Ok(rest) if !rest.as_os_str().is_empty() => rest.display().to_string(),
                    _ => repo.file_name().map_or_else(
                        || repo.display().to_string(),
                        |n| n.to_string_lossy().into_owned(),
                    ),
                };
                let key = (workspace.host.clone(), repo.clone());
                let shut = folded.contains(&key);
                y = self.repo_heading(repo, &key, &label, shut, x, y, width, atlas, theme, out);
                if shut {
                    self.shown_folded.push(repo.clone());
                    continue;
                }
            }
            match self.status_of(repo) {
                None => {
                    skeleton(out, y, 3);
                    y += 3.0 * ROW + 10.0;
                }
                Some(Err(e)) => {
                    let why = if e == crate::project::worktree::UNREACHABLE {
                        "Host unreachable"
                    } else {
                        "Not a Git repository"
                    };
                    note(out, atlas, x, y, width, why, theme);
                    y += ROW;
                }
                Some(Ok(status)) => {
                    let status = status.clone();
                    y = self.repo_card(&status, repo, x, y, width, atlas, theme, out);
                    single = Some(status);
                    // A worktree's are its repository's, read once for all
                    // its worktrees.
                    let main = match &workspace.repo {
                        Some(main) if !workspace.folder => main.clone(),
                        _ => repo.clone(),
                    };
                    let key = (workspace.host.clone(), main);
                    y = self.issues_section(repo, &key, issues, x, y, width, atlas, theme, out);
                }
            }
            let mine: Vec<&(Workspace, String, Pace, bool)> = worktrees
                .iter()
                .filter(|(w, ..)| w.repo.as_deref() == Some(repo.as_path()))
                .collect();
            if !mine.is_empty() {
                y = self.worktree_strips(repo, &mine, x, y, width, atlas, theme, out);
            }
        }
        let (Some(status), [_]) = (single, repos) else {
            return;
        };

        // The pull request, for a branch other than the base.
        let Some(pull_request) = pull_request else {
            return;
        };
        let half = ((width - layout::UI_GAP) * 0.5).floor();
        if y + ROW > bottom {
            return;
        }
        text(out, atlas, x, y, width, "PULL REQUEST", theme.status_text);
        y += ROW;
        if !github {
            note(
                out,
                atlas,
                x,
                y,
                width,
                "Install GitHub's gh to see it here",
                theme,
            );
            return;
        }
        let Some(pr) = pull_request else {
            note(out, atlas, x, y, width, "None yet", theme);
            y += ROW;
            let b = Viewport {
                x,
                y,
                width,
                height: layout::UI_CONTROL,
            };
            let can = status.ahead > 0 || status.base.is_none();
            layout::Button::new(b)
                .label("Create Pull Request")
                .tone(layout::Tone::Primary)
                .enabled(can)
                .tip("Push the branch and open a pull request from its commits")
                .draw(out, atlas, theme);
            if can {
                self.hits.push((b, Action::CreatePullRequest, None));
            }
            return;
        };
        // Number and state, then the title over as many lines as it takes
        // (two at most), then the branches.
        let number = format!("#{}", pr.number);
        let nw = layout::ui_text_width(atlas, &number) + 12.0;
        let pill = Viewport {
            x,
            y: y + 2.0,
            width: nw,
            height: 20.0,
        };
        let state = if pr.draft {
            ("Draft", theme.gutter_text)
        } else {
            match pr.state.as_str() {
                "MERGED" => ("Merged", theme.accent),
                "CLOSED" => ("Closed", theme.diff_removed),
                _ => ("Open", theme.diff_added),
            }
        };
        layout::push_rounded_rect(
            out,
            pill,
            layout::UI_RADIUS_SM,
            layout::faded(state.1, 0.18),
        );
        layout::push_ui_text_centered(out, atlas, pill, &number, state.1);
        let sw = layout::ui_text_width(atlas, state.0);
        text(out, atlas, x + width - sw, y, sw + 2.0, state.0, state.1);
        y += 28.0;
        for line in wrap(atlas, &pr.title, width).into_iter().take(2) {
            text(out, atlas, x, y, width, &line, theme.text);
            y += 20.0;
        }
        let branches = format!("{} \u{2190} {}", pr.base, pr.head);
        let branches = fit(atlas, &branches, width);
        text(out, atlas, x, y, width, &branches, theme.gutter_text);
        y += ROW;
        let mut facts: Vec<Fact> = vec![
            ("Author", vec![(pr.author.clone(), theme.text)]),
            ("Commits", vec![(pr.commits.to_string(), theme.text)]),
            (
                "Changes",
                vec![
                    (format!("+{}", pr.additions), theme.diff_added),
                    (format!("\u{2212}{}", pr.deletions), theme.diff_removed),
                ],
            ),
        ];
        if pr.mergeable == "CONFLICTING" {
            facts.push((
                "Merge",
                vec![("conflicts with base".into(), theme.diff_removed)],
            ));
        }
        y = card(out, atlas, x, y, width, &facts, theme) + 10.0;
        let half_rect = |n: usize| Viewport {
            x: x + n as f32 * (half + layout::UI_GAP),
            y,
            width: half,
            height: layout::UI_CONTROL,
        };
        let open = pr.state == "OPEN";
        let can_merge = open && !pr.draft && pr.mergeable != "CONFLICTING";
        let merge = half_rect(0);
        layout::Button::new(merge)
            .label("Merge\u{2026}")
            .tone(layout::Tone::Primary)
            .menu()
            .enabled(can_merge)
            .tip("Squash, merge commit or rebase, then confirm")
            .draw(out, atlas, theme);
        if can_merge {
            self.hits.push((merge, Action::MergePullRequest, None));
        }
        let view = half_rect(1);
        layout::Button::new(view)
            .label("Open on GitHub")
            .draw(out, atlas, theme);
        self.hits.push((view, Action::OpenPullRequest, None));
        y += layout::UI_CONTROL + 14.0;

        // Its checks.
        if pr.list.is_empty() || y + ROW > bottom {
            return;
        }
        text(out, atlas, x, y, width, "CHECKS", theme.status_text);
        y += ROW;
        let count = |s: CheckState| pr.list.iter().filter(|c| c.state == s).count();
        let mut summary = Vec::new();
        for (state, word) in [
            (CheckState::Failed, "failing"),
            (CheckState::Running, "running"),
            (CheckState::Passed, "successful"),
            (CheckState::Skipped, "skipped"),
        ] {
            let n = count(state);
            if n > 0 {
                summary.push(format!("{n} {word}"));
            }
        }
        text(out, atlas, x, y, width, &summary.join(", "), theme.text);
        y += ROW;
        let room = ((bottom - y) / ROW).floor().max(0.0) as usize;
        let shown = if pr.list.len() > room {
            room.saturating_sub(1)
        } else {
            pr.list.len()
        };
        for check in pr.list.iter().take(shown) {
            let colour = match check.state {
                CheckState::Passed => theme.diff_added,
                CheckState::Failed => theme.diff_removed,
                CheckState::Running => theme.diff_modified,
                CheckState::Skipped => theme.gutter_text,
            };
            layout::push_rounded_rect(
                out,
                Viewport {
                    x: x + 2.0,
                    y: y + 9.0,
                    width: 8.0,
                    height: 8.0,
                },
                4.0,
                colour,
            );
            let label = check.state.label();
            let lw = layout::ui_text_width(atlas, label);
            text(
                out,
                atlas,
                x + width - lw,
                y + 3.0,
                lw + 2.0,
                label,
                theme.gutter_text,
            );
            let room = (width - 18.0 - lw - 8.0).max(0.0);
            let name = fit(atlas, &check.name, room);
            text(out, atlas, x + 18.0, y + 3.0, room, &name, theme.text);
            y += ROW;
        }
        if shown < pr.list.len() {
            note(
                out,
                atlas,
                x,
                y,
                width,
                &format!("and {} more on GitHub", pr.list.len() - shown),
                theme,
            );
        }
    }
}

impl Inspector {
    /// A repository's worktrees, a strip each in one card: the branch (a
    /// click opens it) and, under it, how far ahead of the base and the
    /// change from where it left it; at the right, Review and Terminal, or
    /// Clean up once it is merged. Returns the y under it.
    #[allow(clippy::too_many_arguments)]
    fn worktree_strips(
        &mut self,
        repo: &Path,
        worktrees: &[&(Workspace, String, Pace, bool)],
        x: f32,
        y: f32,
        width: f32,
        atlas: &mut Atlas,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) -> f32 {
        let mut y = y;
        let heading = Viewport {
            x,
            y,
            width,
            height: 20.0,
        };
        text(out, atlas, x, y, width, "WORKTREES", theme.status_text);
        let hw = layout::ui_text_width(atlas, "WORKTREES");
        layout::push_count(out, atlas, x + hw + 8.0, heading, worktrees.len(), theme);
        y += ROW;
        let height = worktrees.len() as f32 * STRIP;
        layout::push_card(
            out,
            Viewport {
                x,
                y,
                width,
                height,
            },
            theme,
        );
        for (n, (tree, name, pace, merged)) in worktrees.iter().enumerate() {
            let merged = *merged;
            self.shown_worktrees.push((
                repo.to_path_buf(),
                tree.path.clone(),
                name.clone(),
                merged,
            ));
            let top = y + n as f32 * STRIP;
            if n > 0 {
                layout::push_rect(
                    out,
                    atlas,
                    [x + 10.0, top],
                    [width - 20.0, 1.0],
                    theme.hairline,
                );
            }
            // The buttons, right to left, beside the branch: the line under
            // it has the whole width.
            let mut right = x + width - 6.0;
            let by = top + 4.0;
            let review_tip = match &tree.base {
                Some(base) => format!("Changes on this branch since {base}, uncommitted included"),
                None => "Changes on this branch, uncommitted included".to_owned(),
            };
            let terminal = (
                "Terminal",
                Action::WorktreeTerminal,
                "Start a shell in this worktree".to_owned(),
            );
            let clean_up = (
                "Clean up",
                Action::CleanUpWorktree,
                "Remove this merged worktree and its branch".to_owned(),
            );
            // A host's worktree is not reviewed from here.
            let buttons = match (merged, tree.host.is_some()) {
                (true, true) => vec![clean_up, terminal],
                (false, true) => vec![terminal],
                (true, false) => vec![clean_up, ("Review", Action::ReviewWorktree, review_tip)],
                (false, false) => vec![terminal, ("Review", Action::ReviewWorktree, review_tip)],
            };
            for (label, action, tip) in buttons {
                let probe = layout::Button::new(Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: 0.0,
                    height: layout::UI_CONTROL_SM,
                })
                .label(label);
                let w = probe.content_width(atlas, 8.0);
                let b = Viewport {
                    x: right - w,
                    y: by,
                    width: w,
                    height: layout::UI_CONTROL_SM,
                };
                // A ring: on the card, a button's fill is the card's own colour.
                layout::push_rounded_rect(
                    out,
                    Viewport {
                        x: b.x - 1.0,
                        y: b.y - 1.0,
                        width: b.width + 2.0,
                        height: b.height + 2.0,
                    },
                    layout::UI_RADIUS_SM + 1.0,
                    theme.control_hover,
                );
                layout::Button { rect: b, ..probe }
                    .tip(&tip)
                    .draw(out, atlas, theme);
                self.hits.push((b, action, Some(tree.path.clone())));
                right = b.x - layout::UI_GAP;
            }
            // The branch name opens the workspace: it lights up under the
            // pointer like a link.
            let name_room = (right - 4.0 - (x + 10.0)).max(0.0);
            let shown = fit(atlas, name, name_room);
            let name_rect = Viewport {
                x: x + 4.0,
                y: by,
                width: (layout::ui_text_width(atlas, &shown) + 12.0).min(name_room + 6.0),
                height: layout::UI_CONTROL_SM,
            };
            let over = layout::hovered(name_rect);
            if over {
                layout::push_rounded_rect(out, name_rect, layout::UI_RADIUS_SM, theme.row_hover);
            }
            layout::hotspot(
                name_rect,
                layout::Cursor::Pointing,
                Some("Show this worktree's workspace"),
            );
            text(
                out,
                atlas,
                x + 10.0,
                top + 5.0,
                name_room,
                &shown,
                if over { theme.accent } else { theme.text },
            );
            self.hits
                .push((name_rect, Action::OpenWorktree, Some(tree.path.clone())));
            let room = (width - 20.0).max(0.0);
            let line2 = top + 24.0;
            match self.status_of(&tree.path) {
                None => layout::push_skeleton_row(
                    out,
                    Viewport {
                        x: x + 10.0,
                        y: line2,
                        width: room,
                        height: 18.0,
                    },
                    0.0,
                    (0.7, 0.0),
                    false,
                    theme,
                ),
                Some(Err(_)) => {
                    text(
                        out,
                        atlas,
                        x + 10.0,
                        line2,
                        room,
                        "not readable",
                        theme.gutter_text,
                    );
                }
                Some(Ok(status)) => {
                    let mut parts: Vec<(String, [f32; 4])> = Vec::new();
                    match (status.base.as_ref(), status.ahead) {
                        _ if merged => parts.push(("merged".into(), theme.diff_added)),
                        (None, _) => parts.push(("no base".into(), theme.gutter_text)),
                        (Some(_), 0) => parts.push(("even".into(), theme.gutter_text)),
                        (Some(_), a) => parts.push((format!("{a} ahead"), theme.status_text)),
                    }
                    if status.files > 0 {
                        parts.push((format!("+{}", status.added), theme.diff_added));
                        parts.push((format!("\u{2212}{}", status.removed), theme.diff_removed));
                        parts.push((plural(status.files, "file"), theme.gutter_text));
                    }
                    // What it has been doing, as its row says it.
                    match pace {
                        Pace::Writing => parts.push(("\u{b7} writing".into(), theme.accent)),
                        Pace::Quiet(_) if merged => {}
                        Pace::Quiet(d) => parts.push((
                            format!("\u{b7} quiet {}", crate::platform::agents::span(*d)),
                            theme.gutter_text,
                        )),
                        Pace::Committed(_) | Pace::Unknown => {}
                    }
                    let mut vx = x + 10.0;
                    for (part, colour) in parts {
                        let left = (x + 10.0 + room - vx).max(0.0);
                        if left <= 0.0 {
                            break;
                        }
                        let part = fit(atlas, &part, left);
                        text(out, atlas, vx, line2, left, &part, colour);
                        vx += layout::ui_text_width(atlas, &part) + 6.0;
                    }
                }
            }
            // The branch's last commit: its subject, and how long ago.
            if let Some((time, subject)) = &tree.commit {
                let line3 = top + 42.0;
                let ago = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH + Duration::from_secs(*time))
                    .unwrap_or_default();
                let ago = format!("{} ago", crate::platform::agents::span(ago));
                let aw = layout::ui_text_width(atlas, &ago);
                text(
                    out,
                    atlas,
                    x + width - 10.0 - aw,
                    line3,
                    aw + 2.0,
                    &ago,
                    theme.gutter_text,
                );
                let room = (width - 20.0 - aw - 8.0).max(0.0);
                let subject = fit(atlas, subject, room);
                text(
                    out,
                    atlas,
                    x + 10.0,
                    line3,
                    room,
                    &subject,
                    theme.status_text,
                );
            }
        }
        y + height + 14.0
    }

    /// A repository's name over its card, a chevron before it: a click
    /// folds the card to this row or opens it. Folded, the row says the
    /// branch and, with a dot, uncommitted work, as last read. Returns the
    /// y under it.
    #[allow(clippy::too_many_arguments)]
    fn repo_heading(
        &mut self,
        repo: &Path,
        key: &RepoKey,
        label: &str,
        folded: bool,
        x: f32,
        y: f32,
        width: f32,
        atlas: &mut Atlas,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) -> f32 {
        let row = Viewport {
            x: x - 4.0,
            y,
            width: width + 8.0,
            height: ROW,
        };
        let over = layout::hovered(row);
        if over {
            layout::push_rounded_rect(out, row, layout::UI_RADIUS_SM, theme.row_hover);
        }
        layout::hotspot(
            row,
            layout::Cursor::Pointing,
            Some(if folded {
                "Show this repository"
            } else {
                "Fold this repository to one row"
            }),
        );
        self.hits
            .push((row, Action::FoldRepo, Some(repo.to_path_buf())));
        layout::push_icon_centered(
            out,
            atlas,
            Viewport {
                x,
                y: y + 3.0,
                width: 16.0,
                height: 20.0,
            },
            if folded {
                icons::CHEVRON_RIGHT
            } else {
                icons::CHEVRON_DOWN
            },
            if over { theme.text } else { theme.gutter_text },
        );
        let left = x + 20.0;
        let mut room = (width - 20.0).max(0.0);
        // Folded: the branch at the right, and a dot for uncommitted work,
        // from the last read; nothing read yet says nothing.
        let known = if folded { self.known.get(key) } else { None };
        if let Some((branch, dirty)) = known {
            let mut right = x + width;
            if *dirty > 0 {
                right -= 8.0;
                layout::push_rounded_rect(
                    out,
                    Viewport {
                        x: right,
                        y: y + 9.0,
                        width: 8.0,
                        height: 8.0,
                    },
                    4.0,
                    theme.diff_modified,
                );
                right -= 8.0;
            }
            let branch = branch.as_deref().unwrap_or("detached HEAD");
            let branch = fit(atlas, branch, (room * 0.5).max(0.0));
            let bw = layout::ui_text_width(atlas, &branch);
            text(
                out,
                atlas,
                right - bw,
                y + 3.0,
                bw + 2.0,
                &branch,
                theme.gutter_text,
            );
            room = (right - bw - 8.0 - left).max(0.0);
        }
        let label = fit(atlas, label, room);
        layout::push_title(out, atlas, left, y + 4.0, 13.0, &label, theme.text, room);
        y + if folded { ROW + 4.0 } else { ROW }
    }

    /// Shows all of the issues of the repository `main`, or the first few
    /// again.
    pub fn toggle_issues(&mut self, main: &Path) {
        if !self.issues_open.remove(main) {
            self.issues_open.insert(main.to_path_buf());
        }
    }

    /// The open issues of `key`'s repository, under `repo`'s card: the
    /// first few in a card, a row each (a click reads it whole), and Show
    /// all; skeletons until they are first read; what went wrong when they
    /// could not be. Nothing for a repository not on GitHub, or with `gh`
    /// not asked. Returns the y under it.
    #[allow(clippy::too_many_arguments)]
    fn issues_section(
        &mut self,
        repo: &Path,
        key: &RepoKey,
        issues: Issues,
        x: f32,
        y: f32,
        width: f32,
        atlas: &mut Atlas,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) -> f32 {
        use crate::project::worktree::IssueError;
        let set = issues.read.get(key);
        if !issues.asked
            || set.is_some_and(|s| s.list.is_empty() && s.error == Some(IssueError::NotGitHub))
        {
            return y;
        }
        let mut y = y;
        text(out, atlas, x, y, width, "ISSUES", theme.status_text);
        // Not asked for: a button to, and nothing read.
        if !issues.loaded.contains(key) {
            y += ROW;
            let b = Viewport {
                x,
                y,
                width: 0.0,
                height: layout::UI_CONTROL_SM,
            };
            let button = layout::Button::new(b)
                .label("Load Issues")
                .icon(icons::CLOUD_DOWNLOAD)
                .tip("Read this repository's open issues from GitHub, now and every few minutes");
            let b = Viewport {
                width: button.content_width(atlas, 10.0),
                ..b
            };
            layout::Button { rect: b, ..button }.draw(out, atlas, theme);
            self.hits.push((b, Action::LoadIssues, Some(key.1.clone())));
            self.shown_issues
                .push((repo.to_path_buf(), "unloaded".into()));
            return y + layout::UI_CONTROL_SM + 14.0;
        }
        // Hide, at the right; the count over a list beside it, none over a
        // note.
        let hide = Viewport {
            x: x + width - layout::UI_CONTROL_SM,
            y: y - 1.0,
            width: layout::UI_CONTROL_SM,
            height: layout::UI_CONTROL_SM,
        };
        layout::Button::new(hide)
            .icon(icons::CLOSE)
            .tone(layout::Tone::Ghost)
            .tip("Hide this repository's issues and stop reading them")
            .draw(out, atlas, theme);
        self.hits
            .push((hide, Action::HideIssues, Some(key.1.clone())));
        if let Some(set) = set.filter(|s| !s.list.is_empty()) {
            let open = format!("{} open", set.count());
            let ow = layout::ui_text_width(atlas, &open);
            text(
                out,
                atlas,
                hide.x - 6.0 - ow,
                y,
                ow + 2.0,
                &open,
                theme.gutter_text,
            );
        }
        y += ROW;
        let Some(set) = set else {
            for n in 0..3 {
                layout::push_skeleton_row(
                    out,
                    Viewport {
                        x,
                        y: y + n as f32 * ROW,
                        width,
                        height: ROW,
                    },
                    0.0,
                    (0.8, 0.0),
                    false,
                    theme,
                );
            }
            self.shown_issues
                .push((repo.to_path_buf(), "loading".into()));
            return y + 3.0 * ROW + 10.0;
        };
        if set.list.is_empty() {
            let (said, shown) = match &set.error {
                None => ("No open issues".to_owned(), "0"),
                Some(IssueError::NeedsAuth) => (IssueError::NeedsAuth.message(), "auth"),
                Some(e) => (format!("Could not read them: {}", e.message()), "error"),
            };
            for line in wrap(atlas, &said, width) {
                note(out, atlas, x, y, width, &line, theme);
                y += 20.0;
            }
            self.shown_issues.push((repo.to_path_buf(), shown.into()));
            return y + 14.0;
        }
        let all = self.issues_open.contains(&key.1);
        let more = set.list.len() > ISSUES_SHOWN;
        let shown = if all || !more {
            set.list.len()
        } else {
            ISSUES_SHOWN
        };
        let rows = &set.list[..shown];
        let height = (shown + usize::from(more)) as f32 * ROW + 8.0;
        layout::push_card(
            out,
            Viewport {
                x,
                y,
                width,
                height,
            },
            theme,
        );
        // The titles line up after the widest number.
        let numbers: Vec<String> = rows.iter().map(|i| format!("#{}", i.number)).collect();
        let nw = numbers
            .iter()
            .map(|n| layout::ui_text_width(atlas, n))
            .fold(0.0, f32::max);
        let inner = width - 20.0;
        for (n, (issue, number)) in rows.iter().zip(&numbers).enumerate() {
            let top = y + 4.0 + n as f32 * ROW;
            let row = Viewport {
                x: x + 4.0,
                y: top,
                width: width - 8.0,
                height: ROW,
            };
            if layout::hovered(row) {
                layout::push_rounded_rect(out, row, layout::UI_RADIUS_SM, theme.row_hover);
            }
            // The whole title, for one cut short.
            layout::hotspot(row, layout::Cursor::Pointing, Some(&issue.title));
            self.hits
                .push((row, Action::OpenIssue(issue.number), Some(key.1.clone())));
            let ty = top + 3.0;
            text(
                out,
                atlas,
                x + 10.0,
                ty,
                nw + 2.0,
                number,
                theme.status_text,
            );
            let age = crate::project::worktree::age(&issue.created_at, issues.now);
            let aw = layout::ui_text_width(atlas, &age);
            let right = x + 10.0 + inner;
            text(
                out,
                atlas,
                right - aw,
                ty,
                aw + 2.0,
                &age,
                theme.gutter_text,
            );
            // Its labels, two at most, between the title and its age: as
            // many as leave the title its room.
            let title_x = x + 10.0 + nw + 8.0;
            let mut end = right - aw - if age.is_empty() { 0.0 } else { 8.0 };
            let mut pills: Vec<(String, f32)> = issue
                .labels
                .iter()
                .take(2)
                .map(|l| {
                    let l = fit(atlas, l, 72.0);
                    let w = layout::ui_text_width(atlas, &l) + 12.0;
                    (l, w)
                })
                .collect();
            while end - pills.iter().map(|(_, w)| w + 4.0).sum::<f32>() - title_x < TITLE_ROOM {
                if pills.pop().is_none() {
                    break;
                }
            }
            for (label, w) in pills.iter().rev() {
                let pill = Viewport {
                    x: end - w,
                    y: top + 4.0,
                    width: *w,
                    height: 18.0,
                };
                layout::push_rounded_rect(
                    out,
                    pill,
                    layout::UI_RADIUS_SM,
                    layout::faded(theme.status_text, 0.16),
                );
                layout::push_ui_text_centered(out, atlas, pill, label, theme.status_text);
                end = pill.x - 4.0;
            }
            let room = (end - 4.0 - title_x).max(0.0);
            let title = fit(atlas, &issue.title, room);
            text(out, atlas, title_x, ty, room, &title, theme.text);
        }
        if more {
            let top = y + 4.0 + shown as f32 * ROW;
            layout::push_rect(
                out,
                atlas,
                [x + 10.0, top],
                [width - 20.0, 1.0],
                theme.hairline,
            );
            let label = if all {
                "Show fewer".to_owned()
            } else {
                format!("Show all {}", set.count())
            };
            let lw = layout::ui_text_width(atlas, &label);
            let row = Viewport {
                x: x + 4.0,
                y: top,
                width: width - 8.0,
                height: ROW,
            };
            let over = layout::hovered(row);
            if over {
                layout::push_rounded_rect(out, row, layout::UI_RADIUS_SM, theme.row_hover);
            }
            layout::hotspot(row, layout::Cursor::Pointing, None);
            text(
                out,
                atlas,
                x + 10.0,
                top + 3.0,
                lw + 2.0,
                &label,
                theme.accent,
            );
            self.hits
                .push((row, Action::ToggleIssues, Some(key.1.clone())));
        }
        y += height;
        let mut said = format!("{}:rows={shown}", set.count());
        // A read that failed keeps the list before, and says so faintly.
        if let Some(e) = &set.error {
            let why = match e {
                IssueError::NeedsAuth => e.message(),
                _ => "Last read failed, showing the one before".to_owned(),
            };
            let why = fit(atlas, &why, width);
            text(
                out,
                atlas,
                x,
                y + 4.0,
                width,
                &why,
                layout::faded(theme.gutter_text, 0.7),
            );
            y += 22.0;
            said.push_str(":stale");
        }
        self.shown_issues.push((repo.to_path_buf(), said));
        y + 14.0
    }

    /// One repository's facts and its buttons; returns the y under them.
    #[allow(clippy::too_many_arguments)]
    fn repo_card(
        &mut self,
        status: &Status,
        repo: &Path,
        x: f32,
        y: f32,
        width: f32,
        atlas: &mut Atlas,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) -> f32 {
        // Where the branch stands.
        let mut facts: Vec<Fact> = vec![(
            "Branch",
            vec![(
                status
                    .branch
                    .clone()
                    .unwrap_or_else(|| "detached HEAD".into()),
                theme.text,
            )],
        )];
        if let Some(base) = &status.base {
            let label = match (status.ahead, status.behind) {
                (0, 0) => "even".to_owned(),
                (a, 0) => format!("{a} ahead"),
                (0, b) => format!("{b} behind"),
                (a, b) => format!("{a} ahead, {b} behind"),
            };
            facts.push(("Against", vec![(format!("{base}: {label}"), theme.text)]));
            if status.files > 0 {
                facts.push((
                    "Changes",
                    vec![
                        (format!("+{}", status.added), theme.diff_added),
                        (format!("\u{2212}{}", status.removed), theme.diff_removed),
                        (plural(status.files, "file"), theme.gutter_text),
                    ],
                ));
            }
        }
        let key = (self.read_host.clone(), repo.to_path_buf());
        let check = self.checks.get(&key);
        let checking = matches!(check, Some(Check::Running(_)));
        facts.push((
            "Remote",
            remote_values(status, check, crate::platform::unix_seconds())
                .into_iter()
                .map(|(value, ink)| (value, ink.colour(theme)))
                .collect(),
        ));
        let failed = match check {
            Some(Check::Failed(e)) => e.lines().map(str::trim).find(|l| !l.is_empty()),
            _ => None,
        };
        facts.push((
            "Uncommitted",
            if status.dirty == 0 {
                vec![("nothing".into(), theme.gutter_text)]
            } else {
                vec![
                    (plural(status.dirty, "file"), theme.text),
                    (format!("+{}", status.dirty_added), theme.diff_added),
                    (
                        format!("\u{2212}{}", status.dirty_removed),
                        theme.diff_removed,
                    ),
                ]
            },
        ));
        if let Some((tag, since)) = &status.tag {
            facts.push((
                "Latest tag",
                vec![
                    (tag.clone(), theme.text),
                    (
                        format!("{} since", plural(*since, "commit")),
                        theme.gutter_text,
                    ),
                ],
            ));
        }
        let mut y = card(out, atlas, x, y, width, &facts, theme) + 10.0;
        // A Check that failed says why, faintly, under the card.
        if let Some(why) = failed {
            let why = fit(atlas, why, width);
            text(
                out,
                atlas,
                x,
                y - 6.0,
                width,
                &why,
                layout::faded(theme.gutter_text, 0.7),
            );
            y += 20.0;
        }

        // What can be done with it.
        let base = status.base.clone().unwrap_or_else(|| "main".into());
        let update = format!("Merge {base} In");
        let check_label = if checking {
            "Checking\u{2026}"
        } else {
            "Check"
        };
        let mut buttons = vec![
            (
                "Commit All\u{2026}",
                Action::CommitAll,
                status.dirty > 0,
                "Stage every change and commit it",
                None,
            ),
            (
                update.as_str(),
                Action::UpdateFromBase,
                status.base.is_some() && status.behind > 0,
                "Take the base branch's new commits: a merge, never a rebase",
                None,
            ),
            // Live with any upstream: `git pull --ff-only` fetches first,
            // so the counts, as of the last fetch, do not decide it.
            (
                "Pull",
                Action::Pull,
                status.upstream.is_some() && !checking,
                "Fast-forward from the upstream",
                None,
            ),
            (
                "Push",
                Action::Push,
                status.branch.is_some() && (status.upstream.is_none() || status.to_push > 0),
                "Push the branch, setting its upstream the first time",
                None,
            ),
        ];
        if status.upstream.is_some() {
            buttons.push((
                check_label,
                Action::CheckRemote,
                !checking,
                "Fetch from the remote, then show what is ahead and behind",
                Some(icons::REFRESH),
            ));
        }
        // The main checkout has nothing to take main's commits from.
        buttons.retain(|b| b.1 != Action::UpdateFromBase || status.base.is_some());
        let half = ((width - layout::UI_GAP) * 0.5).floor();
        for (n, (label, action, enabled, tip, icon)) in buttons.iter().enumerate() {
            let b = Viewport {
                x: x + (n % 2) as f32 * (half + layout::UI_GAP),
                y: y + (n / 2) as f32 * (layout::UI_CONTROL + layout::UI_GAP),
                width: half,
                height: layout::UI_CONTROL,
            };
            let mut button = layout::Button::new(b)
                .label(label)
                .enabled(*enabled)
                .tip(tip);
            if let Some(icon) = icon {
                button = button.icon(*icon);
            }
            button.draw(out, atlas, theme);
            if *enabled {
                self.hits.push((b, *action, Some(repo.to_path_buf())));
            }
        }
        y + buttons.len().div_ceil(2) as f32 * (layout::UI_CONTROL + layout::UI_GAP) + 14.0
    }
}

/// A card of label and value rows; returns the y under it.
fn card(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    x: f32,
    y: f32,
    width: f32,
    facts: &[Fact],
    theme: &Theme,
) -> f32 {
    let height = facts.len() as f32 * ROW + 8.0;
    layout::push_card(
        out,
        Viewport {
            x,
            y,
            width,
            height,
        },
        theme,
    );
    let inner = width - 20.0;
    for (n, (label, values)) in facts.iter().enumerate() {
        let row_y = y + 4.0 + n as f32 * ROW + 3.0;
        text(out, atlas, x + 10.0, row_y, inner, label, theme.status_text);
        // Values right-aligned, in order, a space apart.
        let gap = 6.0;
        let widths: Vec<f32> = values
            .iter()
            .map(|(v, _)| layout::ui_text_width(atlas, v))
            .collect();
        let total: f32 = widths.iter().sum::<f32>() + gap * values.len().saturating_sub(1) as f32;
        let label_w = layout::ui_text_width(atlas, label) + 12.0;
        let mut vx = (x + 10.0 + inner - total).max(x + 10.0 + label_w);
        for ((value, colour), w) in values.iter().zip(&widths) {
            let room = (x + 10.0 + inner - vx).max(0.0);
            let value = fit(atlas, value, room);
            text(out, atlas, vx, row_y, room, &value, *colour);
            vx += w + gap;
        }
    }
    y + height
}

fn text(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    x: f32,
    y: f32,
    width: f32,
    s: &str,
    colour: [f32; 4],
) {
    layout::push_ui_text(
        out,
        atlas,
        Viewport {
            x,
            y,
            width,
            height: 20.0,
        },
        s,
        colour,
    );
}

fn note(
    out: &mut Vec<GlyphInstance>,
    atlas: &mut Atlas,
    x: f32,
    y: f32,
    width: f32,
    s: &str,
    theme: &Theme,
) {
    text(out, atlas, x, y + 3.0, width, s, theme.gutter_text);
}

/// Whether `workspace`'s inspector names each of `repos` over its card,
/// on a row that folds it: a folder workspace with repositories in it does;
/// a worktree, or a folder that is itself the only one, needs no name.
pub fn names_repos(workspace: &Workspace, repos: &[PathBuf]) -> bool {
    repos.len() > 1 || repos.first().is_some_and(|r| *r != workspace.path)
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// `s`, cut with an ellipsis to fit `width`. Half a point of slack: a
/// value right-aligned into exactly its own width comes back a rounding
/// error short of it.
pub(crate) fn fit(atlas: &mut Atlas, s: &str, width: f32) -> String {
    let width = width + 0.5;
    if layout::ui_text_width(atlas, s) <= width {
        return s.to_owned();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    // A UI width counts 120 characters, so a longer cut measures as the
    // whole string did, and does not fit either.
    let top = (chars.len() - 1).min(119);
    let ellipsis = layout::ui_text_width(atlas, "\u{2026}");
    let guess: Vec<f32> = match caret_xs(atlas, &chars[..=top]) {
        Some(x) => (0..=top).rev().map(|n| x[n].0 + ellipsis).collect(),
        None => vec![f32::NEG_INFINITY; top + 1],
    };
    first_fitting(atlas, width, &guess, |i| {
        chars[..top - i].iter().collect::<String>() + "\u{2026}"
    })
    .unwrap_or_default()
}

/// `s`, cut from its start with an ellipsis to fit `width`: for a path,
/// whose end says the most.
pub(crate) fn fit_start(atlas: &mut Atlas, s: &str, width: f32) -> String {
    let width = width + 0.5;
    if layout::ui_text_width(atlas, s) <= width {
        return s.to_owned();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 2 {
        return String::new();
    }
    let ellipsis = layout::ui_text_width(atlas, "\u{2026}");
    // What is measured of a cut is its first 120 characters, the ellipsis
    // and 119 of the rest.
    let guess: Vec<f32> = match caret_xs(atlas, &chars) {
        Some(x) => (1..chars.len())
            .map(|n| ellipsis + x[(n + 119).min(chars.len())].0 - x[n].1)
            .collect(),
        None => vec![f32::NEG_INFINITY; chars.len() - 1],
    };
    first_fitting(atlas, width, &guess, |i| {
        "\u{2026}".to_owned() + &chars[i + 1..].iter().collect::<String>()
    })
    .unwrap_or_default()
}

/// The caret x before each of `chars` and after the last, from the string
/// shaped once, if it can be. Inside a grapheme (an emoji sequence, a
/// base and its marks) a cut shapes as something else, often narrower, so
/// there it is a range: the x where the grapheme starts and where the next
/// one does.
///
/// Not for right-to-left text: a UI width is the caret x at the end,
/// which there is not the width at all, so every cut is measured.
fn caret_xs(atlas: &mut Atlas, chars: &[char]) -> Option<Vec<(f32, f32)>> {
    let rtl = |c: &char| {
        matches!(*c as u32,
            0x0590..=0x08ff | 0xfb1d..=0xfdff | 0xfe70..=0xfeff | 0x10800..=0x10fff
            | 0x1e800..=0x1efff | 0x200f | 0x202b | 0x202e | 0x2067)
    };
    if chars.iter().any(rtl) {
        return None;
    }
    let s: String = chars.iter().collect();
    let line = atlas.shape_ui(&s)?;
    let composed = objc2_foundation::NSString::from_str(&s);
    let mut utf16 = 0;
    let mut x = Vec::with_capacity(chars.len() + 1);
    let mut start = 0.0;
    for c in chars {
        let at = line.caret_offset(utf16);
        if composed
            .rangeOfComposedCharacterSequenceAtIndex(utf16)
            .location
            == utf16
        {
            start = at;
            x.push((at, at));
        } else {
            x.push((start, f32::NAN));
        }
        utf16 += c.len_utf16();
    }
    let mut next = line.caret_offset(utf16);
    x.push((next, next));
    for edge in x.iter_mut().rev() {
        if edge.1.is_nan() {
            edge.1 = next;
        } else {
            next = edge.1;
        }
    }
    Some(x)
}

/// The first of the cuts `cut(0)`, `cut(1)`, ... that measures no wider
/// than `width`, as measuring each in turn finds it, measuring few: the
/// search starts at the first whose `guess`, its width from the whole
/// string's glyph advances, fits, and steps from there while measured
/// widths disagree. The guess is off only by kerning and shaping across
/// the cut, well under `SLACK`; a guess of minus infinity measures every
/// cut in turn.
fn first_fitting(
    atlas: &mut Atlas,
    width: f32,
    guess: &[f32],
    cut: impl Fn(usize) -> String,
) -> Option<String> {
    const SLACK: f32 = 2.0;
    let fits = |atlas: &mut Atlas, i: usize| {
        let c = cut(i);
        (layout::ui_text_width(atlas, &c) <= width).then_some(c)
    };
    let mut i = guess
        .iter()
        .position(|&w| w <= width)
        .unwrap_or(guess.len().checked_sub(1)?);
    match fits(atlas, i) {
        Some(mut found) => {
            while i > 0 && guess[i - 1] <= width + SLACK {
                let Some(c) = fits(atlas, i - 1) else { break };
                found = c;
                i -= 1;
            }
            Some(found)
        }
        None => (i + 1..guess.len()).find_map(|j| fits(atlas, j)),
    }
}

/// `s` in lines no wider than `width`, broken between words; a word wider
/// than the line is cut.
fn wrap(atlas: &mut Atlas, s: &str, width: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in s.split_whitespace() {
        let next = if line.is_empty() {
            word.to_owned()
        } else {
            format!("{line} {word}")
        };
        if line.is_empty() || layout::ui_text_width(atlas, &next) <= width {
            line = next;
        } else {
            lines.push(std::mem::take(&mut line));
            line = word.to_owned();
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    // The second line says there is more, when there is.
    if lines.len() > 2 {
        let rest = lines[1..].join(" ");
        lines.truncate(1);
        lines.push(fit(atlas, &rest, width));
    }
    lines.into_iter().map(|l| fit(atlas, &l, width)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::worktree::{Issue, IssueError};

    fn issue(number: u64) -> Issue {
        Issue {
            number,
            title: format!("Seed tray {number} drains too slowly after a long watering"),
            state: "OPEN".into(),
            labels: vec!["bug".into(), "garden".into()],
            created_at: "2026-10-01T12:00:00Z".into(),
            ..Default::default()
        }
    }

    fn set(list: Vec<Issue>, error: Option<IssueError>) -> IssueSet {
        IssueSet {
            list,
            capped: false,
            error,
            read_at: Instant::now(),
        }
    }

    /// The inspector of a repository at `/r` on this Mac, its status read,
    /// drawn with `read` as its issues, loaded.
    fn drawn(
        inspector: &mut Inspector,
        read: &HashMap<RepoKey, IssueSet>,
        asked: bool,
    ) -> Vec<GlyphInstance> {
        let loaded = HashSet::from([(None, PathBuf::from("/r"))]);
        drawn_loaded(inspector, read, asked, &loaded)
    }

    /// [`drawn`], with the repositories in `loaded` the ones whose issues
    /// were asked for.
    fn drawn_loaded(
        inspector: &mut Inspector,
        read: &HashMap<RepoKey, IssueSet>,
        asked: bool,
        loaded: &HashSet<RepoKey>,
    ) -> Vec<GlyphInstance> {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let workspace = Workspace {
            name: "garden-log".into(),
            path: PathBuf::from("/r"),
            branch: Some("main".into()),
            main: true,
            folder: false,
            key: String::new(),
            repo: None,
            base: None,
            commit: None,
            merged: false,
            host: None,
            github: None,
        };
        inspector.status = vec![(
            PathBuf::from("/r"),
            Ok(Status {
                branch: Some("main".into()),
                ..Default::default()
            }),
        )];
        let mut out = Vec::new();
        inspector.draw(
            Some(&workspace),
            Some(&[PathBuf::from("/r")]),
            &[],
            None,
            None,
            true,
            Issues {
                read,
                asked,
                loaded,
                // Twelve days after the issues were opened.
                now: 1_790_856_000 + 12 * 86_400,
            },
            &HashSet::new(),
            &mut atlas,
            Viewport {
                x: 0.0,
                y: 0.0,
                width: WIDTH,
                height: 4000.0,
            },
            &Theme::dark(),
            &mut out,
        );
        out
    }

    fn rows(inspector: &Inspector) -> Vec<Viewport> {
        inspector
            .hits
            .iter()
            .filter(|(_, a, _)| matches!(a, Action::OpenIssue(_)))
            .map(|(rect, ..)| *rect)
            .collect()
    }

    fn report(inspector: &Inspector) -> String {
        inspector.report(Some(&[PathBuf::from("/r")]))
    }

    #[test]
    fn a_repositorys_issues_show_ten_until_show_all() {
        let key: RepoKey = (None, PathBuf::from("/r"));
        let read = HashMap::from([(key, set((1..=12).map(issue).collect(), None))]);
        let mut inspector = Inspector::default();
        drawn(&mut inspector, &read, true);
        let shown = rows(&inspector);
        assert_eq!(shown.len(), 10);
        assert!(report(&inspector).ends_with(" issues=12:rows=10"));
        // A row each, inside the column, one under the other.
        for pair in shown.windows(2) {
            assert!(pair[1].y >= pair[0].y + pair[0].height);
        }
        assert!(
            shown
                .iter()
                .all(|r| r.x >= PAD && r.x + r.width <= WIDTH - PAD)
        );
        // The rows are named by their place; the first is #1.
        assert_eq!(inspector.named("inspector.issue_open.0"), Some(shown[0]));
        assert_eq!(
            inspector.hit(shown[0].x + 1.0, shown[0].y + 1.0),
            Some((Action::OpenIssue(1), Some(PathBuf::from("/r"))))
        );
        // Show all: all twelve, then Show fewer folds them again.
        let toggle = inspector.named("inspector.issues_toggle").unwrap();
        let (action, repo) = inspector.hit(toggle.x + 1.0, toggle.y + 1.0).unwrap();
        assert_eq!(action, Action::ToggleIssues);
        inspector.toggle_issues(&repo.unwrap());
        drawn(&mut inspector, &read, true);
        assert_eq!(rows(&inspector).len(), 12);
        assert!(report(&inspector).ends_with(" issues=12:rows=12"));
        assert!(inspector.named("inspector.issue_open.11").is_some());
        inspector.toggle_issues(Path::new("/r"));
        drawn(&mut inspector, &read, true);
        assert_eq!(rows(&inspector).len(), 10);
        // The issues are not among the card's buttons.
        assert!(!report(&inspector).contains("issue_open"));
    }

    #[test]
    fn an_issue_list_says_why_it_is_missing_or_old() {
        let key: RepoKey = (None, PathBuf::from("/r"));
        let mut inspector = Inspector::default();
        let says = |inspector: &mut Inspector, read: HashMap<RepoKey, IssueSet>, asked| {
            drawn(inspector, &read, asked);
            (rows(inspector).len(), report(inspector))
        };
        // Not read yet: skeletons, while `gh` is asked.
        let (n, said) = says(&mut inspector, HashMap::new(), true);
        assert_eq!(n, 0);
        assert!(said.ends_with(" issues=loading"), "{said}");
        // `gh` not asked (missing, or a test instance): nothing.
        let (_, said) = says(&mut inspector, HashMap::new(), false);
        assert!(!said.contains("issues="), "{said}");
        let one = |list, error| HashMap::from([(key.clone(), set(list, error))]);
        let (n, said) = says(
            &mut inspector,
            one(Vec::new(), Some(IssueError::NeedsAuth)),
            true,
        );
        assert_eq!(n, 0);
        assert!(said.ends_with(" issues=auth"), "{said}");
        let (_, said) = says(
            &mut inspector,
            one(Vec::new(), Some(IssueError::NotGitHub)),
            true,
        );
        assert!(!said.contains("issues="), "{said}");
        let (_, said) = says(&mut inspector, one(Vec::new(), None), true);
        assert!(said.ends_with(" issues=0"), "{said}");
        // A failed read keeps the list before, and says so.
        let offline = IssueError::Other("error connecting to api.github.com".into());
        let (n, said) = says(
            &mut inspector,
            one(vec![issue(3), issue(4)], Some(offline)),
            true,
        );
        assert_eq!(n, 2);
        assert!(said.ends_with(" issues=2:rows=2:stale"), "{said}");
    }

    #[test]
    fn a_worktrees_issues_are_its_repositorys() {
        let mut inspector = Inspector::default();
        let read = HashMap::from([((None, PathBuf::from("/main")), set(vec![issue(7)], None))]);
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let workspace = Workspace {
            name: "feat".into(),
            path: PathBuf::from("/w/feat"),
            branch: Some("feat".into()),
            main: false,
            folder: false,
            key: String::new(),
            repo: Some(PathBuf::from("/main")),
            base: Some("main".into()),
            commit: None,
            merged: false,
            host: None,
            github: None,
        };
        inspector.status = vec![(PathBuf::from("/w/feat"), Ok(Status::default()))];
        inspector.draw(
            Some(&workspace),
            Some(&[PathBuf::from("/w/feat")]),
            &[],
            None,
            None,
            true,
            Issues {
                read: &read,
                asked: true,
                loaded: &HashSet::from([(None, PathBuf::from("/main"))]),
                now: 0,
            },
            &HashSet::new(),
            &mut atlas,
            Viewport {
                x: 0.0,
                y: 0.0,
                width: WIDTH,
                height: 4000.0,
            },
            &Theme::dark(),
            &mut Vec::new(),
        );
        let row = rows(&inspector)[0];
        assert_eq!(
            inspector.hit(row.x + 1.0, row.y + 1.0),
            Some((Action::OpenIssue(7), Some(PathBuf::from("/main"))))
        );
        assert!(
            inspector
                .report(Some(&[PathBuf::from("/w/feat")]))
                .ends_with(" issues=1:rows=1")
        );
    }

    #[test]
    fn issues_wait_for_load_issues_and_hide_again() {
        let key: RepoKey = (None, PathBuf::from("/r"));
        let read = HashMap::from([(key.clone(), set(vec![issue(1)], None))]);
        let mut inspector = Inspector::default();
        // Not loaded: Load Issues and nothing else, even with a list in hand.
        drawn_loaded(&mut inspector, &HashMap::new(), true, &HashSet::new());
        assert!(report(&inspector).ends_with(" issues=unloaded"));
        assert!(rows(&inspector).is_empty());
        let load = inspector.named("inspector.issues_load").unwrap();
        assert_eq!(
            inspector.hit(load.x + 1.0, load.y + 1.0),
            Some((Action::LoadIssues, Some(PathBuf::from("/r"))))
        );
        assert!(inspector.named("inspector.issues_hide").is_none());
        // `gh` not asked: no button either.
        drawn_loaded(&mut inspector, &HashMap::new(), false, &HashSet::new());
        assert!(inspector.named("inspector.issues_load").is_none());
        // Loaded: the list, and the control that hides it again.
        drawn_loaded(&mut inspector, &read, true, &HashSet::from([key]));
        assert!(report(&inspector).ends_with(" issues=1:rows=1"));
        assert!(inspector.named("inspector.issues_load").is_none());
        let hide = inspector.named("inspector.issues_hide").unwrap();
        assert_eq!(
            inspector.hit(hide.x + 1.0, hide.y + 1.0),
            Some((Action::HideIssues, Some(PathBuf::from("/r"))))
        );
        // Neither is one of the card's buttons.
        assert!(!report(&inspector).contains("issues_"));
    }

    #[test]
    fn a_read_saying_the_same_is_taken_without_a_redraw() {
        let mut inspector = Inspector::default();
        let status = |dirty| Status {
            branch: Some("main".into()),
            dirty,
            ..Status::default()
        };
        let mut land = |read: Vec<Read>| {
            let (tx, rx) = mpsc::channel();
            tx.send((None, read)).unwrap();
            inspector.rx = Some(rx);
            inspector.poll()
        };
        let path = PathBuf::from("/desk/kite");
        assert!(land(vec![(path.clone(), Ok(status(0)))]));
        assert!(!land(vec![(path.clone(), Ok(status(0)))]));
        assert!(land(vec![(path.clone(), Ok(status(2)))]));
        assert!(land(vec![(path, Err("gone".into()))]));
        // Written to: read again once the last read is a few seconds old,
        // not at once; a folder it does not show is not its business.
        assert!(!inspector.touch(Path::new("/desk/other")));
        assert!(inspector.touch(Path::new("/desk/kite")));
        let wanted = vec![(PathBuf::from("/desk/kite"), None, false)];
        inspector.want(None, &wanted);
        assert!(!inspector.busy());
        inspector.read_at = Some(Instant::now() - TOUCHED_WAIT);
        inspector.want(None, &wanted);
        assert!(inspector.busy());
    }

    #[test]
    fn a_folded_repository_is_one_row_from_its_last_read() {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let workspace = Workspace {
            name: "desk".into(),
            path: PathBuf::from("/desk"),
            branch: None,
            main: false,
            folder: true,
            key: "ws:1".into(),
            repo: None,
            base: None,
            commit: None,
            merged: false,
            host: None,
            github: None,
        };
        let repos = [PathBuf::from("/desk/alpha"), PathBuf::from("/desk/beta")];
        let mut inspector = Inspector::default();
        let (tx, rx) = mpsc::channel();
        let dirty = Status {
            branch: Some("seedlings".into()),
            dirty: 2,
            ..Default::default()
        };
        tx.send((
            None,
            vec![
                (repos[0].clone(), Ok(dirty)),
                (repos[1].clone(), Ok(Status::default())),
            ],
        ))
        .unwrap();
        inspector.rx = Some(rx);
        assert!(inspector.poll());
        let mut draw = |inspector: &mut Inspector, folded: &HashSet<RepoKey>| {
            inspector.draw(
                Some(&workspace),
                Some(&repos),
                &[],
                None,
                None,
                true,
                Issues {
                    read: &HashMap::new(),
                    asked: true,
                    loaded: &HashSet::new(),
                    now: 0,
                },
                folded,
                &mut atlas,
                Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: WIDTH,
                    height: 4000.0,
                },
                &Theme::dark(),
                &mut Vec::new(),
            );
        };
        draw(&mut inspector, &HashSet::new());
        let open_load = inspector.named("inspector.issues_load.1").unwrap();
        // Alpha folded: its name row alone, beta's card under it moves up.
        let folded = HashSet::from([(None, repos[0].clone())]);
        // Its read from before stays; the reads it gets stop.
        inspector.status.retain(|(p, _)| *p != repos[0]);
        draw(&mut inspector, &folded);
        let said = inspector.report(Some(&repos));
        assert!(
            said.starts_with("alpha: folded reads=0 | beta: branch="),
            "{said}"
        );
        let fold = inspector.named("inspector.repo_fold.0").unwrap();
        assert_eq!(
            inspector.hit(fold.x + 1.0, fold.y + 1.0),
            Some((Action::FoldRepo, Some(repos[0].clone())))
        );
        // One Load Issues now, beta's, higher than it was.
        assert!(inspector.named("inspector.issues_load.1").is_none());
        let load = inspector.named("inspector.issues_load").unwrap();
        assert!(load.y < open_load.y);
        assert_eq!(
            inspector.known.get(&(None, repos[0].clone())),
            Some(&(Some("seedlings".to_owned()), 2))
        );
    }

    /// `fit` as it was, measuring every shorter cut in turn: the oracle.
    fn fit_by_measuring(atlas: &mut Atlas, s: &str, width: f32) -> String {
        let width = width + 0.5;
        if layout::ui_text_width(atlas, s) <= width {
            return s.to_owned();
        }
        let chars: Vec<char> = s.chars().collect();
        let mut n = chars.len();
        while n > 0 {
            n -= 1;
            let cut: String = chars[..n].iter().collect::<String>() + "\u{2026}";
            if layout::ui_text_width(atlas, &cut) <= width {
                return cut;
            }
        }
        String::new()
    }

    /// `fit_start` as it was: the oracle.
    fn fit_start_by_measuring(atlas: &mut Atlas, s: &str, width: f32) -> String {
        let width = width + 0.5;
        if layout::ui_text_width(atlas, s) <= width {
            return s.to_owned();
        }
        let chars: Vec<char> = s.chars().collect();
        for n in 1..chars.len() {
            let cut: String = "\u{2026}".to_owned() + &chars[n..].iter().collect::<String>();
            if layout::ui_text_width(atlas, &cut) <= width {
                return cut;
            }
        }
        String::new()
    }

    const SAMPLES: [&str; 12] = [
        "Seed tray 12 drains too slowly after a long watering",
        "/srv/garden/seedling/src/beds/north/tray-watering-schedule.rs",
        "trellis: the harvest log loses the weights it was given after a long dry week, \
         seen again on bed 13, and again on bed 40 when the frost came early that year",
        "\u{82d7}\u{5e8a}\u{306e}\u{6c34}\u{306f}\u{3051}\u{304c}\u{9045}\u{3059}\u{304e}\
         \u{308b}\u{554f}\u{984c}\u{3092}\u{9577}\u{3044}\u{4e7e}\u{71e5}\u{306e}\u{5f8c}",
        "Harvest \u{1f345}\u{1f955}\u{1f33d} log \u{1f469}\u{200d}\u{1f33e} and \
         \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467} flags \u{1f1f5}\u{1f1f9}\u{1f1e7}\u{1f1f7} \
         thumbs \u{1f44d}\u{1f3fd} sun \u{2600}\u{fe0f} done",
        "Cafe\u{301} compost tempe\u{301}rature nai\u{308}ve reads stay at zero \
         a\u{30a}\u{301}\u{323} stacked",
        "office affluent flowers ffl ligatures in the trellis report",
        "\u{915}\u{94d}\u{937}\u{947}\u{924}\u{94d}\u{930} \u{938}\u{93f}\u{902}\u{91a}\u{93e}\
         \u{908} \u{915}\u{93e} \u{938}\u{92e}\u{92f}",
        "mulch \u{5d2}\u{5df}\u{20}\u{5d4}\u{5d9}\u{5e8}\u{5e7} bed \u{633}\u{62c}\u{644} \
         \u{627}\u{644}\u{62d}\u{635}\u{627}\u{62f} twelve",
        "x",
        "",
        "AVAVAVA WAWAWA To Ty Yo kerning pairs and more text after them",
    ];

    /// Every width a cut of `s` lands exactly on, a hair either side, and a
    /// coarse sweep.
    fn widths(atlas: &mut Atlas, s: &str) -> Vec<f32> {
        let chars: Vec<char> = s.chars().collect();
        let mut out: Vec<f32> = (-2..60).map(|n| n as f32 * 5.0).collect();
        for n in 0..=chars.len() {
            let head: String = chars[..n].iter().collect::<String>() + "\u{2026}";
            let tail: String = "\u{2026}".to_owned() + &chars[n..].iter().collect::<String>();
            for cut in [head, tail] {
                let w = layout::ui_text_width(atlas, &cut);
                out.extend([w - 0.5, w - 0.501, w - 0.499]);
            }
        }
        out
    }

    #[test]
    fn fit_cuts_where_measuring_every_cut_did() {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        // Past the 120 characters a UI width counts, and a sidebar line.
        let long = [
            "\u{82d7}\u{5e8a}\u{306e}\u{6c34}".repeat(34),
            format!("/srv/garden/{}/tray.rs", "north-bed/".repeat(14)),
            "4 terminals \u{b7} 2 ahead \u{b7} garden-watering-schedule-rework".to_owned(),
        ];
        for s in SAMPLES.into_iter().chain(long.iter().map(String::as_str)) {
            for width in widths(&mut atlas, s) {
                atlas.begin_frame();
                assert_eq!(
                    fit(&mut atlas, s, width),
                    fit_by_measuring(&mut atlas, s, width),
                    "fit {s:?} into {width}"
                );
                assert_eq!(
                    fit_start(&mut atlas, s, width),
                    fit_start_by_measuring(&mut atlas, s, width),
                    "fit_start {s:?} into {width}"
                );
            }
        }
    }

    #[test]
    fn fit_shapes_a_label_a_few_times_not_once_a_character() {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let title = SAMPLES[2];
        let before = atlas.ui_shapes();
        let cut = fit(&mut atlas, title, 180.0);
        assert!(
            cut.ends_with('\u{2026}') && cut.len() < title.len(),
            "{cut}"
        );
        assert!(
            atlas.ui_shapes() - before <= 4,
            "{}",
            atlas.ui_shapes() - before
        );
        let before = atlas.ui_shapes();
        let cut = fit_start(&mut atlas, SAMPLES[1], 120.0);
        assert!(cut.starts_with('\u{2026}'), "{cut}");
        assert!(
            atlas.ui_shapes() - before <= 4,
            "{}",
            atlas.ui_shapes() - before
        );
    }

    /// A repository at `/r` on this Mac with `status`, drawn.
    fn drawn_status(inspector: &mut Inspector, status: Status) {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let workspace = Workspace {
            name: "orchard".into(),
            path: PathBuf::from("/r"),
            branch: status.branch.clone(),
            main: true,
            folder: false,
            key: String::new(),
            repo: None,
            base: None,
            commit: None,
            merged: false,
            host: None,
            github: None,
        };
        inspector.status = vec![(PathBuf::from("/r"), Ok(status))];
        inspector.read_for = vec![PathBuf::from("/r")];
        let mut out = Vec::new();
        inspector.draw(
            Some(&workspace),
            Some(&[PathBuf::from("/r")]),
            &[],
            None,
            None,
            true,
            Issues {
                read: &HashMap::new(),
                asked: false,
                loaded: &HashSet::new(),
                now: 0,
            },
            &HashSet::new(),
            &mut atlas,
            Viewport {
                x: 0.0,
                y: 0.0,
                width: WIDTH,
                height: 4000.0,
            },
            &Theme::dark(),
            &mut out,
        );
    }

    fn live(inspector: &Inspector) -> Vec<Action> {
        inspector.hits.iter().map(|(_, a, _)| *a).collect()
    }

    #[test]
    fn the_remote_row_says_whether_and_when_the_remote_was_checked() {
        let words = |status: &Status, check: Option<&Check>| -> Vec<(String, Ink)> {
            remote_values(status, check, 1_790_000_000)
        };
        let unpushed = Status::default();
        assert_eq!(
            words(&unpushed, None),
            vec![("not pushed".into(), Ink::Faint)]
        );
        let even = Status {
            upstream: Some("origin/main".into()),
            ..Default::default()
        };
        assert_eq!(
            words(&even, None),
            vec![
                ("up to date".into(), Ink::Text),
                ("not checked".into(), Ink::Faint)
            ]
        );
        let (_tx, rx) = mpsc::channel();
        assert_eq!(
            words(&even, Some(&Check::Running(rx)))[1],
            ("checking\u{2026}".into(), Ink::Faint)
        );
        let at = 1_790_000_000 - 120;
        assert_eq!(
            words(&even, Some(&Check::Done(at)))[1],
            (
                format!(
                    "checked {}",
                    crate::platform::clock_time(at as i64, 1_790_000_000)
                ),
                Ink::Faint
            )
        );
        let both = Status {
            upstream: Some("origin/main".into()),
            to_push: 2,
            to_pull: 1,
            ..Default::default()
        };
        assert_eq!(
            words(&both, Some(&Check::Failed("fatal: no".into()))),
            vec![
                ("2 to push, 1 to pull".into(), Ink::Text),
                ("check failed".into(), Ink::Removed)
            ]
        );
    }

    #[test]
    fn pull_and_check_are_live_with_an_upstream_and_wait_on_a_check() {
        let mut inspector = Inspector::default();
        drawn_status(
            &mut inspector,
            Status {
                branch: Some("main".into()),
                ..Default::default()
            },
        );
        assert!(!live(&inspector).contains(&Action::CheckRemote));
        assert!(!live(&inspector).contains(&Action::Pull));
        assert!(!report(&inspector).contains("check="));
        let even = Status {
            branch: Some("main".into()),
            upstream: Some("origin/main".into()),
            ..Default::default()
        };
        drawn_status(&mut inspector, even.clone());
        // Up to date as of the last fetch only: Pull fetches itself.
        assert!(live(&inspector).contains(&Action::Pull));
        assert!(live(&inspector).contains(&Action::CheckRemote));
        assert!(
            report(&inspector).ends_with("tag=- check=none buttons=pull"),
            "{}",
            report(&inspector)
        );
        assert!(inspector.named("inspector.check").is_some());
        let (_tx, rx) = mpsc::channel();
        inspector
            .checks
            .insert((None, PathBuf::from("/r")), Check::Running(rx));
        drawn_status(&mut inspector, even.clone());
        assert!(!live(&inspector).contains(&Action::Pull));
        assert!(!live(&inspector).contains(&Action::CheckRemote));
        assert!(report(&inspector).ends_with("check=running buttons="));
        inspector.checks.insert(
            (None, PathBuf::from("/r")),
            Check::Failed("fatal: no".into()),
        );
        drawn_status(&mut inspector, even);
        assert!(report(&inspector).ends_with("check=err buttons=pull"));
    }

    /// Git in `dir`, never signing, failing the test when it fails.
    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=Robin Example",
                "-c",
                "user.email=r@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    /// A bare origin with one commit on main, and two clones of it: the
    /// folder they are in, `mine`, and `seed`, which pushed that commit.
    fn origin_and_clone(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("crc-check-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        git(
            &root,
            &["init", "-q", "--bare", "-b", "main", "orchard.git"],
        );
        let seed = root.join("seed");
        git(&root, &["clone", "-q", "orchard.git", "seed"]);
        std::fs::write(seed.join("trees.txt"), "apple\n").unwrap();
        git(&seed, &["add", "trees.txt"]);
        git(&seed, &["commit", "-q", "-m", "plant apple"]);
        git(&seed, &["push", "-q", "origin", "HEAD:main"]);
        git(&root, &["clone", "-q", "orchard.git", "mine"]);
        (root.clone(), root.join("mine"), seed)
    }

    /// Polls until no Check runs and no status read is on its way.
    fn settle(inspector: &mut Inspector) {
        let started = Instant::now();
        while inspector.checking_any() || inspector.busy() {
            assert!(started.elapsed() < Duration::from_secs(60), "never settled");
            inspector.poll();
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The status of `dir` on this Mac, read the way the panel reads it.
    fn read_status(inspector: &mut Inspector, dir: &Path) -> Status {
        inspector.want(None, &[(dir.to_path_buf(), None, false)]);
        settle(inspector);
        inspector.status_of(dir).unwrap().clone().unwrap()
    }

    #[test]
    fn a_check_fetches_so_the_status_has_what_the_remote_has() {
        let (root, mine, seed) = origin_and_clone("fetch");
        let mut inspector = Inspector::default();
        assert_eq!(read_status(&mut inspector, &mine).to_pull, 0);
        // Someone else pushes: until a fetch, the status cannot know.
        std::fs::write(seed.join("trees.txt"), "apple\npear\n").unwrap();
        git(&seed, &["commit", "-q", "-am", "plant pear"]);
        git(&seed, &["push", "-q", "origin", "HEAD:main"]);
        inspector.invalidate();
        assert_eq!(read_status(&mut inspector, &mine).to_pull, 0);
        let key: RepoKey = (None, mine.clone());
        inspector.check_remote(key.clone(), None);
        assert!(inspector.checking(&key));
        settle(&mut inspector);
        assert!(
            matches!(inspector.check_of(&key), Some(Check::Done(_))),
            "{:?}",
            match inspector.check_of(&key) {
                Some(Check::Failed(e)) => e.as_str(),
                _ => "",
            }
        );
        // Its status is wanted again (the panel asks at once): it has
        // what came in.
        assert!(inspector.read_at.is_none());
        assert_eq!(read_status(&mut inspector, &mine).to_pull, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_check_of_a_bad_remote_fails_with_gits_message() {
        let (root, mine, _) = origin_and_clone("bad");
        let missing = root.join("no-such-orchard.git");
        git(
            &mine,
            &["remote", "set-url", "origin", &missing.to_string_lossy()],
        );
        let mut inspector = Inspector::default();
        let key: RepoKey = (None, mine.clone());
        inspector.check_remote(key.clone(), None);
        settle(&mut inspector);
        match inspector.check_of(&key) {
            Some(Check::Failed(e)) => assert!(e.contains("no-such-orchard"), "{e}"),
            _ => panic!("the check did not fail"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_second_check_while_one_runs_is_not_started() {
        let (root, mine, _) = origin_and_clone("twice");
        // An ssh that counts how often it is started, then fails slowly.
        let count = root.join("started");
        git(
            &mine,
            &[
                "remote",
                "set-url",
                "origin",
                "ssh://orchard.invalid/orchard.git",
            ],
        );
        let ssh = format!("echo x >> '{}'; sleep 1; exit 1 #", count.display());
        git(&mine, &["config", "core.sshCommand", &ssh]);
        // Plain ssh: Git does not start it first to ask which it is.
        git(&mine, &["config", "ssh.variant", "ssh"]);
        let mut inspector = Inspector::default();
        let key: RepoKey = (None, mine.clone());
        inspector.check_remote(key.clone(), None);
        inspector.check_remote(key.clone(), None);
        assert!(inspector.checking(&key));
        settle(&mut inspector);
        assert!(matches!(inspector.check_of(&key), Some(Check::Failed(_))));
        let started = std::fs::read_to_string(&count).unwrap_or_default();
        assert_eq!(started.lines().count(), 1, "{started:?}");
        // Another repository's Check is its own.
        assert!(!inspector.checking(&(None, root.join("seed"))));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_header_refresh_checks_every_repo_with_an_upstream() {
        let root = std::env::temp_dir().join(format!("crc-check-all-{}", std::process::id()));
        let tracked = Status {
            branch: Some("main".into()),
            upstream: Some("origin/main".into()),
            ..Default::default()
        };
        let [orchard, vineyard, meadow, unread] =
            ["orchard", "vineyard", "meadow", "unread"].map(|n| root.join(n));
        let mut inspector = Inspector {
            status: vec![
                (orchard.clone(), Ok(tracked.clone())),
                (
                    vineyard.clone(),
                    Ok(Status {
                        branch: Some("main".into()),
                        ..Default::default()
                    }),
                ),
                (meadow.clone(), Ok(tracked.clone())),
            ],
            ..Default::default()
        };
        // Meadow is checking already: its Check is not started again.
        let (meadow_tx, meadow_rx) = mpsc::channel();
        inspector
            .checks
            .insert((None, meadow.clone()), Check::Running(meadow_rx));
        let repos = [orchard.clone(), vineyard.clone(), meadow.clone(), unread];
        // Statuses read for this Mac are not another host's.
        assert_eq!(inspector.check_all(Some("box"), &repos, None), 0);
        assert!(!inspector.checking_all());
        assert_eq!(inspector.check_all(None, &repos, None), 1);
        assert!(inspector.checking(&(None, orchard.clone())));
        assert!(inspector.check_of(&(None, vineyard.clone())).is_none());
        assert!(inspector.checking_all());
        // The Check meadow had is the one still running.
        meadow_tx.send(Ok(String::new())).unwrap();
        settle(&mut inspector);
        assert!(matches!(
            inspector.check_of(&(None, meadow)),
            Some(Check::Done(_))
        ));
        // Orchard is no repository, so its fetch fails, on its own.
        assert!(matches!(
            inspector.check_of(&(None, orchard)),
            Some(Check::Failed(_))
        ));
        assert!(!inspector.checking_all());
    }
}
