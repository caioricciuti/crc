//! A GitHub issue in a tab of its own: read whole from the workspace
//! inspector, shown read-only as Markdown, with Start Task and Open on
//! GitHub in a strip above it.

use super::*;
use crate::platform::agents::{IssueAnswer, RepoKey};
use crate::project::worktree::Issue;

/// What an issue tab shows: which repository's issue, as last read.
pub(super) struct IssueTab {
    pub(super) repo: RepoKey,
    pub(super) issue: Issue,
}

/// The issue in the active tab, when it is one.
pub(super) fn active_issue(state: &State) -> Option<&IssueTab> {
    state.issue_tabs.get(&state.docs.active().id())
}

/// Whether Open on GitHub opens `url`: a page on GitHub, nothing else.
pub(super) fn on_github(url: &str) -> bool {
    url.starts_with("https://github.com/")
}

const START: &str = "Start Task";
const GITHUB: &str = "Open on GitHub";

/// Start Task and Open on GitHub, on the strip's second row.
pub(super) fn strip_buttons(atlas: &mut Atlas, strip: Viewport) -> [Viewport; 2] {
    let start = layout::strip_button(atlas, strip, strip.x + 16.0, START, 24.0);
    let github = layout::strip_button(
        atlas,
        strip,
        start.x + start.width + layout::UI_GAP,
        GITHUB,
        24.0,
    );
    [start, github]
}

/// Whether the issue is open, who opened it, its comments, and the two
/// buttons.
pub(super) fn draw_strip(
    tab: &IssueTab,
    atlas: &mut Atlas,
    strip: Viewport,
    theme: &Theme,
    out: &mut Vec<GlyphInstance>,
) {
    let issue = &tab.issue;
    let open = issue.state.eq_ignore_ascii_case("open");
    let (label, tone) = if open {
        ("Open", theme.diff_added)
    } else {
        ("Closed", theme.status_text)
    };
    let pill = layout::strip_pill(out, atlas, strip, label, tone);
    let mut facts = format!("Issue #{}", issue.number);
    if !issue.author.is_empty() {
        facts.push_str(&format!(" by @{}", issue.author));
    }
    facts.push_str(&match issue.comments.len() {
        1 => ", 1 comment".to_owned(),
        n => format!(", {n} comments"),
    });
    if !issue.labels.is_empty() {
        facts.push_str(&format!("   {}", issue.labels.join(", ")));
    }
    layout::strip_facts(out, atlas, strip, pill, &facts, theme);
    let [start, github] = strip_buttons(atlas, strip);
    Button::new(start)
        .label(START)
        .tone(Tone::Primary)
        .tip("A worktree on the issue's branch, with Claude in it")
        .draw(out, atlas, theme);
    Button::new(github)
        .label(GITHUB)
        .enabled(on_github(&issue.url))
        .draw(out, atlas, theme);
    layout::strip_hairline(out, atlas, strip, theme);
}

impl EditorView {
    /// An issue read whole: its tab, or why it could not be read.
    pub(super) fn take_issue(&self, repo: RepoKey, answer: IssueAnswer) {
        match answer {
            Ok(issue) => self.show_issue(repo, issue),
            Err(e) => {
                if let Some(mut state) = self.state_mut() {
                    state.say(layout::Feedback::Failure, e.message());
                }
                self.request_redraw();
            }
        }
    }

    /// Shows `issue` of `repo` in its tab: the one already open for it,
    /// with the text read again, or a new one. The editor has the tabs, so
    /// Agents mode makes way, as it does for a file an agent printed.
    fn show_issue(&self, repo: RepoKey, issue: Issue) {
        if self.state().is_some_and(|state| state.agents.on) {
            self.set_agents_mode(false);
        }
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let label = crate::platform::agents::issue_tab_label(issue.number, &issue.title);
            let text = crate::project::worktree::issue_markdown(&issue);
            let existing = state.docs.iter().position(|b| {
                state
                    .issue_tabs
                    .get(&b.id())
                    .is_some_and(|t| t.repo == repo && t.issue.number == issue.number)
            });
            match existing {
                Some(index) => {
                    state.docs.switch(index);
                    let tab = state.docs.active_mut();
                    tab.label = Some(label);
                    tab.regenerate(&text);
                }
                None => {
                    let mut tab = Buffer::generated(&label, "md", &text);
                    tab.set_view_only(true);
                    state.docs.push(tab);
                }
            }
            let id = state.docs.active().id();
            state.issue_tabs.insert(id, IssueTab { repo, issue });
            state.completion = None;
            // "Loading issue #n" is done with; anything said since stays.
            if state
                .message
                .as_ref()
                .is_some_and(|(said, _)| said.starts_with("Loading issue #"))
            {
                state.message = None;
            }
            reveal_active_tab(&mut state);
        }
        self.request_redraw();
        self.resume_display_link();
    }

    /// Start Task, above an issue's tab or from the Git menu: a worktree on
    /// the issue's branch with Claude in it, told the file the issue is
    /// written to. Never the issue's words on its command line: they are in
    /// the file, which is out of every repository and yours alone. Started
    /// before, its worktree is gone back to.
    pub(super) fn issue_start_task(&self) {
        let Some((repo, issue)) = self
            .state()
            .and_then(|state| active_issue(&state).map(|t| (t.repo.clone(), t.issue.clone())))
        else {
            return;
        };
        let refused = if repo.0.is_some() {
            Some("Start Task from an issue works for repositories on this Mac for now".to_owned())
        } else {
            None
        };
        let written = match refused {
            Some(why) => Err(why),
            None => crate::platform::app_support()
                .ok_or_else(|| "No Application Support folder to write the issue to".to_owned())
                .and_then(|dir| {
                    crate::platform::agents::write_issue_file(
                        &dir,
                        &repo,
                        issue.number,
                        &crate::project::worktree::issue_markdown(&issue),
                    )
                    .map_err(|e| format!("Could not write issue #{}: {e}", issue.number))
                }),
        };
        let file = match written {
            Ok(file) => file,
            Err(why) => {
                if let Some(mut state) = self.state_mut() {
                    state.say(layout::Feedback::Failure, why);
                }
                self.request_redraw();
                return;
            }
        };
        let prompt = crate::platform::agents::issue_prompt(&file, issue.number);
        let branch = crate::project::worktree::issue_branch(issue.number, &issue.title);
        self.set_agents_mode(true);
        self.make_task(None, repo.1, branch, Some(prompt));
    }

    /// Open on GitHub: the issue's page in the browser.
    pub(super) fn issue_open_on_github(&self) {
        let url = self
            .state()
            .and_then(|state| active_issue(&state).map(|t| t.issue.url.clone()));
        if let Some(url) = url.filter(|u| on_github(u))
            && let Err(e) = self.open(false, std::ffi::OsStr::new(&url))
            && let Some(mut state) = self.state_mut()
        {
            state.say(
                layout::Feedback::Failure,
                format!("Could not open {url}: {e}"),
            );
        }
    }
}
