//! Agents mode: the terminal takes the editor column and the sidebar lists
//! tasks, each a worktree of the open repository with the sessions running
//! in it. The toolbar's Editor / Agents switch (Cmd-Shift-A) flips between the
//! two; nothing stops when it does.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use crate::platform::terminal::{Activity, Tab};
use crate::project::icons;
use crate::project::worktree::Worktree;
use crate::render::{
    font::Atlas,
    layout::{self, Theme, Viewport},
    metal::GlyphInstance,
};

pub const ROW: f32 = 26.0;
/// Under a task, its sessions start this far in.
const INDENT: f32 = 22.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// The header's New Task.
    NewTask,
    Refresh,
    /// A task's row: shows its newest session, or starts Claude in it.
    Select(usize),
    /// A session's row, by terminal tab.
    Session(usize),
    /// A new Claude session in a task.
    NewClaude(usize),
    /// The task's menu: open in the editor, merge, push, remove.
    Menu(usize),
}

/// Worktrees read on a worker: the main checkout they are for, and the list
/// with every path resolved.
type Listing = (PathBuf, Result<Vec<Worktree>, String>);

#[derive(Default)]
pub struct Panel {
    /// Agents mode is on: the terminal has the column, this the sidebar.
    pub on: bool,
    /// The main checkout the tasks belong to.
    pub repo: Option<PathBuf>,
    pub tasks: Vec<Worktree>,
    /// The task whose sessions the terminal shows, by path.
    pub selected: Option<PathBuf>,
    /// A failed read or operation, said under the header.
    pub note: Option<String>,
    rx: Option<mpsc::Receiver<Listing>>,
    /// An operation on a task running on a worker, and what it says.
    pub op: Option<(String, mpsc::Receiver<Result<Done, String>>)>,
    /// The task a menu was opened on.
    pub menu_task: Option<usize>,
    /// Whether the terminal panel was open in the editor before Agents
    /// mode took the column, to put it back as it was.
    pub editor_terminal: bool,
    pub scroll: usize,
    pub hits: Vec<(Viewport, Action)>,
}

/// A task operation that worked: what to say, and a folder to start Claude
/// in (a new task's).
pub struct Done {
    pub said: String,
    pub start_in: Option<PathBuf>,
}

impl Panel {
    /// Runs `work` on a worker, saying `doing` at the foot of the list
    /// until it answers.
    pub fn start(
        &mut self,
        doing: &str,
        work: impl FnOnce() -> Result<Done, String> + Send + 'static,
    ) {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(work());
        });
        self.op = Some((doing.to_owned(), rx));
    }

    /// The operation's answer, once it has one.
    pub fn take_done(&mut self) -> Option<Result<Done, String>> {
        let (_, rx) = self.op.as_ref()?;
        let answer = match rx.try_recv() {
            Ok(answer) => answer,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => Err("The task operation stopped".into()),
        };
        self.op = None;
        Some(answer)
    }
}

enum Line {
    Task(usize),
    Session(usize),
    Heading(&'static str),
    Note(String),
}

/// The task a folder belongs to: the deepest worktree holding it, so a
/// worktree inside the main checkout is not taken for the main one.
pub fn task_of(tasks: &[Worktree], folder: &Path) -> Option<usize> {
    tasks
        .iter()
        .enumerate()
        .filter(|(_, t)| folder.starts_with(&t.path))
        .max_by_key(|(_, t)| t.path.components().count())
        .map(|(i, _)| i)
}

impl Panel {
    /// Reads the worktrees of the repository `dir` is in, on a worker.
    pub fn refresh(&mut self, dir: &Path) {
        let (tx, rx) = mpsc::channel();
        let dir = dir.to_path_buf();
        std::thread::spawn(move || {
            let listing = crate::project::worktree::list(&dir).map(|list| {
                list.into_iter()
                    .map(|mut w| {
                        w.path = crate::platform::canonical(&w.path);
                        w
                    })
                    .collect::<Vec<_>>()
            });
            let main = match &listing {
                Ok(list) => list
                    .iter()
                    .find(|w| w.main)
                    .map_or_else(|| dir.clone(), |w| w.path.clone()),
                Err(_) => dir,
            };
            let _ = tx.send((main, listing));
        });
        self.rx = Some(rx);
    }

    /// Takes a finished read. Returns whether anything changed.
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.rx else {
            return false;
        };
        let Ok((main, listing)) = rx.try_recv() else {
            return false;
        };
        self.rx = None;
        match listing {
            Ok(tasks) => {
                self.repo = Some(main);
                self.tasks = tasks;
                if self
                    .note
                    .as_deref()
                    .is_some_and(|n| n.starts_with("No Git"))
                {
                    self.note = None;
                }
            }
            Err(_) => {
                self.repo = None;
                self.tasks.clear();
                self.note = Some("No Git repository here: sessions run in the open folder".into());
            }
        }
        true
    }

    /// A read or an operation is in flight.
    pub fn busy(&self) -> bool {
        self.rx.is_some() || self.op.is_some()
    }

    pub fn selected_index(&self) -> Option<usize> {
        let selected = self.selected.as_deref()?;
        self.tasks.iter().position(|t| t.path == selected)
    }

    fn lines(&self, tabs: &[Tab]) -> Vec<Line> {
        let mut lines = Vec::new();
        if let Some(note) = &self.note {
            lines.push(Line::Note(note.clone()));
        }
        let owner: Vec<Option<usize>> = tabs
            .iter()
            .map(|t| task_of(&self.tasks, &t.folder))
            .collect();
        for task in 0..self.tasks.len() {
            lines.push(Line::Task(task));
            for (tab, _) in owner.iter().enumerate().filter(|(_, o)| **o == Some(task)) {
                lines.push(Line::Session(tab));
            }
        }
        let loose: Vec<usize> = (0..tabs.len()).filter(|i| owner[*i].is_none()).collect();
        if !loose.is_empty() {
            if !self.tasks.is_empty() {
                lines.push(Line::Heading("Other folders"));
            }
            lines.extend(loose.into_iter().map(Line::Session));
        }
        if tabs.is_empty() && self.tasks.len() <= 1 {
            lines.push(Line::Note(
                "New Task (\u{21e7}\u{2318}N) starts an agent on a branch of its own".into(),
            ));
        }
        lines
    }

    fn visible(column: Viewport) -> usize {
        ((column.height - layout::SIDEBAR_HEADER_HEIGHT).max(0.0) / ROW).floor() as usize
    }

    pub fn scroll_by(&mut self, lines: isize, column: Viewport, tabs: &[Tab]) {
        let max = self
            .lines(tabs)
            .len()
            .saturating_sub(Panel::visible(column));
        self.scroll = (self.scroll as isize + lines).clamp(0, max as isize) as usize;
    }

    /// For the self-test: the mode, the tasks by name, the selected one,
    /// and each session's task (`-` for none), the active one starred.
    pub fn report(&self, tabs: &[Tab], active: usize) -> String {
        let name = |i: Option<usize>| i.map_or("-".to_owned(), |i| self.tasks[i].name());
        format!(
            "{} tasks={} selected={} sessions={} op={}",
            if self.on { "on" } else { "off" },
            self.tasks
                .iter()
                .map(Worktree::name)
                .collect::<Vec<_>>()
                .join(","),
            name(self.selected_index()),
            tabs.iter()
                .enumerate()
                .map(|(i, t)| {
                    let task = name(task_of(&self.tasks, &t.folder));
                    if i == active {
                        format!("*{task}")
                    } else {
                        task
                    }
                })
                .collect::<Vec<_>>()
                .join(","),
            self.op.as_ref().map_or("-", |(said, _)| said.as_str()),
        )
    }

    /// A region by the name a self-test script uses: `agents.new`,
    /// `agents.refresh`, `agents.task.0`, `agents.session.1`,
    /// `agents.claude.0`, `agents.menu.0`. As last drawn.
    pub fn named(&self, name: &str) -> Option<Viewport> {
        let rest = name.strip_prefix("agents.")?;
        let (kind, n) = rest.split_once('.').unwrap_or((rest, ""));
        let n: usize = n.parse().unwrap_or(0);
        let want = match kind {
            "new" => Action::NewTask,
            "refresh" => Action::Refresh,
            "task" => Action::Select(n),
            "session" => Action::Session(n),
            "claude" => Action::NewClaude(n),
            "menu" => Action::Menu(n),
            _ => return None,
        };
        self.hits
            .iter()
            .find(|(_, a)| *a == want)
            .map(|(rect, _)| *rect)
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<Action> {
        self.hits
            .iter()
            .find(|(rect, _)| rect.contains(x, y))
            .map(|(_, action)| action.clone())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        tabs: &[Tab],
        active_tab: Option<usize>,
        atlas: &mut Atlas,
        column: Viewport,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) {
        self.hits.clear();
        layout::push_rect(
            out,
            atlas,
            [column.x, column.y],
            [column.width, column.height],
            theme.sidebar_background,
        );
        layout::push_sidebar_title(out, atlas, column, "AGENTS", 2, theme);
        let [new, refresh] = layout::sidebar_header_buttons::<2>(column);
        for (rect, icon, tip, action) in [
            (
                new,
                icons::ADD,
                "New Task  \u{21e7}\u{2318}N",
                Action::NewTask,
            ),
            (refresh, icons::REFRESH, "Refresh Tasks", Action::Refresh),
        ] {
            layout::Button::new(rect)
                .icon(icon)
                .tone(layout::Tone::Ghost)
                .tip(tip)
                .draw(out, atlas, theme);
            self.hits.push((rect, action));
        }
        let top = column.y + layout::SIDEBAR_HEADER_HEIGHT;
        let x = column.x + 10.0;
        let width = (column.width - 20.0).max(0.0);
        let lines = self.lines(tabs);
        let rows = Panel::visible(column);
        self.scroll = self.scroll.min(lines.len().saturating_sub(rows));
        let selected = self.selected_index();
        for (n, line) in lines.iter().skip(self.scroll).take(rows).enumerate() {
            let y = top + n as f32 * ROW;
            let row = Viewport {
                x: column.x + 4.0,
                y,
                width: (column.width - 8.0).max(0.0),
                height: ROW,
            };
            let text_rect = |from: f32, right: f32| Viewport {
                x: x + from,
                y: y + 4.0,
                width: (width - from - right).max(0.0),
                height: 20.0,
            };
            match line {
                Line::Note(text) => {
                    layout::push_ui_text(out, atlas, text_rect(2.0, 0.0), text, theme.gutter_text);
                }
                Line::Heading(text) => {
                    layout::push_ui_text(out, atlas, text_rect(2.0, 0.0), text, theme.status_text);
                }
                Line::Task(index) => {
                    let task = &self.tasks[*index];
                    let over = layout::hovered(row);
                    if selected == Some(*index) {
                        layout::push_rounded_rect(
                            out,
                            row,
                            layout::UI_RADIUS_SM,
                            theme.sidebar_selected,
                        );
                    } else if over {
                        layout::push_row_hover(out, row, theme);
                    }
                    layout::push_icon_centered(
                        out,
                        atlas,
                        Viewport {
                            x,
                            width: 18.0,
                            ..row
                        },
                        if task.main {
                            icons::HOME
                        } else {
                            icons::GIT_BRANCH
                        },
                        if selected == Some(*index) {
                            theme.accent
                        } else {
                            theme.status_text
                        },
                    );
                    let mine: Vec<&Tab> = tabs
                        .iter()
                        .filter(|t| task_of(&self.tasks, &t.folder) == Some(*index))
                        .collect();
                    let waiting = mine
                        .iter()
                        .filter(|t| t.activity() == Activity::Waiting)
                        .count();
                    let working = mine.iter().any(|t| t.activity() == Activity::Working);
                    // Hover trades the summary for the row's two buttons.
                    let buttons = 2.0 * (layout::UI_CONTROL_SM + 2.0);
                    layout::push_ui_text(
                        out,
                        atlas,
                        text_rect(24.0, buttons + 4.0),
                        &task.name(),
                        theme.text,
                    );
                    let right = row.x + row.width - 4.0;
                    if over {
                        let menu = Viewport {
                            x: right - layout::UI_CONTROL_SM,
                            y: y + (ROW - layout::UI_CONTROL_SM) * 0.5,
                            width: layout::UI_CONTROL_SM,
                            height: layout::UI_CONTROL_SM,
                        };
                        let add = Viewport {
                            x: menu.x - layout::UI_CONTROL_SM - 2.0,
                            ..menu
                        };
                        layout::Button::new(add)
                            .icon(icons::ADD)
                            .tone(layout::Tone::Ghost)
                            .tip("New Claude Session Here")
                            .draw(out, atlas, theme);
                        layout::Button::new(menu)
                            .icon(icons::ELLIPSIS)
                            .tone(layout::Tone::Ghost)
                            .tip("Open, Merge, Push or Remove")
                            .draw(out, atlas, theme);
                        self.hits.push((add, Action::NewClaude(*index)));
                        self.hits.push((menu, Action::Menu(*index)));
                    } else if waiting > 0 {
                        let label = if waiting == 1 {
                            "waiting".to_owned()
                        } else {
                            format!("{waiting} waiting")
                        };
                        let w = layout::ui_text_width(atlas, &label) + 4.0;
                        layout::push_ui_text(
                            out,
                            atlas,
                            Viewport {
                                x: right - w,
                                y: y + 4.0,
                                width: w,
                                height: 20.0,
                            },
                            &label,
                            theme.diff_modified,
                        );
                    } else if working {
                        layout::push_spinner(out, right - 22.0, row, theme.accent);
                    } else if *index < 9 {
                        let hint = format!("\u{2303}{}", index + 1);
                        let w = layout::ui_text_width(atlas, &hint) + 4.0;
                        layout::push_ui_text(
                            out,
                            atlas,
                            Viewport {
                                x: right - w,
                                y: y + 4.0,
                                width: w,
                                height: 20.0,
                            },
                            &hint,
                            theme.gutter_text,
                        );
                    }
                    self.hits.push((row, Action::Select(*index)));
                }
                Line::Session(index) => {
                    let tab = &tabs[*index];
                    let indent = if self.tasks.is_empty() { 0.0 } else { INDENT };
                    let row = Viewport {
                        x: row.x + indent,
                        width: (row.width - indent).max(0.0),
                        ..row
                    };
                    if active_tab == Some(*index) {
                        layout::push_rounded_rect(out, row, layout::UI_RADIUS_SM, theme.row_hover);
                    } else if layout::hovered(row) {
                        layout::push_row_hover(out, row, theme);
                    }
                    let activity = tab.activity();
                    let dot_x = row.x + 8.0;
                    match activity {
                        Activity::Working => {
                            layout::push_spinner(out, dot_x - 2.0, row, theme.accent);
                        }
                        Activity::Waiting | Activity::Idle => {
                            layout::push_rounded_rect(
                                out,
                                Viewport {
                                    x: dot_x,
                                    y: y + 9.0,
                                    width: 8.0,
                                    height: 8.0,
                                },
                                4.0,
                                if activity == Activity::Waiting {
                                    theme.diff_modified
                                } else {
                                    theme.gutter_text
                                },
                            );
                        }
                    }
                    let state = match activity {
                        Activity::Waiting => "waiting",
                        Activity::Working => "working",
                        Activity::Idle => "idle",
                    };
                    let state_w = layout::ui_text_width(atlas, state) + 4.0;
                    layout::push_ui_text(
                        out,
                        atlas,
                        Viewport {
                            x: row.x + 26.0,
                            y: y + 4.0,
                            width: (row.width - 26.0 - state_w - 8.0).max(0.0),
                            height: 20.0,
                        },
                        &tab.name(),
                        theme.text,
                    );
                    layout::push_ui_text(
                        out,
                        atlas,
                        Viewport {
                            x: row.x + row.width - state_w - 4.0,
                            y: y + 4.0,
                            width: state_w,
                            height: 20.0,
                        },
                        state,
                        if activity == Activity::Waiting {
                            theme.diff_modified
                        } else {
                            theme.gutter_text
                        },
                    );
                    self.hits.push((row, Action::Session(*index)));
                }
            }
        }
        if let Some((said, _)) = &self.op {
            let bottom = Viewport {
                x: column.x + 4.0,
                y: column.y + column.height - ROW,
                width: (column.width - 8.0).max(0.0),
                height: ROW,
            };
            layout::push_rect(
                out,
                atlas,
                [bottom.x, bottom.y],
                [bottom.width, bottom.height],
                theme.sidebar_background,
            );
            let w = layout::push_spinner(out, bottom.x + 8.0, bottom, theme.accent);
            layout::push_ui_text(
                out,
                atlas,
                Viewport {
                    x: bottom.x + 12.0 + w,
                    y: bottom.y + 4.0,
                    width: (bottom.width - 16.0 - w).max(0.0),
                    height: 20.0,
                },
                said,
                theme.status_text,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_belongs_to_its_deepest_task() {
        let tree = |path: &str, main: bool| Worktree {
            path: PathBuf::from(path),
            branch: None,
            main,
        };
        let tasks = vec![
            tree("/r", true),
            tree("/r/.claude/worktrees/x", false),
            tree("/w/y", false),
        ];
        assert_eq!(task_of(&tasks, Path::new("/r/src")), Some(0));
        assert_eq!(
            task_of(&tasks, Path::new("/r/.claude/worktrees/x/src")),
            Some(1)
        );
        assert_eq!(task_of(&tasks, Path::new("/w/y")), Some(2));
        assert_eq!(task_of(&tasks, Path::new("/elsewhere")), None);
        // A sibling whose name starts the same is not inside.
        assert_eq!(task_of(&tasks, Path::new("/rr")), None);
    }
}
