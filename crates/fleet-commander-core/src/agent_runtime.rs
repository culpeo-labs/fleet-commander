//! ACP (Agent Client Protocol) integration layer.
//!
//! Manages the lifecycle of ACP agent subprocesses. Each agent gets a
//! persistent connection: the process is spawned once, a session is created,
//! and a prompt channel allows the TUI to send messages without respawning.
//!
//! When an agent has a `workspace_folder`, the container is started first
//! via `devcontainer up`, and the ACP command is wrapped with
//! `devcontainer exec`.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::{
    AuthenticateRequest, ContentBlock, InitializeRequest, ListSessionsRequest, LoadSessionRequest,
    NewSessionRequest, PermissionOptionKind, PromptRequest, ProtocolVersion,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    ResumeSessionRequest, SelectedPermissionOutcome, SessionNotification, SessionUpdate,
    TextContent, ToolCallStatus,
};
use agent_client_protocol::{AcpAgent, Agent as AcpAgentRole, ConnectionTo, LineDirection};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::container::{self, ContainerConfig};
use crate::session::{AgentId, SessionEvent, ToolCallStatusKind};
use crate::session_state::SessionStateMachine;

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

#[allow(clippy::too_many_arguments)]
async fn run_persistent_connection(
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
    let mut agent = AcpAgent::from_str(acp_command)?;
    info!(agent_id = %agent_id, command = %acp_command, "Connecting to ACP agent");

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
                async move |notification: SessionNotification, _cx| {
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
                                let auth_cmd = build_auth_command(ci_for_auth.as_ref());
                                let _ = tx.send(SessionEvent::AuthRequired {
                                    agent_id: aid.clone(),
                                    command: auth_cmd,
                                });
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

/// Try to rehydrate a specific session by id. Returns the id on success,
/// `None` on any failure (including the agent reporting that it no longer
/// has the session in its store). All failures are logged at WARN level
/// and a user-facing message is sent to the TUI so the operator can see why
/// the saved session couldn't be restored.
async fn try_resume_specific(
    connection: &ConnectionTo<AcpAgentRole>,
    prev_id: &str,
    session_cwd: &Path,
    agent_id: &str,
    tx: &mpsc::UnboundedSender<SessionEvent>,
    can_resume: bool,
    can_load: bool,
) -> Option<String> {
    if can_resume {
        info!(
            agent_id = %agent_id,
            session_id = %prev_id,
            cwd = %session_cwd.display(),
            "Resuming session"
        );
        let _ = tx.send(SessionEvent::Output {
            agent_id: agent_id.to_string(),
            line: format!("Resuming session {prev_id}…"),
        });
        match connection
            .send_request(ResumeSessionRequest::new(
                prev_id.to_string(),
                session_cwd.to_path_buf(),
            ))
            .block_task()
            .await
        {
            Ok(_) => {
                info!(agent_id = %agent_id, session_id = %prev_id, "Session resumed");
                Some(prev_id.to_string())
            }
            Err(err) => {
                warn!(
                    agent_id = %agent_id,
                    session_id = %prev_id,
                    error = %err,
                    "Session resume failed"
                );
                let _ = tx.send(SessionEvent::Output {
                    agent_id: agent_id.to_string(),
                    line: format!("Resume failed ({err})."),
                });
                None
            }
        }
    } else if can_load {
        info!(
            agent_id = %agent_id,
            session_id = %prev_id,
            cwd = %session_cwd.display(),
            "Loading session"
        );
        let _ = tx.send(SessionEvent::Output {
            agent_id: agent_id.to_string(),
            line: format!("Loading session {prev_id}…"),
        });
        match connection
            .send_request(LoadSessionRequest::new(
                prev_id.to_string(),
                session_cwd.to_path_buf(),
            ))
            .block_task()
            .await
        {
            Ok(_) => {
                info!(agent_id = %agent_id, session_id = %prev_id, "Session loaded");
                Some(prev_id.to_string())
            }
            Err(err) => {
                warn!(
                    agent_id = %agent_id,
                    session_id = %prev_id,
                    error = %err,
                    "Session load failed"
                );
                let _ = tx.send(SessionEvent::Output {
                    agent_id: agent_id.to_string(),
                    line: format!("Load failed ({err})."),
                });
                None
            }
        }
    } else {
        None
    }
}

/// Try to find an existing session for `cwd` via `session/list` and resume it.
///
/// Uses `session/resume` when the agent supports it, otherwise falls back to
/// `session/load`. Returns `Some(session_id)` on success, `None` if no
/// matching session is found or the rehydration call fails.
async fn try_find_and_resume(
    connection: &ConnectionTo<AcpAgentRole>,
    session_cwd: &Path,
    agent_id: &str,
    tx: &mpsc::UnboundedSender<SessionEvent>,
    can_resume: bool,
    can_load: bool,
) -> Option<String> {
    let list_result = connection
        .send_request(ListSessionsRequest::new().cwd(session_cwd.to_path_buf()))
        .block_task()
        .await;

    let sessions = match list_result {
        Ok(resp) => {
            info!(
                agent_id = %agent_id,
                cwd = %session_cwd.display(),
                count = resp.sessions.len(),
                "session/list result"
            );
            resp.sessions
        }
        Err(err) => {
            warn!(
                agent_id = %agent_id,
                cwd = %session_cwd.display(),
                error = %err,
                "session/list failed"
            );
            return None;
        }
    };

    // Pick the most recently updated session.
    let best = sessions.into_iter().max_by(|a, b| {
        a.updated_at
            .as_deref()
            .unwrap_or("")
            .cmp(b.updated_at.as_deref().unwrap_or(""))
    })?;

    info!(
        agent_id = %agent_id,
        session_id = %best.session_id.0,
        title = ?best.title,
        "Picking most recent session for cwd"
    );

    let _ = tx.send(SessionEvent::Output {
        agent_id: agent_id.to_string(),
        line: format!(
            "Found existing session {} — resuming…",
            best.title.as_deref().unwrap_or(&best.session_id.0),
        ),
    });

    let result = if can_resume {
        connection
            .send_request(ResumeSessionRequest::new(
                best.session_id.clone(),
                session_cwd.to_path_buf(),
            ))
            .block_task()
            .await
            .map(|_| ())
    } else if can_load {
        connection
            .send_request(LoadSessionRequest::new(
                best.session_id.clone(),
                session_cwd.to_path_buf(),
            ))
            .block_task()
            .await
            .map(|_| ())
    } else {
        return None;
    };

    match result {
        Ok(()) => Some(best.session_id.to_string()),
        Err(err) => {
            let _ = tx.send(SessionEvent::Output {
                agent_id: agent_id.to_string(),
                line: format!("Resume failed ({err}), creating new session…"),
            });
            None
        }
    }
}

/// Apply an ACP `SessionUpdate` to the per-session state machine. The state
/// machine handles emitting the `*Started` events on the first chunk of a
/// new entity and routing follow-up updates through the live handles.
fn apply_session_update(state: &mut SessionStateMachine, update: &SessionUpdate) {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            if let ContentBlock::Text(text) = &chunk.content {
                state.assistant_chunk(&text.text);
            }
        }
        SessionUpdate::UserMessageChunk(chunk) => {
            if let ContentBlock::Text(text) = &chunk.content {
                state.user_chunk(&text.text);
            }
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            if let ContentBlock::Text(text) = &chunk.content {
                state.thought_chunk(&text.text);
            }
        }
        SessionUpdate::ToolCall(tool_call) => {
            state.tool_call(
                &tool_call.tool_call_id.0,
                tool_call.title.clone(),
                map_tool_status(&tool_call.status),
            );
        }
        SessionUpdate::ToolCallUpdate(update) => {
            state.tool_call_update(
                &update.tool_call_id.0,
                update.fields.title.clone(),
                update.fields.status.as_ref().map(map_tool_status),
            );
        }
        _ => {}
    }
}

/// Map the ACP tool-call status enum to our display-side enum so events stay
/// free of ACP types.
fn map_tool_status(status: &ToolCallStatus) -> ToolCallStatusKind {
    match status {
        ToolCallStatus::Pending => ToolCallStatusKind::Pending,
        ToolCallStatus::InProgress => ToolCallStatusKind::InProgress,
        ToolCallStatus::Completed => ToolCallStatusKind::Completed,
        ToolCallStatus::Failed => ToolCallStatusKind::Failed,
        _ => ToolCallStatusKind::InProgress,
    }
}

/// Construct the interactive auth command for `copilot login`.
///
/// For container agents, wraps with `docker exec -it` so login runs inside
/// the container where copilot stores its credentials. For host agents,
/// runs `copilot login` directly.
fn build_auth_command(container_info: Option<&(String, String, String)>) -> Vec<String> {
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
