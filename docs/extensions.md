# Extensions: design

Status: **the first version is built**: the interpreter, manifests and
capabilities for the selection and the document, the Extensions page,
installs from a folder and from the signed registry (which goes live with
its first release), and commands in the palette. Host functions beyond
`log`, and the capabilities after the document, come next. This is the
concrete version of [vision](vision.md) section 2: an extension is
WebAssembly with a manifest that says what it may read, write and reach,
and a host that enforces it. Installing one never means handing a stranger
your home folder.

## Constraints

- **No new trust in the build.** crc's dependency tree is small on purpose
  (see [dependency-review.md](dependency-review.md)). Whatever runs
  extensions is weighed against that.
- **No JIT.** A JIT needs writable and executable memory, which a hardened,
  notarized app only gets with the `allow-jit` entitlement. An interpreter
  needs nothing.
- **Extensions never block typing.** They run off the main thread, with a
  budget.
- **Built-ins stay built in.** Git, language servers and the index stay
  Rust in the app. Extensions are for what should not ship to everyone.

## The engine

A spike ran the same extension, "Sort Lines" written as ordinary Rust with
`std` and built for `wasm32-unknown-unknown` (a 22 KB module), on
[wasmi](https://github.com/wasmi-labs/wasmi) 2.0.0 and on a small
interpreter written for the purpose. Both produced the right output. Release
builds with LTO on Apple Silicon, median of 20 runs:

| | wasmi 2.0.0 (validation on) | own interpreter |
|---|---|---|
| New crates | 7, including 4 build scripts | 0 |
| Code | about 124,000 lines | about 1,100 lines |
| Binary size added | about 950 KB | about 33 KB |
| Load and instantiate | 0.25 ms | 0.29 ms |
| Sort 10,000 lines (451 KB) | 10.9 ms | 97 ms |
| Spec validation | full | none; every access checked instead |

**Decision: crc's own interpreter, behind a small `Engine` interface.**

- It adds no crates, no build scripts and a few thousand lines we read and
  own. wasmi would add seven crates and four build scripts, mostly from one
  maintainer, for speed the first extensions do not need.
- 97 ms is fine for a command. For work on every keystroke (a linter, a
  completion source), at about 3 ns per instruction a 20 ms budget is some
  six million instructions, off the main thread.
- If real extensions hit that ceiling, wasmi goes in behind the same
  interface, after the same dependency review as everything else.

How the interpreter stays safe without a spec validator: no `unsafe`; every
stack pop, index and memory access is checked and any inconsistency is a
trap, never a panic; calls run on an explicit frame stack with a depth cap,
so a module cannot overflow crc's own stack; memory is capped by the host;
fuel bounds the instructions a call may run. A malformed module can compute
garbage inside its own sandbox, and nothing outside it. A first mutation
fuzz (20,000 corrupted modules) produced no panics.

Before it ships: host imports, a long-running fuzz target, and the official
WebAssembly spec tests for every feature crc claims to support.

## A package

```
sort-lines/
  manifest.json
  sort_lines.wasm
  README.md          shown before install
```

```json
{
  "id": "crc.sort-lines",
  "name": "Sort Lines",
  "version": "0.1.0",
  "api": 1,
  "entry": "sort_lines.wasm",
  "capabilities": ["selection.read", "selection.replace"],
  "commands": [{ "id": "sort", "title": "Sort Lines", "export": "sort_lines" }]
}
```

Extensions are installed under
`~/Library/Application Support/crc/extensions/<id>/<version>/`, never inside
a project: cloning a repository must not be a way to install code. They are
enabled per user and can be turned off per project.

## Capabilities

The manifest declares them, crc shows them before install, and the host
enforces them: a host function the manifest does not grant is not linked at
all, so the module fails to load rather than failing later.

| Capability | Grants |
|---|---|
| `selection.read`, `document.read` | the selection, or the whole active document |
| `selection.replace`, `document.edit` | returning edits, applied by crc as one undo step |
| `index.query` | read-only queries against the project index |
| `diagnostics.publish` | underlines and status line counts, like a language server |
| `status.item` | a short text in the status bar |

Commands listed in the manifest appear in the palette.

**Not in the first version, on purpose:** network, files, processes,
clipboard, other documents, settings. Each will be its own capability, asked
for on its own, when an extension needs it.

### Network, when it comes

The first extensions that need the network will be hosting integrations
(pull requests, review comments, CI status). Access will be:

- through crc only: an extension asks the host to fetch, it never opens a
  socket;
- HTTPS only, to hosts named in the manifest (`"network": ["api.github.com"]`),
  with no wildcards;
- shown before install, and asked again whenever an update changes the list;
- without credentials: a token lives in crc's Keychain entry, and crc adds it
  to requests for the host it belongs to. The extension never sees it.

## How an extension talks to crc

- The module exports `memory`, `crc_alloc(len) -> ptr`, and one function per
  command.
- crc copies the input into memory from `crc_alloc` and calls the command,
  which returns `(ptr << 32) | len` pointing at a small versioned JSON result
  (edits, messages) in its own memory.
- Host functions are imported from the module `crc` (`crc.log`,
  `crc.index_query`, ...) and linked only when granted.
- `api` in the manifest is the version of this interface; crc refuses
  versions it does not know.
- A small `crc-extension` crate wraps all of this, so an extension is a
  plain Rust function over `&str`.

## Running

- One worker thread runs all extensions. Each call gets fuel and a
  wall-clock limit; each instance gets a memory cap.
- An instance stays loaded between calls, so an extension can keep state.
- Edits are applied on the main thread only if the document has not changed
  since the call, the rule crc already uses for formatting.
- A trap or an exhausted budget cancels the call and says so in the status
  line. Three in a row disable the extension until it is turned back on.

## Distribution and signing

- **Official extensions** live in
  [crc-extensions](https://github.com/caioricciuti/crc-extensions), are built
  from source in CI and signed in a protected release environment, the same
  way crc releases are approved. The registry is a signed static index in
  that repository. There is no server.
- **Signatures** are ECDSA P-256, verified with Security.framework, which
  macOS ships. The public key is in the app. No hand-written cryptography
  and no new dependency.
- **Other extensions** install from a file or a URL, unsigned, after a
  warning and the full list of capabilities.
- **Updates are never automatic.** An update that asks for more shows the
  difference and needs a new yes.

## Order of work

1. The interpreter in crc with its tests, fuzz target and host imports.
2. Manifests, installing from a folder, the capability prompt.
3. Extension commands in the palette, run on the worker, edits as one undo
   step. "Sort Lines" as the first official extension.
4. Signing and the registry index.
