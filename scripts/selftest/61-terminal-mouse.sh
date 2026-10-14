# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh mouse.

# ---- terminal mouse reporting ------------------------------------------------
# A full-screen program that asks for the mouse (1000, SGR 1006) and for
# focus reports (1004) hears each click as a press and a release at its
# 1-based cell, the wheel as buttons 64 and 65, and the keyboard leaving and
# coming back as CSI O and CSI I. Option-click is crc's: it selects and the
# program hears nothing. The stand-in puts its tty in raw mode and keeps
# every byte it is sent; it says its size first, for the far corner.
mkdir -p "$T/mouseproj"
printf 'untouched\n' > "$T/mouseproj/notes.txt"
printf '#!/bin/sh\nstty raw -echo\nstty size > %s/mouse.size\nprintf '"'"'mouse ready\\r\\n\\033[?1000h\\033[?1006h\\033[?1004h'"'"'\nexec cat > %s/mouse.bytes\n' \
    "$T" "$T" > "$T/mouse.sh"
chmod +x "$T/mouse.sh"
cat > "$T/mouse.script" <<SCRIPT
wait 500
click @toolbar.terminal
wait 700
wait 700
clickin @terminal 3 3
clickat @terminal 0.999 0.999
clickin @terminal 3 3 3 opt
wait 200
dump $T/mouse-option.out
wheelat @terminal 0 0 60
wheelat @terminal 0 0 -60
click @text
wait 200
clickin @terminal 3 3
wait 500
dump $T/mouse-done.out
quit
SCRIPT
CRC_TERMINAL_SHELL="$T/mouse.sh" CRC_SELFTEST="$T/mouse.script" "$BIN" "$T/mouseproj/notes.txt" 2> "$T/mouse.err"
# Option-click selected instead of reporting.
expect_terminal "$T/mouse-option.out" 'selection=Some("mouse ready")'
read -r mouse_rows mouse_cols < "$T/mouse.size" 2>/dev/null || true
mouse_got=$(LC_ALL=C tr '\033' 'E' < "$T/mouse.bytes" 2>/dev/null || true)
mouse_want="^E\[<0;1;1ME\[<0;1;1mE\[<0;${mouse_cols:-?};${mouse_rows:-?}ME\[<0;${mouse_cols:-?};${mouse_rows:-?}m(E\[<64;1;1M)+(E\[<65;1;1M)+E\[OE\[IE\[<0;1;1ME\[<0;1;1m$"
if ! printf '%s' "$mouse_got" | grep -Eq "$mouse_want"; then
    failed "mouse.bytes: the program heard [$mouse_got] (ESC as E), expected /$mouse_want/"
fi
expect_line "$T/mouse-done.out" 1 "untouched"

# A program that never asks keeps the mouse crc's: a triple click selects
# its line and the wheel scrolls back through the history.
printf '#!/bin/sh\nfor i in $(seq 1 80); do echo "plain line $i"; done\nsleep 30\n' > "$T/mouseoff.sh"
chmod +x "$T/mouseoff.sh"
cat > "$T/mouseoff.script" <<SCRIPT
wait 500
click @toolbar.terminal
wait 700
wait 700
clickin @terminal 3 3 3
wait 200
dump $T/mouseoff-select.out
wheelat @terminal 0.5 0.5 60
wait 200
dump $T/mouseoff-wheel.out
quit
SCRIPT
CRC_TERMINAL_SHELL="$T/mouseoff.sh" CRC_SELFTEST="$T/mouseoff.script" "$BIN" "$T/mouseproj/notes.txt" 2> "$T/mouseoff.err"
expect_terminal "$T/mouseoff-select.out" 'selection=Some("plain line '
if ! grep -m1 '^terminal: ' "$T/mouseoff-wheel.out" | grep -Eq ' back=[1-9]'; then
    failed "mouseoff-wheel.out: the wheel did not scroll the history: $(grep -m1 '^terminal: ' "$T/mouseoff-wheel.out")"
fi
