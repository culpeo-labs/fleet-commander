//! Agent/session lifecycle for [`super::App`]: connecting the ACP agent,
//! draining session events into history, upgrading the explorer to the
//! in-container `ServiceFs`, and per-agent branch/scroll bookkeeping.

use std::path::PathBuf;
use std::sync::Arc;

use tracing::{info, warn};

use fleet_commander_core::agent_runtime;
use fleet_commander_core::service_fs::ServiceFs;
use fleet_commander_core::session::SessionEvent;
use fleet_commander_core::workspace_fs::{LocalFs, WorkspaceFs};

use crate::agent::{AgentId, AgentStatus, ContainerInfo, HistoryEntry};
use crate::event::AppEvent;
use crate::workspace;

use super::{App, PendingPermission, Screen, SessionFocus, SidePane};
use super::{spawn_text_tracker, spawn_tool_tracker};

impl App {
    /// Connect to the in-container `fleet-agent` on a background thread and,
    /// once the (blocking) handshake completes, hand the resulting
    /// [`ServiceFs`] back to the event loop via [`AppEvent::ExplorerFsReady`].
    ///
    /// On failure (no binary mounted, container gone, …) the explorer simply
    /// stays on the host-side [`LocalFs`].
    pub(super) fn request_service_fs_upgrade(
        &self,
        agent_id: AgentId,
        info: ContainerInfo,
        workspace: PathBuf,
    ) {
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let container_id = info.container_id.clone();
            // Route live `fs.didChange` pushes back into the event loop so the
            // explorer refreshes itself when files change inside the container.
            // The sink runs on the transport's reader thread, so it only does
            // a cheap non-blocking channel send.
            let sink: fleet_commander_core::service_fs::NotificationSink = {
                use fleet_commander_core::fleet_protocol::{
                    Notification, SearchDoneParams, SearchResultParams, methods,
                };
                let tx = tx.clone();
                let agent_id = agent_id.clone();
                let container_id = container_id.clone();
                Box::new(move |note| {
                    let Notification { method, params, .. } = note;
                    match method.as_str() {
                        m if m == methods::FS_DID_CHANGE => {
                            let _ = tx.send(AppEvent::ExplorerFsChanged {
                                agent_id: agent_id.clone(),
                                container_id: container_id.clone(),
                            });
                        }
                        m if m == methods::FS_SEARCH_RESULT => {
                            if let Some(params) = params
                                .and_then(|p| serde_json::from_value::<SearchResultParams>(p).ok())
                            {
                                let _ = tx.send(AppEvent::SearchResults {
                                    agent_id: agent_id.clone(),
                                    search_id: params.search_id,
                                    matches: params.matches,
                                });
                            }
                        }
                        m if m == methods::FS_SEARCH_DONE => {
                            if let Some(params) = params
                                .and_then(|p| serde_json::from_value::<SearchDoneParams>(p).ok())
                            {
                                let _ = tx.send(AppEvent::SearchDone {
                                    agent_id: agent_id.clone(),
                                    search_id: params.search_id,
                                    summary: params.summary,
                                });
                            }
                        }
                        _ => {}
                    }
                })
            };
            match ServiceFs::connect_docker_watched(
                workspace,
                &info.remote_workspace_folder,
                &info.container_id,
                &info.remote_user,
                fleet_commander_core::agent_bin::CONTAINER_AGENT_PATH,
                Some(sink),
            ) {
                Ok(fs) => {
                    let _ = tx.send(AppEvent::ExplorerFsReady {
                        agent_id,
                        container_id,
                        fs: Arc::new(fs) as Arc<dyn WorkspaceFs>,
                    });
                }
                Err(e) => {
                    info!(error = %e, "Container service unavailable; explorer stays on host filesystem");
                }
            }
        });
    }
    /// Read the agent's git branch from inside its container on a background
    /// thread (a one-shot `docker exec` via the same `fleet-agent` service the
    /// explorer uses) and deliver it as [`AppEvent::AgentBranchReady`].
    ///
    /// No-op if the agent has no started container — we deliberately never read
    /// the host bind-mount, so the header/list branch and the explorer's git
    /// status always reflect the same (container) filesystem.
    pub(super) fn refresh_agent_branch(&self, agent_id: AgentId) {
        let Some((info, workspace)) = self.agents.iter().find(|a| a.id == agent_id).and_then(|a| {
            let info = a.container.clone()?;
            let ws = a.workspace_folder.clone()?;
            Some((info, ws))
        }) else {
            return;
        };
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let container_id = info.container_id.clone();
            let branch = match ServiceFs::connect_docker(
                workspace,
                &info.remote_workspace_folder,
                &info.container_id,
                &info.remote_user,
                fleet_commander_core::agent_bin::CONTAINER_AGENT_PATH,
            ) {
                Ok(fs) => fs.git_branch(),
                Err(e) => {
                    info!(error = %e, "Branch fetch failed; container service unavailable");
                    return;
                }
            };
            let _ = tx.send(AppEvent::AgentBranchReady {
                agent_id,
                container_id,
                branch,
            });
        });
    }
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
                // Fetch the branch from inside the new container (covers both
                // the viewed agent's header and any other agent's list row).
                self.refresh_agent_branch(agent_id.clone());
                // If this agent's explorer is on screen, upgrade it from the
                // host filesystem to the in-container service.
                if self.viewed_agent_id().as_ref() == Some(&agent_id)
                    && let Some(ws) = self
                        .agents
                        .iter()
                        .find(|a| a.id == agent_id)
                        .and_then(|a| a.workspace_folder.clone())
                {
                    self.request_service_fs_upgrade(agent_id, info, ws);
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
        }
    }
    /// Start the ACP connection for an agent if not already connected.
    pub fn ensure_agent_connected(&mut self, agent_id: AgentId) {
        // Point the explorer at this agent's workspace (no-op if same root).
        // This happens on every screen change into AgentSession, including
        // when the agent is already connected, so the explorer always reflects
        // the currently-viewed agent.
        if let Some(agent) = self.agents.iter().find(|a| a.id == agent_id) {
            let ws = agent.workspace_folder.clone();
            let container = agent.container.clone();
            let local = ws
                .as_ref()
                .map(|w| Arc::new(LocalFs::new(w)) as Arc<dyn WorkspaceFs>);
            // If the explorer already shows a container-backed fs for this
            // same root, don't downgrade it to LocalFs (and don't re-spawn
            // the upgrade) on a repeat entry into the session screen.
            let already_remote = match (&self.explorer.fs, &local) {
                (Some(cur), Some(l)) => cur.is_remote() && cur.root_display() == l.root_display(),
                _ => false,
            };
            if !already_remote {
                let had_fs = self.explorer.fs.is_some();
                self.explorer.set_fs(local);
                // Refresh status when the workspace is set for the first time
                // (or when switching to a new agent's workspace cleared state).
                if self.explorer.fs.is_some() && (!had_fs || self.explorer.status.is_empty()) {
                    self.request_explorer_refresh();
                }
                // Upgrade to the in-container service if the container is up.
                if let (Some(info), Some(w)) = (container, ws) {
                    self.request_service_fs_upgrade(agent_id.clone(), info, w);
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
