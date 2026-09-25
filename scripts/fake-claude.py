#!/usr/bin/env python3
"""A Claude Code stand-in, for the editor's IDE integration test.

Does what `claude` does on the wire: finds the editor's lock file, is
refused with a wrong token, connects with the right one, runs the MCP
handshake, calls the read-only tools, waits for a selection, then proposes
two edits with openDiff. The test script accepts the first and rejects the
second; on FILE_SAVED this writes the file, as claude does, then sends
close_tab. Everything it saw goes to a JSON log that selftest.sh checks.

Usage: fake-claude.py <config dir> <project dir> <log file>

Standard library only, like fake-lsp.py.
"""

import base64
import hashlib
import json
import os
import socket
import stat
import struct
import sys
import time

config_dir, project, log_path = sys.argv[1], os.path.realpath(sys.argv[2]), sys.argv[3]
log = {"errors": []}


def save_log():
    with open(log_path, "w") as f:
        json.dump(log, f, indent=1)


def fail(message):
    log["errors"].append(message)
    save_log()
    sys.exit(1)


def find_lock(deadline):
    ide = os.path.join(config_dir, "ide")
    while time.time() < deadline:
        try:
            names = os.listdir(ide)
        except FileNotFoundError:
            names = []
        for name in names:
            if not name.endswith(".lock"):
                continue
            path = os.path.join(ide, name)
            try:
                with open(path) as f:
                    lock = json.load(f)
            except (OSError, ValueError):
                continue
            if project in lock.get("workspaceFolders", []):
                return path, int(name[: -len(".lock")]), lock
        time.sleep(0.1)
    fail("no lock file for %s in %s" % (project, ide))


def upgrade(port, token):
    """The HTTP upgrade. Returns the socket and the status line."""
    sock = socket.create_connection(("127.0.0.1", port), timeout=20)
    key = base64.b64encode(os.urandom(16)).decode()
    headers = [
        "GET / HTTP/1.1",
        "Host: 127.0.0.1:%d" % port,
        "Upgrade: websocket",
        "Connection: Upgrade",
        "Sec-WebSocket-Key: " + key,
        "Sec-WebSocket-Version: 13",
        "Sec-WebSocket-Protocol: mcp",
        "X-Claude-Code-Ide-Authorization: " + token,
    ]
    sock.sendall(("\r\n".join(headers) + "\r\n\r\n").encode())
    response = b""
    while b"\r\n\r\n" not in response:
        chunk = sock.recv(1)
        if not chunk:
            break
        response += chunk
    text = response.decode("latin-1")
    expected = base64.b64encode(
        hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()
    ).decode()
    if text.startswith("HTTP/1.1 101") and ("Sec-WebSocket-Accept: " + expected) not in text:
        fail("wrong Sec-WebSocket-Accept: " + text)
    return sock, text.split("\r\n", 1)[0]


def send(sock, message):
    payload = json.dumps(message).encode()
    mask = os.urandom(4)
    head = bytes([0x81])
    n = len(payload)
    if n < 126:
        head += bytes([0x80 | n])
    elif n < 65536:
        head += bytes([0x80 | 126]) + struct.pack(">H", n)
    else:
        head += bytes([0x80 | 127]) + struct.pack(">Q", n)
    sock.sendall(head + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))


def read_exact(sock, n):
    data = b""
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            fail("connection closed")
        data += chunk
    return data


def receive(sock):
    """The next text message, answering pings on the way."""
    while True:
        b0, b1 = read_exact(sock, 2)
        if b1 & 0x80:
            fail("server frame was masked")
        n = b1 & 0x7F
        if n == 126:
            n = struct.unpack(">H", read_exact(sock, 2))[0]
        elif n == 127:
            n = struct.unpack(">Q", read_exact(sock, 8))[0]
        payload = read_exact(sock, n)
        opcode = b0 & 0x0F
        if opcode == 0x1:
            return json.loads(payload.decode())
        if opcode == 0x8:
            fail("server closed the connection")


pending_notifications = []


def call(sock, id, method, params=None):
    message = {"jsonrpc": "2.0", "id": id, "method": method}
    if params is not None:
        message["params"] = params
    send(sock, message)
    while True:
        reply = receive(sock)
        if reply.get("id") == id:
            return reply
        pending_notifications.append(reply)


def tool(sock, id, name, arguments):
    reply = call(sock, id, "tools/call", {"name": name, "arguments": arguments})
    if "error" in reply:
        return reply["error"]
    return [item["text"] for item in reply["result"]["content"]]


def main():
    lock_path, port, lock = find_lock(time.time() + 15)
    log["lock_mode"] = oct(stat.S_IMODE(os.stat(lock_path).st_mode))
    log["lock_ide"] = lock.get("ideName")
    log["lock_transport"] = lock.get("transport")

    _, status = upgrade(port, "0" * 32)
    log["wrong_token"] = status

    sock, status = upgrade(port, lock["authToken"])
    log["right_token"] = status

    init = call(sock, 0, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}})
    log["protocol"] = init["result"]["protocolVersion"]
    log["server"] = init["result"]["serverInfo"]["name"]
    send(sock, {"jsonrpc": "2.0", "method": "notifications/initialized"})

    tools = call(sock, 1, "tools/list")
    log["tools"] = sorted(t["name"] for t in tools["result"]["tools"])

    folders = json.loads(tool(sock, 2, "getWorkspaceFolders", {})[0])
    log["root"] = folders["rootPath"]
    editors = json.loads(tool(sock, 3, "getOpenEditors", {})[0])
    log["editors"] = [t["label"] for t in editors["tabs"]]
    log["diagnostics"] = json.loads(tool(sock, 4, "getDiagnostics", {})[0])

    # The editor tells a new client where the caret is.
    deadline = time.time() + 10
    while time.time() < deadline:
        found = [n for n in pending_notifications if n.get("method") == "selection_changed"]
        if found:
            params = found[-1]["params"]
            log["selection_file"] = os.path.basename(params["filePath"])
            log["selection_start"] = params["selection"]["start"]
            log["selection_empty"] = params["selection"]["isEmpty"]
            break
        pending_notifications.append(receive(sock))
    else:
        fail("no selection_changed")

    accept_path = os.path.join(project, "accept.txt")
    reply = tool(sock, 10, "openDiff", {
        "old_file_path": accept_path,
        "new_file_path": accept_path,
        "new_file_contents": "one\nTWO\nthree\n",
        "tab_name": "review-accept",
    })
    log["accept_reply"] = reply
    if reply and reply[0] == "FILE_SAVED":
        # Claude writes the file itself.
        with open(accept_path, "w") as f:
            f.write(reply[1])
    log["accept_close"] = tool(sock, 11, "close_tab", {"tab_name": "review-accept"})

    reject_path = os.path.join(project, "reject.txt")
    reply = tool(sock, 20, "openDiff", {
        "old_file_path": reject_path,
        "new_file_path": reject_path,
        "new_file_contents": "changed\n",
        "tab_name": "review-reject",
    })
    log["reject_reply"] = reply
    log["reject_close"] = tool(sock, 21, "close_tab", {"tab_name": "review-reject"})
    save_log()
    sock.close()


main()
