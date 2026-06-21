//! ACP (Agent Client Protocol) integration layer.
//!
//! Manages the lifecycle of ACP agent subprocesses. Each agent gets a
//! persistent connection: the process is spawned once, a session is created,
//! and a prompt channel allows the TUI to send messages without respawning.
//!
//! When an agent has a `workspace_folder`, the container is started first
//! via `devcontainer up`, and the ACP command is wrapped with
//! `devcontainer exec`.
//!
//! The implementation is split across submodules by concern:
//! - [`connection`] — process spawn, ACP handshake, and the prompt loop.
//! - [`resume`] — session rehydration via `session/resume` and `session/load`.
//! - [`updates`] — applying ACP `SessionUpdate`s to the state machine.
//! - [`auth`] — parsing and building the interactive login command.

mod auth;
mod connection;
mod resume;
mod updates;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tracing::{error, info};

use crate::container::{self, ContainerConfig};
use crate::session::{AgentId, SessionEvent};

use connection::run_persistent_connection;

/// Shared handle to the optional ACP wire-log file. Cloned cheaply across
/// agent tasks so all of them write into the same file.
pub type AcpLog = Arc<Mutex<std::fs::File>>;

/// Spawn a persistent ACP connection for an agent.
///
/// If the agent has a workspace folder, starts the dev container first,
/// then wraps the ACP command with `docker exec` to run inside it.
/// Returns an `mpsc::UnboundedSender<String>` for sending prompts.
///
/// `previous_session_id` allows resuming an existing ACP session instead
/// of creating a new one (if the agent supports it).
pub fn start_agent(
    agent_id: AgentId,
    acp_command: String,
    workspace_folder: Option<PathBuf>,
    previous_session_id: Option<String>,
    event_tx: mpsc::UnboundedSender<SessionEvent>,
    acp_log: Option<AcpLog>,
) -> (mpsc::UnboundedSender<String>, tokio::task::AbortHandle) {
    let (prompt_tx, prompt_rx) = mpsc::unbounded_channel::<String>();

    let handle = tokio::spawn(async move {
        info!(agent_id = %agent_id, command = %acp_command, workspace = ?workspace_folder, "Starting agent");
        // Resolve a host GitHub token for headless auth.
        // The copilot CLI in --acp mode expects to already be authenticated;
        // passing COPILOT_GITHUB_TOKEN lets it work without a keychain or
        // interactive login.
        let host_token = container::resolve_host_github_token();

        // If workspace_folder is set, start the dev container first.
        let (effective_command, session_cwd, container_info) =
            if let Some(ref ws) = workspace_folder {
                let config = ContainerConfig {
                    workspace_folder: ws.clone(),
                };
                let progress_tx = event_tx.clone();
                let progress_aid = agent_id.clone();
                match container::start_container(&config, |msg| {
                    let _ = progress_tx.send(SessionEvent::Output {
                        agent_id: progress_aid.clone(),
                        line: format!("  ⏳ {msg}"),
                    });
                })
                .await
                {
                    Ok(info) => {
                        info!(
                            agent_id = %agent_id,
                            container_id = %info.container_id,
                            remote_user = %info.remote_user,
                            remote_workspace = %info.remote_workspace_folder,
                            "Container ready"
                        );
                        let _ = event_tx.send(SessionEvent::Output {
                            agent_id: agent_id.clone(),
                            line: format!(
                                "Container ready (user: {}, workspace: {})",
                                info.remote_user, info.remote_workspace_folder
                            ),
                        });

                        // Wrap ACP command with docker exec to run inside the container.
                        // Pass the host token via -e so the copilot CLI authenticates
                        // without needing a keychain inside the container.
                        let token_flag = host_token
                            .as_ref()
                            .map(|t| format!(" -e COPILOT_GITHUB_TOKEN={t}"))
                            .unwrap_or_default();
                        let exec_cmd = format!(
                            "docker exec -i{token_flag} -u {} -w {} {} {}",
                            info.remote_user,
                            info.remote_workspace_folder,
                            info.container_id,
                            acp_command,
                        );

                        let cwd = PathBuf::from(&info.remote_workspace_folder);
                        (exec_cmd, cwd, Some(info))
                    }
                    Err(err) => {
                        error!(agent_id = %agent_id, error = %err, "Container failed to start");
                        let _ = event_tx.send(SessionEvent::Error {
                            agent_id: agent_id.clone(),
                            message: format!("Container failed: {err}"),
                        });
                        let _ = event_tx.send(SessionEvent::Exited {
                            agent_id,
                            code: None,
                        });
                        return;
                    }
                }
            } else {
                // Running on the host — prepend the token as an env var in the
                // ACP command string (the ACP crate parses NAME=value prefixes).
                let cmd = if let Some(ref token) = host_token {
                    format!("COPILOT_GITHUB_TOKEN={token} {acp_command}")
                } else {
                    acp_command
                };
                let cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
                (cmd, cwd, None)
            };

        if let Err(err) = run_persistent_connection(
            agent_id.clone(),
            &effective_command,
            session_cwd,
            container_info.as_ref(),
            previous_session_id,
            prompt_rx,
            event_tx.clone(),
            acp_log.clone(),
        )
        .await
        {
            let _ = event_tx.send(SessionEvent::Error {
                agent_id: agent_id.clone(),
                message: format!("ACP connection failed: {err}"),
            });
        }
        // Connection ended — mark agent as stopped.
        let _ = event_tx.send(SessionEvent::Exited {
            agent_id,
            code: None,
        });
    });

    (prompt_tx, handle.abort_handle())
}

/// Send a prompt through an existing agent connection.
///
/// Note: this does not echo the prompt back as a `SessionEvent` — local
/// echo is purely a frontend concern. The agent itself never sends the
/// live prompt back; it does replay user messages during `session/load`
/// via `SessionEvent::UserMessage`.
pub fn send_message(
    agent_id: AgentId,
    prompt_tx: Option<&mpsc::UnboundedSender<String>>,
    message: String,
    event_tx: mpsc::UnboundedSender<SessionEvent>,
) {
    match prompt_tx {
        Some(tx) => {
            if tx.send(message).is_err() {
                let _ = event_tx.send(SessionEvent::Error {
                    agent_id,
                    message: "Agent connection closed".into(),
                });
            }
        }
        None => {
            let _ = event_tx.send(SessionEvent::Error {
                agent_id,
                message: "Agent not connected — press Enter on agent list to connect".into(),
            });
        }
    }
}
