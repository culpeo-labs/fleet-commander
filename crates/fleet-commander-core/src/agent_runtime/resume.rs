//! Session rehydration via `session/resume` and `session/load`.

use std::path::Path;

use agent_client_protocol::schema::{
    ListSessionsRequest, LoadSessionRequest, ResumeSessionRequest,
};
use agent_client_protocol::{Agent as AcpAgentRole, ConnectionTo};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::session::SessionEvent;

/// Try to rehydrate a specific session by id. Returns the id on success,
/// `None` on any failure (including the agent reporting that it no longer
/// has the session in its store). All failures are logged at WARN level
/// and a user-facing message is sent to the TUI so the operator can see why
/// the saved session couldn't be restored.
pub(super) async fn try_resume_specific(
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
pub(super) async fn try_find_and_resume(
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
