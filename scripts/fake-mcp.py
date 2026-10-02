#!/usr/bin/env python3
"""A stand-in MCP server for crc's tests, over stdio.

    fake-mcp.py modern    speaks 2026-07-28: answers server/discover and
                          rejects any request without the per-request _meta
    fake-mcp.py legacy    speaks 2025-11-25: server/discover is an unknown
                          method, initialize opens the session

Tools: add (read-only, sums a and b), wipe (destructive, always an error
result), crash (the process exits). tools/list is paged in two. One
resource, note://beds, and one prompt, plan, with an argument season.
Everything it receives is logged to stderr as one line each.
"""
import json
import sys

MODE = sys.argv[1] if len(sys.argv) > 1 else "legacy"
MODERN = "2026-07-28"
initialized = False


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def result(rid, value):
    if MODE == "modern":
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
    if MODE == "modern":
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
            sys.exit(3)
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


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    sys.stderr.write("fake-mcp: got " + line + "\n")
    sys.stderr.flush()
    handle(json.loads(line))
