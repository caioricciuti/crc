# Why crc exists

Moved out of the README on 2026-09-24 so the README can describe what ships.
Nothing here is built yet; it is the reason the editor was started and the
order the rest of the roadmap follows.

Most editors are a text buffer with a semantic model bolted on somewhere else:
VS Code delegates understanding to language servers over a protocol, JetBrains
keeps a real index but locks it inside the process. Both put the editing
surface a long way from the thing that knows what the code means.

Three bets:

1. **The project index is a database, not a bespoke format.** Symbols,
   references and node spans belong in SQLite, so "find all callers" is a
   query, and any tool (or agent) can read it without asking the editor's
   permission. *Not built yet.*
2. **Extensions are WASM with declared capabilities.** A manifest states what
   an extension may touch and the host enforces it. VS Code's extension host
   is arbitrary Node with full filesystem and network access, auto-updating
   from a marketplace. *Not built yet.*
3. **An agent is a protocol client, not a sidebar.** Buffer state, syntax tree
   and index are exposed through one API; the built-in UI is a consumer of it
   and so is a model. *First step built:* crc is an IDE that Claude Code
   connects to. The general protocol is not built yet.

None of them matter if the editor cannot render text inside a frame budget,
which is why that was built and measured first.
