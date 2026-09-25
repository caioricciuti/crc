//! Which server speaks for which language, and where it is on this Mac.
//!
//! The list is short and each entry is one binary with an LSP mode. They
//! are found by looking where each toolchain installs them, plus `PATH` as
//! the app sees it, which from the Dock is only the system default. Nothing
//! here runs a shell to learn a profile's `PATH`; that decision stays with
//! the user (roadmap, end of phase 6).

use std::path::PathBuf;

use crate::syntax::Language;

/// A server and how to start it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// Shown in the status line.
    pub name: &'static str,
    /// Binaries to try, in order. The first one found wins.
    pub binaries: &'static [&'static str],
    pub args: &'static [&'static str],
}

/// The protocol's name for the language of a file.
pub fn language_id(language: Language) -> &'static str {
    match language {
        Language::Rust => "rust",
        Language::Go => "go",
        Language::Python => "python",
        Language::TypeScript => "typescript",
        Language::Tsx => "typescriptreact",
        Language::JavaScript => "javascript",
        Language::Json => "json",
        Language::Html => "html",
        Language::Css => "css",
        Language::C => "c",
        Language::Cpp => "cpp",
        Language::Toml => "toml",
        Language::Yaml => "yaml",
        Language::Bash => "shellscript",
    }
}

/// The server for `language`, if the editor knows one.
pub fn spec_for(language: Language) -> Option<Spec> {
    Some(match language {
        Language::Rust => Spec {
            name: "rust-analyzer",
            binaries: &["rust-analyzer"],
            args: &[],
        },
        Language::Go => Spec {
            name: "gopls",
            binaries: &["gopls"],
            args: &[],
        },
        Language::Python => Spec {
            name: "pyright",
            binaries: &["basedpyright-langserver", "pyright-langserver"],
            args: &["--stdio"],
        },
        Language::TypeScript | Language::Tsx | Language::JavaScript => Spec {
            name: "typescript",
            binaries: &["typescript-language-server"],
            args: &["--stdio"],
        },
        Language::C | Language::Cpp => Spec {
            name: "clangd",
            binaries: &["clangd"],
            args: &[],
        },
        Language::Json
        | Language::Html
        | Language::Css
        | Language::Toml
        | Language::Yaml
        | Language::Bash => return None,
    })
}

/// Languages that share one server share one process: a TSX file and a
/// JavaScript file both talk to the TypeScript server.
pub fn server_key(language: Language) -> Language {
    match language {
        Language::Tsx | Language::JavaScript => Language::TypeScript,
        Language::Cpp => Language::C,
        other => other,
    }
}

/// Where binaries are looked for, in order: the installed Rust toolchains,
/// then the app's own `PATH`, then where cargo, Go, bun, Homebrew, pnpm and
/// pip put things.
///
/// Toolchains come first on purpose. The `rust-analyzer` on `PATH` is
/// usually a rustup proxy, and Homebrew's rustup proxy recurses until it
/// gives up; the real binary sits in `~/.rustup/toolchains/*/bin`. The
/// newest toolchain wins.
pub fn search_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs: Vec<PathBuf> = Vec::new();
    let rustup_home = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".rustup")));
    if let Some(toolchains) = rustup_home.map(|r| r.join("toolchains"))
        && let Ok(entries) = std::fs::read_dir(&toolchains)
    {
        let mut found: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path().join("bin"))
            .filter(|p| p.is_dir())
            .collect();
        found.sort();
        found.reverse();
        dirs.extend(found);
    }
    dirs.extend(
        std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .unwrap_or_default(),
    );
    if let Some(home) = &home {
        for relative in [
            ".cargo/bin",
            "go/bin",
            ".bun/bin",
            "Library/pnpm",
            ".local/bin",
            ".npm-global/bin",
        ] {
            dirs.push(home.join(relative));
        }
    }
    if let Some(pnpm) = std::env::var_os("PNPM_HOME") {
        dirs.push(PathBuf::from(pnpm));
    }
    for fixed in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/local/go/bin"] {
        dirs.push(PathBuf::from(fixed));
    }
    dirs
}

/// A server ready to start: its name, program and arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub name: String,
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// How to start the server for `language`. `Err` says why there is none:
/// no server known, or none installed.
///
/// `CRC_LSP_FAKE=<script>` points every language at that script under
/// python3, which is how the GUI self-test drives the whole path without a
/// real server installed.
pub fn launch_for(language: Language) -> Result<Launch, String> {
    if let Some(script) = std::env::var_os("CRC_LSP_FAKE") {
        let python = ["/usr/bin/python3", "/opt/homebrew/bin/python3"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
            .ok_or("no python3 for the fake server")?;
        return Ok(Launch {
            name: "fake".into(),
            program: python,
            args: vec![script.to_string_lossy().into_owned()],
        });
    }
    let spec = spec_for(language)
        .ok_or_else(|| format!("no language server for {}", language_id(language)))?;
    let program = locate(&spec).ok_or_else(|| {
        format!(
            "{} is not installed (looked for {})",
            spec.name,
            spec.binaries.join(", ")
        )
    })?;
    Ok(Launch {
        name: spec.name.to_owned(),
        program,
        args: spec.args.iter().map(|a| (*a).to_owned()).collect(),
    })
}

/// The first of `spec`'s binaries found in [`search_dirs`].
pub fn locate(spec: &Spec) -> Option<PathBuf> {
    let dirs = search_dirs();
    spec.binaries.iter().find_map(|binary| {
        dirs.iter()
            .map(|dir| dir.join(binary))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_has_an_id_and_shared_servers_share_a_key() {
        for language in Language::ALL {
            assert!(!language_id(language).is_empty());
        }
        assert_eq!(server_key(Language::Tsx), Language::TypeScript);
        assert_eq!(server_key(Language::JavaScript), Language::TypeScript);
        assert_eq!(server_key(Language::Rust), Language::Rust);
        assert!(spec_for(Language::Json).is_none());
    }

    #[test]
    fn search_covers_the_usual_toolchain_locations() {
        let dirs = search_dirs();
        assert!(dirs.iter().any(|d| d.ends_with(".cargo/bin")));
        // Installed toolchains come before anything on PATH, so a real
        // rust-analyzer beats a rustup proxy of the same name.
        if let Some(first_toolchain) = dirs
            .iter()
            .position(|d| d.to_string_lossy().contains("/toolchains/"))
        {
            let first_path_entry = std::env::var_os("PATH")
                .and_then(|p| std::env::split_paths(&p).next())
                .and_then(|p| dirs.iter().position(|d| *d == p));
            if let Some(first_path_entry) = first_path_entry {
                assert!(first_toolchain < first_path_entry);
            }
        }
        assert!(
            dirs.iter()
                .any(|d| d == std::path::Path::new("/opt/homebrew/bin"))
        );
        let missing = Spec {
            name: "none",
            binaries: &["caio-no-such-server"],
            args: &[],
        };
        assert!(locate(&missing).is_none());
    }
}
