# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh typing.

# ---- typing ---------------------------------------------------------------
: > "$T/typing.txt"
cat > "$T/typing.script" <<SCRIPT
text let x = 1;
key 36
text ok
dump $T/typing.out
quit
SCRIPT
CRC_SELFTEST="$T/typing.script" "$BIN" "$T/typing.txt" 2> "$T/typing.err"
expect_line "$T/typing.out" 1 "let x = 1;"
expect_line "$T/typing.out" 2 "ok"
expect "$T/typing.out" cursor "2:3"
expect "$T/typing.out" dirty "true"
