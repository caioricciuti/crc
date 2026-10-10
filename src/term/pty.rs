//! A program on a pseudo-terminal, and the thread that reads it.
//!
//! The terminal pair comes from libSystem (`posix_openpt`), the child gets
//! the slave side as its controlling terminal through `setsid` and
//! `TIOCSCTTY`, and the master side is read on a thread that feeds a shared
//! [`Term`]. Parsing happens there, off the main thread, so a program that
//! prints a lot costs the editor a mutex and a redraw, not the parse. Replies
//! the terminal owes (cursor reports and the like) go straight back from
//! that thread. Wake-ups to the main thread are coalesced: one is queued at
//! a time, however much arrives.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::Term;

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
    /// Input for the writer thread: a program that stops reading (a paste
    /// into one that is busy) blocks that thread, never the editor.
    input: std::sync::mpsc::Sender<Vec<u8>>,
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
    /// A `crc --hold` helper (see [`super::hold`]): frames go over its
    /// socket, under one lock so they never interleave. `detached` once the
    /// window has let go of it without ending it.
    Held {
        socket: Arc<Mutex<UnixStream>>,
        detached: bool,
    },
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
}

impl Status {
    /// Queues a wake-up. True when none was queued, so the caller wakes
    /// the window; one queued is enough.
    fn queue_wake(&self) -> bool {
        !self.woken.swap(true, Ordering::AcqRel)
    }
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
        let mut replies = master.try_clone()?;
        let term = Arc::new(Mutex::new(Term::new(cols, rows)));
        let (input, typed) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            for bytes in typed {
                if writer.write_all(&bytes).is_err() {
                    return;
                }
            }
        });
        let (shared, seen) = (term.clone(), status.clone());
        std::thread::spawn(move || {
            let mut reader = master;
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                // A closed slave reads as 0 or as EIO, depending on timing.
                let n = match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let reply = {
                    let mut term = shared.lock().unwrap_or_else(|e| e.into_inner());
                    term.advance(&buffer[..n]);
                    term.take_replies()
                };
                if !reply.is_empty() {
                    let _ = replies.write_all(&reply);
                }
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
            input,
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
        use super::hold::{self, Frame};
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
        let socket = Arc::new(Mutex::new(socket));
        let (input, typed) = std::sync::mpsc::channel::<Vec<u8>>();
        {
            let socket = socket.clone();
            std::thread::spawn(move || {
                for bytes in typed {
                    let mut socket = socket.lock().unwrap_or_else(|e| e.into_inner());
                    if hold::send(&mut *socket, &Frame::Input(bytes)).is_err() {
                        return;
                    }
                }
            });
        }
        let (shared, seen, replies) = (term.clone(), status.clone(), input.clone());
        std::thread::spawn(move || {
            loop {
                match hold::receive(&mut reader) {
                    Ok(Frame::Output(bytes)) => {
                        let reply = {
                            let mut term = shared.lock().unwrap_or_else(|e| e.into_inner());
                            term.advance(&bytes);
                            term.take_replies()
                        };
                        if !reply.is_empty() {
                            let _ = replies.send(reply);
                        }
                        if seen.queue_wake() {
                            wake();
                        }
                    }
                    Ok(Frame::Exited(code)) => {
                        seen.code.store(code, Ordering::Release);
                        seen.reaped.store(true, Ordering::Release);
                        break;
                    }
                    // Size changes come only on attach; anything else, or
                    // a helper gone, ends the session.
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            seen.exited.store(true, Ordering::Release);
            seen.woken.store(true, Ordering::Release);
            wake();
        });
        Ok(Session {
            term,
            input,
            control: Control::Held {
                socket,
                detached: false,
            },
            status,
        })
    }

    /// Sends input, as typed or pasted, through the writer thread.
    pub fn write(&mut self, bytes: &[u8]) {
        let _ = self.input.send(bytes.to_vec());
    }

    /// Changes the terminal's size; the program hears SIGWINCH.
    pub fn resize(&self, cols: usize, rows: usize) {
        let mut term = self.term.lock().unwrap_or_else(|e| e.into_inner());
        if (term.cols(), term.rows()) != (cols.max(2), rows.max(1)) {
            term.resize(cols, rows);
            match &self.control {
                Control::Pty { master, .. } => set_size(master.as_raw_fd(), cols, rows),
                Control::Held { socket, .. } => {
                    let mut socket = socket.lock().unwrap_or_else(|e| e.into_inner());
                    let _ =
                        super::hold::send(&mut *socket, &super::hold::Frame::Resize(cols, rows));
                }
            }
        }
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
            Control::Held { socket, .. } => {
                let mut socket = socket.lock().unwrap_or_else(|e| e.into_inner());
                let _ = super::hold::send(&mut *socket, &super::hold::Frame::Kill);
            }
        }
    }

    pub fn has_exited(&self) -> bool {
        self.status.exited.load(Ordering::Acquire)
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
        if let Control::Held { detached: true, .. } = self.control {
            return;
        }
        self.end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_for(session: &Session, what: impl Fn(&Term) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if what(&session.term.lock().unwrap()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
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
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session.has_exited() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(session.has_exited());
    }
}
