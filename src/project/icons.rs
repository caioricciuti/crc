//! Which icon a file gets.
//!
//! The glyphs are private-use characters in the bundled icon font (see
//! third_party/SOURCES.md). Every code point here was read out of that
//! release's `glyphnames.json`, whose name for it is in the comment beside
//! it, so that re-vendoring the font is a lookup and not an archaeology.
//!
//! The font is monochrome, and so are the icons: the caller tints them with
//! a theme colour (accent when active or selected, muted otherwise), which
//! reads on both appearances. There is deliberately no per-type hue table.

use std::path::Path;

/// An icon: the character to draw. The caller picks the tint.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Icon {
    pub glyph: char,
}

// Explorer actions. Verified present in the bundled Symbols Nerd Font: a
// glyph the font does not carry draws nothing at all, silently.
pub const NEW_FILE: char = '\u{ea7f}'; // cod-new_file
pub const NEW_FOLDER: char = '\u{ea80}'; // cod-new_folder
pub const COLLAPSE_ALL: char = '\u{eac5}'; // cod-collapse_all
pub const REFRESH: char = '\u{eb37}'; // cod-refresh

// Controls. Each one was drawn from the bundled font and looked at before
// it went in here.
pub const ADD: char = '\u{ea60}'; // cod-add
pub const REMOVE: char = '\u{eb3b}'; // cod-remove
pub const CLOSE: char = '\u{ea76}'; // cod-close
pub const SYNC: char = '\u{ea77}'; // cod-sync
pub const ELLIPSIS: char = '\u{ea7c}'; // cod-ellipsis
pub const TRASH: char = '\u{ea81}'; // cod-trash
pub const DISCARD: char = '\u{eae2}'; // cod-discard
pub const ARROW_UP: char = '\u{eaa1}'; // cod-arrow_up
pub const ARROW_DOWN: char = '\u{ea9a}'; // cod-arrow_down
pub const CHEVRON_UP: char = '\u{eab7}'; // cod-chevron_up
pub const CHEVRON_LEFT: char = '\u{eab5}'; // cod-chevron_left
pub const CHECK: char = '\u{eab2}'; // cod-check
pub const GIT_COMMIT: char = '\u{eafc}'; // cod-git_commit
pub const REPO_PULL: char = '\u{eb40}'; // cod-repo_pull
pub const REPO_PUSH: char = '\u{eb41}'; // cod-repo_push
pub const CLOUD_DOWNLOAD: char = '\u{eac2}'; // cod-cloud_download
pub const GO_TO_FILE: char = '\u{ea94}'; // cod-go_to_file
pub const WARNING: char = '\u{ea6c}'; // cod-warning
pub const ERROR: char = '\u{ea87}'; // cod-error
pub const INFO: char = '\u{ea74}'; // cod-info
pub const PASS: char = '\u{eba4}'; // cod-pass
pub const SPLIT: char = '\u{eb56}'; // cod-split_horizontal
pub const SIDEBAR_LEFT: char = '\u{ebf3}'; // cod-layout_sidebar_left

pub const CHEVRON_RIGHT: char = '\u{eab6}'; // cod-chevron_right
pub const CHEVRON_DOWN: char = '\u{eab4}'; // cod-chevron_down
pub const HOME: char = '\u{eb06}'; // cod-home
pub const SEARCH: char = '\u{ea6d}'; // cod-search
pub const LIGHTBULB: char = '\u{ea61}'; // cod-lightbulb

// Completion chips: where a suggestion came from, and what it is.
pub const SYMBOL_METHOD: char = '\u{ea8c}'; // cod-symbol_method
pub const SYMBOL_VARIABLE: char = '\u{ea88}'; // cod-symbol_variable
pub const SYMBOL_NAMESPACE: char = '\u{ea8b}'; // cod-symbol_namespace
pub const SYMBOL_TEXT: char = '\u{ea93}'; // cod-symbol_key (plain words)
pub const SYMBOL_CLASS: char = '\u{eb5b}'; // cod-symbol_class
pub const SYMBOL_CONSTANT: char = '\u{eb5d}'; // cod-symbol_constant
pub const SYMBOL_FIELD: char = '\u{eb5f}'; // cod-symbol_field
pub const HISTORY: char = '\u{ea82}'; // cod-history
pub const FILE: char = '\u{ea7b}'; // cod-file
pub const FOLDER_OUTLINE: char = '\u{ea83}'; // cod-folder
pub const TERMINAL: char = '\u{ea85}'; // cod-terminal

/// The icons an extension may name in its manifest (`"icon": "wand"`).
/// A name, never an image: crc does not decode files from extensions, and
/// a glyph from the bundled font is tinted like every other icon. Each
/// code point was read from the font's own glyph names (`cod-<name>`), and
/// crc-extensions' `scripts/build-registry.py` checks against the same list.
pub const EXTENSION_ICONS: &[(&str, char)] = &[
    ("extensions", '\u{eae6}'),       // cod-extensions
    ("sort-precedence", '\u{eb55}'),  // cod-sort_precedence
    ("case-sensitive", '\u{eab1}'),   // cod-case_sensitive
    ("preserve-case", '\u{eb2e}'),    // cod-preserve_case
    ("symbol-string", '\u{eb8d}'),    // cod-symbol_string
    ("symbol-key", '\u{ea93}'),       // cod-symbol_key
    ("symbol-method", '\u{ea8c}'),    // cod-symbol_method
    ("symbol-class", '\u{eb5b}'),     // cod-symbol_class
    ("symbol-color", '\u{eb5c}'),     // cod-symbol_color
    ("symbol-numeric", '\u{ea90}'),   // cod-symbol_numeric
    ("symbol-ruler", '\u{ea96}'),     // cod-symbol_ruler
    ("symbol-namespace", '\u{ea8b}'), // cod-symbol_namespace
    ("json", '\u{eb0f}'),             // cod-json
    ("code", '\u{eac4}'),             // cod-code
    ("file-code", '\u{eae9}'),        // cod-file_code
    ("list-ordered", '\u{eb16}'),     // cod-list_ordered
    ("list-unordered", '\u{eb17}'),   // cod-list_unordered
    ("list-flat", '\u{eb84}'),        // cod-list_flat
    ("filter", '\u{eaf1}'),           // cod-filter
    ("search", '\u{ea6d}'),           // cod-search
    ("replace", '\u{eb3d}'),          // cod-replace
    ("wand", '\u{ebcf}'),             // cod-wand
    ("lightbulb", '\u{ea61}'),        // cod-lightbulb
    ("checklist", '\u{eab3}'),        // cod-checklist
    ("check", '\u{eab2}'),            // cod-check
    ("tools", '\u{eb6d}'),            // cod-tools
    ("gear", '\u{eaf8}'),             // cod-gear
    ("beaker", '\u{ea79}'),           // cod-beaker
    ("bug", '\u{eaaf}'),              // cod-bug
    ("book", '\u{eaa4}'),             // cod-book
    ("note", '\u{eb26}'),             // cod-note
    ("comment", '\u{ea6b}'),          // cod-comment
    ("quote", '\u{eb33}'),            // cod-quote
    ("tag", '\u{ea66}'),              // cod-tag
    ("link", '\u{eb15}'),             // cod-link
    ("lock", '\u{ea75}'),             // cod-lock
    ("key", '\u{eb11}'),              // cod-key
    ("globe", '\u{eb01}'),            // cod-globe
    ("calendar", '\u{eab0}'),         // cod-calendar
    ("whole-word", '\u{eb7e}'),       // cod-whole_word
    ("regex", '\u{eb38}'),            // cod-regex
    ("text-size", '\u{eb69}'),        // cod-text_size
    ("word-wrap", '\u{eb80}'),        // cod-word_wrap
    ("edit", '\u{ea73}'),             // cod-edit
    ("copy", '\u{ebcc}'),             // cod-copy
    ("git-merge", '\u{eafe}'),        // cod-git_merge
    ("terminal", '\u{ea85}'),         // cod-terminal
    ("database", '\u{eace}'),         // cod-database
    ("table", '\u{ebb7}'),            // cod-table
    ("graph", '\u{eb03}'),            // cod-graph
    ("pulse", '\u{eb31}'),            // cod-pulse
    ("rocket", '\u{eb44}'),           // cod-rocket
    ("heart", '\u{eb05}'),            // cod-heart
    ("star-full", '\u{eb59}'),        // cod-star_full
    ("paintcan", '\u{eb2a}'),         // cod-paintcan
    ("color-mode", '\u{eac6}'),       // cod-color_mode
    ("symbol-event", '\u{ea86}'),     // cod-symbol_event
    ("symbol-array", '\u{ea8a}'),     // cod-symbol_array
    ("symbol-boolean", '\u{ea8f}'),   // cod-symbol_boolean
];

#[cfg(test)]
#[test]
fn extension_icon_names_are_unique_and_kebab_case() {
    let mut names: Vec<&str> = EXTENSION_ICONS.iter().map(|(n, _)| *n).collect();
    assert!(
        names
            .iter()
            .all(|n| n.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'))
    );
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), EXTENSION_ICONS.len());
}

/// The glyph for an extension icon name, or the extensions icon.
pub fn extension_icon(name: &str) -> char {
    EXTENSION_ICONS
        .iter()
        .find(|(n, _)| *n == name)
        .map_or('\u{eae6}', |(_, c)| *c)
}

/// The chip icon for a definition kind as the index names it.
pub fn for_kind(kind: &str) -> char {
    match kind {
        "function" => SYMBOL_METHOD,
        "type" => SYMBOL_CLASS,
        "constant" => SYMBOL_CONSTANT,
        "field" => SYMBOL_FIELD,
        "module" => SYMBOL_NAMESPACE,
        _ => SYMBOL_VARIABLE,
    }
}

/// The chip icon for a language server's completion kind number.
pub fn for_lsp_kind(kind: u64) -> char {
    match kind {
        2..=4 => SYMBOL_METHOD,
        5 | 10 => SYMBOL_FIELD,
        7 | 8 | 13 | 22 | 25 => SYMBOL_CLASS,
        9 => SYMBOL_NAMESPACE,
        14 => SYMBOL_TEXT,
        20 | 21 => SYMBOL_CONSTANT,
        17 => FILE,
        19 => FOLDER_OUTLINE,
        _ => SYMBOL_VARIABLE,
    }
}

pub const FOLDER: char = '\u{e5ff}'; // custom-folder
const FOLDER_OPEN: char = '\u{e5fe}'; // custom-folder_open
const FOLDER_GIT: char = '\u{e5fb}'; // custom-folder_git
const FOLDER_GITHUB: char = '\u{e5fd}'; // custom-folder_github
const FOLDER_CONFIG: char = '\u{e5fc}'; // custom-folder_config

/// The icon for a directory.
pub fn for_directory(name: &str, expanded: bool) -> Icon {
    let glyph = match name {
        ".git" => FOLDER_GIT,
        ".github" => FOLDER_GITHUB,
        ".cargo" | ".vscode" | ".config" | "config" => FOLDER_CONFIG,
        _ if expanded => FOLDER_OPEN,
        _ => FOLDER,
    };
    Icon { glyph }
}

/// The icon for a file, from its whole name first and its extension second.
pub fn for_file(path: &Path) -> Icon {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let lower = name.to_ascii_lowercase();

    // Names that mean more than their extension does.
    let by_name = match lower.as_str() {
        "cargo.toml" | "cargo.lock" => Some('\u{e68b}'), // seti-rust
        "package.json" | "package-lock.json" => Some('\u{e60b}'), // seti-json
        "bun.lock" | "bun.lockb" | "bunfig.toml" => Some('\u{e76f}'), // dev-bun
        "tsconfig.json" => Some('\u{e628}'),             // seti-typescript
        "dockerfile" | "docker-compose.yml" | "docker-compose.yaml" | ".dockerignore" => {
            Some('\u{e650}') // seti-docker
        }
        "makefile" => Some('\u{e673}'), // seti-makefile
        "license" | "license.md" | "license.txt" | "copying" => Some('\u{e60a}'), // seti-license
        ".gitignore" | ".gitattributes" | ".gitmodules" => Some('\u{e65d}'), // seti-git
        "favicon.ico" => Some('\u{e623}'), // seti-favicon
        _ => None,
    };
    if let Some(glyph) = by_name {
        return Icon { glyph };
    }

    let extension = lower.rsplit_once('.').map_or("", |(_, e)| e);
    let glyph = match extension {
        "rs" => '\u{e68b}',                                           // seti-rust
        "html" | "htm" => '\u{e60e}',                                 // seti-html
        "css" | "scss" | "sass" | "less" => '\u{e614}',               // seti-css
        "js" | "mjs" | "cjs" => '\u{e60c}',                           // seti-javascript
        "ts" | "mts" | "cts" => '\u{e628}',                           // seti-typescript
        "jsx" | "tsx" => '\u{e625}',                                  // seti-react
        "svelte" => '\u{e697}',                                       // seti-svelte
        "vue" => '\u{e6a0}',                                          // seti-vue
        "json" | "jsonc" => '\u{e60b}',                               // seti-json
        "md" | "markdown" | "mdx" => '\u{e609}',                      // seti-markdown
        "go" => '\u{e627}',                                           // seti-go
        "py" | "pyi" => '\u{e606}',                                   // seti-python
        "c" | "h" => '\u{e649}',                                      // seti-c
        "cpp" | "cc" | "cxx" | "hpp" => '\u{e646}',                   // seti-cpp
        "toml" => '\u{e6b2}',                                         // custom-toml
        "yml" | "yaml" => '\u{e6a8}',                                 // seti-yml
        "ini" | "cfg" | "conf" | "env" | "plist" => '\u{e615}',       // seti-config
        "sh" | "bash" | "zsh" | "fish" => '\u{e691}',                 // seti-shell
        "sql" | "db" | "sqlite" | "sqlite3" | "duckdb" => '\u{e64d}', // seti-db
        "csv" | "tsv" => '\u{e64a}',                                  // seti-csv
        "svg" => '\u{e698}',                                          // seti-svg
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "icns" => {
            '\u{e60d}' // seti-image
        }
        "pdf" => '\u{e67d}',                                      // seti-pdf
        "zip" | "gz" | "xz" | "tar" | "tgz" | "7z" => '\u{e6aa}', // seti-zip
        "ttf" | "otf" | "woff" | "woff2" => '\u{e659}',           // seti-font
        "lock" => '\u{e672}',                                     // seti-lock
        "metal" | "glsl" | "wgsl" => '\u{e615}',                  // seti-config
        _ => '\u{e64e}',                                          // seti-default
    };
    Icon { glyph }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::font::{Atlas, is_icon};

    #[test]
    fn a_whole_name_beats_an_extension() {
        assert_eq!(for_file(Path::new("src/Cargo.toml")).glyph, '\u{e68b}');
        assert_eq!(for_file(Path::new("other.toml")).glyph, '\u{e6b2}');
        assert_eq!(for_file(Path::new("INDEX.HTML")).glyph, '\u{e60e}');
        assert_eq!(for_file(Path::new("no_extension")).glyph, '\u{e64e}');
        assert_eq!(for_directory("src", true).glyph, FOLDER_OPEN);
        assert_eq!(for_directory(".git", true).glyph, FOLDER_GIT);
    }

    /// A code point typed wrong is an icon that silently never draws. Every
    /// one used here has to exist in the font that is actually compiled in.
    #[test]
    fn every_icon_used_exists_in_the_bundled_font() {
        let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
        let names = [
            "Cargo.toml",
            "package.json",
            "bun.lock",
            "tsconfig.json",
            "Dockerfile",
            "Makefile",
            "LICENSE",
            ".gitignore",
            "favicon.ico",
            "a.rs",
            "a.html",
            "a.css",
            "a.js",
            "a.ts",
            "a.tsx",
            "a.svelte",
            "a.vue",
            "a.json",
            "a.md",
            "a.go",
            "a.py",
            "a.c",
            "a.cpp",
            "a.toml",
            "a.yml",
            "a.ini",
            "a.sh",
            "a.sql",
            "a.csv",
            "a.svg",
            "a.png",
            "a.pdf",
            "a.zip",
            "a.ttf",
            "a.lock",
            "a.metal",
            "a.unknown",
        ];
        let mut glyphs: Vec<char> = names.iter().map(|n| for_file(Path::new(n)).glyph).collect();
        for name in ["src", ".git", ".github", ".cargo"] {
            glyphs.push(for_directory(name, false).glyph);
            glyphs.push(for_directory(name, true).glyph);
        }
        glyphs.extend([CHEVRON_RIGHT, CHEVRON_DOWN, HOME, SEARCH, FOLDER]);
        glyphs.extend([
            SYMBOL_METHOD,
            SYMBOL_VARIABLE,
            SYMBOL_NAMESPACE,
            SYMBOL_TEXT,
            SYMBOL_CLASS,
            SYMBOL_CONSTANT,
            SYMBOL_FIELD,
            HISTORY,
            FILE,
            FOLDER_OUTLINE,
        ]);
        glyphs.sort_unstable();
        glyphs.dedup();

        for glyph in glyphs {
            assert!(is_icon(glyph), "{glyph:?} is not a private-use character");
            let slot = atlas.slot_for(glyph);
            assert!(
                slot.is_some(),
                "U+{:04X} is not in the icon font",
                glyph as u32
            );
            assert_eq!(slot.expect("checked").cells, 2, "icons take two cells");
        }
    }
}
