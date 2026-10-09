//! MCP servers in the window: the sidebar panel, call documents, and the
//! tabs answers land in.

use super::*;
use crate::mcp_client::call::{self, Target};
use crate::platform::mcp_panel::Action as McpAction;

/// What `mcp.json` starts as when Edit finds none.
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
            state.palette = None;
            state.completion = None;
            if !state.mcp.loaded {
                state.mcp.reload();
            }
            state.mcp.open = true;
            // The page takes the editor column, as the Extensions page
            // does; Escape, Close or a tab gives it back.
            state.mcp.details = true;
            state.mcp.note = None;
            state.settings_page.open = false;
            state.review_page.open = false;
            state.sidebar = true;
        }
        self.request_redraw();
        self.pump();
    }

    /// `mcp.json` saved in crc: the list follows it at once, where the
    /// watcher would not (it ignores this process's own writes).
    pub(super) fn mcp_config_saved(&self) {
        let Some(mut state) = self.state_mut() else {
            return;
        };
        state.mcp.reload();
        let said = match (&state.mcp.error, state.mcp.servers.len()) {
            (Some(error), _) => error.clone(),
            (None, 1) => "mcp.json saved and read: 1 server".into(),
            (None, n) => format!("mcp.json saved and read: {n} servers"),
        };
        state.mcp.note = Some(said.clone());
        state.message = Some((said, Instant::now()));
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
                    state.mcp.note = Some(said.clone());
                    state.message = Some((said, Instant::now()));
                }
            }
            McpAction::Select(si) => {
                if let Some(mut state) = self.state_mut() {
                    if state.mcp.selected != Some(si) {
                        state.mcp.page_scroll = 0.0;
                    }
                    state.mcp.selected = Some(si);
                    state.mcp.details = true;
                    state.mcp.note = None;
                }
            }
            McpAction::Home => {
                if let Some(mut state) = self.state_mut() {
                    state.mcp.selected = None;
                    state.mcp.page_scroll = 0.0;
                    state.mcp.details = true;
                }
            }
            McpAction::Close => {
                if let Some(mut state) = self.state_mut() {
                    state.mcp.details = false;
                }
            }
            McpAction::AddCommand => {
                if let Some(program) = self.choose_mcp_program() {
                    let entry = crate::mcp_client::ServerConfig {
                        command: program.to_string_lossy().into_owned(),
                        ..Default::default()
                    };
                    self.add_mcp_server(entry);
                }
            }
            McpAction::AddUrl => self.open_mcp_url_prompt(),
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
                        state.mcp.note = Some(said.clone());
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
            if let Err(e) = crate::platform::write_atomically(
                &path,
                crate::mcp_client::config::TEMPLATE.as_bytes(),
            ) && let Some(mut state) = self.state_mut()
            {
                state.message = Some((format!("{}: {e}", path.display()), Instant::now()));
                return;
            }
        }
        // The file is the point: the page gives the column back.
        if let Some(mut state) = self.state_mut() {
            state.mcp.details = false;
        }
        self.run_pick(Pick::File(path));
    }

    /// The program a new entry runs: the Open panel's choice. A test
    /// instance never shows the panel: `CRC_MCP_ADD` names it instead.
    fn choose_mcp_program(&self) -> Option<std::path::PathBuf> {
        if self.ivars().testing {
            return std::env::var_os("CRC_MCP_ADD").map(Into::into);
        }
        choose_path(
            MainThreadMarker::from(self),
            false,
            Some("Choose the installed MCP server program to run"),
        )
    }

    /// Writes `entry` into `mcp.json` under a name made from it, reads the
    /// file again and shows the new server on the page.
    fn add_mcp_server(&self, entry: crate::mcp_client::ServerConfig) {
        let name = {
            let Some(state) = self.state() else {
                return;
            };
            let taken: Vec<String> = state.mcp.servers.iter().map(|s| s.name.clone()).collect();
            crate::mcp_client::config::suggest_name(&entry, &taken)
        };
        let written = crate::mcp_client::config::add(&name, &entry);
        let Some(mut state) = self.state_mut() else {
            return;
        };
        let said = match written {
            Ok(path) => {
                state.mcp.reload();
                state.mcp.selected = state.mcp.find(&name);
                state.mcp.details = true;
                state.mcp.page_scroll = 0.0;
                format!(
                    "Added {name} to {}: Start runs it",
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string())
                )
            }
            Err(e) => format!("Not added: {e}"),
        };
        state.mcp.note = Some(said.clone());
        state.message = Some((said, Instant::now()));
    }

    /// Add by URL: the palette takes the address; Return adds it.
    fn open_mcp_url_prompt(&self) {
        self.open_palette_with("https://");
        if let Some(mut state) = self.state_mut() {
            state.mcp_url_prompt = true;
        }
    }

    /// The palette's answer to Add by URL.
    pub(super) fn add_mcp_url(&self, url: String) {
        self.add_mcp_server(crate::mcp_client::ServerConfig {
            url: Some(url),
            ..Default::default()
        });
        self.request_redraw();
        self.pump();
    }

    /// A call document in a tab of its own, ready to edit and run.
    fn open_call_document(&self, title: &str, text: &str) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            state.docs.push(Buffer::generated(title, "json", text));
            // The document is the point: the page gives the column back.
            state.mcp.details = false;
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
            // The answer is the point: the page gives the column back.
            state.mcp.details = false;
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

    /// A right-click in MCP Servers: what the row under the pointer does,
    /// then the panel's own commands.
    pub(super) fn mcp_context_menu(&self, x: f32, y: f32) -> Retained<NSMenu> {
        use crate::mcp_client::Status;
        let mtm = MainThreadMarker::from(self);
        let menu = NSMenu::new(mtm);
        menu.setAllowsContextMenuPlugIns(false);
        let add = |title: &str, action: Sel| menu.addItem(&menu_item(mtm, title, action));
        let row = self.state().and_then(|state| {
            let action = state
                .mcp
                .hits
                .iter()
                .find(|(r, _)| r.contains(x, y))
                .map(|(_, a)| a.clone())?;
            let title = match &action {
                McpAction::Toggle(si) => match state.mcp.servers.get(*si).map(|s| &s.status) {
                    Some(Status::Ready | Status::Starting) => "Stop Server",
                    _ => "Start Server",
                },
                McpAction::Tool(..) => "Call Tool\u{2026}",
                McpAction::Prompt(..) => "Get Prompt\u{2026}",
                McpAction::Resource(..) => "Read Resource",
                McpAction::Select(..) => "Show Details",
                McpAction::Edit
                | McpAction::Reload
                | McpAction::Home
                | McpAction::Close
                | McpAction::AddCommand
                | McpAction::AddUrl => return None,
            };
            Some((action, title))
        });
        if let Some((action, title)) = row {
            if let Some(mut state) = self.state_mut() {
                state.context_mcp = Some(action);
            }
            add(title, sel!(mcpContextRun:));
            menu.addItem(&NSMenuItem::separatorItem(mtm));
        }
        add("Add Server\u{2026}", sel!(addMcpServer:));
        add("Add Server by URL\u{2026}", sel!(addMcpServerUrl:));
        add("Edit mcp.json", sel!(mcpEdit:));
        add("Reload Servers", sel!(mcpReload:));
        menu
    }
}

/// Whether the MCP page has the editor column.
pub(super) fn mcp_details(state: &State) -> bool {
    state.mcp.open && state.mcp.details
}

/// Add by URL's rows: the one address typed, once it is one.
pub(super) fn mcp_url_rows(query: &str) -> Vec<(layout::PaletteRow, Pick)> {
    let url = query.trim();
    let scheme_only = url == "http://" || url == "https://";
    if scheme_only || !(url.starts_with("http://") || url.starts_with("https://")) {
        return Vec::new();
    }
    let entry = crate::mcp_client::ServerConfig {
        url: Some(url.to_owned()),
        ..Default::default()
    };
    vec![(
        layout::PaletteRow {
            icon: None,
            title: format!(
                "Add \u{201c}{}\u{201d}",
                crate::mcp_client::config::suggest_name(&entry, &[])
            ),
            detail: format!("{url} \u{b7} written to mcp.json, started by a click"),
            shortcut: String::new(),
        },
        Pick::McpUrl(url.to_owned()),
    )]
}
