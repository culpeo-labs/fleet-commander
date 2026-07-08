//! Agent/session lifecycle for [`super::App`]: connecting the ACP agent,
//! draining session events into history, upgrading the explorer to the
//! in-container `ServiceFs`, and per-agent branch/scroll bookkeeping.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rmcp::ServiceExt;
use tokio::io::DuplexStream;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use fleet_commander_core::agent_runtime;
use fleet_commander_core::session::SessionEvent;
use fleet_commander_core::workspace_fs::{LocalFs, WorkspaceFs};

use crate::agent::{AgentId, AgentStatus, ContainerInfo, HistoryEntry};
use crate::event::AppEvent;
use crate::mcp_server::TuiMcpServer;
use crate::workspace;

use super::{App, PendingPermission, Screen, SessionFocus, SidePane};
use super::{spawn_text_tracker, spawn_tool_tracker};

impl App {
    pub(super) fn handle_session_event(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::Output { agent_id, line } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.info(line);
                }
            }
            SessionEvent::ContainerReady {
                agent_id,
                container_id,
                remote_user,
                remote_workspace_folder,
            } => {
                let info = ContainerInfo {
                    container_id,
                    workspace_folder: PathBuf::new(),
                    remote_workspace_folder,
                    remote_user,
                };
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.container = Some(info.clone());
                }
                // The explorer's `ServiceFs` and the agent's git branch are now
                // delivered by the daemon-owned session driver over the shared
                // bridge (see `SessionEvent::ExplorerFs`/`AgentBranch`), so
                // there is nothing to fetch here — we just record the container.
            }
            SessionEvent::Exited { agent_id, .. } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.status = AgentStatus::Stopped;
                    agent.prompt_tx = None;
                    agent.task_handle = None;
                    // Drop the shared-bridge fs so the underlying `docker exec`
                    // is torn down once the session driver also releases it
                    // (`ServiceFs` holds the last `Arc` to the transport, whose
                    // `Drop` kills the child). Without this the exec would leak.
                    agent.explorer_fs = None;
                }
                // If the exited agent's explorer is on screen, downgrade it back
                // to the host filesystem so the last remote `Arc` is released.
                if self.viewed_agent_id().as_ref() == Some(&agent_id) {
                    let local = self
                        .agents
                        .iter()
                        .find(|a| a.id == agent_id)
                        .and_then(|a| a.workspace_folder.clone())
                        .map(|w| Arc::new(LocalFs::new(w)) as Arc<dyn WorkspaceFs>);
                    self.explorer.set_fs(local);
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
                    selected: 0,
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
            SessionEvent::AvailableCommands { agent_id, commands } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.available_commands = commands;
                }
            }
            // The following four events arrive from the daemon-owned session
            // driver over the shared bridge (Phase 4b2 y3). Store per-agent
            // state where relevant, then re-emit the existing `AppEvent`s so the
            // viewed/container/root guards in `app/mod.rs` are reused verbatim.
            SessionEvent::ExplorerFs {
                agent_id,
                container_id,
                fs,
            } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.explorer_fs = Some(fs.clone());
                }
                let _ = self.tx.send(AppEvent::ExplorerFsReady {
                    agent_id,
                    container_id,
                    fs,
                });
            }
            SessionEvent::ExplorerFsChanged {
                agent_id,
                container_id,
            } => {
                let _ = self.tx.send(AppEvent::ExplorerFsChanged {
                    agent_id,
                    container_id,
                });
            }
            SessionEvent::SearchResults {
                agent_id,
                search_id,
                matches,
            } => {
                let _ = self.tx.send(AppEvent::SearchResults {
                    agent_id,
                    search_id,
                    matches,
                });
            }
            SessionEvent::SearchDone {
                agent_id,
                search_id,
                summary,
            } => {
                let _ = self.tx.send(AppEvent::SearchDone {
                    agent_id,
                    search_id,
                    summary,
                });
            }
            SessionEvent::AgentBranch {
                agent_id,
                container_id,
                branch,
            } => {
                let _ = self.tx.send(AppEvent::AgentBranchReady {
                    agent_id,
                    container_id,
                    branch,
                });
            }
            SessionEvent::McpTunnelOpen {
                agent_id,
                tunnel_id,
                stream,
            } => self.serve_mcp_tunnel(agent_id, tunnel_id, stream),
            SessionEvent::McpTunnelClose { tunnel_id, .. } => {
                if let Some(ct) = self.mcp_tunnels.remove(&tunnel_id) {
                    ct.cancel();
                }
            }
        }
    }

    /// Serve a [`TuiMcpServer`] over a freshly-opened cross-workspace MCP tunnel
    /// (Feature 2). The daemon bridged the in-container agent's MCP client to
    /// `stream` (a duplex over the session connection); we run an MCP server on
    /// it so the agent can call the TUI's tools. The task is cancelled when the
    /// tunnel closes (see [`SessionEvent::McpTunnelClose`]).
    fn serve_mcp_tunnel(
        &mut self,
        agent_id: AgentId,
        tunnel_id: u64,
        stream: Arc<Mutex<Option<DuplexStream>>>,
    ) {
        let Some(stream) = stream.lock().ok().and_then(|mut guard| guard.take()) else {
            warn!(tunnel_id, "MCP tunnel opened without a stream");
            return;
        };
        let ct = CancellationToken::new();
        self.mcp_tunnels.insert(tunnel_id, ct.clone());
        let tx = self.tx.clone();
        let pairings = self.pairings.clone();
        tokio::spawn(async move {
            let server = TuiMcpServer::for_tunnel(tx, agent_id, pairings);
            match server.serve_with_ct(stream, ct).await {
                Ok(service) => {
                    let _ = service.waiting().await;
                }
                Err(e) => {
                    warn!(tunnel_id, error = %e, "MCP tunnel server failed to start");
                }
            }
        });
    }
    /// Start the ACP connection for an agent if not already connected.
    pub fn ensure_agent_connected(&mut self, agent_id: AgentId) {
        // Point the explorer at this agent's workspace (no-op if same root).
        // This happens on every screen change into AgentSession, including
        // when the agent is already connected, so the explorer always reflects
        // the currently-viewed agent.
        if let Some(agent) = self.agents.iter().find(|a| a.id == agent_id) {
            let ws = agent.workspace_folder.clone();
            // The container-backed fs delivered over the shared bridge, if the
            // session driver has already handed one to us. Re-installed on
            // re-entry instead of opening a fresh `docker exec`.
            let stored_fs = agent.explorer_fs.clone();
            let local = ws
                .as_ref()
                .map(|w| Arc::new(LocalFs::new(w)) as Arc<dyn WorkspaceFs>);
            // If the explorer already shows a container-backed fs for this
            // same root, don't downgrade it to LocalFs (and don't re-install)
            // on a repeat entry into the session screen.
            let already_remote = match (&self.explorer.fs, &local) {
                (Some(cur), Some(l)) => cur.is_remote() && cur.root_display() == l.root_display(),
                _ => false,
            };
            if !already_remote {
                let had_fs = self.explorer.fs.is_some();
                // Prefer the stored remote fs; fall back to the host `LocalFs`
                // until the session driver delivers one (via `ExplorerFs`).
                match stored_fs {
                    Some(fs) => {
                        self.explorer.set_fs(Some(fs));
                        self.request_explorer_refresh();
                    }
                    None => {
                        self.explorer.set_fs(local);
                        // Refresh status when the workspace is set for the first
                        // time (or when switching to a new agent's workspace
                        // cleared state).
                        if self.explorer.fs.is_some()
                            && (!had_fs || self.explorer.status.is_empty())
                        {
                            self.request_explorer_refresh();
                        }
                    }
                }
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
    pub(super) fn auto_scroll_for(&mut self, agent_id: &str) {
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
    /// Open or replace the side pane when an MCP tool targets a specific agent.
    /// If that agent's session is currently visible, the pane updates immediately.
    /// If the agent list is showing, we navigate into the agent's session.
    pub(super) fn handle_mcp_side_pane(&mut self, agent_id: AgentId, pane: SidePane) {
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
}
