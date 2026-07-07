//! Host-side driver for a **daemon-owned** ACP session (Phase 4b2).
//!
//! Where the Phase 4a tunnel (`tunnel.rs`) forwarded raw ACP stdio and let the
//! host run the ACP `Client`, here the in-container `fleet-agent` owns the ACP
//! client and session. The host speaks the higher-level `session.*` protocol
//! (see `fleet-protocol`): it issues `session.start`, streams prompts as
//! `session.prompt` notifications, answers `session.permissionRequest`s, and
//! observes progress via `session.update`/`connected`/`output`/`error`/`exit`.
//!
//! Because the daemon owns the session, it survives a TUI exit/restart — the
//! original bug that motivated this work. The host still aggregates raw ACP
//! `session/update`s into its own [`SessionStateMachine`], so the TUI rendering
//! is unchanged.

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use agent_client_protocol::schema::v1::{AvailableCommandInput, SessionUpdate};
use fleet_protocol::{
    Notification, SearchDoneParams, SearchResultParams, SessionConnectedParams, SessionErrorParams,
    SessionExitParams, SessionOutputParams, SessionPermissionRequestParams,
    SessionPermissionRespondParams, SessionPromptParams, SessionPromptResultParams,
    SessionStartParams, SessionStartResult, SessionUpdateParams, methods,
};
use tokio::sync::mpsc;
use tracing::info;

use crate::agent_bin::CONTAINER_AGENT_PATH;
use crate::container::ContainerInfo;
use crate::service_fs::{NotificationSink, ProcessTransport, ServiceFs, Transport};
use crate::session::{AgentId, AvailableCommand, SessionEvent};
use crate::session_state::SessionStateMachine;
use crate::workspace_fs::WorkspaceFs;

use super::AcpLog;
use super::auth::build_auth_command;
use super::updates::apply_session_update;

/// Why the daemon-owned session driver could not run to completion.
pub(super) enum SessionRunError {
    /// The daemon does not advertise `capabilities.session`; the caller should
    /// fall back to the Phase 4a ACP tunnel.
    Unsupported,
    /// A fatal error (transport failure, `session.start` rejected, …).
    Fatal(Box<dyn std::error::Error + Send + Sync>),
}

/// Drive a daemon-owned ACP session for a container agent.
///
/// Connects a dedicated `fleet-agent` connection, verifies `capabilities.session`,
/// issues `session.start`, and — on success — runs the prompt loop forwarding
/// host prompts as `session.prompt` notifications until the prompt channel
/// closes or the daemon reports the agent exited. Progress notifications are
/// handled by the transport's notification sink.
///
/// On [`SessionRunError::Unsupported`] the prompt receiver is left untouched so
/// the caller can retry over the legacy tunnel.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
    agent_id: &AgentId,
    acp_command: &str,
    session_cwd: &Path,
    ci: &ContainerInfo,
    previous_session_id: Option<String>,
    prompt_rx: &mut mpsc::UnboundedReceiver<String>,
    event_tx: &mpsc::UnboundedSender<SessionEvent>,
    acp_log: Option<AcpLog>,
) -> Result<(), SessionRunError> {
    info!(agent_id = %agent_id, command = %acp_command, "Connecting to daemon-owned ACP session");

    // The host keeps aggregating raw ACP `session/update`s into its own state
    // machine so the TUI rendering matches the direct/tunnel paths.
    let state = Arc::new(Mutex::new(SessionStateMachine::new(
        agent_id.clone(),
        event_tx.clone(),
    )));

    // The sink runs on the transport's reader thread. To answer a permission
    // request it must send a `session.permissionRespond` notification back over
    // the transport *after* the user replies — but the transport does not exist
    // yet when we build the sink. Publish a `Weak` handle through this cell once
    // the transport is up. A `Weak` (not `Arc`) keeps the reader thread from
    // holding the transport alive, which would otherwise deadlock teardown
    // (Drop kills the child and joins the reader thread).
    let transport_cell: Arc<OnceLock<Weak<ProcessTransport>>> = Arc::new(OnceLock::new());
    let handle = tokio::runtime::Handle::current();
    // Signalled by the sink when the daemon reports the agent exited, so the
    // prompt loop unwinds and the caller emits a single `Exited` event.
    let exited = Arc::new(tokio::sync::Notify::new());

    let sink = build_sink(
        agent_id.clone(),
        ci.container_id.clone(),
        event_tx.clone(),
        state.clone(),
        handle.clone(),
        transport_cell.clone(),
        exited.clone(),
        acp_log,
    );

    let transport = match ProcessTransport::docker_exec_session(
        &ci.container_id,
        &ci.remote_user,
        CONTAINER_AGENT_PATH,
        sink,
    ) {
        Ok(Some(t)) => Arc::new(t),
        Ok(None) => return Err(SessionRunError::Unsupported),
        Err(e) => return Err(SessionRunError::Fatal(Box::new(e))),
    };
    let _ = transport_cell.set(Arc::downgrade(&transport));

    // Unification (Phase 4b2 y3): the explorer's filesystem now rides this
    // same bridge instead of opening its own `docker exec`. Build a `ServiceFs`
    // over a shared clone of the transport and hand it to the consumer. The
    // root label is the *host* workspace path so the App's `same_root` check
    // installs it over its initial `LocalFs`. `fs.watch` was already started by
    // `docker_exec_session`, so `fs.didChange` pushes reach the sink above.
    let fs_transport: Arc<dyn Transport> = transport.clone();
    let service_fs = ServiceFs::new(ci.workspace_folder.clone(), fs_transport);
    // Read the branch over the shared bridge before handing the fs off (a quick
    // blocking RPC, like `session.start` below). Emit it so the header/list row
    // reflect the container's branch — the same filesystem as the explorer.
    let branch = service_fs.git_branch();
    let fs: Arc<dyn WorkspaceFs> = Arc::new(service_fs);
    let _ = event_tx.send(SessionEvent::ExplorerFs {
        agent_id: agent_id.clone(),
        container_id: ci.container_id.clone(),
        fs,
    });
    let _ = event_tx.send(SessionEvent::AgentBranch {
        agent_id: agent_id.clone(),
        container_id: ci.container_id.clone(),
        branch,
    });

    // Start (or resume) the daemon-owned session. This blocks in the daemon
    // until the ACP handshake resolves; the returned result tells us whether a
    // session was established or interactive auth is required.
    let params = SessionStartParams {
        command: acp_command.to_string(),
        cwd: session_cwd.to_string_lossy().into_owned(),
        previous_session_id,
        env: Vec::new(),
    };
    let params = serde_json::to_value(params).expect("serialize session.start params");
    let result: SessionStartResult = transport
        .call(methods::SESSION_START, params)
        .map_err(|e| {
            SessionRunError::Fatal(Box::new(std::io::Error::other(format!(
                "session.start failed: {e}"
            ))))
        })
        .and_then(|value| {
            serde_json::from_value(value).map_err(|e| {
                SessionRunError::Fatal(Box::new(std::io::Error::other(format!(
                    "session.start bad result: {e}"
                ))))
            })
        })?;

    // Auth required: surface the terminal login command (wrapped for the
    // container) and let the main loop rerun with a fresh connection afterward.
    // `Connected` is emitted by the sink on `session.connected`, so nothing to
    // do here on the success path beyond running the prompt loop.
    if let Some(command) = result.auth_required {
        let ci_tuple = (
            ci.container_id.clone(),
            ci.remote_user.clone(),
            ci.remote_workspace_folder.clone(),
        );
        let auth_cmd = wrap_auth_command(&ci_tuple, command);
        let _ = event_tx.send(SessionEvent::AuthRequired {
            agent_id: agent_id.clone(),
            command: auth_cmd,
        });
        return Ok(());
    }

    // Prompt loop: forward host prompts as `session.prompt` notifications.
    // Completion arrives asynchronously as `session.promptResult` (handled by
    // the sink). Unwind when the prompt channel closes (TUI dropped the sender)
    // or the daemon reports the agent exited.
    loop {
        tokio::select! {
            maybe = prompt_rx.recv() => match maybe {
                Some(text) => {
                    let payload = serde_json::to_value(SessionPromptParams { text })
                        .expect("serialize session.prompt params");
                    if let Err(e) = transport.notify(methods::SESSION_PROMPT, payload) {
                        let _ = event_tx.send(SessionEvent::Error {
                            agent_id: agent_id.clone(),
                            message: format!("Failed to send prompt: {e}"),
                        });
                        break;
                    }
                }
                None => break,
            },
            _ = exited.notified() => break,
        }
    }

    Ok(())
}

/// Wrap the daemon's terminal login command (`[program, arg…]`, resolved inside
/// the container) with `docker exec -it` so it runs where copilot stores its
/// credentials.
fn wrap_auth_command(ci: &(String, String, String), command: Vec<String>) -> Vec<String> {
    let mut it = command.into_iter();
    let program = it.next().unwrap_or_default();
    let args: Vec<String> = it.collect();
    build_auth_command(Some(ci), &(program, args))
}

/// Build the notification sink that maps `session.*` notifications onto
/// [`SessionEvent`]s and the [`SessionStateMachine`].
#[allow(clippy::too_many_arguments)]
fn build_sink(
    agent_id: AgentId,
    container_id: String,
    event_tx: mpsc::UnboundedSender<SessionEvent>,
    state: Arc<Mutex<SessionStateMachine>>,
    handle: tokio::runtime::Handle,
    transport_cell: Arc<OnceLock<Weak<ProcessTransport>>>,
    exited: Arc<tokio::sync::Notify>,
    acp_log: Option<AcpLog>,
) -> NotificationSink {
    Box::new(move |note: Notification| match note.method.as_str() {
        methods::SESSION_UPDATE => {
            if let Some(params) = decode::<SessionUpdateParams>(&note) {
                if let Some(log) = &acp_log {
                    log_line(log, &agent_id, "<<", &params.update.to_string());
                }
                match serde_json::from_value::<SessionUpdate>(params.update) {
                    Ok(SessionUpdate::AvailableCommandsUpdate(upd)) => {
                        let commands = upd
                            .available_commands
                            .iter()
                            .map(|c| AvailableCommand {
                                name: c.name.clone(),
                                description: c.description.clone(),
                                hint: c.input.as_ref().and_then(|input| match input {
                                    AvailableCommandInput::Unstructured(u) => Some(u.hint.clone()),
                                    _ => None,
                                }),
                            })
                            .collect::<Vec<_>>();
                        let _ = event_tx.send(SessionEvent::AvailableCommands {
                            agent_id: agent_id.clone(),
                            commands,
                        });
                    }
                    Ok(update) => {
                        let mut sm = state.lock().expect("session state lock poisoned");
                        apply_session_update(&mut sm, &update);
                    }
                    Err(e) => {
                        let _ = event_tx.send(SessionEvent::Output {
                            agent_id: agent_id.clone(),
                            line: format!("  [session.update decode error] {e}"),
                        });
                    }
                }
            }
        }
        methods::SESSION_PERMISSION_REQUEST => {
            if let Some(params) = decode::<SessionPermissionRequestParams>(&note) {
                // `(option_id, display_name, kind_label)` — normalise the wire
                // kind (`allow_once`) to the display form (`allow once`) the UI
                // colours on.
                let options: Vec<(String, String, String)> = params
                    .options
                    .iter()
                    .map(|o| {
                        (
                            o.option_id.clone(),
                            o.name.clone(),
                            o.kind.replace('_', " "),
                        )
                    })
                    .collect();

                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<Option<String>>();
                let reply = Arc::new(Mutex::new(Some(reply_tx)));
                let _ = event_tx.send(SessionEvent::PermissionRequest {
                    agent_id: agent_id.clone(),
                    tool_name: params.tool_name.clone(),
                    options,
                    reply,
                });

                // Await the user's answer off the reader thread, then send the
                // response back over the transport.
                let request_id = params.request_id.clone();
                let cell = transport_cell.clone();
                handle.spawn(async move {
                    let option_id = reply_rx.await.ok().flatten();
                    if let Some(transport) = cell.get().and_then(Weak::upgrade) {
                        let payload = serde_json::to_value(SessionPermissionRespondParams {
                            request_id,
                            option_id,
                        })
                        .expect("serialize session.permissionRespond params");
                        let _ = transport.notify(methods::SESSION_PERMISSION_RESPOND, payload);
                    }
                });
            }
        }
        methods::SESSION_CONNECTED => {
            let session_id = decode::<SessionConnectedParams>(&note).and_then(|p| p.session_id);
            // Flush any replayed chunks (from a resumed/loaded session) into
            // history — the agent never sends an explicit turn-end for history.
            // A no-op for a fresh session.
            state
                .lock()
                .expect("session state lock poisoned")
                .prompt_complete();
            let _ = event_tx.send(SessionEvent::Connected {
                agent_id: agent_id.clone(),
                session_id,
            });
        }
        methods::SESSION_PROMPT_RESULT => {
            if let Some(params) = decode::<SessionPromptResultParams>(&note) {
                if params.ok {
                    state
                        .lock()
                        .expect("session state lock poisoned")
                        .prompt_complete();
                } else {
                    let msg = format!(
                        "Prompt error: {}",
                        params.error.as_deref().unwrap_or("unknown")
                    );
                    state
                        .lock()
                        .expect("session state lock poisoned")
                        .fail_active(&msg);
                    let _ = event_tx.send(SessionEvent::Error {
                        agent_id: agent_id.clone(),
                        message: msg,
                    });
                }
            }
        }
        methods::SESSION_OUTPUT => {
            if let Some(params) = decode::<SessionOutputParams>(&note) {
                let _ = event_tx.send(SessionEvent::Output {
                    agent_id: agent_id.clone(),
                    line: params.line,
                });
            }
        }
        methods::SESSION_ERROR => {
            if let Some(params) = decode::<SessionErrorParams>(&note) {
                let _ = event_tx.send(SessionEvent::Error {
                    agent_id: agent_id.clone(),
                    message: params.message,
                });
            }
        }
        methods::SESSION_EXIT => {
            // The daemon reported the ACP child exited. Wake the prompt loop so
            // the caller emits a single `Exited` event and tears the transport
            // down. `code` is currently discarded to match the tunnel path.
            let _ = decode::<SessionExitParams>(&note);
            exited.notify_one();
        }
        // Auth is surfaced from the synchronous `session.start` result; ignore
        // the mirrored notification to avoid a duplicate prompt.
        methods::SESSION_AUTH_REQUIRED => {}
        // Filesystem traffic on the shared bridge (Phase 4b2 y3). The explorer
        // `ServiceFs` rides this same connection, so its `fs.watch` pushes and
        // `fs.search` results arrive here too — route them to the consumer.
        methods::FS_DID_CHANGE => {
            let _ = event_tx.send(SessionEvent::ExplorerFsChanged {
                agent_id: agent_id.clone(),
                container_id: container_id.clone(),
            });
        }
        methods::FS_SEARCH_RESULT => {
            if let Some(params) = decode::<SearchResultParams>(&note) {
                let _ = event_tx.send(SessionEvent::SearchResults {
                    agent_id: agent_id.clone(),
                    search_id: params.search_id,
                    matches: params.matches,
                });
            }
        }
        methods::FS_SEARCH_DONE => {
            if let Some(params) = decode::<SearchDoneParams>(&note) {
                let _ = event_tx.send(SessionEvent::SearchDone {
                    agent_id: agent_id.clone(),
                    search_id: params.search_id,
                    summary: params.summary,
                });
            }
        }
        _ => {}
    })
}

/// Decode a notification's params into `T`, returning `None` on any mismatch.
fn decode<T: serde::de::DeserializeOwned>(note: &Notification) -> Option<T> {
    let params = note.params.clone()?;
    serde_json::from_value(params).ok()
}

/// Append one line to the ACP wire log (mirrors `tunnel.rs`/`connection.rs`).
fn log_line(log: &AcpLog, agent_id: &str, prefix: &str, line: &str) {
    if let Ok(mut file) = log.lock() {
        use std::io::Write;
        let _ = writeln!(file, "[{agent_id}] {prefix} {line}");
    }
}
