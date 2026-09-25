//! Commands for the Cmd-P palette's `>` mode.
//!
//! The list is read from the menu bar rather than kept beside it. Every
//! command crc has is a menu item already, validated by `validateMenuItem`,
//! so reading the menu means a new item shows up in the palette with its
//! shortcut, and a greyed-out one stays out, without a second table to keep
//! in step.

use crate::project::finder;
use objc2::runtime::Sel;
use objc2::sel;
use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSMenuItem};
use objc2_foundation::MainThreadMarker;

/// One menu item, as the palette shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub title: String,
    /// The top-level menu it lives in: File, View, Git...
    pub group: String,
    /// The key equivalent in menu notation, such as `⌘⇧O`. Empty for none.
    pub shortcut: String,
    /// Other words it goes by, matched but not shown.
    pub keywords: &'static str,
    /// The menu item's action, sent the way the menu would send it.
    pub action: Sel,
}

/// Words people type for a command whose menu title says something else.
fn keywords(action: Sel) -> &'static str {
    if action == sel!(showSourceControl:) {
        "git scm changes diff"
    } else if action == sel!(toggleTerminal:) || action == sel!(newTerminal:) {
        "shell console pty"
    } else if action == sel!(openClaude:) {
        "ai agent terminal"
    } else if action == sel!(openQuickly:) {
        "file find go"
    } else if action == sel!(togglePreview:) {
        "markdown render"
    } else {
        ""
    }
}

/// The items of the main menu that `enabled` allows, in menu order.
///
/// Validation is the caller's: AppKit's own goes through the key window's
/// responder chain, which is not there while the window is not key, and
/// then greys out every item. Call this before the palette takes the
/// keyboard, since a focused field disables the items that act on the tree.
pub fn from_menu(mtm: MainThreadMarker, enabled: impl Fn(&NSMenuItem) -> bool) -> Vec<Command> {
    let Some(menubar) = NSApplication::sharedApplication(mtm).mainMenu() else {
        return Vec::new();
    };
    let mut commands = Vec::new();
    for top in 0..menubar.numberOfItems() {
        let Some(submenu) = menubar.itemAtIndex(top).and_then(|item| item.submenu()) else {
            continue;
        };
        let group = submenu.title().to_string();
        for index in 0..submenu.numberOfItems() {
            let Some(item) = submenu.itemAtIndex(index) else {
                continue;
            };
            let Some(action) = item.action() else {
                continue;
            };
            // The palette is Open Quickly; listing it inside itself only
            // reopens what is already open.
            // Alternates are the Option variants AppKit swaps in while the
            // key is held, such as Quit and Keep Windows.
            if item.isSeparatorItem()
                || item.isHidden()
                || item.isAlternate()
                || action == sel!(openQuickly:)
                || !enabled(&item)
            {
                continue;
            }
            commands.push(Command {
                title: item.title().to_string(),
                group: group.clone(),
                shortcut: shortcut(
                    &item.keyEquivalent().to_string(),
                    item.keyEquivalentModifierMask(),
                ),
                keywords: keywords(action),
                action,
            });
        }
    }
    commands
}

/// A key equivalent as a menu shows it.
pub fn shortcut(key: &str, mask: NSEventModifierFlags) -> String {
    if key.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for (flag, symbol) in [
        (NSEventModifierFlags::Control, '⌃'),
        (NSEventModifierFlags::Option, '⌥'),
        (NSEventModifierFlags::Shift, '⇧'),
        (NSEventModifierFlags::Command, '⌘'),
    ] {
        if mask.contains(flag) {
            out.push(symbol);
        }
    }
    // An upper-case key equivalent implies Shift, as AppKit draws it.
    if key.chars().any(char::is_uppercase) && !out.contains('⇧') {
        out.insert(out.find('⌘').unwrap_or(out.len()), '⇧');
    }
    let name = match key {
        "\r" => "↩".to_owned(),
        "\u{8}" | "\u{7f}" => "⌫".to_owned(),
        " " => "Space".to_owned(),
        "\u{f700}" => "↑".to_owned(),
        "\u{f701}" => "↓".to_owned(),
        "\u{f702}" => "←".to_owned(),
        "\u{f703}" => "→".to_owned(),
        _ => match key.chars().next() {
            Some(c @ '\u{f704}'..='\u{f726}') => format!("F{}", c as u32 - 0xf703),
            _ => key.to_uppercase(),
        },
    };
    out.push_str(&name);
    out
}

/// Indices into `commands` matching `query`, best first. An empty query
/// keeps menu order, so the list reads like the menu bar.
///
/// Each command scores its best of the title alone and the title after its
/// menu's name, so `git` finds the Git menu's items at a word start rather
/// than ranking any title with a g, an i and a t above them. Keywords only
/// count when neither matches.
pub fn search(commands: &[Command], query: &str) -> Vec<usize> {
    let needle: Vec<char> = query.trim().to_lowercase().chars().collect();
    if needle.is_empty() {
        return (0..commands.len()).collect();
    }
    let points = |text: String| finder::score(&text.to_lowercase(), &needle).map(|(p, _)| p);
    let mut hits: Vec<(i32, usize)> = commands
        .iter()
        .enumerate()
        .filter_map(|(index, command)| {
            let named = points(command.title.clone())
                .into_iter()
                .chain(points(format!("{} {}", command.group, command.title)))
                .max();
            named
                .or_else(|| {
                    points(format!(
                        "{} {} {}",
                        command.group, command.title, command.keywords
                    ))
                    .map(|p| p - KEYWORD_PENALTY)
                })
                .map(|p| (p, index))
        })
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    hits.into_iter().map(|(_, index)| index).collect()
}

/// Keeps a command found only through its keywords below one found by name.
const KEYWORD_PENALTY: i32 = 100;

/// The command query, when the palette's query asks for commands.
pub fn query(text: &str) -> Option<&str> {
    text.strip_prefix('>')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(group: &str, title: &str, keywords: &'static str) -> Command {
        Command {
            title: title.into(),
            group: group.into(),
            shortcut: String::new(),
            keywords,
            action: sel!(copy:),
        }
    }

    fn sample() -> Vec<Command> {
        vec![
            command("File", "Save", ""),
            command("View", "Source Control", "git scm changes diff"),
            command("View", "Terminal", "shell console pty"),
            command("Git", "Commit", ""),
            command("View", "Toggle Sidebar", ""),
            command("Go", "Go to Definition", ""),
        ]
    }

    #[test]
    fn only_a_leading_angle_bracket_asks_for_commands() {
        assert_eq!(query("> ter"), Some(" ter"));
        assert_eq!(query(">"), Some(""));
        assert_eq!(query("src/a>b"), None);
        assert_eq!(query(" >x"), None);
    }

    #[test]
    fn an_empty_command_query_lists_the_menu_in_order() {
        assert_eq!(search(&sample(), "  "), vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn menu_names_count_and_keywords_come_last() {
        let commands = sample();
        assert_eq!(search(&commands, "ter")[0], 2);
        // The Git menu's own item first, though "Go to Definition" has a g,
        // an i and a t in its title; Source Control through its keywords.
        let git = search(&commands, "git");
        assert_eq!(git[0], 3);
        assert!(git.contains(&1));
        assert!(git.iter().position(|&i| i == 1) > git.iter().position(|&i| i == 5));
        assert!(!git.contains(&0));
        assert!(search(&commands, "zzqq").is_empty());
    }

    #[test]
    fn shortcuts_read_like_the_menu_bar() {
        let cmd = NSEventModifierFlags::Command;
        assert_eq!(shortcut("p", cmd), "⌘P");
        assert_eq!(shortcut("o", cmd | NSEventModifierFlags::Shift), "⇧⌘O");
        assert_eq!(shortcut("`", NSEventModifierFlags::Control), "⌃`");
        assert_eq!(shortcut("g", cmd | NSEventModifierFlags::Option), "⌥⌘G");
        assert_eq!(shortcut("\r", cmd), "⌘↩");
        assert_eq!(shortcut("\u{8}", cmd), "⌘⌫");
        assert_eq!(shortcut("\u{F70F}", cmd), "⌘F12");
        assert_eq!(shortcut("", cmd), "");
    }
}
