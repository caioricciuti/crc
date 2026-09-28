# WebAssembly spec tests

Files of the official WebAssembly testsuite, unchanged, from
<https://github.com/WebAssembly/testsuite> at commit
`b464a4cd100d98175ae6e3890db89a2e6c8302f7` (2026-09-15). Licensed under
Apache 2.0, in `LICENSE` here.

`tests/wasm_spec.rs` runs them through crc's interpreter. It reads the text
format and builds each module's binary itself: numbers, locals, globals,
blocks, loops, `if`, branches and `br_table`, direct calls, one memory with
its data, loads and stores. No text-format assembler, no outside tool.

| Test | Files | Passed | Not run |
|---|---|---:|---:|
| numbers | i32, i64, f32, f64, f32_cmp, f64_cmp, f32_bitwise, f64_bitwise, conversions | 11,871 | 0 |
| expressions | int_exprs, float_exprs, float_misc, int_literals, float_literals | 1,507 | 0 |
| control and memory | address, block, br, br_if, br_table, loop, if, local_get, local_set, local_tee, select, nop, return, unreachable, load, store, endianness, memory_grow, memory_size, memory, fac, call, stack, labels, switch, left-to-right, forward, unwind, global | 1,666 | 194 |

15,044 assertions pass and none fail. What is not run needs what crc does
not run by design and refuses at load (docs/extensions.md): tables for
`call_indirect`, reference types (`externref`, `funcref` values), imported
functions and globals from the testsuite's `spectest` module, and, in
`memory_grow.wast` and `global.wast`, whole modules built on multi-memory
and extended constants from WebAssembly 3.0. The runner counts them and
the test fails if the count grows, so it cannot quietly stop building
something.

`assert_invalid` and `assert_malformed` (1,044 cases) test a validator,
which the interpreter does not have by design; they are counted and left
out. crc's own malformed-module corpus is in `src/ext/mod.rs`.

`CHECKSUMS` pins every file; `scripts/check-third-party.sh` verifies them
in CI. To update: download the same files at a newer commit, run
`scripts/check-third-party.sh --update tests/spec`, change the commit above,
and commit all three together.
