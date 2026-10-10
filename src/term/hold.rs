//! Sessions that outlive the window: a helper process holds the terminal.
//!
//! `crc --hold <dir> <id>` reads `<id>.spec` in `dir`, starts the program it
//! names on a terminal of its own (through [`super::pty`]) and listens on
//! a socket named after the id (under `/tmp`, where the path is short
//! enough for one; `<id>.pid` in `dir` says which process). The window
//! connects, and from then on the program's output
//! goes to the window and the window's keys to the program, as framed
//! messages over that socket. The helper keeps the last [`KEEP`] bytes of
//! output; a window that connects later gets them first, so its screen is
//! what the program drew. When the window goes away, by quitting or by a
//! crash, the helper keeps the program running. It ends when the program
//! does, or when a window says so (closing the tab), and takes its files
//! with it.
//!
//! The helper is started in a session of its own, so the window's end is
//! not its end. The protocol is private to this binary: a tag byte, a
//! four-byte little-endian length, the payload.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How much output a helper keeps for the next window: enough for the
/// screen a full-screen program last drew and some history, bounded so an
/// agent that prints for hours stays cheap.
pub const KEEP: usize = 2 << 20;

/// How long the window waits for a helper it just started to listen.
const START_TIMEOUT: Duration = Duration::from_secs(5);

const SIGHUP: i32 = 1;

/// Where the helpers' files live: `CRC_HELD_DIR`, or `held` under
/// Application Support.
pub fn dir() -> Option<PathBuf> {
    match std::env::var_os("CRC_HELD_DIR") {
        Some(dir) => Some(PathBuf::from(dir)),
        None => Some(crate::platform::app_support()?.join("held")),
    }
}

/// What a helper runs, and what the window needs to show it again.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Spec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub unset: Vec<String>,
    pub cols: usize,
    pub rows: usize,
    /// The tab's title.
    pub title: String,
    /// Runs Claude Code.
    pub claude: bool,
    /// Its agent review session's folder.
    pub review: PathBuf,
    /// The project the window had open: it comes back with that one.
    pub project: PathBuf,
}

/// What the window wrote about the tab when it let go: read back when it
/// takes it again.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Left {
    /// How far into the review session's events it had read.
    pub events_read: u64,
    /// It was the tab with the keyboard.
    pub active: bool,
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\n', "\\n")
}

fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

fn lines(text: &str) -> impl Iterator<Item = (&str, String)> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key, unescape(value)))
}

impl Spec {
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        let mut put = |key: &str, value: &str| {
            out.push_str(key);
            out.push('=');
            out.push_str(&escape(value));
            out.push('\n');
        };
        put("program", &self.program.to_string_lossy());
        for arg in &self.args {
            put("arg", arg);
        }
        put("cwd", &self.cwd.to_string_lossy());
        for (name, value) in &self.env {
            put("env", &format!("{name}={value}"));
        }
        for name in &self.unset {
            put("unset", name);
        }
        put("cols", &self.cols.to_string());
        put("rows", &self.rows.to_string());
        put("title", &self.title);
        put("claude", if self.claude { "1" } else { "0" });
        put("review", &self.review.to_string_lossy());
        put("project", &self.project.to_string_lossy());
        out
    }

    pub fn parse(text: &str) -> Spec {
        let mut spec = Spec::default();
        for (key, value) in lines(text) {
            match key {
                "program" => spec.program = value.into(),
                "arg" => spec.args.push(value),
                "cwd" => spec.cwd = value.into(),
                "env" => {
                    if let Some((name, value)) = value.split_once('=') {
                        spec.env.push((name.to_owned(), value.to_owned()));
                    }
                }
                "unset" => spec.unset.push(value),
                "cols" => spec.cols = value.parse().unwrap_or(80),
                "rows" => spec.rows = value.parse().unwrap_or(24),
                "title" => spec.title = value,
                "claude" => spec.claude = value == "1",
                "review" => spec.review = value.into(),
                "project" => spec.project = value.into(),
                _ => {}
            }
        }
        spec
    }
}

impl Left {
    pub fn to_text(&self) -> String {
        format!(
            "events_read={}\nactive={}\n",
            self.events_read,
            if self.active { "1" } else { "0" }
        )
    }

    pub fn parse(text: &str) -> Left {
        let mut left = Left::default();
        for (key, value) in lines(text) {
            match key {
                "events_read" => left.events_read = value.parse().unwrap_or(0),
                "active" => left.active = value == "1",
                _ => {}
            }
        }
        left
    }
}

/// A message either way over the socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    /// Helper to window: the terminal's size, first thing on connecting.
    Size(usize, usize),
    /// Helper to window: what the program printed.
    Output(Vec<u8>),
    /// Helper to window: the program ended, with this code (-1: a signal).
    Exited(i32),
    /// Window to helper: what was typed.
    Input(Vec<u8>),
    /// Window to helper: the window's grid changed.
    Resize(usize, usize),
    /// Window to helper: end the program.
    Kill,
}

const SIZE: u8 = 1;
const OUTPUT: u8 = 2;
const EXITED: u8 = 3;
const INPUT: u8 = 4;
const RESIZE: u8 = 5;
const KILL: u8 = 6;

fn size_bytes(cols: usize, rows: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(4);
    out.extend_from_slice(&(cols.min(u16::MAX as usize) as u16).to_le_bytes());
    out.extend_from_slice(&(rows.min(u16::MAX as usize) as u16).to_le_bytes());
    out
}

fn size_of(bytes: &[u8]) -> Option<(usize, usize)> {
    if bytes.len() != 4 {
        return None;
    }
    let cols = u16::from_le_bytes([bytes[0], bytes[1]]) as usize;
    let rows = u16::from_le_bytes([bytes[2], bytes[3]]) as usize;
    Some((cols, rows))
}

pub fn send(to: &mut impl Write, frame: &Frame) -> std::io::Result<()> {
    let (tag, payload): (u8, std::borrow::Cow<[u8]>) = match frame {
        Frame::Size(cols, rows) => (SIZE, size_bytes(*cols, *rows).into()),
        Frame::Output(bytes) => (OUTPUT, bytes.as_slice().into()),
        Frame::Exited(code) => (EXITED, code.to_le_bytes().to_vec().into()),
        Frame::Input(bytes) => (INPUT, bytes.as_slice().into()),
        Frame::Resize(cols, rows) => (RESIZE, size_bytes(*cols, *rows).into()),
        Frame::Kill => (KILL, Vec::new().into()),
    };
    let mut head = [0u8; 5];
    head[0] = tag;
    head[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    to.write_all(&head)?;
    to.write_all(&payload)?;
    to.flush()
}

/// The next frame, or the error that ended the connection (EOF included).
pub fn receive(from: &mut impl Read) -> std::io::Result<Frame> {
    let mut head = [0u8; 5];
    from.read_exact(&mut head)?;
    let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
    let mut payload = vec![0u8; len];
    from.read_exact(&mut payload)?;
    let bad = || std::io::Error::other("malformed frame from the session helper");
    Ok(match head[0] {
        SIZE => size_of(&payload)
            .map(|(c, r)| Frame::Size(c, r))
            .ok_or_else(bad)?,
        OUTPUT => Frame::Output(payload),
        EXITED => {
            let bytes: [u8; 4] = payload.as_slice().try_into().map_err(|_| bad())?;
            Frame::Exited(i32::from_le_bytes(bytes))
        }
        INPUT => Frame::Input(payload),
        RESIZE => size_of(&payload)
            .map(|(c, r)| Frame::Resize(c, r))
            .ok_or_else(bad)?,
        KILL => Frame::Kill,
        _ => return Err(bad()),
    })
}

fn spec_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.spec"))
}

/// Where the sockets live: a folder of this user's own under `/tmp`, since
/// a socket path must fit in about a hundred bytes and Application Support
/// under a long home does not. Refused unless it is ours alone.
fn socket_dir() -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let uid = unsafe { getuid() };
    let dir = PathBuf::from(format!("/tmp/crc-held-{uid}"));
    let made = std::fs::DirBuilder::new().mode(0o700).create(&dir);
    if let Err(e) = made
        && e.kind() != std::io::ErrorKind::AlreadyExists
    {
        return Err(e);
    }
    let meta = std::fs::symlink_metadata(&dir)?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(std::io::Error::other(format!(
            "{} is not a private folder of this user",
            dir.display()
        )));
    }
    Ok(dir)
}

fn socket_path(id: &str) -> std::io::Result<PathBuf> {
    Ok(socket_dir()?.join(format!("{id}.sock")))
}

fn pid_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.pid"))
}

fn left_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.left"))
}

fn remove_all(dir: &Path, id: &str) {
    for path in [spec_path(dir, id), pid_path(dir, id), left_path(dir, id)] {
        let _ = std::fs::remove_file(path);
    }
    if let Ok(socket) = socket_path(id) {
        let _ = std::fs::remove_file(socket);
    }
}

/// Writes what the window knew about a held tab, for the next window.
pub fn leave(dir: &Path, id: &str, left: &Left) {
    let _ = std::fs::write(left_path(dir, id), left.to_text());
}

/// Starts a helper for `spec` with `helper` (this binary, normally) and
/// connects to it: the id the window keeps, and the socket for
/// [`super::pty::Session::attach`].
pub fn start(dir: &Path, spec: &Spec, helper: &Path) -> std::io::Result<(String, UnixStream)> {
    std::fs::create_dir_all(dir)?;
    let id = crate::project::review::new_id();
    std::fs::write(spec_path(dir, &id), spec.to_text())?;
    let mut command = std::process::Command::new(helper);
    command
        .arg("--hold")
        .arg(dir)
        .arg(&id)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // A session of its own: the window's end, or its terminal's, is not
    // a hang-up for the helper.
    unsafe {
        command.pre_exec(|| {
            if setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    if let Err(e) = crate::platform::spawn_reaped(&mut command) {
        remove_all(dir, &id);
        return Err(e);
    }
    let deadline = Instant::now() + START_TIMEOUT;
    let socket = socket_path(&id)?;
    loop {
        match UnixStream::connect(&socket) {
            Ok(socket) => return Ok((id, socket)),
            Err(e) => {
                // The spec gone means the helper ran and could not start
                // the program; it took its files with it.
                if !spec_path(dir, &id).exists() {
                    return Err(std::io::Error::other(format!(
                        "the session helper could not start {}",
                        spec.program.display()
                    )));
                }
                if Instant::now() >= deadline {
                    remove_all(dir, &id);
                    return Err(std::io::Error::new(
                        e.kind(),
                        format!("the session helper did not answer: {e}"),
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// Connects to the helper `id` in `dir`.
pub fn connect(_dir: &Path, id: &str) -> std::io::Result<UnixStream> {
    UnixStream::connect(socket_path(id)?)
}

/// Every helper in `dir` still running, oldest first, with what it runs
/// and what the last window left about it. The files of helpers that are
/// gone (a reboot, a kill) are removed on the way.
pub fn live(dir: &Path) -> Vec<(String, Spec, Left)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            name.strip_suffix(".spec").map(str::to_owned)
        })
        .collect();
    ids.sort();
    let mut found = Vec::new();
    for id in ids {
        let alive = std::fs::read_to_string(pid_path(dir, &id))
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
            .is_some_and(|pid| pid > 0 && crate::platform::send_signal(pid, 0))
            && socket_path(&id).is_ok_and(|socket| socket.exists());
        if !alive {
            remove_all(dir, &id);
            continue;
        }
        let Ok(text) = std::fs::read_to_string(spec_path(dir, &id)) else {
            continue;
        };
        let left = std::fs::read_to_string(left_path(dir, &id))
            .map(|text| Left::parse(&text))
            .unwrap_or_default();
        found.push((id, Spec::parse(&text), left));
    }
    found
}

unsafe extern "C" {
    fn setsid() -> i32;
    fn getuid() -> u32;
}

/// What the helper's threads share.
struct Hold {
    /// The window attached now, numbered so a reader thread knows whether
    /// the connection it lost was still the current one.
    client: Option<(u64, UnixStream)>,
    clients: u64,
    /// The last [`KEEP`] bytes the program printed.
    kept: VecDeque<u8>,
    cols: usize,
    rows: usize,
}

impl Hold {
    /// Sends `frame` to the attached window, if any; a window that cannot
    /// take it is gone.
    fn tell(&mut self, frame: &Frame) {
        if let Some((_, client)) = &mut self.client
            && send(client, frame).is_err()
        {
            self.client = None;
        }
    }

    fn keep(&mut self, bytes: &[u8]) {
        if bytes.len() >= KEEP {
            self.kept.clear();
            self.kept.extend(&bytes[bytes.len() - KEEP..]);
            return;
        }
        let over = (self.kept.len() + bytes.len()).saturating_sub(KEEP);
        if over > 0 {
            self.kept.drain(..over);
        }
        self.kept.extend(bytes);
    }
}

/// The helper: runs the spec's program in `dir` as `id` until it ends or a
/// window ends it. Never returns.
pub fn run(dir: &Path, id: &str) -> ! {
    let Ok(text) = std::fs::read_to_string(spec_path(dir, id)) else {
        remove_all(dir, id);
        std::process::exit(2);
    };
    let spec = Spec::parse(&text);
    let (master, slave) = match super::pty::open_pair(spec.cols, spec.rows) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("crc --hold: {e}");
            remove_all(dir, id);
            std::process::exit(2);
        }
    };
    let args: Vec<&str> = spec.args.iter().map(String::as_str).collect();
    let env: Vec<(&str, &str)> = spec
        .env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let unset: Vec<&str> = spec.unset.iter().map(String::as_str).collect();
    let child = match super::pty::start_on(slave, &spec.program, &args, &spec.cwd, &env, &unset) {
        Ok(child) => child,
        Err(e) => {
            eprintln!("crc --hold: {}: {e}", spec.program.display());
            remove_all(dir, id);
            std::process::exit(2);
        }
    };
    let pid = child.id() as i32;
    let socket = match socket_path(id) {
        Ok(socket) => socket,
        Err(e) => {
            eprintln!("crc --hold: {e}");
            crate::platform::send_signal(-pid, SIGHUP);
            remove_all(dir, id);
            std::process::exit(2);
        }
    };
    let _ = std::fs::remove_file(&socket);
    let listener = match UnixListener::bind(&socket) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("crc --hold: {}: {e}", socket.display());
            crate::platform::send_signal(-pid, SIGHUP);
            remove_all(dir, id);
            std::process::exit(2);
        }
    };
    let _ = std::fs::write(pid_path(dir, id), std::process::id().to_string());

    let master = std::fs::File::from(master);
    let hold = Arc::new(Mutex::new(Hold {
        client: None,
        clients: 0,
        kept: VecDeque::new(),
        cols: spec.cols,
        rows: spec.rows,
    }));
    let ended = Arc::new(AtomicBool::new(false));
    let (dir, id) = (dir.to_path_buf(), id.to_owned());

    // Reaped on a thread of its own; the program's end is the helper's.
    {
        let (hold, dir, id, ended) = (hold.clone(), dir.clone(), id.clone(), ended.clone());
        std::thread::spawn(move || {
            let mut child = child;
            let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
            ended.store(true, Ordering::Release);
            hold.lock()
                .unwrap_or_else(|e| e.into_inner())
                .tell(&Frame::Exited(code));
            remove_all(&dir, &id);
            std::process::exit(0);
        });
    }
    // The program's output: kept, and passed to the window attached.
    {
        let (hold, dir, id, ended) = (hold.clone(), dir.clone(), id.clone(), ended.clone());
        let mut reader = master.try_clone().unwrap_or_else(|_| std::process::exit(2));
        std::thread::spawn(move || {
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                let n = match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let mut hold = hold.lock().unwrap_or_else(|e| e.into_inner());
                hold.keep(&buffer[..n]);
                hold.tell(&Frame::Output(buffer[..n].to_vec()));
            }
            // The terminal closed: the program is ending, or ended and left
            // nothing holding it. A moment for the wait thread to say so
            // with the real code; then this is the end either way.
            let deadline = Instant::now() + Duration::from_secs(2);
            while !ended.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if !ended.load(Ordering::Acquire) {
                crate::platform::send_signal(-pid, SIGHUP);
                hold.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .tell(&Frame::Exited(-1));
                remove_all(&dir, &id);
                std::process::exit(0);
            }
            // The wait thread is exiting the process.
            std::thread::sleep(Duration::from_secs(5));
            std::process::exit(0);
        });
    }
    // Input goes to the program from a thread of its own, so a program
    // that stops reading blocks that thread, never the socket.
    let (input, typed) = std::sync::mpsc::channel::<Vec<u8>>();
    {
        let mut writer = master.try_clone().unwrap_or_else(|_| std::process::exit(2));
        std::thread::spawn(move || {
            for bytes in typed {
                if writer.write_all(&bytes).is_err() {
                    return;
                }
            }
        });
    }

    // Windows come and go; the newest is the one that counts.
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else {
            continue;
        };
        let number = {
            let mut hold = hold.lock().unwrap_or_else(|e| e.into_inner());
            let replay: Vec<u8> = hold.kept.iter().copied().collect();
            let hello = send(&mut stream, &Frame::Size(hold.cols, hold.rows))
                .and_then(|()| send(&mut stream, &Frame::Output(replay)));
            if hello.is_err() {
                continue;
            }
            let Ok(sender) = stream.try_clone() else {
                continue;
            };
            hold.clients += 1;
            hold.client = Some((hold.clients, sender));
            hold.clients
        };
        let (hold, input) = (hold.clone(), input.clone());
        let master = master.try_clone().unwrap_or_else(|_| std::process::exit(2));
        std::thread::spawn(move || {
            loop {
                match receive(&mut stream) {
                    Ok(Frame::Input(bytes)) => {
                        let _ = input.send(bytes);
                    }
                    Ok(Frame::Resize(cols, rows)) => {
                        super::pty::resize_master(&master, cols, rows);
                        let mut hold = hold.lock().unwrap_or_else(|e| e.into_inner());
                        hold.cols = cols;
                        hold.rows = rows;
                    }
                    Ok(Frame::Kill) => {
                        crate::platform::send_signal(-pid, SIGHUP);
                    }
                    // Nothing else is for the helper; a window gone, by
                    // quitting or by a crash, leaves the program running.
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            let mut hold = hold.lock().unwrap_or_else(|e| e.into_inner());
            if hold.client.as_ref().is_some_and(|(n, _)| *n == number) {
                hold.client = None;
            }
        });
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spec_survives_its_file() {
        let spec = Spec {
            program: "/bin/sh".into(),
            args: vec!["-l".into(), "-c".into(), "echo 'a=b'\nexit".into()],
            cwd: "/tmp/with space".into(),
            env: vec![
                ("PS1".into(), "$ ".into()),
                ("NOTE".into(), "x=y\\z".into()),
            ],
            unset: vec!["CLAUDECODE".into()],
            cols: 120,
            rows: 40,
            title: "✻ Claude".into(),
            claude: true,
            review: "/tmp/sessions/abc".into(),
            project: "/tmp/proj".into(),
        };
        assert_eq!(Spec::parse(&spec.to_text()), spec);
        let left = Left {
            events_read: 1234,
            active: true,
        };
        assert_eq!(Left::parse(&left.to_text()), left);
    }

    #[test]
    fn frames_come_back_as_sent() {
        let frames = [
            Frame::Size(80, 24),
            Frame::Output(b"hello\x1b[31m".to_vec()),
            Frame::Exited(-1),
            Frame::Input(Vec::new()),
            Frame::Resize(200, 50),
            Frame::Kill,
        ];
        let mut bytes = Vec::new();
        for frame in &frames {
            send(&mut bytes, frame).unwrap();
        }
        let mut from = bytes.as_slice();
        for frame in &frames {
            assert_eq!(&receive(&mut from).unwrap(), frame);
        }
        assert!(receive(&mut from).is_err(), "EOF is an error");
    }

    #[test]
    fn kept_output_is_the_tail() {
        let mut hold = Hold {
            client: None,
            clients: 0,
            kept: VecDeque::new(),
            cols: 1,
            rows: 1,
        };
        hold.keep(&vec![1u8; KEEP - 10]);
        hold.keep(&[2u8; 20]);
        assert_eq!(hold.kept.len(), KEEP);
        assert_eq!(hold.kept.front(), Some(&1));
        assert_eq!(hold.kept.back(), Some(&2));
        hold.keep(&vec![3u8; KEEP + 5]);
        assert_eq!(hold.kept.len(), KEEP);
        assert!(hold.kept.iter().all(|b| *b == 3));
    }
}
