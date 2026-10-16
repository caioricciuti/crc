# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh sideways.

# ---- a trackpad scrolls long lines sideways by points ----------------------
# Three lines of 300 characters, in a file that does not wrap. With the
# sidebar hidden a cell is 8 pt, so 100 pt sideways is twelve and a half
# columns, and the text moves by all of it rather than by twelve. A click at
# x=300 then lands on the character drawn under it, half a column on from
# where whole-column arithmetic would put it.
for _ in 1 2 3; do
    printf '%s\n' "$(printf 'abcdefghij%.0s' $(seq 1 30))"
done > "$T/wide.log"
cat > "$T/sideways.script" <<SCRIPT
key 11 cmd b
click 300 125
dump $T/sideways-before.out
trackpad 500 200 0 -100
click 300 125
dump $T/sideways.out
trackpad 500 200 0 1000
dump $T/sideways-back.out
quit
SCRIPT
CRC_SELFTEST="$T/sideways.script" "$BIN" "$T/wide.log" 2> "$T/sideways.err"
expect "$T/sideways-before.out" cursor "1:30"
expect "$T/sideways.out" scroll_x "12.50"
expect "$T/sideways.out" cursor "1:43"
# Back past the left edge stops at it, with no part of a column left over.
expect "$T/sideways-back.out" scroll_x "0.00"
