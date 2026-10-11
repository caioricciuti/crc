//! The workspace inspector: a column at the right of Agents mode that says
//! where the selected workspace's branch stands (against main, against its
//! upstream, its uncommitted work) and its pull request with each check,
//! with the buttons to commit, pull, push, take main's new commits, and
//! open or merge the pull request. Its Git status is read on a worker; the
//! pull request comes with the workspace list's own read.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::platform::agents::Workspace;
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
const ROW: f32 = 26.0;
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
        }
    }
}

type Read = (PathBuf, Result<Status, String>);

/// A card row: its label, and its values with their colours.
type Fact<'a> = (&'a str, Vec<(String, [f32; 4])>);

#[derive(Default)]
pub struct Inspector {
    /// Closed by its button or View > Workspace Inspector; open otherwise.
    pub hidden: bool,
    /// The last status read, and the folder it is for.
    pub status: Option<Read>,
    read_at: Option<Instant>,
    rx: Option<mpsc::Receiver<Read>>,
    /// The folder being read.
    reading: Option<PathBuf>,
    pub hits: Vec<(Viewport, Action)>,
}

impl Inspector {
    /// Reads `path`'s status on a worker, unless a fresh one is in hand or
    /// on its way. `base` is the main checkout's branch; `main` asks for
    /// the latest tag too.
    pub fn want(&mut self, path: &Path, base: Option<String>, main: bool) {
        let have = self.status.as_ref().is_some_and(|(p, _)| p == path)
            && self.read_at.is_some_and(|at| at.elapsed() < STALE);
        if have || self.reading.as_deref() == Some(path) {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let dir = path.to_path_buf();
        std::thread::spawn(move || {
            let status = crate::project::worktree::status(&dir, base.as_deref(), main);
            let _ = tx.send((dir, status));
        });
        self.rx = Some(rx);
        self.reading = Some(path.to_path_buf());
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
            Ok(read) => {
                // A read for a workspace no longer selected still counts:
                // the next `want` reads the one that is.
                self.status = Some(read);
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
        self.status
            .as_ref()
            .filter(|(p, _)| p == path)
            .map(|(_, s)| s)
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<Action> {
        self.hits
            .iter()
            .find(|(rect, _)| rect.contains(x, y))
            .map(|(_, a)| *a)
    }

    /// A button by the name a self-test script uses: `inspector.push`.
    pub fn named(&self, name: &str) -> Option<Viewport> {
        let rest = name.strip_prefix("inspector.")?;
        self.hits
            .iter()
            .find(|(_, a)| a.name() == rest)
            .map(|(rect, _)| *rect)
    }

    /// For the self-test: what the status says for `path`, and which
    /// buttons are live.
    pub fn report(&self, path: Option<&Path>) -> String {
        if self.hidden {
            return "hidden".into();
        }
        let Some(path) = path else {
            return "-".into();
        };
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
            .map(|(_, a)| a.name())
            .filter(|n| !matches!(*n, "close" | "refresh"))
            .collect();
        format!("{status} buttons={}", live.join(","))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        workspace: Option<&Workspace>,
        pull_request: Option<Option<&PullRequest>>,
        github: bool,
        atlas: &mut Atlas,
        rect: Viewport,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) {
        self.hits.clear();
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
            self.hits.push((button, action));
        }
        let x = rect.x + PAD;
        let width = (rect.width - 2.0 * PAD).max(0.0);
        let bottom = rect.y + rect.height - PAD;
        let mut y = rect.y + layout::SIDEBAR_HEADER_HEIGHT;
        let Some(workspace) = workspace else {
            note(out, atlas, x, y, width, "Select a workspace", theme);
            return;
        };
        let name = workspace.name();
        let name = fit(atlas, &name, width);
        layout::push_title(out, atlas, x, y, 15.0, &name, theme.text, width);
        y += 24.0;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let shown = match home
            .as_deref()
            .and_then(|h| workspace.path.strip_prefix(h).ok())
        {
            Some(rest) => format!("~/{}", rest.display()),
            None => workspace.path.display().to_string(),
        };
        let shown = fit(atlas, &shown, width);
        text(out, atlas, x, y, width, &shown, theme.gutter_text);
        y += ROW;
        let status = match self.status_of(&workspace.path) {
            None => {
                for n in 0..4 {
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
                return;
            }
            Some(Err(_)) => {
                note(out, atlas, x, y, width, "Not a Git repository", theme);
                return;
            }
            Some(Ok(status)) => status.clone(),
        };

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
        y = card(out, atlas, x, y, width, &facts, theme) + 10.0;

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
                self.hits.push((b, *action));
            }
        }
        y += buttons.len().div_ceil(2) as f32 * (layout::UI_CONTROL + layout::UI_GAP) + 14.0;

        // The pull request, for a branch other than the base.
        let Some(pull_request) = pull_request else {
            return;
        };
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
                self.hits.push((b, Action::CreatePullRequest));
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
            self.hits.push((merge, Action::MergePullRequest));
        }
        let view = half_rect(1);
        layout::Button::new(view)
            .label("Open on GitHub")
            .draw(out, atlas, theme);
        self.hits.push((view, Action::OpenPullRequest));
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
fn fit(atlas: &mut Atlas, s: &str, width: f32) -> String {
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
