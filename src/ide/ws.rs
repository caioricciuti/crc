//! A WebSocket server for one local client, RFC 6455 text frames only.
//!
//! Bound to 127.0.0.1 on a port the system picks. The upgrade must carry
//! the session token in `X-Claude-Code-Ide-Authorization`; anything else is
//! answered 401 and closed before it can send a frame. A second client that
//! authenticates replaces the first, as a restarted `claude` should.
//!
//! Threads: one accepts, one reads each connection. Messages reach the
//! owner over a channel, each followed by the wake-up it supplied, the same
//! arrangement as the language server transport. Writes come from the
//! owner's thread and from the reader answering pings, so the writer sits
//! behind a mutex and a frame is always written whole.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

/// Called after an event is queued, from a server thread.
pub type Wake = Box<dyn Fn() + Send + Sync>;

/// What the server threads deliver. `u64` is the connection, so a close
/// from a client that has since been replaced can be told apart.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Connected(u64),
    Text(u64, String),
    Closed(u64),
}

/// Largest message accepted, after reassembly. `openDiff` carries a whole
/// file, so this is generous.
const MAX_MESSAGE: usize = 64 * 1024 * 1024;
/// Largest upgrade request accepted.
const MAX_REQUEST: usize = 16 * 1024;

type Writer = Arc<Mutex<Option<(u64, TcpStream)>>>;

pub struct Server {
    port: u16,
    rx: mpsc::Receiver<Event>,
    writer: Writer,
    stop: Arc<AtomicBool>,
}

impl Server {
    /// Starts listening. `token` is what the client must present.
    pub fn start(token: String, wake: Wake) -> std::io::Result<Server> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let (tx, rx) = mpsc::channel();
        let writer: Writer = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let shared = writer.clone();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::from(wake);
        std::thread::spawn(move || {
            let next = AtomicU64::new(1);
            for stream in listener.incoming() {
                if stopped.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = stream else {
                    continue;
                };
                let id = next.fetch_add(1, Ordering::Relaxed);
                let (tx, shared, wake, token) =
                    (tx.clone(), shared.clone(), wake.clone(), token.clone());
                // The handshake reads from the socket, so it gets its own
                // thread: a client that connects and says nothing must not
                // hold up the next one.
                std::thread::spawn(move || serve(stream, id, &token, &shared, &tx, &*wake));
            }
        });
        Ok(Server {
            port,
            rx,
            writer,
            stop,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Sends a text message to the connected client. `false` when there is
    /// none or the write failed; a failed client is dropped.
    pub fn send(&self, text: &str) -> bool {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let Some((_, stream)) = writer.as_mut() else {
            return false;
        };
        if write_frame(stream, OP_TEXT, text.as_bytes()).is_ok() {
            return true;
        }
        if let Some((_, stream)) = writer.take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        false
    }

    pub fn is_connected(&self) -> bool {
        self.writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    pub fn try_recv(&self) -> Option<Event> {
        self.rx.try_recv().ok()
    }

    /// Blocks up to `timeout` for the next event. For tests.
    pub fn recv_timeout(&self, timeout: std::time::Duration) -> Option<Event> {
        self.rx.recv_timeout(timeout).ok()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // A connection of our own wakes the accept loop to see the flag.
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some((_, stream)) = self.writer.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

/// Upgrades one connection and reads it until it closes.
fn serve(
    stream: TcpStream,
    id: u64,
    token: &str,
    writer: &Writer,
    tx: &mpsc::Sender<Event>,
    wake: &(dyn Fn() + Send + Sync),
) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    let Ok(mut write_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let Ok(request) = read_request(&mut reader) else {
        return;
    };
    let reply = match handshake(&request, token) {
        Ok(reply) => reply,
        Err(status) => {
            let _ = write!(
                write_half,
                "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = write_half.shutdown(Shutdown::Both);
            return;
        }
    };
    if write_half.write_all(reply.as_bytes()).is_err() {
        return;
    }
    let _ = reader.get_ref().set_read_timeout(None);
    // Replace whoever was connected.
    {
        let mut current = writer.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, old)) = current.replace((id, write_half)) {
            let _ = old.shutdown(Shutdown::Both);
        }
    }
    if tx.send(Event::Connected(id)).is_err() {
        return;
    }
    wake();
    let mut message: Vec<u8> = Vec::new();
    let mut in_text = false;
    while let Ok(frame) = read_frame(&mut reader) {
        match frame.opcode {
            OP_TEXT | OP_CONTINUATION => {
                if frame.opcode == OP_TEXT {
                    message.clear();
                    in_text = true;
                } else if !in_text {
                    break;
                }
                if message.len() + frame.payload.len() > MAX_MESSAGE {
                    break;
                }
                message.extend_from_slice(&frame.payload);
                if frame.fin {
                    in_text = false;
                    let Ok(text) = String::from_utf8(std::mem::take(&mut message)) else {
                        break;
                    };
                    if tx.send(Event::Text(id, text)).is_err() {
                        return;
                    }
                    wake();
                }
            }
            OP_PING => {
                let mut current = writer.lock().unwrap_or_else(|e| e.into_inner());
                match current.as_mut() {
                    Some((owner, stream)) if *owner == id => {
                        if write_frame(stream, OP_PONG, &frame.payload).is_err() {
                            break;
                        }
                    }
                    _ => break,
                }
            }
            OP_PONG => {}
            // Close, binary (MCP never sends it) or anything unknown.
            _ => break,
        }
    }
    let mut current = writer.lock().unwrap_or_else(|e| e.into_inner());
    if current.as_ref().is_some_and(|(owner, _)| *owner == id) {
        if let Some((_, stream)) = current.take() {
            let _ = write_frame(&mut &stream, OP_CLOSE, &[]);
            let _ = stream.shutdown(Shutdown::Both);
        }
        drop(current);
        if tx.send(Event::Closed(id)).is_ok() {
            wake();
        }
    }
}

/// The request line and headers, lowercased names, up to the blank line.
struct Request {
    method: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

fn read_request<R: BufRead>(reader: &mut R) -> std::io::Result<Request> {
    let mut total = 0;
    let mut line = String::new();
    let mut read_line = |line: &mut String| -> std::io::Result<()> {
        line.clear();
        let n = reader.by_ref().take(MAX_REQUEST as u64).read_line(line)?;
        total += n;
        if n == 0 || total > MAX_REQUEST {
            return Err(std::io::Error::other("bad request"));
        }
        Ok(())
    };
    read_line(&mut line)?;
    let method = line.split(' ').next().unwrap_or("").to_owned();
    let mut headers = Vec::new();
    loop {
        read_line(&mut line)?;
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    Ok(Request { method, headers })
}

/// The 101 reply for a valid upgrade, or the status line to refuse with.
fn handshake(request: &Request, token: &str) -> Result<String, &'static str> {
    let presented = request
        .header("x-claude-code-ide-authorization")
        .unwrap_or("");
    if !same(presented.as_bytes(), token.as_bytes()) {
        return Err("401 Unauthorized");
    }
    let upgrade = request
        .header("upgrade")
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let Some(key) = request.header("sec-websocket-key").filter(|_| upgrade) else {
        return Err("400 Bad Request");
    };
    if request.method != "GET" {
        return Err("400 Bad Request");
    }
    let mut reply = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n",
        accept_key(key)
    );
    // Echo the first subprotocol offered; nothing requires one.
    if let Some(protocol) = request
        .header("sec-websocket-protocol")
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        reply.push_str(&format!("Sec-WebSocket-Protocol: {protocol}\r\n"));
    }
    reply.push_str("\r\n");
    Ok(reply)
}

/// Compares without stopping at the first difference, so the time taken
/// says nothing about how much of a guessed token was right.
fn same(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `Sec-WebSocket-Accept` for a client's key, RFC 6455 section 4.2.2.
fn accept_key(key: &str) -> String {
    const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    base64(&sha1(format!("{key}{GUID}").as_bytes()))
}

fn sha1(data: &[u8]) -> [u8; 20] {
    // CommonCrypto, part of libSystem.
    unsafe extern "C" {
        fn CC_SHA1(data: *const u8, len: u32, md: *mut u8) -> *mut u8;
    }
    let mut digest = [0u8; 20];
    unsafe { CC_SHA1(data.as_ptr(), data.len() as u32, digest.as_mut_ptr()) };
    digest
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A random token: 16 bytes from the system, as 32 lowercase hex digits.
pub fn new_token() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

const OP_CONTINUATION: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

struct Frame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

/// One frame from a client. Client frames must be masked (RFC 6455 5.1).
fn read_frame<R: Read>(reader: &mut R) -> std::io::Result<Frame> {
    let mut head = [0u8; 2];
    reader.read_exact(&mut head)?;
    let fin = head[0] & 0x80 != 0;
    let opcode = head[0] & 0x0F;
    if head[1] & 0x80 == 0 {
        return Err(std::io::Error::other("unmasked client frame"));
    }
    let len = match head[1] & 0x7F {
        126 => {
            let mut b = [0u8; 2];
            reader.read_exact(&mut b)?;
            u16::from_be_bytes(b) as u64
        }
        127 => {
            let mut b = [0u8; 8];
            reader.read_exact(&mut b)?;
            u64::from_be_bytes(b)
        }
        n => n as u64,
    };
    if len > MAX_MESSAGE as u64 || (opcode >= 0x8 && (len > 125 || !fin)) {
        return Err(std::io::Error::other("bad frame length"));
    }
    let mut mask = [0u8; 4];
    reader.read_exact(&mut mask)?;
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload)?;
    for (i, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[i % 4];
    }
    Ok(Frame {
        fin,
        opcode,
        payload,
    })
}

/// One unmasked, unfragmented frame, as a server sends.
fn write_frame<W: Write>(writer: &mut W, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut head = Vec::with_capacity(10);
    head.push(0x80 | opcode);
    match payload.len() {
        n if n < 126 => head.push(n as u8),
        n if n <= u16::MAX as usize => {
            head.push(126);
            head.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            head.push(127);
            head.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    writer.write_all(&head)?;
    writer.write_all(payload)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(5);

    #[test]
    fn accept_key_matches_the_rfc_example() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn tokens_are_32_hex_digits_and_differ() {
        let (a, b) = (new_token().unwrap(), new_token().unwrap());
        assert_eq!(a.len(), 32);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(a, b);
        assert!(same(a.as_bytes(), a.clone().as_bytes()));
        assert!(!same(a.as_bytes(), b.as_bytes()));
        assert!(!same(b"abc", b"abcd"));
    }

    /// A minimal client: what `claude` does on the wire.
    struct Client {
        stream: TcpStream,
    }

    impl Client {
        fn connect(port: u16, token: Option<&str>, extra: &str) -> (Client, String) {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream.set_read_timeout(Some(WAIT)).unwrap();
            let auth = token
                .map(|t| format!("X-Claude-Code-Ide-Authorization: {t}\r\n"))
                .unwrap_or_default();
            write!(
                stream,
                "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n{auth}{extra}\r\n"
            )
            .unwrap();
            let mut response = Vec::new();
            let mut byte = [0u8; 1];
            while !response.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).unwrap_or(0) == 0 {
                    break;
                }
                response.push(byte[0]);
            }
            (Client { stream }, String::from_utf8(response).unwrap())
        }

        fn send(&mut self, fin: bool, opcode: u8, payload: &[u8]) {
            let mask = [0x12, 0x34, 0x56, 0x78];
            let mut frame = vec![(if fin { 0x80 } else { 0 }) | opcode];
            match payload.len() {
                n if n < 126 => frame.push(0x80 | n as u8),
                n => {
                    frame.push(0x80 | 126);
                    frame.extend_from_slice(&(n as u16).to_be_bytes());
                }
            }
            frame.extend_from_slice(&mask);
            frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
            self.stream.write_all(&frame).unwrap();
        }

        /// One server frame: opcode and payload.
        fn recv(&mut self) -> (u8, Vec<u8>) {
            let mut head = [0u8; 2];
            self.stream.read_exact(&mut head).unwrap();
            assert_eq!(head[1] & 0x80, 0, "server frames are unmasked");
            let len = match head[1] & 0x7F {
                126 => {
                    let mut b = [0u8; 2];
                    self.stream.read_exact(&mut b).unwrap();
                    u16::from_be_bytes(b) as usize
                }
                127 => {
                    let mut b = [0u8; 8];
                    self.stream.read_exact(&mut b).unwrap();
                    u64::from_be_bytes(b) as usize
                }
                n => n as usize,
            };
            let mut payload = vec![0u8; len];
            self.stream.read_exact(&mut payload).unwrap();
            (head[0] & 0x0F, payload)
        }
    }

    fn server() -> (Server, String) {
        let token = new_token().unwrap();
        (
            Server::start(token.clone(), Box::new(|| {})).unwrap(),
            token,
        )
    }

    #[test]
    fn a_wrong_or_missing_token_is_refused_before_the_upgrade() {
        let (server, _) = server();
        let (_, response) = Client::connect(server.port(), Some("0123"), "");
        assert!(response.starts_with("HTTP/1.1 401"), "{response}");
        let (_, response) = Client::connect(server.port(), None, "");
        assert!(response.starts_with("HTTP/1.1 401"), "{response}");
        assert_eq!(server.recv_timeout(Duration::from_millis(200)), None);
        assert!(!server.is_connected());
    }

    #[test]
    fn text_round_trips_including_fragments_pings_and_large_messages() {
        let (server, token) = server();
        let (mut client, response) = Client::connect(
            server.port(),
            Some(&token),
            "Sec-WebSocket-Protocol: mcp\r\n",
        );
        assert!(response.starts_with("HTTP/1.1 101"), "{response}");
        assert!(response.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"));
        assert!(response.contains("Sec-WebSocket-Protocol: mcp\r\n"));
        let Some(Event::Connected(id)) = server.recv_timeout(WAIT) else {
            panic!("no connection");
        };

        client.send(true, OP_TEXT, "{\"jsonrpc\":\"2.0\"}".as_bytes());
        assert_eq!(
            server.recv_timeout(WAIT),
            Some(Event::Text(id, "{\"jsonrpc\":\"2.0\"}".into()))
        );

        // A ping between the fragments of one message.
        client.send(false, OP_TEXT, "héllo, ".as_bytes());
        client.send(true, OP_PING, b"p");
        assert_eq!(client.recv(), (OP_PONG, b"p".to_vec()));
        client.send(true, OP_CONTINUATION, "wörld".as_bytes());
        assert_eq!(
            server.recv_timeout(WAIT),
            Some(Event::Text(id, "héllo, wörld".into()))
        );

        let big = "x".repeat(70_000);
        assert!(server.send(&big));
        assert_eq!(client.recv(), (OP_TEXT, big.into_bytes()));
        assert!(server.send("hi"));
        assert_eq!(client.recv(), (OP_TEXT, b"hi".to_vec()));

        client.send(true, OP_CLOSE, &[]);
        assert_eq!(server.recv_timeout(WAIT), Some(Event::Closed(id)));
        assert!(!server.is_connected());
        assert!(!server.send("gone"));
    }

    #[test]
    fn a_new_client_replaces_the_old_one() {
        let (server, token) = server();
        let (mut first, _) = Client::connect(server.port(), Some(&token), "");
        let Some(Event::Connected(a)) = server.recv_timeout(WAIT) else {
            panic!("no first connection");
        };
        let (mut second, _) = Client::connect(server.port(), Some(&token), "");
        let Some(Event::Connected(b)) = server.recv_timeout(WAIT) else {
            panic!("no second connection");
        };
        assert_ne!(a, b);
        // The first reader ends, but its close is not reported: it no
        // longer owns the connection.
        assert_eq!(server.recv_timeout(Duration::from_millis(300)), None);
        let mut byte = [0u8; 1];
        assert_eq!(first.stream.read(&mut byte).unwrap_or(0), 0);

        assert!(server.send("to b"));
        assert_eq!(second.recv(), (OP_TEXT, b"to b".to_vec()));
        second.send(true, OP_TEXT, b"from b");
        assert_eq!(
            server.recv_timeout(WAIT),
            Some(Event::Text(b, "from b".into()))
        );
    }

    #[test]
    fn unmasked_client_frames_drop_the_connection() {
        let (server, token) = server();
        let (mut client, _) = Client::connect(server.port(), Some(&token), "");
        let Some(Event::Connected(id)) = server.recv_timeout(WAIT) else {
            panic!("no connection");
        };
        client.stream.write_all(&[0x81, 0x02, b'h', b'i']).unwrap();
        assert_eq!(server.recv_timeout(WAIT), Some(Event::Closed(id)));
    }
}
