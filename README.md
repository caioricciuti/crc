# crc

A text editor for macOS, drawn on the GPU straight from Metal and CoreText,
with a terminal, local Git and Claude Code built in.

**Alpha.** It is used daily by its author and is now going to a small group of
testers. Passing tests are not a stability guarantee; known problems are in
the [issue tracker](https://github.com/caioricciuti/crc/issues). Why the
project exists is in [docs/vision.md](docs/vision.md).

## The number

Measured on 2026-09-24 for `v0.2.0-alpha.1`, with a 100 MiB file and the
caret halfway through 2.7 million lines:

| Scope | Keystroke to GPU completion, p99 | Budget |
|---|---:|---:|
| Editor viewport | 0.803 ms | 8.333 ms |
| Editor with native toolbar, sidebar, tabs and status | 1.009 ms | 8.333 ms |

Reproduce both:

```sh
cargo run --release --offline --example frame_latency
cargo run --release --offline --example frame_latency -- --chrome
```

The harness measures buffer mutation, layout, draw encoding and GPU
completion. It excludes event delivery before the app and display scanout, so
it is not a camera-measured input-to-photon result. Results vary by machine.

## Requirements

- An Apple Silicon Mac on macOS 14 or later. There is no Intel build in the
  alpha and no plan for Linux or Windows; see [Design decisions](#design-decisions).
- Nothing else. Language servers are optional and found if installed.

## Install

Download the DMG from the latest release, open it, drag crc to Applications.
The app is signed with a Developer ID and notarized, so it opens without a
Gatekeeper warning. Check the download against `SHA256SUMS` from the same
release if you like.

To build it yourself instead, with Rust 1.98 or newer:

```sh
git clone https://github.com/caioricciuti/crc
cd crc
scripts/bundle.sh --install
```

That puts an ad-hoc signed `crc.app` in `/Applications`. Either way:

```sh
open -a crc path/to/file    # open a file
open -a crc .               # open the current folder as a project
```

## Settings

crc > Settings (`Cmd-,`) opens `~/.config/crc/config.toml`, created from a
commented template the first time: `font`, `font_size`, `theme`
(`system`, `dark` or `light`), `caret_blink`, `update_check`,
`format_on_save`, `word_wrap`, `ssh_auth_sock` and `conflict_view`
(`inline` or `side-by-side`), each explained in the file. Saving the file
applies it. `Cmd-=`, `Cmd--` and `Cmd-0` change
the size and write it back.

## What works

- **Syntax highlighting** via tree-sitter, compiled from vendored C: Rust,
  Python, C, C++, Go, HTML, CSS, JavaScript and JSX, TypeScript and TSX,
  JSON, TOML, YAML and shell. Languages embedded in one another are parsed
  as themselves. Indent guides, bracket-pair highlight, and Git marks in the
  gutter for lines that differ from HEAD.
- **Completion that learns, in every language**: the best guess appears as
  ghost text at the caret (Tab takes it) and the alternatives as a single
  row of chips under the line, with a line saying why the picked one is
  offered: where it is defined, how often it is used, how often you picked
  it after the same receiver. Suggestions come from the language server,
  a SQLite index of what the whole project defines and uses
  (`~/Library/Caches/crc/index/`), words near the caret, paths (any
  `.gitignore`, `./` imports, quoted paths), and your own history
  (`~/Library/Application Support/crc/completion.db`, local only;
  Go > Forget Completion History clears it). Typing a `.gitignore` line
  shows in the Explorer what it would ignore before you save.
- **Language servers**: completion as part of the above,
  diagnostics underlined with counts in the status line, go to definition
  (`F12` or `Cmd`-click) and hover (`F1`). One server per language per
  project, started when the first file of that language opens:
  rust-analyzer, gopls, pyright, typescript-language-server and clangd,
  found in the usual install locations without reading your shell profile.
  Find references (`Shift-F12`), rename (`F2`), format (`Shift-Option-F`,
  or on save) and signature help. No code actions yet.
- **Claude Code inside crc**: type `claude` in any crc terminal and it
  connects to the window by itself, or press `Cmd-Shift-C` for a Claude tab.
  Claude sees the file and selection you are on and the language server
  diagnostics, and every edit it proposes opens as a diff tab: Accept
  (`Cmd-Return`) or Reject (`Esc`). Files Claude writes to disk show up in
  their tabs at once. The bridge listens on 127.0.0.1 only, behind a random
  token in a lock file readable by you alone; `CRC_NO_CLAUDE=1` turns it off.
- **Terminal panel** (`` Ctrl-` `` or `Cmd-J`): sessions under the editor
  running your login shell in the project folder, on an xterm-compatible
  emulator of our own with 24-bit colour, scroll regions, alternate screen
  and bracketed paste. `Cmd`-click a path such as `src/main.rs:42:7` in the
  output to open it at that line.
- **Local Git** (`Cmd-Option-G`): branch and status, changed files, staged
  and working-tree diffs, whole-file and hunk stage/unstage, commit. Git runs
  on workers and reads saved disk state; hooks and signing stay as Git has
  them. Branch switch and create from the palette, fetch, fast-forward pull
  and push (never forced), and the caret line's blame in the status line.
- **Merge conflicts**, however the merge, rebase, cherry-pick or stash pop
  that left them was run: conflicted files get their own group in Source
  Control and the status bar says `MERGING` or `REBASING`. In the file,
  each conflict is washed in its side's colour with Accept Current,
  Incoming, Both and (with `diff3` or `zdiff3` markers) Base on its first
  line; or switch to side by side, where the current, base and incoming
  versions sit in aligned columns with the same buttons. Every accept is
  one undo step. Mark Resolved saves and stages the file once no markers
  are left, and refuses while any are.
- **Files you can trust**: atomic saves that keep mode, links and symlink
  targets; a file changed by another program reloads if the tab is clean,
  and Save asks Overwrite, Cancel or Reload if it is not; a deleted file
  marks its tab unsaved; unsaved-changes prompts on close and quit; crash
  recovery of unsaved text; UTF-8 and UTF-16 BOMs, Windows-1252 and every
  line ending preserved. Files over 512 MB open read-only and files over
  2 GB are refused, both saying why.
- **Split panes** (`Cmd-\`), up to four, each with its own tabs.
- **Project search** (`Cmd-Shift-F`) in the background, `Cmd-P` fuzzy file
  open, and `>` in the same box to run any menu command.
- **HTTP requests from `.http` files** (`Cmd-Return`) in the JetBrains and
  VS Code REST Client format, with environments and a response tab.
- **Markdown** renders in place as you edit; images, PDFs, audio, video and
  office files open as Quick Look previews.
- **A native, themed UI**: system-font chrome, light and dark following the
  system or your setting, a home screen with recent projects, overlay
  scrollbars, font zoom, and the right pointer for every control.
- Everything an editor needs: multiple cursors (`Cmd-D`, Option-click), find
  and replace with regex, go to line, comment toggle, line move and
  duplicate, auto-indent and auto-close, word-wise and Emacs motions, full
  Unicode with emoji and CJK, session restore, Dock and Finder integration.

## What does not

- **Language coverage is the list above.** A grammar is vendored generated C
  pinned in `third_party/CHECKSUMS`; SQL and Svelte are not in the alpha.
- **Git stops short of history.** No history browser, branch deletion or
  merge and rebase commands of its own (abort and continue are the
  terminal's). Pull is fast-forward only. Hunk staging covers tracked text
  changes; other change types use whole-file actions. Gutter marks compare
  with HEAD, not the index. Conflict markers must be Git's default seven
  characters.
- **Language servers have no code actions yet**, so no quick fixes or
  organise imports.
- **No minimap.** Folding is by indentation, not by syntax.
- **Panes are a way of looking.** A file is open in one pane at a time, and
  a session restores every pane's files into one pane.
- **Bounded by design.** Project search skips files over 2 MiB and shows the
  first 500 matches; gutter marks stop at 2 MiB; the bracket matcher counts
  its own pair only and does not skip strings or comments.
- **Text input is new.** Dead keys and IME go through macOS text input and
  need wider testing across layouts.
- **Updates are announced, not installed.** Once a day crc asks GitHub for
  the list of releases and says in the status line if a newer one exists;
  Help > Check for Updates opens its page. `update_check = false` turns
  the daily check off. Nothing is downloaded.
- **macOS only, and deliberately so.**

## Keys

| | |
|---|---|
| arrows, Home/End, PageUp/PageDown | move |
| shift + any of those | extend the selection |
| click, drag, shift-click | position and select |
| `Cmd-A` | select all |
| `Cmd-C` / `Cmd-X` / `Cmd-V` | copy, cut, paste |
| `Cmd-Z` / `Cmd-Shift-Z` | undo, redo |
| `Cmd-F` | find; Tab switches to replace; Enter cycles or replaces |
| `Cmd-/` | toggle comment |
| `Cmd-Shift-D` | duplicate line |
| `Cmd-Shift-Up/Down` | move line |
| Tab / Shift-Tab | indent / outdent selection |
| `Cmd-P` | fuzzy file open |
| `Cmd-Option-G` | local Source Control |
| `Cmd-Return` in Source Control | commit staged changes |
| `Cmd-Return` in a `.http` file | send the request under the caret |
| `Cmd-\` | split the editor to the right |
| `Cmd-Option-[` / `Cmd-Option-]` | focus the previous / next pane |
| `Cmd-Option-W` | close the pane |
| `F12`, Cmd-click | go to definition |
| `F1` | hover information for the symbol at the caret |
| `Ctrl-Space` | completion; Tab takes the suggestion, Up/Down pick another (then Return takes it too), Option-1 to 9 take one directly, Escape closes |
| Escape in a picker/panel | dismiss |
| `Cmd-F` / `Cmd-Shift-F` | find in file / find in project |
| `Cmd-L` | go to line |
| `Cmd-D` | select word, then next occurrence as another cursor |
| Option-click | add a cursor |
| Escape | back to one cursor |
| `Cmd-O` / `Cmd-Shift-O` | open file / open folder |
| `Cmd-S` / `Cmd-Shift-S` | save / save as; File > Revert to Saved goes back to disk |
| `Cmd-,` | settings file |
| `Cmd-=` / `Cmd--` / `Cmd-0` | zoom the code font in, out, back to 13 |
| `Cmd-Shift-C` | Claude Code tab |
| `` Ctrl-` `` / `Cmd-J` | terminal panel |
| `>` in `Cmd-P` | run a menu command |
| `Cmd-N` | new file: a name field in the sidebar |
| `Cmd-B` | toggle sidebar |
| `Cmd-1..9`, `Cmd-[`, `Cmd-]` | switch tabs |
| drag a tab, or right-click it | reorder or manage tabs |
| scroll over the tab strip | reveal overflowed tabs |
| double-click empty tab strip | new file |
| `Cmd-W` / `Cmd-Q` | close tab, quit |
| Option-arrows, Option-Backspace | word-wise motion and deletion |
| `Ctrl-A/E/K/D/B/F/P/N` | the macOS Emacs bindings |

## Design decisions

**macOS only, no wgpu.** Going straight to Metal via `objc2` costs ~10 crates
instead of ~200, and one build script instead of ~40. The price is that a
Linux or Windows port means writing a second backend from scratch. That was a
deliberate trade, not an oversight.

**CoreText, not a Rust font stack.** It ships with the OS, handles hinting and
complex-script shaping better than anything we would write, and costs zero
dependencies.

**Our own rope.** `src/text/rope.rs` is a persistent B-tree. Not
[Not Invented Here]: the rope's API shape dictates undo, multi-cursor,
syntax-tree sync and eventual CRDT merging, so it is core to the product
rather than plumbing. Because it is persistent, `Rope::clone` is O(1) and
shares structure, which is what makes whole-buffer undo snapshots cheap and
will let a background thread parse a consistent snapshot while you keep
typing.

**One draw call per frame.** Every glyph is an instance of one quad. Drawing a
screen of text is a single `drawPrimitives` with no per-frame geometry work.

## Supply chain

The dependency tree is **14 crates, one vendored build script, zero proc
macros**, and that build script has been read line by line.

tree-sitter is compiled from C checked into `third_party/` with our own
`build.rs`, rather than taken as a crate. The crate would have cost 29 crates,
10 build scripts and a proc macro, because `serde_json` is one of its *build*
dependencies.

This is not incidental. Cargo runs arbitrary code at build time via `build.rs`
and at compile time via proc macros, with no allowlist, no `--ignore-scripts`,
and no minimum release age. Every dependency is pinned exactly (`=x.y.z`), and
CI fails if a new build script or any proc-macro crate enters the tree.

Details, including the review of every build script, are in
[`docs/dependency-review.md`](docs/dependency-review.md). Read it before
proposing a dependency.

`vendor/` is not committed (16MB against 132KB of source). Run
`scripts/vendor.sh` for a fully offline, auditable build.

## Layout

```
src/text/rope.rs        persistent B-tree rope
src/text/buffer.rs      cursor, selection, undo, files, edit tracking
src/text/documents.rs   open tabs
src/syntax/             tree-sitter parsing and highlight queries
src/project/tree.rs     sidebar file tree
src/project/finder.rs   Cmd-P fuzzy matching
src/project/git.rs      local Git commands, status parsing and bounded diffs
src/platform/git_panel.rs native Source Control panel and workers
src/http/               .http request files, environments, curl runner
src/json.rs             JSON reader and printer, shared by HTTP and LSP
src/lsp/                language servers: registry, stdio transport, client
src/project/watch.rs    FSEvents watcher for the project root
src/platform/dispatch.rs main-thread wake-ups from other threads
src/render/font.rs      CoreText shaping and paged glyph rasterization
src/render/layout.rs    visible lines to glyph quads, hit testing
src/render/metal.rs     pipeline, instance buffer, one draw call
src/render/shader.metal vertex + fragment
src/platform/window.rs  NSWindow, input, event loop
src/platform/latency.rs keystroke-to-present timing
src/platform/session.rs what was open last time
examples/               benchmarks and headless render dumps
```

## Reporting a problem

Help > Report a Problem opens a new GitHub issue in your browser with the
version, your macOS version and the last crash (if any) filled in; crc
sends nothing itself. Crash logs are in `~/Library/Logs/crc` (Help > Show
Crash Logs). Or open an issue by hand with what you did, what you
expected, what happened, and the version from crc > About crc or
`crc --version`. A screenshot helps more than a long description.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The short version: a dependency needs
a real argument, and performance claims need a measurement.

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE).
