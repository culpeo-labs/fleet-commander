//! Application state machine.
//!
//! The UI is structured as two screens:
//!
//!   * `Screen::AgentList` — top-level overview of all agents.
//!   * `Screen::AgentSession` — immersive view of a single agent. The
//!     conversation/history is the main pane; a `SidePane` (Diff or Editor)
//!     can appear on the right, but only when invoked by a change event or
//!     by the user.
//!
//! Input handling is dispatched per-screen so a keypress can never silently
//! mutate a hidden buffer.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{info, warn};

use fleet_commander_core::session::{MessageStatus, SessionEvent, ToolCallStatusKind};
use fleet_commander_core::workspace_fs::LocalFs;
use fleet_commander_core::{agent_runtime, container};

use crate::agent::{Agent, AgentId, AgentStatus, HistoryEntry};
use crate::agent_kind::AgentKind;
use crate::change_source::ChangeEvent;
use crate::completion::{PathCompleter, split_command_and_path};
use crate::config::{Action, Config};
use crate::event::AppEvent;
use crate::explorer::ExplorerState;
use crate::init;
use crate::workspace;

#[derive(Debug, Clone)]
pub enum Screen {
    AgentList {
        selected: usize,
    },
    AgentSession {
        agent_id: AgentId,
        focus: SessionFocus,
        side_pane: Option<SidePane>,
        scroll: usize,
        /// When true, the user is typing a message to send to the agent.
        input_mode: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionFocus {
    Conversation,
    SidePane,
    Explorer,
}

#[derive(Debug, Clone)]
pub enum SidePane {
    Diff { path: PathBuf, content: String },
}

pub struct App {
    pub config: Config,
    pub agents: Vec<Agent>,
    pub screen: Screen,
    pub should_quit: bool,
    /// Text the user is composing in insert mode.
    pub input_buffer: String,
    /// When true, the user is typing a `:` command (vim-style command mode).
    pub command_mode: bool,
    /// Buffer for the current command being typed.
    pub command_buffer: String,
    /// Ephemeral status message shown in the footer (e.g. error from a command).
    pub status_message: Option<String>,
    /// Tab-completion state for command mode paths.
    pub completer: PathCompleter,
    /// Channel for sending events (used to dispatch messages to agents).
    pub tx: mpsc::UnboundedSender<AppEvent>,
    /// Channel handed to the runtime crate for it to emit `RuntimeEvent`s.
    /// A bridge task in `main.rs` forwards these into `tx` as `AppEvent`s.
    pub runtime_tx: mpsc::UnboundedSender<SessionEvent>,
    /// Set when an agent needs interactive auth — the main loop suspends the
    /// TUI and runs this command with inherited stdio.
    pub auth_pending: Option<(AgentId, Vec<String>)>,
    /// Pending tool permission request awaiting user response.
    /// Contains (tool_name, options: Vec<(id, label, kind)>, reply_channel).
    pub permission_pending: Option<PendingPermission>,
    /// Shared handle to the ACP wire-log file when `--acp-log` was passed.
    /// `None` disables logging.
    pub acp_log: Option<Arc<Mutex<File>>>,
    /// Optional substring filter applied to agent id when deciding whether
    /// to enable ACP logging for a given agent.
    pub acp_log_filter: Option<String>,
    /// Lazy tree-view state for the current agent's workspace. Reset
    /// whenever the active workspace changes. Toggle visibility with
    /// `Ctrl+E`.
    pub explorer: ExplorerState,
}

/// A tool permission request waiting for the user's y/n decision.
pub struct PendingPermission {
    pub tool_name: String,
    pub options: Vec<(String, String, String)>,
    pub reply: crate::event::PermissionReply,
}

impl App {
    #[cfg(test)]
    pub fn new(config: Config, agents: Vec<Agent>, tx: mpsc::UnboundedSender<AppEvent>) -> Self {
        let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
        Self::with_acp_log(config, agents, tx, runtime_tx, None, None)
    }

    pub fn with_acp_log(
        config: Config,
        agents: Vec<Agent>,
        tx: mpsc::UnboundedSender<AppEvent>,
        runtime_tx: mpsc::UnboundedSender<SessionEvent>,
        acp_log: Option<Arc<Mutex<File>>>,
        acp_log_filter: Option<String>,
    ) -> Self {
        Self {
            config,
            agents,
            screen: Screen::AgentList { selected: 0 },
            should_quit: false,
            input_buffer: String::new(),
            command_mode: false,
            command_buffer: String::new(),
            status_message: None,
            completer: PathCompleter::default(),
            tx,
            runtime_tx,
            auth_pending: None,
            permission_pending: None,
            acp_log,
            acp_log_filter,
            explorer: ExplorerState::default(),
        }
    }

    pub fn handle(&mut self, event: AppEvent) {
        match event {
            AppEvent::Input(key) => self.handle_key(key),
            AppEvent::Change(change) => self.handle_change(change),
            AppEvent::McpShowDiff {
                agent_id,
                path,
                content,
            } => self.handle_mcp_side_pane(agent_id, SidePane::Diff { path, content }),
            AppEvent::McpShowFile {
                agent_id,
                path,
                content,
            } => self.handle_mcp_side_pane(agent_id, SidePane::Diff { path, content }),
            AppEvent::McpNotify { agent_id, message } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.info(message);
                }
            }
            AppEvent::ReconnectAgent { agent_id } => {
                info!(agent_id = %agent_id, "Reconnecting agent after rebuild");
                self.ensure_agent_connected(agent_id);
            }
            AppEvent::Repaint => {
                // No-op: the redraw is performed by the main loop after this
                // handler returns. Repaint events exist purely to wake the
                // event loop when a tracked handle (tool call, streaming
                // text, etc.) ticks. We deliberately do not snap scroll
                // here — if the user is reading history, leave them alone.
            }
            AppEvent::ExplorerStatusReady {
                root,
                include_ignored,
                result,
            } => {
                // Drop responses from a previous workspace or a stale
                // include_ignored value — they would overwrite the
                // current state with the wrong data.
                let root_matches = self
                    .explorer
                    .fs
                    .as_ref()
                    .map(|fs| fs.root_display() == root)
                    .unwrap_or(false);
                if root_matches && include_ignored == self.explorer.show_ignored {
                    self.explorer.apply_status(result);
                } else {
                    self.explorer.is_refreshing = false;
                }
                if self.explorer.refresh_pending {
                    self.request_explorer_refresh();
                }
            }
            AppEvent::Session(event) => self.handle_session_event(event),
        }
    }

    fn handle_session_event(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::Output { agent_id, line } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.info(line);
                }
            }
            SessionEvent::Exited { agent_id, .. } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.status = AgentStatus::Stopped;
                    agent.prompt_tx = None;
                    agent.task_handle = None;
                }
            }
            SessionEvent::Error { agent_id, message } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.error(format!("[error] {message}"));
                    agent.status = AgentStatus::Error;
                }
            }
            SessionEvent::Connected {
                agent_id,
                session_id,
            } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.status = AgentStatus::Idle;
                    agent.session_id = session_id.clone();
                    agent.info("ACP session connected.");
                    if let Some(ws) = &agent.workspace_folder {
                        let state = workspace::WorkspaceState { session_id };
                        if let Err(e) = workspace::save_state(ws, &state) {
                            warn!(error = %e, "Failed to save workspace state");
                        }
                    }
                }
            }
            SessionEvent::AuthRequired { agent_id, command } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.info("🔑 Authentication required — launching login flow...");
                    agent.status = AgentStatus::Stopped;
                    agent.prompt_tx = None;
                    // The runtime task already returned after sending this
                    // event; clear its handle so ensure_agent_connected can
                    // spawn a fresh task after login completes.
                    agent.task_handle = None;
                }
                self.auth_pending = Some((agent_id, command));
            }
            SessionEvent::PermissionRequest {
                agent_id,
                tool_name,
                options,
                reply,
            } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.info(format!("🔐 Permission requested: {tool_name}"));
                }
                self.permission_pending = Some(PendingPermission {
                    tool_name,
                    options,
                    reply,
                });
            }
            SessionEvent::AssistantMessage { agent_id, message } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.status = AgentStatus::Running;
                    agent.history.push(HistoryEntry::Assistant(message.clone()));
                }
                spawn_text_tracker(message.text, message.status, self.tx.clone());
            }
            SessionEvent::Thought { agent_id, thought } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.history.push(HistoryEntry::Thought(thought.clone()));
                }
                spawn_text_tracker(thought.text, thought.status, self.tx.clone());
            }
            SessionEvent::UserMessage { agent_id, message } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.history.push(HistoryEntry::User(message.clone()));
                }
                spawn_text_tracker(message.text, message.status, self.tx.clone());
            }
            SessionEvent::ToolCall { agent_id, call } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.history.push(HistoryEntry::Tool(call.clone()));
                }
                spawn_tool_tracker(call.title, call.status, self.tx.clone());
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Clear status message on any keypress.
        self.status_message = None;

        // Permission prompt — intercept y/n/Esc before anything else.
        if self.permission_pending.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let perm = self.permission_pending.take().unwrap();
                    // Pick the first "allow" option.
                    let allow_id = perm
                        .options
                        .iter()
                        .find(|(_, _, kind)| kind.starts_with("allow"))
                        .map(|(id, _, _)| id.clone());
                    if let Ok(mut guard) = perm.reply.lock()
                        && let Some(tx) = guard.take()
                    {
                        let _ = tx.send(allow_id);
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    let perm = self.permission_pending.take().unwrap();
                    if let Ok(mut guard) = perm.reply.lock()
                        && let Some(tx) = guard.take()
                    {
                        let _ = tx.send(None);
                    }
                }
                _ => {}
            }
            return;
        }

        // Command mode (`:` prompt) — intercept all keys.
        if self.command_mode {
            match key.code {
                KeyCode::Esc => {
                    self.command_mode = false;
                    self.command_buffer.clear();
                    self.completer.reset();
                }
                KeyCode::Enter => {
                    let cmd = std::mem::take(&mut self.command_buffer);
                    self.command_mode = false;
                    self.completer.reset();
                    self.execute_command(&cmd);
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    let (verb, partial) = split_command_and_path(&self.command_buffer);
                    let verb = verb.to_string();
                    // Only complete paths for commands that take a path arg.
                    if matches!(verb.as_str(), "open" | "o") {
                        let partial = partial.to_string();
                        let completed = if key.code == KeyCode::Tab {
                            self.completer.complete(&partial).map(String::from)
                        } else {
                            self.completer.complete_prev(&partial).map(String::from)
                        };
                        if let Some(path) = completed {
                            self.command_buffer = format!("{verb} {path}");
                        }
                    }
                }
                KeyCode::Backspace => {
                    self.completer.reset();
                    if self.command_buffer.pop().is_none() {
                        self.command_mode = false;
                    }
                }
                KeyCode::Char(c) => {
                    self.completer.reset();
                    self.command_buffer.push(c);
                }
                _ => {}
            }
            return;
        }

        // In input mode, capture text instead of dispatching actions.
        if let Screen::AgentSession {
            input_mode: true,
            agent_id,
            ..
        } = &self.screen
        {
            match key.code {
                KeyCode::Esc => {
                    if let Screen::AgentSession { input_mode, .. } = &mut self.screen {
                        *input_mode = false;
                    }
                    self.input_buffer.clear();
                }
                KeyCode::Enter => {
                    // Alt+Enter / Shift+Enter insert a newline so the user
                    // can compose multi-line messages. Plain Enter sends.
                    if key
                        .modifiers
                        .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT)
                    {
                        self.input_buffer.push('\n');
                        return;
                    }
                    let message = std::mem::take(&mut self.input_buffer);
                    if !message.is_empty() {
                        let agent_id = agent_id.clone();
                        if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                            agent.prompt(message.clone());
                            agent_runtime::send_message(
                                agent.id.clone(),
                                agent.prompt_tx.as_ref(),
                                message,
                                self.runtime_tx.clone(),
                            );
                        }
                        self.auto_scroll_for(&agent_id);
                    }
                    if let Screen::AgentSession { input_mode, .. } = &mut self.screen {
                        *input_mode = false;
                    }
                }
                KeyCode::Backspace => {
                    self.input_buffer.pop();
                }
                KeyCode::Char(c) => {
                    self.input_buffer.push(c);
                }
                _ => {}
            }
            return;
        }

        let Some(action) = self.config.bindings.action_for(&key) else {
            // Explorer-focus-specific character keys that aren't part of
            // the global Action set: `r` refresh, `.` toggle ignored,
            // `@` insert reference + switch to input mode.
            if let Screen::AgentSession { focus, .. } = &self.screen
                && *focus == SessionFocus::Explorer
                && let KeyCode::Char(c) = key.code
                && !key.modifiers.contains(KeyModifiers::CONTROL)
            {
                match c {
                    'r' => self.request_explorer_refresh(),
                    '.' => {
                        self.explorer.show_ignored = !self.explorer.show_ignored;
                        // Re-query because the include_ignored flag changes
                        // what git returns.
                        self.request_explorer_refresh();
                    }
                    '@' => {
                        if let Some(entry) = self.explorer.selected_entry() {
                            let path = entry.path.display().to_string();
                            if !self.input_buffer.is_empty() && !self.input_buffer.ends_with(' ') {
                                self.input_buffer.push(' ');
                            }
                            self.input_buffer.push('@');
                            self.input_buffer.push_str(&path);
                            self.input_buffer.push(' ');
                            if let Screen::AgentSession {
                                input_mode, focus, ..
                            } = &mut self.screen
                            {
                                *input_mode = true;
                                *focus = SessionFocus::Conversation;
                            }
                        }
                    }
                    _ => {}
                }
            }
            return;
        };

        // Command mode activation works on any screen.
        if action == Action::Command {
            self.command_mode = true;
            self.command_buffer.clear();
            return;
        }

        let next = match &mut self.screen {
            Screen::AgentList { selected } => {
                handle_list_action(action, selected, &self.agents, &mut self.should_quit)
            }
            Screen::AgentSession {
                agent_id,
                focus,
                side_pane,
                scroll,
                ..
            } => handle_session_action(
                action,
                agent_id,
                focus,
                side_pane,
                scroll,
                &self.agents,
                &mut self.explorer,
            ),
        };
        if let Some(next) = next {
            self.screen = next;
            // Lazily start ACP connection when entering an agent session.
            if let Screen::AgentSession { agent_id, .. } = &self.screen {
                self.ensure_agent_connected(agent_id.clone());
            }
        }
        // Toggling the explorer open is the one mutation handle_session_action
        // makes that the user expects to see freshly-resolved git status for.
        // Issue the refresh from here because spawning the background task
        // needs `&mut App`.
        if action == Action::ToggleExplorer && self.explorer.open && self.explorer.fs.is_some() {
            self.request_explorer_refresh();
        }
    }

    /// Spawn a background `git status` for the active workspace and
    /// pump the result back into the event loop as
    /// [`AppEvent::ExplorerStatusReady`]. Coalesces bursty callers:
    /// if a refresh is already in flight, sets a pending flag so a
    /// follow-up runs once the in-flight one lands.
    ///
    /// Cheap no-op when the explorer has no filesystem attached **or
    /// is closed** — there's no point spending cycles updating git
    /// state the user can't see.
    pub fn request_explorer_refresh(&mut self) {
        if !self.explorer.open {
            return;
        }
        let Some(fs) = self.explorer.fs.clone() else {
            return;
        };
        if self.explorer.is_refreshing {
            self.explorer.refresh_pending = true;
            return;
        }
        self.explorer.is_refreshing = true;
        self.explorer.refresh_pending = false;
        let include_ignored = self.explorer.show_ignored;
        let root = fs.root_display().to_path_buf();
        let tx = self.tx.clone();
        // `git status` is a sync subprocess; off-load it to the blocking
        // pool so the UI loop keeps draining events while it runs.
        tokio::task::spawn_blocking(move || {
            let result = fs.git_status(include_ignored).map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::ExplorerStatusReady {
                root,
                include_ignored,
                result,
            });
        });
    }

    /// Start the ACP connection for an agent if not already connected.
    pub fn ensure_agent_connected(&mut self, agent_id: AgentId) {
        // Point the explorer at this agent's workspace (no-op if same root).
        // This happens on every screen change into AgentSession, including
        // when the agent is already connected, so the explorer always reflects
        // the currently-viewed agent.
        if let Some(agent) = self.agents.iter().find(|a| a.id == agent_id) {
            let fs = agent.workspace_folder.as_ref().map(|ws| {
                Arc::new(LocalFs::new(ws))
                    as Arc<dyn fleet_commander_core::workspace_fs::WorkspaceFs>
            });
            let had_fs = self.explorer.fs.is_some();
            self.explorer.set_fs(fs);
            // Refresh status when the workspace is set for the first time
            // (or when switching to a new agent's workspace cleared state).
            if self.explorer.fs.is_some() && (!had_fs || self.explorer.status.is_empty()) {
                self.request_explorer_refresh();
            }
        }
        let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) else {
            return;
        };
        // A task that already finished should not block a reconnect. This
        // can happen when the previous run exited cleanly (e.g. AuthRequired)
        // without the cleanup path clearing the handle.
        if let Some(handle) = &agent.task_handle
            && handle.is_finished()
        {
            agent.task_handle = None;
        }
        if agent.prompt_tx.is_some() || agent.task_handle.is_some() {
            return; // Already connected or connecting.
        }
        if agent.acp_command.is_empty() {
            return; // No command configured.
        }
        let log_for_agent = match (&self.acp_log, &self.acp_log_filter) {
            (Some(log), Some(pattern)) if agent.id.contains(pattern) => Some(log.clone()),
            (Some(log), None) => Some(log.clone()),
            _ => None,
        };
        let (prompt_tx, abort_handle) = agent_runtime::start_agent(
            agent.id.clone(),
            agent.effective_acp_command(),
            agent.workspace_folder.clone(),
            agent.session_id.clone(),
            self.runtime_tx.clone(),
            log_for_agent,
        );
        agent.prompt_tx = Some(prompt_tx);
        agent.task_handle = Some(abort_handle);
        agent.status = AgentStatus::Running;
        let label = match &agent.workspace_folder {
            Some(ws) => format!("Starting container ({})...", ws.display()),
            None => "Connecting...".into(),
        };
        agent.info(label);
    }

    /// Scroll to the bottom when content arrives for the currently viewed agent.
    /// Snap to the bottom of the conversation pane for the agent that the
    /// user is currently viewing.
    ///
    /// Called by callers (e.g. side-pane updates) that need to force an
    /// auto-scroll regardless of where the user has scrolled to. Routine
    /// streaming updates do **not** call this — they leave `scroll` alone
    /// so the user can read history without being yanked back to the bottom
    /// every time a chunk arrives. The user can re-engage follow-bottom
    /// with `Action::FollowBottom` (bound to `G` by default).
    fn auto_scroll_for(&mut self, agent_id: &str) {
        if let Screen::AgentSession {
            agent_id: current,
            scroll,
            ..
        } = &mut self.screen
        {
            if current != agent_id {
                return;
            }
            *scroll = usize::MAX;
        }
    }

    fn handle_change(&mut self, change: ChangeEvent) {
        if let Screen::AgentSession { side_pane, .. } = &mut self.screen {
            let content = std::fs::read_to_string(&change.path).unwrap_or_default();
            *side_pane = Some(SidePane::Diff {
                path: change.path,
                content,
            });
        }
        if self.explorer.fs.is_some() {
            self.request_explorer_refresh();
        }
    }

    /// Open or replace the side pane when an MCP tool targets a specific agent.
    /// If that agent's session is currently visible, the pane updates immediately.
    /// If the agent list is showing, we navigate into the agent's session.
    fn handle_mcp_side_pane(&mut self, agent_id: AgentId, pane: SidePane) {
        match &mut self.screen {
            Screen::AgentSession {
                agent_id: current,
                side_pane,
                ..
            } if *current == agent_id => {
                *side_pane = Some(pane);
            }
            _ => {
                self.screen = Screen::AgentSession {
                    agent_id,
                    focus: SessionFocus::Conversation,
                    side_pane: Some(pane),
                    scroll: 0,
                    input_mode: false,
                };
            }
        }
        // A new diff means files on disk likely changed — refresh the
        // explorer's git cues so the user sees the same files highlighted.
        if self.explorer.fs.is_some() {
            self.request_explorer_refresh();
        }
    }

    /// Parse and execute a `:` command.
    fn execute_command(&mut self, cmd: &str) {
        let cmd = cmd.trim();
        if cmd.is_empty() {
            return;
        }
        let (verb, rest) = cmd.split_once(' ').unwrap_or((cmd, ""));
        let rest = rest.trim();
        match verb {
            "open" | "o" => {
                if rest.is_empty() {
                    self.status_message = Some("Usage: :open <path/to/repo>".into());
                } else {
                    self.open_workspace(rest);
                }
            }
            "close" => {
                self.close_current_workspace();
            }
            "rebuild" => {
                self.rebuild_current_container();
            }
            "q" | "quit" => {
                self.should_quit = true;
            }
            _ => {
                self.status_message = Some(format!("Unknown command: {verb}"));
            }
        }
    }

    /// Create a new Copilot agent for the given workspace path and navigate to it.
    pub fn open_workspace(&mut self, path: &str) {
        let workspace = PathBuf::from(path);
        let dir_name = workspace
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("workspace");
        let agent_id = format!("copilot-{dir_name}");

        // Check if an agent with this workspace already exists.
        if let Some(existing) = self.agents.iter().find(|a| a.id == agent_id) {
            self.screen = Screen::AgentSession {
                agent_id: existing.id.clone(),
                focus: SessionFocus::Conversation,
                side_pane: None,
                scroll: 0,
                input_mode: false,
            };
            self.ensure_agent_connected(agent_id);
            return;
        }

        let agent = Agent::new(&agent_id, format!("Copilot ({dir_name})"))
            .with_acp_command("copilot --acp --stdio")
            .with_workspace(&workspace);
        self.agents.push(agent);

        // Generate per-workspace base layer (mounts, env, etc.).
        if let Err(err) = init::generate_workspace_layer(&workspace, AgentKind::Copilot) {
            self.status_message = Some(format!("Layer warning: {err}"));
        }

        // Persist to workspaces.yaml.
        if let Err(err) = workspace::save(&workspace::from_agents(&self.agents)) {
            self.status_message = Some(format!("Warning: {err}"));
        }

        self.screen = Screen::AgentSession {
            agent_id: agent_id.clone(),
            focus: SessionFocus::Conversation,
            side_pane: None,
            scroll: 0,
            input_mode: false,
        };
        self.ensure_agent_connected(agent_id);
    }

    /// Remove the currently viewed workspace agent and go back to the list.
    fn close_current_workspace(&mut self) {
        let agent_id = match &self.screen {
            Screen::AgentSession { agent_id, .. } => agent_id.clone(),
            _ => {
                self.status_message = Some("No workspace open — use :close from a session".into());
                return;
            }
        };

        self.agents.retain(|a| a.id != agent_id);

        // Persist removal.
        if let Err(err) = workspace::save(&workspace::from_agents(&self.agents)) {
            self.status_message = Some(format!("Warning: {err}"));
        }

        self.screen = Screen::AgentList { selected: 0 };
    }

    /// Rebuild the container for the currently viewed workspace agent.
    ///
    /// Stops and removes the existing container, regenerates the base layer,
    /// clears session_id (a rebuild invalidates the session), then reconnects.
    fn rebuild_current_container(&mut self) {
        let agent_id = match &self.screen {
            Screen::AgentSession { agent_id, .. } => agent_id.clone(),
            _ => {
                self.status_message =
                    Some("No workspace open — use :rebuild from a session".into());
                return;
            }
        };

        let agent = match self.agents.iter_mut().find(|a| a.id == agent_id) {
            Some(a) => a,
            None => return,
        };

        let workspace = match &agent.workspace_folder {
            Some(ws) => ws.clone(),
            None => {
                self.status_message =
                    Some("Agent has no workspace — :rebuild needs a container agent".into());
                return;
            }
        };

        info!(agent_id = %agent_id, workspace = %workspace.display(), "Rebuilding container");

        // Abort the existing agent task so it doesn't compete with the new one.
        if let Some(handle) = agent.task_handle.take() {
            handle.abort();
        }
        // Drop existing connection.
        agent.prompt_tx = None;
        agent.status = AgentStatus::Stopped;
        agent.session_id = None;
        agent.info("🔄 Rebuilding container...");

        // Regenerate base layer with latest mount config.
        if let Err(err) = init::generate_workspace_layer(&workspace, AgentKind::Copilot) {
            warn!(error = %err, "Failed to regenerate workspace layer");
            self.status_message = Some(format!("Layer warning: {err}"));
        }

        // Stop + remove the container asynchronously, then reconnect.
        let tx = self.tx.clone();
        let aid = agent_id;
        tokio::spawn(async move {
            if let Err(err) = container::remove_workspace_container(&workspace).await {
                let _ = tx.send(AppEvent::Session(SessionEvent::Output {
                    agent_id: aid.clone(),
                    line: format!("[warn] Failed to remove container: {err}"),
                }));
            }
            let _ = tx.send(AppEvent::Session(SessionEvent::Output {
                agent_id: aid.clone(),
                line: "Container removed. Reconnecting...".into(),
            }));
            let _ = tx.send(AppEvent::ReconnectAgent { agent_id: aid });
        });

        // Persist the cleared session_id.
        if let Err(err) = workspace::save(&workspace::from_agents(&self.agents)) {
            self.status_message = Some(format!("Warning: {err}"));
        }
    }
}

/// Spawn a tracker task for a streaming text handle (assistant, thought,
/// user). Sends `AppEvent::Repaint` whenever the handle's text or status
/// changes; terminates when the status reaches a terminal state or either
/// watch channel is closed.
fn spawn_text_tracker(
    mut text: tokio::sync::watch::Receiver<String>,
    mut status: tokio::sync::watch::Receiver<MessageStatus>,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                res = text.changed() => {
                    if res.is_err() {
                        let _ = tx.send(AppEvent::Repaint);
                        break;
                    }
                }
                res = status.changed() => {
                    if res.is_err() {
                        let _ = tx.send(AppEvent::Repaint);
                        break;
                    }
                }
            }
            let _ = tx.send(AppEvent::Repaint);
            if status.borrow().is_terminal() {
                break;
            }
        }
    });
}

/// Spawn a tracker task for a tool-call handle. Like `spawn_text_tracker`
/// but watches `title` + `status` instead.
fn spawn_tool_tracker(
    mut title: tokio::sync::watch::Receiver<String>,
    mut status: tokio::sync::watch::Receiver<ToolCallStatusKind>,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                res = title.changed() => {
                    if res.is_err() {
                        let _ = tx.send(AppEvent::Repaint);
                        break;
                    }
                }
                res = status.changed() => {
                    if res.is_err() {
                        let _ = tx.send(AppEvent::Repaint);
                        break;
                    }
                }
            }
            let _ = tx.send(AppEvent::Repaint);
            if status.borrow().is_terminal() {
                break;
            }
        }
    });
}

fn handle_list_action(
    action: Action,
    selected: &mut usize,
    agents: &[Agent],
    should_quit: &mut bool,
) -> Option<Screen> {
    match action {
        Action::Quit => {
            *should_quit = true;
            None
        }
        Action::Down if !agents.is_empty() => {
            *selected = (*selected + 1) % agents.len();
            None
        }
        Action::Up if !agents.is_empty() => {
            *selected = if *selected == 0 {
                agents.len() - 1
            } else {
                *selected - 1
            };
            None
        }
        Action::Activate => agents.get(*selected).map(|agent| Screen::AgentSession {
            agent_id: agent.id.clone(),
            focus: SessionFocus::Conversation,
            side_pane: None,
            scroll: 0,
            input_mode: false,
        }),
        _ => None,
    }
}

fn handle_session_action(
    action: Action,
    agent_id: &AgentId,
    focus: &mut SessionFocus,
    side_pane: &mut Option<SidePane>,
    scroll: &mut usize,
    agents: &[Agent],
    explorer: &mut ExplorerState,
) -> Option<Screen> {
    // Explorer-focused actions: arrows navigate the tree, Enter expands
    // dirs / opens files, DismissPane returns focus to conversation.
    if *focus == SessionFocus::Explorer {
        return handle_explorer_action(action, focus, side_pane, agents, agent_id, explorer);
    }
    match action {
        Action::Back => {
            let idx = agents.iter().position(|a| &a.id == agent_id).unwrap_or(0);
            Some(Screen::AgentList { selected: idx })
        }
        Action::Insert => Some(Screen::AgentSession {
            agent_id: agent_id.clone(),
            focus: SessionFocus::Conversation,
            side_pane: side_pane.clone(),
            scroll: *scroll,
            input_mode: true,
        }),
        Action::DismissPane if side_pane.is_some() => {
            *side_pane = None;
            *focus = SessionFocus::Conversation;
            None
        }
        Action::TogglePane => {
            // Cycle: Conversation -> Explorer (if open) -> SidePane (if open) -> Conversation
            *focus = next_focus(*focus, explorer.open, side_pane.is_some());
            None
        }
        Action::ToggleExplorer => {
            explorer.open = !explorer.open;
            if explorer.open {
                // Refresh happens in the caller because it's an async
                // operation that needs `&mut App` to spawn the task.
                *focus = SessionFocus::Explorer;
            } else if *focus == SessionFocus::Explorer {
                *focus = SessionFocus::Conversation;
            }
            None
        }
        Action::Down => {
            *scroll = scroll.saturating_add(1);
            None
        }
        Action::Up => {
            if *scroll == usize::MAX {
                // Currently following the bottom — break out of follow mode
                // by anchoring at whatever line is currently at the top of
                // the viewport, then step up by one.
                let top = agents
                    .iter()
                    .find(|a| &a.id == agent_id)
                    .map(|a| a.last_effective_top.get())
                    .unwrap_or(0);
                *scroll = top.saturating_sub(1);
            } else {
                *scroll = scroll.saturating_sub(1);
            }
            None
        }
        Action::FollowBottom => {
            *scroll = usize::MAX;
            None
        }
        _ => None,
    }
}

/// Tab cycle through visible panes. Skips panes that aren't currently
/// showing so the user never lands on an invisible focus target.
fn next_focus(current: SessionFocus, explorer_open: bool, side_pane_open: bool) -> SessionFocus {
    let order = [
        SessionFocus::Conversation,
        SessionFocus::Explorer,
        SessionFocus::SidePane,
    ];
    let idx = order.iter().position(|f| *f == current).unwrap_or(0);
    for step in 1..=order.len() {
        let candidate = order[(idx + step) % order.len()];
        let visible = match candidate {
            SessionFocus::Conversation => true,
            SessionFocus::Explorer => explorer_open,
            SessionFocus::SidePane => side_pane_open,
        };
        if visible {
            return candidate;
        }
    }
    SessionFocus::Conversation
}

fn handle_explorer_action(
    action: Action,
    focus: &mut SessionFocus,
    side_pane: &mut Option<SidePane>,
    agents: &[Agent],
    agent_id: &AgentId,
    explorer: &mut ExplorerState,
) -> Option<Screen> {
    match action {
        Action::ToggleExplorer => {
            explorer.open = false;
            *focus = SessionFocus::Conversation;
            None
        }
        Action::TogglePane => {
            *focus = next_focus(*focus, explorer.open, side_pane.is_some());
            None
        }
        Action::Back => {
            // Esc unfocuses the explorer without closing the pane, mirroring
            // how Esc dismisses other modal focus states.
            *focus = SessionFocus::Conversation;
            None
        }
        Action::Down => {
            explorer.move_cursor(1);
            None
        }
        Action::Up => {
            explorer.move_cursor(-1);
            None
        }
        Action::Activate => {
            if let Some(entry) = explorer.selected_entry() {
                if entry.is_dir {
                    explorer.toggle_selected_dir();
                } else if let Some(fs) = explorer.fs.clone()
                    && let Ok(bytes) = fs.read_file(&entry.path)
                {
                    let content = String::from_utf8_lossy(&bytes).into_owned();
                    let full = fs.root_display().join(&entry.path);
                    *side_pane = Some(SidePane::Diff {
                        path: full,
                        content,
                    });
                }
            }
            // Touch agents/agent_id to keep the signature usable for future
            // per-agent dispatch (e.g. sending the file path to the agent).
            let _ = agents;
            let _ = agent_id;
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change_source::{ChangeEvent, ChangeKind};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn app_with_agents() -> App {
        let agents = vec![
            Agent::new("a1", "First"),
            Agent::new("a2", "Second"),
            Agent::new("a3", "Third"),
        ];
        let (tx, _rx) = mpsc::unbounded_channel();
        App::new(Config::default(), agents, tx)
    }

    fn press(code: KeyCode) -> AppEvent {
        AppEvent::Input(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn down_then_up_moves_selection() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Char('j')));
        match app.screen {
            Screen::AgentList { selected } => assert_eq!(selected, 1),
            _ => panic!("expected AgentList"),
        }
        app.handle(press(KeyCode::Char('k')));
        match app.screen {
            Screen::AgentList { selected } => assert_eq!(selected, 0),
            _ => panic!("expected AgentList"),
        }
    }

    #[test]
    fn down_wraps_around() {
        let mut app = app_with_agents();
        for _ in 0..app.agents.len() {
            app.handle(press(KeyCode::Char('j')));
        }
        match app.screen {
            Screen::AgentList { selected } => assert_eq!(selected, 0),
            _ => panic!("expected AgentList"),
        }
    }

    #[test]
    fn activate_enters_session_for_selected_agent() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Char('j')));
        app.handle(press(KeyCode::Enter));
        match &app.screen {
            Screen::AgentSession {
                agent_id,
                side_pane,
                focus,
                ..
            } => {
                assert_eq!(agent_id, "a2");
                assert!(side_pane.is_none(), "side pane should start hidden");
                assert_eq!(*focus, SessionFocus::Conversation);
            }
            other => panic!("expected AgentSession, got {other:?}"),
        }
    }

    #[test]
    fn back_returns_to_agent_list_with_prior_selection() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Char('j')));
        app.handle(press(KeyCode::Char('j')));
        app.handle(press(KeyCode::Enter));
        app.handle(press(KeyCode::Esc));
        match app.screen {
            Screen::AgentList { selected } => assert_eq!(selected, 2),
            _ => panic!("expected AgentList"),
        }
    }

    #[test]
    fn change_event_outside_session_is_ignored() {
        let mut app = app_with_agents();
        app.handle(AppEvent::Change(ChangeEvent {
            path: PathBuf::from("/nonexistent"),
            kind: ChangeKind::Modified,
        }));
        assert!(matches!(app.screen, Screen::AgentList { .. }));
    }

    #[test]
    fn change_event_in_session_opens_diff_side_pane() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        app.handle(AppEvent::Change(ChangeEvent {
            path: PathBuf::from("/definitely/missing.rs"),
            kind: ChangeKind::Modified,
        }));
        match &app.screen {
            Screen::AgentSession {
                side_pane: Some(SidePane::Diff { path, .. }),
                ..
            } => {
                assert_eq!(path, &PathBuf::from("/definitely/missing.rs"));
            }
            other => panic!("expected Diff side pane, got {other:?}"),
        }
    }

    #[test]
    fn dismiss_pane_clears_side_pane_and_refocuses_conversation() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        app.handle(AppEvent::Change(ChangeEvent {
            path: PathBuf::from("/x"),
            kind: ChangeKind::Modified,
        }));
        app.handle(press(KeyCode::Tab));
        app.handle(press(KeyCode::Char('d')));
        match &app.screen {
            Screen::AgentSession {
                side_pane, focus, ..
            } => {
                assert!(side_pane.is_none());
                assert_eq!(*focus, SessionFocus::Conversation);
            }
            _ => panic!("expected AgentSession"),
        }
    }

    #[test]
    fn dismiss_with_no_side_pane_is_a_noop() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        app.handle(press(KeyCode::Char('d')));
        match &app.screen {
            Screen::AgentSession {
                side_pane, focus, ..
            } => {
                assert!(side_pane.is_none());
                assert_eq!(*focus, SessionFocus::Conversation);
            }
            _ => panic!("expected AgentSession"),
        }
    }

    #[test]
    fn quit_only_quits_from_agent_list() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        app.handle(press(KeyCode::Char('q')));
        assert!(!app.should_quit, "q in session should not quit the app");
        app.handle(press(KeyCode::Esc));
        app.handle(press(KeyCode::Char('q')));
        assert!(app.should_quit);
    }

    #[test]
    fn agent_output_appends_to_history() {
        let mut app = app_with_agents();
        app.handle(AppEvent::Session(SessionEvent::Output {
            agent_id: "a2".into(),
            line: "hello".into(),
        }));
        let a2 = app.agents.iter().find(|a| a.id == "a2").unwrap();
        assert_eq!(a2.history.len(), 1);
        assert!(matches!(&a2.history[0], HistoryEntry::Info(s) if s == "hello"));
    }

    #[test]
    fn agent_exited_marks_status_stopped() {
        let mut app = app_with_agents();
        app.handle(AppEvent::Session(SessionEvent::Exited {
            agent_id: "a1".into(),
            code: Some(0),
        }));
        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(a1.status, AgentStatus::Stopped);
    }

    #[tokio::test]
    async fn assistant_message_started_appends_handle() {
        use tokio::sync::watch;
        let mut app = app_with_agents();
        let (text_tx, text_rx) = watch::channel(String::new());
        let (status_tx, status_rx) = watch::channel(MessageStatus::Streaming);
        let message = fleet_commander_core::session::AssistantMessage {
            text: text_rx,
            status: status_rx,
        };
        app.handle(AppEvent::Session(SessionEvent::AssistantMessage {
            agent_id: "a1".into(),
            message,
        }));

        let _ = text_tx.send("Hello".to_string());
        let _ = status_tx.send(MessageStatus::Completed);

        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(a1.status, AgentStatus::Running);
        assert_eq!(a1.history.len(), 1);
        match &a1.history[0] {
            HistoryEntry::Assistant(m) => {
                assert_eq!(*m.text.borrow(), "Hello");
                assert_eq!(*m.status.borrow(), MessageStatus::Completed);
            }
            _ => panic!("expected assistant entry"),
        }
    }

    #[test]
    fn session_error_appends_to_history() {
        let mut app = app_with_agents();
        app.handle(AppEvent::Session(SessionEvent::Error {
            agent_id: "a2".into(),
            message: "connection lost".into(),
        }));
        let a2 = app.agents.iter().find(|a| a.id == "a2").unwrap();
        match a2.history.last().unwrap() {
            HistoryEntry::Error(s) => assert!(s.contains("connection lost")),
            other => panic!("expected error entry, got {other:?}"),
        }
        assert_eq!(a2.status, AgentStatus::Error);
    }

    #[test]
    fn insert_action_enters_input_mode() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter)); // enter session
        app.handle(press(KeyCode::Char('i'))); // insert mode
        match &app.screen {
            Screen::AgentSession { input_mode, .. } => assert!(*input_mode),
            _ => panic!("expected AgentSession"),
        }
    }

    #[test]
    fn esc_in_input_mode_cancels_input() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        app.handle(press(KeyCode::Char('i')));
        // Type some text
        app.handle(AppEvent::Input(KeyEvent::new(
            KeyCode::Char('h'),
            KeyModifiers::NONE,
        )));
        assert_eq!(app.input_buffer, "h");
        // Esc cancels
        app.handle(press(KeyCode::Esc));
        match &app.screen {
            Screen::AgentSession { input_mode, .. } => assert!(!*input_mode),
            _ => panic!("expected AgentSession"),
        }
        assert!(app.input_buffer.is_empty());
    }

    #[test]
    fn alt_enter_inserts_newline_without_sending() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        app.handle(press(KeyCode::Char('i')));
        app.handle(AppEvent::Input(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        )));
        app.handle(AppEvent::Input(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::ALT,
        )));
        app.handle(AppEvent::Input(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::NONE,
        )));
        assert_eq!(app.input_buffer, "a\nb");
        match &app.screen {
            Screen::AgentSession { input_mode, .. } => assert!(*input_mode),
            _ => panic!("expected AgentSession"),
        }
    }

    #[test]
    fn shift_enter_inserts_newline_without_sending() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        app.handle(press(KeyCode::Char('i')));
        app.handle(AppEvent::Input(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::SHIFT,
        )));
        assert_eq!(app.input_buffer, "\n");
    }

    #[test]
    fn plain_enter_sends_and_exits_input_mode() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        app.handle(press(KeyCode::Char('i')));
        app.handle(AppEvent::Input(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )));
        app.handle(press(KeyCode::Enter));
        assert!(app.input_buffer.is_empty());
        match &app.screen {
            Screen::AgentSession { input_mode, .. } => assert!(!*input_mode),
            _ => panic!("expected AgentSession"),
        }
    }

    #[test]
    fn agent_connected_sets_idle_status() {
        let mut app = app_with_agents();
        app.handle(AppEvent::Session(SessionEvent::Connected {
            agent_id: "a1".into(),
            session_id: Some("sess_test".into()),
        }));
        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(a1.status, AgentStatus::Idle);
        match a1.history.last().unwrap() {
            HistoryEntry::Info(s) => assert!(s.contains("connected")),
            other => panic!("expected info entry, got {other:?}"),
        }
    }

    #[test]
    fn agent_exited_clears_prompt_tx() {
        let mut app = app_with_agents();
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        app.agents[0].prompt_tx = Some(tx);
        app.handle(AppEvent::Session(SessionEvent::Exited {
            agent_id: "a1".into(),
            code: Some(0),
        }));
        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(a1.status, AgentStatus::Stopped);
        assert!(a1.prompt_tx.is_none());
    }

    #[tokio::test]
    async fn tool_call_started_appends_handle() {
        use tokio::sync::watch;
        let mut app = app_with_agents();
        let (title_tx, title_rx) = watch::channel("read_file".to_string());
        let (status_tx, status_rx) = watch::channel(ToolCallStatusKind::InProgress);
        let call = fleet_commander_core::session::ToolCall {
            id: "call_1".into(),
            title: title_rx,
            status: status_rx,
        };
        app.handle(AppEvent::Session(SessionEvent::ToolCall {
            agent_id: "a1".into(),
            call,
        }));

        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(a1.history.len(), 1);
        match &a1.history[0] {
            HistoryEntry::Tool(tc) => {
                assert_eq!(tc.id, "call_1");
                assert_eq!(*tc.title.borrow(), "read_file");
                assert_eq!(*tc.status.borrow(), ToolCallStatusKind::InProgress);
            }
            _ => panic!("expected tool entry"),
        }

        // Title rewrites + status flips reflect through the handle without
        // any extra history mutation.
        let _ = title_tx.send("read_file completed".to_string());
        let _ = status_tx.send(ToolCallStatusKind::Completed);

        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(a1.history.len(), 1);
        match &a1.history[0] {
            HistoryEntry::Tool(tc) => {
                assert_eq!(*tc.title.borrow(), "read_file completed");
                assert_eq!(*tc.status.borrow(), ToolCallStatusKind::Completed);
            }
            _ => panic!("expected tool entry"),
        }
    }

    // ─── scrolling ────────────────────────────────────────────────────────

    /// Drive the app into a session screen with a known scroll offset so we
    /// can verify how events / key actions mutate it.
    fn app_in_session(scroll: usize) -> App {
        let mut app = app_with_agents();
        app.screen = Screen::AgentSession {
            agent_id: "a1".into(),
            focus: super::SessionFocus::Conversation,
            side_pane: None,
            scroll,
            input_mode: false,
        };
        app
    }

    fn current_scroll(app: &App) -> usize {
        match &app.screen {
            Screen::AgentSession { scroll, .. } => *scroll,
            _ => panic!("expected AgentSession"),
        }
    }

    #[test]
    fn incoming_event_preserves_manual_scroll() {
        // New "sticky scroll" behaviour: an incoming event must NOT yank the
        // viewport back to the bottom. The user has scrolled to line 5;
        // they stay there until they explicitly press `G`.
        let mut app = app_in_session(5);
        app.handle(AppEvent::Session(SessionEvent::Output {
            agent_id: "a1".into(),
            line: "new line".into(),
        }));
        assert_eq!(
            current_scroll(&app),
            5,
            "manual scroll position must persist across incoming events"
        );
    }

    #[test]
    fn incoming_event_preserves_follow_bottom_sentinel() {
        // Conversely, if the user is already following the bottom
        // (scroll == usize::MAX), the sentinel is preserved and the
        // renderer will naturally show the newest content.
        let mut app = app_in_session(usize::MAX);
        app.handle(AppEvent::Session(SessionEvent::Output {
            agent_id: "a1".into(),
            line: "new line".into(),
        }));
        assert_eq!(current_scroll(&app), usize::MAX);
    }

    #[test]
    fn incoming_event_for_other_agent_does_not_change_scroll() {
        let mut app = app_in_session(5);
        // Viewing a1, event arrives for a2.
        app.handle(AppEvent::Session(SessionEvent::Output {
            agent_id: "a2".into(),
            line: "new line".into(),
        }));
        assert_eq!(
            current_scroll(&app),
            5,
            "scroll for a1 must not move when a2 receives content"
        );
    }

    #[test]
    fn repaint_event_preserves_scroll() {
        // Repaint events exist to wake the event loop when a tracked
        // handle ticks; they must not disturb the user's scroll position.
        let mut app = app_in_session(3);
        app.handle(AppEvent::Repaint);
        assert_eq!(current_scroll(&app), 3);
    }

    #[test]
    fn down_action_increments_scroll() {
        let mut app = app_in_session(0);
        app.handle(press(KeyCode::Char('j')));
        assert_eq!(current_scroll(&app), 1);
        app.handle(press(KeyCode::Char('j')));
        assert_eq!(current_scroll(&app), 2);
    }

    #[test]
    fn up_action_saturates_at_zero() {
        let mut app = app_in_session(1);
        app.handle(press(KeyCode::Char('k')));
        assert_eq!(current_scroll(&app), 0);
        app.handle(press(KeyCode::Char('k')));
        assert_eq!(
            current_scroll(&app),
            0,
            "scrolling up past 0 must saturate, not underflow"
        );
    }

    #[test]
    fn manual_scroll_persists_across_events() {
        // After the sticky-scroll refactor, once the user has scrolled
        // away (`scroll` is finite), no streaming event should move them.
        let mut app = app_in_session(usize::MAX);
        // Seed last_effective_top so the Up handler has a known anchor.
        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        a1.last_effective_top.set(7);
        app.handle(press(KeyCode::Char('k'))); // scroll up
        assert_eq!(
            current_scroll(&app),
            6,
            "Up from follow-bottom must anchor at last_effective_top - 1"
        );
        app.handle(AppEvent::Session(SessionEvent::Output {
            agent_id: "a1".into(),
            line: "interrupt".into(),
        }));
        assert_eq!(
            current_scroll(&app),
            6,
            "incoming event must not disturb manual scroll"
        );
    }

    #[test]
    fn follow_bottom_action_re_engages_follow() {
        // After the user has scrolled away, pressing `G` (Shift-G) re-engages
        // follow-bottom by resetting scroll to the sentinel.
        let mut app = app_in_session(5);
        app.handle(AppEvent::Input(KeyEvent::new(
            KeyCode::Char('G'),
            KeyModifiers::SHIFT,
        )));
        assert_eq!(current_scroll(&app), usize::MAX);
    }

    // ─── file explorer ────────────────────────────────────────────────────

    fn ctrl_e() -> AppEvent {
        AppEvent::Input(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL))
    }

    #[test]
    fn ctrl_e_toggles_the_explorer_pane() {
        let mut app = app_in_session(0);
        assert!(!app.explorer.open);
        app.handle(ctrl_e());
        assert!(app.explorer.open, "Ctrl+E must open the explorer");
        // Focus follows the new pane so arrows immediately navigate it.
        match &app.screen {
            Screen::AgentSession { focus, .. } => {
                assert_eq!(*focus, SessionFocus::Explorer);
            }
            _ => panic!("expected AgentSession"),
        }
        app.handle(ctrl_e());
        assert!(!app.explorer.open, "Ctrl+E must close the explorer");
    }

    #[test]
    fn esc_unfocuses_explorer_without_closing_it() {
        let mut app = app_in_session(0);
        app.handle(ctrl_e());
        app.handle(press(KeyCode::Esc));
        match &app.screen {
            Screen::AgentSession { focus, .. } => {
                assert_eq!(*focus, SessionFocus::Conversation);
            }
            _ => panic!("expected AgentSession"),
        }
        assert!(
            app.explorer.open,
            "Esc from explorer focus must keep the pane open"
        );
    }

    #[test]
    fn dot_toggles_show_ignored_when_explorer_focused() {
        let mut app = app_in_session(0);
        app.handle(ctrl_e());
        assert!(!app.explorer.show_ignored);
        app.handle(press(KeyCode::Char('.')));
        assert!(app.explorer.show_ignored, ". must toggle show_ignored on");
        app.handle(press(KeyCode::Char('.')));
        assert!(!app.explorer.show_ignored);
    }

    // ─── session rehydration ──────────────────────────────────────────────
    //
    // During session/load the agent replays prior turns as a sequence of
    // SessionEvent::UserMessage and SessionEvent::AssistantMessage events
    // (with handles whose status quickly transitions to Completed). The
    // app must:
    //   - append each entry to history in arrival order;
    //   - auto-follow to the bottom after each event so the most recent
    //     turn is the one the user sees.

    fn replayed_assistant(body: &str) -> fleet_commander_core::session::AssistantMessage {
        use tokio::sync::watch;
        let (text_tx, text_rx) = watch::channel(body.to_string());
        let (status_tx, status_rx) =
            watch::channel(fleet_commander_core::session::MessageStatus::Completed);
        // Senders are dropped after the channels are seeded, which is fine
        // for replayed (terminal) entries — the receiver still yields the
        // last value via `borrow()`.
        let _ = (text_tx, status_tx);
        fleet_commander_core::session::AssistantMessage {
            text: text_rx,
            status: status_rx,
        }
    }

    fn replayed_user(body: &str) -> fleet_commander_core::session::UserMessage {
        use tokio::sync::watch;
        let (text_tx, text_rx) = watch::channel(body.to_string());
        let (status_tx, status_rx) =
            watch::channel(fleet_commander_core::session::MessageStatus::Completed);
        let _ = (text_tx, status_tx);
        fleet_commander_core::session::UserMessage {
            text: text_rx,
            status: status_rx,
        }
    }

    #[tokio::test]
    async fn session_rehydration_appends_history_in_order_and_follows_bottom() {
        // Start in follow-bottom mode (the default on session entry) so
        // we can verify the sentinel persists across rehydration.
        let mut app = app_in_session(usize::MAX);

        // Simulate session/load replay: a few prior turns.
        let turns = [
            ("first question", "first answer"),
            ("second question", "second answer"),
            ("third question", "third answer"),
        ];
        for (q, a) in turns.iter() {
            app.handle(AppEvent::Session(SessionEvent::UserMessage {
                agent_id: "a1".into(),
                message: replayed_user(q),
            }));
            app.handle(AppEvent::Session(SessionEvent::AssistantMessage {
                agent_id: "a1".into(),
                message: replayed_assistant(a),
            }));
        }

        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(
            a1.history.len(),
            6,
            "all 6 replayed entries (3 turns) must be in history"
        );

        // Verify order: User, Assistant, User, Assistant, User, Assistant.
        let mut iter = a1.history.iter();
        for (q, a) in turns.iter() {
            match iter.next().unwrap() {
                HistoryEntry::User(u) => assert_eq!(u.text.borrow().as_str(), *q),
                other => panic!("expected User, got {other:?}"),
            }
            match iter.next().unwrap() {
                HistoryEntry::Assistant(m) => assert_eq!(m.text.borrow().as_str(), *a),
                other => panic!("expected Assistant, got {other:?}"),
            }
        }

        // Sticky scroll: rehydration events do not move the scroll.
        // The session starts in follow-bottom mode (usize::MAX) and stays
        // there because no manual scroll was performed; the renderer will
        // naturally show the bottom (i.e. the latest turn) — see the
        // `rehydration_renders_latest_turn_visible` UI test for the
        // visible end-to-end behaviour.
        assert_eq!(
            current_scroll(&app),
            usize::MAX,
            "scroll sentinel must be preserved when no user input intervenes"
        );
    }

    #[tokio::test]
    async fn session_rehydration_for_inactive_agent_does_not_move_scroll() {
        let mut app = app_in_session(7);
        // App is viewing a1; rehydration arrives for a2.
        app.handle(AppEvent::Session(SessionEvent::UserMessage {
            agent_id: "a2".into(),
            message: replayed_user("not me"),
        }));
        app.handle(AppEvent::Session(SessionEvent::AssistantMessage {
            agent_id: "a2".into(),
            message: replayed_assistant("nor me"),
        }));
        assert_eq!(
            current_scroll(&app),
            7,
            "scroll for a1 must not move when a2 rehydrates"
        );
        // But a2's history must have grown.
        let a2 = app.agents.iter().find(|a| a.id == "a2").unwrap();
        assert_eq!(a2.history.len(), 2);
    }
}
