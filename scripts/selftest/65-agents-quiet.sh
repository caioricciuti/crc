# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh quiet.

# ---- Agents mode left alone runs little Git, and none in the background ---
# A folder workspace of five repositories, one with a tag and one with
# uncommitted work. A stand-in `git` on PATH logs each call with the phase
# the script is in, then runs the real one. Left alone in front for a
# minute, Agents mode reads the selected folder's worktrees once and the
# inspector's status about once: no read every few seconds. Behind another
# app (`front off`) nothing runs; back in front, the status is read again
# at once. The counts are printed for the record.
QD="$T/quietdesk"
mkdir -p "$T/quietbin" "$T/home/quiet"
for repo in almanac birdhouse compost lantern windmill; do
    mkdir -p "$QD/$repo"
    printf '%s\n' "$repo" > "$QD/$repo/notes.txt"
    testgit "$QD/$repo" init -q -b main
    testgit "$QD/$repo" add notes.txt
    testgit "$QD/$repo" commit -qm "$repo begins"
done
testgit "$QD/lantern" -c tag.gpgsign=false tag v0.3
printf 'wick\n' >> "$QD/lantern/notes.txt"
testgit "$QD/lantern" commit -qam wick
printf 'blades\n' >> "$QD/windmill/notes.txt"
printf 'gears\n' > "$QD/windmill/gears.txt"
real_git=$(command -v git)
cat > "$T/quietbin/git" <<GIT
#!/bin/sh
echo "\$(cat "\$QUIET_PHASE" 2>/dev/null) \$*" >> "\$QUIET_LOG"
exec "$real_git" "\$@"
GIT
chmod +x "$T/quietbin/git"
printf 'launch\n' > "$T/quiet-phase"
: > "$T/quiet-git.log"
cat > "$T/quiet.script" <<SCRIPT
wait 500
key 0 cmd,shift A
idle 4000
dump $T/quiet-shown.out
write $T/quiet-phase front
idle 60000
write $T/quiet-phase away
front off
idle 25000
write $T/quiet-phase back
front on
idle 3000
dump $T/quiet-back.out
write $T/quiet-phase done
quit
SCRIPT
HOME="$T/home/quiet" PATH="$T/quietbin:$PATH" QUIET_PHASE="$T/quiet-phase" QUIET_LOG="$T/quiet-git.log" \
    CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/quiet.script" \
    "$BIN" "$QD" 2> "$T/quiet.err"
expect "$T/quiet-shown.out" agents_tree "quietdesk"
quiet_count() { grep -c "^$1 " "$T/quiet-git.log" || true; }
echo "quiet: git runs at launch $(quiet_count launch), in front for a minute $(quiet_count front), \
in the background for 25 s $(quiet_count away), back in front for 3 s $(quiet_count back)"
[ "$(quiet_count front)" -le 45 ] \
    || failed "quiet: $(quiet_count front) git runs in a minute left alone: $(grep '^front ' "$T/quiet-git.log" | sort | uniq -c | sort -rn | head)"
[ "$(quiet_count away)" = 0 ] \
    || failed "quiet: git ran in the background: $(grep '^away ' "$T/quiet-git.log" | head)"
grep -q '^back .* status --porcelain=v2' "$T/quiet-git.log" \
    || failed "quiet: the status was not read on coming back: $(grep '^back ' "$T/quiet-git.log" | head)"
