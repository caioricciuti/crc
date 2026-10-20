# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh links.

# ---- terminal links -----------------------------------------------------------
# A program that asks for the mouse, as Claude Code does, prints a web
# address, an OSC 8 link whose visible text is not its target, and an
# address too long for a row, which the terminal wraps onto the next.
# Holding Command over each underlines it (the dump names the link and its
# columns, and a frame shows the underline); letting go clears it. A
# Command-click opens it, which a test instance records instead of opening
# a browser, and the program hears nothing of it. A plain click still goes
# to the program. Rows are 0-based; a cell is 8pt wide and `linkrow` points
# tall, its centre at half that.
mkdir -p "$T/linkproj"
printf 'untouched\n' > "$T/linkproj/notes.txt"
cat > "$T/links.sh" <<'STANDIN'
#!/bin/sh
stty raw -echo
cols=$(stty size | cut -d' ' -f2)
pad=$(printf '%*s' "$cols" '' | tr ' ' 'w')
printf '\033[?1000h\033[?1006h\033[H\033[2J'
printf 'See https://example.com/owner/repo/pull/341.\r\n'
printf 'Open \033]8;;https://example.com/osc8/target\033\\the pull request\033]8;;\033\\ now\r\n'
printf 'https://example.com/wrapped/%s/end\r\n' "$pad"
exec cat > "$LINKS_BYTES"
STANDIN
chmod +x "$T/links.sh"
linkrow=17
row() { echo $(( $1 * linkrow + linkrow / 2 )); }
cat > "$T/links.script" <<SCRIPT
wait 500
click @toolbar.terminal
wait 700
wait 700
movein @terminal 84 $(row 0) cmd
wait 100
dump $T/links-hover-plain.out
frame $T/links-hover.bmp
flags -
wait 100
dump $T/links-released.out
movein @terminal 84 $(row 1) cmd
wait 100
dump $T/links-hover-osc8.out
movein @terminal 44 $(row 3) cmd
wait 100
dump $T/links-hover-wrapped.out
movein @terminal 44 $(row 3) -
wait 100
dump $T/links-moved-plain.out
clickin @terminal 84 $(row 0) 1 cmd
clickin @terminal 84 $(row 1) 1 cmd
clickin @terminal 44 $(row 3) 1 cmd
wait 100
dump $T/links-opened.out
clickin @terminal 84 $(row 0)
wait 300
dump $T/links-plain-click.out
quit
SCRIPT
LINKS_BYTES="$T/links.bytes" CRC_TERMINAL_SHELL="$T/links.sh" CRC_SELFTEST="$T/links.script" \
    "$BIN" "$T/linkproj/notes.txt" 2> "$T/links.err"
expect_terminal "$T/links-hover-plain.out" 'link=https://example.com/owner/repo/pull/341@4-43 '
expect_terminal "$T/links-released.out" 'link=none '
expect_terminal "$T/links-hover-osc8.out" 'link=https://example.com/osc8/target@5-21 '
expect_terminal "$T/links-hover-wrapped.out" '/end@0-'
expect_terminal "$T/links-moved-plain.out" 'link=none '
expect_terminal "$T/links-opened.out" 'opened=https://example.com/owner/repo/pull/341|https://example.com/osc8/target|https://example.com/wrapped/www'
if ! grep -m1 '^terminal: ' "$T/links-opened.out" | grep -Eq 'opened=[^ ]*\|https://example\.com/wrapped/w+/end( |$)'; then
    failed "links-opened.out: the wrapped address was not opened whole: $(grep -m1 '^terminal: ' "$T/links-opened.out")"
fi
# Only the plain click reached the program: a press and a release at
# column 11 of row 1, 1-based.
links_got=$(LC_ALL=C tr '\033' 'E' < "$T/links.bytes" 2>/dev/null || true)
if [ "$links_got" != 'E[<0;11;1ME[<0;11;1m' ]; then
    failed "links.bytes: the program heard [$links_got] (ESC as E), expected one plain click"
fi
expect_line "$T/links-plain-click.out" 1 "untouched"
