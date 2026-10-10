//! The session helper, run as the real binary: a program it holds outlives
//! the window that started it, and the next window gets its output back.

use crc::term::hold::{self, Frame, Spec};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn scratch(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!("crc-held-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn shell_spec(dir: &Path) -> Spec {
    Spec {
        program: "/bin/sh".into(),
        args: Vec::new(),
        cwd: "/".into(),
        env: vec![("PS1".into(), "$ ".into())],
        unset: Vec::new(),
        cols: 40,
        rows: 6,
        title: "shell".into(),
        claude: false,
        review: dir.join("review"),
        project: dir.to_path_buf(),
    }
}

/// Output frames until `text` has been seen, or the connection ends.
fn read_until(socket: &mut UnixStream, text: &str) -> String {
    let mut seen = String::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !seen.contains(text) && Instant::now() < deadline {
        match hold::receive(socket) {
            Ok(Frame::Output(bytes)) => seen.push_str(&String::from_utf8_lossy(&bytes)),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    seen
}

fn wait_until(what: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if what() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn a_held_program_outlives_its_window() {
    let dir = scratch("socket");
    let spec = shell_spec(&dir);
    let helper = Path::new(env!("CARGO_BIN_EXE_crc"));
    let (id, mut socket) = hold::start(&dir, &spec, helper).expect("the helper starts");
    assert_eq!(hold::receive(&mut socket).unwrap(), Frame::Size(40, 6));
    // Then what it printed so far: nothing, or already the prompt.
    assert!(matches!(
        hold::receive(&mut socket).unwrap(),
        Frame::Output(_)
    ));
    hold::send(
        &mut socket,
        &Frame::Input(b"echo held-$((40+2))\n".to_vec()),
    )
    .unwrap();
    let seen = read_until(&mut socket, "held-42");
    assert!(seen.contains("held-42"), "{seen}");

    // The window goes: the helper stays, listed with what it runs.
    drop(socket);
    std::thread::sleep(Duration::from_millis(100));
    let live = hold::live(&dir);
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].0, id);
    assert_eq!(live[0].1, spec);
    hold::leave(
        &dir,
        &id,
        &hold::Left {
            events_read: 7,
            active: true,
        },
    );
    assert_eq!(hold::live(&dir)[0].2.events_read, 7);

    // The next window gets the output back, then runs it as before.
    let mut socket = hold::connect(&dir, &id).expect("connects again");
    assert_eq!(hold::receive(&mut socket).unwrap(), Frame::Size(40, 6));
    let replay = match hold::receive(&mut socket).unwrap() {
        Frame::Output(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        other => panic!("{other:?}"),
    };
    assert!(replay.contains("held-42"), "{replay}");
    hold::send(&mut socket, &Frame::Resize(50, 8)).unwrap();
    hold::send(&mut socket, &Frame::Input(b"stty size\n".to_vec())).unwrap();
    let seen = read_until(&mut socket, "8 50");
    assert!(seen.contains("8 50"), "{seen}");

    // Closing the tab ends it, and its files go with it.
    hold::send(&mut socket, &Frame::Kill).unwrap();
    let ended = loop {
        match hold::receive(&mut socket) {
            Ok(Frame::Exited(code)) => break Some(code),
            Ok(_) => {}
            Err(_) => break None,
        }
    };
    assert_eq!(ended, Some(-1), "a hang-up, not an exit code");
    assert!(wait_until(|| hold::live(&dir).is_empty()));
    assert!(wait_until(|| std::fs::read_dir(&dir)
        .map(|entries| entries.count() == 0)
        .unwrap_or(false)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_session_attaches_to_a_held_program() {
    let dir = scratch("session");
    let spec = shell_spec(&dir);
    let helper = Path::new(env!("CARGO_BIN_EXE_crc"));
    let (id, socket) = hold::start(&dir, &spec, helper).expect("the helper starts");
    let mut session = crc::term::pty::Session::attach(socket, Box::new(|| {})).expect("attaches");
    session.write(b"echo attached-$((40+2))\n");
    let shown = |session: &crc::term::pty::Session| session.term.lock().unwrap().screen_text();
    assert!(
        wait_until(|| shown(&session).contains("attached-42")),
        "{}",
        shown(&session)
    );
    assert!(session.is_held());
    // Let go: dropping it hangs up on a program run here, not on a held one.
    session.detach();
    drop(session);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(hold::live(&dir).len(), 1);

    let socket = hold::connect(&dir, &id).expect("connects again");
    let session = crc::term::pty::Session::attach(socket, Box::new(|| {})).expect("attaches again");
    assert!(
        wait_until(|| shown(&session).contains("attached-42")),
        "{}",
        shown(&session)
    );
    session.resize(50, 8);
    assert_eq!(session.term.lock().unwrap().cols(), 50);
    // Dropped without detaching, as closing the tab does: the program ends.
    drop(session);
    assert!(wait_until(|| hold::live(&dir).is_empty()));
    let _ = std::fs::remove_dir_all(&dir);
}
