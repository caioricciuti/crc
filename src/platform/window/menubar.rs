//! crc in the menu bar: how many sessions wait on you, seen from any app,
//! and a menu of the workspaces that goes to one.
//!
//! The rows are made from plain data, so their order is tested without a
//! window. The menu is made when it opens, never while it shows, and its
//! order holds until it opens again: like the sidebar, it never reorders
//! under the pointer. A test instance has no item: it must not touch the
//! menu bar of the Mac it runs on.

use std::cell::{Cell, RefCell};

use objc2_app_kit::{NSImage, NSStatusBar, NSStatusItem, NSVariableStatusItemLength};

use super::*;
use crate::platform::agents::task_of;
use crate::platform::terminal::Activity;

/// A workspace's row in the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Row {
    /// What picks it again when chosen: its workspace's id.
    pub id: String,
    pub name: String,
    /// The most pressing of its sessions': waiting, else working.
    pub activity: Activity,
    /// The session choosing it shows; see [`best_tab`].
    pub tab: Option<usize>,
}

impl Row {
    /// The item's title: `name · waiting`.
    pub fn title(&self) -> String {
        format!("{} \u{b7} {}", self.name, self.activity.label())
    }
}

/// The session a workspace shows when it is chosen, of `mine` (its tabs,
/// in order): one waiting on you, else the one showing, else the newest.
pub(super) fn best_tab(
    mine: &[usize],
    waiting: impl Fn(usize) -> bool,
    active: usize,
) -> Option<usize> {
    mine.iter()
        .copied()
        .find(|i| waiting(*i))
        .or_else(|| mine.contains(&active).then_some(active))
        .or_else(|| mine.last().copied())
}

/// How many sessions wait on you: the Dock's badge and the item's count.
pub(super) fn waiting_count(activities: impl IntoIterator<Item = Activity>) -> usize {
    activities
        .into_iter()
        .filter(|a| *a == Activity::Waiting)
        .count()
}

fn rank(activity: Activity) -> u8 {
    match activity {
        Activity::Waiting => 0,
        Activity::Working => 1,
        Activity::Idle => 2,
    }
}

/// The menu's rows, from `spaces` (each workspace's id and name, in the
/// list's order) and `tabs` (each session's workspace, by index into
/// `spaces`, and what it is doing), `active` the session showing. One row
/// per workspace: the waiting ones first, then the working, then the idle,
/// each group in the list's order. With the count of sessions waiting,
/// those without a workspace too, as the Dock counts them.
pub(super) fn menu_rows(
    spaces: &[(String, String)],
    tabs: &[(Option<usize>, Activity)],
    active: usize,
) -> (usize, Vec<Row>) {
    let waiting = waiting_count(tabs.iter().map(|(_, a)| *a));
    let mut rows: Vec<Row> = spaces
        .iter()
        .enumerate()
        .map(|(index, (id, name))| {
            let mine: Vec<usize> = (0..tabs.len())
                .filter(|i| tabs[*i].0 == Some(index))
                .collect();
            let activity = mine
                .iter()
                .map(|i| tabs[*i].1)
                .min_by_key(|a| rank(*a))
                .unwrap_or(Activity::Idle);
            Row {
                id: id.clone(),
                name: name.clone(),
                activity,
                tab: best_tab(&mine, |i| tabs[i].1 == Activity::Waiting, active),
            }
        })
        .collect();
    // Stable: the list's order inside each group.
    rows.sort_by_key(|row| rank(row.activity));
    (waiting, rows)
}

/// [`menu_rows`] of the window's workspaces and sessions now.
pub(super) fn rows_of(state: &State) -> (usize, Vec<Row>) {
    let tasks = &state.agents.tasks;
    let spaces: Vec<(String, String)> = tasks.iter().map(|t| (t.id(), t.name())).collect();
    let tabs: Vec<(Option<usize>, Activity)> = state
        .terminal
        .tabs
        .iter()
        .map(|t| (task_of(tasks, t), t.activity()))
        .collect();
    menu_rows(&spaces, &tabs, state.terminal.active)
}

/// What the self-test's dump says of the menu: the count, then each row's
/// title.
pub(super) fn report(state: &State) -> String {
    let (waiting, rows) = rows_of(state);
    let rows: Vec<String> = rows.iter().map(Row::title).collect();
    format!("{waiting} {}", rows.join("|"))
}

thread_local! {
    static ITEM: RefCell<Option<Retained<NSStatusItem>>> = const { RefCell::new(None) };
    /// The count the Dock and the item say now; `None` before the first.
    static SAID: Cell<Option<usize>> = const { Cell::new(None) };
}

/// The item, with its menu, which `view` fills each time it opens. Not in
/// a test instance.
pub(super) fn install(view: &EditorView) {
    if std::env::var_os("CRC_SELFTEST").is_some() {
        return;
    }
    let mtm = MainThreadMarker::from(view);
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    menu.setDelegate(Some(ProtocolObject::from_ref(view)));
    item.setMenu(Some(&menu));
    ITEM.with(|slot| *slot.borrow_mut() = Some(item));
    SAID.with(|said| said.set(None));
}

/// The item says `waiting`: crc's symbol, and the count beside it while
/// any session waits.
pub(super) fn sync_status_item(waiting: usize, mtm: MainThreadMarker) {
    SAID.with(|said| said.set(Some(waiting)));
    ITEM.with(|slot| {
        let Some(button) = slot.borrow().as_ref().and_then(|item| item.button(mtm)) else {
            return;
        };
        if button.image().is_none() {
            let name = NSString::from_str("terminal");
            let alt = NSString::from_str("crc");
            match NSImage::imageWithSystemSymbolName_accessibilityDescription(&name, Some(&alt)) {
                Some(image) => {
                    button.setImage(Some(&image));
                    // NSImageLeft: the count after the symbol.
                    let _: () = unsafe { msg_send![&*button, setImagePosition: 2isize] };
                }
                None => button.setTitle(&NSString::from_str("crc")),
            }
        }
        let symbol = button.image().is_some();
        let title = match (waiting, symbol) {
            (0, true) => String::new(),
            (0, false) => "crc".to_string(),
            (n, true) => format!(" {n}"),
            (n, false) => format!("crc {n}"),
        };
        button.setTitle(&NSString::from_str(&title));
        let tip = match waiting {
            0 => "crc".to_string(),
            1 => "crc: 1 session waiting on you".to_string(),
            n => format!("crc: {n} sessions waiting on you"),
        };
        button.setToolTip(Some(&NSString::from_str(&tip)));
    });
}

/// The Dock badge and the item, when the count waiting is not what they
/// say: cheap enough for every pump, so a session that stops waiting
/// (closed, back at work) is never left counted.
pub(super) fn sync_if_changed(state: &State, mtm: MainThreadMarker) {
    let waiting = waiting_count(state.terminal.tabs.iter().map(|t| t.activity()));
    if SAID.with(Cell::get) != Some(waiting) {
        super::agents::sync_dock_badge(state, mtm);
    }
}

/// The menu, made again as it opens: Show crc, a row per workspace, Quit.
/// Each row carries its workspace's id, looked up again when chosen.
pub(super) fn fill(view: &EditorView, menu: &NSMenu) {
    let mtm = MainThreadMarker::from(view);
    menu.removeAllItems();
    let target: &AnyObject = view;
    let add = |title: &str, action: Sel| {
        let item = menu_item(mtm, title, action);
        unsafe { item.setTarget(Some(target)) };
        menu.addItem(&item);
        item
    };
    add("Show crc", sel!(statusItemShow:));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    let listed = 'rows: {
        // Busy up the stack: no rows this time, Show and Quit still work.
        let Some(mut state) = view.state_mut() else {
            break 'rows Some(Vec::new());
        };
        let listed = state.agents.listed() || !state.agents.tasks.is_empty();
        if !listed {
            // Never read in this run (crc stayed in the editor): read now,
            // for the next time it opens.
            state.agents.refresh_due(false);
        }
        listed.then(|| rows_of(&state).1)
    };
    match listed {
        Some(rows) => {
            for row in rows {
                let item = add(&row.title(), sel!(statusItemPick:));
                let id = NSString::from_str(&row.id);
                unsafe { item.setRepresentedObject(Some(&id)) };
            }
        }
        None => {
            view.resume_display_link();
            let item = NSMenuItem::new(mtm);
            item.setTitle(&NSString::from_str("Reading workspaces\u{2026}"));
            item.setEnabled(false);
            menu.addItem(&item);
        }
    }
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    let quit = menu_item(mtm, "Quit crc", sel!(terminate:));
    let app = NSApplication::sharedApplication(mtm);
    unsafe { quit.setTarget(Some(&app)) };
    menu.addItem(&quit);
}

impl EditorView {
    /// crc to the front, its window out of the Dock. A test instance
    /// leaves your keyboard in your app.
    pub(super) fn status_item_show(&self) {
        if self.ivars().testing {
            return;
        }
        let mtm = MainThreadMarker::from(self);
        NSApplication::sharedApplication(mtm).activate();
        if let Some(window) = self.window() {
            if window.isMiniaturized() {
                window.deminiaturize(None);
            }
            window.makeKeyAndOrderFront(None);
        }
    }

    /// A workspace's row chosen: crc in front, in Agents mode, on its
    /// waiting session (else the one showing, else its newest), or a new
    /// shell in it when it has none, as a click on its row does.
    pub(super) fn status_item_pick(&self, id: &str) {
        self.status_item_show();
        let index = self.state().and_then(|state| state.agents.by_id(id));
        if let Some(index) = index {
            self.select_task(index);
        }
        self.request_redraw();
        self.pump();
    }

    /// The self-test's `menubar N`: the menu as it would open, and its
    /// Nth workspace row chosen through the item's own action.
    pub(super) fn selftest_menubar_pick(&self, number: usize) {
        let menu = NSMenu::new(MainThreadMarker::from(self));
        fill(self, &menu);
        let item = menu
            .itemArray()
            .iter()
            .filter(|item| item.action() == Some(sel!(statusItemPick:)))
            .nth(number.saturating_sub(1));
        match item {
            Some(item) => {
                let _: () = unsafe { msg_send![self, statusItemPick: &*item] };
            }
            None => eprintln!("selftest: no menu bar row {number}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spaces(names: &[&str]) -> Vec<(String, String)> {
        names
            .iter()
            .map(|n| (format!("id-{n}"), n.to_string()))
            .collect()
    }

    fn names(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn waiting_rows_lead_and_the_count_is_the_docks() {
        use Activity::*;
        let spaces = spaces(&["alpha", "beta", "gamma"]);
        let tabs = [
            (Some(0), Idle),
            (Some(1), Working),
            (Some(2), Waiting),
            (Some(2), Waiting),
            // A session in no workspace still counts, as on the Dock.
            (None, Waiting),
        ];
        let (waiting, rows) = menu_rows(&spaces, &tabs, 0);
        assert_eq!(waiting, waiting_count(tabs.iter().map(|t| t.1)));
        assert_eq!(waiting, 3);
        assert_eq!(names(&rows), ["gamma", "beta", "alpha"]);
        assert_eq!(rows[0].title(), "gamma \u{b7} waiting");
        assert_eq!(rows[1].title(), "beta \u{b7} working");
        assert_eq!(rows[2].title(), "alpha \u{b7} idle");
    }

    #[test]
    fn rows_keep_the_lists_order_inside_each_group() {
        use Activity::*;
        // The sidebar never reorders by itself; inside a group, neither
        // does the menu.
        let spaces = spaces(&["d", "c", "b", "a", "e"]);
        let tabs = [
            (Some(4), Waiting),
            (Some(0), Idle),
            (Some(3), Idle),
            (Some(1), Waiting),
            (Some(2), Idle),
        ];
        let (_, rows) = menu_rows(&spaces, &tabs, 0);
        assert_eq!(names(&rows), ["c", "e", "d", "b", "a"]);
    }

    #[test]
    fn a_workspace_without_a_terminal_is_listed_idle() {
        let spaces = spaces(&["empty", "busy"]);
        let tabs = [(Some(1), Activity::Working)];
        let (waiting, rows) = menu_rows(&spaces, &tabs, 0);
        assert_eq!(waiting, 0);
        assert_eq!(names(&rows), ["busy", "empty"]);
        assert_eq!(rows[1].activity, Activity::Idle);
        assert_eq!(rows[1].tab, None);
        assert_eq!(rows[1].id, "id-empty");
    }

    #[test]
    fn a_rows_pick_is_its_waiting_session_over_the_one_showing() {
        use Activity::*;
        let spaces = spaces(&["one", "two"]);
        let tabs = [
            (Some(0), Idle),
            (Some(0), Waiting),
            (Some(0), Idle),
            (Some(1), Idle),
            (Some(1), Idle),
        ];
        // Tab 0 shows; one's waiting session is still the pick.
        let (_, rows) = menu_rows(&spaces, &tabs, 0);
        assert_eq!(rows[0].name, "one");
        assert_eq!(rows[0].tab, Some(1));
        // Nothing waiting in two: the one showing, else its newest.
        assert_eq!(rows[1].tab, Some(4));
        let (_, rows) = menu_rows(&spaces, &tabs, 3);
        assert_eq!(rows[1].tab, Some(3));
    }

    #[test]
    fn best_tab_takes_waiting_then_showing_then_newest() {
        assert_eq!(best_tab(&[2, 5, 7], |i| i == 5, 7), Some(5));
        assert_eq!(best_tab(&[2, 5, 7], |_| false, 2), Some(2));
        assert_eq!(best_tab(&[2, 5, 7], |_| false, 9), Some(7));
        assert_eq!(best_tab(&[], |_| true, 0), None);
    }
}
