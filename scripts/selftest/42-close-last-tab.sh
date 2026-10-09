# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh close-last-tab.

# ---- Cmd-W on the last tab -------------------------------------------------
# Cmd-W on the last tab goes back to Home, as the tab's close button does,
# and a second Cmd-W at Home leaves the window open. It used to close the
# window, and closing the last window quits the app. A dump after each
# press only lands while the app is still running.
mkdir -p "$T/lastproj"
printf 'only\n' > "$T/lastproj/only.txt"
cat > "$T/last.script" <<SCRIPT
wait 600
key 13 cmd w
wait 100
dump $T/last-home.out
key 13 cmd w
wait 100
dump $T/last-again.out
quit
SCRIPT
CRC_SELFTEST="$T/last.script" "$BIN" "$T/lastproj/only.txt" 2> "$T/last.err"
expect "$T/last-home.out" tabs "Home"
expect "$T/last-again.out" tabs "Home"
