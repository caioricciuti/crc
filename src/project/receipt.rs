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
    /// How many tests it passed and failed, for a test command whose
    /// output said so.
    pub tests: Option<TestCounts>,
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

/// How many tests a run passed and failed, as its runner printed them.
/// Only the counts are kept, never the output they came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TestCounts {
    pub passed: u32,
    pub failed: u32,
}

impl TestCounts {
    /// How it is kept in the events file: `passed/failed`.
    pub fn code(self) -> String {
        format!("{}/{}", self.passed, self.failed)
    }

    pub fn from_code(code: &str) -> Option<TestCounts> {
        let (passed, failed) = code.split_once('/')?;
        Some(TestCounts {
            passed: passed.parse().ok()?,
            failed: failed.parse().ok()?,
        })
    }

    /// A few words, for the receipt's rows: `12 passed, 1 failed`.
    pub fn label(self) -> String {
        match (self.passed, self.failed) {
            (passed, 0) => format!("{passed} passed"),
            (0, failed) => format!("{failed} failed"),
            (passed, failed) => format!("{passed} passed, {failed} failed"),
        }
    }
}

/// The test counts a runner's output reports: the `test result:` lines of
/// cargo test, added up, or nextest's summary; the `Tests` line of vitest
/// or jest; pytest's closing line; go test's `--- PASS` and `--- FAIL`
/// lines, or with no `-v` its `ok` packages as the passed. `None` when the
/// output has none of them.
pub fn test_counts(output: &str) -> Option<TestCounts> {
    let output = without_escapes(output);
    let lines: Vec<&str> = output.lines().map(str::trim).collect();
    cargo_counts(&lines)
        .or_else(|| js_counts(&lines))
        .or_else(|| pytest_counts(&lines))
        .or_else(|| go_counts(&output))
}

/// The text with its terminal escapes (colours, mostly) taken out.
fn without_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

/// The number right before `word` in `text`, words split at anything
/// that is not a letter or a digit: `12` for `passed` in `12 passed; 1
/// failed`.
fn count_before(text: &str, word: &str) -> Option<u32> {
    let words: Vec<&str> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    words
        .windows(2)
        .find(|pair| pair[1] == word)
        .and_then(|pair| pair[0].parse().ok())
}

/// `test result: ok. 12 passed; 1 failed; ...`, once per test binary, or
/// nextest's `12 tests run: 11 passed, 1 failed, 0 skipped`.
fn cargo_counts(lines: &[&str]) -> Option<TestCounts> {
    let mut found: Option<TestCounts> = None;
    for line in lines.iter().filter(|l| l.starts_with("test result:")) {
        let total = found.get_or_insert_default();
        total.passed += count_before(line, "passed").unwrap_or(0);
        total.failed += count_before(line, "failed").unwrap_or(0);
    }
    found.or_else(|| {
        let line = lines.iter().rev().find(|l| l.contains(" tests run: "))?;
        let (_, rest) = line.split_once(" tests run: ")?;
        Some(TestCounts {
            passed: count_before(rest, "passed").unwrap_or(0),
            failed: count_before(rest, "failed").unwrap_or(0),
        })
    })
}

/// Jest's `Tests:       1 failed, 12 passed, 13 total`, or vitest's
/// `Tests  1 failed | 12 passed (13)`; not vitest's `Test Files` line.
fn js_counts(lines: &[&str]) -> Option<TestCounts> {
    lines.iter().rev().find_map(|line| {
        let rest = line.strip_prefix("Tests")?;
        if !rest.starts_with([':', ' ', '\t']) {
            return None;
        }
        let passed = count_before(rest, "passed");
        let failed = count_before(rest, "failed");
        (passed.is_some() || failed.is_some()).then(|| TestCounts {
            passed: passed.unwrap_or(0),
            failed: failed.unwrap_or(0),
        })
    })
}

/// What pytest's closing line may count, besides `passed` and `failed`.
const PYTEST_WORDS: &[&str] = &[
    "passed",
    "failed",
    "error",
    "errors",
    "skipped",
    "xfailed",
    "xpassed",
    "warning",
    "warnings",
    "deselected",
    "rerun",
];

/// pytest's closing line, `=== 3 passed, 1 failed in 0.20s ===`, or with
/// `-q` the same without the rules. An error counts as a failure.
fn pytest_counts(lines: &[&str]) -> Option<TestCounts> {
    lines.iter().rev().find_map(|line| {
        let (counts, _) = line.trim_matches(['=', ' ']).split_once(" in ")?;
        let mut found = TestCounts::default();
        let mut any = false;
        for part in counts.split(", ") {
            let (n, word) = part.split_once(' ')?;
            let n: u32 = n.parse().ok()?;
            if !PYTEST_WORDS.contains(&word) {
                return None;
            }
            match word {
                "passed" => found.passed += n,
                "failed" | "error" | "errors" => found.failed += n,
                _ => {}
            }
            any = true;
        }
        any.then_some(found)
    })
}

/// go test: with `-v`, a `--- PASS` or `--- FAIL` line per test (subtests
/// are indented and not counted); without it, only the failures are named,
/// so each `ok` package counts as one passed. A package that did not build
/// counts as one failed when no test failed.
fn go_counts(output: &str) -> Option<TestCounts> {
    let (mut pass, mut fail, mut ok, mut broken) = (0, 0, 0, 0);
    for line in output.lines() {
        if line.starts_with("--- PASS: ") {
            pass += 1;
        } else if line.starts_with("--- FAIL: ") {
            fail += 1;
        } else if line.starts_with("ok  ") || line.starts_with("ok\t") {
            ok += 1;
        } else if line.starts_with("FAIL\t") {
            broken += 1;
        }
    }
    if pass + fail + ok + broken == 0 {
        return None;
    }
    Some(TestCounts {
        passed: if pass > 0 { pass } else { ok },
        failed: if fail > 0 { fail } else { broken },
    })
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
    /// The package installs its commands ran, in the order they ran, each
    /// once, the newest [`KEEP`] of them.
    pub installs: Vec<Installed>,
}

impl Receipt {
    pub fn note(&mut self, ran: Ran) {
        self.total += 1;
        if ran.outcome.failed() {
            self.failed += 1;
        }
        for install in installs_of(&ran.command) {
            self.note_install(Installed {
                install,
                outcome: ran.outcome,
                command: ran.command.clone(),
            });
        }
        if self.ran.len() == KEEP {
            self.ran.pop_front();
        }
        self.ran.push_back(ran);
    }

    /// An install, unless the same one, ended the same way, is listed.
    fn note_install(&mut self, installed: Installed) {
        if self.installs.iter().any(|i| i.same(&installed)) {
            return;
        }
        if self.installs.len() == KEEP {
            self.installs.remove(0);
        }
        self.installs.push(installed);
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
                let seen = of_kind
                    .clone()
                    .find(|(_, r)| r.outcome != Outcome::Background);
                let tests = seen.and_then(|(_, ran)| ran.tests);
                let verdict = match seen {
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
                    tests,
                }
            })
            .collect();
    }

    /// Takes in the receipt of a later session: the counts add up, the
    /// commands go in the order they ran, the newest [`KEEP`] of them, its
    /// installs after these, each once, and its claims stand when it made
    /// any. A turn does not span sessions, so what is claimed next is
    /// checked against nothing merged here.
    pub fn merge(&mut self, newer: Receipt) {
        self.total += newer.total;
        self.failed += newer.failed;
        for installed in newer.installs {
            self.note_install(installed);
        }
        let mut ran: Vec<Ran> = self.ran.drain(..).chain(newer.ran).collect();
        ran.sort_by_key(|r| r.at);
        let skip = ran.len().saturating_sub(KEEP);
        self.ran = ran.into_iter().skip(skip).collect();
        if !newer.claims.is_empty() {
            self.claims = newer.claims;
        }
        self.turn = self.total;
        self.wrote = None;
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
    /// The test counts of the command its verdict rests on, when it
    /// printed them.
    pub tests: Option<TestCounts>,
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
/// passed: a failure, a negation, a plan, a condition. Any word ending in
/// "n't" (didn't, weren't, shouldn't) is a negation too; see
/// [`not_a_claim`].
const NOT_A_CLAIM: &[&str] = &[
    "not", "no", "never", "without", "cannot", "unable", "skip", "skipped", "fail", "fails",
    "failing", "failed", "failure", "failures", "broken", "error", "errors", "should", "would",
    "could", "will", "might", "may", "once", "until", "if", "unless", "before", "need", "needs",
    "try", "please", "let", "run", "rerun",
];

/// Whether `word`, lowercased, keeps the clause it is in from being a claim.
fn not_a_claim(word: &str) -> bool {
    NOT_A_CLAIM.contains(&word) || word.ends_with("n't")
}

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
            if words.iter().any(|w| not_a_claim(w)) {
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

/// A package install a command ran: the tool and what it was asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Install {
    /// The program and its verb as crc names them: `npm install`, `cargo
    /// add`, `uv pip install`.
    pub tool: String,
    /// The packages named, as typed, flags left out. None for an install
    /// from a lockfile or a manifest.
    pub packages: Vec<String>,
}

/// Package managers whose install with no packages is from the lockfile.
const LOCKFILE_TOOLS: &[&str] = &["npm", "pnpm", "yarn", "bun"];

impl Install {
    /// A few words, for the receipt card: `npm install left-pad`, or `npm
    /// ci (lockfile)`.
    pub fn label(&self) -> String {
        let program = self.tool.split(' ').next().unwrap_or_default();
        if !self.packages.is_empty() {
            format!("{} {}", self.tool, self.packages.join(" "))
        } else if LOCKFILE_TOOLS.contains(&program) {
            format!("{} (lockfile)", self.tool)
        } else {
            self.tool.clone()
        }
    }
}

/// An install with how the command that ran it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    pub install: Install,
    pub outcome: Outcome,
    /// The whole command it came from.
    pub command: String,
}

impl Installed {
    /// The same install, ended the same way, whatever command it was in.
    fn same(&self, other: &Installed) -> bool {
        self.install == other.install && self.outcome == other.outcome
    }
}

/// Programs that run the command after them.
const WRAPPERS: &[&str] = &["sudo", "time", "command", "env", "nice", "nohup", "exec"];

/// Flags of npm that take the next word.
const NPM_VALUES: &[&str] = &["-w", "--workspace", "--prefix", "--registry"];
/// Flags of pnpm, yarn and bun that take the next word; their `-w` takes
/// none.
const JS_VALUES: &[&str] = &["-F", "--filter", "-C", "--dir", "--cwd", "--registry"];
/// Flags of `cargo add` and `cargo install` that take the next word.
const CARGO_VALUES: &[&str] = &[
    "-p",
    "--package",
    "-F",
    "--features",
    "--rename",
    "--path",
    "--git",
    "--branch",
    "--tag",
    "--rev",
    "--registry",
    "--index",
    "--manifest-path",
    "--target",
    "--target-dir",
    "--version",
    "--vers",
    "--root",
    "--bin",
    "--example",
    "--profile",
    "-j",
    "--jobs",
    "--config",
    "-Z",
];
/// Flags of pip and uv that take the next word.
const PY_VALUES: &[&str] = &[
    "-r",
    "--requirement",
    "-c",
    "--constraint",
    "-e",
    "--editable",
    "-i",
    "--index-url",
    "--extra-index-url",
    "--index",
    "--default-index",
    "-f",
    "--find-links",
    "-t",
    "--target",
    "--prefix",
    "--root",
    "-p",
    "--python",
    "--group",
    "--optional",
    "--extra",
    "--package",
    "--with",
];

/// The package installs a command runs: each part of a list or pipeline
/// that is npm, pnpm, yarn or bun adding or installing, cargo adding or
/// installing, pip or uv installing, or brew installing. Anything in front
/// of the program that only sets it up (an environment variable, `sudo`,
/// `time`) is passed over; a separator inside quotes does not start a
/// part, and a program in quotes is not run.
pub fn installs_of(command: &str) -> Vec<Install> {
    command_parts(command)
        .iter()
        .filter_map(|part| install_in(part))
        .collect()
}

/// The parts of a command list or pipeline, split at `&&`, `||`, `;`,
/// `|`, `&` and new lines outside quotes; a `&` in a redirect (`2>&1`)
/// does not split.
fn command_parts(command: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut prev = ' ';
    for c in command.chars() {
        if escaped {
            escaped = false;
            part.push(c);
        } else if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '\\' && q == '"' {
                escaped = true;
            }
            part.push(c);
        } else {
            match c {
                '\\' => {
                    escaped = true;
                    part.push(c);
                }
                '\'' | '"' => {
                    quote = Some(c);
                    part.push(c);
                }
                ';' | '|' | '\n' => parts.push(std::mem::take(&mut part)),
                '&' if !matches!(prev, '>' | '<') => parts.push(std::mem::take(&mut part)),
                _ => part.push(c),
            }
        }
        prev = c;
    }
    parts.push(part);
    parts
}

/// Whether `word` sets an environment variable: `NAME=value`.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// The install one part of a command runs, if it is one.
fn install_in(part: &str) -> Option<Install> {
    let words: Vec<&str> = part.split_whitespace().collect();
    // Past what only sets the program up: variables, wrappers and their
    // flags.
    let mut at = 0;
    let mut wrapped = false;
    while let Some(word) = words.get(at) {
        if WRAPPERS.contains(word) {
            wrapped = true;
        } else if !is_assignment(word) && !(wrapped && word.starts_with('-')) {
            break;
        }
        at += 1;
    }
    let program = *words.get(at)?;
    let name = program.rsplit('/').next().unwrap_or(program);
    // The words given to it, up to a redirect.
    let args: Vec<&str> = words[at + 1..]
        .iter()
        .copied()
        .take_while(|w| w.starts_with(['"', '\'']) || !w.contains(['>', '<']))
        .collect();
    // Its verb: the first word not a flag, a flag's value, nor a
    // `+toolchain`.
    let globals = match name {
        "npm" => NPM_VALUES,
        "pnpm" | "yarn" | "bun" => JS_VALUES,
        "cargo" => &["-C", "-Z", "--config"],
        "uv" => &["--directory", "--project", "-p", "--python"],
        _ => &[],
    };
    let mut verb_at = 0;
    while let Some(arg) = args.get(verb_at) {
        if !arg.starts_with(['-', '+']) {
            break;
        }
        verb_at += if globals.contains(arg) { 2 } else { 1 };
    }
    args.get(verb_at)?;
    let (verb, rest) = (args[verb_at], &args[verb_at + 1..]);
    let (tool, rest, values): (String, &[&str], &[&str]) = match (name, verb) {
        ("npm", "install" | "i" | "in" | "add") => ("npm install".into(), rest, NPM_VALUES),
        ("npm", "ci") => ("npm ci".into(), rest, NPM_VALUES),
        ("pnpm", "add") | ("yarn", "add") | ("bun", "add" | "a") => {
            (format!("{name} add"), rest, JS_VALUES)
        }
        ("pnpm" | "yarn" | "bun", "install" | "i") => (format!("{name} install"), rest, JS_VALUES),
        ("cargo", "add" | "install") => (format!("cargo {verb}"), rest, CARGO_VALUES),
        ("pip" | "pip3", "install") => (format!("{name} install"), rest, PY_VALUES),
        ("uv", "add") => ("uv add".into(), rest, PY_VALUES),
        ("uv", "pip" | "tool") => {
            let next = rest.iter().position(|a| !a.starts_with('-'))?;
            if rest[next] != "install" {
                return None;
            }
            (format!("uv {verb} install"), &rest[next + 1..], PY_VALUES)
        }
        ("brew", "install") => ("brew install".into(), rest, &[]),
        _ => return python_pip(name, &args),
    };
    Some(Install {
        tool,
        packages: packages(rest, values),
    })
}

/// `python -m pip install`, as pip.
fn python_pip(name: &str, args: &[&str]) -> Option<Install> {
    if !matches!(name, "python" | "python3") {
        return None;
    }
    let ["-m", "pip", "install", rest @ ..] = args else {
        return None;
    };
    Some(Install {
        tool: "pip install".into(),
        packages: packages(rest, PY_VALUES),
    })
}

/// The package names among an install's arguments: not its flags, nor the
/// word a flag in `values` takes. Quotes around a name are taken off.
fn packages(args: &[&str], values: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut skip = false;
    for arg in args {
        if std::mem::take(&mut skip) {
            continue;
        }
        if arg.starts_with('-') {
            skip = values.contains(arg);
            continue;
        }
        let name = arg.trim_matches(['"', '\'']);
        if !name.is_empty() {
            out.push(name.to_owned());
        }
    }
    out
}

/// A commit made in the session, as a handoff lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitFact {
    /// Abbreviated, as `git log` prints it.
    pub hash: String,
    /// Git's `%G?`: `N` for no signature, `G` for a good one.
    pub signature: char,
    pub subject: String,
}

impl CommitFact {
    /// A word or two on its signature: unsigned only when it has none.
    pub fn signature_label(&self) -> &'static str {
        match self.signature {
            'N' => "unsigned",
            'G' | 'U' => "signed",
            'B' => "bad signature",
            _ => "signed, unverified",
        }
    }
}

/// What a session handed off, as plain text to paste into a chat or a pull
/// request: the claims checked, the commits made since it started
/// (`commits`, oldest first), the installs, the latest test counts of each
/// test command, the files written (`wrote`, shown under `folder` when they
/// are in it), and the commands that failed. A section with nothing in it
/// is left out; with nothing at all, the text is empty. No command's
/// output is in it: the receipt never keeps any.
pub fn handoff_text(
    receipt: &Receipt,
    commits: &[CommitFact],
    wrote: &[std::path::PathBuf],
    folder: Option<&std::path::Path>,
) -> String {
    let mut sections: Vec<(String, Vec<String>)> = Vec::new();
    let claims = receipt
        .claims
        .iter()
        .map(|c| match c.tests {
            Some(tests) => format!(
                "{}: {} ({})",
                c.kind.said(),
                c.verdict.label(),
                tests.label()
            ),
            None => format!("{}: {}", c.kind.said(), c.verdict.label()),
        })
        .collect();
    sections.push(("Claims checked".into(), claims));
    let commits = commits
        .iter()
        .map(|c| format!("{} ({}) {}", c.hash, c.signature_label(), c.subject))
        .collect();
    sections.push(("Commits".into(), commits));
    let installs = receipt
        .installs
        .iter()
        .map(|i| format!("{}: {}", i.install.label(), i.outcome.label()))
        .collect();
    sections.push(("Installs".into(), installs));
    // The newest counts of each test command, in the order they last ran.
    let mut tests: Vec<(&str, TestCounts)> = Vec::new();
    for ran in &receipt.ran {
        if let Some(counts) = ran.tests {
            tests.retain(|(command, _)| *command != ran.command);
            tests.push((&ran.command, counts));
        }
    }
    let tests = tests
        .iter()
        .map(|(command, counts)| format!("{command}: {}", counts.label()))
        .collect();
    sections.push(("Tests".into(), tests));
    let mut files: Vec<String> = Vec::new();
    for path in wrote {
        let shown = folder
            .and_then(|f| path.strip_prefix(f).ok())
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        if !files.contains(&shown) {
            files.push(shown);
        }
    }
    sections.push(("Files written".into(), files));
    // Each failure once, with how many times it ended that way.
    let mut failed: Vec<(&str, Outcome, usize)> = Vec::new();
    for ran in receipt.ran.iter().filter(|r| r.outcome.failed()) {
        match failed
            .iter_mut()
            .find(|(c, o, _)| *c == ran.command && *o == ran.outcome)
        {
            Some((.., times)) => *times += 1,
            None => failed.push((&ran.command, ran.outcome, 1)),
        }
    }
    let failed = failed
        .iter()
        .map(|(command, outcome, times)| match times {
            1 => format!("{}: {command}", outcome.label()),
            n => format!("{}: {command} ({n} times)", outcome.label()),
        })
        .collect();
    sections.push(("Failed commands".into(), failed));
    sections.retain(|(_, lines)| !lines.is_empty());
    if sections.is_empty() && receipt.total == 0 {
        return String::new();
    }
    let commands = match receipt.total {
        0 => "no commands".to_owned(),
        1 => "1 command".to_owned(),
        n => format!("{n} commands"),
    };
    let mut text = match (receipt.total, receipt.failed) {
        (0, _) => format!("Receipt: {commands} ran\n"),
        (_, 0) => format!("Receipt: {commands} ran, none failed\n"),
        (_, failed) => format!("Receipt: {commands} ran, {failed} failed\n"),
    };
    for (title, lines) in sections {
        // The tests are per command, not counted.
        if title == "Tests" {
            text.push_str(&format!("\n{title}\n"));
        } else {
            text.push_str(&format!("\n{title}: {}\n", lines.len()));
        }
        for line in lines {
            text.push_str(&format!("- {line}\n"));
        }
    }
    text
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
                tests: None,
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
            "The tests weren't passing.",
            "The build shouldn't fail now.",
            "Clippy hadn't passed before this change.",
            "It mustn't pass CI until the fixture is in.",
            "The tests needn't pass for this.",
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
            tests: None,
        }
    }

    fn counts(passed: u32, failed: u32) -> Option<TestCounts> {
        Some(TestCounts { passed, failed })
    }

    #[test]
    fn cargo_test_counts_add_up_over_its_binaries() {
        let output = "\
running 12 tests
test parse::one ... ok
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

running 3 tests
test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.20s

   Doc-tests crc
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
";
        assert_eq!(test_counts(output), counts(14, 1));
        // As a failed run's error carries it, coloured.
        let error = "Exit code 101\n\u{1b}[31mtest result: FAILED.\u{1b}[0m 12 passed; 1 failed; \
                     0 ignored\nerror: test failed, to rerun pass `--lib`";
        assert_eq!(test_counts(error), counts(12, 1));
        let nextest = "        PASS [   0.004s] crc parse::one\n     Summary [   0.5s] \
                       12 tests run: 11 passed, 1 failed, 0 skipped";
        assert_eq!(test_counts(nextest), counts(11, 1));
    }

    #[test]
    fn vitest_and_jest_counts_come_from_their_tests_line() {
        let jest = "\
FAIL src/sum.test.js
Test Suites: 1 failed, 2 passed, 3 total
Tests:       1 failed, 12 passed, 13 total
Snapshots:   0 total
Time:        1.2 s";
        assert_eq!(test_counts(jest), counts(12, 1));
        let vitest = "\
 Test Files  1 failed | 2 passed (3)
      Tests  1 failed | 12 passed (13)
   Start at  10:00:00
   Duration  420ms";
        assert_eq!(test_counts(vitest), counts(12, 1));
        assert_eq!(
            test_counts(" Test Files  3 passed (3)\n      Tests  20 passed (20)"),
            counts(20, 0)
        );
    }

    #[test]
    fn pytest_counts_come_from_its_closing_line() {
        let output = "\
============================= test session starts ==============================
collected 4 items

tests/test_parse.py ...F                                                 [100%]
=========================== short test summary info ============================
FAILED tests/test_parse.py::test_empty - AssertionError
========================= 3 passed, 1 failed in 0.20s ==========================";
        assert_eq!(test_counts(output), counts(3, 1));
        assert_eq!(
            test_counts("....\n4 passed, 2 warnings in 0.05s"),
            counts(4, 0)
        );
        assert_eq!(
            test_counts("=== 1 failed, 5 passed, 1 error, 2 skipped in 65.12s (0:01:05) ==="),
            counts(5, 2)
        );
    }

    #[test]
    fn go_test_counts_tests_or_packages() {
        let verbose = "\
=== RUN   TestParse
--- PASS: TestParse (0.00s)
=== RUN   TestTable
    --- PASS: TestTable/empty (0.00s)
    --- FAIL: TestTable/long (0.00s)
--- FAIL: TestTable (0.00s)
=== RUN   TestLex
--- PASS: TestLex (0.00s)
FAIL
FAIL\texample.com/parse\t0.012s
ok  \texample.com/lex\t0.004s";
        assert_eq!(test_counts(verbose), counts(2, 1));
        let quiet = "\
--- FAIL: TestTable (0.00s)
    table_test.go:12: got 1, want 2
FAIL
FAIL\texample.com/parse\t0.012s
ok  \texample.com/lex\t0.004s
ok  \texample.com/util\t(cached)
?   \texample.com/cmd\t[no test files]";
        assert_eq!(test_counts(quiet), counts(2, 1));
        assert_eq!(
            test_counts(
                "# example.com/parse\nparse.go:3:1: syntax error\nFAIL\texample.com/parse [build failed]"
            ),
            counts(0, 1)
        );
    }

    #[test]
    fn output_without_counts_has_none() {
        for output in [
            "",
            "ok",
            "Exit code 101\nerror: could not compile `crc`",
            "Compiling crc v0.1.0\n    Finished `test` profile",
            " Test Files  no tests",
            "Tests: 0 total",
            "no tests ran in 0.01s",
            "fixed the parser in 2 places",
            "took 3 passed in review",
        ] {
            assert_eq!(test_counts(output), None, "{output:?}");
        }
    }

    #[test]
    fn test_counts_survive_their_code_and_read_as_words() {
        for tests in [counts(12, 1), counts(0, 0), counts(u32::MAX, 3)] {
            let tests = tests.expect("counts");
            assert_eq!(TestCounts::from_code(&tests.code()), Some(tests));
        }
        assert_eq!(TestCounts::from_code("12"), None);
        assert_eq!(TestCounts::from_code("a/1"), None);
        let label = |p, f| counts(p, f).expect("counts").label();
        assert_eq!(label(12, 1), "12 passed, 1 failed");
        assert_eq!(label(12, 0), "12 passed");
        assert_eq!(label(0, 3), "3 failed");
    }

    #[test]
    fn a_claim_carries_the_counts_of_its_command() {
        let mut receipt = Receipt::default();
        receipt.note_prompt();
        receipt.note(Ran {
            tests: counts(12, 1),
            ..ran("cargo test", Outcome::Failed(101))
        });
        receipt.note(ran("cargo build", Outcome::Passed));
        receipt.note_said(&sentences("All tests pass. It builds."));
        let tests: Vec<_> = receipt.claims.iter().map(|c| c.tests).collect();
        assert_eq!(tests, [counts(12, 1), None]);
        // Merged into a workspace's card, the counts go with the command.
        let mut merged = Receipt::default();
        merged.merge(receipt);
        assert_eq!(merged.ran[0].tests, counts(12, 1));
        assert_eq!(merged.claims[0].tests, counts(12, 1));
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

    fn labels(command: &str) -> Vec<String> {
        installs_of(command).iter().map(Install::label).collect()
    }

    #[test]
    fn installs_are_known_per_tool() {
        for (command, label) in [
            ("npm install left-pad", "npm install left-pad"),
            (
                "npm i -D vitest@2 typescript",
                "npm install vitest@2 typescript",
            ),
            ("npm add -w web --save-exact zod", "npm install zod"),
            ("pnpm add -w -D eslint", "pnpm add eslint"),
            ("pnpm --filter web add zod", "pnpm add zod"),
            ("pnpm install", "pnpm install (lockfile)"),
            ("yarn add react react-dom", "yarn add react react-dom"),
            ("bun add -d @types/bun", "bun add @types/bun"),
            ("bun install --frozen-lockfile", "bun install (lockfile)"),
            ("cargo add serde --features derive", "cargo add serde"),
            ("cargo add -F derive serde", "cargo add serde"),
            (
                "cargo +nightly install ripgrep --locked",
                "cargo install ripgrep",
            ),
            ("cargo install --path .", "cargo install"),
            ("pip install requests==2.31", "pip install requests==2.31"),
            ("pip3 install --user -r requirements.txt", "pip3 install"),
            (
                "pip install 'requests>=2' -i https://example.com",
                "pip install requests>=2",
            ),
            ("python3 -m pip install httpx", "pip install httpx"),
            ("uv add --dev pytest", "uv add pytest"),
            ("uv pip install -e . ruff", "uv pip install ruff"),
            ("uv tool install ruff", "uv tool install ruff"),
            ("brew install --cask iterm2", "brew install iterm2"),
            ("/opt/homebrew/bin/brew install jq", "brew install jq"),
        ] {
            assert_eq!(labels(command), [label], "{command}");
        }
    }

    #[test]
    fn a_lockfile_install_is_listed_without_packages() {
        assert_eq!(labels("npm install"), ["npm install (lockfile)"]);
        assert_eq!(labels("npm ci"), ["npm ci (lockfile)"]);
        assert_eq!(
            labels("yarn install --immutable"),
            ["yarn install (lockfile)"]
        );
        assert!(installs_of("npm ci")[0].packages.is_empty());
    }

    #[test]
    fn installs_are_found_in_each_part_of_a_command() {
        assert_eq!(labels("cd web && npm install"), ["npm install (lockfile)"]);
        assert_eq!(
            labels("cd web && npm i zod; cargo add anyhow"),
            ["npm install zod", "cargo add anyhow"]
        );
        assert_eq!(
            labels("CI=1 NODE_ENV=development npm ci 2>&1 | tail -5"),
            ["npm ci (lockfile)"]
        );
        assert_eq!(
            labels("sudo -E pip install black > /dev/null"),
            ["pip install black"]
        );
        assert_eq!(
            labels("npm i zod || npm i zod@3"),
            ["npm install zod", "npm install zod@3"]
        );
    }

    #[test]
    fn what_is_not_an_install_is_not_listed() {
        for command in [
            "npm run install-thing",
            "npm test",
            "npm uninstall left-pad",
            "cargo install-update -a",
            "cargo build",
            "pnpm run add",
            "brew upgrade jq",
            "uv pip list",
            "pip download requests",
            "echo npm install left-pad",
            "git commit -m 'npm install left-pad'",
            "git commit -m \"fix; npm install left-pad\"",
            "sh -c 'npm install left-pad'",
            "grep -rn 'cargo add' docs",
        ] {
            assert_eq!(labels(command), Vec::<String>::new(), "{command}");
        }
    }

    #[test]
    fn a_receipt_lists_its_installs_once_and_merges_them_in_order() {
        let mut first = Receipt::default();
        first.note(ran("npm install left-pad", Outcome::Passed));
        first.note(ran("cargo test", Outcome::Passed));
        first.note(ran("pip install nope", Outcome::Failed(1)));
        first.note(ran("cd web && npm i left-pad", Outcome::Passed));
        let shown = |r: &Receipt| -> Vec<(String, bool)> {
            r.installs
                .iter()
                .map(|i| (i.install.label(), i.outcome.failed()))
                .collect()
        };
        assert_eq!(
            shown(&first),
            [
                ("npm install left-pad".into(), false),
                ("pip install nope".into(), true),
            ]
        );
        assert_eq!(first.installs[1].command, "pip install nope");

        let mut later = Receipt::default();
        later.note(ran("pip install nope", Outcome::Passed));
        later.note(ran("npm install left-pad", Outcome::Passed));
        later.note(ran("brew install jq", Outcome::Passed));
        first.merge(later);
        assert_eq!(
            shown(&first),
            [
                ("npm install left-pad".into(), false),
                ("pip install nope".into(), true),
                ("pip install nope".into(), false),
                ("brew install jq".into(), false),
            ]
        );
    }

    fn handoff_receipt() -> Receipt {
        let mut receipt = Receipt::default();
        receipt.note_prompt();
        receipt.note(ran("npm install left-pad", Outcome::Passed));
        receipt.note(Ran {
            tests: counts(10, 2),
            ..ran("cargo test", Outcome::Failed(101))
        });
        receipt.note(ran("cargo build", Outcome::Failed(1)));
        receipt.note(Ran {
            tests: counts(12, 1),
            ..ran("cargo test", Outcome::Failed(101))
        });
        receipt.note(Ran {
            tests: counts(4, 0),
            ..ran("npm test", Outcome::Passed)
        });
        receipt.note(ran("npm run dev", Outcome::Background));
        receipt.note_said(&sentences("All tests pass. It builds. Clippy is clean."));
        receipt
    }

    #[test]
    fn a_handoff_lists_every_section() {
        let commits = [
            CommitFact {
                hash: "1a2b3c4".into(),
                signature: 'N',
                subject: "Fix the parser".into(),
            },
            CommitFact {
                hash: "5d6e7f8".into(),
                signature: 'G',
                subject: "Add a test".into(),
            },
        ];
        let folder = std::path::Path::new("/w/proj");
        let wrote = [
            folder.join("src/parse.rs"),
            folder.join("README.md"),
            folder.join("src/parse.rs"),
            std::path::PathBuf::from("/elsewhere/notes.md"),
        ];
        let text = handoff_text(&handoff_receipt(), &commits, &wrote, Some(folder));
        assert_eq!(
            text,
            "\
Receipt: 6 commands ran, 3 failed

Claims checked: 3
- Said tests pass: checked (4 passed)
- Said it builds: exit 1
- Said lint is clean: not run

Commits: 2
- 1a2b3c4 (unsigned) Fix the parser
- 5d6e7f8 (signed) Add a test

Installs: 1
- npm install left-pad: ok

Tests
- cargo test: 12 passed, 1 failed
- npm test: 4 passed

Files written: 3
- src/parse.rs
- README.md
- /elsewhere/notes.md

Failed commands: 2
- exit 101: cargo test (2 times)
- exit 1: cargo build
"
        );
        assert!(!text.contains('\u{2014}'));
    }

    #[test]
    fn an_empty_handoff_is_empty() {
        assert_eq!(handoff_text(&Receipt::default(), &[], &[], None), "");
    }

    #[test]
    fn a_handoff_leaves_out_what_it_has_none_of() {
        // Commands that all passed, no tests, no installs, nothing written.
        let mut receipt = Receipt::default();
        receipt.note(ran("cargo build", Outcome::Passed));
        assert_eq!(
            handoff_text(&receipt, &[], &[], None),
            "Receipt: 1 command ran, none failed\n"
        );
        // Only a commit and a file, outside any folder.
        let commits = [CommitFact {
            hash: "abc1234".into(),
            signature: 'E',
            subject: "Tidy".into(),
        }];
        let text = handoff_text(
            &Receipt::default(),
            &commits,
            &[std::path::PathBuf::from("/a/b.rs")],
            None,
        );
        assert_eq!(
            text,
            "Receipt: no commands ran\n\nCommits: 1\n- abc1234 (signed, unverified) Tidy\n\nFiles written: 1\n- /a/b.rs\n"
        );
        for section in ["Claims", "Installs", "Tests", "Failed"] {
            assert!(!text.contains(section), "{section}");
        }
    }
}
