# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh pr-refresh.

# ---- A worktree's pull request is read again on its own ---------------------
# A stand-in `gh` on PATH answers `gh pr view` from a state file the script
# rewrites: the pull request is open with its checks running, then GitHub
# cannot be reached (the badge stays), then it is merged on GitHub and the
# row folds away under the Merged line, with no click. A test instance asks
# `gh` only with CRC_SELFTEST_GH set; CRC_SELFTEST_PR_EVERY shortens the
# reads' wait to a second. Then, at the usual cadence, a push of the branch
# (its remote-tracking ref moving) has its pull request read within
# seconds, not at the next read. SELFTEST_FRAMES, when set, is a folder to
# keep the frames in.
PD="$T/prdesk"
PL="$PD/pr-log"
mkdir -p "$PL" "$T/prbin" "$T/home/prdesk"
printf 'seeds\n' > "$PL/seeds.txt"
testgit "$PL" init -q -b main
testgit "$PL" add seeds.txt
testgit "$PL" commit -qm seeds
testgit "$PL" worktree add -q -b feat/rain-gauge "$PD/pr-log-rain"
printf 'mm\n' > "$PD/pr-log-rain/rain.txt"
testgit "$PD/pr-log-rain" add rain.txt
testgit "$PD/pr-log-rain" commit -qm rain
cat > "$T/prbin/gh" <<'GH'
#!/bin/sh
# Only feat/rain-gauge has a pull request; each call is logged with the
# state it answered in.
state=$(cat "$PR_GH_STATE" 2>/dev/null)
echo "$state $*" >> "$PR_GH_LOG"
[ "$1 $2" = "pr view" ] || exit 1
if [ "$3" != "feat/rain-gauge" ]; then
    echo "no pull requests found for branch \"$3\"" >&2
    exit 1
fi
case "$state" in
OPEN) echo '{"number":12,"state":"OPEN","url":"u","statusCheckRollup":[{"status":"IN_PROGRESS","conclusion":""}]}' ;;
MERGED) echo '{"number":12,"state":"MERGED","url":"u","statusCheckRollup":[{"status":"COMPLETED","conclusion":"SUCCESS"}]}' ;;
FAIL)
    echo "error connecting to api.github.com" >&2
    exit 1
    ;;
*)
    echo "no pull requests found for branch \"$3\"" >&2
    exit 1
    ;;
esac
GH
chmod +x "$T/prbin/gh"
printf 'OPEN\n' > "$T/pr-state"
: > "$T/pr-gh.log"
cat > "$T/pr.script" <<SCRIPT
wait 500
key 0 cmd,shift A
idle 2000
frame $T/pr-open.bmp
dump $T/pr-open.out
write $T/pr-state FAIL
idle 2500
frame $T/pr-fail.bmp
dump $T/pr-fail.out
write $T/pr-state MERGED
idle 2500
frame $T/pr-merged.bmp
dump $T/pr-merged.out
quit
SCRIPT
HOME="$T/home/prdesk" PATH="$T/prbin:$PATH" PR_GH_STATE="$T/pr-state" PR_GH_LOG="$T/pr-gh.log" \
    CRC_SELFTEST_GH=1 CRC_SELFTEST_PR_EVERY=1 CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/pr.script" \
    "$BIN" "$PD" 2> "$T/pr.err"
expect "$T/pr-open.out" agents_tree "prdesk,>feat/rain-gauge"
expect "$T/pr-open.out" agents_prs "feat/rain-gauge=#12:OPEN:running"
# gh failed: it was asked, and the row still has its pull request.
grep -q '^FAIL pr view feat/rain-gauge ' "$T/pr-gh.log" \
    || failed "pr-refresh: gh was not asked while it failed: $(cat "$T/pr-gh.log")"
expect "$T/pr-fail.out" agents_prs "feat/rain-gauge=#12:OPEN:running"
# Merged on GitHub: the row folds away by itself.
expect "$T/pr-merged.out" agents_prs "feat/rain-gauge=#12:MERGED:passing"
expect "$T/pr-merged.out" agents_tree "prdesk,~merged(+1)"

# A push, at the usual cadence (the next periodic read is 20 seconds off):
# no pull request when the list is read, then one is opened and the branch
# pushed from outside crc. The push is heard, and read two seconds later.
printf 'NONE\n' > "$T/pr-state"
: > "$T/pr-gh.log"
cat > "$T/pr-push.script" <<SCRIPT
wait 500
key 0 cmd,shift A
idle 2500
dump $T/pr-before-push.out
idle 7000
frame $T/pr-pushed.bmp
dump $T/pr-pushed.out
quit
SCRIPT
(
    sleep 6
    printf 'OPEN\n' > "$T/pr-state"
    testgit "$PL" update-ref refs/remotes/origin/feat/rain-gauge feat/rain-gauge
) &
pr_pusher=$!
HOME="$T/home/prdesk" PATH="$T/prbin:$PATH" PR_GH_STATE="$T/pr-state" PR_GH_LOG="$T/pr-gh.log" \
    CRC_SELFTEST_GH=1 CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/pr-push.script" \
    "$BIN" "$PD" 2> "$T/pr-push.err"
wait "$pr_pusher" 2>/dev/null || true
expect "$T/pr-before-push.out" agents_prs ""
expect "$T/pr-pushed.out" agents_prs "feat/rain-gauge=#12:OPEN:running"
[ "$(grep -c '^OPEN pr view feat/rain-gauge ' "$T/pr-gh.log")" = 1 ] \
    || failed "pr-refresh: after the push gh was asked: $(cat "$T/pr-gh.log")"
[ -z "${SELFTEST_FRAMES:-}" ] || cp -f "$T/pr-open.bmp" "$T/pr-fail.bmp" "$T/pr-merged.bmp" \
    "$T/pr-pushed.bmp" "$SELFTEST_FRAMES/"
testgit "$PL" worktree remove --force "$PD/pr-log-rain" 2>/dev/null || true
