//! The workspace inspector: a column at the right of Agents mode that says
//! where each repository of the selected workspace stands (its branch
//! against main and its upstream, its uncommitted work), with the buttons to
//! commit, pull, push and take main's new commits; for a worktree, its pull
//! request with each check, and the buttons to open or merge it. A folder
//! workspace shows every repository in it: the folder itself and its direct
//! children, each with a strip per worktree of it: where its branch stands
//! against the base, and buttons to open it or start a shell in it. The Git
//! status is read on a worker; the pull request comes with the workspace
//! list's own read.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::platform::agents::{Pace, Workspace};
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
/// A status read this old is read again when the panel is drawn.
const STALE: Duration = Duration::from_secs(15);

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
        }
    }
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
}

impl Inspector {
    /// Reads each repository's status on a worker, unless fresh ones are in
    /// hand or on their way: on `host` over ssh, a call per repository, or
    /// on this Mac for `None`.
    pub fn want(&mut self, host: Option<&str>, wanted: &[Want]) {
        let paths: Vec<PathBuf> = wanted.iter().map(|w| w.0.clone()).collect();
        let host = host.map(str::to_owned);
        let same = self.read_for == paths && self.read_host == host;
        let have = same && self.read_at.is_some_and(|at| at.elapsed() < STALE);
        let key = (host.clone(), paths.clone());
        if have || self.reading.as_ref() == Some(&key) {
            return;
        }
        // Another machine's paths are other folders, even named the same.
        if self.read_host != host {
            self.status.clear();
            self.read_for.clear();
            self.read_host.clone_from(&host);
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
        if !same {
            self.scroll = 0.0;
        }
        self.rx = Some(rx);
        self.reading = Some(key);
    }

    /// The next read reads again, now.
    pub fn invalidate(&mut self) {
        self.read_at = None;
    }

    /// Takes a finished read. Returns whether anything changed.
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.rx else {
            return false;
        };
        match rx.try_recv() {
            Ok((host, read)) => {
                // A read for a workspace no longer selected still counts:
                // the next `want` reads the one that is.
                self.read_for = read.iter().map(|(p, _)| p.clone()).collect();
                self.read_host = host;
                self.status = read;
                self.read_at = Some(Instant::now());
            }
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => {}
        }
        self.rx = None;
        self.reading = None;
        true
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
        // A worktree strip's button by the strip's place: `.1` the second.
        if rest.starts_with("worktree_") {
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
            return at.unwrap_or_else(|| "-".into());
        };
        if repos.is_empty() {
            return at.unwrap_or_else(|| "none".into());
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
            let live: Vec<&str> = self
                .hits
                .iter()
                .filter(|(_, a, r)| {
                    !matches!(a, Action::Close | Action::Refresh)
                        && r.as_deref().is_none_or(|r| r == path)
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
            if strips.is_empty() {
                format!("{status} buttons={}", live.join(","))
            } else {
                format!(
                    "{status} buttons={} worktrees={}",
                    live.join(","),
                    strips.join(",")
                )
            }
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
    /// doing, and whether it is merged.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        workspace: Option<&Workspace>,
        repos: Option<&[PathBuf]>,
        worktrees: &[(Workspace, String, Pace, bool)],
        remote: Option<&crate::platform::remote::Remote>,
        pull_request: Option<Option<&PullRequest>>,
        github: bool,
        atlas: &mut Atlas,
        rect: Viewport,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) {
        self.hits.clear();
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
        for (button, icon, tip, action) in [
            (refresh, icons::REFRESH, "Read Again", Action::Refresh),
            (
                close,
                icons::CLOSE,
                "Hide Inspector  \u{2325}\u{2318}I",
                Action::Close,
            ),
        ] {
            layout::Button::new(button)
                .icon(icon)
                .tone(layout::Tone::Ghost)
                .tip(tip)
                .draw(out, atlas, theme);
            self.hits.push((button, action, None));
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
        let shown = match &workspace.host {
            Some(host) => format!("{host}:{}", workspace.path.display()),
            None => crate::platform::window::shown_path(&workspace.path),
        };
        let shown = fit(atlas, &shown, width);
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
        let Some(repos) = repos else {
            skeleton(out, y, 4);
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
        // A folder workspace with repositories in it names each over its
        // card; a worktree, or a folder that is itself the only one, needs
        // no name.
        let named = repos.len() > 1 || repos[0] != workspace.path;
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
                let label = fit(atlas, &label, width);
                layout::push_title(out, atlas, x, y + 4.0, 13.0, &label, theme.text, width);
                y += ROW;
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
            // A host's worktree is not reviewed or removed from here.
            let buttons = match (merged, tree.host.is_some()) {
                (_, true) => vec![terminal],
                (true, false) => vec![
                    (
                        "Clean up",
                        Action::CleanUpWorktree,
                        "Remove this merged worktree and its branch".to_owned(),
                    ),
                    ("Review", Action::ReviewWorktree, review_tip),
                ],
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
        facts.push((
            "Remote",
            vec![match &status.upstream {
                None => ("not pushed".into(), theme.gutter_text),
                Some(_) if status.to_push == 0 && status.to_pull == 0 => {
                    ("up to date".into(), theme.text)
                }
                Some(_) => {
                    let mut parts = Vec::new();
                    if status.to_push > 0 {
                        parts.push(format!("{} to push", status.to_push));
                    }
                    if status.to_pull > 0 {
                        parts.push(format!("{} to pull", status.to_pull));
                    }
                    (parts.join(", "), theme.text)
                }
            }],
        ));
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
        let y = card(out, atlas, x, y, width, &facts, theme) + 10.0;

        // What can be done with it.
        let base = status.base.clone().unwrap_or_else(|| "main".into());
        let update = format!("Merge {base} In");
        let buttons = [
            (
                "Commit All\u{2026}",
                Action::CommitAll,
                status.dirty > 0,
                "Stage every change and commit it",
            ),
            (
                update.as_str(),
                Action::UpdateFromBase,
                status.base.is_some() && status.behind > 0,
                "Take the base branch's new commits: a merge, never a rebase",
            ),
            (
                "Pull",
                Action::Pull,
                status.upstream.is_some() && status.to_pull > 0,
                "Fast-forward from the upstream",
            ),
            (
                "Push",
                Action::Push,
                status.branch.is_some() && (status.upstream.is_none() || status.to_push > 0),
                "Push the branch, setting its upstream the first time",
            ),
        ];
        // The main checkout has nothing to take main's commits from.
        let buttons: Vec<_> = buttons
            .iter()
            .filter(|b| b.1 != Action::UpdateFromBase || status.base.is_some())
            .collect();
        let half = ((width - layout::UI_GAP) * 0.5).floor();
        for (n, (label, action, enabled, tip)) in buttons.iter().enumerate() {
            let b = Viewport {
                x: x + (n % 2) as f32 * (half + layout::UI_GAP),
                y: y + (n / 2) as f32 * (layout::UI_CONTROL + layout::UI_GAP),
                width: half,
                height: layout::UI_CONTROL,
            };
            layout::Button::new(b)
                .label(label)
                .enabled(*enabled)
                .tip(tip)
                .draw(out, atlas, theme);
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
