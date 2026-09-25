//! The editor side of Claude Code's IDE integration.
//!
//! Claude Code finds an editor through a lock file in `~/.claude/ide/` and
//! connects to it over a local WebSocket, then speaks MCP: it calls tools the
//! editor serves (diagnostics, open files, a diff to review) and listens for
//! notifications (the selection). crc does not ship an agent; it is the IDE
//! an agent connects to.
//!
//! The protocol is documented only in part. The sources are the Claude Code
//! docs ("The built-in IDE MCP server") and two open-source editors that
//! implement it, coder/claudecode.nvim and manzaltu/claude-code-ide.el.

pub mod diff;
pub mod lock;
pub mod mcp;
pub mod ws;
