//! Reversible disk encoding around the UTF-8, LF-only editing rope.

use std::io::{self, Write};

use super::rope::Rope;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Windows1252,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    Lf,
    CrLf,
    Cr,
}

#[derive(Clone, Debug)]
pub struct DiskFormat {
    pub encoding: Encoding,
    pub bom: bool,
    /// The endings present at open, one per newline. Extra newlines use the
    /// dominant style. This preserves mixed endings when edits leave the
    /// line count alone, without putting CR characters inside the rope.
    pub endings: Vec<Ending>,
    pub preferred: Ending,
}

impl Default for DiskFormat {
    fn default() -> Self {
        Self {
            encoding: Encoding::Utf8,
            bom: false,
            endings: Vec::new(),
            preferred: Ending::Lf,
        }
    }
}

impl DiskFormat {
    pub fn label(&self) -> &'static str {
        match self.encoding {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf16Le => "UTF-16 LE",
            Encoding::Utf16Be => "UTF-16 BE",
            Encoding::Windows1252 => "Windows-1252",
        }
    }

    pub fn line_ending_label(&self) -> &'static str {
        if self.endings.iter().any(|e| *e != self.preferred) {
            "mixed endings"
        } else {
            match self.preferred {
                Ending::Lf => "LF",
                Ending::CrLf => "CRLF",
                Ending::Cr => "CR",
            }
        }
    }
}

pub fn decode(bytes: &[u8]) -> io::Result<(String, DiskFormat)> {
    let (text, encoding, bom) = if let Some(rest) = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        (
            String::from_utf8(rest.to_vec()).map_err(invalid_utf8)?,
            Encoding::Utf8,
            true,
        )
    } else if let Some(rest) = bytes.strip_prefix(&[0xff, 0xfe]) {
        (decode_utf16(rest, true)?, Encoding::Utf16Le, true)
    } else if let Some(rest) = bytes.strip_prefix(&[0xfe, 0xff]) {
        (decode_utf16(rest, false)?, Encoding::Utf16Be, true)
    } else if let Ok(text) = std::str::from_utf8(bytes) {
        (text.to_string(), Encoding::Utf8, false)
    } else {
        if bytes.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "binary file without a text encoding marker",
            ));
        }
        (
            bytes.iter().map(|b| decode_1252(*b)).collect(),
            Encoding::Windows1252,
            false,
        )
    };
    let mut normalized = String::with_capacity(text.len());
    let mut endings = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
                endings.push(Ending::CrLf);
            } else {
                endings.push(Ending::Cr);
            }
            normalized.push('\n');
        } else if ch == '\n' {
            endings.push(Ending::Lf);
            normalized.push('\n');
        } else {
            normalized.push(ch);
        }
    }
    let mut preferred = Ending::Lf;
    let mut most = 0;
    for style in [Ending::Lf, Ending::CrLf, Ending::Cr] {
        let count = endings.iter().filter(|e| **e == style).count();
        if count > most {
            preferred = style;
            most = count;
        }
    }
    Ok((
        normalized,
        DiskFormat {
            encoding,
            bom,
            endings,
            preferred,
        },
    ))
}

fn invalid_utf8(error: std::string::FromUtf8Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn decode_utf16(bytes: &[u8], little: bool) -> io::Result<String> {
    if !bytes.len().is_multiple_of(2) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "odd-length UTF-16 file",
        ));
    }
    let units = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            if little {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            }
        })
        .collect::<Vec<_>>();
    String::from_utf16(&units).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

const CP1252: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}',
    '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
];

fn decode_1252(byte: u8) -> char {
    if (0x80..=0x9f).contains(&byte) {
        CP1252[(byte - 0x80) as usize]
    } else {
        char::from_u32(byte as u32).expect("byte is a scalar")
    }
}

fn encode_1252(ch: char) -> Option<u8> {
    let value = ch as u32;
    if value <= 0x7f || (0xa0..=0xff).contains(&value) {
        return Some(value as u8);
    }
    CP1252.iter().position(|c| *c == ch).map(|i| i as u8 + 0x80)
}

pub fn write<W: Write>(mut out: W, rope: &Rope, format: &DiskFormat) -> io::Result<()> {
    if format.encoding == Encoding::Utf8
        && format.preferred == Ending::Lf
        && format.endings.iter().all(|ending| *ending == Ending::Lf)
    {
        if format.bom {
            out.write_all(&[0xef, 0xbb, 0xbf])?;
        }
        for chunk in rope.chunks_in(0..rope.len_bytes()) {
            out.write_all(chunk.as_bytes())?;
        }
        return Ok(());
    }
    match (format.encoding, format.bom) {
        (Encoding::Utf8, true) => out.write_all(&[0xef, 0xbb, 0xbf])?,
        (Encoding::Utf16Le, true) => out.write_all(&[0xff, 0xfe])?,
        (Encoding::Utf16Be, true) => out.write_all(&[0xfe, 0xff])?,
        _ => {}
    }
    let mut newline = 0;
    for chunk in rope.chunks_in(0..rope.len_bytes()) {
        for ch in chunk.chars() {
            if ch == '\n' {
                let ending = format
                    .endings
                    .get(newline)
                    .copied()
                    .unwrap_or(format.preferred);
                newline += 1;
                match ending {
                    Ending::Lf => write_char(&mut out, '\n', format.encoding)?,
                    Ending::CrLf => {
                        write_char(&mut out, '\r', format.encoding)?;
                        write_char(&mut out, '\n', format.encoding)?;
                    }
                    Ending::Cr => write_char(&mut out, '\r', format.encoding)?,
                }
            } else {
                write_char(&mut out, ch, format.encoding)?;
            }
        }
    }
    Ok(())
}

/// Check representability before opening a hard-linked destination for an
/// in-place write. Failure here must leave its old bytes untouched.
pub fn validate(rope: &Rope, format: &DiskFormat) -> io::Result<()> {
    if format.encoding == Encoding::Windows1252 {
        for chunk in rope.chunks_in(0..rope.len_bytes()) {
            if let Some(ch) = chunk.chars().find(|ch| encode_1252(*ch).is_none()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("character {ch:?} cannot be saved as Windows-1252"),
                ));
            }
        }
    }
    Ok(())
}

fn write_char<W: Write>(out: &mut W, ch: char, encoding: Encoding) -> io::Result<()> {
    match encoding {
        Encoding::Utf8 => {
            let mut bytes = [0; 4];
            out.write_all(ch.encode_utf8(&mut bytes).as_bytes())
        }
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let mut units = [0; 2];
            for unit in ch.encode_utf16(&mut units).iter() {
                let bytes = if encoding == Encoding::Utf16Le {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                };
                out.write_all(&bytes)?;
            }
            Ok(())
        }
        Encoding::Windows1252 => {
            let byte = encode_1252(ch).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("character {ch:?} cannot be saved as Windows-1252"),
                )
            })?;
            out.write_all(&[byte])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_crlf_and_utf8_bom() {
        let raw = b"\xef\xbb\xbfOne\r\nTwo\r\n";
        let (text, format) = decode(raw).unwrap();
        assert_eq!(text, "One\nTwo\n");
        let mut out = Vec::new();
        write(&mut out, &Rope::from_text(&text), &format).unwrap();
        assert_eq!(out, raw);
    }

    #[test]
    fn roundtrips_utf16_and_cp1252() {
        for raw in [&b"\xff\xfeA\0\r\0\n\0"[..], &b"caf\xe9\r\n"[..]] {
            let (text, format) = decode(raw).unwrap();
            let mut out = Vec::new();
            write(&mut out, &Rope::from_text(&text), &format).unwrap();
            assert_eq!(out, raw);
        }
    }

    #[test]
    fn refuses_unrepresentable_character() {
        let (_, format) = decode(b"caf\xe9").unwrap();
        assert!(write(Vec::new(), &Rope::from_text("emoji 😀"), &format).is_err());
    }
}
