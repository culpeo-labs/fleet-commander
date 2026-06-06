//! Copilot SDK integration layer.
//!
//! Manages the lifecycle of a [`github_copilot_sdk::Client`] and creates
//! one [`Session`] per agent. Streaming session events are forwarded into
//! the TUI's [`AppEvent`] channel so the UI reacts in real time.

use std::sync::Arc;
use std::time::Duration;

use github_copilot_sdk::handler::ApproveAllHandler;
use github_copilot_sdk::types::{MessageOptions, SessionConfig};
use github_copilot_sdk::{Client, ClientOptions};
use tokio::sync::mpsc;

use crate::agent::{Agent, AgentStatus};
use crate::event::AppEvent;

/// Start the Copilot SDK client and attach sessions to each agent.
///
/// This spawns a background task that:
/// 1. Starts the Copilot CLI client.
/// 2. Creates a session for each agent using its system prompt.
/// 3. Subscribes to each session's event stream and forwards events as
///    `AppEvent`s through `tx`.
///
/// Returns a handle to the client for graceful shutdown.
pub async fn start_copilot_runtime(
    agents: &mut [Agent],
    tx: mpsc::UnboundedSender<AppEvent>,
) -> Option<Client> {
    let client = match Client::start(ClientOptions::default()).await {
        Ok(c) => c,
        Err(err) => {
            let _ = tx.send(AppEvent::CopilotClientError {
                message: format!("Failed to start Copilot client: {err}"),
            });
            return None;
        }
    };

    for agent in agents.iter_mut() {
        let config = SessionConfig::default()
            .with_permission_handler(Arc::new(ApproveAllHandler))
            .with_streaming(true);

        match client.create_session(config).await {
            Ok(session) => {
                let session = Arc::new(session);
                agent.session = Some(Arc::clone(&session));
                agent.status = AgentStatus::Idle;
                agent.history.push("Session connected.".into());

                // Spawn event-forwarding task for this session.
                let agent_id = agent.id.clone();
                let tx = tx.clone();
                let mut events = session.subscribe();
                tokio::spawn(async move {
                    while let Ok(event) = events.recv().await {
                        let evt = match event.event_type.as_str() {
                            "assistant.message_delta" => {
                                let text = event
                                    .data
                                    .get("deltaContent")
                                    .and_then(|c| c.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if text.is_empty() {
                                    continue;
                                }
                                AppEvent::AssistantDelta {
                                    agent_id: agent_id.clone(),
                                    text,
                                }
                            }
                            "assistant.message" => AppEvent::AssistantDone {
                                agent_id: agent_id.clone(),
                            },
                            "session.error" => {
                                let msg = event
                                    .data
                                    .get("message")
                                    .and_then(|m| m.as_str())
                                    .unwrap_or("unknown error")
                                    .to_string();
                                AppEvent::SessionError {
                                    agent_id: agent_id.clone(),
                                    message: msg,
                                }
                            }
                            _ => continue,
                        };
                        if tx.send(evt).is_err() {
                            break;
                        }
                    }
                });
            }
            Err(err) => {
                agent.status = AgentStatus::Error;
                agent.history.push(format!("Session error: {err}"));
            }
        }
    }

    Some(client)
}

/// Send a user message to the agent's Copilot session.
///
/// This is called from the TUI when the user submits input in insert mode.
/// The response arrives asynchronously via the event-forwarding task.
pub fn send_message(agent: &Agent, message: String, tx: mpsc::UnboundedSender<AppEvent>) {
    let Some(session) = agent.session.as_ref() else {
        let _ = tx.send(AppEvent::SessionError {
            agent_id: agent.id.clone(),
            message: "No active session".into(),
        });
        return;
    };

    let session = Arc::clone(session);
    let agent_id = agent.id.clone();
    let tx = tx.clone();

    tokio::spawn(async move {
        let opts = MessageOptions::new(&message).with_wait_timeout(Duration::from_secs(120));

        // Mark the agent as running via an event.
        let _ = tx.send(AppEvent::AgentOutput {
            agent_id: agent_id.clone(),
            line: format!("> {message}"),
        });

        if let Err(err) = session.send_and_wait(opts).await {
            let _ = tx.send(AppEvent::SessionError {
                agent_id: agent_id.clone(),
                message: format!("Send failed: {err}"),
            });
        }
    });
}
