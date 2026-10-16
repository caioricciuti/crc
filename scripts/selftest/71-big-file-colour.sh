# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh big-file-colour.

# ---- a big file's colour ----------------------------------------------------
# A Rust file past the main thread's parse budget parses on the syntax
# worker: it opens at once, and its colour lands a few frames later. Typing
# goes on while a parse runs, and the next one picks the edit up.
yes 'fn handle_event(ev: &Event) -> Result<(), String> { let n = 42; Ok(()) } // note' \
    | head -n 37000 > "$T/bigcolour.rs" || true
cat > "$T/bigcolour.script" <<SCRIPT
idle 1500
dump $T/bigcolour1.out
text x
idle 1000
dump $T/bigcolour2.out
quit
SCRIPT
CRC_SELFTEST="$T/bigcolour.script" "$BIN" "$T/bigcolour.rs" 2> "$T/bigcolour.err"
for out in bigcolour1 bigcolour2; do
    spans=$(grep -m1 '^spans: ' "$T/$out.out" 2>/dev/null | sed 's/^spans: //' || true)
    if [ -z "$spans" ] || [ "$spans" -lt 100 ]; then
        failed "$out.out: [$spans] spans on screen, expected a coloured page"
    fi
done
expect "$T/bigcolour2.out" read_only "false"
expect "$T/bigcolour2.out" dirty "true"
expect_line "$T/bigcolour2.out" 1 "xfn handle_event(ev: &Event) -> Result<(), String> { let n = 42; Ok(()) } // note"
