# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh crash-recovery-and-update.

# ---- crash recovery ----------------------------------------------------------
# A real panic with a document unsaved: the hook writes it out as the
# process aborts, the next launch names it and restores it as unsaved
# changes, and the recovered copies are removed. The file on disk is not
# touched. Then the same with Discard.
mkdir -p "$T/crashproj"
printf 'fn main() {}\n' > "$T/crashproj/notes.rs"
recovery="$HOME/Library/Application Support/crc/recovery"
cat > "$T/crash.script" <<SCRIPT
text // kept
panic
SCRIPT
cat > "$T/restored.script" <<SCRIPT
dump $T/restored.out
quit
SCRIPT
CRC_SELFTEST="$T/crash.script" "$BIN" "$T/crashproj/notes.rs" 2> "$T/crash.log" || true
[ -n "$(ls -A "$recovery" 2>/dev/null)" ] \
    || failed "crash: nothing written to $recovery"
CRC_SELFTEST="$T/restored.script" "$BIN" 2> "$T/restored.log"
grep -q 'restore prompt: Unsaved changes to 1 document were written to disk as the app went down: notes.rs (in crashproj)\.' "$T/restored.log" \
    || failed "restored.log: $(grep 'restore prompt' "$T/restored.log")"
expect "$T/restored.out" tabs "notes.rs"
expect "$T/restored.out" dirty "true"
expect_line "$T/restored.out" 1 "// keptfn main() {}"
[ -z "$(ls -A "$recovery" 2>/dev/null)" ] \
    || failed "restored: recovery copies left in $recovery"
[ "$(cat "$T/crashproj/notes.rs")" = "fn main() {}" ] \
    || failed "restored: the file on disk changed"

# The crash left a log, and Help > Report a Problem (run from the palette)
# builds an issue from it. A test instance says what it would open.
grep -q '^panic: selftest: forced panic$' "$HOME/Library/Logs/crc/"crash-*.log \
    || failed "crash: no crash log in $HOME/Library/Logs/crc"
cat > "$T/report.script" <<SCRIPT
key 35 cmd p
text >report a problem
key 36
dump $T/report.out
quit
SCRIPT
CRC_SELFTEST="$T/report.script" "$BIN" "$T/crashproj/notes.rs" 2> "$T/report.err"
grep -q '^message: would open https://github.com/caioricciuti/crc/issues/new?body=What%20did%20you%20do%3F' "$T/report.out" \
    || failed "report.out: $(grep '^message:' "$T/report.out" | cut -c1-160)"
grep -q 'Last%20crash%3A%20panic%3A%20selftest%3A%20forced%20panic' "$T/report.out" \
    || failed "report.out: the crash is not in the issue"

# ---- update check -------------------------------------------------------------
# Help > Check for Updates, from the palette, against local listings: a newer
# release is announced (a test instance does not open the browser), and an
# empty listing means this is the latest.
printf '[{"tag_name": "v999.0.0", "html_url": "https://github.com/caioricciuti/crc/releases/tag/v999.0.0", "draft": false}]' > "$T/releases-new.json"
printf '[]' > "$T/releases-none.json"
cat > "$T/update.script" <<SCRIPT
key 35 cmd p
text >check for updates
key 36
wait 500
wait 500
dump $T/update.out
quit
SCRIPT
CRC_UPDATE_URL="file://$T/releases-new.json" CRC_SELFTEST="$T/update.script" "$BIN" "$T/crashproj/notes.rs" 2> "$T/update.err"
expect "$T/update.out" message "crc 999.0.0 is out; would open https://github.com/caioricciuti/crc/releases/tag/v999.0.0"
CRC_UPDATE_URL="file://$T/releases-none.json" CRC_SELFTEST="$T/update.script" "$BIN" "$T/crashproj/notes.rs" 2> "$T/update-none.err"
mv "$T/update.out" "$T/update-none.out"
grep -q '^message: crc .* is the latest release$' "$T/update-none.out" \
    || failed "update-none.out: $(grep '^message:' "$T/update-none.out")"

CRC_SELFTEST="$T/crash.script" "$BIN" "$T/crashproj/notes.rs" 2> "$T/crash2.log" || true
CRC_RESTORE_ANSWER=discard CRC_SELFTEST="$T/restored.script" "$BIN" "$T/crashproj/notes.rs" 2> "$T/discarded.log"
mv "$T/restored.out" "$T/discarded.out"
expect "$T/discarded.out" dirty "false"
expect_line "$T/discarded.out" 1 "fn main() {}"
# The crashes write their panic on purpose, so these logs are not *.err;
# past the prompt the relaunches must be as quiet as any other run.
if cat "$T/restored.log" "$T/discarded.log" | grep -v '^crc: restore prompt: ' | grep -v '^slow frame' | grep -q .; then
    echo "FAIL: a relaunch after a crash wrote to stderr:"
    cat "$T/restored.log" "$T/discarded.log"
    fail=1
fi
[ -z "$(ls -A "$recovery" 2>/dev/null)" ] \
    || failed "discarded: recovery copies left in $recovery"
