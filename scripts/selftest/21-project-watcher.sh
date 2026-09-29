# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh project-watcher.

# ---- the project watcher --------------------------------------------------
# A file created by another process shows up in the finder without a
# refresh. The scripted touch writes it from this process, which FSEvents
# would ignore as our own, so a subprocess writes it instead.
mkdir -p "$T/watchproj"
printf 'one\n' > "$T/watchproj/first.txt"
cat > "$T/watch.script" <<SCRIPT
wait 600
wait 100
dump $T/watch-before.out
wait 1200
wait 400
wait 400
wait 400
wait 400
dump $T/watch-after.out
quit
SCRIPT
# Written once the app has taken its "before" dump, not at a guessed time
# after launch: a slow launch made the check fail in either direction.
(
    for _ in $(seq 1 200); do
        [ -s "$T/watch-before.out" ] && break
        sleep 0.05
    done
    sleep 0.3
    printf 'two\n' > "$T/watchproj/second.txt"
) &
CRC_SELFTEST="$T/watch.script" "$BIN" "$T/watchproj" 2> "$T/watch.err"
wait
expect "$T/watch-before.out" finder_entries 1
expect "$T/watch-after.out" finder_entries 2
