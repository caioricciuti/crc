# Sourced by scripts/selftest.sh, which sets up $T, $BIN, $fail and the
# expect helpers; run it alone with scripts/selftest.sh http-request.

# ---- HTTP request from a .http file ----------------------------------------
# Nothing listens on port 1, so curl fails fast. What is under test is the
# native path: Cmd-Return in a request file opens a response tab, the worker
# answers, and the tab is filled in and left clean.
mkdir -p "$T/httpproj"
printf '### Closed port\nGET http://127.0.0.1:1/\n' > "$T/httpproj/probe.http"
cat > "$T/http.script" <<SCRIPT
key 36 cmd
wait 1500
wait 100
dump $T/http.out
click @response.segment.2
wait 200
dump $T/http-request.out
quit
SCRIPT
CRC_SELFTEST="$T/http.script" "$BIN" "$T/httpproj/probe.http" 2> "$T/http.err"
expect "$T/http.out" tabs "probe.http | GET Closed port"
expect "$T/http.out" active 1
expect "$T/http.out" dirty false
# The body segment shows curl's own error when nothing came back; the
# Request segment shows what was sent.
if ! sed -n '/^text:$/,$p' "$T/http.out" | sed -n 2p | grep -q '^curl: (7)'; then
    failed "http.out: expected curl's connection error as the response body"
fi
expect_line "$T/http-request.out" 1 "GET http://127.0.0.1:1/"
