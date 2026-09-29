//! Bytes from the program into the parser's state: UTF-8, controls,
//! escapes, CSI parameters and strings.

use super::*;

impl Term {
    /// Feeds the program's output.
    pub fn advance(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.byte(byte);
        }
        self.generation += 1;
    }
    pub(super) fn byte(&mut self, byte: u8) {
        // Strings end at ST (ESC \) or BEL and take everything else.
        if matches!(self.state, State::Osc | State::Ignore) {
            if self.string_escape {
                self.string_escape = false;
                if byte == b'\\' {
                    self.end_string();
                    return;
                }
                // ESC ESC inside a device-control string is tmux passing an
                // escape through to the outer terminal: part of the string.
                if byte == 0x1B && self.state == State::Ignore {
                    return;
                }
                // ESC and anything but a backslash: the string was cut.
                self.end_string();
                self.state = State::Escape;
                self.escape(byte);
                return;
            }
            match byte {
                0x07 => self.end_string(),
                0x1B => self.string_escape = true,
                0x18 | 0x1A => self.state = State::Ground,
                _ if self.state == State::Osc && self.osc.len() < MAX_OSC => self.osc.push(byte),
                _ => {}
            }
            return;
        }
        // C0 controls act anywhere, even inside a sequence.
        match byte {
            0x1B => {
                self.utf8.clear();
                self.state = State::Escape;
                return;
            }
            0x18 | 0x1A => {
                self.state = State::Ground;
                return;
            }
            0x00..=0x1F => {
                self.control(byte);
                return;
            }
            _ => {}
        }
        match self.state.clone() {
            State::Ground => self.ground(byte),
            State::Escape => self.escape(byte),
            State::EscapeIntermediate(i) => {
                self.escape_intermediate(i, byte);
                self.state = State::Ground;
            }
            State::Csi => self.csi_byte(byte),
            State::Osc | State::Ignore => unreachable!("handled above"),
        }
    }
    pub(super) fn ground(&mut self, byte: u8) {
        if byte < 0x80 {
            self.utf8.clear();
            if byte != 0x7F {
                self.print(byte as char);
            }
            return;
        }
        // UTF-8: collect the bytes of one character.
        if byte & 0xC0 != 0x80 {
            self.utf8.clear();
        }
        self.utf8.push(byte);
        let expected = match self.utf8[0] {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => {
                self.utf8.clear();
                self.print(char::REPLACEMENT_CHARACTER);
                return;
            }
        };
        if self.utf8.len() < expected {
            return;
        }
        let ch = std::str::from_utf8(&self.utf8)
            .ok()
            .and_then(|s| s.chars().next())
            .unwrap_or(char::REPLACEMENT_CHARACTER);
        self.utf8.clear();
        self.print(ch);
    }
    pub(super) fn control(&mut self, byte: u8) {
        match byte {
            0x08 => {
                self.col = self.col.saturating_sub(1);
                self.pending_wrap = false;
            }
            0x09 => self.tab_forward(1),
            0x0A..=0x0C => self.linefeed(),
            0x0D => {
                self.col = 0;
                self.pending_wrap = false;
            }
            // SO and SI shift to G1 and back; G1 is never line drawing here.
            0x0E | 0x0F => {}
            _ => {}
        }
    }
    pub(super) fn escape(&mut self, byte: u8) {
        self.state = State::Ground;
        match byte {
            b'[' => {
                self.params.clear();
                self.params.push(Vec::new());
                self.private = None;
                self.intermediate = None;
                self.state = State::Csi;
            }
            b']' => {
                self.osc.clear();
                self.state = State::Osc;
            }
            b'P' | b'X' | b'^' | b'_' => self.state = State::Ignore,
            b'(' | b')' | b'*' | b'+' | b'#' | b' ' | b'%' => {
                self.state = State::EscapeIntermediate(byte)
            }
            b'7' => self.save_cursor(),
            b'8' => self.restore_cursor(),
            b'D' => self.linefeed(),
            b'E' => {
                self.col = 0;
                self.linefeed();
            }
            b'M' => self.reverse_index(),
            b'H' => {
                if let Some(stop) = self.tabs.get_mut(self.col) {
                    *stop = true;
                }
            }
            b'c' => self.reset(),
            // Keypad application and numeric modes: keys are sent the same.
            b'=' | b'>' => {}
            _ => {}
        }
    }
    pub(super) fn escape_intermediate(&mut self, intermediate: u8, byte: u8) {
        if intermediate == b'(' {
            self.line_drawing = byte == b'0';
        }
    }
    pub(super) fn csi_byte(&mut self, byte: u8) {
        match byte {
            b'0'..=b'9' => {
                let digit = (byte - b'0') as u16;
                let params = self.current_param();
                match params.last_mut() {
                    Some(last) => *last = last.saturating_mul(10).saturating_add(digit),
                    None => params.push(digit),
                }
            }
            b';' => {
                if self.params.len() < MAX_PARAMS {
                    self.params.push(Vec::new());
                }
            }
            b':' => {
                let params = self.current_param();
                if params.is_empty() {
                    params.push(0);
                }
                // Bounded like `;`: `cat` of a binary could send megabytes
                // of colons.
                if params.len() < MAX_PARAMS {
                    params.push(0);
                }
            }
            b'<' | b'=' | b'>' | b'?' => self.private = Some(byte),
            0x20..=0x2F => self.intermediate = Some(byte),
            0x40..=0x7E => {
                self.state = State::Ground;
                self.csi(byte);
            }
            _ => self.state = State::Ground,
        }
    }
    /// The parameter being read, with its sub-parameters. A CSI starts
    /// with one; should it not, there is one now.
    pub(super) fn current_param(&mut self) -> &mut Vec<u16> {
        if self.params.is_empty() {
            self.params.push(Vec::new());
        }
        let last = self.params.len() - 1;
        &mut self.params[last]
    }
    /// Parameter `i`, or `default` when absent or zero.
    pub(super) fn param(&self, i: usize, default: usize) -> usize {
        match self.params.get(i).and_then(|p| p.first()) {
            Some(&0) | None => default,
            Some(&n) => n as usize,
        }
    }
    pub(super) fn end_string(&mut self) {
        if self.state == State::Osc {
            let text = String::from_utf8_lossy(&self.osc).into_owned();
            if let Some((kind, value)) = text.split_once(';') {
                match (kind, value) {
                    ("0" | "2", _) => self.title = value.to_owned(),
                    // A query: answered, or the program waits out its timeout.
                    ("10" | "11", "?") => {
                        let [r, g, b] = if kind == "10" {
                            self.colors.0
                        } else {
                            self.colors.1
                        };
                        let reply = format!(
                            "\x1b]{kind};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\"
                        );
                        self.replies.extend_from_slice(reply.as_bytes());
                    }
                    _ => {}
                }
            }
        }
        self.osc.clear();
        self.state = State::Ground;
    }
}
