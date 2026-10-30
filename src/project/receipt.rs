//! A session's receipt: every shell command its agent ran, and how each
//! ended, as Claude Code's own hooks report it rather than as the agent
//! later tells it. A run that succeeded comes from PostToolUse, which
//! carries no exit code; one that failed from PostToolUseFailure, whose
//! error begins `Exit code N`. A command started in the background has
//! not ended when its hook runs, so its outcome is not known.

use std::collections::VecDeque;

/// The most commands a receipt keeps; the counts go on past it.
pub const KEEP: usize = 200;
/// The most of a command a receipt keeps.
pub const COMMAND_CHARS: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Exit code 0.
    Passed,
    /// A non-zero exit code.
    Failed(i32),
    /// Failed without an exit code: a timeout, a refused call, a tool
    /// error.
    Errored,
    /// Stopped by the person.
    Interrupted,
    /// Started in the background: still running when its hook ran.
    Background,
}

impl Outcome {
    /// How it is kept in the events file.
    pub fn code(self) -> String {
        match self {
            Outcome::Passed => "0".into(),
            Outcome::Failed(n) => n.to_string(),
            Outcome::Errored => "err".into(),
            Outcome::Interrupted => "int".into(),
            Outcome::Background => "bg".into(),
        }
    }

    pub fn from_code(code: &str) -> Option<Outcome> {
        Some(match code {
            "0" => Outcome::Passed,
            "err" => Outcome::Errored,
            "int" => Outcome::Interrupted,
            "bg" => Outcome::Background,
            n => Outcome::Failed(n.parse().ok().filter(|&n| n != 0)?),
        })
    }

    /// Whether it did not succeed.
    pub fn failed(self) -> bool {
        matches!(
            self,
            Outcome::Failed(_) | Outcome::Errored | Outcome::Interrupted
        )
    }

    /// A few words, for the receipt's rows.
    pub fn label(self) -> String {
        match self {
            Outcome::Passed => "ok".into(),
            Outcome::Failed(n) => format!("exit {n}"),
            Outcome::Errored => "error".into(),
            Outcome::Interrupted => "stopped".into(),
            Outcome::Background => "background".into(),
        }
    }
}

/// The outcome a PostToolUseFailure's `error` text says: `Exit code N` on
/// its first line, else an error without a code.
pub fn failure_outcome(error: &str, interrupted: bool) -> Outcome {
    if interrupted {
        return Outcome::Interrupted;
    }
    error
        .lines()
        .next()
        .and_then(|line| line.trim().strip_prefix("Exit code "))
        .and_then(|n| n.trim().parse::<i32>().ok())
        .filter(|&n| n != 0)
        .map_or(Outcome::Errored, Outcome::Failed)
}

/// One command an agent ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ran {
    /// Unix seconds, when its hook ran.
    pub at: u64,
    pub outcome: Outcome,
    /// On one line, at most [`COMMAND_CHARS`].
    pub command: String,
}

/// The command as a receipt keeps it: one line, bounded.
pub fn clean_command(command: &str) -> String {
    command
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(COMMAND_CHARS)
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Receipt {
    /// The newest last.
    pub ran: VecDeque<Ran>,
    /// Every command since the session started, past [`KEEP`] too.
    pub total: usize,
    pub failed: usize,
}

impl Receipt {
    pub fn note(&mut self, ran: Ran) {
        self.total += 1;
        if ran.outcome.failed() {
            self.failed += 1;
        }
        if self.ran.len() == KEEP {
            self.ran.pop_front();
        }
        self.ran.push_back(ran);
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// The newest `n`, newest first.
    pub fn latest(&self, n: usize) -> impl Iterator<Item = &Ran> {
        self.ran.iter().rev().take(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_reads_its_exit_code_from_the_error() {
        assert_eq!(
            failure_outcome("Exit code 101\nerror: test failed", false),
            Outcome::Failed(101)
        );
        assert_eq!(
            failure_outcome("Command timed out after 2m", false),
            Outcome::Errored
        );
        assert_eq!(failure_outcome("Exit code 1", true), Outcome::Interrupted);
        assert_eq!(failure_outcome("Exit code 0", false), Outcome::Errored);
    }

    #[test]
    fn an_outcome_survives_its_code() {
        for outcome in [
            Outcome::Passed,
            Outcome::Failed(101),
            Outcome::Failed(-1),
            Outcome::Errored,
            Outcome::Interrupted,
            Outcome::Background,
        ] {
            assert_eq!(Outcome::from_code(&outcome.code()), Some(outcome));
        }
        assert_eq!(Outcome::from_code("x"), None);
    }

    #[test]
    fn a_command_is_one_bounded_line() {
        let long = format!("cargo test\n  --offline {}", "x".repeat(900));
        let clean = clean_command(&long);
        assert!(clean.starts_with("cargo test --offline x"));
        assert_eq!(clean.chars().count(), COMMAND_CHARS);
    }

    #[test]
    fn a_receipt_counts_past_what_it_keeps() {
        let mut receipt = Receipt::default();
        for i in 0..KEEP + 5 {
            receipt.note(Ran {
                at: i as u64,
                outcome: if i % 2 == 0 {
                    Outcome::Passed
                } else {
                    Outcome::Failed(1)
                },
                command: format!("c{i}"),
            });
        }
        assert_eq!(receipt.total, KEEP + 5);
        assert_eq!(receipt.failed, (KEEP + 5) / 2);
        assert_eq!(receipt.ran.len(), KEEP);
        let newest: Vec<_> = receipt.latest(2).map(|r| r.command.as_str()).collect();
        assert_eq!(newest, [format!("c{}", KEEP + 4), format!("c{}", KEEP + 3)]);
    }
}
