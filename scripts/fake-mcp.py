#!/usr/bin/env python3
"""A stand-in MCP server for crc's tests.

    fake-mcp.py modern       stdio, 2026-07-28: answers server/discover and
                             rejects any request without the per-request _meta
    fake-mcp.py legacy       stdio, 2025-11-25: server/discover is an unknown
                             method, initialize opens the session
    fake-mcp.py http-modern  Streamable HTTP on 127.0.0.1, modern; prints its
    fake-mcp.py http-legacy  port first. Modern checks MCP-Protocol-Version
                             and Mcp-Method against the body; legacy hands out
                             an Mcp-Session-Id on initialize, requires it
                             afterwards, and answers as an event stream.

Tools: add (read-only, sums a and b), wipe (destructive, always an error
result), crash (the process exits). tools/list is paged in two. One
resource, note://beds, and one prompt, plan, with an argument season.
Everything it receives over stdio is logged to stderr as one line each.
"""
import json
import os
import sys

MODE = sys.argv[1] if len(sys.argv) > 1 else "legacy"
MODERN_MODE = MODE in ("modern", "http-modern")
MODERN = "2026-07-28"
initialized = False
outbox = None


def send(message):
    if outbox is not None:
        outbox.append(message)
        return
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def result(rid, value):
    if MODERN_MODE:
        value = dict(value, resultType="complete")
    send({"jsonrpc": "2.0", "id": rid, "result": value})


def error(rid, code, text):
    send({"jsonrpc": "2.0", "id": rid, "error": {"code": code, "message": text}})


TOOLS = [
    {
        "name": "add",
        "description": "Adds two numbers.",
        "inputSchema": {
            "type": "object",
            "required": ["a", "b"],
            "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
        },
        "annotations": {"readOnlyHint": True},
    },
    {
        "name": "wipe",
        "description": "Wipes the beds.",
        "inputSchema": {"type": "object", "properties": {}},
        "annotations": {"destructiveHint": True},
    },
    {
        "name": "crash",
        "description": "Exits.",
        "inputSchema": {"type": "object", "properties": {}},
    },
]


def handle(message):
    global initialized
    method = message.get("method")
    rid = message.get("id")
    params = message.get("params") or {}
    if rid is None:
        if method == "notifications/initialized":
            initialized = True
        return
    if MODERN_MODE:
        meta = params.get("_meta") or {}
        if method == "initialize":
            error(rid, -32601, "initialize is not a method; send server/discover")
            return
        if meta.get("io.modelcontextprotocol/protocolVersion") != MODERN or \
                "io.modelcontextprotocol/clientCapabilities" not in meta:
            error(rid, -32602, "missing per-request _meta")
            return
        if method == "server/discover":
            result(rid, {
                "supportedVersions": [MODERN],
                "capabilities": {"tools": {}, "resources": {}, "prompts": {}},
                "serverInfo": {"name": "fake-modern", "version": "1.0"},
            })
            return
    else:
        if method == "server/discover":
            error(rid, -32601, "Method not found")
            return
        if method == "initialize":
            result(rid, {
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {}, "resources": {}, "prompts": {}},
                "serverInfo": {"name": "fake-legacy", "version": "0.9"},
            })
            return
        if not initialized:
            error(rid, -32002, "not initialized")
            return
    if method == "tools/list":
        if params.get("cursor") == "page2":
            result(rid, {"tools": TOOLS[1:]})
        else:
            result(rid, {"tools": TOOLS[:1], "nextCursor": "page2"})
    elif method == "tools/call":
        name = params.get("name")
        args = params.get("arguments") or {}
        if name == "add":
            total = args.get("a", 0) + args.get("b", 0)
            result(rid, {"content": [{"type": "text", "text": str(total)}]})
        elif name == "wipe":
            result(rid, {"content": [{"type": "text", "text": "refusing to wipe"}], "isError": True})
        elif name == "crash":
            sys.stderr.write("fake-mcp: crashing on purpose\n")
            sys.stderr.flush()
            os._exit(3)
        else:
            error(rid, -32602, "unknown tool " + str(name))
    elif method == "resources/list":
        result(rid, {"resources": [{"uri": "note://beds", "name": "beds", "mimeType": "text/markdown"}]})
    elif method == "resources/read":
        result(rid, {"contents": [{"uri": params.get("uri"), "text": "# Beds\ntomatoes, beans\n"}]})
    elif method == "prompts/list":
        result(rid, {"prompts": [{"name": "plan", "description": "Plans a season.",
                                  "arguments": [{"name": "season", "required": True}]}]})
    elif method == "prompts/get":
        season = (params.get("arguments") or {}).get("season", "")
        result(rid, {"messages": [{"role": "user", "content": {"type": "text",
                                                               "text": "Plan the " + season + " beds"}}]})
    else:
        error(rid, -32601, "Method not found")


def serve_http():
    import threading
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    lock = threading.Lock()
    session = "garden-session-1"

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def reply(self, status, body=b"", content_type=None, headers=()):
            self.send_response(status)
            if content_type:
                self.send_header("Content-Type", content_type)
            for k, v in headers:
                self.send_header(k, v)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            global outbox
            body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            message = json.loads(body)
            method = message.get("method", "")
            if MODERN_MODE:
                meta = (message.get("params") or {}).get("_meta") or {}
                version = meta.get("io.modelcontextprotocol/protocolVersion")
                if version is None:
                    # What a modern-only server says to a legacy client.
                    self.reply(400, b"modern clients only", "text/plain")
                    return
                if self.headers.get("MCP-Protocol-Version") != version or \
                        self.headers.get("Mcp-Method") != method:
                    err = {"jsonrpc": "2.0", "id": message.get("id"),
                           "error": {"code": -32020, "message": "header mismatch"}}
                    self.reply(400, json.dumps(err).encode(), "application/json")
                    return
            elif method not in ("initialize", "server/discover"):
                if self.headers.get("Mcp-Session-Id") != session:
                    self.reply(400, b"missing session", "text/plain")
                    return
            with lock:
                outbox = []
                handle(message)
                out, outbox = outbox, None
            if message.get("id") is None:
                self.reply(202)
                return
            extra = [("Mcp-Session-Id", session)] if method == "initialize" else []
            if MODERN_MODE:
                self.reply(200, json.dumps(out[0]).encode(), "application/json", extra)
            else:
                stream = "".join("event: message\ndata: " + json.dumps(m) + "\n\n" for m in out)
                self.reply(200, stream.encode(), "text/event-stream", extra)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    sys.stdout.write(str(server.server_address[1]) + "\n")
    sys.stdout.flush()
    server.serve_forever()


if MODE.startswith("http-"):
    serve_http()
else:
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        sys.stderr.write("fake-mcp: got " + line + "\n")
        sys.stderr.flush()
        handle(json.loads(line))
