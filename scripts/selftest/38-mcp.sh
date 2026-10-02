# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh mcp.

# ---- MCP servers ------------------------------------------------------------------
# The fake server in scripts/fake-mcp.py stands in for a real one. View >
# MCP Servers lists mcp.json without starting anything; a click starts a
# server and lists its tools; a launcher that downloads code is refused; a
# tool opens a call document that Cmd-Return runs into an answer tab; a
# destructive tool asks first; a resource opens at once.
mkdir -p "$T/mcphome/.config/crc" "$T/mcpproj"
printf 'notes\n' > "$T/mcpproj/notes.txt"
cat > "$T/mcphome/.config/crc/mcp.json" <<JSON
{"mcpServers": {
  "garden": {"command": "/usr/bin/python3", "args": ["$PWD/scripts/fake-mcp.py", "legacy"]},
  "fetcher": {"command": "npx", "args": ["-y", "some-mcp-server"]}
}}
JSON
cat > "$T/mcp.script" <<SCRIPT
wait 800
wait 300
key 35 cmd p
wait 200
text >mcp servers
key 36
wait 300
dump $T/mcp-listed.out
click @mcp.server.garden
wait 800
wait 600
wait 300
dump $T/mcp-started.out
click @mcp.server.fetcher
wait 300
dump $T/mcp-refused.out
click @mcp.tool.garden.add
wait 300
dump $T/mcp-calldoc.out
key 36 cmd
wait 600
wait 300
dump $T/mcp-answer.out
click @mcp.tool.garden.wipe
wait 300
key 36 cmd
wait 600
wait 300
dump $T/mcp-wipe.out
click @mcp.resource.garden.0
wait 600
wait 300
dump $T/mcp-resource.out
quit
SCRIPT
HOME="$T/mcphome" CRC_SELFTEST="$T/mcp.script" "$BIN" "$T/mcpproj" 2> "$T/mcp.err"
grep -q '^activity: mcp ' "$T/mcp-listed.out" \
    || failed "mcp-listed.out: $(grep '^activity:' "$T/mcp-listed.out")"
# Listed, and nothing started.
expect "$T/mcp-listed.out" mcp "garden:stopped:0,fetcher:stopped:0"
expect "$T/mcp-started.out" mcp "garden:ready:3,fetcher:stopped:0"
grep -q '^mcp: garden:ready:3,fetcher:failed: npx downloads a package and runs it' "$T/mcp-refused.out" \
    || failed "mcp-refused.out: $(grep '^mcp:' "$T/mcp-refused.out")"
grep -q '^message: fetcher: npx downloads a package and runs it' "$T/mcp-refused.out" \
    || failed "mcp-refused.out: $(grep '^message:' "$T/mcp-refused.out")"
# The call document names the server and tool, with placeholders.
grep -q '^tabs: .*garden · add call' "$T/mcp-calldoc.out" \
    || failed "mcp-calldoc.out: $(grep '^tabs:' "$T/mcp-calldoc.out")"
expect_line "$T/mcp-calldoc.out" 2 '  "server": "garden",'
# Cmd-Return ran it: 0 + 0 in the answer tab.
expect "$T/mcp-answer.out" message "garden · add: done"
expect_line "$T/mcp-answer.out" 1 "0"
# The destructive tool asked, the test said no, and it did not run.
grep -q '^crc: mcp confirm prompt: garden · wipe: ' "$T/mcp.err" \
    || failed "mcp: running a destructive tool did not ask first"
# An answer tab is "garden · wipe" exactly; the call document's ends in "call".
grep -Eq '^tabs: (.* \| )?garden · wipe( \||$)' "$T/mcp-wipe.out" \
    && failed "mcp-wipe.out: the destructive tool ran without a yes"
grep -v '^crc: mcp confirm prompt: ' "$T/mcp.err" > "$T/mcp.err.rest" || true
mv "$T/mcp.err.rest" "$T/mcp.err"
expect_line "$T/mcp-resource.out" 1 "# Beds"
