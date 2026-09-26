#!/usr/bin/env python3
"""A language server small enough to read, for the editor's tests.

Speaks just enough of the protocol to exercise the client: initialize,
document sync, diagnostics for lines containing TODO or ERROR, completion,
definition, hover, references and rename by whole word, formatting (trailing
spaces go, `){` gains a space), signature help for one made-up function, code
actions (a TODO fix that needs resolving, a command that comes back as
workspace/applyEdit, a disabled one, organize imports by sorting the leading
`import` lines), a server-to-client request, and shutdown. Deliberately
plain: no threads, no library, so the tests depend on nothing but python3,
which ships with the Xcode command line tools.
"""

import json
import sys

documents = {}


def read_message():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        line = line.decode("ascii", "replace").strip()
        if not line:
            break
        if line.lower().startswith("content-length:"):
            length = int(line.split(":", 1)[1].strip())
    if length is None:
        return None
    body = sys.stdin.buffer.read(length)
    return json.loads(body.decode("utf-8"))


def send(message):
    body = json.dumps(message).encode("utf-8")
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()


def reply(request, result):
    send({"jsonrpc": "2.0", "id": request["id"], "result": result})


def notify(method, params):
    send({"jsonrpc": "2.0", "method": method, "params": params})


def publish(uri):
    text = documents.get(uri, "")
    diagnostics = []
    for number, line in enumerate(text.split("\n")):
        for word, severity in (("ERROR", 1), ("TODO", 2)):
            at = line.find(word)
            if at >= 0:
                diagnostics.append(
                    {
                        "range": {
                            "start": {"line": number, "character": at},
                            "end": {"line": number, "character": at + len(word)},
                        },
                        "severity": severity,
                        "source": "fake",
                        "code": word.lower(),
                        "data": {"fix": "DONE" if word == "TODO" else None},
                        "message": "%s on line %d" % (word.lower(), number + 1),
                    }
                )
    notify("textDocument/publishDiagnostics", {"uri": uri, "diagnostics": diagnostics})


def word_before(uri, position):
    text = documents.get(uri, "")
    lines = text.split("\n")
    if position["line"] >= len(lines):
        return "", position["character"]
    line = lines[position["line"]]
    end = min(position["character"], len(line))
    start = end
    while start > 0 and (line[start - 1].isalnum() or line[start - 1] == "_"):
        start -= 1
    return line[start:end], start


def word_at(uri, position):
    text = documents.get(uri, "")
    lines = text.split("\n")
    if position["line"] >= len(lines):
        return ""
    line = lines[position["line"]]
    start = end = min(position["character"], len(line))
    ident = lambda c: c.isalnum() or c == "_"
    while start > 0 and ident(line[start - 1]):
        start -= 1
    while end < len(line) and ident(line[end]):
        end += 1
    return line[start:end]


def on_disk():
    """Open documents, plus the other .rs files beside them, read from disk,
    the way a real server knows the whole project."""
    import os
    from urllib.parse import quote, unquote

    texts = dict(documents)
    for uri in list(documents):
        folder = os.path.dirname(unquote(uri[len("file://"):]))
        for name in sorted(os.listdir(folder)):
            path = os.path.join(folder, name)
            other = "file://" + quote(path)
            if name.endswith(".rs") and other not in texts:
                with open(path, encoding="utf-8") as f:
                    texts[other] = f.read()
    return texts


def occurrences(word):
    """Every whole-word use of `word` in the project."""
    found = []
    for uri, text in sorted(on_disk().items()):
        for number, line in enumerate(text.split("\n")):
            at = 0
            while word:
                at = line.find(word, at)
                if at < 0:
                    break
                before = line[at - 1] if at > 0 else " "
                after = line[at + len(word)] if at + len(word) < len(line) else " "
                if not (before.isalnum() or before == "_" or after.isalnum() or after == "_"):
                    found.append((uri, number, at))
                at += len(word)
    return found


def span(number, start, end):
    return {
        "start": {"line": number, "character": start},
        "end": {"line": number, "character": end},
    }


next_server_id = 1000

while True:
    message = read_message()
    if message is None:
        break
    method = message.get("method")
    params = message.get("params") or {}
    if method == "initialize":
        reply(
            message,
            {
                "capabilities": {
                    "textDocumentSync": 1,
                    "completionProvider": {"triggerCharacters": ["."]},
                    "definitionProvider": True,
                    "hoverProvider": True,
                    "referencesProvider": True,
                    "renameProvider": True,
                    "documentFormattingProvider": True,
                    "codeActionProvider": {"resolveProvider": True},
                    "executeCommandProvider": {"commands": ["fake.upper"]},
                    "signatureHelpProvider": {
                        "triggerCharacters": ["("],
                        "retriggerCharacters": [","],
                    },
                },
                "serverInfo": {"name": "fake-lsp", "version": "1"},
            },
        )
    elif method == "initialized":
        notify("window/logMessage", {"type": 3, "message": "fake-lsp ready"})
        # A request in the other direction: the client must answer it.
        send(
            {
                "jsonrpc": "2.0",
                "id": next_server_id,
                "method": "workspace/configuration",
                "params": {"items": [{"section": "fake"}]},
            }
        )
        next_server_id += 1
    elif method == "textDocument/didOpen":
        document = params["textDocument"]
        documents[document["uri"]] = document["text"]
        publish(document["uri"])
    elif method == "textDocument/didChange":
        uri = params["textDocument"]["uri"]
        for change in params["contentChanges"]:
            documents[uri] = change["text"]
        publish(uri)
    elif method == "textDocument/didClose":
        documents.pop(params["textDocument"]["uri"], None)
    elif method == "textDocument/completion":
        uri = params["textDocument"]["uri"]
        prefix, start = word_before(uri, params["position"])
        items = []
        for label in ("alpha", "alphabet", "beta"):
            items.append(
                {
                    "label": label,
                    "kind": 3,
                    "detail": "fn %s()" % label,
                    "textEdit": {
                        "range": {
                            "start": {"line": params["position"]["line"], "character": start},
                            "end": params["position"],
                        },
                        "newText": label + "()",
                    },
                }
            )
        items.append({"label": "gamma", "kind": 6, "insertText": "gamma_value"})
        reply(message, {"isIncomplete": False, "items": items})
    elif method == "textDocument/definition":
        reply(
            message,
            [
                {
                    "uri": params["textDocument"]["uri"],
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 4},
                    },
                }
            ],
        )
    elif method == "textDocument/hover":
        reply(message, {"contents": {"kind": "markdown", "value": "**hover** text"}})
    elif method == "textDocument/references":
        word = word_at(params["textDocument"]["uri"], params["position"])
        reply(
            message,
            [
                {"uri": uri, "range": span(number, at, at + len(word))}
                for uri, number, at in occurrences(word)
            ],
        )
    elif method == "textDocument/rename":
        word = word_at(params["textDocument"]["uri"], params["position"])
        changes = {}
        for uri, number, at in occurrences(word):
            changes.setdefault(uri, []).append(
                {"range": span(number, at, at + len(word)), "newText": params["newName"]}
            )
        reply(message, {"changes": changes})
    elif method == "textDocument/formatting":
        edits = []
        text = documents.get(params["textDocument"]["uri"], "")
        for number, line in enumerate(text.split("\n")):
            trimmed = line.rstrip(" ")
            if len(trimmed) < len(line):
                edits.append({"range": span(number, len(trimmed), len(line)), "newText": ""})
            at = trimmed.find("){")
            if at >= 0:
                edits.append({"range": span(number, at + 1, at + 1), "newText": " "})
        reply(message, edits)
    elif method == "textDocument/signatureHelp":
        text = documents.get(params["textDocument"]["uri"], "")
        lines = text.split("\n")
        position = params["position"]
        line = lines[position["line"]] if position["line"] < len(lines) else ""
        before = line[: position["character"]]
        call = before.rfind("(")
        if call < 0 or ")" in before[call:]:
            reply(message, None)
        else:
            label = "fn alpha(first: i32, second: i32)"
            reply(
                message,
                {
                    "signatures": [
                        {
                            "label": label,
                            "parameters": [{"label": [9, 19]}, {"label": "second: i32"}],
                        }
                    ],
                    "activeSignature": 0,
                    "activeParameter": before[call:].count(","),
                },
            )
    elif method == "textDocument/codeAction":
        uri = params["textDocument"]["uri"]
        context = params.get("context", {})
        only = context.get("only")
        actions = []
        if only is None or "source.organizeImports" in only:
            lines = documents.get(uri, "").split("\n")
            count = 0
            while count < len(lines) and lines[count].startswith("import "):
                count += 1
            if count > 0:
                block = sorted(set(lines[:count]))
                actions.append(
                    {
                        "title": "Organize Imports",
                        "kind": "source.organizeImports",
                        "edit": {
                            "changes": {
                                uri: [
                                    {
                                        "range": {
                                            "start": {"line": 0, "character": 0},
                                            "end": {"line": count, "character": 0},
                                        },
                                        "newText": "".join(l + "\n" for l in block),
                                    }
                                ]
                            }
                        },
                    }
                )
        if only is None:
            # A fix only for a diagnostic sent back whole, data and all.
            for diagnostic in context.get("diagnostics", []):
                fix = (diagnostic.get("data") or {}).get("fix")
                if diagnostic.get("code") == "todo" and fix:
                    actions.append(
                        {
                            "title": "Replace TODO with %s" % fix,
                            "kind": "quickfix",
                            "isPreferred": True,
                            "diagnostics": [diagnostic],
                            "data": {"uri": uri, "range": diagnostic["range"], "text": fix},
                        }
                    )
            line = params["range"]["start"]["line"]
            actions.append(
                {
                    "title": "Upper-case this line",
                    "kind": "refactor.rewrite",
                    "command": {
                        "title": "Upper-case",
                        "command": "fake.upper",
                        "arguments": [uri, line],
                    },
                }
            )
            actions.append(
                {
                    "title": "Extract function",
                    "kind": "refactor.extract",
                    "disabled": {"reason": "select an expression first"},
                }
            )
        reply(message, actions)
    elif method == "codeAction/resolve":
        data = params.get("data")
        resolved = dict(params)
        if data:
            resolved["edit"] = {
                "changes": {data["uri"]: [{"range": data["range"], "newText": data["text"]}]}
            }
        reply(message, resolved)
    elif method == "workspace/executeCommand":
        uri, line = params["arguments"]
        text = documents.get(uri, "").split("\n")[line]
        send(
            {
                "jsonrpc": "2.0",
                "id": next_server_id,
                "method": "workspace/applyEdit",
                "params": {
                    "label": "Upper-case",
                    "edit": {
                        "changes": {
                            uri: [{"range": span(line, 0, len(text)), "newText": text.upper()}]
                        }
                    },
                },
            }
        )
        next_server_id += 1
        reply(message, None)
    elif method == "shutdown":
        reply(message, None)
    elif method == "exit":
        break
    elif "id" in message and method is None:
        # The client's answer to our configuration request.
        pass
    elif "id" in message:
        send(
            {
                "jsonrpc": "2.0",
                "id": message["id"],
                "error": {"code": -32601, "message": "method not found: %s" % method},
            }
        )
