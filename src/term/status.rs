//! The Program Status Protocol (OSC 7501): a program tells the terminal
//! whether it is idle, working, done, blocked on the person or failed, and
//! why. Claude Code reports through it since 2.1.295, once the terminal
//! has answered its `?` probe. Specification revision 0.3 (2026-10-07).
//!
//! A report is `key=value` pairs joined by `:`. The terminal keeps one
//! record per `id`; a report replaces its record whole, `state=clear`
//! removes a record and everything under it, and without an id, every
//! record. The root record, with no id, is what a session's row shows.

use std::collections::BTreeMap;

/// The longest report taken, as the specification has it.
const MAX_REPORT: usize = 4096;
/// Records kept per terminal; the least recently updated goes first.
const MAX_RECORDS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// At rest, waiting for the next instruction.
    Idle,
    Working,
    /// Finished, and the result has not been seen yet.
    Done,
    /// Needs the person: `kind` says what for, `msg` why.
    Blocked,
    /// Failed and stopped.
    Error,
}

/// What a blocked program waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Approval to act.
    Permission,
    /// An answer to type.
    Question,
    /// A login, token or credential.
    Auth,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    pub kind: Option<Kind>,
    /// 0 to 100, for working and blocked; absent is indeterminate.
    pub progress: Option<u8>,
    /// The program's machine name, `claude-code` say.
    pub app: Option<String>,
    pub title: Option<String>,
    pub msg: Option<String>,
}

impl Status {
    /// Its one line for a person: the message, else the title, else what
    /// the state and kind say on their own.
    pub fn text(&self) -> String {
        if let Some(msg) = self.msg.as_deref().filter(|m| !m.is_empty()) {
            return msg.to_owned();
        }
        if let Some(title) = self.title.as_deref().filter(|t| !t.is_empty()) {
            return title.to_owned();
        }
        match (self.state, self.kind) {
            (State::Blocked, Some(Kind::Permission)) => "needs permission".into(),
            (State::Blocked, Some(Kind::Question)) => "asks a question".into(),
            (State::Blocked, Some(Kind::Auth)) => "needs a login".into(),
            (State::Blocked, None) => "needs you".into(),
            (State::Done, _) => "finished".into(),
            (State::Error, _) => "failed".into(),
            (State::Working, _) => "working".into(),
            (State::Idle, _) => "idle".into(),
        }
    }
}

/// One parsed report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Report {
    /// `?`: the program asks whether the terminal speaks the protocol.
    Query,
    /// Remove the record with this id and its descendants; all without one.
    Clear(Option<String>),
    /// Replace the record with this id (`""` is the root).
    Set(String, Status),
}

/// The records of one terminal, by id; the root is `""`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Records {
    by_id: BTreeMap<String, Status>,
    /// Update order, oldest first, so the cap removes the stalest.
    order: Vec<String>,
}

impl Records {
    pub fn root(&self) -> Option<&Status> {
        self.by_id.get("")
    }

    pub fn get(&self, id: &str) -> Option<&Status> {
        self.by_id.get(id)
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// The records under the root, the program's own tasks, by id.
    pub fn children(&self) -> impl Iterator<Item = (&str, &Status)> {
        self.by_id
            .iter()
            .filter(|(id, _)| !id.is_empty())
            .map(|(id, status)| (id.as_str(), status))
    }

    /// Applies a report; `true` when a record changed.
    pub fn apply(&mut self, report: Report) -> bool {
        match report {
            Report::Query => false,
            Report::Clear(None) => {
                let had = !self.by_id.is_empty();
                self.by_id.clear();
                self.order.clear();
                had
            }
            Report::Clear(Some(id)) => {
                let gone: Vec<String> = self
                    .by_id
                    .keys()
                    .filter(|k| **k == id || k.starts_with(&format!("{id}/")))
                    .cloned()
                    .collect();
                for k in &gone {
                    self.by_id.remove(k);
                }
                self.order.retain(|k| !gone.contains(k));
                !gone.is_empty()
            }
            Report::Set(id, status) => {
                let same = self.by_id.get(&id) == Some(&status);
                self.order.retain(|k| *k != id);
                self.order.push(id.clone());
                self.by_id.insert(id, status);
                while self.by_id.len() > MAX_RECORDS {
                    let oldest = self.order.remove(0);
                    self.by_id.remove(&oldest);
                }
                !same
            }
        }
    }

    /// The person has seen the result: a root record that says done or
    /// failed goes, as the specification leaves to the terminal to decide.
    /// `true` when one went.
    pub fn acknowledge(&mut self) -> bool {
        let seen = self
            .root()
            .is_some_and(|s| matches!(s.state, State::Done | State::Error));
        if seen {
            self.by_id.remove("");
            self.order.retain(|k| !k.is_empty());
        }
        seen
    }

    /// A new shell prompt, or the program gone: what was in flight is
    /// over, what finished stays until the person has seen it.
    pub fn end_of_command(&mut self) -> bool {
        let before = self.by_id.len();
        self.by_id
            .retain(|_, s| !matches!(s.state, State::Working | State::Blocked | State::Idle));
        self.order.retain(|k| self.by_id.contains_key(k));
        self.by_id.len() != before
    }
}

/// The report in an OSC 7501 body, or `None` for one to ignore: an
/// unknown state, a malformed id, text that does not decode or carries a
/// control character, a body over the limit.
pub fn parse(body: &str) -> Option<Report> {
    if body.len() > MAX_REPORT {
        return None;
    }
    if body.trim() == "?" {
        return Some(Report::Query);
    }
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    for pair in body.split(':') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if key.is_empty()
            || key.len() > 16
            || !key.bytes().all(|b| b.is_ascii_lowercase())
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.,+/=-".contains(&b))
        {
            continue;
        }
        // The last of a repeated key wins.
        pairs.retain(|(k, _)| *k != key);
        pairs.push((key, value));
    }
    let get = |name: &str| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| *v);
    let id = match get("id") {
        None => String::new(),
        Some(id) => valid_id(id)?.to_owned(),
    };
    let state = match get("state")? {
        "idle" => State::Idle,
        "working" => State::Working,
        "done" => State::Done,
        "blocked" => State::Blocked,
        "error" => State::Error,
        "clear" => {
            return Some(Report::Clear(get("id").map(|_| id)));
        }
        _ => return None,
    };
    let kind = match (state, get("kind")) {
        (State::Blocked, Some("permission")) => Some(Kind::Permission),
        (State::Blocked, Some("question")) => Some(Kind::Question),
        (State::Blocked, Some("auth")) => Some(Kind::Auth),
        _ => None,
    };
    let progress = match state {
        State::Working | State::Blocked => get("progress")
            .and_then(|p| p.parse::<u8>().ok())
            .filter(|p| *p <= 100),
        _ => None,
    };
    let app = get("app")
        .filter(|a| !a.is_empty() && a.len() <= 32)
        .map(str::to_owned);
    let title = match get("title") {
        Some(coded) => Some(decode_text(coded, 192)?),
        None => None,
    };
    let msg = match get("msg") {
        Some(coded) => Some(decode_text(coded, 2048)?),
        None => None,
    };
    Some(Report::Set(
        id,
        Status {
            state,
            kind,
            progress,
            app,
            title,
            msg,
        },
    ))
}

/// Segments of letters, digits and `_ . + -`, 1 to 32 bytes each, `/`
/// between, 8 deep and 128 bytes at most.
fn valid_id(id: &str) -> Option<&str> {
    if id.is_empty() || id.len() > 128 {
        return None;
    }
    let segments: Vec<&str> = id.split('/').collect();
    let ok = segments.len() <= 8
        && segments.iter().all(|s| {
            (1..=32).contains(&s.len())
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b))
        });
    ok.then_some(id)
}

/// Base64 UTF-8 of at most `max` decoded bytes, refused with any control
/// character; text-direction overrides and other invisible formatting are
/// dropped, since a record's text is shown outside the grid.
fn decode_text(coded: &str, max: usize) -> Option<String> {
    let bytes = crate::base64::decode(coded)?;
    if bytes.len() > max {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    if text.chars().any(char::is_control) {
        return None;
    }
    Some(
        text.chars()
            .filter(|c| {
                !matches!(
                    *c,
                    '\u{200B}'..='\u{200F}'
                        | '\u{202A}'..='\u{202E}'
                        | '\u{2060}'..='\u{2064}'
                        | '\u{2066}'..='\u{2069}'
                        | '\u{FEFF}'
                )
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(text: &str) -> String {
        crate::base64::encode(text.as_bytes())
    }

    #[test]
    fn parses_a_report_with_every_key() {
        let body = format!(
            "state=blocked:kind=permission:app=claude-code:progress=40:title={}:msg={}",
            b64("Edit"),
            b64("Edit src/main.rs?")
        );
        assert_eq!(
            parse(&body),
            Some(Report::Set(
                String::new(),
                Status {
                    state: State::Blocked,
                    kind: Some(Kind::Permission),
                    progress: Some(40),
                    app: Some("claude-code".into()),
                    title: Some("Edit".into()),
                    msg: Some("Edit src/main.rs?".into()),
                }
            ))
        );
        // Padding is optional, and the last of a repeated key wins.
        let body = format!(
            "state=idle:state=working:msg={}",
            b64("ab").trim_end_matches('=')
        );
        let Some(Report::Set(id, status)) = parse(&body) else {
            panic!("no report");
        };
        assert_eq!(id, "");
        assert_eq!(status.state, State::Working);
        assert_eq!(status.msg.as_deref(), Some("ab"));
        // Kind and progress only mean something in some states.
        let Some(Report::Set(_, done)) = parse("state=done:kind=question:progress=50") else {
            panic!("no report");
        };
        assert_eq!((done.kind, done.progress), (None, None));
    }

    #[test]
    fn ignores_what_the_specification_says_to() {
        assert_eq!(parse("state=flying"), None);
        assert_eq!(parse("app=x"), None);
        assert_eq!(parse("state=working:id=/a"), None);
        assert_eq!(parse("state=working:id=a//b"), None);
        assert_eq!(parse("state=working:id=a/b/c/d/e/f/g/h/i"), None);
        assert_eq!(parse(&format!("state=done:msg={}", b64("a\x07b"))), None);
        assert_eq!(parse("state=done:msg=a.b"), None);
        assert_eq!(
            parse(&format!("state=done:title={}", b64(&"x".repeat(193)))),
            None
        );
        assert_eq!(parse(&format!("state=done{}", ":k=v".repeat(1100))), None);
        // A malformed pair is skipped, the rest read; unknown keys too.
        assert!(matches!(
            parse("state=working:garbage:future=1:progress=200"),
            Some(Report::Set(
                _,
                Status {
                    state: State::Working,
                    progress: None,
                    ..
                }
            ))
        ));
        // An unknown kind is as good as none.
        let Some(Report::Set(_, s)) = parse("state=blocked:kind=lunch") else {
            panic!("no report");
        };
        assert_eq!(s.kind, None);
        // Direction overrides are dropped, not refused.
        let Some(Report::Set(_, s)) = parse(&format!("state=done:msg={}", b64("a\u{202E}b")))
        else {
            panic!("no report");
        };
        assert_eq!(s.msg.as_deref(), Some("ab"));
    }

    #[test]
    fn records_are_kept_by_id_and_cleared_by_subtree() {
        let mut records = Records::default();
        let set = |id: &str, state: State| {
            Report::Set(
                id.into(),
                Status {
                    state,
                    kind: None,
                    progress: None,
                    app: None,
                    title: None,
                    msg: None,
                },
            )
        };
        assert!(records.apply(set("", State::Working)));
        assert!(!records.apply(set("", State::Working)));
        assert!(records.apply(set("t1", State::Working)));
        assert!(records.apply(set("t1/a", State::Done)));
        assert!(records.apply(set("t10", State::Working)));
        assert_eq!(records.len(), 4);
        assert_eq!(records.children().count(), 3);
        // `t1` goes with what is under it; `t10` is not under it.
        assert!(records.apply(Report::Clear(Some("t1".into()))));
        assert_eq!(
            records.children().map(|(id, _)| id).collect::<Vec<_>>(),
            ["t10"]
        );
        assert!(!records.apply(Report::Clear(Some("nothing".into()))));
        // A prompt ends what was in flight; what finished stays.
        assert!(records.apply(set("t10", State::Done)));
        assert!(records.end_of_command());
        assert_eq!(records.root(), None);
        assert_eq!(records.get("t10").map(|s| s.state), Some(State::Done));
        assert!(records.apply(Report::Clear(None)));
        assert!(records.is_empty());
        // Typing takes a result as seen; a block is not answered that way.
        records.apply(set("", State::Blocked));
        assert!(!records.acknowledge());
        records.apply(set("", State::Done));
        assert!(records.acknowledge());
        assert!(records.root().is_none());
        // The cap removes the stalest.
        for i in 0..300 {
            records.apply(set(&format!("r{i}"), State::Working));
        }
        assert_eq!(records.len(), MAX_RECORDS);
        assert!(records.get("r0").is_none());
        assert!(records.get("r299").is_some());
    }

    #[test]
    fn text_says_what_a_record_means() {
        let status = |state, kind| Status {
            state,
            kind,
            progress: None,
            app: None,
            title: None,
            msg: None,
        };
        assert_eq!(
            status(State::Blocked, Some(Kind::Permission)).text(),
            "needs permission"
        );
        assert_eq!(status(State::Done, None).text(), "finished");
        let mut with = status(State::Blocked, Some(Kind::Question));
        with.title = Some("Pick".into());
        assert_eq!(with.text(), "Pick");
        with.msg = Some("Which file?".into());
        assert_eq!(with.text(), "Which file?");
    }
}
