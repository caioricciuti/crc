# Extension fixtures

Built modules the extension tests and the GUI self-test install and run.
Each is the release build of an extension in
[crc-extensions](https://github.com/caioricciuti/crc-extensions), copied
unchanged; rebuild from that commit to check it.

| Fixture | Source | SHA-256 |
|---|---|---|
| `sort-lines/sort_lines.wasm` | crc-extensions `e59392e` (the module unchanged since `aa9604c`), `extensions/sort-lines`, `cargo build --release --target wasm32-unknown-unknown` with Rust 1.98.0 | `7e052ae010e48dd1738b0e47d628e50459fab0af573b9cb3eb16edc0f7f5a449` |
| `markdown-preview/markdown_preview.wasm` | crc-extensions `44de852` (Markdown Preview 0.1.1), `extensions/markdown-preview`, the same build | `669bd56bd1bd838fd5e166aeed14c3562111d6f6d18aa6d78234bce18c0901c6` |
