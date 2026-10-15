# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh launch-agents.

# ---- A plain launch opens on Agents mode -------------------------------
# Launched with nothing to open, as from the Dock, the window opens on
# Agents mode with a shell started and the keyboard in it: what is typed
# first lands in the terminal. Launched on a file, it opens the editor on
# that file. A test instance keeps the editor unless it asks for a plain
# launch, which these do; a home of their own keeps the other scenarios'
# held sessions out.
mkdir -p "$T/home/launch" "$T/launchproj"
printf 'kept\n' > "$T/launchproj/a.txt"
cat > "$T/launch-plain.script" <<SCRIPT
wait 900
text echo launched-agents
key 36
wait 700
dump $T/launch-plain.out
quit
SCRIPT
HOME="$T/home/launch" CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST_PLAIN_LAUNCH=1 \
    CRC_SELFTEST="$T/launch-plain.script" "$BIN" 2> "$T/launch-plain.err"
grep -q '^agents: on ' "$T/launch-plain.out" \
    || failed "launch-plain.out: $(grep -m1 '^agents: ' "$T/launch-plain.out")"
expect_terminal "$T/launch-plain.out" "open=true focus=true"
expect_terminal "$T/launch-plain.out" "launched-agents"

cat > "$T/launch-file.script" <<SCRIPT
wait 500
dump $T/launch-file.out
quit
SCRIPT
HOME="$T/home/launch" CRC_SELFTEST_PLAIN_LAUNCH=1 \
    CRC_SELFTEST="$T/launch-file.script" "$BIN" "$T/launchproj/a.txt" 2> "$T/launch-file.err"
grep -q '^agents: off ' "$T/launch-file.out" \
    || failed "launch-file.out: $(grep -m1 '^agents: ' "$T/launch-file.out")"
expect "$T/launch-file.out" tabs "a.txt"
