# Dependency review

Reviewed 2026-09-18. Re-do this before adding, removing or bumping anything.

## Why this file exists

The usual supply-chain controls (a minimum release age, refusing unreviewed
install scripts, exact pins) are npm-shaped and have **no cargo
equivalent**.

Cargo is also structurally worse than npm on the axis that matters here:

- `build.rs` runs arbitrary code at build time, by default. There is no
  `strict-allow-scripts` equivalent, no allowlist, and no `--ignore-scripts`.
- Proc macros execute arbitrary code at compile time and cannot be opted out
  of. They *are* the compiler at that point.
- crates.io has no `min-release-age`, and cargo has no native equivalent.

So `cargo build` on an unreviewed tree executes strictly more untrusted code
than `npm install --ignore-scripts` does.

## The controls actually in place here

1. **Exact pins.** Every dependency is `=x.y.z`. No caret ranges, so a
   malicious patch release cannot drift in on a rebuild.
2. **Vendored sources, built offline in CI.** `scripts/vendor.sh` writes the
   full source of every crate to `vendor/` and points `.cargo/config.toml` at
   it. Neither is committed (16MB against 132KB of source), so a fresh clone
   fetches from crates.io until that script has run. What makes the claim
   true where it matters is CI: every job that compiles anything vendors
   with `--locked`, which can only fetch what `Cargo.lock` names and verifies
   it against the checksums recorded there, and then builds with
   `CARGO_NET_OFFLINE=true`.
3. **Every build script read.** There is exactly one in the whole tree, and
   `scripts/check-build-scripts.sh` fails CI if that changes. The jobs that
   compile wait for it, so a pull request that adds a build script is stopped
   before `cargo test` would have run it.

   That check asks cargo, through `cargo metadata`, which build-script and
   proc-macro targets it would build. It went through two worse versions
   first. Globbing for files named `build.rs` missed crates that point
   `build` somewhere else: evaluating tree-sitter turned up two doing exactly
   that (`binding_rust/build.rs`, `bindings/rust/build.rs`), and the check
   reported success while missing both. Grepping manifests for a `build = `
   line fixed that and missed the opposite case, a crate with no `build` key
   that ships a `build.rs`, which cargo detects and runs on its own. A check
   that can be evaded is worse than no check, because it manufactures
   confidence. Cargo's own list cannot disagree with what cargo does.
4. **Minimal feature surface.** `default-features = false` everywhere. The
   objc2 crates gate each Objective-C class behind its own feature and the
   defaults enable all of them plus four sibling framework crates we never
   touch. Listing only what we use dropped the tree from 18 crates to 10.

5. **A pinned compiler.** `rust-toolchain.toml` names an exact release. It was
   the one input still floating.
6. **Pinned actions, read-only token.** Every GitHub Action is referenced by
   commit, and only GitHub's own are used. The workflow token has
   `contents: read` and checkouts do not persist it.
7. **Checksummed C.** The tree-sitter runtime and grammar in `third_party/`
   are compiled by our own `build.rs`, outside everything cargo knows about.
   `third_party/CHECKSUMS` pins each file, `third_party/SOURCES.md` names the
   upstream commit each tree is byte-identical to and the one deliberate
   difference, and `scripts/check-third-party.sh` verifies both in CI.

## The tree

10 crates, **1 build script, 0 proc macros**.

| crate | version | published | maintainers |
|---|---|---|---|
| objc2 | 0.6.4 | 2026-02-26 | simlay, madsmtm |
| objc2-app-kit | 0.3.2 | 2025-10-04 | simlay, madsmtm |
| objc2-core-foundation | 0.3.2 | 2025-10-04 | simlay, madsmtm |
| objc2-core-graphics | 0.3.2 | 2025-10-04 | simlay, madsmtm |
| objc2-core-text | 0.3.2 | 2025-10-04 | simlay, madsmtm |
| objc2-foundation | 0.3.2 | 2025-10-04 | simlay, madsmtm |
| objc2-metal | 0.3.2 | 2025-10-04 | simlay, madsmtm |
| objc2-quartz-core | 0.3.2 | 2025-10-04 | simlay, madsmtm |
| objc2-encode | 4.1.0 | — | simlay, madsmtm |
| bitflags | 2.13.2 | — | cuviper, KodrAus |

All well past any sane minimum release age. The objc2 download counts are in
the tens of millions because winit and wgpu sit on top of them, so this code
is exercised by most of the Rust-on-Apple ecosystem.

## Build scripts

### `objc2-0.6.4/build.rs` — reviewed, benign

39 lines. Reads `TARGET` and `CARGO_CFG_TARGET_ABI`, prints `cargo:rustc-cfg`
lines to distinguish Mac Catalyst and simulator targets. No filesystem access,
no network, no subprocesses.

## Things deliberately not depended on

- **`crop` / `ropey`** (rope). `crop` is single-maintainer with 341K lifetime
  downloads and nothing published in 510 days. But the deciding reason is not
  supply chain: the rope's API shape dictates undo, multi-cursor, syntax-tree
  sync and CRDT merging, so it is core to the product rather than plumbing.
  Ours is in `src/text/rope.rs`.
- **`swash` / `fontdue`** (rasterization). CoreText ships with the OS, handles
  hinting and complex-script shaping better than we would, and costs nothing.
- **`wgpu` / `winit`**. Would have been ~200 crates and ~40 build scripts for
  portability we are not buying. See the macOS-only decision.

## Unbound system APIs

`src/render/font.rs` declares a handful of CoreGraphics externs the objc2
bindings do not generate (`CGBitmapContextCreate`, `CGBitmapContextGetData`,
`CGColorSpaceCreateDeviceGray`, and a few context setters). These are stable
system APIs decades old. Declaring them links against the OS; it does not add
a dependency.

## Adding a dependency

1. Check publish date, maintainers, download counts against crates.io.
   `scripts/cargo-vet.sh` does this.
2. Add with an exact `=` pin and `default-features = false`.
3. `cargo vendor --versioned-dirs`.
4. `find vendor -maxdepth 2 -name build.rs` and **read every new one**.
5. `grep -l "proc-macro = true" vendor/*/Cargo.toml` — anything new here runs
   at compile time and needs the same scrutiny as a build script.
6. Run `scripts/check-build-scripts.sh` and update its allowlist deliberately.
7. Update this file.

## tree-sitter: vendored C, not the crate

Adopted 2026-09-18 by compiling the C directly, in `third_party/`, with our
own `build.rs`. Cost:

| | before | vendored C | the crate |
|---|---|---|---|
| crates | 10 | **14** | 29 |
| vendored build scripts | 1 | **1** | 10 |
| proc macros | 0 | **0** | 1 |

The three new crates are `cc` and its two dependencies. `cc` is pinned to
`=1.4.5`, the newest release that was at least 7 days old when it was added:
1.4.7 was published that same day and 1.4.6 five days earlier, both inside
a seven-day minimum release age, which npm can enforce and cargo cannot.

`build.rs` in the project root is ours. It hands three C files to `cc` and
emits a static library: no network, no code generation, no writes outside
`OUT_DIR`. It is not covered by `scripts/check-build-scripts.sh`, which only
inspects vendored crates, because it is in-repo and reviewable in a diff.

What is vendored:

- `third_party/tree-sitter` — 24K lines of hand-written C. `lib.c` includes
  every other `.c`, so the runtime is one translation unit, and `wasm_store.c`
  sits inside an `#ifdef` we never define, which is what keeps wasmtime out.
- `third_party/tree-sitter-rust` — 207K lines, of which 206K is a single
  generated `parser.c`. That is a parse table, not logic. Reviewing it by
  reading is not meaningful; the way to verify it is to regenerate it from
  the grammar with `tree-sitter-cli` and diff. **This is the real cost of
  this approach, and it recurs per language.**

Both are MIT, which is GPL-3.0 compatible.

FFI is hand-written in `src/syntax/ffi.rs`, about twenty functions, declared
against the checked-in `api.h`. No `bindgen`, which would have pulled in
libclang.

## Evaluated and not adopted

### tree-sitter as a crate (0.27.0), evaluated 2026-09-18

Wanted for syntax highlighting. Measured cost, on top of the current tree:

| | before | with tree-sitter |
|---|---|---|
| crates | 10 | 29 |
| build scripts | 1 | 10 |
| proc macros | 0 | 1 (`serde_derive`) |
| C files compiled | 0 | 49 |

`serde_json` is a *build-dependency* of tree-sitter, which drags `serde`,
`serde_derive`, `proc-macro2`, `quote` and `syn` into the build graph. All of
those compile and execute during `cargo build`. `regex` arrives as a normal
dependency of the runtime.

Not a rejection of tree-sitter on quality grounds: it is the right parser.
But it roughly triples the crate count and multiplies build-time code
execution by ten, which is a decision to take explicitly rather than absorb
as a side effect of wanting coloured keywords.
