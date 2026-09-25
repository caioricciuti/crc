# Contributing

Open an issue first for anything bigger than a small fix, so the approach
can be agreed before the work. Bug reports are most useful with the steps to
reproduce, the output of `crc --version` and a screenshot.

## Build and test

```
cargo test --offline
cargo clippy --offline --all-targets -- -D warnings
cargo fmt --all --check
```

CI runs all three plus a headless render and a supply-chain gate. It only
builds on macOS, because Metal and CoreText are macOS APIs.

For editor changes, also run the real-window suite and latency gates:

```sh
scripts/selftest.sh
cargo run --release --offline --example frame_latency
cargo run --release --offline --example frame_latency -- --chrome
cargo run --release --offline --example long_line_latency
scripts/check-third-party.sh
scripts/check-build-scripts.sh
```

Use temporary repositories for Git regression tests. For a visual change,
look at a rendered frame or a capture of the real window, not only at the
tests; say in the pull request which checks you could not run.
Documentation-only changes need link and whitespace checks, not another
benchmark run.

## Dependencies

**Default to zero new dependencies.** This project has 14 crates, one vendored dependency build script,
and no proc macros, and CI fails if that changes without the allowlist being
updated deliberately.

That is not aesthetic minimalism. Cargo executes arbitrary code at build time
(`build.rs`) and at compile time (proc macros), with no allowlist, no
`--ignore-scripts` equivalent and no minimum release age. Every crate added is
a standing invitation to run code on every contributor's machine and in CI.

Before proposing one, open an issue with:

1. The publish date of the version that would install
2. Its maintainers and download counts (`scripts/cargo-vet.sh <crate>`)
3. Whether it has a build script or is a proc macro, and what that script does
4. Its transitive dependency count
5. What it would take to write the needed part ourselves

"Write the two hundred lines instead" is very often the right answer here. The
rope is in-tree for exactly this reason.

If a dependency is accepted, it goes in with an exact `=x.y.z` pin and
`default-features = false`, and `docs/dependency-review.md` gets updated in the
same PR.

## Performance claims

Anything asserting a change is faster needs a measurement, not a rationale.
`examples/frame_latency.rs` is the harness; add to it rather than benchmarking
in a scratch file.

The budget is 8.333 ms from buffer mutation through GPU completion with a
100 MiB file. It excludes event delivery and physical presentation. The dated
[README measurements](README.md#the-number) cover editor-only and native-chrome
runs; measure the relevant scope again for rendering changes rather than treating
an old result as a guarantee.

## Rendering changes

`examples/frame_dump.rs` renders a real frame offscreen through the same
device, pipeline, shader and layout as the live window, and writes it to a
BMP. CI runs it and uploads the result as an artifact.

Use it. Unit tests can tell you a glyph has ink in it; they cannot tell you
the baseline is three pixels off or the atlas is upside down. Both of those
happened during milestone 0 and only a dumped image caught them.

## Style

- Comments explain *why*, not *what*. If a comment restates the code, delete it.
- Name the trade-off when you make one. Future readers cannot see the options
  you rejected.
- Tests assert behaviour, not implementation. The rope's test suite compares
  against a `String` oracle over hundreds of random edits; that caught more
  than any hand-written case did.
- No `unsafe` without a comment saying what invariant makes it sound. The
  `objc2` bindings are mostly safe already; reach for `unsafe` only where they
  genuinely require it.

## Scope

Issues and PRs that move toward the three bets in the README (the SQLite
index, WASM extensions with declared capabilities, the agent-as-protocol-client
model) are the most valuable.

Before building something large, open an issue first. The architecture is
young enough that a wrong foundation is expensive.

## Licence

By contributing you agree your work is licensed under GPL-3.0-or-later, the
same as the project.
