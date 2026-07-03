//! Daemon-owned ACP session (Phase 4b2).
//!
//! Unlike the raw `acp.*` byte tunnel (Phase 4a), where the host ran the ACP
//! client and the daemon only relayed stdio, here the **daemon** owns the ACP
//! client: it spawns the coding agent, runs the ACP handshake
//! (initialize/authenticate/resume-or-new) once, and keeps the connection alive
//! at daemon scope. The host drives it through the higher-level `session.*`
//! protocol:
//!
//! - `session.start` (request) → spawn/resume and return the session id (or an
//!   `auth_required` terminal command).
//! - `session.prompt` (notification) → run a prompt turn; completion arrives as
//!   a `session.promptResult` notification.
//! - `session.permissionRespond` (notification) → the operator's answer to a
//!   `session.permissionRequest`.
//!
//! Progress flows back as `session.update` (raw ACP `session/update` JSON, which
//! the host aggregates itself), plus `session.connected`/`output`/`error`/
//! `exit`/`authRequired`.
//!
//! The ACP client is async (tokio + `agent-client-protocol`), but the daemon's
//! serve loop is synchronous. So a session runs on its own thread with a
//! dedicated tokio runtime; [`SessionHandle`] is the sync-side handle the
//! dispatch loop holds. Dropping it closes the prompt channel, which ends the
//! prompt loop, drops the ACP connection, and kills the agent child.

use std::collections::HashMap;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AuthMethod, AuthenticateRequest, ContentBlock, InitializeRequest, ListSessionsRequest,
    LoadSessionRequest, NewSessionRequest, PermissionOptionKind, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    ResumeSessionRequest, SelectedPermissionOutcome, SessionNotification, TextContent,
};
use agent_client_protocol::{
    AcpAgent, Agent as AcpAgentRole, ConnectionTo, DynConnectTo, LineDirection,
};
use fleet_protocol::{
    Notification, PermissionOption, Request, Response, SessionAuthRequiredParams,
    SessionConnectedParams, SessionErrorParams, SessionExitParams, SessionOutputParams,
    SessionPermissionRequestParams, SessionPermissionRespondParams, SessionPromptParams,
    SessionPromptResultParams, SessionStartParams, SessionStartResult, SessionUpdateParams,
    methods,
};
use serde::Serialize;
use tokio::sync::mpsc as tokio_mpsc;
use tokio::sync::oneshot;

use crate::util::parse_params;

/// Registry of in-flight permission prompts, keyed by the `request_id` the
/// daemon minted. The dispatch loop resolves an entry when the matching
/// `session.permissionRespond` arrives.
type PermissionRegistry = Arc<Mutex<HashMap<String, oneshot::Sender<Option<String>>>>>;

/// The result of the initial handshake, sent back to the sync `session.start`
/// caller so it can build a [`SessionStartResult`].
enum StartOutcome {
    Connected { session_id: Option<String> },
    AuthRequired { command: Vec<String> },
    Failed { message: String },
}

/// Sync-side handle to a daemon-owned ACP session. Held by the dispatch loop;
/// dropping it tears the session down.
pub(crate) struct SessionHandle {
    prompt_tx: tokio_mpsc::UnboundedSender<String>,
    perms: PermissionRegistry,
    session_id: Option<String>,
    thread: Option<JoinHandle<()>>,
}

/// Outcome of [`SessionHandle::start`]: the started result to return to the
/// host, plus the live handle when a session actually opened (absent when the
/// handshake ended in auth-required or failure, in which case the worker thread
/// has already finished).
pub(crate) struct Started {
    pub result: SessionStartResult,
    pub handle: Option<SessionHandle>,
}

impl SessionHandle {
    /// Start (or resume) a daemon-owned ACP session. Blocks until the handshake
    /// resolves (connected, auth-required, or failed) — well within the host's
    /// request timeout — then returns the [`Started`] outcome.
    pub(crate) fn start(params: SessionStartParams, out: std_mpsc::Sender<Vec<u8>>) -> Started {
        let (prompt_tx, prompt_rx) = tokio_mpsc::unbounded_channel::<String>();
        let perms: PermissionRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (outcome_tx, outcome_rx) = std_mpsc::channel::<StartOutcome>();

        let perms_worker = perms.clone();
        let thread = thread::Builder::new()
            .name("fleet-agent-session".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = outcome_tx.send(StartOutcome::Failed {
                            message: format!("tokio runtime: {e}"),
                        });
                        return;
                    }
                };
                runtime.block_on(run_session(
                    params,
                    out,
                    prompt_rx,
                    perms_worker,
                    outcome_tx,
                ));
            })
            .expect("spawn session thread");

        // Wait for the handshake to resolve. A dropped sender (worker panicked)
        // surfaces as a failure rather than a hang.
        let outcome = outcome_rx.recv().unwrap_or(StartOutcome::Failed {
            message: "session worker exited before reporting status".into(),
        });

        match outcome {
            StartOutcome::Connected { session_id } => Started {
                result: SessionStartResult {
                    session_id: session_id.clone(),
                    auth_required: None,
                },
                handle: Some(SessionHandle {
                    prompt_tx,
                    perms,
                    session_id,
                    thread: Some(thread),
                }),
            },
            StartOutcome::AuthRequired { command } => {
                let _ = thread.join();
                Started {
                    result: SessionStartResult {
                        session_id: None,
                        auth_required: Some(command),
                    },
                    handle: None,
                }
            }
            StartOutcome::Failed { message } => {
                let _ = thread.join();
                Started {
                    result: SessionStartResult {
                        session_id: None,
                        auth_required: Some(vec![format!("session start failed: {message}")]),
                    },
                    handle: None,
                }
            }
        }
    }

    /// Feed a prompt turn to the running session. Ignored if the session has
    /// already ended (the host will observe `session.exit`).
    pub(crate) fn prompt(&self, text: String) {
        let _ = self.prompt_tx.send(text);
    }

    /// Resolve a pending permission prompt with the operator's answer.
    pub(crate) fn respond_permission(&self, request_id: &str, option_id: Option<String>) {
        if let Some(reply) = self
            .perms
            .lock()
            .expect("permission registry poisoned")
            .remove(request_id)
        {
            let _ = reply.send(option_id);
        }
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        // Fail any still-pending permission prompts so their awaiting ACP tasks
        // unblock (they resolve to "cancelled"), then close the prompt channel
        // to end the prompt loop → drop the ACP connection → kill the child.
        if let Ok(mut map) = self.perms.lock() {
            map.clear();
        }
        // Dropping `prompt_tx` happens as the struct is dropped; explicitly join
        // the worker so the child is reaped before we return.
        if let Some(thread) = self.thread.take() {
            // The prompt channel closes when `self.prompt_tx` drops at the end
            // of this scope, so drop our sender first by replacing it.
            let (dead_tx, _) = tokio_mpsc::unbounded_channel();
            let live = std::mem::replace(&mut self.prompt_tx, dead_tx);
            drop(live);
            let _ = thread.join();
        }
    }
}

/// Serialize a `session.*` notification into a frame for the outbound channel.
fn frame(method: &str, params: impl Serialize) -> Vec<u8> {
    serde_json::to_vec(&Notification::new(method, params)).unwrap_or_default()
}

impl crate::Server {
    /// Handle a `session.start` request: start (or resume) a daemon-owned ACP
    /// session and return its id (or an auth-required command). If a session is
    /// already running on this connection, returns its current id unchanged.
    pub(crate) fn handle_session_start(
        &self,
        req: &Request,
        out: &std_mpsc::Sender<Vec<u8>>,
        session: &mut Option<SessionHandle>,
    ) -> Response {
        let params: SessionStartParams = match parse_params(req) {
            Ok(p) => p,
            Err(e) => return Response::err(req.id, e),
        };
        if let Some(existing) = session.as_ref() {
            return Response::ok(
                req.id,
                SessionStartResult {
                    session_id: existing.session_id.clone(),
                    auth_required: None,
                },
            );
        }
        let started = SessionHandle::start(params, out.clone());
        let result = started.result.clone();
        *session = started.handle;
        Response::ok(req.id, result)
    }
}

/// Route a `session.prompt` notification to the running session.
pub(crate) fn handle_session_prompt(note: &Notification, session: &Option<SessionHandle>) {
    let params: SessionPromptParams = match note
        .params
        .clone()
        .and_then(|p| serde_json::from_value(p).ok())
    {
        Some(p) => p,
        None => return,
    };
    if let Some(handle) = session.as_ref() {
        handle.prompt(params.text);
    }
}

/// Route a `session.permissionRespond` notification to the running session.
pub(crate) fn handle_session_permission_respond(
    note: &Notification,
    session: &Option<SessionHandle>,
) {
    let params: SessionPermissionRespondParams = match note
        .params
        .clone()
        .and_then(|p| serde_json::from_value(p).ok())
    {
        Some(p) => p,
        None => return,
    };
    if let Some(handle) = session.as_ref() {
        handle.respond_permission(&params.request_id, params.option_id);
    }
}

/// Handle a `session.cancel` request. A full mid-turn cancel (forwarding an ACP
/// `session/cancel` to the agent) lands with the host rewire; for now this
/// acknowledges the request without interrupting the in-flight turn.
pub(crate) fn handle_session_cancel(req: &Request, session: &Option<SessionHandle>) -> Response {
    let active = session.is_some();
    Response::ok(req.id, serde_json::json!({ "cancelled": active }))
}

/// Build the ACP agent from the requested command line, injecting any extra
/// environment variables. Without env vars this is a straight `from_str`
/// (which handles shell-style quoting); with them we assemble the stdio server
/// JSON so the vars reach the child.
fn build_agent(params: &SessionStartParams) -> Result<AcpAgent, String> {
    use std::str::FromStr;
    if params.env.is_empty() {
        return AcpAgent::from_str(&params.command).map_err(|e| format!("{e}"));
    }
    let mut parts = params.command.split_whitespace();
    let program = parts.next().ok_or("empty acp command")?;
    let args: Vec<String> = parts.map(String::from).collect();
    let env: Vec<serde_json::Value> = params
        .env
        .iter()
        .map(|v| serde_json::json!({ "name": v.name, "value": v.value }))
        .collect();
    let config = serde_json::json!({
        "type": "stdio",
        "name": "acp",
        "command": program,
        "args": args,
        "env": env,
    });
    AcpAgent::from_str(&config.to_string()).map_err(|e| format!("{e}"))
}

/// Extract the interactive `(program, args)` login command from an auth
/// method's `terminal-auth` metadata, if present.
fn terminal_auth_command(method: &AuthMethod) -> Option<Vec<String>> {
    let terminal_auth = method.meta()?.get("terminal-auth")?;
    let command = terminal_auth.get("command")?.as_str()?.to_string();
    let mut v = vec![command];
    if let Some(args) = terminal_auth.get("args").and_then(|a| a.as_array()) {
        v.extend(args.iter().filter_map(|a| a.as_str().map(String::from)));
    }
    Some(v)
}

/// Drive one daemon-owned ACP session to completion. Runs on the session
/// thread's tokio runtime.
async fn run_session(
    params: SessionStartParams,
    out: std_mpsc::Sender<Vec<u8>>,
    prompt_rx: tokio_mpsc::UnboundedReceiver<String>,
    perms: PermissionRegistry,
    outcome_tx: std_mpsc::Sender<StartOutcome>,
) {
    // The ACP handler closures require `Send + Sync` senders, but the outbound
    // channel to the writer thread is a std mpsc `Sender` (not `Sync`). Bridge
    // through a tokio channel drained by a forwarder task.
    let (note_tx, mut note_rx) = tokio_mpsc::unbounded_channel::<Vec<u8>>();
    let forwarder = tokio::spawn(async move {
        while let Some(frame) = note_rx.recv().await {
            if out.send(frame).is_err() {
                break;
            }
        }
    });

    let agent = match build_agent(&params) {
        Ok(a) => a,
        Err(e) => {
            let _ = outcome_tx.send(StartOutcome::Failed {
                message: format!("spawn agent: {e}"),
            });
            return;
        }
    };

    // Forward the agent's stderr (device-code login URLs, diagnostics) to the
    // host as `session.output`.
    let stderr_tx = note_tx.clone();
    let agent = agent.with_debug(move |line, direction| {
        if direction == LineDirection::Stderr {
            let _ = stderr_tx.send(frame(
                methods::SESSION_OUTPUT,
                SessionOutputParams {
                    line: line.to_string(),
                },
            ));
        }
    });

    let component: DynConnectTo<agent_client_protocol::Client> = DynConnectTo::new(agent);
    let cwd = std::path::PathBuf::from(&params.cwd);
    let previous_session_id = params.previous_session_id.clone();

    // Shared so the prompt loop and the handshake can both emit outbound frames.
    let outcome_tx = Arc::new(Mutex::new(Some(outcome_tx)));

    let connect_result = agent_client_protocol::Client
        .builder()
        .on_receive_notification(
            {
                let note_tx = note_tx.clone();
                async move |notification: SessionNotification, _cx| {
                    // Forward the raw ACP `session/update` so the host can feed
                    // it into its own aggregation. Serialize just the `update`
                    // payload; the host reconstructs a `SessionUpdate` from it.
                    if let Ok(update) = serde_json::to_value(&notification.update) {
                        let _ = note_tx.send(frame(
                            methods::SESSION_UPDATE,
                            SessionUpdateParams { update },
                        ));
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let note_tx = note_tx.clone();
                let perms = perms.clone();
                async move |request: RequestPermissionRequest, responder, _connection| {
                    let request_id = format!(
                        "perm-{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos())
                            .unwrap_or(0)
                    );
                    let tool_name = request
                        .tool_call
                        .fields
                        .title
                        .as_deref()
                        .unwrap_or("unknown")
                        .to_string();
                    let options: Vec<PermissionOption> = request
                        .options
                        .iter()
                        .map(|opt| {
                            let kind = match opt.kind {
                                PermissionOptionKind::AllowOnce => "allow_once",
                                PermissionOptionKind::AllowAlways => "allow_always",
                                PermissionOptionKind::RejectOnce => "reject_once",
                                PermissionOptionKind::RejectAlways => "reject_always",
                                _ => "unknown",
                            };
                            PermissionOption {
                                option_id: opt.option_id.0.to_string(),
                                name: opt.name.clone(),
                                kind: kind.to_string(),
                            }
                        })
                        .collect();

                    let (reply_tx, reply_rx) = oneshot::channel::<Option<String>>();
                    perms
                        .lock()
                        .expect("permission registry poisoned")
                        .insert(request_id.clone(), reply_tx);

                    let _ = note_tx.send(frame(
                        methods::SESSION_PERMISSION_REQUEST,
                        SessionPermissionRequestParams {
                            request_id: request_id.clone(),
                            tool_name,
                            options,
                        },
                    ));

                    // Await the host's answer. A dropped sender (session tearing
                    // down, or an unanswered prompt) resolves to "cancelled".
                    match reply_rx.await {
                        Ok(Some(option_id)) => responder.respond(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                                option_id,
                            )),
                        )),
                        _ => {
                            // Make sure we don't leak the registry entry.
                            perms
                                .lock()
                                .expect("permission registry poisoned")
                                .remove(&request_id);
                            responder.respond(RequestPermissionResponse::new(
                                RequestPermissionOutcome::Cancelled,
                            ))
                        }
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(component, {
            let note_tx = note_tx.clone();
            let outcome_tx = outcome_tx.clone();
            move |connection: ConnectionTo<AcpAgentRole>| {
                let note_tx = note_tx.clone();
                let outcome_tx = outcome_tx.clone();
                let prompt_rx = prompt_rx;
                async move {
                    handshake_and_run(
                        connection,
                        cwd,
                        previous_session_id,
                        note_tx,
                        outcome_tx,
                        prompt_rx,
                    )
                    .await
                }
            }
        })
        .await;

    // If we never reported an outcome (connection setup failed), do so now.
    if let Some(tx) = outcome_tx.lock().expect("outcome lock poisoned").take() {
        let message = match &connect_result {
            Ok(()) => "session ended before it connected".to_string(),
            Err(e) => format!("connect: {e}"),
        };
        let _ = tx.send(StartOutcome::Failed { message });
    }

    // The session ended; tell the host the agent is gone.
    let _ = note_tx.send(frame(
        methods::SESSION_EXIT,
        SessionExitParams { code: None },
    ));
    drop(note_tx);
    let _ = forwarder.await;
}

/// The per-connection handshake and prompt loop. Reports the first resolved
/// outcome (connected / auth-required / failed) through `outcome_tx`, then, on
/// success, loops forwarding prompts until the channel closes.
async fn handshake_and_run(
    connection: ConnectionTo<AcpAgentRole>,
    cwd: std::path::PathBuf,
    previous_session_id: Option<String>,
    note_tx: tokio_mpsc::UnboundedSender<Vec<u8>>,
    outcome_tx: Arc<Mutex<Option<std_mpsc::Sender<StartOutcome>>>>,
    mut prompt_rx: tokio_mpsc::UnboundedReceiver<String>,
) -> Result<(), agent_client_protocol::Error> {
    let report = |outcome: StartOutcome| {
        if let Some(tx) = outcome_tx.lock().expect("outcome lock poisoned").take() {
            let _ = tx.send(outcome);
        }
    };

    let init_resp = connection
        .send_request(InitializeRequest::new(ProtocolVersion::V1))
        .block_task()
        .await?;

    // Authenticate up front if the agent advertises methods.
    if !init_resp.auth_methods.is_empty() {
        let method = &init_resp.auth_methods[0];
        let _ = note_tx.send(frame(
            methods::SESSION_OUTPUT,
            SessionOutputParams {
                line: format!(
                    "🔑 Authentication required: {} — {}",
                    method.name(),
                    method.description().unwrap_or("authenticating…"),
                ),
            },
        ));
        if let Err(err) = connection
            .send_request(AuthenticateRequest::new(method.id().clone()))
            .block_task()
            .await
        {
            let _ = note_tx.send(frame(
                methods::SESSION_ERROR,
                SessionErrorParams {
                    message: format!("Authentication failed: {err}"),
                },
            ));
            // Fall through to session creation; it will surface auth-required if
            // the agent still refuses.
        }
    }

    let caps = &init_resp.agent_capabilities.session_capabilities;
    let can_resume = caps.resume.is_some();
    let can_list = caps.list.is_some();
    let can_load = init_resp.agent_capabilities.load_session;

    // Resume an existing session when possible, else create a fresh one.
    let mut session_id: Option<String> = None;
    if let Some(ref prev) = previous_session_id {
        session_id =
            try_resume_specific(&connection, prev, &cwd, can_resume, can_load, &note_tx).await;
    }
    if session_id.is_none() && can_list && (can_resume || can_load) {
        session_id = try_find_and_resume(&connection, &cwd, can_resume, can_load, &note_tx).await;
    }

    let session_id: String = match session_id {
        Some(id) => id,
        None => {
            match connection
                .send_request(NewSessionRequest::new(cwd.clone()))
                .block_task()
                .await
            {
                Ok(resp) => resp.session_id.to_string(),
                Err(err) => {
                    let msg = format!("{err}");
                    if msg.contains("Authentication required") || msg.contains("auth") {
                        match init_resp
                            .auth_methods
                            .first()
                            .and_then(terminal_auth_command)
                        {
                            Some(command) => {
                                let _ = note_tx.send(frame(
                                    methods::SESSION_AUTH_REQUIRED,
                                    SessionAuthRequiredParams {
                                        command: command.clone(),
                                    },
                                ));
                                report(StartOutcome::AuthRequired { command });
                            }
                            None => {
                                let message = "agent required authentication but advertised no terminal login command".to_string();
                                let _ = note_tx.send(frame(
                                    methods::SESSION_ERROR,
                                    SessionErrorParams {
                                        message: message.clone(),
                                    },
                                ));
                                report(StartOutcome::Failed { message });
                            }
                        }
                    } else {
                        let message = format!("Session creation failed: {err}");
                        let _ = note_tx.send(frame(
                            methods::SESSION_ERROR,
                            SessionErrorParams {
                                message: message.clone(),
                            },
                        ));
                        report(StartOutcome::Failed { message });
                    }
                    return Ok(());
                }
            }
        }
    };

    // Announce readiness both to the sync `session.start` caller and to the host
    // event stream.
    report(StartOutcome::Connected {
        session_id: Some(session_id.clone()),
    });
    let _ = note_tx.send(frame(
        methods::SESSION_CONNECTED,
        SessionConnectedParams {
            session_id: Some(session_id.clone()),
        },
    ));

    // Prompt loop: forward host prompts to the agent, reporting each turn's
    // completion.
    while let Some(text) = prompt_rx.recv().await {
        let result = connection
            .send_request(PromptRequest::new(
                session_id.clone(),
                vec![ContentBlock::Text(TextContent::new(text))],
            ))
            .block_task()
            .await;
        match result {
            Ok(_) => {
                let _ = note_tx.send(frame(
                    methods::SESSION_PROMPT_RESULT,
                    SessionPromptResultParams {
                        ok: true,
                        error: None,
                    },
                ));
            }
            Err(err) => {
                let _ = note_tx.send(frame(
                    methods::SESSION_PROMPT_RESULT,
                    SessionPromptResultParams {
                        ok: false,
                        error: Some(format!("{err}")),
                    },
                ));
            }
        }
    }

    Ok(())
}

/// Try to rehydrate a specific session id via `session/resume` (preferred) or
/// `session/load`. Returns the id on success. Failures are reported to the host
/// as `session.output` and yield `None` so the caller can fall back.
async fn try_resume_specific(
    connection: &ConnectionTo<AcpAgentRole>,
    prev_id: &str,
    cwd: &std::path::Path,
    can_resume: bool,
    can_load: bool,
    note_tx: &tokio_mpsc::UnboundedSender<Vec<u8>>,
) -> Option<String> {
    let outcome = if can_resume {
        let _ = note_tx.send(frame(
            methods::SESSION_OUTPUT,
            SessionOutputParams {
                line: format!("Resuming session {prev_id}…"),
            },
        ));
        connection
            .send_request(ResumeSessionRequest::new(
                prev_id.to_string(),
                cwd.to_path_buf(),
            ))
            .block_task()
            .await
            .map(|_| ())
    } else if can_load {
        let _ = note_tx.send(frame(
            methods::SESSION_OUTPUT,
            SessionOutputParams {
                line: format!("Loading session {prev_id}…"),
            },
        ));
        connection
            .send_request(LoadSessionRequest::new(
                prev_id.to_string(),
                cwd.to_path_buf(),
            ))
            .block_task()
            .await
            .map(|_| ())
    } else {
        return None;
    };

    match outcome {
        Ok(()) => Some(prev_id.to_string()),
        Err(err) => {
            let _ = note_tx.send(frame(
                methods::SESSION_OUTPUT,
                SessionOutputParams {
                    line: format!("Resume failed ({err})."),
                },
            ));
            None
        }
    }
}

/// Ask the agent which sessions it has for `cwd` and resume the most recent.
async fn try_find_and_resume(
    connection: &ConnectionTo<AcpAgentRole>,
    cwd: &std::path::Path,
    can_resume: bool,
    can_load: bool,
    note_tx: &tokio_mpsc::UnboundedSender<Vec<u8>>,
) -> Option<String> {
    let sessions = match connection
        .send_request(ListSessionsRequest::new().cwd(cwd.to_path_buf()))
        .block_task()
        .await
    {
        Ok(resp) => resp.sessions,
        Err(_) => return None,
    };

    let best = sessions.into_iter().max_by(|a, b| {
        a.updated_at
            .as_deref()
            .unwrap_or("")
            .cmp(b.updated_at.as_deref().unwrap_or(""))
    })?;

    let _ = note_tx.send(frame(
        methods::SESSION_OUTPUT,
        SessionOutputParams {
            line: format!(
                "Found existing session {} — resuming…",
                best.title.as_deref().unwrap_or(&best.session_id.0),
            ),
        },
    ));

    let outcome = if can_resume {
        connection
            .send_request(ResumeSessionRequest::new(
                best.session_id.clone(),
                cwd.to_path_buf(),
            ))
            .block_task()
            .await
            .map(|_| ())
    } else if can_load {
        connection
            .send_request(LoadSessionRequest::new(
                best.session_id.clone(),
                cwd.to_path_buf(),
            ))
            .block_task()
            .await
            .map(|_| ())
    } else {
        return None;
    };

    match outcome {
        Ok(()) => Some(best.session_id.to_string()),
        Err(_) => None,
    }
}
