# Why crc exists

You spend more hours in your editor than in any other program. In 2026 it
is also the program most likely to hurt you. It runs code from hundreds of
strangers with your credentials, it hands your files to AI agents, and it
does both inside a web browser pretending to be a desktop app.

crc is a bet that an editor can be three things at once that today's
editors treat as trade-offs: **fast enough to disappear, small enough to
trust, and open enough for agents to work in without being let loose.**

## What is wrong with editors now

**They are browsers.** The most popular editors are built on Electron: a
copy of Chromium and Node.js under every keystroke. That is why they weigh
hundreds of megabytes before you open a file, why a big file can make typing
lag, and why "native" so often means a web page in a window.

**They trust everyone.** An extension in VS Code is arbitrary Node code with
full access to your filesystem and network, installed from a marketplace and
updated without asking. Your editor holds your SSH keys, your cloud tokens
and your source. Supply-chain attacks through package registries are now
routine. An editor that runs whatever
a stranger publishes has made itself the easiest way in.

**Their knowledge is locked away.** What an editor knows about your code,
where a symbol is defined and who calls it, lives in a language server's
memory or a private index format. A script cannot ask it a question. Neither
can an agent, except by grepping and guessing.

**Agents are bolted on.** AI now writes a real share of the code we ship,
and in most editors it lives in a sidebar that sees a different world from
the one you are editing. It pastes into files behind your back, and you find
out what it did afterwards.

## What crc does instead

**It is native, and it is measured.** crc draws text on the GPU straight
through Metal and CoreText, the same APIs macOS uses for itself. One draw
call per frame, only the visible lines laid out. A keystroke reaches the GPU
in 0.8 ms at p99 in a 100 MiB file, a tenth of a frame at 120 Hz, and the
harness that proves it ships in the repository so you can run it yourself.
The whole app is 17 MB.

**It is small enough to read.** The Rust dependency tree is 14 crates, one
build script and zero proc macros, every one pinned to an exact version and
read line by line. Where the OS already ships the thing, crc uses it: CoreText
instead of a font stack, Metal instead of a graphics layer, the system SQLite
instead of a crate. CI fails if a new build script or proc macro appears. No
telemetry, no accounts, no web view, no JavaScript runtime.

**It is honest about your files.** Atomic saves. External changes, from Git
or a formatter or an agent, reload a clean tab and ask before touching a
dirty one. Unsaved text survives a crash as plain files you can open with
anything.

**It lets the agent in, where you can see it.** Claude Code connects to crc
as its IDE. It sees your file, your selection and your diagnostics, and every
edit it proposes opens as a diff you accept or reject. Files it writes show up
in their tabs at once. The bridge listens on localhost only, behind a token
only you can read.

## Three bets

These decide what gets built, and in what order.

### 1. The project index is a database

Symbols, references and spans belong in SQLite, not in a format only the
editor can read. Then "find all callers" is a query, and any tool, script or
agent can open the file and ask, without the editor's permission and without
the editor running.

*Partly built.* crc keeps a SQLite index per project of what every file
defines and uses. It drives completion and Go to Symbol in Project today, and
you can query it with `sqlite3`. Apple's SQLite is compiled without extension
loading, so a database other programs are invited to open cannot be made to
load code. Real references and syntax spans are next.

### 2. Extensions declare what they touch

An extension should be WebAssembly with a manifest that says what it may
read, write and reach, and a host that enforces it. Installing an extension
should never mean handing a stranger your home folder.

*Not built yet.* crc has no extensions at all until this exists, on purpose.
How they will work is in [the extensions design](extensions.md).

### 3. An agent is a protocol client, not a sidebar

The buffer, the syntax tree and the index should sit behind one API. The
editor's own UI is one client of it; a model is another, with exactly the
same view of your code as you have.

*First step built.* crc speaks the protocol Claude Code uses for IDEs, so
the agent sees what you see and edits through reviewable diffs. The general
API is not built yet.

## What crc will not do

- **Be cross-platform at the cost of being good on one.** macOS only, Apple
  Silicon only. A port would mean a second backend written from scratch, and
  that trade was made deliberately.
- **Phone home.** Nothing is sent anywhere except a once-a-day check for a
  newer release, which one setting turns off.
- **Grow a dependency without an argument.** Every crate needs a reason
  written down, and a measurement where it claims speed.
- **Pretend.** Features are described as built, partly built or not built.
  The [known limits](../README.md#what-does-not) are listed where you can find them.

## Why this order

None of the bets matter if the editor cannot draw text inside a frame
budget, or loses your work. So the renderer was built and measured first,
then the file handling, then the index and the agent bridge. The rest of the
roadmap follows the same rule: make it fast, make it safe, then make it
smart.
