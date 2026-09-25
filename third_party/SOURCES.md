# Where this code came from

Everything here is compiled into the editor by the `build.rs` in the project
root. `CHECKSUMS` pins every file; `scripts/check-third-party.sh` verifies
them and runs in CI.

Both trees were compared file by file against the upstream tag on 2026-09-19,
by git blob hash. The only differences are the ones listed.

## tree-sitter

- Upstream: https://github.com/tree-sitter/tree-sitter
- Tag `v0.27.0`, commit `6070dbfefd326bd735e5683eb128cc1b57dad0c0`
- Upstream `lib/include` and `lib/src` are `include` and `src` here. `LICENSE`
  is the repository's.
- Local changes: none. Every file is identical to upstream.

## tree-sitter-rust

- Upstream: https://github.com/tree-sitter/tree-sitter-rust
- Tag `v0.24.2`, commit `77a3747266f4d621d0757825e6b11edcbf991ca5`
- Taken: `src/`, `queries/highlights.scm`, `LICENSE`.
- Local changes: one, in `queries/highlights.scm`. Upstream's constant pattern
  is `"^[A-Z][A-Z\\d_]+$'"`, with a stray apostrophe after the end anchor that
  nothing can match, so SCREAMING_CASE names fell through and coloured as
  types. The apostrophe is removed. The file carries a comment at the spot.
  Re-apply it, or check whether upstream fixed it, when re-vendoring.

## The web grammars

Added 2026-09-19. Each was cloned at its tag, the commit checked against the
one the GitHub API reports for that tag, and its `LANGUAGE_VERSION` checked to
be inside what the runtime accepts (13 to 15). From every one: `src/parser.c`,
`src/scanner.c` where there is one, the headers under `src/`, `queries/*.scm`
and `LICENSE`. Not taken: `grammar.json`, `node-types.json`, bindings, tests.
All are MIT. No local changes to any of them.

| Directory | Upstream | Tag | Commit | ABI |
|---|---|---|---|---|
| `tree-sitter-html` | tree-sitter/tree-sitter-html | `v0.23.2` | `5a5ca8551a179998360b4a4ca2c0f366a35acc03` | 14 |
| `tree-sitter-javascript` | tree-sitter/tree-sitter-javascript | `v0.25.0` | `44c892e0be055ac465d5eeddae6d3e194424e7de` | 15 |
| `tree-sitter-css` | tree-sitter/tree-sitter-css | `v0.25.0` | `dda5cfc5722c429eaba1c910ca32c2c0c5bb1a3f` | 15 |
| `tree-sitter-json` | tree-sitter/tree-sitter-json | `v0.24.8` | `ee35a6ebefcef0c5c416c0d1ccec7370cfca5a24` | 14 |
| `tree-sitter-typescript` | tree-sitter/tree-sitter-typescript | `v0.23.2` | `f975a621f4e7f532fe322e13c4f79495e0a7b2e7` | 14 |

`tree-sitter-typescript` holds two parsers, `typescript/` and `tsx/`, whose
scanners both include `../../common/scanner.h`, so upstream's layout is kept.

Two things about the queries live in `src/syntax/mod.rs` rather than here,
because they are decisions and not files: the JavaScript query is written so
that the later of two patterns wins while the older ones expect the earlier,
and TypeScript's queries are therefore loaded after JavaScript's, not before
as upstream's `tree-sitter.json` lists them. HTML's `injections.scm` is
vendored for reference; the two injections it describes are declared in Rust.

## Additional code grammars

Added 2026-09-19 for Python, C, C++, and Go highlighting. From each upstream
repository: `src/parser.c`, `src/scanner.c` where present, the headers under
`src/tree_sitter/`, `queries/highlights.scm`, and `LICENSE`. All four parsers
use tree-sitter ABI 15 and are MIT licensed. No local changes were made to
the vendored files. C++ inherits the C highlight query in `src/syntax/mod.rs`.

| Directory | Upstream | Revision |
|---|---|---|
| `tree-sitter-python` | tree-sitter/tree-sitter-python | `v0.25.0`, `293fdc02038ee2bf0e2e206711b69c90ac0d413f` |
| `tree-sitter-c` | tree-sitter/tree-sitter-c | `b780e47fc780ddc8da13afa35a3f4ed5c157823d` |
| `tree-sitter-cpp` | tree-sitter/tree-sitter-cpp | `c009222808634c1014f82438d4883753516a2c24` |
| `tree-sitter-go` | tree-sitter/tree-sitter-go | `2346a3ab1bb3857b48b29d779a1ef9799a248cd7` |

## Configuration and shell grammars

Added 2026-09-24 for TOML, YAML and shell scripts, the files every project
has beside its code. Each was cloned at its release tag, the commit checked
against the one the GitHub API reports for that tag, the ABI read from
`LANGUAGE_VERSION` in `parser.c`, and the compiled size measured before
vendoring: 28 KB, 197 KB and 1.36 MB. The scanners were read: none touches
files, the environment or the network. From each: `src/parser.c`,
`src/scanner.c`, the headers under `src/tree_sitter/`,
`queries/highlights.scm`, `LICENSE`; YAML also brings the `src/schema.*.c`
tables its scanner `#include`s. All MIT. No local changes.

| Directory | Upstream | Tag | Commit | ABI |
|---|---|---|---|---|
| `tree-sitter-toml` | tree-sitter-grammars/tree-sitter-toml | `v0.7.0` | `64b56832c2cffe41758f28e05c756a3a98d16f41` | 14 |
| `tree-sitter-yaml` | tree-sitter-grammars/tree-sitter-yaml | `v0.7.2` | `7708026449bed86239b1cd5bce6e3c34dbca6415` | 14 |
| `tree-sitter-bash` | tree-sitter/tree-sitter-bash | `v0.25.1` | `a06c2e4415e9bc0346c6b86d401879ffb44058f7` | 15 |

Bash's `@embedded` capture has no highlight kind and is left uncoloured.
Markdown has no grammar on purpose: the editor renders Markdown source
itself (`src/markdown`).

## nerd-fonts-symbols

Added 2026-09-19. Not code: one font file, compiled into the binary with
`include_bytes!` and handed to CoreText, which is the only thing that parses
it. It is where the file-type and folder icons come from. The renderer draws
glyphs from an atlas, so an icon font goes through the path that already
exists, where a set of SVGs would have needed a rasteriser.

- Upstream: https://github.com/ryanoasis/nerd-fonts
- Release `v3.5.1`, published 2026-08-21, asset `NerdFontsSymbolsOnly.tar.xz`,
  SHA-256 `01172f37db8543edb102e5cb5c64101c9f4686630804d49b419aa07b23a69996`,
  which is the value the release's own `SHA-256.txt` gives for it.
- Taken: `SymbolsNerdFont-Regular.ttf` and `LICENSE`. The `Mono` variant was
  not: its glyphs are squeezed into one cell, and ours get two.
- Code points come from `glyphnames.json` at the same tag, not from memory.

The font is a collection, and each part keeps its own licence. From
upstream's `license-audit.md` at that tag:

| Glyph set | Licence |
|---|---|
| Seti-UI (modified), Devicons, Octicons, Powerline Extra, IEC Power, Font Awesome Extension | MIT |
| Codicons, Font Awesome | CC BY 4.0 |
| Material Design Icons | Apache 2.0 |
| Weather Icons, Pomicons | SIL OFL 1.1 |
| Font Logos | The Unlicense (the audit says "Unlicensed"; the upstream repository says Unlicense) |

All of them permit redistribution with attribution, which this table and the
`LICENSE` file beside the font are. The font ships next to a GPL-3.0 program
as a separate work; nothing links against it.

## Re-vendoring

1. Replace the files from the new upstream tag.
2. Re-apply the local changes above, or drop the ones upstream has fixed.
3. `scripts/check-third-party.sh --update`
4. Update the tag, commit and date in this file, in the same commit.

`parser.c` is a generated parse table of about 206K lines and cannot be
reviewed by reading. The check that means something is the one above: that it
is byte-identical to a named upstream commit.
