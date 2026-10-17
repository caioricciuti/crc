# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh agents-wheel.

# ---- The wheel over the Agents list and over a full-screen program --------
# Thirty workspaces from the kept list overflow the Agents sidebar: the
# wheel over it scrolls the list, down to where its last row meets the
# bottom and no further, and back up to the first. Then a terminal: a
# program on the alternate screen that did not ask for the mouse (a pager)
# hears each line of the wheel as an arrow key, Up or Down, in the cursor
# mode it set (xterm's alternate scroll); one that asked for the mouse
# hears wheel buttons as before. The stand-ins keep every byte sent.
mkdir -p "$T/wheelproj"
printf 'x\n' > "$T/wheelproj/x.txt"
wheel_kept="$T/home/wheel/Library/Application Support/crc/agents"
mkdir -p "$wheel_kept"
: > "$wheel_kept/workspaces"
for i in $(seq -w 1 30); do
    mkdir -p "$T/wheel/ws$i"
    printf 'ws%s\t%s/wheel/ws%s\tfolder:ws%s\n' "$i" "$T" "$i" "$i" >> "$wheel_kept/workspaces"
done
cat > "$T/wheel.script" <<SCRIPT
wait 500
key 0 cmd,shift A
wait 900
dump $T/wheel-top.out
wheelat @agents.new 0.5 0.5 -200
wait 200
dump $T/wheel-down.out
wheelat @agents.new 0.5 0.5 -5000
wait 200
dump $T/wheel-end.out
wheelat @agents.new 0.5 0.5 5000
wait 200
dump $T/wheel-back.out
quit
SCRIPT
HOME="$T/home/wheel" CRC_TERMINAL_SHELL=/bin/sh CRC_SELFTEST="$T/wheel.script" "$BIN" "$T/wheelproj" 2> "$T/wheel.err"
wheel_tasks=$(grep -m1 '^agents: ' "$T/wheel-top.out" | grep -o 'tasks=[^ ]*' | tr ',' '\n' | wc -l | tr -d ' ')
[ "$wheel_tasks" -ge 30 ] || failed "wheel-top.out: expected 30 workspaces listed, got $wheel_tasks: $(grep -m1 '^agents: ' "$T/wheel-top.out")"
expect "$T/wheel-top.out" agents_scroll "0"
wheel_down=$(grep -m1 '^agents_scroll: ' "$T/wheel-down.out" | sed 's/^agents_scroll: //')
wheel_end=$(grep -m1 '^agents_scroll: ' "$T/wheel-end.out" | sed 's/^agents_scroll: //')
[ "${wheel_down:-0}" -gt 0 ] || failed "wheel-down.out: the wheel did not scroll the Agents list: agents_scroll is [$wheel_down]"
# Past the end the list stops where its last row meets the bottom: more
# wheel moves it no further than the rows allow, and never off the list.
[ "${wheel_end:-0}" -ge "${wheel_down:-0}" ] && [ "${wheel_end:-99}" -lt 30 ] \
    || failed "wheel-end.out: the list did not stop at its end: agents_scroll is [$wheel_end] after [$wheel_down]"
expect "$T/wheel-back.out" agents_scroll "0"

# A pager: the alternate screen, no mouse. The wheel is Up and Down arrows.
cat > "$T/alt.sh" <<SHELL
#!/bin/sh
stty raw -echo
printf '\033[?1049hpager ready\r\n'
exec cat > "$T/alt.bytes"
SHELL
chmod +x "$T/alt.sh"
cat > "$T/alt.script" <<SCRIPT
wait 500
click @toolbar.terminal
wait 700
wait 700
wheelat @terminal 0 0 60
wait 200
wheelat @terminal 0 0 -60
wait 300
dump $T/alt-done.out
quit
SCRIPT
CRC_TERMINAL_SHELL="$T/alt.sh" CRC_SELFTEST="$T/alt.script" "$BIN" "$T/wheelproj/x.txt" 2> "$T/alt.err"
alt_got=$(LC_ALL=C tr '\033' 'E' < "$T/alt.bytes" 2>/dev/null || true)
if ! printf '%s' "$alt_got" | grep -Eq '^(E\[A)+(E\[B)+$'; then
    failed "alt.bytes: the pager heard [$alt_got] (ESC as E), expected Up arrows then Down arrows"
fi
# Nothing to scroll back through on the alternate screen.
expect_terminal "$T/alt-done.out" "back=0"

# The same pager in application cursor mode (DECCKM): the SS3 form.
cat > "$T/altapp.sh" <<SHELL
#!/bin/sh
stty raw -echo
printf '\033[?1049h\033[?1hpager ready\r\n'
exec cat > "$T/altapp.bytes"
SHELL
chmod +x "$T/altapp.sh"
sed "s#alt-done.out#altapp-done.out#" "$T/alt.script" > "$T/altapp.script"
CRC_TERMINAL_SHELL="$T/altapp.sh" CRC_SELFTEST="$T/altapp.script" "$BIN" "$T/wheelproj/x.txt" 2> "$T/altapp.err"
altapp_got=$(LC_ALL=C tr '\033' 'E' < "$T/altapp.bytes" 2>/dev/null || true)
if ! printf '%s' "$altapp_got" | grep -Eq '^(EOA)+(EOB)+$'; then
    failed "altapp.bytes: the pager heard [$altapp_got] (ESC as E), expected SS3 Up arrows then Down arrows"
fi

# A full-screen program that asked for the mouse still hears the wheel as
# buttons 64 and 65 at the cell, not as arrows.
cat > "$T/altmouse.sh" <<SHELL
#!/bin/sh
stty raw -echo
printf '\033[?1049h\033[?1000h\033[?1006hmouse ready\r\n'
exec cat > "$T/altmouse.bytes"
SHELL
chmod +x "$T/altmouse.sh"
sed "s#alt-done.out#altmouse-done.out#" "$T/alt.script" > "$T/altmouse.script"
CRC_TERMINAL_SHELL="$T/altmouse.sh" CRC_SELFTEST="$T/altmouse.script" "$BIN" "$T/wheelproj/x.txt" 2> "$T/altmouse.err"
altmouse_got=$(LC_ALL=C tr '\033' 'E' < "$T/altmouse.bytes" 2>/dev/null || true)
if ! printf '%s' "$altmouse_got" | grep -Eq '^(E\[<64;1;1M)+(E\[<65;1;1M)+$'; then
    failed "altmouse.bytes: the program heard [$altmouse_got] (ESC as E), expected SGR wheel buttons"
fi
