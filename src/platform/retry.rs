//! Auto-continue: a Claude Code turn that ended on a temporary API error
//! (an overloaded or failing server, a dropped connection) is told to
//! "continue", after 20, 40 and 80 seconds, unless the person types into
//! it first. Claude's `StopFailure` hook says which error ended the turn;
//! this says whether it is worth another try, and when.
//!
//! Everything else is the person's: a usage or rate limit, a login,
//! billing or account problem, a bad request, a model that is not there.
//! Another try would fail the same way, or spend what the person did not
//! mean to.

use std::time::{Duration, Instant};

/// How many times one streak of errors is told to continue.
pub const MAX_SENDS: u32 = 3;

/// The wait before the first try; each one after waits twice the last.
pub const FIRST_DELAY: Duration = Duration::from_secs(20);

/// How recent a keystroke into the session showing holds a try back.
pub const TYPING_QUIET: Duration = Duration::from_secs(10);

/// What is typed into the session to carry on.
pub const WORD: &str = "continue";

/// How long after the word its Return goes: one write of both reads as a
/// paste with a line break in it to a program that tells pastes from
/// typing by how fast the bytes come.
pub const ENTER_GAP: Duration = Duration::from_millis(200);

/// Words in an `unknown` error's text that say the connection failed, not
/// the request: worth another try.
const NETWORK: [&str; 7] = [
    "unable to connect",
    "connection dropped",
    "connection lost",
    "connection closed",
    "econn",
    "timed out",
    "socket hang up",
];

/// Whether an API error that ended a turn is temporary: `error` as Claude
/// Code's `StopFailure` hook names it, `text` what it said of it.
pub fn temporary(error: &str, text: &str) -> bool {
    match error {
        "overloaded" | "server_error" => true,
        "unknown" => {
            let text = text.to_lowercase();
            NETWORK.iter().any(|word| text.contains(word))
        }
        // rate_limit covers usage limits and "high load, use /model" too:
        // waiting a minute does not lift either.
        _ => false,
    }
}

/// The wait before the first try: [`FIRST_DELAY`], or under a self-test
/// what `CRC_AUTO_CONTINUE_DELAY_MS` says.
pub fn first_delay() -> Duration {
    if std::env::var_os("CRC_SELFTEST").is_some()
        && let Some(ms) = std::env::var("CRC_AUTO_CONTINUE_DELAY_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
    {
        return Duration::from_millis(ms);
    }
    FIRST_DELAY
}

/// One streak of temporary errors in a session: from the first until a
/// turn ends without one or the person types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retry {
    /// When the newest error ended the turn.
    pub error_at: Instant,
    /// How many times the streak was told to continue.
    pub tries: u32,
    /// When the next try is due; `None` while one is out, and once the
    /// streak has had [`MAX_SENDS`].
    pub next_at: Option<Instant>,
    /// When the Return after the word just typed goes.
    pub enter_at: Option<Instant>,
}

impl Retry {
    /// A temporary error ended the turn at `now`, `streak` the one it
    /// carries on, if any: the next try scheduled, each wait twice the
    /// last from `first`, none after [`MAX_SENDS`].
    pub fn after_error(streak: Option<&Retry>, now: Instant, first: Duration) -> Retry {
        let tries = streak.map_or(0, |r| r.tries);
        Retry {
            error_at: now,
            tries,
            next_at: (tries < MAX_SENDS).then(|| now + first * 2u32.pow(tries)),
            enter_at: None,
        }
    }

    /// When something is next to be done: a try, or a Return.
    pub fn next_look(&self) -> Option<Instant> {
        match (self.next_at, self.enter_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Whether a try is scheduled: what the sidebar says as "retrying".
    pub fn pending(&self) -> bool {
        self.next_at.is_some()
    }

    /// Whether the person typed into the session since the error: the
    /// streak is theirs now, and nothing more is sent.
    pub fn typed_since(&self, last_typed: Option<Instant>) -> bool {
        last_typed.is_some_and(|at| at >= self.error_at)
    }

    /// Whether to send now, checked right before sending: due, nothing
    /// typed into the session since the error, and, when it is the one
    /// showing, nothing typed into it for [`TYPING_QUIET`].
    pub fn due(&self, now: Instant, last_typed: Option<Instant>, showing: bool) -> bool {
        let typing = showing
            && last_typed.is_some_and(|at| now.saturating_duration_since(at) < TYPING_QUIET);
        self.next_at.is_some_and(|at| now >= at) && !self.typed_since(last_typed) && !typing
    }

    /// The word was typed at `now`: counted, its Return due after
    /// [`ENTER_GAP`], and nothing more until the turn it starts ends.
    pub fn sent(&mut self, now: Instant) {
        self.tries += 1;
        self.next_at = None;
        self.enter_at = Some(now + ENTER_GAP);
    }

    /// Whether the Return after the word goes now: nothing typed since the
    /// error, which would be in the line it sends.
    pub fn enter_due(&self, now: Instant, last_typed: Option<Instant>) -> bool {
        self.enter_at.is_some_and(|at| now >= at) && !self.typed_since(last_typed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_temporary_errors_are_tried_again() {
        assert!(temporary("overloaded", ""));
        assert!(temporary("server_error", "API Error: 500"));
        for text in [
            "API Error: Unable to connect to API (ECONNRESET)",
            "Connection dropped",
            "Request timed out.",
            "socket hang up",
            "connect ECONNREFUSED 127.0.0.1:443",
        ] {
            assert!(temporary("unknown", text), "{text}");
        }
        assert!(!temporary("unknown", ""));
        assert!(!temporary("unknown", "something went wrong"));
        for error in [
            "rate_limit",
            "authentication_failed",
            "oauth_org_not_allowed",
            "account_on_hold",
            "verification_required",
            "billing_error",
            "invalid_request",
            "model_not_found",
            "max_output_tokens",
            "cloud_credential_error",
        ] {
            assert!(!temporary(error, "Connection dropped"), "{error}");
        }
        assert!(!temporary(
            "rate_limit",
            "Claude usage limit reached, resets at 5pm"
        ));
        assert!(!temporary(
            "rate_limit",
            "Claude is under high load, use /model to switch"
        ));
    }

    #[test]
    fn the_waits_double_and_stop_after_three() {
        let start = Instant::now();
        let first = FIRST_DELAY;
        let mut retry = Retry::after_error(None, start, first);
        assert_eq!(retry.next_at, Some(start + Duration::from_secs(20)));
        assert!(retry.pending());
        assert!(!retry.due(start + Duration::from_secs(19), None, false));
        assert!(retry.due(start + Duration::from_secs(20), None, false));
        let at = start + Duration::from_secs(20);
        retry.sent(at);
        assert!(!retry.pending());
        // Its Return a moment after the word.
        assert_eq!(retry.next_look(), Some(at + ENTER_GAP));
        assert!(!retry.enter_due(at, None));
        assert!(retry.enter_due(at + ENTER_GAP, None));
        assert!(!retry.enter_due(at + ENTER_GAP, Some(at + Duration::from_millis(50))));

        let again = start + Duration::from_secs(30);
        let mut retry = Retry::after_error(Some(&retry), again, first);
        assert_eq!(retry.next_at, Some(again + Duration::from_secs(40)));
        assert_eq!(retry.enter_at, None);
        retry.sent(again + Duration::from_secs(40));
        let third = start + Duration::from_secs(100);
        let mut retry = Retry::after_error(Some(&retry), third, first);
        assert_eq!(retry.next_at, Some(third + Duration::from_secs(80)));
        retry.sent(third + Duration::from_secs(80));
        assert_eq!(retry.tries, 3);

        // The third failed too: nothing more, it waits on the person.
        let last = start + Duration::from_secs(300);
        let retry = Retry::after_error(Some(&retry), last, first);
        assert!(!retry.pending());
        assert_eq!(retry.next_look(), None);
        assert!(!retry.due(last + Duration::from_secs(1000), None, false));
    }

    #[test]
    fn a_turn_that_ends_well_starts_a_new_streak() {
        let start = Instant::now();
        let mut retry = Retry::after_error(None, start, FIRST_DELAY);
        retry.sent(start + FIRST_DELAY);
        // The session worked again and finished: the window drops the
        // streak, so the next error is a first one.
        let later = start + Duration::from_secs(600);
        let retry = Retry::after_error(None, later, FIRST_DELAY);
        assert_eq!(retry.tries, 0);
        assert_eq!(retry.next_at, Some(later + FIRST_DELAY));
    }

    #[test]
    fn typing_since_the_error_holds_it_back() {
        let start = Instant::now();
        let retry = Retry::after_error(None, start, FIRST_DELAY);
        let due = start + FIRST_DELAY;
        assert!(!retry.typed_since(Some(start - Duration::from_secs(1))));
        assert!(retry.due(due, Some(start - Duration::from_secs(60)), true));
        let typed = Some(start + Duration::from_secs(5));
        assert!(retry.typed_since(typed));
        assert!(!retry.due(due, typed, false));
        assert!(!retry.due(due + Duration::from_secs(600), typed, false));
    }

    #[test]
    fn typing_in_the_session_showing_holds_it_back() {
        let start = Instant::now();
        // Typed before the error, so not an answer to it, but recently.
        let retry = Retry::after_error(None, start, Duration::from_millis(500));
        let typed = Some(start - Duration::from_millis(100));
        let due = start + Duration::from_secs(1);
        assert!(!retry.due(due, typed, true), "being typed into");
        assert!(retry.due(due, typed, false), "not the one showing");
        assert!(retry.due(start + TYPING_QUIET, typed, true));
    }
}
