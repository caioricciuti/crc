#!/usr/bin/env bash
# Plays input into the real app and checks what came out.
#
# The unit tests cannot reach window.rs, which is behind AppKit, and the bugs
# that mattered have been there: clicks landing rows away from the pointer
# under a fully green suite. This drives the actual window with real NSEvents
# (see src/platform/selftest.rs) and reads the document back.
#
# It opens a window for a few seconds. It does not need the app to be in
# front, and it never touches the saved session: the app is launched on a
# file, which makes the session ephemeral, and quits without saving.
#
# What it cannot check is anything that needs the app to be frontmost, which
# includes dead keys: the input system only composes for the active app.
#
# Usage: scripts/selftest.sh        (builds first)
#        scripts/selftest.sh --debug  (a debug build, which checks every
#                                      Objective-C message's types)
#        scripts/selftest.sh terminal conflicts   (only the scenarios in
#                                      scripts/selftest/ whose file names
#                                      contain one of the words)
#
# The scenarios are scripts/selftest/NN-*.sh, run in order in this shell.
set -euo pipefail
# A Mac that sleeps mid-run fails scenarios for no fault of their own (see
# slept_note below), so the whole run holds the machine awake. Once: the
# re-exec marks itself so the inner run goes straight on.
if [ -z "${CRC_SELFTEST_CAFFEINATED:-}" ] && command -v caffeinate >/dev/null 2>&1; then
    CRC_SELFTEST_CAFFEINATED=1 exec caffeinate -i "$BASH" "$0" "$@"
fi
cd "$(dirname "$0")/.."

# now: seconds since the epoch, with a fraction. macOS's date has no %N and
# this bash is 3.2, without EPOCHREALTIME, so python3 tells the time.
now() {
    python3 -c 'import time;print(time.time())'
}
# elapsed <start>: seconds since <start>, to a tenth.
elapsed() {
    python3 -c "import time;print(f'{time.time()-$1:.1f}')"
}
run_start=$(now)

debug=0
only=""
for arg in "$@"; do
    case "$arg" in
        --debug) debug=1 ;;
        *) only="$only $arg" ;;
    esac
done
build_start=$(now)
if [ "$debug" = 1 ]; then
    cargo build --locked
    BIN=target/debug/crc
else
    cargo build --release --locked
    BIN=target/release/crc
fi
build_secs=$(elapsed "$build_start")
T=$(mktemp -d)
# A Mac that sleeps mid-run (a closed lid) goes on only in short dark
# wakes with the display off: no frames are drawn and every wait runs
# long, so the scenarios then fail for no fault of their own. Say so.
started=$(date '+%Y-%m-%d %H:%M:%S')
slept_note() {
    local slept
    slept=$(pmset -g log 2>/dev/null \
        | awk -v since="$started" 'substr($0, 1, 19) >= since && $4 == "Sleep" { print $2; exit }' \
        || true)
    if [ -n "$slept" ]; then
        echo "Note: the Mac went to sleep during this run (at $slept); failures after that may be the sleep's. Run again with it awake."
    fi
}
# Terminals run in helper processes that outlive each app launch (see
# src/term/hold.rs); the run's are ended with it.
trap 'result=$?; pkill -f -- "--hold $T/home/" 2>/dev/null || true; if [ "$result" -eq 0 ]; then rm -rf "$T"; else slept_note; echo "Self-test failure artifacts: $T (what failed: $T/selftest.log)"; fi' EXIT
# Everything said from here is also kept with the artifacts, so a failure
# seen once can still be named after the terminal has scrolled past it.
exec > >(tee "$T/selftest.log") 2>&1
# The build ran before the log existed; its time is said now so the log has
# every timing line, one per step, and a summary at the end.
echo "timing: build $build_secs"
timings=""
fail=0
# Every launch serves Claude Code for its project. Keep the lock files out of
# the real ~/.claude/ide, where a running claude would offer these windows.
export CLAUDE_CONFIG_DIR="$T/claude-config"
# The person running this has settings of their own: a zoomed font moves
# every click target by a column, a light theme changes what a dump says.
# Every launch gets an empty home, so the checks below see the defaults.
# Scenarios that need a settings file make one under their own HOME.
mkdir -p "$T/home"
export HOME="$T/home"
unset XDG_CONFIG_HOME
# Local history goes to the run's own file, whatever HOME a scenario sets:
# no launch may write the real one in Application Support.
export CRC_HISTORY_DB="$T/history.db"

# expect <file> <field> <value>
# A missing field or dump is a FAIL like any other, not the end of the run:
# under `set -euo pipefail` grep's exit 1 used to stop everything after it.
expect() {
    local got
    got=$(grep -m1 "^$2: " "$1" 2>/dev/null | sed "s/^$2: //" || true)
    if [ "$got" != "$3" ]; then
        echo "FAIL $(basename "$1"): $2 is [$got], expected [$3]"
        fail=1
    fi
}
# expect_line <file> <n> <text>: line n of the document
expect_line() {
    local got
    got=$(sed -n '/^text:$/,$p' "$1" 2>/dev/null | sed -n "$(($2 + 1))p" || true)
    if [ "$got" != "$3" ]; then
        echo "FAIL $(basename "$1"): line $2 is [$got], expected [$3]"
        fail=1
    fi
}

# failed <message>: a check that did not hold.
failed() {
    echo "FAIL $1"
    fail=1
}
# testgit <dir> <args>: git in a fixture, with a test identity and no
# signing, so no one's own git settings reach a commit made here.
testgit() {
    local dir=$1
    shift
    git -C "$dir" -c user.name=Tester -c user.email=t@example.com -c commit.gpgsign=false "$@"
}

# Geometry the scripts rely on, with the sidebar hidden: a one-digit gutter is
# 3 cells of 8pt, the text starts 116pt down (toolbar 48 + tabs 30 + breadcrumbs 30 + 8 of air), and a
# line is 19pt. So line n is centred at y = 116 + 19 * (n - 1) + 9.

mgit() {
    testgit "$T/mergeproj" -c merge.conflictStyle=zdiff3 -c rerere.enabled=false "$@"
}

# expect_claude <python expression over the log `j`> <description>
expect_claude() {
    if ! python3 -c "import json,sys; j=json.load(open('$T/claude.json')); sys.exit(0 if ($1) else 1)" 2>/dev/null; then
        echo "FAIL claude: $2"
        fail=1
    fi
}

# expect_terminal <file> <fixed text the terminal line must contain>
expect_terminal() {
    if ! grep -m1 '^terminal: ' "$1" | grep -qF -- "$2"; then
        echo "FAIL $(basename "$1"): terminal line lacks [$2]: $(grep -m1 '^terminal: ' "$1")"
        fail=1
    fi
}

ran=""
for scenario in scripts/selftest/[0-9][0-9]-*.sh; do
    name=$(basename "$scenario" .sh)
    if [ -n "$only" ]; then
        wanted=0
        for word in $only; do
            case "$name" in *"$word"*) wanted=1 ;; esac
        done
        [ "$wanted" = 1 ] || continue
    fi
    scenario_start=$(now)
    # shellcheck source=/dev/null
    . "$scenario"
    scenario_secs=$(elapsed "$scenario_start")
    echo "timing: ${name#[0-9][0-9]-} $scenario_secs"
    timings="$timings${name#[0-9][0-9]-} $scenario_secs
"
    # Every launch takes back the sessions helpers hold, whatever folder it
    # opens: a scenario's terminals, and their records, end with it.
    pkill -f -- "--hold $T/home/" 2>/dev/null || true
    find "$T/home" -type d -path '*/crc/held' -prune -exec rm -rf {} + 2>/dev/null || true
    ran="$ran ${name#[0-9][0-9]-}"
done
if [ -z "$ran" ]; then
    echo "selftest: no scenario matches:$only"
    exit 1
fi

# "slow frame" lines are the app's own latency log, and a machine busy
# compiling will produce them. Anything else on stderr is a finding.
if ls "$T"/*.err >/dev/null 2>&1 && cat "$T"/*.err | grep -v '^slow frame' | grep -q .; then
    echo "FAIL: the app wrote to stderr:"
    cat "$T"/*.err | grep -v '^slow frame'
    fail=1
fi

# Where the time went: the whole run, the build, the scenarios together,
# and the five that took longest, so a slow suite names its slow parts.
echo "timing: total $(elapsed "$run_start")"
echo "timing: build $build_secs"
echo "timing: scenarios $(printf '%s' "$timings" | awk '{ s += $2 } END { printf "%.1f", s }')"
printf '%s' "$timings" | sort -k2 -g -r | head -5 | while read -r slow_name slow_secs; do
    echo "timing: slowest $slow_name $slow_secs"
done

if [ "$fail" = 0 ]; then
    if [ -n "$only" ]; then
        echo "selftest:$ran pass"
    else
        echo "selftest: editing, project search, native previews, HTTP requests, Claude Code, the terminal and audit crash cases pass"
    fi
fi
exit $fail
