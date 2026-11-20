//! The few things a user can set, in a file they can also edit by hand.
//!
//! `~/.config/crc/config.toml`, a flat list of `key = value` lines. The
//! reader understands exactly that subset of TOML: quoted strings, numbers,
//! `#` comments. No tables, no arrays, no dependency. The writer changes the
//! one line it has to and leaves everything else in the file as it found it,
//! comments included, so zooming in the app does not trample a hand edit.

use std::path::{Path, PathBuf};

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
    /// Whether the Claude Code crc starts runs its review hooks: each file
    /// kept as it was before Claude's first edit, for review and undo.
    pub agent_review: bool,
    /// Whether a Claude Code turn that ended on a temporary API error (an
    /// overloaded or failing server, a dropped connection) is told to
    /// continue, a few times, unless the person types first.
    pub auto_continue: bool,
    /// Whether a session that starts waiting on you, when it is not the
    /// one in front of you, says so in a macOS notification you can reply
    /// from.
    pub notify_waiting: bool,
    /// Whether the gutter shows how recently each line was changed, from
    /// Git's blame. Off unless asked for: it runs `git blame` per file.
    pub blame_heat: bool,
    /// Whether past versions of the files edited here are kept, in
    /// Application Support.
    pub local_history: bool,
    /// Files local history leaves out besides secrets: `.gitignore` globs,
    /// separated by commas.
    pub local_history_exclude: String,
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
            agent_review: true,
            auto_continue: true,
            notify_waiting: true,
            blame_heat: false,
            local_history: true,
            local_history_exclude: String::new(),
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

# Shade a thin strip in the gutter by how recently Git says each line was
# changed: brightest for this week, gone after a year. Git > Blame Heat
# flips it until crc quits.
blame_heat = false

# When crc starts Claude Code in its terminal, keep each file as it was
# before Claude's first edit to it, so the change can be reviewed and
# undone. Nothing is sent anywhere; the copies stay in crc's folder.
agent_review = true

# When a Claude Code turn crc started ends on a temporary API error (an
# overloaded or failing server, a dropped connection), type \"continue\"
# into it after 20, 40 and 80 seconds, unless you type first. Usage limits,
# billing and login errors are left to you.
auto_continue = true

# When a session starts waiting on you and it is not the one in front of
# you, say so in a macOS notification. Reply from it to type the answer
# into the session; click it to go there.
notify_waiting = true

# Keep past versions of the files you edit, in crc's folder in Application
# Support: as opened, at each save, before a reload, when closed unsaved,
# and after 30 seconds without typing. Secrets such as .env files, keys
# and ~/.ssh are never kept. Up to 30 days.
local_history = true

# More files for local history to leave out: globs as in .gitignore,
# separated by commas. A leading ~/ is your home folder.
local_history_exclude = \"\"
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
        Settings::load_checked().0
    }

    /// The settings, and every line of the file that was not used: an
    /// unknown key, or a value the key does not take. A file that cannot
    /// be read gives the defaults and says why.
    pub fn load_checked() -> (Settings, Vec<String>) {
        let Some(path) = Settings::path() else {
            return (Settings::default(), Vec::new());
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Settings::parse_checked(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Settings::default(), Vec::new()),
            Err(e) => (
                Settings::default(),
                vec![format!("config.toml could not be read: {e}")],
            ),
        }
    }

    /// One status-line sentence for `problems`, or none.
    pub fn describe_problems(problems: &[String]) -> Option<String> {
        let first = problems.first()?;
        Some(match problems.len() {
            1 => first.clone(),
            n => format!("{first} (and {} more)", n - 1),
        })
    }

    pub fn parse(text: &str) -> Settings {
        Settings::parse_checked(text).0
    }

    pub fn parse_checked(text: &str) -> (Settings, Vec<String>) {
        let mut settings = Settings::default();
        let mut problems = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let Some((key, value)) = split_line(line) else {
                continue;
            };
            let bare = unquote(value);
            let word = bare.as_deref().unwrap_or(value);
            // What the key takes, when the value is not it.
            let takes: Option<&str> = match key {
                "font" => match bare.as_deref() {
                    Some(name) if !name.is_empty() => {
                        settings.font = name.to_owned();
                        None
                    }
                    _ => Some("a font name in quotes"),
                },
                "font_size" => match value.parse::<f32>() {
                    Ok(size) if size.is_finite() => {
                        settings.font_size = size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
                        None
                    }
                    _ => Some("a number"),
                },
                "theme" => match ThemeChoice::parse(word) {
                    Some(choice) => {
                        settings.theme = choice;
                        None
                    }
                    None => Some("\"system\", \"dark\" or \"light\""),
                },
                "caret_blink" => set_bool(value, &mut settings.caret_blink),
                "update_check" => set_bool(value, &mut settings.update_check),
                "agent_review" => set_bool(value, &mut settings.agent_review),
                "auto_continue" => set_bool(value, &mut settings.auto_continue),
                "notify_waiting" => set_bool(value, &mut settings.notify_waiting),
                "blame_heat" => set_bool(value, &mut settings.blame_heat),
                "local_history" => set_bool(value, &mut settings.local_history),
                "local_history_exclude" => match bare.as_deref() {
                    Some(globs) => {
                        settings.local_history_exclude = globs.to_owned();
                        None
                    }
                    None => Some("globs in quotes, separated by commas"),
                },
                "word_wrap" => match word {
                    "auto" => {
                        settings.word_wrap = WordWrap::Auto;
                        None
                    }
                    "on" | "true" => {
                        settings.word_wrap = WordWrap::On;
                        None
                    }
                    "off" | "false" => {
                        settings.word_wrap = WordWrap::Off;
                        None
                    }
                    _ => Some("\"auto\", \"on\" or \"off\""),
                },
                "ssh_auth_sock" => match bare.as_deref() {
                    Some(path) if !path.is_empty() => {
                        settings.ssh_auth_sock = Some(match path.strip_prefix("~/") {
                            Some(rest) => std::env::var_os("HOME")
                                .map(PathBuf::from)
                                .unwrap_or_default()
                                .join(rest),
                            None => PathBuf::from(path),
                        });
                        None
                    }
                    _ => Some("a path in quotes"),
                },
                "conflict_view" => match word {
                    "inline" => {
                        settings.conflict_side_by_side = false;
                        None
                    }
                    "side-by-side" | "side" => {
                        settings.conflict_side_by_side = true;
                        None
                    }
                    _ => Some("\"inline\" or \"side-by-side\""),
                },
                "format_on_save" => set_bool(value, &mut settings.format_on_save),
                "organize_imports_on_save" => {
                    set_bool(value, &mut settings.organize_imports_on_save)
                }
                _ => {
                    problems.push(format!(
                        "config.toml line {}: no setting named {key}",
                        n + 1
                    ));
                    continue;
                }
            };
            if let Some(takes) = takes {
                problems.push(format!(
                    "config.toml line {}: {key} takes {takes}, not {value}",
                    n + 1
                ));
            }
        }
        (settings, problems)
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
    /// Saves the font, its size and the theme, the fields `save` writes;
    /// every other line of the file stays as it is.
    pub fn save_font(font: &str, font_size: f32, theme: ThemeChoice) -> std::io::Result<()> {
        Settings {
            font: font.to_owned(),
            font_size,
            theme,
            ..Settings::parse("")
        }
        .save()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = Settings::path() else {
            return Err(std::io::Error::other("no home directory"));
        };
        self.save_to(&path)
    }

    fn save_to(&self, path: &Path) -> std::io::Result<()> {
        let existing = match std::fs::read(path) {
            Ok(bytes) => String::from_utf8(bytes).map_err(|_| {
                // Rewriting it from the template would drop every hand edit.
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "the settings file is not UTF-8, so it was left alone",
                )
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let text = self.merged_into(&existing);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::platform::write_atomically(path, text.as_bytes())
    }

    /// `existing` with our keys replaced in place, or appended.
    fn merged_into(&self, existing: &str) -> String {
        let lines = [
            ("font", format!("font = {}", quote(&self.font))),
            (
                "font_size",
                format!("font_size = {}", format_size(self.font_size)),
            ),
            ("theme", format!("theme = {}", quote(self.theme.name()))),
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

/// One setting as the Settings page shows it: its key in the file, the
/// name it is shown by, the group it is listed under, and what it does.
pub struct Described {
    pub key: &'static str,
    pub label: &'static str,
    pub group: &'static str,
    pub what: &'static str,
}

/// Every key the file takes, in the page's order. A test checks this
/// against the template, so a new key cannot ship without a sentence here.
pub const DESCRIBED: [Described; 16] = [
    Described {
        key: "theme",
        label: "Theme",
        group: "Appearance",
        what: "Light, dark, or whatever the system uses.",
    },
    Described {
        key: "font",
        label: "Font",
        group: "Appearance",
        what: "The code font: any monospace face installed on this Mac.",
    },
    Described {
        key: "font_size",
        label: "Font size",
        group: "Appearance",
        what: "8 to 32 points. \u{2318}= and \u{2318}- change it, \u{2318}0 goes back.",
    },
    Described {
        key: "caret_blink",
        label: "Caret blink",
        group: "Appearance",
        what: "Whether the caret blinks. It is solid while you type either way.",
    },
    Described {
        key: "word_wrap",
        label: "Word wrap",
        group: "Editing",
        what: "Wrap long lines: auto wraps Markdown and text files. Option-Z flips one document.",
    },
    Described {
        key: "format_on_save",
        label: "Format on save",
        group: "Editing",
        what: "Format the file with its language server when saving it.",
    },
    Described {
        key: "organize_imports_on_save",
        label: "Organize imports on save",
        group: "Editing",
        what: "Sort and prune imports with the language server when saving, before any format.",
    },
    Described {
        key: "conflict_view",
        label: "Merge conflicts",
        group: "Git",
        what: "How a file with merge conflicts opens: in the text, or as columns.",
    },
    Described {
        key: "blame_heat",
        label: "Blame heat",
        group: "Git",
        what: "Shade a strip in the gutter by how recently each line was changed. Git > Blame Heat flips it for the session.",
    },
    Described {
        key: "ssh_auth_sock",
        label: "SSH agent socket",
        group: "Git",
        what: "The SSH agent socket for fetch, pull and push, for when crc is opened from the Dock without your shell's SSH_AUTH_SOCK.",
    },
    Described {
        key: "update_check",
        label: "Check for updates",
        group: "App",
        what: "Once a day at launch, ask GitHub whether a newer release exists. Nothing is downloaded.",
    },
    Described {
        key: "agent_review",
        label: "Review agent edits",
        group: "Agents",
        what: "Keep each file as it was before the Claude Code crc starts first edits it, so its changes can be reviewed and undone.",
    },
    Described {
        key: "auto_continue",
        label: "Continue after API errors",
        group: "Agents",
        what: "When a Claude Code turn ends on a temporary API error, type \"continue\" into it after 20, 40 and 80 seconds, unless you type first.",
    },
    Described {
        key: "notify_waiting",
        label: "Notify when an agent waits",
        group: "Agents",
        what: "Notify when an agent waits: a macOS notification for a session that starts waiting while it is not the one in front of you. Reply from it, or click it to go there.",
    },
    Described {
        key: "local_history",
        label: "Local history",
        group: "Local History",
        what: "Keep past versions of the files you edit: opened, saved, reloaded, closed unsaved, and after 30 seconds without typing. Never secrets.",
    },
    Described {
        key: "local_history_exclude",
        label: "Left out of local history",
        group: "Local History",
        what: "More files for local history to leave out: globs as in .gitignore, separated by commas.",
    },
];

/// How the Settings page changes a key: a switch, a stepper between two
/// bounds, one of a few words, or the file at that line for free text.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Control {
    Toggle,
    Stepper {
        min: f32,
        max: f32,
    },
    /// The choices, as (what the page shows, what the file says).
    Choice(&'static [(&'static str, &'static str)]),
    Text,
}

/// The control the Settings page draws for `key`.
pub fn control_of(key: &str) -> Control {
    match key {
        "font_size" => Control::Stepper {
            min: MIN_FONT_SIZE,
            max: MAX_FONT_SIZE,
        },
        "theme" => Control::Choice(&[("System", "system"), ("Light", "light"), ("Dark", "dark")]),
        "word_wrap" => Control::Choice(&[("Auto", "auto"), ("On", "on"), ("Off", "off")]),
        "conflict_view" => {
            Control::Choice(&[("Inline", "inline"), ("Side by Side", "side-by-side")])
        }
        "caret_blink"
        | "update_check"
        | "format_on_save"
        | "organize_imports_on_save"
        | "blame_heat"
        | "agent_review"
        | "auto_continue"
        | "notify_waiting"
        | "local_history" => Control::Toggle,
        _ => Control::Text,
    }
}

/// The name the Settings page shows for `key`, or the key itself.
pub fn label_of(key: &str) -> &str {
    DESCRIBED
        .iter()
        .find(|d| d.key == key)
        .map_or(key, |d| d.label)
}

/// A stepper's next value: one point from `value` toward `up` or down,
/// landing on a whole number, inside `min..=max`. `None` at the bound.
pub fn stepped(value: f32, up: bool, min: f32, max: f32) -> Option<f32> {
    let next = if up {
        value.floor() + 1.0
    } else {
        value.ceil() - 1.0
    }
    .clamp(min, max);
    ((next - value).abs() > 0.001).then_some(next)
}

impl Settings {
    /// What a toggle reads: whether `key` is on.
    pub fn flag_of(&self, key: &str) -> bool {
        self.value_of(key) == "true"
    }

    /// A number setting's value, for a stepper.
    pub fn number_of(&self, key: &str) -> f32 {
        match key {
            "font_size" => self.font_size,
            _ => 0.0,
        }
    }

    /// A choice setting's word as the file spells it, without its quotes.
    pub fn word_of(&self, key: &str) -> String {
        let value = self.value_of(key);
        unquote(&value).map_or(value.clone(), |w| w.into_owned())
    }

    /// A key's value as a person reads it: no quotes, On and Off, the
    /// choice's own name.
    pub fn display_of(&self, key: &str) -> String {
        match control_of(key) {
            Control::Toggle => if self.flag_of(key) { "On" } else { "Off" }.to_owned(),
            Control::Stepper { .. } => format!("{} pt", format_size(self.number_of(key))),
            Control::Choice(choices) => {
                let word = self.word_of(key);
                choices
                    .iter()
                    .find(|(_, w)| *w == word)
                    .map_or(word.clone(), |(label, _)| (*label).to_owned())
            }
            Control::Text => match key {
                "ssh_auth_sock" => self.ssh_auth_sock.as_ref().map_or_else(
                    || "Not set".to_owned(),
                    |p| p.to_string_lossy().into_owned(),
                ),
                "local_history_exclude" if self.local_history_exclude.trim().is_empty() => {
                    "None".to_owned()
                }
                _ => self.word_of(key),
            },
        }
    }

    /// What the file should say for `key` after a page control changed
    /// it: a toggle flipped, a stepper moved, a choice made.
    pub fn literal_for(&self, key: &str, change: Change<'_>) -> Option<String> {
        match (control_of(key), change) {
            (Control::Toggle, Change::Flip) => {
                Some(if self.flag_of(key) { "false" } else { "true" }.to_owned())
            }
            (Control::Stepper { min, max }, Change::Step(up)) => {
                stepped(self.number_of(key), up, min, max).map(format_size)
            }
            (Control::Choice(choices), Change::Choose(word)) => {
                choices.iter().any(|(_, w)| *w == word).then(|| quote(word))
            }
            _ => None,
        }
    }

    /// Sets one key in config.toml to `literal`, as the file spells it,
    /// leaving every other line, and any comment after the value, as it
    /// was. A missing file starts from the template.
    pub fn set_value(key: &str, literal: &str) -> std::io::Result<()> {
        let Some(path) = Settings::path() else {
            return Err(std::io::Error::other("no home directory"));
        };
        set_value_in(&path, key, literal)
    }
}

/// What a control on the Settings page asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change<'a> {
    Flip,
    Step(bool),
    Choose(&'a str),
}

fn set_value_in(path: &Path, key: &str, literal: &str) -> std::io::Result<()> {
    let existing = match std::fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the settings file is not UTF-8, so it was left alone",
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let base = if existing.trim().is_empty() {
        TEMPLATE
    } else {
        existing.as_str()
    };
    let text = with_value(base, key, literal);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::platform::write_atomically(path, text.as_bytes())
}

/// `text` with the line that sets `key` (the last one, which is the one
/// that counts) given `literal` as its value, its spacing and any comment
/// after the value kept; or the line appended when no line sets it.
fn with_value(text: &str, key: &str, literal: &str) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let at = lines
        .iter()
        .rposition(|line| split_line(line).is_some_and(|(k, _)| k == key));
    let Some(at) = at else {
        let mut out = text.to_owned();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("{key} = {literal}\n"));
        return out;
    };
    let line = lines[at];
    let (_, value) = split_line(line).expect("matched above");
    // `value` is a slice of `line`, so its place in the line is known.
    let start = value.as_ptr() as usize - line.as_ptr() as usize;
    let end = start + value.len();
    let mut out = String::with_capacity(text.len() + literal.len());
    for (i, l) in lines.iter().enumerate() {
        if i == at {
            out.push_str(&line[..start]);
            out.push_str(literal);
            out.push_str(&line[end..]);
        } else {
            out.push_str(l);
        }
    }
    out
}

impl Settings {
    /// A key's value as the file would spell it, for the Settings page.
    pub fn value_of(&self, key: &str) -> String {
        let flag = |on: bool| if on { "true" } else { "false" }.to_owned();
        match key {
            "font" => quote(&self.font),
            "font_size" => format_size(self.font_size),
            "theme" => quote(self.theme.name()),
            "caret_blink" => flag(self.caret_blink),
            "update_check" => flag(self.update_check),
            "agent_review" => flag(self.agent_review),
            "auto_continue" => flag(self.auto_continue),
            "notify_waiting" => flag(self.notify_waiting),
            "blame_heat" => flag(self.blame_heat),
            "local_history" => flag(self.local_history),
            "local_history_exclude" => quote(&self.local_history_exclude),
            "format_on_save" => flag(self.format_on_save),
            "organize_imports_on_save" => flag(self.organize_imports_on_save),
            "word_wrap" => quote(match self.word_wrap {
                WordWrap::Auto => "auto",
                WordWrap::On => "on",
                WordWrap::Off => "off",
            }),
            "ssh_auth_sock" => self
                .ssh_auth_sock
                .as_ref()
                .map_or_else(|| "not set".to_owned(), |p| quote(&p.to_string_lossy())),
            "conflict_view" => quote(if self.conflict_side_by_side {
                "side-by-side"
            } else {
                "inline"
            }),
            _ => String::new(),
        }
    }
}

/// The 0-based line where `key` is set in `text`, or where the template
/// has it commented out; the last setting wins, as in [`Settings::parse`].
pub fn line_of(text: &str, key: &str) -> Option<usize> {
    let set = text
        .lines()
        .enumerate()
        .filter(|(_, line)| split_line(line).is_some_and(|(k, _)| k == key))
        .map(|(n, _)| n)
        .last();
    set.or_else(|| {
        text.lines().position(|line| {
            line.trim()
                .strip_prefix('#')
                .and_then(|rest| rest.trim_start().split_once('='))
                .is_some_and(|(k, _)| k.trim() == key)
        })
    })
}

/// `true` or `false` into `field`; anything else leaves it as it was.
/// Sets `field` from `true` or `false`; says what the key takes otherwise.
fn set_bool(value: &str, field: &mut bool) -> Option<&'static str> {
    match value {
        "true" => *field = true,
        "false" => *field = false,
        _ => return Some("true or false"),
    }
    None
}

/// `key = value` with comments and blank lines skipped.
pub(crate) fn split_line(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (key, value) = line.split_once('=')?;
    let value = value.trim();
    // A comment after the value goes; a `#` inside quotes is part of the
    // string, so a quoted value ends at its closing quote instead: the
    // first unescaped `"`, or the next `'` for a literal string.
    let closing = match value.chars().next() {
        Some('"') => {
            let mut escaped = false;
            value[1..].char_indices().find_map(|(i, c)| {
                let end = !escaped && c == '"';
                escaped = !escaped && c == '\\';
                end.then_some(i + 1)
            })
        }
        Some('\'') => value[1..].find('\'').map(|i| i + 1),
        _ => None,
    };
    let value = match closing {
        Some(close) => &value[..close + 1],
        None => value.find('#').map_or(value, |at| &value[..at]),
    };
    Some((key.trim(), value.trim()))
}

/// A TOML string's contents: `"basic"` with its escapes, or `'literal'`
/// as written.
pub(crate) fn unquote(value: &str) -> Option<std::borrow::Cow<'_, str>> {
    if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
        return Some(inner.into());
    }
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    if !inner.contains('\\') {
        return Some(inner.into());
    }
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'u' => {
                let hex: String = chars.by_ref().take(4).collect();
                out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
            }
            other => out.push(other),
        }
    }
    Some(out.into())
}

/// `text` as a TOML basic string, which [`unquote`] reads back as it was.
fn quote(text: &str) -> String {
    let mut out = String::from('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
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

    #[test]
    fn every_template_key_is_described_and_parsed() {
        let template: Vec<&str> = TEMPLATE
            .lines()
            .filter_map(|line| {
                let line = line.trim().trim_start_matches('#').trim_start();
                let (key, _) = line.split_once('=')?;
                let key = key.trim();
                key.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_')
                    .then_some(key)
            })
            .collect();
        let described: Vec<&str> = DESCRIBED.iter().map(|d| d.key).collect();
        let mut a = template.clone();
        let mut b = described.clone();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(
            a, b,
            "the template and the Settings page list the same keys"
        );
        let defaults = Settings::default();
        for key in described {
            let value = match defaults.value_of(key).as_str() {
                "not set" => "\"/tmp/agent.sock\"".to_owned(),
                v => v.to_owned(),
            };
            let (_, problems) = Settings::parse_checked(&format!("{key} = {value}\n"));
            assert!(problems.is_empty(), "{key} = {value}: {problems:?}");
        }
    }

    #[test]
    fn a_page_change_rewrites_one_value_and_keeps_the_rest() {
        let text = "# mine\ncaret_blink = true   # keep me\nfont_size = 13\ntab_width = 2\n";
        let s = Settings::parse(text);
        let flipped = s.literal_for("caret_blink", Change::Flip).unwrap();
        assert_eq!(flipped, "false");
        assert_eq!(
            with_value(text, "caret_blink", &flipped),
            "# mine\ncaret_blink = false   # keep me\nfont_size = 13\ntab_width = 2\n"
        );
        // The last line that sets the key is the one that counts.
        assert_eq!(
            with_value("theme = \"dark\"\ntheme = \"light\"", "theme", "\"system\""),
            "theme = \"dark\"\ntheme = \"system\""
        );
        // A key not set yet is appended; a commented one is left alone.
        assert_eq!(
            with_value("# blame_heat = true\nfont = \"A\"", "blame_heat", "true"),
            "# blame_heat = true\nfont = \"A\"\nblame_heat = true\n"
        );
        // Through the file: a missing one starts from the template.
        let dir = std::env::temp_dir().join(format!("crc-settings-set-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let _ = std::fs::remove_file(&path);
        set_value_in(&path, "caret_blink", "false").unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            written,
            TEMPLATE.replace("caret_blink = true", "caret_blink = false")
        );
        assert!(!Settings::parse(&written).caret_blink);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn steppers_stop_at_their_bounds_and_choices_take_their_words() {
        assert_eq!(stepped(13.0, true, 8.0, 32.0), Some(14.0));
        assert_eq!(stepped(13.5, true, 8.0, 32.0), Some(14.0));
        assert_eq!(stepped(13.5, false, 8.0, 32.0), Some(13.0));
        assert_eq!(stepped(32.0, true, 8.0, 32.0), None);
        assert_eq!(stepped(8.0, false, 8.0, 32.0), None);
        let at_max = Settings::parse("font_size = 32\n");
        assert_eq!(at_max.literal_for("font_size", Change::Step(true)), None);
        assert_eq!(
            at_max
                .literal_for("font_size", Change::Step(false))
                .as_deref(),
            Some("31")
        );
        let s = Settings::default();
        assert_eq!(
            s.literal_for("theme", Change::Choose("light")).as_deref(),
            Some("\"light\"")
        );
        assert_eq!(s.literal_for("theme", Change::Choose("sepia")), None);
        assert_eq!(s.literal_for("font", Change::Flip), None);
    }

    #[test]
    fn labels_and_values_read_as_words() {
        assert_eq!(label_of("font_size"), "Font size");
        assert_eq!(label_of("caret_blink"), "Caret blink");
        for d in &DESCRIBED {
            assert!(!d.label.contains('_'), "{}: {}", d.key, d.label);
            assert!(
                d.label.starts_with(|c: char| c.is_uppercase()),
                "{}",
                d.label
            );
        }
        let s = Settings::parse("conflict_view = \"side-by-side\"\nblame_heat = true\n");
        assert_eq!(s.display_of("theme"), "System");
        assert_eq!(s.display_of("font"), "SF Mono");
        assert_eq!(s.display_of("font_size"), "13 pt");
        assert_eq!(s.display_of("blame_heat"), "On");
        assert_eq!(s.display_of("caret_blink"), "On");
        assert_eq!(s.display_of("format_on_save"), "Off");
        assert_eq!(s.display_of("conflict_view"), "Side by Side");
        assert_eq!(s.display_of("ssh_auth_sock"), "Not set");
        assert_eq!(s.display_of("local_history_exclude"), "None");
        assert_eq!(control_of("caret_blink"), Control::Toggle);
        assert_eq!(control_of("font"), Control::Text);
    }

    #[test]
    fn value_of_reads_back() {
        let s = Settings::parse(
            "font_size = 15\nword_wrap = \"off\"\nconflict_view = \"side-by-side\"\n",
        );
        assert_eq!(s.value_of("font_size"), "15");
        assert_eq!(s.value_of("word_wrap"), "\"off\"");
        assert_eq!(s.value_of("conflict_view"), "\"side-by-side\"");
        assert_eq!(s.value_of("caret_blink"), "true");
        assert_eq!(s.value_of("ssh_auth_sock"), "not set");
    }

    #[test]
    fn line_of_finds_a_set_or_commented_key() {
        let text = "# font = \"x\"\nfont = \"A\"\n# ssh_auth_sock = \"~/s\"\nfont = \"B\"\n";
        assert_eq!(line_of(text, "font"), Some(3), "the last setting wins");
        assert_eq!(line_of(text, "ssh_auth_sock"), Some(2), "commented out");
        assert_eq!(line_of(text, "theme"), None);
        assert!(line_of(TEMPLATE, "ssh_auth_sock").is_some());
    }
    use super::*;

    #[test]
    fn font_names_with_quotes_round_trip_and_literal_strings_read() {
        let odd = "Foo \"Bar\" \\ Baz # not a comment";
        let written = format!("font = {}\n", quote(odd));
        assert_eq!(Settings::parse(&written).font, odd);
        assert_eq!(
            Settings::parse("font = 'SF Mono' # comment\n").font,
            "SF Mono"
        );
    }

    #[test]
    fn a_settings_file_that_is_not_utf8_is_left_alone() {
        let dir = std::env::temp_dir().join(format!("caio-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let original = b"# caf\xe9\nformat_on_save = true\n".to_vec();
        std::fs::write(&path, &original).unwrap();
        let err = Settings::parse("").save_to(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::write(&path, "format_on_save = true\n").unwrap();
        Settings::parse("").save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("format_on_save = true\n"), "{text}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lines_that_are_not_used_are_reported_with_their_number() {
        let (settings, problems) = Settings::parse_checked(
            "font_size = 13\n# a comment\ncolour = \"red\"\nfont_size = big\ncaret_blink = yes\ntheme = \"sepia\"\nword_wrap = \"on\"\n",
        );
        assert_eq!(settings.font_size, 13.0);
        assert_eq!(settings.word_wrap, WordWrap::On);
        assert_eq!(
            problems,
            [
                "config.toml line 3: no setting named colour",
                "config.toml line 4: font_size takes a number, not big",
                "config.toml line 5: caret_blink takes true or false, not yes",
                "config.toml line 6: theme takes \"system\", \"dark\" or \"light\", not \"sepia\"",
            ]
        );
        assert_eq!(Settings::parse_checked(TEMPLATE).1, Vec::<String>::new());
        assert_eq!(Settings::describe_problems(&[]), None);
        assert_eq!(
            Settings::describe_problems(&["a".into(), "b".into(), "c".into()]).as_deref(),
            Some("a (and 2 more)")
        );
    }

    #[test]
    fn organize_imports_on_save_defaults_off() {
        assert!(!Settings::parse("").organize_imports_on_save);
        assert!(!Settings::parse(TEMPLATE).organize_imports_on_save);
        assert!(Settings::parse("organize_imports_on_save = true\n").organize_imports_on_save);
    }

    #[test]
    fn blame_heat_defaults_off() {
        assert!(!Settings::default().blame_heat);
        assert!(!Settings::parse("").blame_heat);
        assert!(!Settings::parse(TEMPLATE).blame_heat);
        assert!(Settings::parse("blame_heat = true\n").blame_heat);
        assert_eq!(
            Settings::parse("blame_heat = true\n").value_of("blame_heat"),
            "true"
        );
        assert_eq!(
            Settings::parse_checked("blame_heat = on\n").1,
            ["config.toml line 1: blame_heat takes true or false, not on"]
        );
    }

    #[test]
    fn local_history_defaults_on_with_nothing_excluded() {
        assert!(Settings::parse("").local_history);
        assert!(Settings::parse(TEMPLATE).local_history);
        assert_eq!(Settings::parse(TEMPLATE).local_history_exclude, "");
        assert!(!Settings::parse("local_history = false\n").local_history);
        let s = Settings::parse("local_history_exclude = \"*.log, notes/\"\n");
        assert_eq!(s.local_history_exclude, "*.log, notes/");
        assert_eq!(s.value_of("local_history_exclude"), "\"*.log, notes/\"");
        assert_eq!(
            Settings::parse_checked("local_history_exclude = *.log\n").1,
            [
                "config.toml line 1: local_history_exclude takes globs in quotes, separated by commas, not *.log"
            ]
        );
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
    fn auto_continue_defaults_on_and_takes_a_boolean() {
        assert!(Settings::parse("").auto_continue);
        assert!(Settings::parse(TEMPLATE).auto_continue);
        assert!(!Settings::parse("auto_continue = false\n").auto_continue);
        assert_eq!(
            Settings::parse("auto_continue = false\n").value_of("auto_continue"),
            "false"
        );
        assert!(
            Settings::parse("auto_continue = \"false\"\n").auto_continue,
            "a string is not a boolean"
        );
    }

    #[test]
    fn notify_waiting_defaults_on_and_takes_a_boolean() {
        assert!(Settings::parse("").notify_waiting);
        assert!(Settings::parse(TEMPLATE).notify_waiting);
        assert!(!Settings::parse("notify_waiting = false\n").notify_waiting);
        assert_eq!(
            Settings::parse("notify_waiting = false\n").value_of("notify_waiting"),
            "false"
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
            agent_review: true,
            auto_continue: true,
            notify_waiting: true,
            blame_heat: false,
            local_history: true,
            local_history_exclude: String::new(),
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
