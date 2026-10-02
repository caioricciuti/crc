//! MCP servers in the window: the sidebar panel, call documents, and the
//! tabs answers land in.

use super::*;
use crate::mcp_client::call::{self, Target};
use crate::platform::mcp_panel::Action as McpAction;

/// What `mcp.json` starts as when Edit finds none.
const CONFIG_TEMPLATE: &str = "{\n  \"mcpServers\": {\n  }\n}\n";

impl EditorView {
    /// The wake-up a server's reader thread uses: a main-thread poll.
    pub(super) fn mcp_wake(&self) -> crate::lsp::transport::Wake {
        self.wake(EditorView::poll_mcp)
    }

    /// Shows the MCP panel in the sidebar, reading `mcp.json` the first
    /// time. Nothing starts.
    pub(super) fn open_mcp(&self) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.extensions = None;
            state.git_open = false;
            state.git_focus = false;
            if !state.mcp.loaded {
                state.mcp.reload();
            }
            state.mcp.open = true;
            state.sidebar = true;
        }
        self.request_redraw();
        self.pump();
    }

    pub(super) fn mcp_click(&self, x: f32, y: f32) {
        let action = self.state().and_then(|state| state.mcp.hit(x, y));
        if let Some(action) = action {
            self.mcp_action(action);
        }
    }

    pub(super) fn mcp_action(&self, action: McpAction) {
        match action {
            McpAction::Edit => self.edit_mcp_config(),
            McpAction::Reload => {
                if let Some(mut state) = self.state_mut() {
                    state.mcp.reload();
                    let said = match (&state.mcp.error, state.mcp.servers.len()) {
                        (Some(error), _) => error.clone(),
                        (None, 1) => "mcp.json read: 1 server".into(),
                        (None, n) => format!("mcp.json read: {n} servers"),
                    };
                    state.message = Some((said, Instant::now()));
                }
            }
            McpAction::Toggle(si) => {
                let wake = self.mcp_wake();
                if let Some(mut state) = self.state_mut() {
                    let root = state.workspace.root().to_path_buf();
                    if let Some(server) = state.mcp.servers.get_mut(si) {
                        if server.is_running() {
                            server.stop();
                        } else {
                            server.start(&root, wake);
                        }
                    }
                    // The sidebar is too narrow for the reason; the status
                    // line has room.
                    let failed = state.mcp.servers.get(si).and_then(|s| match &s.status {
                        crate::mcp_client::Status::Failed(why) => {
                            Some(format!("{}: {why}", s.name))
                        }
                        _ => None,
                    });
                    if let Some(said) = failed {
                        state.message = Some((said, Instant::now()));
                    }
                }
                self.resume_display_link();
            }
            McpAction::Tool(si, ti) => {
                let doc = self.state().and_then(|state| {
                    let server = state.mcp.servers.get(si)?;
                    let tool = server.tools.get(ti)?;
                    Some((
                        format!("{} \u{b7} {} call", server.name, tool.name),
                        call::tool_document(&server.name, tool),
                    ))
                });
                if let Some((title, text)) = doc {
                    self.open_call_document(&title, &text);
                }
            }
            McpAction::Prompt(si, pi) => {
                let doc = self.state().and_then(|state| {
                    let server = state.mcp.servers.get(si)?;
                    let prompt = server.prompts.get(pi)?;
                    Some((
                        format!("{} \u{b7} {} call", server.name, prompt.name),
                        call::prompt_document(&server.name, prompt),
                    ))
                });
                if let Some((title, text)) = doc {
                    self.open_call_document(&title, &text);
                }
            }
            McpAction::Resource(si, ri) => {
                // Reading changes nothing: it runs at once.
                let read = self.state().and_then(|state| {
                    let server = state.mcp.servers.get(si)?;
                    let resource = server.resources.get(ri)?;
                    Some(call::Call {
                        server: server.name.clone(),
                        target: Target::Resource(resource.uri.clone()),
                        arguments: crate::json::Value::Object(Vec::new()),
                    })
                });
                if let Some(read) = read {
                    self.run_call(read);
                }
            }
        }
        self.request_redraw();
        self.pump();
    }

    /// Opens `mcp.json`, writing an empty one first when there is none.
    fn edit_mcp_config(&self) {
        let Some(path) = crate::mcp_client::config::path() else {
            return;
        };
        if !path.exists() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = crate::platform::write_atomically(&path, CONFIG_TEMPLATE.as_bytes())
                && let Some(mut state) = self.state_mut()
            {
                state.message = Some((format!("{}: {e}", path.display()), Instant::now()));
                return;
            }
        }
        self.run_pick(Pick::File(path));
    }

    /// A call document in a tab of its own, ready to edit and run.
    fn open_call_document(&self, title: &str, text: &str) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.docs.push(Buffer::generated(title, "json", text));
            reveal_active_tab(&mut state);
            state.message = Some(("Cmd-Return runs the call".into(), Instant::now()));
        }
        self.sync_title();
        self.reparse();
    }

    /// Run > Copy crc MCP Server Command: the line that starts `crc --mcp`
    /// for this workspace, for an agent's MCP settings. crc never writes
    /// another tool's configuration itself.
    pub(super) fn copy_mcp_server_command(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let said = match (std::env::current_exe(), state.tree.root()) {
            (Ok(exe), Some(root)) => {
                let line = crate::mcp_server::command_line(&exe, root);
                crate::platform::clipboard::write_text(&line);
                "copied: add it to your agent's MCP servers (for Claude Code: claude mcp add crc -- <the line>)".to_string()
            }
            (_, None) => "open a folder first".to_string(),
            (Err(e), _) => e.to_string(),
        };
        state.message = Some((said, Instant::now()));
        drop(state);
        self.request_redraw();
    }

    /// Run > Save MCP Call to Workspace: the active call document into
    /// the workspace's `calls/` folder, where Home lists it.
    pub(super) fn save_mcp_call(&self) {
        let saved = {
            let Some(state) = self.state() else {
                return;
            };
            let text = state.docs.active().rope.to_string();
            call::parse(&text).and_then(|parsed| {
                let what = match &parsed.target {
                    Target::Tool(name) | Target::Prompt(name) => name.clone(),
                    Target::Resource(_) => "resource".into(),
                };
                state
                    .workspace
                    .save_call(&format!("{}-{what}", parsed.server), &text)
            })
        };
        if let Some(mut state) = self.state_mut() {
            let said = match saved {
                Ok(path) => {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    start_home_summary(&mut state);
                    format!("saved as calls/{name}; Home lists it")
                }
                Err(error) => error,
            };
            state.message = Some((said, Instant::now()));
        }
        self.resume_display_link();
        self.request_redraw();
    }

    /// Runs the active call document, or the call behind the active
    /// answer tab again.
    pub(super) fn run_mcp_call(&self) {
        let parsed = {
            let Some(state) = self.state() else {
                return;
            };
            let buffer = state.docs.active();
            match state.mcp_calls.get(&buffer.id()) {
                Some(again) => Ok(again.clone()),
                None => call::parse(&buffer.rope.to_string()),
            }
        };
        match parsed {
            Ok(call) => self.run_call(call),
            Err(error) => {
                if let Some(mut state) = self.state_mut() {
                    state.message = Some((error, Instant::now()));
                }
                self.request_redraw();
            }
        }
    }

    fn run_call(&self, call: call::Call) {
        let (destructive, ready) = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            if !state.mcp.loaded {
                state.mcp.reload();
            }
            let Some(si) = state.mcp.find(&call.server) else {
                state.message = Some((
                    format!(
                        "no server named \u{201c}{}\u{201d} in mcp.json",
                        call.server
                    ),
                    Instant::now(),
                ));
                drop(state);
                self.request_redraw();
                return;
            };
            let server = &state.mcp.servers[si];
            let destructive = match &call.target {
                Target::Tool(name) => server
                    .tools
                    .iter()
                    .any(|t| &t.name == name && t.destructive),
                _ => false,
            };
            (
                destructive,
                server.status == crate::mcp_client::Status::Ready,
            )
        };
        if !ready {
            // Never started behind the person's back: the panel's click
            // starts a server.
            if let Some(mut state) = self.state_mut() {
                state.message = Some((
                    format!("{} is not running: start it in the MCP panel", call.server),
                    Instant::now(),
                ));
            }
            self.request_redraw();
            return;
        }
        if destructive {
            let what = call.title();
            let detail = "The server says this tool may change or delete things.";
            let run = if self.ivars().testing {
                eprintln!("crc: mcp confirm prompt: {what}: {detail}");
                std::env::var("CRC_MCP_CONFIRM").is_ok_and(|a| a == "run")
            } else {
                ask(
                    MainThreadMarker::from(self),
                    &format!("Run \u{201c}{what}\u{201d}?"),
                    detail,
                    &["Run", "Cancel"],
                ) == 0
            };
            if !run {
                return;
            }
        }
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let title = call.title();
            let id = match state.docs.index_of_label(&title) {
                Some(index) => {
                    state.docs.switch(index);
                    state.docs.active_mut().regenerate("Running\u{2026}\n");
                    state.docs.active().id()
                }
                None => {
                    state
                        .docs
                        .push(Buffer::generated(&title, "txt", "Running\u{2026}\n"));
                    state.docs.active().id()
                }
            };
            reveal_active_tab(&mut state);
            let Some(si) = state.mcp.find(&call.server) else {
                return;
            };
            let server = &mut state.mcp.servers[si];
            match &call.target {
                Target::Tool(name) => server.call_tool(id, name, call.arguments.clone()),
                Target::Resource(uri) => server.read_resource(id, uri),
                Target::Prompt(name) => server.get_prompt(id, name, call.arguments.clone()),
            }
            state.message = Some((format!("running {title}"), Instant::now()));
            state.mcp_calls.insert(id, call);
        }
        self.resume_display_link();
        self.sync_title();
        self.reparse();
        self.request_redraw();
        self.pump();
    }

    /// Takes what the servers sent: answers into their tabs, and list or
    /// status changes into the panel.
    pub(super) fn poll_mcp(&self) {
        let mut redraw = false;
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let mut answers = Vec::new();
            for server in &mut state.mcp.servers {
                for event in server.poll() {
                    match event {
                        crate::mcp_client::Event::Changed => redraw = true,
                        crate::mcp_client::Event::Answer { id, text, error } => {
                            answers.push((id, text, error));
                        }
                    }
                }
            }
            for (id, text, error) in answers {
                redraw = true;
                let title = state
                    .mcp_calls
                    .get(&id)
                    .map(call::Call::title)
                    .unwrap_or_default();
                let State {
                    docs,
                    panes,
                    message,
                    ..
                } = &mut *state;
                if let Some(buffer) = buffer_by_id_mut(docs, panes, id) {
                    buffer.regenerate(&text);
                }
                let outcome = if error { "failed" } else { "done" };
                *message = Some((format!("{title}: {outcome}"), Instant::now()));
            }
        }
        if redraw {
            self.request_redraw();
            if let Some(window) = self.window() {
                window.invalidateCursorRectsForView(self);
            }
        }
    }
}
