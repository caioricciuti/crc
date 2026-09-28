//! JSON-RPC over a child process's stdio, framed with `Content-Length`.
//!
//! A reader thread turns the byte stream into messages and hands them over
//! a channel; after each one it calls the wake-up the owner gave it, which
//! in the app queues a main-thread poll. Writes go through a writer thread,
//! so a server that stops reading never blocks the caller. The process is
//! killed when the transport is dropped, so a server never outlives the
//! editor.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;

use crate::json::{self, Value};

/// What the reader thread delivers.
#[derive(Debug)]
pub enum Incoming {
    Message(Value),
    /// The stream ended: the server exited or closed its output.
    Closed,
}

/// Called after a message is queued, from the reader thread.
pub use crate::platform::dispatch::Wake;

pub struct Transport {
    /// `None` only while being dropped.
    child: Option<Child>,
    /// Frames for the writer thread; `None` once it has stopped.
    writer: Option<mpsc::Sender<Vec<u8>>>,
    /// Set by the writer thread when a write failed: the server is gone.
    broken: std::sync::Arc<std::sync::atomic::AtomicBool>,
    rx: mpsc::Receiver<Incoming>,
    /// The last lines the server wrote to stderr, for the error shown when
    /// it dies.
    stderr_tail: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl Transport {
    /// Starts `program args` in `root` and begins reading its replies.
    pub fn spawn(
        program: &Path,
        args: &[&str],
        root: &Path,
        wake: Wake,
    ) -> std::io::Result<Transport> {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("no stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| std::io::Error::other("no stderr"))?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_message(&mut reader) {
                    Ok(Some(Ok(value))) => {
                        if tx.send(Incoming::Message(value)).is_err() {
                            break;
                        }
                        wake();
                    }
                    // One message the parser refused is one lost message,
                    // not the end of the server.
                    Ok(Some(Err(_))) => {}
                    Ok(None) | Err(_) => {
                        let _ = tx.send(Incoming::Closed);
                        wake();
                        break;
                    }
                }
            }
        });
        let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let tail = stderr_tail.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut tail = tail.lock().unwrap_or_else(|e| e.into_inner());
                tail.push(line);
                if tail.len() > 20 {
                    tail.remove(0);
                }
            }
        });
        let broken = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (writer, frames) = mpsc::channel::<Vec<u8>>();
        let failed = broken.clone();
        std::thread::spawn(move || write_frames(stdin, frames, &failed));
        Ok(Transport {
            child: Some(child),
            writer: Some(writer),
            broken,
            rx,
            stderr_tail,
        })
    }

    /// Queues one message for the writer thread. A failure means the
    /// server is gone: an earlier write failed, or the thread stopped.
    pub fn send(&mut self, message: &Value) -> std::io::Result<()> {
        let gone = || std::io::Error::new(std::io::ErrorKind::BrokenPipe, "server is gone");
        if self.broken.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(gone());
        }
        let body = json::compact(message);
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend_from_slice(body.as_bytes());
        self.writer
            .as_ref()
            .ok_or_else(gone)?
            .send(frame)
            .map_err(|_| gone())
    }

    pub fn try_recv(&self) -> Option<Incoming> {
        self.rx.try_recv().ok()
    }

    /// Blocks up to `timeout` for the next message. For tests.
    pub fn recv_timeout(&self, timeout: std::time::Duration) -> Option<Incoming> {
        self.rx.recv_timeout(timeout).ok()
    }

    pub fn stderr_tail(&self) -> String {
        self.stderr_tail
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .join("\n")
    }

    pub fn is_running(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|c| matches!(c.try_wait(), Ok(None)))
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        // Closing the writer ends its thread once what was queued is out;
        // the child is killed now and reaped on a thread of its own, so
        // closing a project never waits on a server's exit.
        self.writer = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

/// Writes frames to the server in order until the channel closes or a
/// write fails.
fn write_frames(
    mut stdin: ChildStdin,
    frames: mpsc::Receiver<Vec<u8>>,
    broken: &std::sync::atomic::AtomicBool,
) {
    for frame in frames {
        if stdin
            .write_all(&frame)
            .and_then(|()| stdin.flush())
            .is_err()
        {
            broken.store(true, std::sync::atomic::Ordering::Relaxed);
            return;
        }
    }
}

/// One framed message, `None` at end of stream. A message whose body is not
/// JSON, or is too large to take, comes back as `Some(Err(..))`, and the
/// stream carries on after it. Lines before a header (a wrapper's banner, a
/// runtime's warning on stdout) are skipped.
/// The most header bytes one message may carry. Real ones are a line or
/// two; the cap keeps a stream that never ends a line from filling memory.
const MAX_HEADER_BYTES: u64 = 64 * 1024;

fn read_message<R: BufRead>(reader: &mut R) -> std::io::Result<Option<Result<Value, String>>> {
    let mut length: Option<usize> = None;
    let mut line = String::new();
    let mut header_bytes = 0u64;
    loop {
        line.clear();
        let n = reader
            .by_ref()
            .take(MAX_HEADER_BYTES - header_bytes)
            .read_line(&mut line)?;
        if n == 0 {
            if header_bytes >= MAX_HEADER_BYTES {
                return Err(std::io::Error::other("message headers too long"));
            }
            return Ok(None);
        }
        header_bytes += n as u64;
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if length.is_some() {
                break;
            }
            continue;
        }
        if let Some(value) = trimmed
            .split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.trim())
        {
            length = value.parse().ok();
        }
    }
    let Some(length) = length else {
        return Ok(Some(Err("frame without Content-Length".into())));
    };
    if length > 64 * 1024 * 1024 {
        // Read past it, so the next header is where the stream resumes.
        let mut rest = <&mut R as std::io::Read>::take(reader, length as u64);
        std::io::copy(&mut rest, &mut std::io::sink())?;
        return Ok(Some(Err("frame too large".into())));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    let text = String::from_utf8_lossy(&body);
    Ok(Some(
        json::parse(&text).map_err(|e| format!("bad JSON from server: {e}")),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_read_by_content_length_and_case_insensitively() {
        let stream =
            b"content-length: 13\r\nX-Other: 1\r\n\r\n{\"a\":[1,2,3]}Content-Length: 4\r\n\r\nnull";
        let mut reader = BufReader::new(&stream[..]);
        assert_eq!(
            read_message(&mut reader).unwrap(),
            Some(Ok(json::parse("{\"a\":[1,2,3]}").unwrap()))
        );
        assert_eq!(read_message(&mut reader).unwrap(), Some(Ok(Value::Null)));
        assert_eq!(read_message(&mut reader).unwrap(), None);
    }

    #[test]
    fn a_banner_or_a_bad_body_does_not_end_the_stream() {
        let stream =
            b"Starting server v1\n\nContent-Length: 3\r\n\r\n{x}Content-Length: 4\r\n\r\ntrue";
        let mut reader = BufReader::new(&stream[..]);
        assert!(matches!(read_message(&mut reader), Ok(Some(Err(_)))));
        assert_eq!(
            read_message(&mut reader).unwrap(),
            Some(Ok(Value::Bool(true)))
        );
        assert_eq!(read_message(&mut reader).unwrap(), None);
    }

    #[test]
    fn a_header_that_never_ends_is_refused_not_held() {
        let mut stream = b"Content-Length: 4\r\nX-Junk: ".to_vec();
        stream.extend(std::iter::repeat_n(b'x', 100 * 1024));
        let mut reader = BufReader::new(&stream[..]);
        assert!(read_message(&mut reader).is_err());
        // A large header set that does end is still read.
        let mut fine = b"Content-Length: 4\r\n".to_vec();
        for _ in 0..100 {
            fine.extend_from_slice(b"X-Pad: 0123456789\r\n");
        }
        fine.extend_from_slice(b"\r\nnull");
        let mut reader = BufReader::new(&fine[..]);
        assert_eq!(read_message(&mut reader).unwrap(), Some(Ok(Value::Null)));
    }
}
