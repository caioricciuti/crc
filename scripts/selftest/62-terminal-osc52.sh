# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh osc52.

# ---- terminal copy through OSC 52 ---------------------------------------------
# A program that copies through OSC 52, as Claude Code does with a drag it
# was sent, puts the text on the pasteboard: pasting into cat writes it.
# A read (`?`) after it is refused and changes nothing, and the shell hears
# no reply. The self-test copies to a private pasteboard, not the general
# one, which outlives the run: the text is new each time, so an earlier
# run's copy cannot pass for this one's.
mkdir -p "$T/osc52proj"
printf 'untouched\n' > "$T/osc52proj/notes.txt"
osc52_want="copied by osc 52 $$-$RANDOM"
osc52_coded=$(printf '%s' "$osc52_want" | base64 | tr -d '\n')
printf '#!/bin/sh\nprintf '"'"'\\033]52;c;%s\\007\\033]52;c;?\\033\\\\'"'"'\n' \
    "$osc52_coded" > "$T/osc52proj/copy.sh"
chmod +x "$T/osc52proj/copy.sh"
cat > "$T/osc52.script" <<SCRIPT
wait 500
click @toolbar.terminal
wait 700
wait 700
text cd $T/osc52proj
key 36
text PS1=
key 36
text ./copy.sh
key 36
wait 500
text cat > pasted.txt
key 36
wait 300
key 9 cmd v
key 36
key 2 ctrl d
wait 500
wait 500
dump $T/osc52-pasted.out
quit
SCRIPT
CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/osc52.script" "$BIN" "$T/osc52proj/notes.txt" 2> "$T/osc52.err"
if [ "$(cat "$T/osc52proj/pasted.txt" 2>/dev/null)" != "$osc52_want" ]; then
    failed "pasted.txt: pasted [$(cat "$T/osc52proj/pasted.txt" 2>/dev/null)], expected [$osc52_want]"
fi
expect_line "$T/osc52-pasted.out" 1 "untouched"
