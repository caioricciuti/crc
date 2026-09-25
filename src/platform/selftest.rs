//! A script of input events, played into the real window.
//!
//! Everything in `window.rs` sits behind AppKit, where a unit test cannot go,
//! and that is where the bugs have been: a green suite sat on top of clicks
//! that landed two rows low. This is the way in. Set `CRC_SELFTEST` to a
//! script and the app plays it after launch, as real `NSEvent`s sent through
//! the window's own `sendEvent:`, so they take the path a person's keys and
//! clicks take: the responder chain, `keyDown:`, the input system, hit
//! testing. Then it writes down what it sees.
//!
//! One command per line, `#` for comments:
//!
//! ```text
//! key <code> [mods] [chars]   a key press; mods are any of cmd,shift,opt,ctrl
//!                             or `-`; chars is what the key types, if anything
//! text <string>               one key press per character of <string>
//! click <x> <y> [count]       a left click at a point in the view, in points
//! click @<region> [count]     a left click on the centre of a named region
//!                             of the last frame (`sidebar.action.0`,
//!                             `tab.close.1`, `response.segment.2`)
//! down <x> <y> [count]        press and hold, for a drag that follows
//! clickin @<region> <dx> <dy> [count] [mods]
//!                             a click at an offset from a region's top-left
//!                             corner, with modifiers as for `key`
//! down @<region>              press and hold on the centre of a region
//! drag <x> <y>                a drag to a point, mouse still down
//! dragby <dx> <dy>            a drag to a point relative to the press
//! up <x> <y>                  release
//! upby <dx> <dy>              release relative to the press
//! resize <width> <height>     resize the native window in points
//! touch <file>                create a file without notifying the project UI
//! write <file> <text>         overwrite a file, as another program would
//! dump <file>                 write the state of the active document
//! quit
//! ```
//!
//! Key codes are the hardware ones (`kVK_*`), since that is what the input
//! system reads to decide what a dead key is. This module is only the
//! parsing and the report; the part that touches AppKit is in `window.rs`.

use crate::text::buffer::Buffer;

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Key {
        code: u16,
        mods: Mods,
        chars: String,
    },
    Click {
        x: f64,
        y: f64,
        count: isize,
    },
    /// A click on a named region, resolved against the frame when it runs.
    ClickNamed {
        name: String,
        count: isize,
    },
    Down {
        x: f64,
        y: f64,
        count: isize,
    },
    /// A click at an offset into a named region, with modifiers.
    ClickIn {
        name: String,
        dx: f64,
        dy: f64,
        count: isize,
        mods: Mods,
    },
    /// A press on a named region, resolved when it runs.
    DownNamed {
        name: String,
    },
    /// A drag or release relative to where the last press was.
    DragBy {
        dx: f64,
        dy: f64,
    },
    UpBy {
        dx: f64,
        dy: f64,
    },
    Drag {
        x: f64,
        y: f64,
    },
    Up {
        x: f64,
        y: f64,
    },
    Resize {
        width: f64,
        height: f64,
    },
    Touch(String),
    Write(String, String),
    /// A mouse-wheel delta in notches, as `scrollingDeltaY` reports them.
    /// Only the palette is driven this way.
    Wheel(f64),
    /// A trackpad scroll at a point, `dy` in points as `scrollingDeltaY`
    /// reports them with precise deltas: positive moves the text down.
    Trackpad {
        x: f64,
        y: f64,
        dy: f64,
    },
    Dump(String),
    /// A real panic, so the crash hook and recovery run end to end.
    Panic,
    Wait(u64),
    Quit,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mods {
    pub command: bool,
    pub shift: bool,
    pub option: bool,
    pub control: bool,
}

/// Hardware key codes for the characters `text` knows how to type, on any
/// ANSI layout. Letters, digits, space and a little punctuation: enough to
/// write a test, not a keyboard driver.
fn key_for(ch: char) -> Option<(u16, bool)> {
    const LETTERS: [u16; 26] = [
        0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12, 15, 1, 17, 32, 9, 13, 7, 16,
        6,
    ];
    const DIGITS: [u16; 10] = [29, 18, 19, 20, 21, 23, 22, 26, 28, 25];
    Some(match ch {
        'a'..='z' => (LETTERS[ch as usize - 'a' as usize], false),
        'A'..='Z' => (LETTERS[ch as usize - 'A' as usize], true),
        '0'..='9' => (DIGITS[ch as usize - '0' as usize], false),
        ' ' => (49, false),
        '.' => (47, false),
        '>' => (47, true),
        '/' => (44, false),
        ':' => (41, true),
        ',' => (43, false),
        ';' => (41, false),
        '=' => (24, false),
        '-' => (27, false),
        '(' => (25, true),
        ')' => (29, true),
        '@' => (19, true),
        '#' => (20, true),
        '_' => (27, true),
        '{' => (33, true),
        '}' => (30, true),
        _ => return None,
    })
}

/// Parses a script. A line that does not parse is an error with its number,
/// because a test that silently skips a step passes for the wrong reason.
pub fn parse(script: &str) -> Result<Vec<Step>, String> {
    let mut steps = Vec::new();
    for (n, line) in script.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = || format!("line {}: cannot read {line:?}", n + 1);
        let (command, rest) = line.split_once(' ').unwrap_or((line, ""));
        let mut words = rest.split_whitespace();
        let mut number = || words.next().and_then(|w| w.parse::<f64>().ok());
        match command {
            "key" => {
                let mut words = rest.split_whitespace();
                let code = words.next().and_then(|w| w.parse().ok()).ok_or_else(bad)?;
                let mut mods = Mods::default();
                for name in words.next().unwrap_or("-").split(',') {
                    match name {
                        "cmd" => mods.command = true,
                        "shift" => mods.shift = true,
                        "opt" => mods.option = true,
                        "ctrl" => mods.control = true,
                        "-" | "" => {}
                        _ => return Err(bad()),
                    }
                }
                let chars = words.next().unwrap_or("").to_string();
                steps.push(Step::Key { code, mods, chars });
            }
            "text" => {
                for ch in rest.chars() {
                    let (code, shift) = key_for(ch).ok_or_else(bad)?;
                    steps.push(Step::Key {
                        code,
                        mods: Mods {
                            shift,
                            ..Mods::default()
                        },
                        chars: ch.to_string(),
                    });
                }
            }
            "click" if rest.starts_with('@') => {
                let mut words = rest[1..].split_whitespace();
                let name = words.next().ok_or_else(bad)?.to_string();
                let count = words.next().and_then(|w| w.parse().ok()).unwrap_or(1);
                steps.push(Step::ClickNamed { name, count });
            }
            "click" => {
                let (x, y) = (number().ok_or_else(bad)?, number().ok_or_else(bad)?);
                let count = number().map_or(1, |c| c as isize);
                steps.push(Step::Click { x, y, count });
            }
            "clickin" if rest.starts_with('@') => {
                let mut words = rest[1..].split_whitespace();
                let name = words.next().ok_or_else(bad)?.to_string();
                let mut next = || words.next().and_then(|w| w.parse::<f64>().ok());
                let (dx, dy) = (next().ok_or_else(bad)?, next().ok_or_else(bad)?);
                let count = next().map_or(1, |c| c as isize);
                let mut mods = Mods::default();
                for name in words.next().unwrap_or("-").split(',') {
                    match name {
                        "cmd" => mods.command = true,
                        "shift" => mods.shift = true,
                        "opt" => mods.option = true,
                        "ctrl" => mods.control = true,
                        "-" | "" => {}
                        _ => return Err(bad()),
                    }
                }
                steps.push(Step::ClickIn {
                    name,
                    dx,
                    dy,
                    count,
                    mods,
                });
            }
            "down" if rest.starts_with('@') => steps.push(Step::DownNamed {
                name: rest[1..].trim().to_string(),
            }),
            "dragby" => steps.push(Step::DragBy {
                dx: number().ok_or_else(bad)?,
                dy: number().ok_or_else(bad)?,
            }),
            "upby" => steps.push(Step::UpBy {
                dx: number().ok_or_else(bad)?,
                dy: number().ok_or_else(bad)?,
            }),
            "down" => {
                let (x, y) = (number().ok_or_else(bad)?, number().ok_or_else(bad)?);
                let count = number().map_or(1, |c| c as isize);
                steps.push(Step::Down { x, y, count });
            }
            "drag" => steps.push(Step::Drag {
                x: number().ok_or_else(bad)?,
                y: number().ok_or_else(bad)?,
            }),
            "up" => steps.push(Step::Up {
                x: number().ok_or_else(bad)?,
                y: number().ok_or_else(bad)?,
            }),
            "resize" => steps.push(Step::Resize {
                width: number().ok_or_else(bad)?,
                height: number().ok_or_else(bad)?,
            }),
            "touch" if !rest.is_empty() => steps.push(Step::Touch(rest.to_string())),
            "write" if !rest.is_empty() => {
                let (file, text) = rest.split_once(' ').unwrap_or((rest, ""));
                steps.push(Step::Write(file.to_string(), text.to_string()));
            }
            "dump" if !rest.is_empty() => steps.push(Step::Dump(rest.to_string())),
            "wait" => steps.push(Step::Wait(rest.parse().map_err(|_| bad())?)),
            "wheel" => steps.push(Step::Wheel(rest.trim().parse().map_err(|_| bad())?)),
            "trackpad" => steps.push(Step::Trackpad {
                x: number().ok_or_else(bad)?,
                y: number().ok_or_else(bad)?,
                dy: number().ok_or_else(bad)?,
            }),
            "quit" => steps.push(Step::Quit),
            "panic" => steps.push(Step::Panic),
            _ => return Err(bad()),
        }
    }
    Ok(steps)
}

/// What a `dump` writes: enough to tell whether the keys and clicks did what
/// they were meant to, in a form a shell script can grep.
pub fn report(
    buffer: &Buffer,
    titles: &[String],
    active: usize,
    marked: Option<&str>,
    layout: &str,
    previews: (bool, bool),
    live: bool,
) -> String {
    let (line, column) = buffer.cursor_position();
    let mut out = String::new();
    out.push_str(&format!("layout: {layout}\n"));
    out.push_str(&format!("tabs: {}\n", titles.join(" | ")));
    out.push_str(&format!("active: {active}\n"));
    out.push_str(&format!("preview: {}\n", previews.0));
    out.push_str(&format!("native_preview: {}\n", previews.1));
    out.push_str(&format!("live: {live}\n"));
    out.push_str(&format!("cursor: {}:{}\n", line + 1, column + 1));
    out.push_str(&format!("cursors: {}\n", buffer.cursor_count()));
    out.push_str(&format!("scroll: {}\n", buffer.scroll_line));
    out.push_str(&format!("scroll_fraction: {:.2}\n", buffer.scroll_fraction));
    out.push_str(&format!(
        "folds: {}\n",
        buffer
            .folds
            .iter()
            .map(|(a, b)| format!("{}-{}", a + 1, b + 1))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    out.push_str(&format!(
        "wrap: {} row {}\n",
        buffer.wrap.map_or("off".to_string(), |c| c.to_string()),
        buffer.scroll_row
    ));
    out.push_str(&format!(
        "selection: {:?}\n",
        buffer.selected_text().unwrap_or_default()
    ));
    out.push_str(&format!("marked: {:?}\n", marked.unwrap_or("")));
    out.push_str(&format!("dirty: {}\n", buffer.is_dirty()));
    out.push_str("text:\n");
    out.push_str(&buffer.rope.to_string());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_command() {
        let steps = parse(
            "# a comment\n\nkey 14 opt\nkey 36\nkey 1 cmd,shift s\ntext aB 1\nclick 300.5 120 2\nclick @sidebar.action.0 2\ndrag 10 20\nup 10 20\ndump /tmp/out.txt\nquit\n",
        )
        .expect("parses");
        assert_eq!(
            steps[0],
            Step::Key {
                code: 14,
                mods: Mods {
                    option: true,
                    ..Mods::default()
                },
                chars: String::new()
            }
        );
        assert_eq!(
            steps[1],
            Step::Key {
                code: 36,
                mods: Mods::default(),
                chars: String::new()
            }
        );
        assert_eq!(
            steps[2],
            Step::Key {
                code: 1,
                mods: Mods {
                    command: true,
                    shift: true,
                    ..Mods::default()
                },
                chars: "s".into()
            }
        );
        // `text aB 1`: a, shifted b, space, 1.
        assert_eq!(
            steps[3],
            Step::Key {
                code: 0,
                mods: Mods::default(),
                chars: "a".into()
            }
        );
        assert_eq!(
            steps[4],
            Step::Key {
                code: 11,
                mods: Mods {
                    shift: true,
                    ..Mods::default()
                },
                chars: "B".into()
            }
        );
        assert_eq!(
            steps[7],
            Step::Click {
                x: 300.5,
                y: 120.0,
                count: 2
            }
        );
        assert_eq!(steps.last(), Some(&Step::Quit));
    }

    #[test]
    fn a_line_it_cannot_read_is_an_error_not_a_skipped_step() {
        assert!(parse("click 10").is_err());
        assert!(parse("key abc").is_err());
        assert!(parse("key 1 hyper").is_err());
        assert!(parse("text \u{e9}").is_err());
        assert!(parse("teleport 1 2").unwrap_err().contains("line 1"));
    }
}
