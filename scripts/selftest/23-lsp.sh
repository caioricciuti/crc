# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh lsp.

# ---- language server --------------------------------------------------------
# The fake server in scripts/fake-lsp.py stands in for rust-analyzer: it
# flags TODO lines, completes alpha/alphabet/beta/gamma and defines
# everything at 1:1. Diagnostics arrive on open, the list follows typing,
# Tab accepts the suggestion, F12 jumps.
mkdir -p "$T/lspproj"
printf 'fn main() {\n    // TODO later\n}\n' > "$T/lspproj/main.rs"
cat > "$T/lsp.script" <<SCRIPT
wait 800
wait 400
wait 400
dump $T/lsp-open.out
text alp
wait 400
wait 300
dump $T/lsp-list.out
key 48
wait 200
dump $T/lsp-accepted.out
key 111
wait 400
wait 300
dump $T/lsp-definition.out
quit
SCRIPT
CRC_LSP_FAKE="$PWD/scripts/fake-lsp.py" CRC_SELFTEST="$T/lsp.script" "$BIN" "$T/lspproj/main.rs" 2> "$T/lsp.err"
expect "$T/lsp-open.out" lsp "fake Ready"
expect "$T/lsp-open.out" diagnostics 1
expect "$T/lsp-list.out" completion "alpha|alphabet"
expect_line "$T/lsp-accepted.out" 1 "alpha()fn main() {"
expect "$T/lsp-accepted.out" completion ""
expect "$T/lsp-definition.out" cursor "1:1"
