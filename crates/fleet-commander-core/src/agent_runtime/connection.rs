//! Process spawn, ACP handshake, and the prompt loop.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AuthenticateRequest, ContentBlock, InitializeRequest, NewSessionRequest, PermissionOptionKind,
    PromptRequest, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, SessionUpdate, TextContent,
};
use agent_client_protocol::{
    AcpAgent, Agent as AcpAgentRole, ConnectionTo, DynConnectTo, LineDirection,
};
use tokio::sync::mpsc;
use tracing::info;

use crate::container;
use crate::session::{AgentId, AvailableCommand, SessionEvent};
use crate::session_state::SessionStateMachine;

use super::AcpLog;
use super::auth::{build_auth_command, terminal_auth_command};
use super::resume::{try_find_and_resume, try_resume_specific};
use super::tunnel;
use super::updates::apply_session_update;

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_persistent_connection(
    agent_id: AgentId,
    acp_command: &str,
    session_cwd: PathBuf,
    container_info: Option<&container::ContainerInfo>,
    previous_session_id: Option<String>,
    prompt_rx: mpsc::UnboundedReceiver<String>,
    event_tx: mpsc::UnboundedSender<SessionEvent>,
    acp_log: Option<AcpLog>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let prompt_rx = std::sync::Arc::new(tokio::sync::Mutex::new(prompt_rx));

    connect_and_run(
        agent_id,
        acp_command,
        session_cwd,
        container_info,
        previous_session_id,
        prompt_rx,
        event_tx,
        acp_log,
    )
    .await
}

/// Connect to an ACP agent and run the prompt loop.
///
/// If the agent advertises `authMethods`, calls `authenticate` via the
/// ACP protocol. If session creation fails with "Authentication required",
/// sends an `AuthRequired` event so the main loop can run an interactive
/// login flow.
///
/// When `previous_session_id` is set and the agent supports session resume,
/// attempts to resume that session instead of creating a new one.
#[allow(clippy::too_many_arguments)]
async fn connect_and_run(
    agent_id: AgentId,
    acp_command: &str,
    session_cwd: PathBuf,
    container_info: Option<&container::ContainerInfo>,
    previous_session_id: Option<String>,
    prompt_rx: std::sync::Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<String>>>,
    event_tx: mpsc::UnboundedSender<SessionEvent>,
    acp_log: Option<AcpLog>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!(agent_id = %agent_id, command = %acp_command, "Connecting to ACP agent");

    // Build the ACP transport. Inside a container the agent is spawned and
    // tunnelled by `fleet-agent` (Phase 4a); on the host it is spawned
    // directly. Either way it is type-erased to a `DynConnectTo<Client>` so the
    // builder chain below is identical.
    let component: DynConnectTo<agent_client_protocol::Client> = if let Some(ci) = container_info {
        let lines = tunnel::connect(
            ci,
            acp_command,
            agent_id.clone(),
            event_tx.clone(),
            acp_log.clone(),
        )?;
        DynConnectTo::new(lines)
    } else {
        let mut agent = AcpAgent::from_str(acp_command)?;
        // Forward agent stderr to the TUI so the user sees device-code URLs,
        // diagnostic messages, etc. When --acp-log is set, also append every
        // wire line (both directions) to the log file for protocol debugging.
        let debug_aid = agent_id.clone();
        let debug_tx = event_tx.clone();
        let debug_log = acp_log.clone();
        agent = agent.with_debug(move |line, direction| {
            if direction == LineDirection::Stderr {
                let _ = debug_tx.send(SessionEvent::Output {
                    agent_id: debug_aid.clone(),
                    line: format!("  {line}"),
                });
            }
            if let Some(ref log) = debug_log {
                let prefix = match direction {
                    LineDirection::Stdin => ">>",
                    LineDirection::Stdout => "<<",
                    LineDirection::Stderr => "!!",
                };
                if let Ok(mut file) = log.lock() {
                    use std::io::Write;
                    let _ = writeln!(file, "[{debug_aid}] {prefix} {line}");
                }
            }
        });
        DynConnectTo::new(agent)
    };

    // Clone container_info fields we need into the closure (can't move a reference).
    let ci_for_auth: Option<(String, String, String)> = container_info.map(|ci| {
        (
            ci.container_id.clone(),
            ci.remote_user.clone(),
            ci.remote_workspace_folder.clone(),
        )
    });

    let aid = agent_id.clone();
    let tx = event_tx.clone();
    let state = Arc::new(Mutex::new(SessionStateMachine::new(
        aid.clone(),
        tx.clone(),
    )));

    agent_client_protocol::Client
        .builder()
        .on_receive_notification(
            {
                let state = state.clone();
                let aid = aid.clone();
                let tx = tx.clone();
                async move |notification: SessionNotification, _cx| {
                    // Available-commands updates are session metadata, not chat
                    // history; route them straight to the consumer rather than
                    // through the chat state machine.
                    if let SessionUpdate::AvailableCommandsUpdate(ref upd) =
                        notification.update
                    {
                        let commands = upd
                            .available_commands
                            .iter()
                            .map(|c| AvailableCommand {
                                name: c.name.clone(),
                                description: c.description.clone(),
                                hint: c.input.as_ref().and_then(|input| match input {
                                    agent_client_protocol::schema::v1::AvailableCommandInput::Unstructured(u) => Some(u.hint.clone()),
                                    _ => None,
                                }),
                            })
                            .collect::<Vec<_>>();
                        let _ = tx.send(SessionEvent::AvailableCommands {
                            agent_id: aid.clone(),
                            commands,
                        });
                        return Ok(());
                    }
                    let mut sm = state.lock().expect("session state lock poisoned");
                    apply_session_update(&mut sm, &notification.update);
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
                    let tool_title = request
                        .tool_call
                        .fields
                        .title
                        .as_deref()
                        .unwrap_or("unknown")
                        .to_string();

                    // Build option list for the UI.
                    let options: Vec<(String, String, String)> = request
                        .options
                        .iter()
                        .map(|opt| {
                            let kind_label = match opt.kind {
                                PermissionOptionKind::AllowOnce => "allow once",
                                PermissionOptionKind::AllowAlways => "allow always",
                                PermissionOptionKind::RejectOnce => "reject once",
                                PermissionOptionKind::RejectAlways => "reject always",
                                _ => "unknown",
                            };
                            (opt.option_id.0.to_string(), opt.name.clone(), kind_label.to_string())
                        })
                        .collect();

                    // Create a oneshot channel for the user's response.
                    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                    let reply = std::sync::Arc::new(std::sync::Mutex::new(Some(reply_tx)));

                    let _ = tx.send(SessionEvent::PermissionRequest {
                        agent_id: aid.clone(),
                        tool_name: tool_title.clone(),
                        options,
                        reply,
                    });

                    // Wait for the user to respond.
                    match reply_rx.await {
                        Ok(Some(option_id)) => {
                            let _ = tx.send(SessionEvent::Output {
                                agent_id: aid.clone(),
                                line: format!("[permission] approved: {tool_title}"),
                            });
                            responder.respond(RequestPermissionResponse::new(
                                RequestPermissionOutcome::Selected(
                                    SelectedPermissionOutcome::new(option_id),
                                ),
                            ))
                        }
                        _ => {
                            let _ = tx.send(SessionEvent::Output {
                                agent_id: aid.clone(),
                                line: format!("[permission] denied: {tool_title}"),
                            });
                            responder.respond(RequestPermissionResponse::new(
                                RequestPermissionOutcome::Cancelled,
                            ))
                        }
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(component, |connection: ConnectionTo<AcpAgentRole>| {
            let aid = aid.clone();
            let tx = event_tx;
            async move {
                // Initialize the ACP protocol.
                let init_resp = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                // Authenticate if the agent requires it.
                if !init_resp.auth_methods.is_empty() {
                    info!(agent_id = %aid, methods = init_resp.auth_methods.len(), "Authentication required");
                    let method = &init_resp.auth_methods[0];
                    let _ = tx.send(SessionEvent::Output {
                        agent_id: aid.clone(),
                        line: format!(
                            "🔑 Authentication required: {} — {}",
                            method.name(),
                            method.description().unwrap_or("authenticating…"),
                        ),
                    });

                    match connection
                        .send_request(AuthenticateRequest::new(method.id().clone()))
                        .block_task()
                        .await
                    {
                        Ok(_) => {
                            let _ = tx.send(SessionEvent::Output {
                                agent_id: aid.clone(),
                                line: "✓ Authentication successful.".into(),
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(SessionEvent::Error {
                                agent_id: aid.clone(),
                                message: format!("Authentication failed: {err}"),
                            });
                            return Ok(());
                        }
                    }
                }

                let caps = &init_resp.agent_capabilities.session_capabilities;
                let can_resume = caps.resume.is_some();
                let can_list = caps.list.is_some();
                let can_load = init_resp.agent_capabilities.load_session;
                info!(
                    agent_id = %aid,
                    can_resume,
                    can_load,
                    can_list,
                    previous_session_id = ?previous_session_id,
                    "Session capabilities"
                );

                // Try to resume an existing session. Prefer `session/resume` when
                // advertised; fall back to `session/load` for agents (like
                // Copilot CLI) that only support the older mechanism. If the
                // saved session id is rejected (e.g., Copilot CLI dropped it
                // from its store between runs), fall back to `session/list` to
                // discover any other session for the same cwd before giving up
                // and creating a fresh one.
                let mut session_id: Option<String> = None;

                if let Some(ref prev_id) = previous_session_id {
                    session_id = try_resume_specific(
                        &connection,
                        prev_id,
                        &session_cwd,
                        &aid,
                        &tx,
                        can_resume,
                        can_load,
                    )
                    .await;
                }

                if session_id.is_none() && can_list && (can_resume || can_load) {
                    // Either no saved id, or the saved id is stale. Ask the
                    // agent which sessions it actually has for this cwd.
                    session_id = try_find_and_resume(
                        &connection,
                        &session_cwd,
                        &aid,
                        &tx,
                        can_resume,
                        can_load,
                    )
                    .await;
                }

                // Rename to make the next step's intent explicit: this is the
                // id from the resume/load path, if any.
                let resumed_session_id = session_id;

                // If resume didn't work, create a new session. If it did, flush
                // any replayed chunks (UserMessageChunk / AgentMessageChunk
                // notifications sent during load/resume) into history so they
                // get markdown rendering — the agent never sends an explicit
                // turn-end signal for replayed history.
                let session_id: String = if let Some(id) = resumed_session_id {
                    state.lock().expect("session state lock poisoned").prompt_complete();
                    id
                } else {
                    let session_result = connection
                        .send_request(NewSessionRequest::new(session_cwd))
                        .block_task()
                        .await;

                    match session_result {
                        Ok(resp) => {
                            let id = resp.session_id.to_string();
                            info!(agent_id = %aid, session_id = %id, "New session created");
                            id
                        }
                        Err(err) => {
                            let msg = format!("{err}");
                            if msg.contains("Authentication required") || msg.contains("auth") {
                                match  init_resp.auth_methods.first().and_then(terminal_auth_command)
                                {
                                    Some(terminal) => {
                                        let auth_cmd = build_auth_command(ci_for_auth.as_ref(), &terminal);
                                        let _ = tx.send(SessionEvent::AuthRequired {
                                            agent_id: aid.clone(),
                                            command: auth_cmd,
                                        });

                                    }
                                    None => {
                                        let _ = tx.send(SessionEvent::Error {
                                         agent_id: aid.clone(),
                                        message:
                                            "agent required authentication but advertised no terminal login command"
                                                .into(),
                                        });
                                    }
                                }
                            } else {
                                let _ = tx.send(SessionEvent::Error {
                                    agent_id: aid.clone(),
                                    message: format!("Session creation failed: {err}"),
                                });
                            }
                            return Ok(());
                        }
                    }
                };

                let _ = tx.send(SessionEvent::Connected {
                    agent_id: aid.clone(),

                    session_id: Some(session_id.clone()),
                });

                // Prompt loop — wait for messages from the TUI and forward to agent.
                let mut rx = prompt_rx.lock().await;
                while let Some(message) = rx.recv().await {
                    let result = connection
                        .send_request(PromptRequest::new(
                            session_id.clone(),
                            vec![ContentBlock::Text(TextContent::new(message))],
                        ))
                        .block_task()
                        .await;

                    match result {
                        Ok(_prompt_resp) => {
                            state
                                .lock()
                                .expect("session state lock poisoned")
                                .prompt_complete();
                        }
                        Err(err) => {
                            let msg = format!("Prompt error: {err}");
                            state
                                .lock()
                                .expect("session state lock poisoned")
                                .fail_active(&msg);
                            let _ = tx.send(SessionEvent::Error {
                                agent_id: aid.clone(),
                                message: msg,
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
