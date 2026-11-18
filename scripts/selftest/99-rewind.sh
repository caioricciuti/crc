# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh rewind.

# ---- Rewind: a session's turns, and a file as a turn left it ----------
# A stand-in agent is sent two prompts. In the first it writes notes.md
# through the Write hooks, which keep the agent's version in local
# history, and runs `echo ok`; Stop ends the turn. A second later the
# second prompt rewrites notes.md, writes plan.md and .env (a secret, of
# which no version is kept) and runs a failing `cargo test`. In Agents
# mode the inspector's Rewind lists both turns, newest first. A click on
# the newest lists its three files and its command; a click on the older
# one and then its notes.md opens File History at the first turn's
# version, not the newest. Its title folds it, and View > Show Rewind
# opens the hidden inspector with it unfolded. Then the newest's notes.md opens at its own,
# and its .env, with no version, opens as it is, said on the status line
# and on its row. The frames are for a look at the section in both
# themes.
case "$BIN" in /*) rw_bin=$BIN ;; *) rw_bin=$PWD/$BIN ;; esac
rw_proj="$T/rewindproj"
mkdir -p "$rw_proj"
printf 'x\n' > "$rw_proj/notes.md"
testgit "$rw_proj" init -q -b main
testgit "$rw_proj" add notes.md
testgit "$rw_proj" commit -qm one
cat > "$T/rewind-agent.sh" <<AGENT
#!/bin/sh
# rw_write <file> <text>: the file written between the Write hooks.
rw_write() {
    printf '{"tool_name":"Write","tool_input":{"file_path":"%s/%s"}}' "$rw_proj" "\$1" | "$rw_bin" --hook pre
    printf '%s\n' "\$2" > "$rw_proj/\$1"
    printf '{"tool_name":"Write","tool_input":{"file_path":"%s/%s"}}' "$rw_proj" "\$1" | "$rw_bin" --hook post
}
printf '{"prompt":"write the notes"}' | "$rw_bin" --hook prompt
rw_write notes.md 'first notes'
printf '{"tool_name":"Bash","tool_input":{"command":"echo ok"},"tool_response":{"stdout":"ok"}}' | "$rw_bin" --hook ran
printf '{"hook_event_name":"Stop","last_assistant_message":"Wrote the notes."}' | "$rw_bin" --hook stop
sleep 1.2
printf '{"prompt":"rewrite the notes and add a plan"}' | "$rw_bin" --hook prompt
rw_write notes.md 'second notes'
rw_write plan.md 'the plan'
rw_write .env 'TOKEN=1'
printf '{"tool_name":"Bash","tool_input":{"command":"cargo test --offline"},"error":"Exit code 101","is_interrupt":false}' | "$rw_bin" --hook ran-failed
printf '{"hook_event_name":"Stop","last_assistant_message":"Done."}' | "$rw_bin" --hook stop
printf 'done\n'
sleep 30
AGENT
chmod +x "$T/rewind-agent.sh"

# rw_launch <run> <theme>: one crc run with the stand-in, in a home and a
# local history of its own, playing $T/rewind-<run>.script.
rw_launch() {
    mkdir -p "$T/home/rewind-$1/.config/crc"
    printf 'theme = "%s"\n' "$2" > "$T/home/rewind-$1/.config/crc/config.toml"
    HOME="$T/home/rewind-$1" CRC_HISTORY_DB="$T/rewind-$1.db" CRC_TERMINAL_SHELL=/bin/sh \
        CRC_CLAUDE_COMMAND="$T/rewind-agent.sh" \
        CRC_SELFTEST="$T/rewind-$1.script" "$BIN" "$rw_proj" 2> "$T/rewind-$1.${3:-err}"
}
# rw_scrub <dump>: the scrub line without its counts of what is loaded.
rw_scrub() {
    grep -m1 '^scrub: ' "$1" | sed -e 's/^scrub: //' -e 's/ loaded=[0-9]* pending=[A-Za-z0-9()]*//'
}

# Turn 1's notes.md: the first of its two agent versions.
cat > "$T/rewind-dark.script" <<SCRIPT
wait 500
key 8 cmd,shift C
wait 700
wait 700
wait 700
wait 700
key 0 cmd,shift A
wait 700
wait 500
dump $T/rewind-dark.out
frame $T/rewind-dark.png
click @inspector.rewind_fold
wait 200
dump $T/rewind-dark-folded.out
action toggleInspector:
wait 200
action showRewind:
wait 300
dump $T/rewind-dark-shown.out
click @inspector.rewind_turn.0
wait 300
dump $T/rewind-dark-newest.out
frame $T/rewind-dark-newest.png
click @inspector.rewind_turn.1
wait 300
dump $T/rewind-dark-older.out
click @inspector.rewind_file.0
wait 700
wait 700
dump $T/rewind-dark-file.out
quit
SCRIPT
rw_launch dark dark
expect "$T/rewind-dark.out" rewind "sessions=1 turns=2 rows=2 sel=- files=- cmds=0 missing=-"
# Its title folds it; View > Show Rewind, with the inspector hidden,
# brings both back.
expect "$T/rewind-dark-folded.out" rewind "folded turns=2"
expect "$T/rewind-dark-shown.out" rewind "sessions=1 turns=2 rows=2 sel=- files=- cmds=0 missing=-"
case "$(grep -m1 '^inspector: ' "$T/rewind-dark-shown.out")" in
    'inspector: hidden') failed "rewind-dark-shown.out: Show Rewind left the inspector hidden" ;;
esac
expect "$T/rewind-dark-newest.out" rewind "sessions=1 turns=2 rows=2 sel=2 files=notes.md,plan.md,.env cmds=1 missing=-"
expect "$T/rewind-dark-older.out" rewind "sessions=1 turns=2 rows=2 sel=1 files=notes.md cmds=1 missing=-"
rw_got=$(rw_scrub "$T/rewind-dark-file.out")
[ "$rw_got" = "2/3@local:by agent first=first notes" ] \
    || failed "rewind-dark-file.out: scrub is [$rw_got], expected [2/3@local:by agent first=first notes]"

# Turn 2: .env has no version, so it opens as it is and its row says so
# once Agents mode shows again; notes.md opens at the turn's version.
cat > "$T/rewind-light.script" <<SCRIPT
wait 500
key 8 cmd,shift C
wait 700
wait 700
wait 700
wait 700
key 0 cmd,shift A
wait 700
wait 500
click @inspector.rewind_turn.0
wait 300
frame $T/rewind-light.png
click @inspector.rewind_file.2
wait 700
wait 500
dump $T/rewind-light-env.out
key 0 cmd,shift A
wait 700
wait 500
dump $T/rewind-light-missing.out
frame $T/rewind-light-missing.png
click @inspector.rewind_file.0
wait 700
wait 700
dump $T/rewind-light-file.out
quit
SCRIPT
rw_launch light light
expect "$T/rewind-light-env.out" message "No saved agent version of .env for turn 2"
expect "$T/rewind-light-missing.out" rewind "sessions=1 turns=2 rows=2 sel=2 files=notes.md,plan.md,.env cmds=1 missing=.env"
rw_got=$(rw_scrub "$T/rewind-light-file.out")
[ "$rw_got" = "3/3@local:by agent first=second notes" ] \
    || failed "rewind-light-file.out: scrub is [$rw_got], expected [3/3@local:by agent first=second notes]"

# Restore to a turn, from a project as it was before the first run. Back
# to turn 1: notes.md takes turn 1's version, and plan.md and .env, which
# the agent created in turn 2, go. Then forward to turn 2: notes.md and
# plan.md come back, said changed since the agent left them (the restore
# changed them); .env, a secret with no version kept, is left as it is.
rm -f "$rw_proj/plan.md" "$rw_proj/.env"
printf 'x\n' > "$rw_proj/notes.md"
cat > "$T/rewind-restore.script" <<SCRIPT
wait 500
key 8 cmd,shift C
wait 700
wait 700
wait 700
wait 700
key 0 cmd,shift A
wait 700
wait 500
click @inspector.rewind_turn.1
wait 300
frame $T/rewind-restore.png
click @inspector.rewind_restore
wait 700
wait 500
dump $T/rewind-restore-1.out
click @inspector.rewind_turn.0
wait 300
click @inspector.rewind_restore
wait 700
wait 500
dump $T/rewind-restore-2.out
quit
SCRIPT
# Its stderr holds the questions a test instance prints instead of asking,
# so it is a .log, checked here for anything else.
rw_launch restore dark log
if grep -v '^crc: restore to turn: ' "$T/rewind-restore.log" | grep -v '^slow frame' | grep -q .; then
    failed "rewind-restore.log: the app wrote to stderr: $(cat "$T/rewind-restore.log")"
fi
expect "$T/rewind-restore-1.out" message "Restored 3 files to turn 1"
expect "$T/rewind-restore-2.out" message "Restored 2 files to turn 2"
[ "$(cat "$rw_proj/notes.md")" = "second notes" ] \
    || failed "rewind restore: notes.md is [$(cat "$rw_proj/notes.md")], expected [second notes]"
[ "$(cat "$rw_proj/plan.md" 2>/dev/null)" = "the plan" ] \
    || failed "rewind restore: plan.md did not come back with turn 2"
[ ! -e "$rw_proj/.env" ] || failed "rewind restore: .env came back with no version kept"
grep -q 'restore to turn: Restore 3 files to how turn 1 left them? notes.md, plan.md, .env' "$T/rewind-restore.log" \
    || failed "rewind-restore.log: no question for turn 1"
grep -q 'Changed since the agent left them, and that change is replaced: notes.md, plan.md' "$T/rewind-restore.log" \
    || failed "rewind-restore.log: turn 2's question does not name the changed files"
grep -q 'Left as they are: .env (no saved version from turn 2)' "$T/rewind-restore.log" \
    || failed "rewind-restore.log: turn 2's question does not name .env"
