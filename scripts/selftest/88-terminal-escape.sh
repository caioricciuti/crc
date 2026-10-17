# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh terminal-escape.

# ---- Escape over a page never reaches the shell -------------------------------
# With the terminal holding the keyboard, Escape closes the page over the
# editor column (Extensions, Settings, MCP) and is spent there: the shell
# never hears it, so bash does not ring the bell on ESC ESC after a page
# was merely closed. With nothing to close, Escape goes to the shell, as
# vim and friends need. The shell stand-in logs every byte it is sent.
mkdir -p "$T/escproj" "$T/eschome" "$T/escnoreg"
printf 'untouched\n' > "$T/escproj/notes.txt"
cat > "$T/esc-shell.sh" <<SHELL
#!/bin/sh
stty -icanon -echo min 1 time 0 2>/dev/null
exec cat > "$T/esc.bytes"
SHELL
chmod +x "$T/esc-shell.sh"
cat > "$T/esc.script" <<SCRIPT
wait 500
click @toolbar.terminal
wait 700
wait 700
text a
wait 200
key 7 cmd,shift x
wait 500
dump $T/esc-ext.out
key 53
wait 300
dump $T/esc-ext-closed.out
key 43 cmd ,
wait 300
dump $T/esc-settings.out
key 53
wait 300
dump $T/esc-settings-closed.out
key 35 cmd p
wait 200
text >mcp servers
wait 300
key 36
wait 300
dump $T/esc-mcp.out
key 53
wait 300
dump $T/esc-mcp-closed.out
text b
wait 200
key 53
wait 200
text c
wait 500
quit
SCRIPT
HOME="$T/eschome" CRC_EXT_REGISTRY="file://$T/escnoreg/" CRC_TERMINAL_SHELL="$T/esc-shell.sh" \
    CRC_SELFTEST="$T/esc.script" "$BIN" "$T/escproj/notes.txt" 2> "$T/esc.err"
# The terminal kept the keyboard under each page, and each Escape closed
# the page.
expect_terminal "$T/esc-ext.out" "open=true focus=true"
grep -q '^extensions: open ' "$T/esc-ext.out" \
    || failed "esc-ext.out: Cmd-Shift-X did not open the Extensions page: $(grep '^extensions:' "$T/esc-ext.out")"
grep -q '^extensions: list ' "$T/esc-ext-closed.out" \
    || failed "esc-ext-closed.out: Escape did not close the Extensions page: $(grep '^extensions:' "$T/esc-ext-closed.out")"
expect_terminal "$T/esc-settings.out" "open=true focus=true"
grep -q '^settings_page: open ' "$T/esc-settings.out" \
    || failed "esc-settings.out: Cmd-, did not open the Settings page: $(grep '^settings_page:' "$T/esc-settings.out")"
grep -q '^settings_page: closed ' "$T/esc-settings-closed.out" \
    || failed "esc-settings-closed.out: Escape did not close the Settings page: $(grep '^settings_page:' "$T/esc-settings-closed.out")"
expect_terminal "$T/esc-mcp.out" "open=true focus=true"
grep -q '^mcp_page: open ' "$T/esc-mcp.out" \
    || failed "esc-mcp.out: MCP Servers did not open the page: $(grep '^mcp_page:' "$T/esc-mcp.out")"
grep -q '^mcp_page: closed ' "$T/esc-mcp-closed.out" \
    || failed "esc-mcp-closed.out: Escape did not close the MCP page: $(grep '^mcp_page:' "$T/esc-mcp-closed.out")"
# The shell heard the typing and the one Escape pressed with nothing open,
# and none of the three that closed a page.
bytes=$(od -An -c "$T/esc.bytes" 2>/dev/null | tr -s ' \n' ' ' | sed 's/^ //; s/ $//')
[ "$bytes" = "a b 033 c" ] \
    || failed "esc.bytes: the shell was sent [$bytes], expected [a b 033 c]"
# The document underneath was never typed into.
expect_line "$T/esc-mcp-closed.out" 1 "untouched"
