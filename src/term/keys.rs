//! Keys as a terminal program expects them, xterm style.
//!
//! macOS gives a key code, the characters it types and the modifiers; a
//! program on a terminal wants bytes. Text goes as UTF-8, Control-letter as
//! the control character, and the keys that type nothing as escape
//! sequences, in application cursor mode when the program asked for it.
//! Option-Left and Option-Right move by word and Option-Delete deletes one,
//! as they do in any Mac text field; Shift-Return is a new line for
//! programs that take one (`claude` does), sent as Escape then Return.

/// A key press, as the window saw it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Key {
    /// The hardware key code (`kVK_*`).
    pub code: u16,
    /// What the key types with its modifiers applied.
    pub chars: String,
    /// What it types without them, for Control combinations.
    pub bare: String,
    pub shift: bool,
    pub control: bool,
    pub option: bool,
}

const RETURN: u16 = 36;
const KEYPAD_ENTER: u16 = 76;
const TAB: u16 = 48;
const DELETE: u16 = 51;
const ESCAPE: u16 = 53;
const FORWARD_DELETE: u16 = 117;
const HOME: u16 = 115;
const END: u16 = 119;
const PAGE_UP: u16 = 116;
const PAGE_DOWN: u16 = 121;
const LEFT: u16 = 123;
const RIGHT: u16 = 124;
const DOWN: u16 = 125;
const UP: u16 = 126;

/// The bytes for `key`, or `None` when it sends nothing.
pub fn encode(key: &Key, app_cursor: bool) -> Option<Vec<u8>> {
    // xterm's modifier parameter: 1 plus shift 1, alt 2, control 4.
    let modifier = 1 + key.shift as u8 + 2 * key.option as u8 + 4 * key.control as u8;
    let cursor = |letter: char| {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{letter}")
        } else if app_cursor {
            format!("\x1bO{letter}")
        } else {
            format!("\x1b[{letter}")
        }
        .into_bytes()
    };
    let tilde = |n: u8| {
        if modifier > 1 {
            format!("\x1b[{n};{modifier}~")
        } else {
            format!("\x1b[{n}~")
        }
        .into_bytes()
    };
    Some(match key.code {
        RETURN | KEYPAD_ENTER if key.shift || key.option => b"\x1b\r".to_vec(),
        RETURN | KEYPAD_ENTER => b"\r".to_vec(),
        TAB if key.shift => b"\x1b[Z".to_vec(),
        TAB => b"\t".to_vec(),
        DELETE if key.option => b"\x1b\x7f".to_vec(),
        DELETE if key.control => b"\x08".to_vec(),
        DELETE => b"\x7f".to_vec(),
        ESCAPE => b"\x1b".to_vec(),
        FORWARD_DELETE => tilde(3),
        PAGE_UP => tilde(5),
        PAGE_DOWN => tilde(6),
        LEFT if key.option && !key.shift && !key.control => b"\x1bb".to_vec(),
        RIGHT if key.option && !key.shift && !key.control => b"\x1bf".to_vec(),
        UP => cursor('A'),
        DOWN => cursor('B'),
        RIGHT => cursor('C'),
        LEFT => cursor('D'),
        HOME => cursor('H'),
        END => cursor('F'),
        _ if key.control => control(&key.bare)?,
        _ if key.chars.is_empty() => return None,
        // The private-use characters AppKit gives function keys type nothing.
        _ if key
            .chars
            .chars()
            .all(|c| ('\u{F700}'..='\u{F8FF}').contains(&c)) =>
        {
            return None;
        }
        _ => key.chars.clone().into_bytes(),
    })
}

/// Control plus a key: the letters and the handful of punctuation keys
/// that have a control character.
fn control(bare: &str) -> Option<Vec<u8>> {
    let ch = bare.chars().next()?.to_ascii_lowercase();
    let byte = match ch {
        'a'..='z' => ch as u8 - b'a' + 1,
        ' ' | '2' | '@' => 0,
        '[' | '3' => 0x1B,
        '\\' | '4' => 0x1C,
        ']' | '5' => 0x1D,
        '6' | '^' => 0x1E,
        '/' | '-' | '_' | '7' => 0x1F,
        '8' | '?' => 0x7F,
        _ => return None,
    };
    Some(vec![byte])
}

/// Text to paste. Inside bracketed paste the program is told where it
/// starts and ends, and the end marker cannot be smuggled inside it.
pub fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        let clean = text.replace("\x1b[201~", "");
        format!("\x1b[200~{clean}\x1b[201~").into_bytes()
    } else {
        text.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: u16, chars: &str) -> Key {
        Key {
            code,
            chars: chars.into(),
            bare: chars.into(),
            ..Key::default()
        }
    }

    #[test]
    fn text_and_editing_keys() {
        assert_eq!(encode(&key(0, "a"), false), Some(b"a".to_vec()));
        assert_eq!(encode(&key(0, "é"), false), Some("é".as_bytes().to_vec()));
        assert_eq!(encode(&key(RETURN, "\r"), false), Some(b"\r".to_vec()));
        assert_eq!(
            encode(
                &Key {
                    shift: true,
                    ..key(RETURN, "\r")
                },
                false
            ),
            Some(b"\x1b\r".to_vec())
        );
        assert_eq!(encode(&key(DELETE, "\x7f"), false), Some(b"\x7f".to_vec()));
        assert_eq!(
            encode(
                &Key {
                    option: true,
                    ..key(DELETE, "\x7f")
                },
                false
            ),
            Some(b"\x1b\x7f".to_vec())
        );
        assert_eq!(
            encode(
                &Key {
                    shift: true,
                    ..key(TAB, "\t")
                },
                false
            ),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(encode(&key(ESCAPE, "\x1b"), false), Some(b"\x1b".to_vec()));
        assert_eq!(
            encode(&key(FORWARD_DELETE, "\u{F728}"), false),
            Some(b"\x1b[3~".to_vec())
        );
        assert_eq!(
            encode(&key(122, "\u{F704}"), false),
            None,
            "F1 types nothing yet"
        );
    }

    #[test]
    fn cursor_keys_follow_the_mode_and_modifiers() {
        assert_eq!(encode(&key(UP, ""), false), Some(b"\x1b[A".to_vec()));
        assert_eq!(encode(&key(UP, ""), true), Some(b"\x1bOA".to_vec()));
        assert_eq!(
            encode(
                &Key {
                    shift: true,
                    ..key(RIGHT, "")
                },
                true
            ),
            Some(b"\x1b[1;2C".to_vec())
        );
        assert_eq!(
            encode(
                &Key {
                    option: true,
                    ..key(LEFT, "")
                },
                false
            ),
            Some(b"\x1bb".to_vec())
        );
        assert_eq!(encode(&key(END, ""), false), Some(b"\x1b[F".to_vec()));
    }

    #[test]
    fn control_combinations() {
        let ctrl = |bare: &str| Key {
            control: true,
            bare: bare.into(),
            chars: String::new(),
            ..Key::default()
        };
        assert_eq!(encode(&ctrl("c"), false), Some(vec![3]));
        assert_eq!(encode(&ctrl("C"), false), Some(vec![3]));
        assert_eq!(encode(&ctrl(" "), false), Some(vec![0]));
        assert_eq!(encode(&ctrl("["), false), Some(vec![0x1b]));
        assert_eq!(encode(&ctrl("1"), false), None);
    }

    #[test]
    fn pastes_are_bracketed_and_cannot_close_early() {
        assert_eq!(paste("a\nb", false), b"a\rb".to_vec());
        assert_eq!(paste("x\x1b[201~y", true), b"\x1b[200~xy\x1b[201~".to_vec());
    }
}
