//! The few things a user can set, in a file they can also edit by hand.
//!
//! `~/.config/crc/config.toml`, a flat list of `key = value` lines. The
//! reader understands exactly that subset of TOML: quoted strings, numbers,
//! `#` comments. No tables, no arrays, no dependency. The writer changes the
//! one line it has to and leaves everything else in the file as it found it,
//! comments included, so zooming in the app does not trample a hand edit.

use std::path::PathBuf;

/// The default code font, and the size the UI font stays at.
pub const DEFAULT_FONT: &str = "SF Mono";
pub const DEFAULT_FONT_SIZE: f32 = 13.0;
pub const MIN_FONT_SIZE: f32 = 8.0;
pub const MAX_FONT_SIZE: f32 = 32.0;

/// Which appearance to draw in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ThemeChoice {
    /// Follow the system setting.
    #[default]
    System,
    Dark,
    Light,
}

impl ThemeChoice {
    fn parse(value: &str) -> Option<ThemeChoice> {
        match value {
            "system" => Some(ThemeChoice::System),
            "dark" => Some(ThemeChoice::Dark),
            "light" => Some(ThemeChoice::Light),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            ThemeChoice::System => "system",
            ThemeChoice::Dark => "dark",
            ThemeChoice::Light => "light",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub font: String,
    pub font_size: f32,
    pub theme: ThemeChoice,
    /// Whether the caret blinks. It is solid while you type either way.
    pub caret_blink: bool,
    /// Whether launch asks GitHub, at most daily, for a newer release.
    pub update_check: bool,
    /// Whether saving asks the language server to format the file first.
    pub format_on_save: bool,
    /// Whether saving asks the language server to organize the imports,
    /// before any format.
    pub organize_imports_on_save: bool,
    /// Which documents wrap long lines.
    pub word_wrap: WordWrap,
    /// The SSH agent socket for fetch, pull and push. An app opened from
    /// the Dock does not get the shell's `SSH_AUTH_SOCK`, and crc does not
    /// read the shell profile, so it is given here.
    pub ssh_auth_sock: Option<PathBuf>,
    /// How a file with merge conflicts opens: in the text, or as columns.
    pub conflict_side_by_side: bool,
}

/// `word_wrap`: every document, none, or prose (Markdown and text) only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordWrap {
    Auto,
    On,
    Off,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            font: DEFAULT_FONT.to_owned(),
            font_size: DEFAULT_FONT_SIZE,
            theme: ThemeChoice::System,
            caret_blink: true,
            update_check: true,
            format_on_save: false,
            organize_imports_on_save: false,
            word_wrap: WordWrap::Auto,
            ssh_auth_sock: None,
            conflict_side_by_side: false,
        }
    }
}

/// What a fresh settings file says: every key, with what it takes.
pub const TEMPLATE: &str = "\
# crc settings. The app rewrites its own keys when you zoom or pick a
# theme and leaves every other line alone, so edit freely. Saving this
# file in crc applies it.

# Any monospace face installed on this Mac.
font = \"SF Mono\"

# 8 to 32 points. Cmd-= and Cmd-- change it; Cmd-0 goes back to 13.
font_size = 13

# \"system\", \"dark\" or \"light\".
theme = \"system\"

# false keeps the caret solid.
caret_blink = true

# Once a day at launch, ask GitHub whether a newer release exists. Nothing
# is downloaded; Help > Check for Updates works either way.
update_check = true

# Format the file with its language server when saving it.
format_on_save = false

# Sort and prune imports with the language server when saving, before any
# format. Go > Organize Imports (Shift-Option-O) does it by hand.
organize_imports_on_save = false

# Wrap long lines: \"auto\" for Markdown and text files, \"on\" or \"off\".
# View > Word Wrap (Option-Z) flips it for one document.
word_wrap = \"auto\"

# The SSH agent socket Git uses for fetch, pull and push, for when crc is
# opened from the Dock and does not have your shell's SSH_AUTH_SOCK. Run
# `echo $SSH_AUTH_SOCK` in a terminal and put the path here. A leading ~/
# is your home folder.
# ssh_auth_sock = \"~/.ssh/agent.sock\"

# How a file with merge conflicts opens: \"inline\", with buttons on each
# conflict in the text, or \"side-by-side\", as columns. The strip above
# the file switches between them.
conflict_view = \"inline\"
";

impl Settings {
    /// `$XDG_CONFIG_HOME/crc/config.toml`, or `~/.config/crc/config.toml`.
    pub fn path() -> Option<PathBuf> {
        let base = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
        };
        Some(base.join("crc").join("config.toml"))
    }

    /// The settings on disk, with defaults for anything missing or wrong.
    pub fn load() -> Settings {
        let Some(path) = Settings::path() else {
            return Settings::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Settings::parse(&text),
            Err(_) => Settings::default(),
        }
    }

    pub fn parse(text: &str) -> Settings {
        let mut settings = Settings::default();
        for line in text.lines() {
            let Some((key, value)) = split_line(line) else {
                continue;
            };
            match key {
                "font" => {
                    if let Some(name) = unquote(value)
                        && !name.is_empty()
                    {
                        settings.font = name.to_owned();
                    }
                }
                "font_size" => {
                    if let Ok(size) = value.parse::<f32>()
                        && size.is_finite()
                    {
                        settings.font_size = size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
                    }
                }
                "theme" => {
                    if let Some(choice) = unquote(value).and_then(ThemeChoice::parse) {
                        settings.theme = choice;
                    }
                }
                "caret_blink" => match value {
                    "true" => settings.caret_blink = true,
                    "false" => settings.caret_blink = false,
                    _ => {}
                },
                "update_check" => match value {
                    "true" => settings.update_check = true,
                    "false" => settings.update_check = false,
                    _ => {}
                },
                "word_wrap" => match unquote(value).unwrap_or(value) {
                    "auto" => settings.word_wrap = WordWrap::Auto,
                    "on" | "true" => settings.word_wrap = WordWrap::On,
                    "off" | "false" => settings.word_wrap = WordWrap::Off,
                    _ => {}
                },
                "ssh_auth_sock" => {
                    if let Some(path) = unquote(value).filter(|p| !p.is_empty()) {
                        settings.ssh_auth_sock = Some(match path.strip_prefix("~/") {
                            Some(rest) => std::env::var_os("HOME")
                                .map(PathBuf::from)
                                .unwrap_or_default()
                                .join(rest),
                            None => PathBuf::from(path),
                        });
                    }
                }
                "conflict_view" => match unquote(value).unwrap_or(value) {
                    "inline" => settings.conflict_side_by_side = false,
                    "side-by-side" | "side" => settings.conflict_side_by_side = true,
                    _ => {}
                },
                "format_on_save" => match value {
                    "true" => settings.format_on_save = true,
                    "false" => settings.format_on_save = false,
                    _ => {}
                },
                "organize_imports_on_save" => match value {
                    "true" => settings.organize_imports_on_save = true,
                    "false" => settings.organize_imports_on_save = false,
                    _ => {}
                },
                _ => {}
            }
        }
        settings
    }

    /// Creates the settings file from the template if there is none, and
    /// says where it is. For File > Settings: the file people edit should
    /// open with every key in it, explained, rather than empty.
    pub fn ensure_file() -> std::io::Result<PathBuf> {
        let path = Settings::path().ok_or_else(|| std::io::Error::other("no home directory"))?;
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, TEMPLATE)?;
        }
        Ok(path)
    }

    /// Writes the settings, keeping every line of the existing file that is
    /// not one of ours.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = Settings::path() else {
            return Err(std::io::Error::other("no home directory"));
        };
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let text = self.merged_into(&existing);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, text)
    }

    /// `existing` with our keys replaced in place, or appended.
    fn merged_into(&self, existing: &str) -> String {
        let lines = [
            ("font", format!("font = {:?}", self.font)),
            (
                "font_size",
                format!("font_size = {}", format_size(self.font_size)),
            ),
            ("theme", format!("theme = {:?}", self.theme.name())),
        ];
        let mut wrote = [false; 3];
        let mut out = String::new();
        for line in existing.lines() {
            let key = split_line(line).map(|(key, _)| key);
            match lines.iter().position(|(k, _)| Some(*k) == key) {
                Some(i) if !wrote[i] => {
                    out.push_str(&lines[i].1);
                    wrote[i] = true;
                }
                Some(_) => continue,
                None => out.push_str(line),
            }
            out.push('\n');
        }
        if existing.is_empty() {
            out.push_str(TEMPLATE);
            return self.merged_into(&out);
        }
        for (i, (_, line)) in lines.iter().enumerate() {
            if !wrote[i] {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }
}

/// `key = value` with comments and blank lines skipped.
fn split_line(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (key, value) = line.split_once('=')?;
    let value = value.trim();
    // A comment after the value goes; a `#` inside quotes is part of the
    // string, so a quoted value ends at its closing quote instead.
    let value = match value.strip_prefix('"').and_then(|rest| rest.find('"')) {
        Some(close) => &value[..close + 2],
        None => value.find('#').map_or(value, |at| &value[..at]),
    };
    Some((key.trim(), value.trim()))
}

fn unquote(value: &str) -> Option<&str> {
    value.strip_prefix('"')?.strip_suffix('"')
}

/// `13` rather than `13.0`, `13.5` when it is.
fn format_size(size: f32) -> String {
    if size.fract() == 0.0 {
        format!("{}", size as i32)
    } else {
        format!("{size}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn organize_imports_on_save_defaults_off() {
        assert!(!Settings::parse("").organize_imports_on_save);
        assert!(!Settings::parse(TEMPLATE).organize_imports_on_save);
        assert!(Settings::parse("organize_imports_on_save = true\n").organize_imports_on_save);
    }

    #[test]
    fn conflict_view_defaults_to_inline() {
        assert!(!Settings::parse("").conflict_side_by_side);
        assert!(Settings::parse("conflict_view = \"side-by-side\"\n").conflict_side_by_side);
        assert!(!Settings::parse(TEMPLATE).conflict_side_by_side);
    }

    #[test]
    fn caret_blink_defaults_on_and_takes_a_bare_boolean() {
        assert!(Settings::parse("").caret_blink);
        assert!(!Settings::parse("caret_blink = false\n").caret_blink);
        assert!(Settings::parse("").update_check);
        assert!(!Settings::parse("update_check = false\n").update_check);
        assert!(
            Settings::parse("caret_blink = \"false\"\n").caret_blink,
            "a string is not a boolean"
        );
        assert!(
            !Settings::parse(
                TEMPLATE
                    .replace("caret_blink = true", "caret_blink = false")
                    .as_str()
            )
            .caret_blink
        );
    }

    #[test]
    fn parses_the_subset_and_ignores_the_rest() {
        let s = Settings::parse(
            "# a comment\nfont = \"Menlo\"  # trailing\nfont_size = 15\nunknown = [1, 2]\n",
        );
        assert_eq!(s.font, "Menlo");
        assert_eq!(s.font_size, 15.0);
    }

    #[test]
    fn bad_values_fall_back_to_defaults() {
        let s = Settings::parse("font = \nfont_size = huge\n");
        assert_eq!(s, Settings::default());
        assert_eq!(Settings::parse("font_size = 900").font_size, MAX_FONT_SIZE);
        assert_eq!(Settings::parse("font_size = 1").font_size, MIN_FONT_SIZE);
    }

    #[test]
    fn saving_keeps_hand_edits_and_replaces_only_its_own_lines() {
        let existing = "# mine\ntab_width = 2\nfont_size = 13\n";
        let merged = Settings {
            font: "SF Mono".into(),
            font_size: 14.0,
            theme: ThemeChoice::System,
            caret_blink: true,
            update_check: true,
            format_on_save: false,
            organize_imports_on_save: false,
            word_wrap: WordWrap::Auto,
            ssh_auth_sock: None,
            conflict_side_by_side: false,
        }
        .merged_into(existing);
        assert_eq!(
            merged,
            "# mine\ntab_width = 2\nfont_size = 14\nfont = \"SF Mono\"\ntheme = \"system\"\n"
        );
        assert_eq!(Settings::parse(&merged).font_size, 14.0);

        // An empty file becomes the explained template with our values in.
        let fresh = Settings {
            theme: ThemeChoice::Light,
            ..Settings::default()
        }
        .merged_into("");
        assert!(fresh.starts_with("# crc settings"));
        assert!(fresh.contains("# 8 to 32 points"));
        assert_eq!(Settings::parse(&fresh).theme, ThemeChoice::Light);
        assert_eq!(Settings::parse(TEMPLATE), Settings::default());
    }
}
