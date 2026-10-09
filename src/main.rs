//! crc: an editor.

use crc::platform::settings::Settings;
use crc::platform::{recovery, window};
use crc::text::buffer::Buffer;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--version" || a == "-V") {
        println!("crc {}", crc::build_label());
        return;
    }
    // An agent's hook, run by Claude Code inside a crc terminal: keeps the
    // file it is about to write. Silent, and always succeeds.
    if args.first().is_some_and(|a| a == "--hook") {
        crc::project::review::run_hook(args.get(1).map_or("", String::as_str));
        return;
    }
    // An MCP server for agents, on stdio, for the folder given or the
    // current one. No window.
    if args.first().is_some_and(|a| a == "--mcp") {
        let folder = match args.get(1) {
            Some(path) => PathBuf::from(path),
            None => std::env::current_dir().unwrap_or_default(),
        };
        if !folder.is_dir() {
            eprintln!("crc: {} is not a folder", folder.display());
            std::process::exit(2);
        }
        crc::mcp_server::run(&folder);
        return;
    }

    let folder = args
        .first()
        .filter(|path| std::path::Path::new(path).is_dir())
        .map(|path| crc::platform::canonical(std::path::Path::new(path)));
    let buffer = match args.first().filter(|_| folder.is_none()) {
        // Buffer::open rather than reading the file here: it records the
        // path, which is what makes Cmd-S save back rather than fail.
        Some(path) => match Buffer::open(path) {
            Ok(buffer) => buffer,
            Err(err) => {
                eprintln!("crc: cannot read {path}: {err}");
                std::process::exit(1);
            }
        },
        // Nothing to open is the home screen, which the window draws for an
        // untouched empty buffer. It used to be a buffer full of welcome
        // text, which was not empty, so nothing ever replaced it and every
        // launch left a stray "Untitled" tab beside the restored ones.
        None => Buffer::new(),
    };

    migrate_support_dir();
    if let Some(sessions) = crc::project::review::root() {
        crc::project::review::prune(&sessions, crc::project::review::MAX_AGE);
    }

    // Before anything that can panic with documents open. Release builds
    // abort on panic, and the hook is what saves unsaved work first.
    if let Some(dir) = recovery::default_dir() {
        recovery::install(dir);
    }

    let settings = Settings::load();
    window::run(buffer, folder, &settings.font, settings.font_size);
}

/// The app was called caio-code until 2026-09-23. Its session and any
/// recovered unsaved work live under the old name, so carry them over once.
fn migrate_support_dir() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let support = PathBuf::from(home).join("Library/Application Support");
    let (old, new) = (support.join("caio-code"), support.join("crc"));
    if old.is_dir()
        && !new.exists()
        && let Err(e) = std::fs::rename(&old, &new)
    {
        eprintln!(
            "crc: could not move {} to {}: {e}",
            old.display(),
            new.display()
        );
    }
}
