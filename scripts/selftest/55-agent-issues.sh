# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh agent-issues.

# ---- A repository's open issues in the workspace inspector -----------------
# A stand-in `gh` on PATH answers `gh issue list` from a state file the
# script rewrites; it knows no pull requests. A test instance asks `gh` only
# with CRC_SELFTEST_GH set. Until Load Issues is clicked `gh` is not asked
# about issues at all. The list comes slowly the first time (the
# inspector shows skeletons), then twelve issues: ten show, Show all shows
# twelve, and a click on one asks `gh` for it whole: it opens in a read-only
# tab with Start Task and Open on GitHub above it. Start Task makes the
# issue's worktree and starts a stand-in Claude there, told only the 0600
# file the issue is written to; again, it goes back to that worktree. Back
# in Agents, Refresh reads again: no issues, then `gh` not signed in; the
# hide control beside them puts Load Issues back, and a new launch
# remembers it. SELFTEST_FRAMES, when set, is a folder to keep the frames
# in.
ID="$T/issuedesk"
IR="$ID/garden-log"
mkdir -p "$IR" "$T/issuebin" "$T/home/issuedesk"
printf 'seeds\n' > "$IR/seeds.txt"
testgit "$IR" init -q -b main
testgit "$IR" config commit.gpgsign false
testgit "$IR" add seeds.txt
testgit "$IR" commit -qm seeds
testgit "$IR" remote add origin https://github.com/example/garden-log.git
cat > "$T/issuebin/gh" <<'GH'
#!/bin/sh
# Each call is logged with the state it answered in.
state=$(cat "$ISSUE_GH_STATE" 2>/dev/null)
echo "$state $*" >> "$ISSUE_GH_LOG"
case "$1 $2" in
"issue list") ;;
"issue view")
    # A body that tries to end a shell's quoting: it must reach Claude
    # only as the file it is written to.
    echo "{\"number\":$3,\"title\":\"Seed tray $3 drains too slowly after watering\",\"state\":\"OPEN\",\"url\":\"https://github.com/example/garden-log/issues/$3\",\"author\":{\"login\":\"sam\"},\"labels\":[{\"name\":\"bug\"}],\"body\":\"Water pools.'; touch pwned; '\",\"comments\":[{\"author\":{\"login\":\"kim\"},\"body\":\"Seen it too.\",\"createdAt\":\"2026-10-02T09:00:00Z\"}],\"createdAt\":\"2026-10-01T12:00:00Z\",\"updatedAt\":\"2026-10-01T12:00:00Z\"}"
    exit 0
    ;;
*)
    echo "no pull requests found" >&2
    exit 1
    ;;
esac
case "$state" in
SLOW | TWELVE)
    [ "$state" = SLOW ] && sleep 3
    printf '['
    n=12
    while [ "$n" -gt 0 ]; do
        printf '{"number":%d,"title":"Seed tray %d drains too slowly after watering","labels":[{"name":"bug"},{"name":"garden"}],"createdAt":"2026-10-0%dT08:00:00Z","updatedAt":"2026-10-09T08:00:00Z","author":{"login":"sam"},"url":"u%d"}' \
            "$n" "$n" "$(( (n % 9) + 1 ))" "$n"
        [ "$n" -gt 1 ] && printf ','
        n=$((n - 1))
    done
    printf ']\n'
    ;;
NONE) echo '[]' ;;
AUTH)
    echo 'To get started with GitHub CLI, please run:  gh auth login' >&2
    exit 4
    ;;
esac
GH
chmod +x "$T/issuebin/gh"
# The stand-in Claude: what it was told, one argument a line after their
# count, then it stays.
cat > "$T/issue-agent.sh" <<'AGENT'
#!/bin/sh
{ echo "argc=$#"; for a in "$@"; do echo "arg=$a"; done; } >> "$ISSUE_AGENT_LOG"
echo "agent on the issue"
sleep 30
AGENT
chmod +x "$T/issue-agent.sh"
: > "$T/issue-agent.log"
printf 'UNASKED\n' > "$T/issue-state"
: > "$T/issue-gh.log"
cat > "$T/issue.script" <<SCRIPT
wait 500
key 0 cmd,shift A
idle 1500
frame $T/issue-unloaded.bmp
dump $T/issue-unloaded.out
write $T/issue-state SLOW
click @inspector.issues_load
idle 1500
frame $T/issue-loading.bmp
dump $T/issue-loading.out
write $T/issue-state TWELVE
idle 3000
frame $T/issue-list.bmp
dump $T/issue-list.out
click @inspector.issues_toggle
idle 300
frame $T/issue-all.bmp
dump $T/issue-all.out
click @inspector.issue_open.1
idle 700
frame $T/issue-tab.bmp
dump $T/issue-open.out
text zz
idle 200
dump $T/issue-typed.out
click @issue.start
idle 2500
dump $T/issue-task.out
key 0 cmd,shift A
idle 700
dump $T/issue-back.out
click @issue.start
idle 2500
dump $T/issue-again.out
write $T/issue-state NONE
click @inspector.refresh
idle 1500
frame $T/issue-none.bmp
dump $T/issue-none.out
write $T/issue-state AUTH
click @inspector.refresh
idle 1500
frame $T/issue-auth.bmp
dump $T/issue-auth.out
write $T/issue-state HIDDEN
click @inspector.issues_hide
idle 300
dump $T/issue-hidden.out
click @inspector.refresh
idle 1500
dump $T/issue-hidden-refresh.out
quit
SCRIPT
HOME="$T/home/issuedesk" PATH="$T/issuebin:$PATH" ISSUE_GH_STATE="$T/issue-state" \
    ISSUE_GH_LOG="$T/issue-gh.log" CRC_SELFTEST_GH=1 CRC_TERMINAL_SHELL=/bin/sh \
    CRC_CLAUDE_COMMAND="$T/issue-agent.sh" ISSUE_AGENT_LOG="$T/issue-agent.log" \
    CRC_WORKTREES="$T/issue-worktrees" \
    CRC_SELFTEST="$T/issue.script" "$BIN" "$IR" 2> "$T/issue.err"
issues_of() {
    grep -m1 '^inspector: ' "$1" 2>/dev/null | sed -n 's/.* issues=\([^ ]*\).*/\1/p'
}
expect_issues() {
    local got
    got=$(issues_of "$1")
    [ "$got" = "$2" ] || failed "agent-issues $(basename "$1"): issues=[$got], expected [$2]: $(grep -m1 '^inspector: ' "$1")"
}
# Before Load Issues: the button, and not one `gh issue list`.
expect_issues "$T/issue-unloaded.out" unloaded
! grep -q '^UNASKED issue list' "$T/issue-gh.log" \
    || failed "agent-issues: gh listed issues before Load Issues: $(cat "$T/issue-gh.log")"
expect_issues "$T/issue-loading.out" loading
expect_issues "$T/issue-list.out" "12:rows=10"
expect_issues "$T/issue-all.out" "12:rows=12"
# The second row is #11: gh is asked for it whole, and it opens in a tab
# of its own, in the editor, that takes no typing.
grep -q 'issue view 11 ' "$T/issue-gh.log" \
    || failed "agent-issues: gh was not asked for #11: $(cat "$T/issue-gh.log")"
expect "$T/issue-open.out" issue_tab \
    "#11 repo=$(cd "$IR" && pwd -P) view_only=true url=https://github.com/example/garden-log/issues/11"
grep -q '^tabs: .*#11 Seed tray 11 drains too slowly after' "$T/issue-open.out" \
    || failed "agent-issues: no #11 tab: $(grep -m1 '^tabs: ' "$T/issue-open.out")"
grep -q '^agents: off' "$T/issue-open.out" \
    || failed "agent-issues: the tab is not in the editor: $(grep -m1 '^agents: ' "$T/issue-open.out")"
expect_line "$T/issue-open.out" 1 "# #11 Seed tray 11 drains too slowly after watering"
expect "$T/issue-typed.out" dirty false
expect_line "$T/issue-typed.out" 1 "# #11 Seed tray 11 drains too slowly after watering"
# Start Task: the issue's worktree, and Claude in it told the file.
issue_branch=issue-11-seed-tray-11-drains-too-slowly-after
issue_trees() {
    testgit "$IR" worktree list --porcelain | grep -c "^branch refs/heads/$issue_branch\$" || true
}
[ "$(issue_trees)" = 1 ] \
    || failed "agent-issues: no $issue_branch worktree: $(testgit "$IR" worktree list)"
grep -q "^agents: on .*selected=$issue_branch" "$T/issue-task.out" \
    || failed "agent-issues: the task is not selected: $(grep -m1 '^agents: ' "$T/issue-task.out")"
issue_file=$(sed -n 's/^arg=Read the GitHub issue in \(.*\) and work on issue #11\.$/\1/p' "$T/issue-agent.log" | head -1)
[ "$(grep -m1 '^argc=' "$T/issue-agent.log")" = "argc=1" ] && [ -n "$issue_file" ] \
    || failed "agent-issues: Claude was not told the issue file: $(cat "$T/issue-agent.log")"
case "$issue_file" in
"$T/home/issuedesk/Library/Application Support/crc/issues/"*-11.md) ;;
*) failed "agent-issues: the issue file is not in Application Support: $issue_file" ;;
esac
[ "$(stat -f %Lp "$issue_file" 2>/dev/null)" = 600 ] \
    || failed "agent-issues: $issue_file is not 0600: $(stat -f %Lp "$issue_file" 2>&1)"
grep -q "Water pools.'; touch pwned; '" "$issue_file" \
    || failed "agent-issues: the issue file lacks the body: $(cat "$issue_file")"
[ -z "$(find "$T" -name pwned)" ] || failed "agent-issues: the issue body ran as a command"
# Back in the editor the tab is there; Start Task again goes back to the
# worktree, with Claude told again, and makes no second one.
grep -q '^issue_tab: #11 ' "$T/issue-back.out" \
    || failed "agent-issues: the tab is gone: $(grep -m1 '^issue_tab: ' "$T/issue-back.out")"
[ "$(issue_trees)" = 1 ] && [ "$(testgit "$IR" worktree list | wc -l | tr -d ' ')" = 2 ] \
    || failed "agent-issues: Start Task twice made another worktree: $(testgit "$IR" worktree list)"
[ "$(grep -c '^argc=1$' "$T/issue-agent.log")" = 2 ] \
    || failed "agent-issues: Claude did not start again in the worktree: $(cat "$T/issue-agent.log")"
grep -q "^agents: on .*selected=$issue_branch" "$T/issue-again.out" \
    || failed "agent-issues: the task is not selected again: $(grep -m1 '^agents: ' "$T/issue-again.out")"
expect_issues "$T/issue-none.out" 0
expect_issues "$T/issue-auth.out" auth
# Hidden again: Load Issues, and Refresh does not ask `gh` for them.
expect_issues "$T/issue-hidden.out" unloaded
expect_issues "$T/issue-hidden-refresh.out" unloaded
! grep -q '^HIDDEN issue list' "$T/issue-gh.log" \
    || failed "agent-issues: gh listed hidden issues: $(cat "$T/issue-gh.log")"
# Both are remembered: a new launch has Load Issues again, and once it is
# clicked, the launch after that reads them without a click.
printf 'TWELVE\n' > "$T/issue-state"
cat > "$T/issue-relaunch.script" <<SCRIPT
wait 500
key 0 cmd,shift A
idle 1500
dump $T/issue-relaunch.out
click @inspector.issues_load
idle 300
quit
SCRIPT
HOME="$T/home/issuedesk" PATH="$T/issuebin:$PATH" ISSUE_GH_STATE="$T/issue-state" \
    ISSUE_GH_LOG="$T/issue-gh.log" CRC_SELFTEST_GH=1 CRC_TERMINAL_SHELL=/bin/sh \
    CRC_SELFTEST="$T/issue-relaunch.script" "$BIN" "$IR" 2> "$T/issue-relaunch.err"
expect_issues "$T/issue-relaunch.out" unloaded
cat > "$T/issue-kept.script" <<SCRIPT
wait 500
key 0 cmd,shift A
idle 2000
dump $T/issue-kept.out
quit
SCRIPT
HOME="$T/home/issuedesk" PATH="$T/issuebin:$PATH" ISSUE_GH_STATE="$T/issue-state" \
    ISSUE_GH_LOG="$T/issue-gh.log" CRC_SELFTEST_GH=1 CRC_TERMINAL_SHELL=/bin/sh \
    CRC_SELFTEST="$T/issue-kept.script" "$BIN" "$IR" 2> "$T/issue-kept.err"
expect_issues "$T/issue-kept.out" "12:rows=10"
[ -z "${SELFTEST_FRAMES:-}" ] || cp -f "$T/issue-unloaded.bmp" "$T/issue-loading.bmp" \
    "$T/issue-list.bmp" "$T/issue-all.bmp" "$T/issue-tab.bmp" "$T/issue-none.bmp" \
    "$T/issue-auth.bmp" "$SELFTEST_FRAMES/"
