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
    fn kill(pid: i32, signal: i32) -> i32;
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
pub type Wake = Box<dyn Fn() + Send + Sync>;

/// A running program and its screen.
pub struct Session {
    pub term: Arc<Mutex<Term>>,
    writer: File,
    pid: i32,
    /// Set by the reader when the program's side closed.
    exited: Arc<AtomicBool>,
    /// A wake-up is queued and not yet taken by [`Session::drain_wake`].
    woken: Arc<AtomicBool>,
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
        let child = command.spawn()?;
        let pid = child.id() as i32;
        // Reaped on a thread of its own, so it never lingers as a zombie.
        std::thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
        });

        let master = File::from(master);
        let writer = master.try_clone()?;
        let mut replies = master.try_clone()?;
        let term = Arc::new(Mutex::new(Term::new(cols, rows)));
        let exited = Arc::new(AtomicBool::new(false));
        let woken = Arc::new(AtomicBool::new(false));
        let (shared, done, pending) = (term.clone(), exited.clone(), woken.clone());
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
                if !pending.swap(true, Ordering::AcqRel) {
                    wake();
                }
            }
            done.store(true, Ordering::Release);
            pending.store(true, Ordering::Release);
            wake();
        });
        Ok(Session {
            term,
            writer,
            pid,
            exited,
            woken,
        })
    }

    /// Sends input, as typed or pasted.
    pub fn write(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
    }

    /// Changes the terminal's size; the program hears SIGWINCH.
    pub fn resize(&self, cols: usize, rows: usize) {
        let mut term = self.term.lock().unwrap_or_else(|e| e.into_inner());
        if (term.cols(), term.rows()) != (cols.max(2), rows.max(1)) {
            term.resize(cols, rows);
            set_size(self.writer.as_raw_fd(), cols, rows);
        }
    }

    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }

    /// Whether the reader asked for a redraw since the last call.
    pub fn drain_wake(&self) -> bool {
        self.woken.swap(false, Ordering::AcqRel)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // The whole process group, as closing a terminal window does.
        unsafe { kill(-self.pid, SIGHUP) };
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
