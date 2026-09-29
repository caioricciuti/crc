# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh big-files.

# ---- big files --------------------------------------------------------------
# Past the read-only limit (lowered here: half a gigabyte per run is too
# much) the file opens, says why, and typing changes nothing.
printf 'first\nsecond\n' > "$T/big.txt"
cat > "$T/big.script" <<SCRIPT
text typed
key 36
dump $T/big.out
quit
SCRIPT
CRC_READ_ONLY_BYTES=8 CRC_SELFTEST="$T/big.script" "$BIN" "$T/big.txt" 2> "$T/big.err"
expect "$T/big.out" read_only "true"
expect "$T/big.out" dirty "false"
expect_line "$T/big.out" 1 "first"
expect_line "$T/big.out" 2 "second"
# The limit in force, which this run lowered to 8 bytes.
grep -q '^message: opened .*big.txt read-only: it is over 8 bytes$' "$T/big.out" \
    || failed "big.out: no read-only note: $(grep '^message:' "$T/big.out")"

# Past the hard cap (sparse, so nothing is written) it is refused from its
# size alone, from inside the app; a test instance gets the status line
# instead of the alert. (On the command line crc prints the reason and exits.)
mkdir -p "$T/hugeproj"
mkfile -n 3g "$T/hugeproj/huge.txt"
cat > "$T/huge.script" <<SCRIPT
wait 300
key 35 cmd p
text huge
key 36
dump $T/huge.out
quit
SCRIPT
CRC_SELFTEST="$T/huge.script" "$BIN" "$T/hugeproj" 2> "$T/huge.err"
grep -q '^message: not opened: huge.txt is 3.0 GB; crc opens files up to 2.0 GB$' "$T/huge.out" \
    || failed "huge.out: $(grep '^message:' "$T/huge.out")"
rm -rf "$T/hugeproj"

# A non-ASCII line past the shaping limit stays editable, and the status
# line says it is drawn without shaping while it is on screen.
(yes 'é' || true) | head -c 3300000 | tr -d '\n' > "$T/long.txt"
cat > "$T/long.script" <<SCRIPT
wait 200
text x
dump $T/long.out
quit
SCRIPT
CRC_SELFTEST="$T/long.script" "$BIN" "$T/long.txt" 2> "$T/long.err"
expect "$T/long.out" read_only "false"
expect "$T/long.out" unshaped "true"
expect "$T/long.out" dirty "true"
