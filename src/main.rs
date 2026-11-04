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
    // A terminal held for the window: see `crc::term::hold`. No window.
    if args.first().is_some_and(|a| a == "--hold") {
        let (Some(dir), Some(id)) = (args.get(1), args.get(2)) else {
            eprintln!("crc: --hold needs a folder and an id");
            std::process::exit(2);
        };
        crc::term::hold::run(std::path::Path::new(dir), id);
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

    // First, before anything below can panic. Release builds abort on
    // panic, and the hook is what saves unsaved work first; under the
    // self-test it is also what keeps a panic from ending in a Problem
    // Report, which it does with or without a folder to write to.
    recovery::install(recovery::default_dir());

    // What this window runs from, to notice an install over it.
    crc::platform::update::note_launch();

    let folder = args
        .first()
        .filter(|path| std::path::Path::new(path).is_dir())
        .map(|path| crc::platform::canonical(std::path::Path::new(path)));
    let buffer = match args.first().filter(|_| folder.is_none()) {
        Some(path) => match open_argument(path) {
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
    prune_sessions();

    let settings = Settings::load();
    window::run(buffer, folder, &settings.font, settings.font_size);
}

/// The document for a path named on the command line. `Buffer::open` rather
/// than reading the file here: it records the path, which is what makes
/// Cmd-S save back rather than fail. A path that does not exist yet, in a
/// folder that does, is a new file, as `crc notes.txt` means it in a
/// terminal: an empty document with that path, which Cmd-S creates.
fn open_argument(path: &str) -> std::io::Result<Buffer> {
    match Buffer::open(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let absolute = std::path::absolute(path)?;
            let (Some(parent), Some(name)) = (absolute.parent(), absolute.file_name()) else {
                return Err(err);
            };
            if !parent.is_dir() {
                return Err(err);
            }
            let mut buffer = Buffer::new();
            // The folder resolved as an opened file's path would be, so the
            // save and the tab agree on the key.
            buffer.path = Some(crc::platform::canonical(parent).join(name));
            Ok(buffer)
        }
        result => result,
    }
}

/// Agent review sessions whose terminal is gone: aged out, except those of
/// the terminals helpers still hold, which come back with the window. Once,
/// every session without a held terminal goes whatever its age: closing a
/// terminal removes its session now, and the sessions of terminals closed
/// under earlier builds stayed on disk, tens of thousands of files.
fn prune_sessions() {
    use crc::project::review;
    let Some(sessions) = review::root() else {
        return;
    };
    let held =
        crc::term::hold::dir().map_or_else(Vec::new, |dir| crc::term::hold::held_reviews(&dir));
    review::prune(&sessions, review::MAX_AGE, &held);
    let Some(once) = crc::platform::app_support().map(|dir| dir.join("sessions-pruned")) else {
        return;
    };
    if once.exists() {
        return;
    }
    review::prune_closed(&sessions, &held);
    if let Some(parent) = once.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(once, "");
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

#[cfg(test)]
mod tests {
    use super::open_argument;

    /// Regression: `crc newfile.txt` for a file not there yet exited with
    /// "cannot read" instead of starting the file.
    #[test]
    fn a_missing_file_in_a_folder_that_exists_starts_a_new_document() {
        let dir = std::env::temp_dir().join(format!("crc-open-argument-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("new.txt");
        let mut buffer = open_argument(path.to_str().unwrap()).unwrap();
        assert_eq!(
            buffer.path.as_deref(),
            Some(crc::platform::canonical(&dir).join("new.txt").as_path())
        );
        assert!(!buffer.is_dirty());
        assert_eq!(buffer.rope.len_bytes(), 0);
        // Cmd-S creates it, no Save As panel in between.
        buffer.insert("hello");
        buffer.save(None).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        // A file that is there reads as before.
        assert_eq!(
            open_argument(path.to_str().unwrap())
                .unwrap()
                .rope
                .to_string(),
            "hello"
        );
        // A folder that is not there is still the error it was.
        let missing = dir.join("no-such-folder").join("new.txt");
        assert!(open_argument(missing.to_str().unwrap()).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
