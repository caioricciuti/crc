# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh held.

# ---- sessions outlive the window ---------------------------------------------
# A terminal runs in a helper process of its own (crc --hold). Quitting
# leaves it running; the next window on the same project takes it back with
# the output it printed meanwhile, and typing into it works as before.
mkdir -p "$T/heldproj"
printf 'kept\n' > "$T/heldproj/a.txt"
cat > "$T/held-1.script" <<SCRIPT
wait 500
click @toolbar.terminal
wait 700
wait 500
text echo held-ok
key 36
wait 500
dump $T/held-first.out
quit
SCRIPT
CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/held-1.script" "$BIN" "$T/heldproj" 2> "$T/held-1.err"
expect_terminal "$T/held-first.out" "held-ok"
ls "$T"/home/Library/Application\ Support/crc/held/*.spec >/dev/null 2>&1 \
    || failed "held: no helper left after quitting"
cat > "$T/held-2.script" <<SCRIPT
wait 700
wait 500
dump $T/held-back.out
click @toolbar.terminal
wait 300
text echo held-again
key 36
wait 500
dump $T/held-typed.out
quit
SCRIPT
CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/held-2.script" "$BIN" "$T/heldproj" 2> "$T/held-2.err"
expect_terminal "$T/held-back.out" "held-ok"
expect_terminal "$T/held-typed.out" "held-again"
# Another project does not get it.
mkdir -p "$T/heldother"
cat > "$T/held-3.script" <<SCRIPT
wait 700
dump $T/held-other.out
quit
SCRIPT
CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/held-3.script" "$BIN" "$T/heldother" 2> "$T/held-3.err"
grep -q '^terminals: $' "$T/held-other.out" \
    || failed "held: another project took the session: $(grep '^terminals:' "$T/held-other.out")"
