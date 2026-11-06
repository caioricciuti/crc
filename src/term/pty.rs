//! A program on a pseudo-terminal, and the thread that reads it.
//!
//! The terminal pair comes from libSystem (`posix_openpt`), the child gets
//! the slave side as its controlling terminal through `setsid` and
//! `TIOCSCTTY`, and the master side is read on a thread that feeds a shared
//! [`Term`]. Parsing happens there, off the main thread, so a program that
//! prints a lot costs the editor a mutex and a redraw, not the parse. Replies
//! the terminal owes (cursor reports and the like) go to the writer thread,
//! behind what was typed, so the reader never waits on a program that has
//! stopped reading. Wake-ups to the main thread are coalesced: one is queued
//! at a time, however much arrives.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use super::Term;
use super::hold::{self, Frame};

/// Keep a busy session from holding up the UI's status and screen reads
/// for a whole 64 KiB burst (or a 2 MiB held-session replay). Parser state
/// survives a slice ending inside UTF-8 or a control sequence.
const OUTPUT_SLICE: usize = 4096;

/// How much may wait for the writer thread before the replies the terminal
/// owes are dropped instead of queued: a program that floods queries and
/// never reads its input would otherwise grow the queue without bound.
const REPLY_LIMIT: usize = 64 * 1024;

fn advance_output(term: &Mutex<Term>, bytes: &[u8], mut reply: impl FnMut(Vec<u8>)) {
    for slice in bytes.chunks(OUTPUT_SLICE) {
        let replies = {
            let mut term = term.lock().unwrap_or_else(|e| e.into_inner());
            term.advance(slice);
            term.take_replies()
        };
        // The UI can acquire the terminal between slices; writes back to
        // the program must not extend the time spent holding its lock.
        reply(replies);
    }
}

/// The next read of a program's output: how much came, or `None` once its
/// side closed, which a closed slave reports as 0 or as EIO depending on
/// timing. A read a signal interrupted is tried again, not taken as the
/// end.
pub(super) fn read_output(reader: &mut impl Read, buffer: &mut [u8]) -> Option<usize> {
    loop {
        match reader.read(buffer) {
            Ok(0) => return None,
            Ok(n) => return Some(n),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
}

unsafe extern "C" {
    fn posix_openpt(flags: i32) -> i32;
    fn grantpt(fd: i32) -> i32;
    fn unlockpt(fd: i32) -> i32;
    fn ptsname_r(fd: i32, buffer: *mut std::ffi::c_char, length: usize) -> i32;
    fn ioctl(fd: i32, request: u64, ...) -> i32;
    fn fcntl(fd: i32, command: i32, ...) -> i32;
    fn setsid() -> i32;
}

const O_RDWR: i32 = 0x2;
const O_NOCTTY: i32 = 0x20000;
const F_SETFD: i32 = 2;
const FD_CLOEXEC: i32 = 1;
const TIOCSCTTY: u64 = 0x2000_7461;
const TIOCSWINSZ: u64 = 0x8008_7467;
const SIGHUP: i32 = 1;

#[repr(C)]
struct WinSize {
    rows: u16,
    cols: u16,
    x_pixels: u16,
    y_pixels: u16,
}

fn check(result: i32, what: &str) -> std::io::Result<i32> {
    if result < 0 {
        let error = std::io::Error::last_os_error();
        return Err(std::io::Error::new(
            error.kind(),
            format!("{what}: {error}"),
        ));
    }
    Ok(result)
}

fn set_size(fd: i32, cols: usize, rows: usize) {
    let size = WinSize {
        rows: rows.min(u16::MAX as usize) as u16,
        cols: cols.min(u16::MAX as usize) as u16,
        x_pixels: 0,
        y_pixels: 0,
    };
    unsafe { ioctl(fd, TIOCSWINSZ, &size as *const WinSize) };
}

/// Called from the reader thread when there is something new to draw.
pub use crate::platform::dispatch::Wake;

/// A new terminal pair of `cols` by `rows`: the master, and the slave side
/// opened for the program.
pub(crate) fn open_pair(cols: usize, rows: usize) -> std::io::Result<(OwnedFd, File)> {
    let master = check(unsafe { posix_openpt(O_RDWR | O_NOCTTY) }, "posix_openpt")?;
    // Owned at once, so every early return closes it.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let fd = master.as_raw_fd();
    check(unsafe { fcntl(fd, F_SETFD, FD_CLOEXEC) }, "fcntl")?;
    check(unsafe { grantpt(fd) }, "grantpt")?;
    check(unsafe { unlockpt(fd) }, "unlockpt")?;
    let mut name = [0 as std::ffi::c_char; 128];
    check(
        unsafe { ptsname_r(fd, name.as_mut_ptr(), name.len()) },
        "ptsname",
    )?;
    let name = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_NOCTTY)
        .open(&name)?;
    // On the slave: set on the master before the slave is open, the
    // size does not stick on macOS and the program reads 0 by 0.
    set_size(slave.as_raw_fd(), cols, rows);
    Ok((master, slave))
}

/// Starts `program args` in `cwd` on `slave`, as its controlling terminal,
/// with `env` added to this process's environment and `unset` taken out.
pub(crate) fn start_on(
    slave: File,
    program: &Path,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    unset: &[&str],
) -> std::io::Result<std::process::Child> {
    let mut command = Command::new(program);
    for name in unset {
        command.env_remove(name);
    }
    command
        .args(args)
        .current_dir(cwd)
        .envs(env.iter().copied())
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    // In the child, between fork and exec: a session of its own, with
    // the terminal on fd 0 as its controlling terminal, which is what
    // makes Ctrl-C a signal and job control work.
    unsafe {
        command.pre_exec(|| {
            if setsid() < 0 || ioctl(0, TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()
}

/// Sets the window size of a terminal by its master.
pub(crate) fn resize_master(master: &File, cols: usize, rows: usize) {
    set_size(master.as_raw_fd(), cols, rows);
}

/// A running program and its screen.
pub struct Session {
    pub term: Arc<Mutex<Term>>,
    /// The way to the program, through the writer thread: a program that
    /// stops reading (a paste into one that is busy) blocks that thread,
    /// never the editor.
    out: Outgoing,
    /// The program's side: the terminal here, or a helper holding it.
    control: Control,
    /// What the reader and wait threads have seen.
    status: Arc<Status>,
}

/// Who holds the terminal.
enum Control {
    /// This process: the master, for the window-size ioctl, and the
    /// program's pid.
    Pty { master: File, pid: i32 },
    /// A `crc --hold` helper (see [`super::hold`]): every frame to it goes
    /// the way of the input, so only the writer thread touches its socket.
    /// `detached` once the window has let go of it without ending it.
    Held { detached: bool },
}

/// What goes to the program, in order, through the writer thread: typed
/// and pasted input, the replies the terminal owes, and for a held program
/// its size changes and its end. Counts the bytes of input the writer has
/// not written yet, so a reply can be dropped when too much is waiting;
/// what was typed always goes.
#[derive(Clone)]
struct Outgoing {
    frames: Sender<Frame>,
    queued: Arc<AtomicUsize>,
}

impl Outgoing {
    fn new() -> (Outgoing, Receiver<Frame>) {
        let (frames, receiver) = std::sync::mpsc::channel();
        let out = Outgoing {
            frames,
            queued: Arc::new(AtomicUsize::new(0)),
        };
        (out, receiver)
    }

    fn send(&self, frame: Frame) {
        self.queued.fetch_add(input_bytes(&frame), Ordering::AcqRel);
        let _ = self.frames.send(frame);
    }

    /// Queues a reply the terminal owes, unless the writer is already
    /// [`REPLY_LIMIT`] behind: the program has stopped reading, and a
    /// late report is worth less than a bounded queue. Whether it went.
    fn reply(&self, bytes: Vec<u8>) -> bool {
        if bytes.is_empty() || self.queued.load(Ordering::Acquire) > REPLY_LIMIT {
            return false;
        }
        self.send(Frame::Input(bytes));
        true
    }
}

/// The bytes of input a frame carries.
fn input_bytes(frame: &Frame) -> usize {
    match frame {
        Frame::Input(bytes) => bytes.len(),
        _ => 0,
    }
}

/// The writer thread: hands each frame to `deliver`, in order, until the
/// session is gone or the program's side fails.
fn write_frames(
    frames: Receiver<Frame>,
    queued: Arc<AtomicUsize>,
    mut deliver: impl FnMut(&Frame) -> std::io::Result<()>,
) {
    for frame in frames {
        let delivered = deliver(&frame);
        queued.fetch_sub(input_bytes(&frame), Ordering::AcqRel);
        if delivered.is_err() {
            return;
        }
    }
}

/// Set by the reader thread and the wait thread, read by the session.
#[derive(Default)]
struct Status {
    /// Set when the program's side closed, or the program itself exited:
    /// a background job it left holding the terminal keeps the first from
    /// ever happening.
    exited: AtomicBool,
    /// The program has been waited for; its pid may belong to another
    /// process by now.
    reaped: AtomicBool,
    /// How the program ended, once reaped: its exit code, or -1 when a
    /// signal ended it.
    code: std::sync::atomic::AtomicI32,
    /// A wake-up is queued and not yet taken by [`Session::drain_wake`].
    woken: AtomicBool,
    /// How many times the terminal was given another size.
    resizes: AtomicUsize,
}

impl Status {
    /// Queues a wake-up. True when none was queued, so the caller wakes
    /// the window; one queued is enough.
    fn queue_wake(&self) -> bool {
        !self.woken.swap(true, Ordering::AcqRel)
    }
}

/// The reader thread of a held session: the helper's frames, from after
/// its size, into the screen, until the program exits or the helper is
/// gone. The first output frame is the helper's replay of what the program
/// printed before this window: the queries in it were answered when they
/// were asked, so its replies are dropped rather than sent again.
fn relay_held(
    reader: &mut impl Read,
    term: &Mutex<Term>,
    status: &Status,
    out: &Outgoing,
    wake: &dyn Fn(),
) {
    let mut replay = true;
    loop {
        match hold::receive(reader) {
            Ok(Frame::Output(bytes)) => {
                if replay {
                    advance_output(term, &bytes, |_| {});
                    replay = false;
                } else {
                    advance_output(term, &bytes, |reply| {
                        out.reply(reply);
                    });
                }
                if status.queue_wake() {
                    wake();
                }
            }
            Ok(Frame::Exited(code)) => {
                status.code.store(code, Ordering::Release);
                status.reaped.store(true, Ordering::Release);
                break;
            }
            // Size changes come only on attach; anything else, or a
            // helper gone, ends the session.
            Ok(_) => {}
            Err(_) => break,
        }
    }
    status.exited.store(true, Ordering::Release);
    status.woken.store(true, Ordering::Release);
    wake();
}

impl Session {
    /// Starts `program args` in `cwd` on a new terminal of `cols` by `rows`,
    /// with `env` added to the editor's own environment and `unset` taken
    /// out of it.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        program: &Path,
        args: &[&str],
        cwd: &Path,
        env: &[(&str, &str)],
        unset: &[&str],
        cols: usize,
        rows: usize,
        wake: Wake,
    ) -> std::io::Result<Session> {
        let (master, slave) = open_pair(cols, rows)?;
        let child = start_on(slave, program, args, cwd, env, unset)?;
        let pid = child.id() as i32;
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::from(wake);
        let status = Arc::new(Status::default());
        // Reaped on a thread of its own, so it never lingers as a zombie.
        {
            let (status, wake) = (status.clone(), wake.clone());
            std::thread::spawn(move || {
                let mut child = child;
                let ended = child.wait();
                let code = ended.ok().and_then(|s| s.code()).unwrap_or(-1);
                status.code.store(code, Ordering::Release);
                status.reaped.store(true, Ordering::Release);
                status.exited.store(true, Ordering::Release);
                if status.queue_wake() {
                    wake();
                }
            });
        }

        let master = File::from(master);
        let mut writer = master.try_clone()?;
        let control = master.try_clone()?;
        let term = Arc::new(Mutex::new(Term::new(cols, rows)));
        let (out, frames) = Outgoing::new();
        {
            let queued = out.queued.clone();
            std::thread::spawn(move || {
                write_frames(frames, queued, |frame| match frame {
                    Frame::Input(bytes) => writer.write_all(bytes),
                    _ => Ok(()),
                })
            });
        }
        let (shared, seen, replies) = (term.clone(), status.clone(), out.clone());
        std::thread::spawn(move || {
            let mut reader = master;
            let mut buffer = vec![0u8; 64 * 1024];
            while let Some(n) = read_output(&mut reader, &mut buffer) {
                advance_output(&shared, &buffer[..n], |reply| {
                    replies.reply(reply);
                });
                if seen.queue_wake() {
                    wake();
                }
            }
            seen.exited.store(true, Ordering::Release);
            seen.woken.store(true, Ordering::Release);
            wake();
        });
        Ok(Session {
            term,
            out,
            control: Control::Pty {
                master: control,
                pid,
            },
            status,
        })
    }

    /// A program a `crc --hold` helper runs, over its socket: the screen is
    /// rebuilt from the output the helper kept, at the size it has there.
    /// The window resizes it to its grid as it does any session.
    pub fn attach(mut socket: UnixStream, wake: Wake) -> std::io::Result<Session> {
        // The helper says its size first, before any output.
        let (cols, rows) = match hold::receive(&mut socket)? {
            Frame::Size(cols, rows) => (cols, rows),
            _ => {
                return Err(std::io::Error::other(
                    "the session helper did not say its size",
                ));
            }
        };
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::from(wake);
        let status = Arc::new(Status::default());
        let term = Arc::new(Mutex::new(Term::new(cols, rows)));
        let mut reader = socket.try_clone()?;
        let (out, frames) = Outgoing::new();
        {
            let queued = out.queued.clone();
            let mut socket = socket;
            std::thread::spawn(move || {
                write_frames(frames, queued, |frame| hold::send(&mut socket, frame))
            });
        }
        let (shared, seen, replies) = (term.clone(), status.clone(), out.clone());
        std::thread::spawn(move || relay_held(&mut reader, &shared, &seen, &replies, &*wake));
        Ok(Session {
            term,
            out,
            control: Control::Held { detached: false },
            status,
        })
    }

    /// A session with no program behind it, for the benchmarks: output
    /// comes in through [`Session::feed`], input goes nowhere, and dropping
    /// it ends nothing.
    #[doc(hidden)]
    pub fn offline(cols: usize, rows: usize) -> std::io::Result<Session> {
        Ok(Session {
            term: Arc::new(Mutex::new(Term::new(cols, rows))),
            out: Outgoing::new().0,
            control: Control::Held { detached: true },
            status: Arc::new(Status::default()),
        })
    }

    /// Output, as the reader thread takes it from the program: the screen
    /// advanced and a wake-up queued. Returns whether the reader would have
    /// woken the window (none was queued yet).
    #[doc(hidden)]
    pub fn feed(&self, bytes: &[u8]) -> bool {
        advance_output(&self.term, bytes, |_| {});
        self.status.queue_wake()
    }

    /// Sends input, as typed or pasted, through the writer thread.
    pub fn write(&mut self, bytes: &[u8]) {
        self.out.send(Frame::Input(bytes.to_vec()));
    }

    /// Changes the terminal's size; the program hears SIGWINCH. A held
    /// program hears it after the input queued before, from the writer
    /// thread: this never waits on the helper.
    pub fn resize(&self, cols: usize, rows: usize) {
        let mut term = self.term.lock().unwrap_or_else(|e| e.into_inner());
        if (term.cols(), term.rows()) != (cols.max(2), rows.max(1)) {
            term.resize(cols, rows);
            self.status.resizes.fetch_add(1, Ordering::Relaxed);
            match &self.control {
                Control::Pty { master, .. } => set_size(master.as_raw_fd(), cols, rows),
                Control::Held { .. } => self.out.send(Frame::Resize(cols, rows)),
            }
        }
    }

    /// How many times [`Session::resize`] gave the terminal another size,
    /// and so told its program: a size it already has does not count.
    pub fn resizes(&self) -> usize {
        self.status.resizes.load(Ordering::Relaxed)
    }

    /// Whether a helper holds the program, so it can outlive the window.
    pub fn is_held(&self) -> bool {
        matches!(self.control, Control::Held { .. })
    }

    /// Lets go of a held program without ending it: the helper keeps it
    /// for the next window. Nothing for a program run here.
    pub fn detach(&mut self) {
        if let Control::Held { detached, .. } = &mut self.control {
            *detached = true;
        }
    }

    /// Ends the program now, as closing its tab does, whoever holds it.
    pub fn end(&self) {
        match &self.control {
            Control::Pty { pid, .. } => {
                if !self.status.reaped.load(Ordering::Acquire) {
                    crate::platform::send_signal(-pid, SIGHUP);
                }
            }
            Control::Held { .. } => self.out.send(Frame::Kill),
        }
    }

    pub fn has_exited(&self) -> bool {
        self.status.exited.load(Ordering::Acquire)
    }

    /// The program's pid when this process runs it and it has not been
    /// reaped; a held session's is the helper's child (see
    /// [`super::hold::program_pid`]).
    pub fn program_pid(&self) -> Option<i32> {
        match &self.control {
            Control::Pty { pid, .. } if !self.status.reaped.load(Ordering::Acquire) => Some(*pid),
            _ => None,
        }
    }

    /// How the program ended, once it has been reaped: its exit code, or
    /// `None` when a signal ended it or it is still running.
    pub fn exit_code(&self) -> Option<i32> {
        if !self.status.reaped.load(Ordering::Acquire) {
            return None;
        }
        Some(self.status.code.load(Ordering::Acquire)).filter(|c| *c >= 0)
    }

    /// Whether the reader asked for a redraw since the last call.
    pub fn drain_wake(&self) -> bool {
        self.status.woken.swap(false, Ordering::AcqRel)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // The whole process group, as closing a terminal window does; not
        // once the program is gone, since its pid may have been reused. A
        // background job it left behind keeps the terminal until it closes.
        // A held program the window let go of stays with its helper.
        if let Control::Held { detached: true } = self.control {
            return;
        }
        self.end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn output_slices_release_the_terminal_and_preserve_parser_state() {
        let mut output = vec![b'x'; OUTPUT_SLICE - 1];
        // A multibyte character, CSI and OSC each cross a slice boundary.
        output.extend_from_slice("\u{1f980}".as_bytes());
        output.resize(OUTPUT_SLICE * 2 - 1, b'y');
        output.extend_from_slice(b"\x1b[31mred\x1b[0m\r\n");
        output.resize(OUTPUT_SLICE * 3 - 3, b'z');
        output.extend_from_slice(b"\x1b]2;Personal sample\x1b\\\x1b[6n\x1b]9;Done\x07");
        output.extend_from_slice(b"\x1b]52;c;Y3Jj\x07\x1b[5n");

        let mut expected = Term::new(120, 40);
        expected.advance(&output);
        let terminal = Mutex::new(Term::new(120, 40));
        let mut replies = Vec::new();
        let mut read_before_title = false;
        advance_output(&terminal, &output, |reply| {
            let view = terminal.try_lock().expect("UI can read between slices");
            read_before_title |= view.title.is_empty();
            replies.extend(reply);
        });
        assert!(read_before_title, "UI can read before the burst finishes");
        let mut actual = terminal.lock().unwrap();
        assert_eq!(actual.screen_text(), expected.screen_text());
        assert_eq!(actual.cursor(), expected.cursor());
        assert_eq!(actual.title, expected.title);
        assert_eq!(actual.scrollback_len(), expected.scrollback_len());
        for row in 0..actual.rows() {
            assert_eq!(actual.visible_row(row, 0), expected.visible_row(row, 0));
        }
        assert_eq!(replies, expected.take_replies());
        assert!(!replies.is_empty());
        assert_eq!(actual.take_notices(), expected.take_notices());
        assert_eq!(actual.take_clipboard(), expected.take_clipboard());
    }

    /// A reader that is interrupted once, then has `data`, then ends.
    struct Interrupted<'a> {
        data: &'a [u8],
        interrupted: bool,
    }

    impl Read for Interrupted<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            let n = self.data.len().min(buffer.len());
            buffer[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    #[test]
    fn an_interrupted_read_is_not_the_end_of_the_output() {
        let mut reader = Interrupted {
            data: b"after",
            interrupted: false,
        };
        let mut buffer = [0u8; 16];
        assert_eq!(read_output(&mut reader, &mut buffer), Some(5));
        assert_eq!(&buffer[..5], b"after");
        assert_eq!(read_output(&mut reader, &mut buffer), None, "EOF ends it");
    }

    #[test]
    fn replies_are_bounded_when_the_program_stops_reading() {
        // Nobody takes the writer's end: the queue only ever grows.
        let (out, unread) = Outgoing::new();
        let queries = b"\x1b[5n".repeat(200_000);
        let terminal = Mutex::new(Term::new(80, 24));
        let (mut sent, mut dropped) = (0usize, 0usize);
        advance_output(&terminal, &queries, |reply| {
            if out.reply(reply) {
                sent += 1;
            } else {
                dropped += 1;
            }
        });
        assert!(dropped > 0, "{sent} sent, none dropped");
        let queued = out.queued.load(Ordering::Acquire);
        // At most one slice's worth past the limit.
        assert!(
            queued <= REPLY_LIMIT + OUTPUT_SLICE * 4,
            "{queued} bytes queued"
        );
        assert_eq!(unread.try_iter().count(), sent);
    }

    #[test]
    fn the_replay_on_attach_is_not_answered_but_live_output_is() {
        let queries = b"\x1b[6n\x1b[c".to_vec();
        let mut frames = Vec::new();
        hold::send(&mut frames, &Frame::Output(queries.clone())).unwrap();
        hold::send(&mut frames, &Frame::Output(queries)).unwrap();
        let (out, sent) = Outgoing::new();
        let terminal = Mutex::new(Term::new(80, 24));
        let status = Status::default();
        relay_held(&mut frames.as_slice(), &terminal, &status, &out, &|| {});
        let answers: Vec<Frame> = sent.try_iter().collect();
        assert_eq!(answers.len(), 1, "{answers:?}");
        let Frame::Input(reply) = &answers[0] else {
            panic!("{answers:?}");
        };
        let reply = String::from_utf8_lossy(reply);
        assert!(reply.contains("\x1b[1;1R"), "{reply:?}");
        assert!(reply.ends_with('c'), "{reply:?}");
        assert!(
            status.exited.load(Ordering::Acquire),
            "EOF ends the session"
        );
    }

    fn until(timeout: Duration, done: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        done()
    }

    #[test]
    fn resize_and_end_of_a_held_session_never_wait_on_the_helper() {
        let (ours, mut helper) = UnixStream::pair().unwrap();
        hold::send(&mut helper, &Frame::Size(80, 24)).unwrap();
        let mut session = Session::attach(ours, Box::new(|| {})).unwrap();
        // More than the socket takes while the other end reads nothing:
        // the writer thread is stuck on it once a few frames are in.
        for _ in 0..256 {
            session.write(&[b'x'; 1024]);
        }
        assert!(until(Duration::from_secs(5), || {
            session.out.queued.load(Ordering::Acquire) < 256 * 1024
        }));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            session.resize(100, 30);
            session.end();
            tx.send(session).unwrap();
        });
        let session = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("resize and end return while the socket is full");
        assert!(session.is_held());
        // Drained, the helper hears everything in order: the input, then
        // the size, then the end.
        let mut seen = Vec::new();
        loop {
            match hold::receive(&mut helper).unwrap() {
                Frame::Input(_) => {}
                frame @ Frame::Kill => {
                    seen.push(frame);
                    break;
                }
                frame => seen.push(frame),
            }
        }
        assert_eq!(seen, [Frame::Resize(100, 30), Frame::Kill]);
    }

    fn wait_for(session: &Session, what: impl Fn(&Term) -> bool) -> bool {
        until(Duration::from_secs(5), || {
            what(&session.term.lock().unwrap())
        })
    }

    #[test]
    fn a_program_runs_on_a_real_terminal() {
        let mut session = Session::spawn(
            Path::new("/bin/sh"),
            &[],
            Path::new("/"),
            &[("PS1", "$ "), ("CRC_TERM_TEST", "hello")],
            &[],
            40,
            6,
            Box::new(|| {}),
        )
        .expect("spawns /bin/sh");
        // It is a terminal to the program, the size is the one given, and
        // the environment arrived.
        session.write(b"test -t 0 && stty size && echo $CRC_TERM_TEST-$PWD\n");
        assert!(
            wait_for(&session, |t| t.screen_text().contains("6 40")
                && t.screen_text().contains("hello-/")),
            "{}",
            session.term.lock().unwrap().screen_text()
        );
        session.resize(50, 8);
        session.write(b"stty size\n");
        assert!(wait_for(&session, |t| t.screen_text().contains("8 50")));
        assert!(session.drain_wake());
        session.write(b"exit\n");
        assert!(until(Duration::from_secs(5), || session.has_exited()));
    }
}
