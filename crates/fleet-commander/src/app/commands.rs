//! `:` command execution and workspace lifecycle for [`super::App`]:
//! opening/closing a workspace agent and rebuilding its container.

use std::path::PathBuf;
use std::sync::Arc;

use tracing::{info, warn};

use fleet_commander_core::container;
use fleet_commander_core::session::SessionEvent;
use fleet_commander_core::workspace_fs::{LocalFs, WorkspaceFs};

use crate::agent::{Agent, AgentStatus};
use crate::agent_kind::AgentKind;
use crate::event::AppEvent;
use crate::{init, workspace};

use super::{App, Screen, SessionFocus, SidePane};

impl App {
    /// Parse and execute a `:` command.
    pub(super) fn execute_command(&mut self, cmd: &str) {
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
            "commands" | "cmds" => {
                self.open_commands_view();
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

    /// Open the slash-commands browser in the right side pane.
    ///
    /// Snapshots the current agent's `available_commands`. If the agent
    /// later updates them via ACP, the user can reopen with `:commands`
    /// to refresh.
    pub(super) fn open_commands_view(&mut self) {
        let agent_id = match &self.screen {
            Screen::AgentSession { agent_id, .. } => agent_id.clone(),
            _ => {
                self.status_message =
                    Some("No workspace open — :commands needs an agent session".into());
                return;
            }
        };
        let commands = match self.agents.iter().find(|a| a.id == agent_id) {
            Some(a) if !a.available_commands.is_empty() => a.available_commands.clone(),
            Some(_) => {
                self.status_message =
                    Some("Agent has not advertised any slash commands yet".into());
                return;
            }
            None => return,
        };
        if let Screen::AgentSession { side_pane, .. } = &mut self.screen {
            *side_pane = Some(SidePane::Commands {
                commands,
                scroll: 0,
            });
        }
    }

    /// Remove the currently viewed workspace agent and go back to the list.
    pub(super) fn close_current_workspace(&mut self) {
        let agent_id = match &self.screen {
            Screen::AgentSession { agent_id, .. } => agent_id.clone(),
            _ => {
                self.status_message = Some("No workspace open — use :close from a session".into());
                return;
            }
        };

        self.agents.retain(|a| a.id != agent_id);

        // Drop any container-backed explorer fs so its `docker exec` child is
        // torn down rather than leaked once we leave the session screen.
        if self
            .explorer
            .fs
            .as_ref()
            .map(|f| f.is_remote())
            .unwrap_or(false)
        {
            self.explorer.set_fs(None);
        }

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
    pub(super) fn rebuild_current_container(&mut self) {
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
        // The container is about to be removed, so the in-container service
        // (and any ServiceFs bound to it) is no longer valid. Clearing this
        // also makes a stale in-flight `ExplorerFsReady` install fail the
        // container-id check once the new container comes up.
        agent.container = None;
        // The branch came from that container; drop it until the rebuilt
        // container reports a fresh one (no host fallback).
        agent.git_branch = None;
        agent.info("🔄 Rebuilding container...");

        // Downgrade the explorer off the soon-to-be-dead container's service
        // back to the host filesystem (dropping the old `docker exec` child),
        // if this agent's explorer is the one on screen.
        if self.viewed_agent_id().as_ref() == Some(&agent_id)
            && self
                .explorer
                .fs
                .as_ref()
                .map(|f| f.is_remote())
                .unwrap_or(false)
        {
            let local = Some(Arc::new(LocalFs::new(&workspace)) as Arc<dyn WorkspaceFs>);
            self.explorer.set_fs(local);
        }

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
