//! Cmd-P in Agents mode: a switcher over every session, and every
//! workspace without one.
//!
//! An unprefixed query picks a session; `>` is still commands, `@` and `#`
//! symbols, and `/` the project's files. With nothing typed the rows come
//! in the order you were last on them, the one showing second, so Cmd-P
//! then Return goes back to the one before. The window hands this plain
//! rows, so the order and the matching are tested without a window.

use std::path::PathBuf;

use crate::project::finder;
use crate::render::layout::PaletteRow;

/// Places remembered at most.
pub const FOCUS_CAP: usize = 64;

/// What the palette's query asks for in Agents mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Query<'a> {
    /// A session or a workspace, by this.
    Switch(&'a str),
    /// A project file, by what follows the `/`.
    Files(&'a str),
    /// Commands or symbols, by their own prefixes.
    Other,
}

pub fn query(text: &str) -> Query<'_> {
    if let Some(rest) = text.strip_prefix('/') {
        Query::Files(rest)
    } else if text.starts_with(['>', '@', '#']) {
        Query::Other
    } else {
        Query::Switch(text)
    }
}

/// A session's place in the focus list.
pub fn session_key(id: &std::path::Path) -> String {
    format!("s:{}", id.display())
}

/// A workspace's place in the focus list, by its id.
pub fn workspace_key(id: &str) -> String {
    format!("w:{id}")
}

/// `key` was just shown: to the front, once, the oldest dropped past
/// [`FOCUS_CAP`].
pub fn touch(focus: &mut Vec<String>, key: &str) {
    if focus.first().is_some_and(|k| k == key) {
        return;
    }
    focus.retain(|k| k != key);
    focus.insert(0, key.to_owned());
    focus.truncate(FOCUS_CAP);
}

/// How recently `key` was shown: 0 the latest, `None` never.
pub fn rank_of(focus: &[String], key: &str) -> Option<usize> {
    focus.iter().position(|k| k == key)
}

/// Of `tabs`, the one shown most recently, by `key` (its session
/// key): a workspace goes back to the tab you left it on.
pub fn latest(focus: &[String], tabs: &[usize], key: impl Fn(usize) -> String) -> Option<usize> {
    tabs.iter()
        .copied()
        .filter_map(|i| rank_of(focus, &key(i)).map(|rank| (rank, i)))
        .min()
        .map(|(_, i)| i)
}

/// A workspace, as the switcher shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Space {
    pub id: String,
    pub name: String,
    pub branch: Option<String>,
    /// Its folder as the palette shows it, or its host for a remote one;
    /// empty for one without a folder.
    pub place: String,
    /// Its folder's last name, matched on.
    pub folder: String,
}

/// A session, as the switcher shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Session {
    /// What picks it again: see `Tab::id`.
    pub id: PathBuf,
    pub title: String,
    /// Its workspace, by index into the spaces.
    pub space: usize,
    /// What it is doing: waiting, working or idle.
    pub state: &'static str,
}

/// What choosing a row shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Session(PathBuf),
    /// A workspace with no session, by id.
    Workspace(String),
}

/// What the switcher lists from.
#[derive(Clone, Debug, Default)]
pub struct Sources {
    /// In the list's order.
    pub spaces: Vec<Space>,
    /// In the tabs' order.
    pub sessions: Vec<Session>,
    /// Places shown, latest first.
    pub focus: Vec<String>,
    /// The place showing now.
    pub current: Option<String>,
}

struct Item {
    key: String,
    row: PaletteRow,
    haystack: String,
    target: Target,
}

/// The rows for `query`, best first. A session belongs to its workspace's
/// place in the list, so rows never shown keep that order.
pub fn rows(sources: &Sources, query: &str) -> Vec<(PaletteRow, Target)> {
    let mut items = Vec::new();
    for (index, space) in sources.spaces.iter().enumerate() {
        let mine: Vec<&Session> = sources
            .sessions
            .iter()
            .filter(|s| s.space == index)
            .collect();
        for session in &mine {
            items.push(Item {
                key: session_key(&session.id),
                row: PaletteRow {
                    icon: Some(crate::project::icons::TERMINAL),
                    title: format!("{} / {}", space.name, session.title),
                    detail: detail(session.state, space),
                    shortcut: String::new(),
                },
                haystack: format!(
                    "{} {} {} {}",
                    space.name,
                    session.title,
                    space.branch.as_deref().unwrap_or(""),
                    space.folder
                ),
                target: Target::Session(session.id.clone()),
            });
        }
        if mine.is_empty() {
            items.push(Item {
                key: workspace_key(&space.id),
                row: PaletteRow {
                    icon: Some(crate::project::icons::FOLDER_OUTLINE),
                    title: space.name.clone(),
                    detail: detail("no session", space),
                    shortcut: String::new(),
                },
                haystack: format!(
                    "{} {} {}",
                    space.name,
                    space.branch.as_deref().unwrap_or(""),
                    space.folder
                ),
                target: Target::Workspace(space.id.clone()),
            });
        }
    }
    order(&items, sources, query)
        .into_iter()
        .map(|i| (items[i].row.clone(), items[i].target.clone()))
        .collect()
}

/// The state word, the branch and the folder, as far as there are any.
fn detail(state: &str, space: &Space) -> String {
    [state, space.branch.as_deref().unwrap_or(""), &space.place]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" \u{b7} ")
}

/// The indexes of `items` in the order shown.
fn order(items: &[Item], sources: &Sources, query: &str) -> Vec<usize> {
    let recency = |i: usize| rank_of(&sources.focus, &items[i].key).unwrap_or(usize::MAX);
    let needle: Vec<char> = query.trim().to_lowercase().chars().collect();
    if !needle.is_empty() {
        let mut hits: Vec<(i32, usize)> = (0..items.len())
            .filter_map(|i| {
                finder::score(&items[i].haystack.to_lowercase(), &needle).map(|(p, _)| (p, i))
            })
            .collect();
        hits.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then(recency(a.1).cmp(&recency(b.1)))
                .then(a.1.cmp(&b.1))
        });
        return hits.into_iter().map(|(_, i)| i).collect();
    }
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by_key(|i| (recency(*i), *i));
    // The one showing goes second: the first is the one before it.
    let current = sources.current.as_deref();
    if let Some(at) = order
        .iter()
        .position(|i| Some(items[*i].key.as_str()) == current)
    {
        let shown = order.remove(at);
        order.insert(order.len().min(1), shown);
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space(id: &str, name: &str) -> Space {
        Space {
            id: id.into(),
            name: name.into(),
            branch: Some(format!("{name}-branch")),
            place: format!("~/code/{name}"),
            folder: name.into(),
        }
    }

    fn session(id: &str, title: &str, space: usize) -> Session {
        Session {
            id: PathBuf::from(id),
            title: title.into(),
            space,
            state: "idle",
        }
    }

    fn titles(sources: &Sources, query: &str) -> Vec<String> {
        rows(sources, query)
            .into_iter()
            .map(|(row, _)| row.title)
            .collect()
    }

    /// Two workspaces with a session each, and one with none.
    fn three() -> Sources {
        Sources {
            spaces: vec![space("a", "alpha"), space("b", "beta"), space("c", "gamma")],
            sessions: vec![session("/s/1", "shell", 0), session("/s/2", "claude", 1)],
            ..Sources::default()
        }
    }

    #[test]
    fn latest_is_the_tab_shown_last_of_the_ones_given() {
        let focus: Vec<String> = ["s:/s/3", "w:a", "s:/s/1", "s:/s/2"]
            .map(String::from)
            .into();
        let key = |i: usize| format!("s:/s/{i}");
        assert_eq!(latest(&focus, &[1, 2], key), Some(1));
        assert_eq!(latest(&focus, &[2, 3], key), Some(3));
        assert_eq!(latest(&focus, &[4, 5], key), None);
        assert_eq!(latest(&focus, &[], key), None);
    }

    #[test]
    fn touch_moves_a_key_to_the_front_once() {
        let mut focus = Vec::new();
        touch(&mut focus, "s:a");
        touch(&mut focus, "s:b");
        touch(&mut focus, "s:a");
        assert_eq!(focus, ["s:a", "s:b"]);
        touch(&mut focus, "s:a");
        assert_eq!(focus, ["s:a", "s:b"]);
        assert_eq!(rank_of(&focus, "s:b"), Some(1));
        assert_eq!(rank_of(&focus, "s:c"), None);
    }

    #[test]
    fn touch_keeps_the_newest_up_to_the_cap() {
        let mut focus = Vec::new();
        for i in 0..FOCUS_CAP + 5 {
            touch(&mut focus, &format!("s:{i}"));
        }
        assert_eq!(focus.len(), FOCUS_CAP);
        assert_eq!(focus[0], format!("s:{}", FOCUS_CAP + 4));
        assert_eq!(rank_of(&focus, "s:4"), None);
        assert_eq!(rank_of(&focus, "s:5"), Some(FOCUS_CAP - 1));
    }

    #[test]
    fn nothing_typed_puts_the_one_showing_second() {
        let mut sources = three();
        sources.focus = vec!["s:/s/2".into(), "s:/s/1".into()];
        sources.current = Some("s:/s/2".into());
        assert_eq!(
            titles(&sources, ""),
            ["alpha / shell", "beta / claude", "gamma"]
        );
        // The previous one is first however long ago the rest were.
        sources.focus = vec!["s:/s/2".into(), "w:c".into(), "s:/s/1".into()];
        assert_eq!(
            titles(&sources, ""),
            ["gamma", "beta / claude", "alpha / shell"]
        );
        // Showing one never recorded: second still.
        sources.focus = vec!["s:/s/1".into()];
        sources.current = Some("s:/s/2".into());
        assert_eq!(
            titles(&sources, ""),
            ["alpha / shell", "beta / claude", "gamma"]
        );
    }

    #[test]
    fn rows_never_shown_keep_the_lists_order() {
        let mut sources = three();
        sources.sessions.push(session("/s/3", "logs", 0));
        assert_eq!(
            titles(&sources, ""),
            ["alpha / shell", "alpha / logs", "beta / claude", "gamma"]
        );
        sources.focus = vec!["w:c".into()];
        assert_eq!(
            titles(&sources, ""),
            ["gamma", "alpha / shell", "alpha / logs", "beta / claude"]
        );
    }

    #[test]
    fn a_query_ranks_by_score_then_recency() {
        let mut sources = three();
        sources.spaces[1].name = "alpine".into();
        // "al" starts both names equally: the latest shown goes first.
        sources.focus = vec!["s:/s/2".into()];
        assert_eq!(titles(&sources, "al"), ["alpine / claude", "alpha / shell"]);
        sources.focus = vec!["s:/s/1".into()];
        assert_eq!(titles(&sources, "al"), ["alpha / shell", "alpine / claude"]);
        // A better match wins over recency.
        assert_eq!(titles(&sources, "alpi")[0], "alpine / claude");
        // The branch and the folder are matched too.
        assert_eq!(titles(&sources, "gamma-branch"), ["gamma"]);
        assert!(titles(&sources, "zzz").is_empty());
    }

    #[test]
    fn rows_say_state_branch_and_folder() {
        let mut sources = three();
        sources.sessions[0].state = "waiting";
        sources.spaces[1].branch = None;
        sources.spaces[1].place = "devbox".into();
        let rows = rows(&sources, "");
        assert_eq!(
            rows[0].0.detail,
            "waiting \u{b7} alpha-branch \u{b7} ~/code/alpha"
        );
        assert_eq!(rows[0].1, Target::Session(PathBuf::from("/s/1")));
        assert_eq!(rows[1].0.detail, "idle \u{b7} devbox");
        assert_eq!(rows[2].0.title, "gamma");
        assert_eq!(rows[2].1, Target::Workspace("c".into()));
    }

    #[test]
    fn prefixes_leave_the_switcher() {
        assert_eq!(query("api"), Query::Switch("api"));
        assert_eq!(query(""), Query::Switch(""));
        assert_eq!(query("/main.rs"), Query::Files("main.rs"));
        assert_eq!(query(">split"), Query::Other);
        assert_eq!(query("@parse"), Query::Other);
        assert_eq!(query("#Parser"), Query::Other);
    }
}
