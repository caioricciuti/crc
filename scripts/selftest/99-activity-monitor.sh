# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh activity-monitor.

# ---- Activity: the whole Mac's load at the foot of the Agents sidebar ----
# In Agents mode the panel shows open: its worker reads within a couple of
# seconds, and the panel lists the three busiest processes. Its title row
# folds it, which stops the worker and forgets the read, and the fold is
# kept: the next launch starts folded. Opened again, it starts over from
# its skeleton until the first read lands. The frames are for a look at the
# panel in both themes.
P="$T/activityproj"
mkdir -p "$P"
printf 'x\n' > "$P/a.txt"

# am_run <name> <theme> <steps>: one crc run in Agents mode, in a home of
# its own per theme, so the second dark run finds the first one's fold.
am_run() {
    printf 'wait 500\nkey 0 cmd,shift A\nwait 700\n%b\nquit\n' "$3" > "$T/activity-$1.script"
    mkdir -p "$T/home/activity-$2/.config/crc"
    printf 'theme = "%s"\n' "$2" > "$T/home/activity-$2/.config/crc/config.toml"
    HOME="$T/home/activity-$2" CRC_TERMINAL_SHELL=/bin/sh \
        CRC_SELFTEST="$T/activity-$1.script" "$BIN" "$P" 2> "$T/activity-$1.err"
}
am_run dark dark "wait 2500\ndump $T/activity-dark.out\nframe $T/activity-dark.png\nclick @agents.activity\nwait 400\ndump $T/activity-folded.out"
expect "$T/activity-dark.out" monitor "folded=no running=yes sampled=yes rows=3"
expect "$T/activity-folded.out" monitor "folded=yes running=no sampled=no rows=0"
grep -qx 'activity-folded' "$T/home/activity-dark/Library/Application Support/crc/agents/order" 2>/dev/null \
    || failed "activity: the fold was not kept in the arrangement file"
am_run kept dark "dump $T/activity-kept.out\nclick @agents.activity\nwait 200\ndump $T/activity-reopened.out\nwait 2500\ndump $T/activity-again.out"
expect "$T/activity-kept.out" monitor "folded=yes running=no sampled=no rows=0"
expect "$T/activity-reopened.out" monitor "folded=no running=yes sampled=no rows=0"
expect "$T/activity-again.out" monitor "folded=no running=yes sampled=yes rows=3"
am_run light light "wait 2500\ndump $T/activity-light.out\nframe $T/activity-light.png"
expect "$T/activity-light.out" monitor "folded=no running=yes sampled=yes rows=3"
