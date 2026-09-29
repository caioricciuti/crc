# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh lsp-features.

# ---- language server: references, rename, format, signature help ----------
# The same fake server. Shift-F12 lists every use of the name under the
# caret in the find bar; F2 renames it in the open file and in one that is
# not open, on disk; Shift-Option-F formats (trailing spaces, `){`); typing
# `(` shows the signature and `,` moves to the second parameter.
mkdir -p "$T/lsp2proj"
printf 'fn total(){   \n    let count = 1;\n    count + count\n}\n' > "$T/lsp2proj/main.rs"
printf 'fn other() { count(); }\n' > "$T/lsp2proj/other.rs"
cat > "$T/lsp2.script" <<SCRIPT
wait 800
wait 400
wait 400
key 125 -
key 124 -
key 124 -
key 124 -
key 124 -
key 124 -
key 124 -
key 124 -
key 124 -
key 124 -
key 111 shift
wait 400
wait 300
dump $T/lsp2-refs.out
key 53
key 120 -
wait 100
text tally
dump $T/lsp2-field.out
key 36
wait 400
wait 300
dump $T/lsp2-renamed.out
key 3 shift,opt f
wait 400
wait 300
dump $T/lsp2-formatted.out
key 125 cmd
text alpha(1,
wait 400
wait 300
dump $T/lsp2-signature.out
text  2)
wait 300
dump $T/lsp2-closed.out
quit
SCRIPT
CRC_LSP_FAKE="$PWD/scripts/fake-lsp.py" CRC_SELFTEST="$T/lsp2.script" "$BIN" "$T/lsp2proj/main.rs" 2> "$T/lsp2.err"
expect "$T/lsp2-refs.out" find_results "main.rs:2|main.rs:3|main.rs:3|other.rs:1"
expect "$T/lsp2-refs.out" message "4 references"
expect "$T/lsp2-field.out" rename "tally"
expect_line "$T/lsp2-renamed.out" 2 "    let tally = 1;"
expect_line "$T/lsp2-renamed.out" 3 "    tally + tally"
expect "$T/lsp2-renamed.out" dirty true
expect "$T/lsp2-renamed.out" message "renamed in 2 files, 1 open and unsaved"
# other.rs was not open: renamed on disk. main.rs is open: not written.
[ "$(cat "$T/lsp2proj/other.rs")" = "fn other() { tally(); }" ] \
    || { echo "FAIL lsp2: other.rs is: $(cat "$T/lsp2proj/other.rs")"; fail=1; }
grep -q 'let count = 1;' "$T/lsp2proj/main.rs" \
    || { echo "FAIL lsp2: the open main.rs was written to disk"; fail=1; }
expect_line "$T/lsp2-formatted.out" 1 "fn total() {"
expect "$T/lsp2-formatted.out" message "formatted"
expect "$T/lsp2-signature.out" signature "fn alpha(first: i32, second: i32) [second: i32]"
expect "$T/lsp2-closed.out" signature ""
