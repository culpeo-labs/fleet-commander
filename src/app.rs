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

use crossterm::event::{KeyCode, KeyEvent};
use std::path::PathBuf;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::agent::{Agent, AgentId, AgentStatus};
use crate::agent_kind::AgentKind;
use crate::agent_runtime;
use crate::change_source::ChangeEvent;
use crate::completion::{PathCompleter, split_command_and_path};
use crate::config::{Action, Config};
use crate::container;
use crate::event::AppEvent;
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
}

#[derive(Debug, Clone)]
pub enum SidePane {
    Diff {
        path: PathBuf,
        content: String,
    },
    #[allow(dead_code)] // Editor variant is a stub for now.
    Editor {
        path: PathBuf,
        buffer: String,
    },
}

impl SidePane {
    #[allow(dead_code)] // exposed for future actions on the side pane.
    pub fn path(&self) -> &PathBuf {
        match self {
            SidePane::Diff { path, .. } | SidePane::Editor { path, .. } => path,
        }
    }
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
    /// Set when an agent needs interactive auth — the main loop suspends the
    /// TUI and runs this command with inherited stdio.
    pub auth_pending: Option<(AgentId, Vec<String>)>,
    /// Pending tool permission request awaiting user response.
    /// Contains (tool_name, options: Vec<(id, label, kind)>, reply_channel).
    pub permission_pending: Option<PendingPermission>,
}

/// A tool permission request waiting for the user's y/n decision.
#[allow(dead_code)]
pub struct PendingPermission {
    pub agent_id: AgentId,
    pub tool_name: String,
    pub options: Vec<(String, String, String)>,
    pub reply: crate::event::PermissionReply,
}

impl App {
    pub fn new(config: Config, agents: Vec<Agent>, tx: mpsc::UnboundedSender<AppEvent>) -> Self {
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
            auth_pending: None,
            permission_pending: None,
        }
    }

    pub fn handle(&mut self, event: AppEvent) {
        match event {
            AppEvent::Input(key) => self.handle_key(key),
            AppEvent::Change(change) => self.handle_change(change),
            AppEvent::AgentOutput { agent_id, line } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.history.push(line);
                }
                self.auto_scroll_for(&agent_id);
            }
            AppEvent::AgentExited { agent_id, .. } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.status = AgentStatus::Stopped;
                    agent.prompt_tx = None;
                    agent.task_handle = None;
                }
            }
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
                    agent.history.push(message);
                }
            }
            AppEvent::AssistantDelta { agent_id, text } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    // A new assistant chunk closes any pending replayed user
                    // message and the prior thought stream.
                    flush_pending_user_message(agent);
                    flush_pending_thought(agent);
                    agent.status = AgentStatus::Running;
                    agent.pending_response.push_str(&text);
                }
                self.auto_scroll_for(&agent_id);
            }
            AppEvent::ThoughtDelta { agent_id, text } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    flush_pending_user_message(agent);
                    agent.pending_thought.push_str(&text);
                }
                self.auto_scroll_for(&agent_id);
            }
            AppEvent::UserMessageDelta { agent_id, text } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    // A new user message marks the end of the prior turn —
                    // flush so the assistant response (if any) gets pushed to
                    // history and rendered as markdown.
                    flush_pending_thought(agent);
                    flush_pending_response(agent);
                    agent.pending_user_message.push_str(&text);
                }
                self.auto_scroll_for(&agent_id);
            }
            AppEvent::AssistantDone { agent_id } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    flush_pending_user_message(agent);
                    flush_pending_thought(agent);
                    flush_pending_response(agent);
                    agent.status = AgentStatus::Idle;
                }
                self.auto_scroll_for(&agent_id);
            }
            AppEvent::SessionError { agent_id, message } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.history.push(format!("[error] {message}"));
                    agent.status = AgentStatus::Error;
                }
            }
            AppEvent::ToolCallUpdate {
                agent_id,
                tool_name,
                status,
            } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    let label = if tool_name.is_empty() {
                        format!("[tool] {status}")
                    } else {
                        format!("[tool: {tool_name}] {status}")
                    };
                    agent.history.push(label);
                }
            }
            AppEvent::AgentConnected { agent_id, session_id } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.status = AgentStatus::Idle;
                    agent.session_id = session_id.clone();
                    agent.history.push("ACP session connected.".into());
                    // Persist session_id to per-workspace data dir.
                    if let Some(ws) = &agent.workspace_folder {
                        let state = workspace::WorkspaceState { session_id };
                        if let Err(e) = workspace::save_state(ws, &state) {
                            warn!(error = %e, "Failed to save workspace state");
                        }
                    }
                }
            }
            AppEvent::AuthRequired { agent_id, command } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.history.push("🔑 Authentication required — launching login flow...".into());
                    agent.status = AgentStatus::Stopped;
                    agent.prompt_tx = None;
                }
                self.auth_pending = Some((agent_id, command));
            }
            AppEvent::ReconnectAgent { agent_id } => {
                info!(agent_id = %agent_id, "Reconnecting agent after rebuild");
                self.ensure_agent_connected(agent_id);
            }
            AppEvent::PermissionRequest {
                agent_id,
                tool_name,
                options,
                reply,
            } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.history.push(format!("🔐 Permission requested: {tool_name}"));
                }
                self.permission_pending = Some(PendingPermission {
                    agent_id,
                    tool_name,
                    options,
                    reply,
                });
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
                    if let Ok(mut guard) = perm.reply.lock() {
                        if let Some(tx) = guard.take() {
                            let _ = tx.send(allow_id);
                        }
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    let perm = self.permission_pending.take().unwrap();
                    if let Ok(mut guard) = perm.reply.lock() {
                        if let Some(tx) = guard.take() {
                            let _ = tx.send(None);
                        }
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
                    let message = std::mem::take(&mut self.input_buffer);
                    if let Some(agent) = (!message.is_empty())
                        .then(|| self.agents.iter().find(|a| a.id == *agent_id))
                        .flatten()
                    {
                        agent_runtime::send_message(
                            agent.id.clone(),
                            agent.prompt_tx.as_ref(),
                            message,
                            self.tx.clone(),
                        );
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
            } => handle_session_action(action, agent_id, focus, side_pane, scroll, &self.agents),
        };
        if let Some(next) = next {
            self.screen = next;
            // Lazily start ACP connection when entering an agent session.
            if let Screen::AgentSession { agent_id, .. } = &self.screen {
                self.ensure_agent_connected(agent_id.clone());
            }
        }
    }

    /// Start the ACP connection for an agent if not already connected.
    pub fn ensure_agent_connected(&mut self, agent_id: AgentId) {
        let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) else {
            return;
        };
        if agent.prompt_tx.is_some() || agent.task_handle.is_some() {
            return; // Already connected or connecting.
        }
        if agent.acp_command.is_empty() {
            return; // No command configured.
        }
        let (prompt_tx, abort_handle) = agent_runtime::start_agent(
            agent.id.clone(),
            agent.effective_acp_command(),
            agent.workspace_folder.clone(),
            agent.session_id.clone(),
            self.tx.clone(),
        );
        agent.prompt_tx = Some(prompt_tx);
        agent.task_handle = Some(abort_handle);
        agent.status = AgentStatus::Running;
        let label = match &agent.workspace_folder {
            Some(ws) => format!("Starting container ({})...", ws.display()),
            None => "Connecting...".into(),
        };
        agent.history.push(label);
    }

    /// Scroll to the bottom when content arrives for the currently viewed agent.
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
            // Sentinel: usize::MAX means "follow bottom". The render function
            // computes the actual offset based on viewport height.
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
                self.status_message = Some("No workspace open — use :rebuild from a session".into());
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
                self.status_message = Some("Agent has no workspace — :rebuild needs a container agent".into());
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
        agent.history.push("🔄 Rebuilding container...".into());

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
                let _ = tx.send(AppEvent::AgentOutput {
                    agent_id: aid.clone(),
                    line: format!("[warn] Failed to remove container: {err}"),
                });
            }
            let _ = tx.send(AppEvent::AgentOutput {
                agent_id: aid.clone(),
                line: "Container removed. Reconnecting...".into(),
            });
            let _ = tx.send(AppEvent::ReconnectAgent { agent_id: aid });
        });

        // Persist the cleared session_id.
        if let Err(err) = workspace::save(&workspace::from_agents(&self.agents)) {
            self.status_message = Some(format!("Warning: {err}"));
        }
    }
}

/// Flush accumulated thought text into history as a single collapsed entry.
fn flush_pending_thought(agent: &mut Agent) {
    if !agent.pending_thought.is_empty() {
        let thought = std::mem::take(&mut agent.pending_thought);
        agent.history.push(format!("[thought] {}", thought.trim()));
    }
}

/// Flush accumulated assistant response into history so it gets rendered
/// with full markdown formatting (rather than as plain streaming text).
fn flush_pending_response(agent: &mut Agent) {
    if !agent.pending_response.is_empty() {
        let response = std::mem::take(&mut agent.pending_response);
        agent.history.push(response);
    }
}

/// Flush accumulated user-message chunks (replayed during session/load) into
/// history with the `> ` prefix used for regular user messages so they get
/// the same cyan-bold styling.
fn flush_pending_user_message(agent: &mut Agent) {
    if !agent.pending_user_message.is_empty() {
        let message = std::mem::take(&mut agent.pending_user_message);
        for line in message.lines() {
            agent.history.push(format!("> {line}"));
        }
    }
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
) -> Option<Screen> {
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
        Action::TogglePane if side_pane.is_some() => {
            *focus = match *focus {
                SessionFocus::Conversation => SessionFocus::SidePane,
                SessionFocus::SidePane => SessionFocus::Conversation,
            };
            None
        }
        Action::Down => {
            *scroll = scroll.saturating_add(1);
            None
        }
        Action::Up => {
            *scroll = scroll.saturating_sub(1);
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
        app.handle(AppEvent::AgentOutput {
            agent_id: "a2".into(),
            line: "hello".into(),
        });
        let a2 = app.agents.iter().find(|a| a.id == "a2").unwrap();
        assert_eq!(a2.history, vec!["hello".to_string()]);
    }

    #[test]
    fn agent_exited_marks_status_stopped() {
        let mut app = app_with_agents();
        app.handle(AppEvent::AgentExited {
            agent_id: "a1".into(),
            code: Some(0),
        });
        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(a1.status, AgentStatus::Stopped);
    }

    #[test]
    fn assistant_delta_accumulates_pending_response() {
        let mut app = app_with_agents();
        app.handle(AppEvent::AssistantDelta {
            agent_id: "a1".into(),
            text: "Hello".into(),
        });
        app.handle(AppEvent::AssistantDelta {
            agent_id: "a1".into(),
            text: " world".into(),
        });
        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(a1.pending_response, "Hello world");
        assert_eq!(a1.status, AgentStatus::Running);
    }

    #[test]
    fn assistant_done_flushes_pending_to_history() {
        let mut app = app_with_agents();
        app.handle(AppEvent::AssistantDelta {
            agent_id: "a1".into(),
            text: "response text".into(),
        });
        app.handle(AppEvent::AssistantDone {
            agent_id: "a1".into(),
        });
        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert!(a1.pending_response.is_empty());
        assert_eq!(a1.history.last().unwrap(), "response text");
        assert_eq!(a1.status, AgentStatus::Idle);
    }

    #[test]
    fn session_error_appends_to_history() {
        let mut app = app_with_agents();
        app.handle(AppEvent::SessionError {
            agent_id: "a2".into(),
            message: "connection lost".into(),
        });
        let a2 = app.agents.iter().find(|a| a.id == "a2").unwrap();
        assert!(a2.history.last().unwrap().contains("connection lost"));
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
    fn agent_connected_sets_idle_status() {
        let mut app = app_with_agents();
        app.handle(AppEvent::AgentConnected {
            agent_id: "a1".into(),
            session_id: Some("sess_test".into()),
        });
        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(a1.status, AgentStatus::Idle);
        assert!(a1.history.last().unwrap().contains("connected"));
    }

    #[test]
    fn agent_exited_clears_prompt_tx() {
        let mut app = app_with_agents();
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        app.agents[0].prompt_tx = Some(tx);
        app.handle(AppEvent::AgentExited {
            agent_id: "a1".into(),
            code: Some(0),
        });
        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(a1.status, AgentStatus::Stopped);
        assert!(a1.prompt_tx.is_none());
    }

    #[test]
    fn tool_call_update_appends_to_history() {
        let mut app = app_with_agents();
        app.handle(AppEvent::ToolCallUpdate {
            agent_id: "a1".into(),
            tool_name: "read_file".into(),
            status: "started".into(),
        });
        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert!(a1.history.last().unwrap().contains("read_file"));
    }
}
