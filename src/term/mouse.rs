//! Mouse reports, xterm style.
//!
//! A program asks for the mouse with `CSI ? 1000 h` (presses and releases),
//! 1002 (also motion while a button is down) or 1003 (all motion), and
//! picks how the report is spelled: the default X10 bytes, 1006 (SGR, what
//! current programs use) or 1015 (urxvt). Each report names the button,
//! the modifiers and the 1-based cell.

use super::Modes;

/// Which mouse events the program asked for. The last of 1000, 1002 and
/// 1003 set wins; resetting any of them turns reporting off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tracking {
    #[default]
    Off,
    /// 1000: presses and releases, and the wheel.
    Press,
    /// 1002: also motion while a button is held.
    Button,
    /// 1003: all motion.
    Any,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
}

impl Button {
    fn code(self) -> u32 {
        match self {
            Button::Left => 0,
            Button::Middle => 1,
            Button::Right => 2,
            Button::WheelUp => 64,
            Button::WheelDown => 65,
            Button::WheelLeft => 66,
            Button::WheelRight => 67,
        }
    }

    fn wheel(self) -> bool {
        self.code() >= 64
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Press,
    Release,
    /// The pointer moved to another cell; `button` is the one held, if any.
    Motion,
}

/// A mouse event over the screen. `row` and `col` count from 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub action: Action,
    pub button: Option<Button>,
    pub row: usize,
    pub col: usize,
    pub shift: bool,
    pub option: bool,
    pub control: bool,
}

/// X10 reports a coordinate as one byte, 32 plus the 1-based cell, so
/// nothing past cell 223 fits. xterm sends nothing there.
const X10_LAST: usize = 223;

/// The bytes reporting `event` to a program in `modes`, or `None` when it
/// did not ask for this kind of event.
pub fn encode(event: &Event, modes: &Modes) -> Option<Vec<u8>> {
    let wanted = match event.action {
        Action::Press | Action::Release => modes.mouse >= Tracking::Press,
        Action::Motion if event.button.is_some() => modes.mouse >= Tracking::Button,
        Action::Motion => modes.mouse == Tracking::Any,
    };
    if !wanted {
        return None;
    }
    // The wheel has no release.
    if event.action == Action::Release && event.button.is_none_or(Button::wheel) {
        return None;
    }
    let mut code = match (event.action, event.button) {
        (Action::Release, _) if !modes.sgr_mouse => 3,
        (_, Some(button)) => button.code(),
        (_, None) => 3,
    };
    if event.action == Action::Motion {
        code += 32;
    }
    code += 4 * event.shift as u32 + 8 * event.option as u32 + 16 * event.control as u32;
    let (x, y) = (event.col + 1, event.row + 1);
    if modes.sgr_mouse {
        let last = if event.action == Action::Release {
            'm'
        } else {
            'M'
        };
        Some(format!("\x1b[<{code};{x};{y}{last}").into_bytes())
    } else if modes.urxvt_mouse {
        Some(format!("\x1b[{};{x};{y}M", code + 32).into_bytes())
    } else if x > X10_LAST || y > X10_LAST {
        None
    } else {
        Some(vec![
            0x1b,
            b'[',
            b'M',
            (32 + code) as u8,
            (32 + x) as u8,
            (32 + y) as u8,
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modes(mouse: Tracking, sgr: bool) -> Modes {
        Modes {
            mouse,
            sgr_mouse: sgr,
            ..Modes::default()
        }
    }

    fn event(action: Action, button: Option<Button>, row: usize, col: usize) -> Event {
        Event {
            action,
            button,
            row,
            col,
            shift: false,
            option: false,
            control: false,
        }
    }

    fn text(bytes: Option<Vec<u8>>) -> Option<String> {
        bytes.map(|b| String::from_utf8(b).unwrap())
    }

    #[test]
    fn sgr_press_and_release_keep_the_button() {
        let m = modes(Tracking::Press, true);
        let press = event(Action::Press, Some(Button::Left), 4, 9);
        assert_eq!(text(encode(&press, &m)).as_deref(), Some("\x1b[<0;10;5M"));
        let release = event(Action::Release, Some(Button::Right), 0, 0);
        assert_eq!(text(encode(&release, &m)).as_deref(), Some("\x1b[<2;1;1m"));
    }

    #[test]
    fn x10_bytes_offset_by_32_and_release_as_3() {
        let m = modes(Tracking::Press, false);
        let press = event(Action::Press, Some(Button::Middle), 2, 3);
        assert_eq!(encode(&press, &m), Some(b"\x1b[M!$#".to_vec()));
        let release = event(Action::Release, Some(Button::Middle), 2, 3);
        assert_eq!(encode(&release, &m), Some(b"\x1b[M#$#".to_vec()));
    }

    #[test]
    fn x10_skips_cells_past_223() {
        let m = modes(Tracking::Press, false);
        let edge = event(Action::Press, Some(Button::Left), 0, 222);
        assert_eq!(encode(&edge, &m), Some(vec![0x1b, b'[', b'M', 32, 255, 33]));
        assert_eq!(
            encode(&event(Action::Press, Some(Button::Left), 0, 223), &m),
            None
        );
        assert_eq!(
            encode(&event(Action::Press, Some(Button::Left), 300, 0), &m),
            None
        );
        // SGR has no limit.
        let far = event(Action::Press, Some(Button::Left), 0, 300);
        assert_eq!(
            text(encode(&far, &modes(Tracking::Press, true))).as_deref(),
            Some("\x1b[<0;301;1M")
        );
    }

    #[test]
    fn modifiers_add_to_the_button() {
        let m = modes(Tracking::Press, true);
        let mut e = event(Action::Press, Some(Button::Left), 0, 0);
        e.shift = true;
        assert_eq!(text(encode(&e, &m)).as_deref(), Some("\x1b[<4;1;1M"));
        e.option = true;
        assert_eq!(text(encode(&e, &m)).as_deref(), Some("\x1b[<12;1;1M"));
        e.control = true;
        assert_eq!(text(encode(&e, &m)).as_deref(), Some("\x1b[<28;1;1M"));
    }

    #[test]
    fn motion_follows_the_tracking_level() {
        let drag = event(Action::Motion, Some(Button::Left), 1, 1);
        let hover = event(Action::Motion, None, 1, 1);
        let press = modes(Tracking::Press, true);
        assert_eq!(encode(&drag, &press), None);
        assert_eq!(encode(&hover, &press), None);
        let button = modes(Tracking::Button, true);
        assert_eq!(
            text(encode(&drag, &button)).as_deref(),
            Some("\x1b[<32;2;2M")
        );
        assert_eq!(encode(&hover, &button), None);
        let any = modes(Tracking::Any, true);
        assert_eq!(text(encode(&hover, &any)).as_deref(), Some("\x1b[<35;2;2M"));
        assert_eq!(
            encode(&hover, &modes(Tracking::Any, false)),
            Some(b"\x1b[MC\"\"".to_vec())
        );
    }

    #[test]
    fn wheel_reports_a_press_and_no_release() {
        let m = modes(Tracking::Press, true);
        let up = event(Action::Press, Some(Button::WheelUp), 0, 0);
        assert_eq!(text(encode(&up, &m)).as_deref(), Some("\x1b[<64;1;1M"));
        let down = event(Action::Press, Some(Button::WheelDown), 0, 0);
        assert_eq!(text(encode(&down, &m)).as_deref(), Some("\x1b[<65;1;1M"));
        let left = event(Action::Press, Some(Button::WheelLeft), 0, 0);
        assert_eq!(text(encode(&left, &m)).as_deref(), Some("\x1b[<66;1;1M"));
        let x10 = modes(Tracking::Press, false);
        assert_eq!(encode(&up, &x10), Some(b"\x1b[M`!!".to_vec()));
        let release = event(Action::Release, Some(Button::WheelUp), 0, 0);
        assert_eq!(encode(&release, &m), None);
    }

    #[test]
    fn urxvt_spells_the_code_in_decimal() {
        let m = Modes {
            mouse: Tracking::Press,
            urxvt_mouse: true,
            ..Modes::default()
        };
        let press = event(Action::Press, Some(Button::Left), 299, 0);
        assert_eq!(text(encode(&press, &m)).as_deref(), Some("\x1b[32;1;300M"));
        let release = event(Action::Release, Some(Button::Left), 0, 0);
        assert_eq!(text(encode(&release, &m)).as_deref(), Some("\x1b[35;1;1M"));
    }

    #[test]
    fn nothing_without_tracking() {
        let m = modes(Tracking::Off, true);
        assert_eq!(
            encode(&event(Action::Press, Some(Button::Left), 0, 0), &m),
            None
        );
    }
}
