//! The client against `scripts/fake-mcp.py`, in both protocol eras.

use super::*;

fn fake(mode: &str) -> Server {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/fake-mcp.py");
    Server::new(
        "garden",
        ServerConfig {
            command: "/usr/bin/python3".into(),
            args: vec![script.into(), mode.into()],
            ..ServerConfig::default()
        },
    )
}

/// Polls until `done` holds, collecting every event, or fails after a while.
fn until(server: &mut Server, mut done: impl FnMut(&Server, &[Event]) -> bool) -> Vec<Event> {
    let start = Instant::now();
    let mut events = Vec::new();
    loop {
        events.extend(server.poll());
        if done(server, &events) {
            return events;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out: {:?}, {events:?}",
            server.status
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn lists_loaded(server: &Server) -> bool {
    server.status == Status::Ready
        && server.tools.len() == 3
        && server.resources.len() == 1
        && server.prompts.len() == 1
        && !server.busy()
}

fn answer(events: &[Event], id: u64) -> Option<(&str, bool)> {
    events.iter().find_map(|e| match e {
        Event::Answer { id: i, text, error } if *i == id => Some((text.as_str(), *error)),
        _ => None,
    })
}

fn exercise(mode: &str, era: Era, info: &str) {
    let mut server = fake(mode);
    server.start(Path::new(env!("CARGO_MANIFEST_DIR")), Box::new(|| {}));
    assert_eq!(server.status, Status::Starting);
    // A call made while it starts is sent once it is ready.
    server.call_tool(1, "add", json::parse(r#"{"a": 2, "b": 3}"#).unwrap());
    let events = until(&mut server, |s, e| {
        lists_loaded(s) && answer(e, 1).is_some()
    });
    assert_eq!(server.era, Some(era));
    assert_eq!(server.info, info);
    assert_eq!(answer(&events, 1), Some(("5\n", false)));
    // Paged: the second page was followed.
    let names: Vec<&str> = server.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["add", "wipe", "crash"]);
    assert!(server.tools[0].read_only && !server.tools[0].destructive);
    assert!(server.tools[1].destructive);
    assert_eq!(server.prompts[0].arguments, [("season".to_string(), true)]);

    server.call_tool(2, "wipe", Value::Object(Vec::new()));
    server.read_resource(3, "note://beds");
    server.get_prompt(4, "plan", json::parse(r#"{"season": "spring"}"#).unwrap());
    let events = until(&mut server, |_, e| {
        [2, 3, 4].iter().all(|id| answer(e, *id).is_some())
    });
    assert_eq!(answer(&events, 2), Some(("refusing to wipe\n", true)));
    assert_eq!(
        answer(&events, 3),
        Some(("# Beds\ntomatoes, beans\n", false))
    );
    assert_eq!(
        answer(&events, 4),
        Some(("user:\nPlan the spring beds\n", false))
    );

    // A server that dies answers its waiting calls and says why.
    server.call_tool(5, "crash", Value::Object(Vec::new()));
    let events = until(&mut server, |s, _| matches!(s.status, Status::Failed(_)));
    assert_eq!(answer(&events, 5).map(|(_, e)| e), Some(true));
    assert_eq!(
        server.status,
        Status::Failed("the server exited: fake-mcp: crashing on purpose".into())
    );
    assert!(!server.is_running());
}

#[test]
fn a_legacy_server_is_opened_with_initialize() {
    exercise("legacy", Era::Legacy, "fake-legacy 0.9");
}

#[test]
fn a_modern_server_gets_its_version_on_every_request() {
    // The fake refuses any request without the per-request _meta, so
    // getting through at all shows every request carried it.
    exercise("modern", Era::Modern, "fake-modern 1.0");
}

#[test]
fn a_refused_command_never_starts() {
    let mut server = Server::new(
        "fetching",
        ServerConfig {
            command: "npx".into(),
            args: vec!["-y".into(), "some-server".into()],
            ..ServerConfig::default()
        },
    );
    server.start(Path::new("/tmp"), Box::new(|| {}));
    assert!(matches!(&server.status, Status::Failed(why) if why.contains("downloads")));
    assert!(!server.is_running());
}

/// The fake as an HTTP server: started, its port read, killed on drop.
struct HttpFake(std::process::Child, u16);

impl HttpFake {
    fn start(mode: &str) -> HttpFake {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/fake-mcp.py");
        let child = std::process::Command::new("/usr/bin/python3")
            .args([script, mode])
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // The guard first, so a panic reading the port still kills it.
        let mut fake = HttpFake(child, 0);
        let mut line = String::new();
        std::io::BufReader::new(fake.0.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        fake.1 = line.trim().parse().unwrap();
        fake
    }
}

impl Drop for HttpFake {
    fn drop(&mut self) {
        // Its own group (`process_group(0)`): the negative pid is all of it.
        if let Ok(pid) = i32::try_from(self.0.id()) {
            crate::platform::send_signal(-pid, 9);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn over_http(mode: &str, era: Era, info: &str) {
    let fake = HttpFake::start(mode);
    let mut server = Server::new(
        "garden",
        ServerConfig {
            url: Some(format!("http://127.0.0.1:{}/mcp", fake.1)),
            ..ServerConfig::default()
        },
    );
    server.start(Path::new("/tmp"), Box::new(|| {}));
    server.call_tool(1, "add", json::parse(r#"{"a": 4, "b": 5}"#).unwrap());
    let events = until(&mut server, |s, e| {
        lists_loaded(s) && answer(e, 1).is_some()
    });
    assert_eq!(server.era, Some(era));
    assert_eq!(server.info, info);
    assert_eq!(answer(&events, 1), Some(("9\n", false)));
    server.read_resource(2, "note://beds");
    let events = until(&mut server, |_, e| answer(e, 2).is_some());
    assert_eq!(
        answer(&events, 2),
        Some(("# Beds\ntomatoes, beans\n", false))
    );
    // The server gone: a request fails locally, and says HTTP or curl.
    drop(fake);
    server.call_tool(3, "add", Value::Object(Vec::new()));
    let events = until(&mut server, |_, e| answer(e, 3).is_some());
    assert_eq!(answer(&events, 3).map(|(_, e)| e), Some(true));
}

#[test]
fn a_legacy_http_server_keeps_its_session_and_streams() {
    over_http("http-legacy", Era::Legacy, "fake-legacy 0.9");
}

#[test]
fn a_modern_http_server_gets_its_headers() {
    over_http("http-modern", Era::Modern, "fake-modern 1.0");
}

use std::path::Path;
