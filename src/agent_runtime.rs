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
    AuthenticateRequest, ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest,
    ProtocolVersion, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionNotification, SessionUpdate,
    TextContent,
};
use agent_client_protocol::{AcpAgent, Agent as AcpAgentRole, ConnectionTo, LineDirection};
use tokio::sync::mpsc;

use crate::agent::AgentId;
use crate::container::{self, ContainerConfig};
use crate::event::AppEvent;

/// Spawn a persistent ACP connection for an agent.
///
/// If the agent has a workspace folder, starts the dev container first,
/// then wraps the ACP command with `docker exec` to run inside it.
/// Returns an `mpsc::UnboundedSender<String>` for sending prompts.
pub fn start_agent(
    agent_id: AgentId,
    acp_command: String,
    workspace_folder: Option<PathBuf>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) -> mpsc::UnboundedSender<String> {
    let (prompt_tx, prompt_rx) = mpsc::unbounded_channel::<String>();

    tokio::spawn(async move {
        // Resolve a host GitHub token for headless auth.
        // The copilot CLI in --acp mode expects to already be authenticated;
        // passing COPILOT_GITHUB_TOKEN lets it work without a keychain or
        // interactive login.
        let host_token = container::resolve_host_github_token();

        // If workspace_folder is set, start the dev container first.
        let (effective_command, session_cwd, container_info) = if let Some(ref ws) = workspace_folder {
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

                    // Wrap ACP command with docker exec to run inside the container.
                    // Pass the host token via -e so the copilot CLI authenticates
                    // without needing a keychain inside the container.
                    let token_flag = host_token.as_ref()
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
    container_info: Option<&container::ContainerInfo>,
    prompt_rx: mpsc::UnboundedReceiver<String>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let prompt_rx = std::sync::Arc::new(tokio::sync::Mutex::new(prompt_rx));

    connect_and_run(agent_id, acp_command, session_cwd, container_info, prompt_rx, event_tx).await
}

/// Connect to an ACP agent and run the prompt loop.
///
/// If the agent advertises `authMethods`, calls `authenticate` via the
/// ACP protocol. If session creation fails with "Authentication required",
/// sends an `AuthRequired` event so the main loop can run an interactive
/// login flow.
async fn connect_and_run(
    agent_id: AgentId,
    acp_command: &str,
    session_cwd: PathBuf,
    container_info: Option<&container::ContainerInfo>,
    prompt_rx: std::sync::Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<String>>>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut agent = AcpAgent::from_str(acp_command)?;

    // Forward agent stderr to the TUI so the user sees device-code URLs,
    // diagnostic messages, etc.
    let debug_aid = agent_id.clone();
    let debug_tx = event_tx.clone();
    agent = agent.with_debug(move |line, direction| {
        if direction == LineDirection::Stderr {
            let _ = debug_tx.send(AppEvent::AgentOutput {
                agent_id: debug_aid.clone(),
                line: format!("  {line}"),
            });
        }
    });

    // Clone container_info fields we need into the closure (can't move a reference).
    let ci_for_auth: Option<(String, String, String)> = container_info.map(|ci| {
        (ci.container_id.clone(), ci.remote_user.clone(), ci.remote_workspace_folder.clone())
    });

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
                    let _ = tx.send(AppEvent::AgentOutput {
                        agent_id: aid.clone(),
                        line: format!(
                            "[permission] auto-approved: {}",
                            request
                                .tool_call
                                .fields
                                .title
                                .as_deref()
                                .unwrap_or("unknown")
                        ),
                    });
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
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, |connection: ConnectionTo<AcpAgentRole>| {
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
                    let method = &init_resp.auth_methods[0];
                    let _ = tx.send(AppEvent::AgentOutput {
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
                            let _ = tx.send(AppEvent::AgentOutput {
                                agent_id: aid.clone(),
                                line: "✓ Authentication successful.".into(),
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(AppEvent::SessionError {
                                agent_id: aid.clone(),
                                message: format!("Authentication failed: {err}"),
                            });
                            return Ok(());
                        }
                    }
                }

                // Create a session with the appropriate working directory.
                let session_result = connection
                    .send_request(NewSessionRequest::new(session_cwd))
                    .block_task()
                    .await;

                let session_id = match session_result {
                    Ok(resp) => resp.session_id,
                    Err(err) => {
                        let msg = format!("{err}");
                        if msg.contains("Authentication required") || msg.contains("auth") {
                            // Build the interactive auth command.
                            let auth_cmd = build_auth_command(ci_for_auth.as_ref());
                            let _ = tx.send(AppEvent::AuthRequired {
                                agent_id: aid.clone(),
                                command: auth_cmd,
                            });
                        } else {
                            let _ = tx.send(AppEvent::SessionError {
                                agent_id: aid.clone(),
                                message: format!("Session creation failed: {err}"),
                            });
                        }
                        return Ok(());
                    }
                };

                let _ = tx.send(AppEvent::AgentConnected {
                    agent_id: aid.clone(),
                });

                // Prompt loop — wait for messages from the TUI and forward to agent.
                let mut rx = prompt_rx.lock().await;
                while let Some(message) = rx.recv().await {
                    let _ = tx.send(AppEvent::AssistantDelta {
                        agent_id: aid.clone(),
                        text: String::new(),
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
                let _ = tx.send(AppEvent::ThoughtDelta {
                    agent_id: agent_id.to_string(),
                    text: text.text.clone(),
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

/// Construct the interactive auth command for `copilot login`.
///
/// For container agents, wraps with `docker exec -it` so login runs inside
/// the container where copilot stores its credentials. For host agents,
/// runs `copilot login` directly.
fn build_auth_command(
    container_info: Option<&(String, String, String)>,
) -> Vec<String> {
    if let Some((container_id, remote_user, remote_workdir)) = container_info {
        vec![
            "docker".into(),
            "exec".into(),
            "-it".into(),
            "-u".into(),
            remote_user.clone(),
            "-w".into(),
            remote_workdir.clone(),
            container_id.clone(),
            "copilot".into(),
            "login".into(),
        ]
    } else {
        vec!["copilot".into(), "login".into()]
    }
}
