# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh mcp-server.

# ---- crc --mcp, the project for agents ------------------------------------------
# No window: the binary answers MCP on stdio for a folder, both eras, and
# its tools read the index (built on the spot), files and the workspace.
mkdir -p "$T/srvproj/src" "$T/srvproj/docs"
printf 'fn water_the_tomatoes() {}\npub struct GardenBed;\n' > "$T/srvproj/src/garden.rs"
printf '# State\n\n## Waiting on you\n\n- Pick the seeds\n' > "$T/srvproj/docs/state.md"
git init -q "$T/srvproj"
git -C "$T/srvproj" add -A
META='"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}'
# The index is built when the server starts; the questions wait for it.
mcp_ask() {
    ( sleep "$1"; shift; for line in "$@"; do printf '%s\n' "$line"; done ) \
        | HOME="$T/srvhome" "$BIN" --mcp "$T/srvproj" 2>> "$T/srv.log"
}
mkdir -p "$T/srvhome"
mcp_ask 2 \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"project.symbols","arguments":{"query":"tomatoes"}}}' \
    '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"project.read","arguments":{"path":"src/garden.rs","start_line":2}}}' \
    '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"project.read","arguments":{"path":"../srvhome/x"}}}' \
    > "$T/srv-legacy.out"
mcp_ask 0 \
    "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"server/discover\",\"params\":{$META}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"workspace.state\",\"arguments\":{},$META}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"project.files\",\"arguments\":{\"query\":\"src garden\"},$META}}" \
    > "$T/srv-modern.out"
grep -q '"protocolVersion":"2025-11-25"' "$T/srv-legacy.out" || failed "srv-legacy.out: no initialize answer"
grep -q 'water_the_tomatoes' "$T/srv-legacy.out" || failed "srv-legacy.out: project.symbols found nothing: $(sed -n 2p "$T/srv-legacy.out")"
grep -q '"text":"pub struct GardenBed;\\n"' "$T/srv-legacy.out" || failed "srv-legacy.out: project.read: $(sed -n 3p "$T/srv-legacy.out")"
sed -n 4p "$T/srv-legacy.out" | grep -q '"isError":true' || failed "srv-legacy.out: a path outside the folder was read"
grep -q '"supportedVersions":\["2026-07-28","2025-11-25"\]' "$T/srv-modern.out" || failed "srv-modern.out: no discover answer"
grep -q '"waiting":\["Pick the seeds"\]' "$T/srv-modern.out" || failed "srv-modern.out: workspace.state: $(sed -n 2p "$T/srv-modern.out")"
grep -q '"files":\["src/garden.rs"\]' "$T/srv-modern.out" || failed "srv-modern.out: project.files: $(sed -n 3p "$T/srv-modern.out")"
# The log names tools, never their arguments.
grep -q 'tomatoes' "$T/srv.log" && failed "srv.log carries a tool's arguments"
true
