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
