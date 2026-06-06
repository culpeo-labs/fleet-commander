//! ACP (Agent Client Protocol) integration layer.
//!
//! Manages the lifecycle of ACP agent subprocesses. Each agent gets a
//! persistent connection: the process is spawned once, a session is created,
//! and a prompt channel allows the TUI to send messages without respawning.
//!
//! When an agent has a `workspace_folder`, the container is started first
//! via `devcontainer up`, and the ACP command is wrapped with
//! `devcontainer exec`.

use std::path::PathBuf;
use std::str::FromStr;

use agent_client_protocol::schema::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, ProtocolVersion,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, SessionUpdate, TextContent,
};
use agent_client_protocol::{AcpAgent, Agent as AcpAgentRole, ConnectionTo};
use tokio::sync::mpsc;

use crate::agent::AgentId;
use crate::container::{self, ContainerConfig};
use crate::event::AppEvent;

/// Spawn a persistent ACP connection for an agent.
///
/// If the agent has a workspace folder, starts the dev container first.
/// Returns an `mpsc::UnboundedSender<String>` for sending prompts.
pub fn start_agent(
    agent_id: AgentId,
    effective_command: String,
    workspace_folder: Option<PathBuf>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) -> mpsc::UnboundedSender<String> {
    let (prompt_tx, prompt_rx) = mpsc::unbounded_channel::<String>();

    tokio::spawn(async move {
        // If workspace_folder is set, start the dev container first.
        let session_cwd = if let Some(ref ws) = workspace_folder {
            let _ = event_tx.send(AppEvent::AgentOutput {
                agent_id: agent_id.clone(),
                line: format!("Starting container for {}...", ws.display()),
            });

            let config = ContainerConfig {
                workspace_folder: ws.clone(),
            };
            match container::start_container(&config).await {
                Ok(info) => {
                    let _ = event_tx.send(AppEvent::AgentOutput {
                        agent_id: agent_id.clone(),
                        line: format!(
                            "Container ready (user: {}, workspace: {})",
                            info.remote_user, info.remote_workspace_folder
                        ),
                    });
                    PathBuf::from(info.remote_workspace_folder)
                }
                Err(err) => {
                    let _ = event_tx.send(AppEvent::SessionError {
                        agent_id: agent_id.clone(),
                        message: format!("Container failed: {err}"),
                    });
                    let _ = event_tx.send(AppEvent::AgentExited {
                        agent_id,
                        code: None,
                    });
                    return;
                }
            }
        } else {
            std::env::current_dir().unwrap_or_else(|_| "/".into())
        };

        if let Err(err) = run_persistent_connection(
            agent_id.clone(),
            &effective_command,
            session_cwd,
            prompt_rx,
            event_tx.clone(),
        )
        .await
        {
            let _ = event_tx.send(AppEvent::SessionError {
                agent_id: agent_id.clone(),
                message: format!("ACP connection failed: {err}"),
            });
        }
        // Connection ended — mark agent as stopped.
        let _ = event_tx.send(AppEvent::AgentExited {
            agent_id,
            code: None,
        });
    });

    prompt_tx
}

/// Send a prompt through an existing agent connection.
pub fn send_message(
    agent_id: AgentId,
    prompt_tx: Option<&mpsc::UnboundedSender<String>>,
    message: String,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) {
    // Echo the user's message.
    let _ = event_tx.send(AppEvent::AgentOutput {
        agent_id: agent_id.clone(),
        line: format!("> {message}"),
    });

    match prompt_tx {
        Some(tx) => {
            if tx.send(message).is_err() {
                let _ = event_tx.send(AppEvent::SessionError {
                    agent_id,
                    message: "Agent connection closed".into(),
                });
            }
        }
        None => {
            let _ = event_tx.send(AppEvent::SessionError {
                agent_id,
                message: "Agent not connected — press Enter on agent list to connect".into(),
            });
        }
    }
}

async fn run_persistent_connection(
    agent_id: AgentId,
    acp_command: &str,
    session_cwd: PathBuf,
    mut prompt_rx: mpsc::UnboundedReceiver<String>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let agent = AcpAgent::from_str(acp_command)?;
    let aid = agent_id.clone();
    let tx = event_tx.clone();

    agent_client_protocol::Client
        .builder()
        .on_receive_notification(
            {
                let aid = aid.clone();
                let tx = tx.clone();
                async move |notification: SessionNotification, _cx| {
                    forward_session_update(&aid, &notification.update, &tx);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let aid = aid.clone();
                let tx = tx.clone();
                async move |request: RequestPermissionRequest, responder, _connection| {
                    // Log the permission request for visibility.
                    let _ = tx.send(AppEvent::AgentOutput {
                        agent_id: aid.clone(),
                        line: format!(
                            "[permission] auto-approved: {}",
                            request.tool_call.fields.title.as_deref().unwrap_or("unknown")
                        ),
                    });
                    // Auto-approve by selecting the first option.
                    let option_id = request.options.first().map(|opt| opt.option_id.clone());
                    if let Some(id) = option_id {
                        responder.respond(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Selected(
                                SelectedPermissionOutcome::new(id),
                            ),
                        ))
                    } else {
                        responder.respond(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Cancelled,
                        ))
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| {
            let aid = aid.clone();
            let tx = event_tx;
            async move {
                // Initialize the ACP protocol.
                let _init = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                // Create a session with the appropriate working directory.
                let session_resp = connection
                    .send_request(NewSessionRequest::new(session_cwd))
                    .block_task()
                    .await?;

                let session_id = session_resp.session_id;

                let _ = tx.send(AppEvent::AgentConnected {
                    agent_id: aid.clone(),
                });

                // Prompt loop — wait for messages from the TUI and forward to agent.
                while let Some(message) = prompt_rx.recv().await {
                    let _ = tx.send(AppEvent::AssistantDelta {
                        agent_id: aid.clone(),
                        text: String::new(), // Signals "running" state.
                    });

                    let result = connection
                        .send_request(PromptRequest::new(
                            session_id.clone(),
                            vec![ContentBlock::Text(TextContent::new(message))],
                        ))
                        .block_task()
                        .await;

                    match result {
                        Ok(_prompt_resp) => {
                            let _ = tx.send(AppEvent::AssistantDone {
                                agent_id: aid.clone(),
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(AppEvent::SessionError {
                                agent_id: aid.clone(),
                                message: format!("Prompt error: {err}"),
                            });
                        }
                    }
                }

                Ok(())
            }
        })
        .await?;

    Ok(())
}

/// Map an ACP `SessionUpdate` to `AppEvent`s and send them.
fn forward_session_update(
    agent_id: &str,
    update: &SessionUpdate,
    tx: &mpsc::UnboundedSender<AppEvent>,
) {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            if let ContentBlock::Text(text) = &chunk.content {
                let _ = tx.send(AppEvent::AssistantDelta {
                    agent_id: agent_id.to_string(),
                    text: text.text.clone(),
                });
            }
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            if let ContentBlock::Text(text) = &chunk.content {
                let _ = tx.send(AppEvent::AgentOutput {
                    agent_id: agent_id.to_string(),
                    line: format!("[thought] {}", text.text),
                });
            }
        }
        SessionUpdate::ToolCall(tool_call) => {
            let _ = tx.send(AppEvent::ToolCallUpdate {
                agent_id: agent_id.to_string(),
                tool_name: tool_call.title.clone(),
                status: "started".to_string(),
            });
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let status = format!("{:?}", update.fields.status);
            let _ = tx.send(AppEvent::ToolCallUpdate {
                agent_id: agent_id.to_string(),
                tool_name: String::new(),
                status,
            });
        }
        _ => {}
    }
}
