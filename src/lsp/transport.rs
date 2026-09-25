//! JSON-RPC over a child process's stdio, framed with `Content-Length`.
//!
//! A reader thread turns the byte stream into messages and hands them over
//! a channel; after each one it calls the wake-up the owner gave it, which
//! in the app queues a main-thread poll. Writes happen on the caller's
//! thread. The process is killed when the transport is dropped, so a server
//! never outlives the editor.

use std::io::{BufRead, BufReader, Write};
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
pub type Wake = Box<dyn Fn() + Send + Sync>;

pub struct Transport {
    child: Child,
    stdin: ChildStdin,
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
                    Ok(Some(value)) => {
                        if tx.send(Incoming::Message(value)).is_err() {
                            break;
                        }
                        wake();
                    }
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
        Ok(Transport {
            child,
            stdin,
            rx,
            stderr_tail,
        })
    }

    /// Writes one message. A failure means the server is gone.
    pub fn send(&mut self, message: &Value) -> std::io::Result<()> {
        let body = json::compact(message);
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
        self.stdin.flush()
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
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One framed message, `None` at end of stream.
fn read_message<R: BufRead>(reader: &mut R) -> std::io::Result<Option<Value>> {
    let mut length: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if length.is_some() {
                break;
            }
            return Err(std::io::Error::other("frame without Content-Length"));
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
        return Err(std::io::Error::other("frame without Content-Length"));
    };
    if length > 64 * 1024 * 1024 {
        return Err(std::io::Error::other("frame too large"));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    let text = String::from_utf8_lossy(&body);
    json::parse(&text)
        .map(Some)
        .map_err(|e| std::io::Error::other(format!("bad JSON from server: {e}")))
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
            Some(json::parse("{\"a\":[1,2,3]}").unwrap())
        );
        assert_eq!(read_message(&mut reader).unwrap(), Some(Value::Null));
        assert_eq!(read_message(&mut reader).unwrap(), None);
        let mut bad = BufReader::new(&b"Foo: 1\r\n\r\n{}"[..]);
        assert!(read_message(&mut bad).is_err());
    }
}
