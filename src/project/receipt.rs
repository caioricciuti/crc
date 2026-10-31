//! A session's receipt: every shell command its agent ran, and how each
//! ended, as Claude Code's own hooks report it rather than as the agent
//! later tells it. A run that succeeded comes from PostToolUse, which
//! carries no exit code; one that failed from PostToolUseFailure, whose
//! error begins `Exit code N`. A command started in the background has
//! not ended when its hook runs, so its outcome is not known.
//!
//! The receipt also checks what the agent said at the end of a turn
//! against it: a message that says the tests pass, the code builds or the
//! lint is clean is held up to the newest command of that kind the turn
//! ran, which may have passed, failed, not run at all, or passed before
//! the agent wrote a file again.

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
    /// [`Receipt::total`] when the person last sent a prompt: the turn's
    /// commands are the ones after it.
    turn: usize,
    /// [`Receipt::total`] when the agent last wrote a file this turn.
    wrote: Option<usize>,
    /// What the agent's last message claimed, each checked.
    pub claims: Vec<Claim>,
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

    /// Whether there is anything to show: commands, or claims with no
    /// commands behind them.
    pub fn shown(&self) -> bool {
        !self.is_empty() || !self.claims.is_empty()
    }

    /// The person sent a prompt: a new turn, with nothing claimed yet.
    pub fn note_prompt(&mut self) {
        self.turn = self.total;
        self.wrote = None;
        self.claims.clear();
    }

    /// The agent wrote a file: what passed before it may not pass now.
    pub fn note_wrote(&mut self) {
        self.wrote = Some(self.total);
    }

    /// The agent's last message, in sentences: its claims, each held up to
    /// the newest command of its kind this turn ran.
    pub fn note_said(&mut self, sentences: &[String]) {
        let kept = self.ran.len();
        let first = self.total - kept;
        // The turn's commands with their place in the session.
        let turn: Vec<(usize, &Ran)> = self
            .ran
            .iter()
            .enumerate()
            .map(|(i, ran)| (first + i, ran))
            .filter(|(n, _)| *n >= self.turn)
            .collect();
        self.claims = claimed(sentences)
            .into_iter()
            .map(|(kind, sentence)| {
                let mut of_kind = turn
                    .iter()
                    .rev()
                    .filter(|(_, ran)| kinds_of(&ran.command).contains(&kind));
                let verdict = match of_kind
                    .clone()
                    .find(|(_, r)| r.outcome != Outcome::Background)
                {
                    Some((_, ran)) if ran.outcome.failed() => Verdict::Contradicted {
                        command: ran.command.clone(),
                        outcome: ran.outcome,
                    },
                    Some((n, ran)) if self.wrote.is_some_and(|w| w > *n) => {
                        Verdict::Stale(ran.command.clone())
                    }
                    Some((_, ran)) => Verdict::Backed(ran.command.clone()),
                    None => match of_kind.next() {
                        Some((_, ran)) => Verdict::Background(ran.command.clone()),
                        None => Verdict::Unrun,
                    },
                };
                Claim {
                    kind,
                    sentence,
                    verdict,
                }
            })
            .collect();
    }

    /// The newest `n`, newest first.
    pub fn latest(&self, n: usize) -> impl Iterator<Item = &Ran> {
        self.ran.iter().rev().take(n)
    }
}

/// The most sentences of a message a session keeps: the last ones, where
/// an agent sums up.
pub const SENTENCES: usize = 60;
/// The most of one sentence a session keeps.
pub const SENTENCE_CHARS: usize = 300;

/// What a claim is about, and what kind of command would show it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Tests,
    Build,
    Lint,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Tests => "tests",
            Kind::Build => "build",
            Kind::Lint => "lint",
        }
    }

    /// What the agent said, in a few words.
    pub fn said(self) -> &'static str {
        match self {
            Kind::Tests => "Said tests pass",
            Kind::Build => "Said it builds",
            Kind::Lint => "Said lint is clean",
        }
    }
}

/// How a claim stands against the turn's commands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The newest command of its kind passed, and nothing was written
    /// since.
    Backed(String),
    /// It passed, but the agent wrote a file after it.
    Stale(String),
    /// The newest command of its kind did not pass.
    Contradicted { command: String, outcome: Outcome },
    /// The only command of its kind went to the background: its outcome
    /// was never seen.
    Background(String),
    /// No command of its kind ran this turn.
    Unrun,
}

impl Verdict {
    /// A word or two, for the receipt card and the self-test.
    pub fn label(&self) -> String {
        match self {
            Verdict::Backed(_) => "checked".into(),
            Verdict::Stale(_) => "before last edit".into(),
            Verdict::Contradicted { outcome, .. } => outcome.label(),
            Verdict::Background(_) => "unseen".into(),
            Verdict::Unrun => "not run".into(),
        }
    }

    /// The command it rests on, when there is one.
    pub fn command(&self) -> Option<&str> {
        match self {
            Verdict::Backed(c) | Verdict::Stale(c) | Verdict::Background(c) => Some(c),
            Verdict::Contradicted { command, .. } => Some(command),
            Verdict::Unrun => None,
        }
    }
}

/// One thing the agent's last message said it checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    pub kind: Kind,
    /// The sentence that said it.
    pub sentence: String,
    pub verdict: Verdict,
}

/// A message as a session keeps it: its sentences, each on one line, the
/// last [`SENTENCES`] of them.
pub fn sentences(message: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in message.lines() {
        let mut start = 0;
        let mut chars = line.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            let ends = matches!(c, '.' | '!' | '?')
                && chars.peek().is_none_or(|(_, next)| next.is_whitespace());
            if ends {
                out.push(&line[start..i + c.len_utf8()]);
                start = i + c.len_utf8();
            }
        }
        out.push(&line[start..]);
    }
    let out: Vec<String> = out
        .into_iter()
        .map(|s| {
            s.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(SENTENCE_CHARS)
                .collect::<String>()
        })
        .filter(|s| !s.is_empty())
        .collect();
    let skip = out.len().saturating_sub(SENTENCES);
    out.into_iter().skip(skip).collect()
}

/// Words that make a sentence something other than a claim that a check
/// passed: a failure, a negation, a plan, a condition.
const NOT_A_CLAIM: &[&str] = &[
    "not", "no", "never", "without", "didn't", "haven't", "hasn't", "couldn't", "can't", "cannot",
    "wasn't", "isn't", "aren't", "won't", "don't", "doesn't", "unable", "skip", "skipped", "fail",
    "fails", "failing", "failed", "failure", "failures", "broken", "error", "errors", "should",
    "would", "could", "will", "might", "may", "once", "until", "if", "unless", "before", "need",
    "needs", "try", "please", "let", "run", "rerun",
];

/// Ways to say nothing failed, which would otherwise read as a failure.
const NOTHING_FAILED: &[&str] = &[
    "0 failed",
    "0 failures",
    "0 errors",
    "zero failures",
    "zero errors",
    "no failures",
    "no errors",
    "no warnings",
    "no issues",
    "without errors",
    "without warnings",
];

/// Words that say a check came out well.
const PASSED: &[&str] = &[
    "pass",
    "passes",
    "passed",
    "passing",
    "green",
    "succeed",
    "succeeds",
    "succeeded",
    "success",
    "successful",
    "successfully",
    "clean",
    "cleanly",
    "ok",
    "okay",
    "fine",
    "good",
    "works",
];

/// The kinds of check a word names, and whether it says one passed by
/// itself (`compiles`).
fn subject(word: &str) -> Option<(Kind, bool)> {
    Some(match word {
        "test" | "tests" | "suite" | "specs" | "selftest" => (Kind::Tests, false),
        w if w.ends_with("tests") => (Kind::Tests, false),
        "build" => (Kind::Build, false),
        "builds" | "built" | "compiles" | "compiled" => (Kind::Build, true),
        "clippy" | "lint" | "lints" | "linter" | "linting" | "eslint" | "typecheck"
        | "typechecks" | "tsc" | "ruff" | "mypy" | "fmt" | "rustfmt" | "prettier" => {
            (Kind::Lint, false)
        }
        _ => return None,
    })
}

/// The kinds of check a message claims passed, in the order it first
/// claims them, with the sentence that does. A sentence is cut into
/// clauses at commas, semicolons and `but`, so `clippy is clean, but the
/// tests fail` claims the lint and not the tests; a question claims
/// nothing.
pub fn claimed(sentences: &[String]) -> Vec<(Kind, String)> {
    let mut out: Vec<(Kind, String)> = Vec::new();
    for sentence in sentences {
        if sentence.ends_with('?') {
            continue;
        }
        let mut lower = sentence.to_lowercase().replace('\u{2019}', "'");
        for phrase in NOTHING_FAILED {
            lower = lower.replace(phrase, " clean ");
        }
        let mut found = Vec::new();
        for clause in lower
            .split([',', ';'])
            .flat_map(|c| c.split(" but "))
            .flat_map(|c| c.split(" although "))
            .flat_map(|c| c.split(" except "))
        {
            let words: Vec<&str> = clause
                .split(|c: char| !(c.is_alphanumeric() || c == '\''))
                .filter(|w| !w.is_empty())
                .collect();
            if words.iter().any(|w| NOT_A_CLAIM.contains(w)) {
                continue;
            }
            let passed = words.iter().any(|w| PASSED.contains(w));
            for word in &words {
                if let Some((kind, by_itself)) = subject(word)
                    && (passed || by_itself)
                    && !found.contains(&kind)
                {
                    found.push(kind);
                }
            }
        }
        found.sort();
        for kind in found {
            if !out.iter().any(|(k, _)| *k == kind) {
                out.push((kind, sentence.clone()));
            }
        }
    }
    out
}

/// Programs whose arguments are not commands: a test named in a commit
/// message or a search is not a test run.
const NOT_RUNNERS: &[&str] = &[
    "git", "echo", "printf", "grep", "rg", "cat", "sed", "awk", "ls", "head", "tail", "find", "gh",
    "cd", "less", "wc", "touch", "mkdir", "rm", "cp", "mv", "open", "kill", "pkill",
];

/// The kinds of check a command runs. Each part of a pipeline or a list
/// counts, and anything in front of the first program crc knows (an
/// environment variable, `time`, a wrapper) is passed over.
pub fn kinds_of(command: &str) -> Vec<Kind> {
    let mut kinds = Vec::new();
    for part in command.split(['&', '|', ';']) {
        let words: Vec<&str> = part.split_whitespace().collect();
        for (i, word) in words.iter().enumerate() {
            if word.starts_with(['"', '\'', '(', '$', '`']) || NOT_RUNNERS.contains(word) {
                break;
            }
            if let Some(kind) = runs(word, &words[i + 1..]) {
                if !kinds.contains(&kind) {
                    kinds.push(kind);
                }
                break;
            }
        }
    }
    kinds
}

/// The kind of check `program` with `args` runs, if crc knows it.
fn runs(program: &str, args: &[&str]) -> Option<Kind> {
    let name = program.rsplit('/').next().unwrap_or(program);
    let first = args.iter().copied().find(|a| !a.starts_with('-'));
    // A script or task named for what it does.
    let named = |task: &str| {
        let task = task.to_lowercase();
        if task.contains("test") || task == "t" {
            Some(Kind::Tests)
        } else if [
            "lint",
            "clippy",
            "typecheck",
            "tsc",
            "fmt",
            "format",
            "check",
        ]
        .iter()
        .any(|w| task.contains(w))
        {
            Some(Kind::Lint)
        } else if task.contains("build") || task.contains("compile") || task == "b" {
            Some(Kind::Build)
        } else {
            None
        }
    };
    match name {
        "cargo" | "go" | "swift" | "dotnet" | "mix" | "zig" | "gradle" | "gradlew" | "mvn" => {
            match first? {
                "test" | "t" | "nextest" => Some(Kind::Tests),
                "build" | "b" | "check" | "c" | "compile" | "assemble" => Some(Kind::Build),
                "clippy" | "vet" | "fmt" | "lint" => Some(Kind::Lint),
                _ => None,
            }
        }
        "npm" | "pnpm" | "yarn" | "bun" | "deno" => match first? {
            "run" | "run-script" => {
                named(after(args, first?).iter().find(|a| !a.starts_with('-'))?)
            }
            "exec" | "x" | "dlx" => program_in(after(args, first?)),
            task => named(task),
        },
        "npx" | "bunx" | "pnpx" | "uvx" => program_in(args),
        "uv" | "poetry" | "pipenv" | "hatch" => match first? {
            "run" => program_in(after(args, "run")),
            "build" => Some(Kind::Build),
            _ => None,
        },
        "python" | "python3" => program_in(after(args, "-m")),
        "make" | "just" | "task" | "ninja" => first.map_or(Some(Kind::Build), named),
        "pytest" | "vitest" | "jest" | "playwright" | "mocha" | "rspec" | "phpunit" | "ctest"
        | "tox" | "nox" | "unittest" => Some(Kind::Tests),
        "tsc" | "eslint" | "ruff" | "mypy" | "pyright" | "biome" | "oxlint" | "golangci-lint"
        | "shellcheck" | "rustfmt" | "prettier" | "svelte-check" | "stylelint" => Some(Kind::Lint),
        "xcodebuild" | "swiftc" | "rustc" | "gcc" | "clang" | "cmake" | "webpack" | "esbuild" => {
            Some(Kind::Build)
        }
        // A script of the project's own, named for what it does.
        _ if program.contains('/') || name.ends_with(".sh") => named(name),
        _ => None,
    }
}

/// What follows the first `word` in `args`, or nothing.
fn after<'a>(args: &'a [&'a str], word: &str) -> &'a [&'a str] {
    args.iter()
        .position(|a| *a == word)
        .map_or(&[], |at| &args[at + 1..])
}

/// The kind of check the program `args` start with, past their flags.
fn program_in(args: &[&str]) -> Option<Kind> {
    let at = args.iter().position(|a| !a.starts_with('-'))?;
    runs(args[at], &args[at + 1..])
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

    fn said(message: &str) -> Vec<Kind> {
        claimed(&sentences(message))
            .into_iter()
            .map(|(k, _)| k)
            .collect()
    }

    #[test]
    fn a_message_is_kept_as_its_sentences() {
        assert_eq!(
            sentences("Done. Bumped v1.2.3 too!\n\n- fmt: clean\nWhat next?"),
            ["Done.", "Bumped v1.2.3 too!", "- fmt: clean", "What next?"]
        );
        let long = "Word. ".repeat(SENTENCES + 5);
        assert_eq!(sentences(&long).len(), SENTENCES);
    }

    /// Sign-offs the way agents write them.
    #[test]
    fn claims_are_what_a_message_says_passed() {
        use Kind::*;
        assert_eq!(said("All 1289 tests pass."), [Tests]);
        assert_eq!(said("- **Tests:** 42 passed, 0 failed"), [Tests]);
        assert_eq!(
            said("cargo test is green and clippy is clean."),
            [Tests, Lint]
        );
        assert_eq!(said("It compiles. The test suite passes."), [Build, Tests]);
        assert_eq!(said("Build succeeds with no warnings"), [Build]);
        assert_eq!(said("Clippy is clean, but two tests fail."), [Lint]);
        assert_eq!(said("tsc reports no errors."), [Lint]);
        // Not claims: failures, negations, plans, conditions, questions.
        for not_a_claim in [
            "Two tests fail.",
            "I didn't run the tests.",
            "I couldn't build it: the SDK is missing.",
            "The tests should pass now.",
            "Run cargo test to check it passes.",
            "Once CI is green, merge it.",
            "Do the tests pass for you?",
            "Fixed the failing test.",
            "Added a test for the parser.",
            "This is the latest version, which works.",
        ] {
            assert_eq!(said(not_a_claim), [], "{not_a_claim}");
        }
        // Each kind once, from the first sentence that claims it.
        let claims = claimed(&sentences("Tests pass. Also, all tests pass."));
        assert_eq!(claims, [(Tests, "Tests pass.".to_owned())]);
    }

    #[test]
    fn commands_are_known_by_what_they_check() {
        use Kind::*;
        for (command, kinds) in [
            ("cargo test --offline", vec![Tests]),
            (
                "cd crc && cargo build && cargo clippy -- -D warnings",
                vec![Build, Lint],
            ),
            ("RUST_LOG=debug time cargo nextest run", vec![Tests]),
            ("npm test", vec![Tests]),
            ("bun run typecheck", vec![Lint]),
            ("pnpm build", vec![Build]),
            ("npx vitest run", vec![Tests]),
            ("uv run pytest -q", vec![Tests]),
            ("python3 -m pytest tests/", vec![Tests]),
            ("go vet ./...", vec![Lint]),
            ("make", vec![Build]),
            ("make test", vec![Tests]),
            ("scripts/selftest.sh --jobs 4", vec![Tests]),
            ("cargo test 2>&1 | tail -20", vec![Tests]),
            ("tsc --noEmit", vec![Lint]),
            // A test named, not run.
            ("git commit -m 'cargo test fix'", vec![]),
            ("grep -rn 'fn test' src", vec![]),
            ("echo pytest", vec![]),
            ("ls tests", vec![]),
            ("npm run dev", vec![]),
        ] {
            assert_eq!(kinds_of(command), kinds, "{command}");
        }
    }

    fn ran(command: &str, outcome: Outcome) -> Ran {
        Ran {
            at: 0,
            outcome,
            command: command.into(),
        }
    }

    fn verdicts(receipt: &Receipt) -> Vec<(Kind, String)> {
        receipt
            .claims
            .iter()
            .map(|c| (c.kind, c.verdict.label()))
            .collect()
    }

    #[test]
    fn a_claim_stands_on_the_newest_command_of_its_kind_this_turn() {
        use Kind::*;
        let mut receipt = Receipt::default();
        // Last turn's passing run backs nothing this turn.
        receipt.note(ran("cargo clippy", Outcome::Passed));
        receipt.note_prompt();
        receipt.note(ran("cargo test", Outcome::Failed(101)));
        receipt.note(ran("cargo test", Outcome::Passed));
        receipt.note(ran("cargo build", Outcome::Failed(1)));
        receipt.note_said(&sentences("All tests pass. It builds. Clippy is clean."));
        assert_eq!(
            verdicts(&receipt),
            [
                (Tests, "checked".into()),
                (Build, "exit 1".into()),
                (Lint, "not run".into()),
            ]
        );
        assert_eq!(receipt.claims[0].verdict.command(), Some("cargo test"));

        // A file written after the passing run.
        receipt.note_prompt();
        assert!(receipt.claims.is_empty());
        receipt.note(ran("cargo test", Outcome::Passed));
        receipt.note_wrote();
        receipt.note(ran("npm test", Outcome::Background));
        receipt.note_said(&sentences("Tests pass."));
        assert_eq!(verdicts(&receipt), [(Tests, "before last edit".into())]);

        // Only a background run: its outcome was never seen.
        receipt.note_prompt();
        receipt.note(ran("npm test", Outcome::Background));
        receipt.note_said(&sentences("Tests pass."));
        assert_eq!(verdicts(&receipt), [(Tests, "unseen".into())]);
        assert!(receipt.shown());

        // Claims with nothing run still show the card.
        let mut bare = Receipt::default();
        assert!(!bare.shown());
        bare.note_said(&sentences("All tests pass."));
        assert!(bare.shown());
        assert_eq!(verdicts(&bare), [(Tests, "not run".into())]);
    }
}
