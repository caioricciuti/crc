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

    let folder = args
        .first()
        .filter(|path| std::path::Path::new(path).is_dir())
        .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path)));
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
