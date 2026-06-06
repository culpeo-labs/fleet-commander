//! ACP (Agent Client Protocol) integration layer.
//!
//! Manages the lifecycle of ACP agent subprocesses. Each agent is spawned
//! as a separate process (e.g. `copilot --acp --stdio`) and communicated
//! with via JSON-RPC over stdio. Session update notifications are forwarded
//! into the TUI's [`AppEvent`] channel so the UI reacts in real time.

use std::str::FromStr;

use agent_client_protocol::schema::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, ProtocolVersion,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, SessionUpdate, TextContent,
};
use agent_client_protocol::{AcpAgent, Agent as AcpAgentRole, ConnectionTo};
use tokio::sync::mpsc;

use crate::agent::AgentId;
use crate::event::AppEvent;

/// Spawn an ACP agent process, initialize it, create a session, and return
/// a handle for sending prompts.
///
/// Session update notifications are forwarded as `AppEvent`s through `tx`.
/// The entire lifecycle runs in a spawned task so it doesn't block the UI.
#[allow(dead_code)] // Will be used when persistent connections are implemented.
pub fn spawn_agent(
    agent_id: AgentId,
    acp_command: String,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        if let Err(err) = run_agent_connection(agent_id.clone(), &acp_command, tx.clone()).await {
            let _ = tx.send(AppEvent::SessionError {
                agent_id,
                message: format!("ACP connection failed: {err}"),
            });
        }
    });
}

/// Send a user prompt to an ACP agent by spawning a fresh connection.
///
/// In the current architecture, each prompt creates a new ACP connection.
/// A future iteration will maintain persistent connections with session resume.
pub fn send_message(agent_id: AgentId, acp_command: String, message: String, tx: mpsc::UnboundedSender<AppEvent>) {
    // Echo the user's message first.
    let _ = tx.send(AppEvent::AgentOutput {
        agent_id: agent_id.clone(),
        line: format!("> {message}"),
    });

    tokio::spawn(async move {
        if let Err(err) = run_prompt(agent_id.clone(), &acp_command, &message, tx.clone()).await {
            let _ = tx.send(AppEvent::SessionError {
                agent_id,
                message: format!("Prompt failed: {err}"),
            });
        }
    });
}

#[allow(dead_code)] // Will be used when persistent connections are implemented.
async fn run_agent_connection(
    agent_id: AgentId,
    acp_command: &str,
    tx: mpsc::UnboundedSender<AppEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let agent = AcpAgent::from_str(acp_command)?;
    let tx_notif = tx.clone();
    let aid = agent_id.clone();

    agent_client_protocol::Client
        .builder()
        .on_receive_notification(
            {
                let aid = aid.clone();
                let tx = tx_notif.clone();
                async move |notification: SessionNotification, _cx| {
                    forward_session_update(&aid, &notification.update, &tx);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _connection| {
                // Auto-approve for now; future: route to TUI for user decision.
                let option_id = request.options.first().map(|opt| opt.option_id.clone());
                if let Some(id) = option_id {
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
                    ))
                } else {
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| {
            let aid = aid.clone();
            let tx = tx.clone();
            async move {
                // Initialize
                let _init = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                // Create session
                let cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
                let session_resp = connection
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;

                let _ = tx.send(AppEvent::AgentConnected {
                    agent_id: aid.clone(),
                });
                let _ = tx.send(AppEvent::AgentOutput {
                    agent_id: aid.clone(),
                    line: format!("Session created: {}", session_resp.session_id),
                });

                // Keep connection alive — the process will exit when dropped.
                // In a full implementation, we'd hold this and accept prompts.
                // For now, signal readiness and let the connection close.
                Ok(())
            }
        })
        .await?;

    Ok(())
}

async fn run_prompt(
    agent_id: AgentId,
    acp_command: &str,
    message: &str,
    tx: mpsc::UnboundedSender<AppEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let agent = AcpAgent::from_str(acp_command)?;
    let tx_notif = tx.clone();
    let aid = agent_id.clone();
    let prompt_text = message.to_string();

    agent_client_protocol::Client
        .builder()
        .on_receive_notification(
            {
                let aid = aid.clone();
                let tx = tx_notif.clone();
                async move |notification: SessionNotification, _cx| {
                    forward_session_update(&aid, &notification.update, &tx);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _connection| {
                let option_id = request.options.first().map(|opt| opt.option_id.clone());
                if let Some(id) = option_id {
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
                    ))
                } else {
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| {
            let aid = aid.clone();
            let tx = tx.clone();
            let prompt_text = prompt_text.clone();
            async move {
                // Initialize
                let _init = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                // Create session
                let cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
                let session_resp = connection
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;

                // Send prompt
                let _prompt_resp = connection
                    .send_request(PromptRequest::new(
                        session_resp.session_id,
                        vec![ContentBlock::Text(TextContent::new(prompt_text))],
                    ))
                    .block_task()
                    .await?;

                let _ = tx.send(AppEvent::AssistantDone {
                    agent_id: aid,
                });

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
