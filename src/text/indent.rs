//! How a document indents: from `.editorconfig` when a file there says, or
//! read from the text itself.
//!
//! Only the indentation properties are applied (`indent_style`,
//! `indent_size`, `tab_width`). The glob subset is what projects use in
//! practice: `*`, `*.ext`, `*.{a,b}`, `**/`, `?`, and a bare file name.

use std::path::Path;

use super::rope::Rope;

/// Tabs, or spaces of a width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub tabs: bool,
    /// Columns per level.
    pub width: usize,
}

impl Style {
    /// One level, as text.
    pub fn unit(self) -> String {
        if self.tabs {
            "\t".into()
        } else {
            " ".repeat(self.width)
        }
    }
}

/// The style for `path`: `.editorconfig` first, then the text, then `None`.
pub fn for_file(path: &Path, rope: &Rope) -> Option<Style> {
    editorconfig(path).or_else(|| detect(rope))
}

/// Reads the text's own indentation from its first thousand lines: tabs if
/// more lines start with a tab than with spaces, otherwise the most common
/// step between consecutive indented lines (2, 4 or 8). `None` when too few
/// lines are indented to tell.
pub fn detect(rope: &Rope) -> Option<Style> {
    let (mut tabs, mut spaces) = (0usize, 0usize);
    let mut steps = [0usize; 9];
    let mut previous = 0usize;
    for line in 0..rope.len_lines().min(1000) {
        let start = rope.line_to_byte(line);
        let end = rope.len_bytes().min(start + 256);
        let head = rope.slice_to_string(start..end);
        let head = head.split('\n').next().unwrap_or("");
        if head.trim().is_empty() {
            continue;
        }
        if head.starts_with('\t') {
            tabs += 1;
            continue;
        }
        let n = head.chars().take_while(|c| *c == ' ').count();
        if n > 0 {
            spaces += 1;
        }
        let step = n.abs_diff(previous);
        if (2..=8).contains(&step) {
            steps[step] += 1;
        }
        previous = n;
    }
    if tabs + spaces < 3 {
        return None;
    }
    if tabs > spaces {
        return Some(Style {
            tabs: true,
            width: 4,
        });
    }
    let width = [2, 4, 8]
        .into_iter()
        .max_by_key(|&w| (steps[w], w == 4))
        .filter(|&w| steps[w] > 0)?;
    Some(Style { tabs: false, width })
}

/// The indentation `.editorconfig` files give `path`, nearest file last so
/// it wins, stopping at one that says `root = true`.
pub fn editorconfig(path: &Path) -> Option<Style> {
    let mut files = Vec::new();
    let mut dir = path.parent();
    while let Some(d) = dir {
        if let Ok(text) = std::fs::read_to_string(d.join(".editorconfig")) {
            let root = text.lines().any(|l| {
                let l = l.trim().to_ascii_lowercase().replace(' ', "");
                l == "root=true"
            });
            files.push((d.to_path_buf(), text));
            if root {
                break;
            }
        }
        dir = d.parent();
    }
    let (mut style, mut size, mut tab_width): (Option<bool>, Option<String>, Option<usize>) =
        (None, None, None);
    for (dir, text) in files.iter().rev() {
        let Ok(relative) = path.strip_prefix(dir) else {
            continue;
        };
        let relative = relative.to_string_lossy();
        let mut applies = false;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                applies = section_matches(section, &relative);
                continue;
            }
            if !applies {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().to_ascii_lowercase();
            match key.trim().to_ascii_lowercase().as_str() {
                "indent_style" => style = Some(value == "tab"),
                "indent_size" => size = Some(value),
                "tab_width" => tab_width = value.parse().ok(),
                _ => {}
            }
        }
    }
    let width = match size.as_deref() {
        Some("tab") => tab_width,
        Some(n) => n.parse().ok(),
        None => None,
    };
    match (style, width) {
        (None, None) => None,
        (tabs, width) => Some(Style {
            tabs: tabs.unwrap_or(false),
            width: width.or(tab_width).unwrap_or(4).clamp(1, 16),
        }),
    }
}

/// Whether an editorconfig section applies to `relative` (with `/`). A
/// pattern without a slash matches the file name anywhere below.
fn section_matches(pattern: &str, relative: &str) -> bool {
    let pattern = pattern.trim();
    let (pattern, subject) = if pattern.contains('/') {
        (pattern.trim_start_matches('/').to_string(), relative)
    } else {
        (
            pattern.to_string(),
            relative.rsplit('/').next().unwrap_or(relative),
        )
    };
    expand_braces(&pattern)
        .iter()
        .any(|p| glob(p.as_bytes(), subject.as_bytes()))
}

/// `*.{js,ts}` to `*.js` and `*.ts`. One level of braces.
fn expand_braces(pattern: &str) -> Vec<String> {
    match (pattern.find('{'), pattern.find('}')) {
        (Some(open), Some(close)) if open < close => pattern[open + 1..close]
            .split(',')
            .map(|alt| format!("{}{}{}", &pattern[..open], alt, &pattern[close + 1..]))
            .collect(),
        _ => vec![pattern.to_string()],
    }
}

/// `*` matches within a path segment, `**` across them, `?` one character.
fn glob(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.first() {
        None => text.is_empty(),
        Some(b'*') if pattern.get(1) == Some(&b'*') => {
            let rest = pattern[2..].strip_prefix(b"/").unwrap_or(&pattern[2..]);
            (0..=text.len()).any(|i| glob(rest, &text[i..]))
        }
        Some(b'*') => (0..=text.len())
            .take_while(|&i| i == 0 || text[i - 1] != b'/')
            .any(|i| glob(&pattern[1..], &text[i..])),
        Some(b'?') => !text.is_empty() && text[0] != b'/' && glob(&pattern[1..], &text[1..]),
        Some(&c) => text.first() == Some(&c) && glob(&pattern[1..], &text[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_tabs_two_and_four_spaces() {
        let tabs = Rope::from_text("fn a() {\n\tx;\n\ty;\n\tz;\n}\n");
        assert_eq!(
            detect(&tabs),
            Some(Style {
                tabs: true,
                width: 4
            })
        );
        let two = Rope::from_text("a:\n  b:\n    c: 1\n  d: 2\ne:\n  f: 3\n");
        assert_eq!(
            detect(&two),
            Some(Style {
                tabs: false,
                width: 2
            })
        );
        let four = Rope::from_text("def a():\n    x = 1\n    if x:\n        y\n    return\n");
        assert_eq!(
            detect(&four),
            Some(Style {
                tabs: false,
                width: 4
            })
        );
        assert_eq!(detect(&Rope::from_text("one line\n")), None);
    }

    #[test]
    fn globs_and_sections() {
        assert!(section_matches("*", "src/a.rs"));
        assert!(section_matches("*.{js,ts}", "web/app.ts"));
        assert!(!section_matches("*.{js,ts}", "web/app.rs"));
        assert!(section_matches("Makefile", "sub/Makefile"));
        assert!(section_matches("src/**.rs", "src/deep/x.rs"));
        assert!(!section_matches("src/*.rs", "src/deep/x.rs"));
        assert!(section_matches("?.md", "a.md"));
    }

    #[test]
    fn nearest_editorconfig_wins_and_root_stops() {
        let dir = std::env::temp_dir().join(format!("crc-editorconfig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("proj/web")).unwrap();
        std::fs::write(dir.join(".editorconfig"), "[*]\nindent_style = tab\n").unwrap();
        std::fs::write(
            dir.join("proj/.editorconfig"),
            "root = true\n[*]\nindent_style = space\nindent_size = 4\n[*.ts]\nindent_size = 2\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("proj/web/.editorconfig"),
            "[Makefile]\nindent_style = tab\n",
        )
        .unwrap();
        let style = |name: &str| editorconfig(&dir.join("proj/web").join(name));
        assert_eq!(
            style("app.ts"),
            Some(Style {
                tabs: false,
                width: 2
            })
        );
        assert_eq!(
            style("app.rs"),
            Some(Style {
                tabs: false,
                width: 4
            })
        );
        // The nearer file's section, and the outer tab setting never reached.
        assert_eq!(
            style("Makefile"),
            Some(Style {
                tabs: true,
                width: 4
            })
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
