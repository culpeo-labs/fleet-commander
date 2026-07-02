//! ACP-over-`fleet-agent` tunnel transport (Phase 4a).
//!
//! Instead of the host spawning `copilot --acp --stdio` directly (via
//! `docker exec`), the in-container `fleet-agent` daemon spawns and owns the
//! ACP child and tunnels its newline-delimited JSON stdio through a dedicated
//! `fleet-agent` connection. This module turns that tunnel into a
//! [`agent_client_protocol::Lines`] component so the existing ACP [`Client`]
//! builder drives the agent unchanged — only its transport differs.
//!
//! [`Client`]: agent_client_protocol::Client

use std::io;
use std::sync::Arc;

use agent_client_protocol::Lines;
use fleet_protocol::{AcpDataParams, Notification, methods};
use futures_channel::mpsc as fmpsc;
use tokio::sync::mpsc as tmpsc;

use crate::agent_bin::CONTAINER_AGENT_PATH;
use crate::container::ContainerInfo;
use crate::service_fs::{NotificationSink, ProcessTransport};
use crate::session::{AgentId, SessionEvent};

use super::AcpLog;

/// A boxed `Sink<String, Error = io::Error>` carrying outgoing ACP wire lines.
type OutgoingSink = std::pin::Pin<Box<dyn futures_util::Sink<String, Error = io::Error> + Send>>;

/// The [`Lines`] component representing one end of the ACP tunnel.
pub(super) type AcpTunnel = Lines<OutgoingSink, fmpsc::UnboundedReceiver<io::Result<String>>>;

/// Establish an ACP tunnel to the container's `fleet-agent`: open a dedicated
/// daemon connection, issue `acp.start` (spawning `acp_command` inside the
/// container), and return a [`Lines`] component wired to the tunnel.
///
/// Incoming `acp.recv` lines feed the component's stream; outgoing lines become
/// `acp.send` notifications. `acp.stderr` is surfaced to the operator and
/// `acp.exit` closes the stream so the ACP client observes EOF. When
/// `acp_log` is set, every tunnelled line is appended for protocol debugging.
pub(super) fn connect(
    ci: &ContainerInfo,
    acp_command: &str,
    agent_id: AgentId,
    event_tx: tmpsc::UnboundedSender<SessionEvent>,
    acp_log: Option<AcpLog>,
) -> io::Result<AcpTunnel> {
    let (inc_tx, inc_rx) = fmpsc::unbounded::<io::Result<String>>();

    let sink: NotificationSink = {
        let inc_tx = inc_tx.clone();
        let event_tx = event_tx.clone();
        let agent_id = agent_id.clone();
        let recv_log = acp_log.clone();
        Box::new(move |note: Notification| match note.method.as_str() {
            methods::ACP_RECV => {
                if let Some(data) = decode_data(&note) {
                    if let Some(log) = &recv_log {
                        log_line(log, &agent_id, "<<", &data);
                    }
                    let _ = inc_tx.unbounded_send(Ok(data));
                }
            }
            methods::ACP_STDERR => {
                if let Some(data) = decode_data(&note) {
                    let _ = event_tx.send(SessionEvent::Output {
                        agent_id: agent_id.clone(),
                        line: format!("  {data}"),
                    });
                }
            }
            methods::ACP_EXIT => {
                // Child gone → end the incoming stream so the ACP client sees
                // EOF and the connection unwinds.
                inc_tx.close_channel();
            }
            _ => {}
        })
    };

    let transport = ProcessTransport::docker_exec_acp(
        &ci.container_id,
        &ci.remote_user,
        CONTAINER_AGENT_PATH,
        &ci.remote_workspace_folder,
        acp_command,
        sink,
    )?;
    let transport = Arc::new(transport);

    let outgoing: OutgoingSink = Box::pin(futures_util::sink::unfold(
        (transport, agent_id, acp_log),
        move |(transport, agent_id, acp_log), line: String| async move {
            if let Some(log) = &acp_log {
                log_line(log, &agent_id, ">>", &line);
            }
            let params = serde_json::to_value(AcpDataParams { data: line })
                .map_err(|e| io::Error::other(format!("encode acp.send: {e}")))?;
            transport
                .notify(methods::ACP_SEND, params)
                .map_err(|e| io::Error::other(format!("acp.send: {e}")))?;
            Ok((transport, agent_id, acp_log))
        },
    ));

    Ok(Lines::new(outgoing, inc_rx))
}

/// Extract the `data` line from an `acp.recv`/`acp.stderr` notification.
fn decode_data(note: &Notification) -> Option<String> {
    let params = note.params.clone()?;
    serde_json::from_value::<AcpDataParams>(params)
        .ok()
        .map(|p| p.data)
}

/// Append one tunnelled line to the ACP wire log (mirrors the direct-spawn
/// `with_debug` logging so both transports produce comparable traces).
fn log_line(log: &AcpLog, agent_id: &str, prefix: &str, line: &str) {
    if let Ok(mut file) = log.lock() {
        use std::io::Write;
        let _ = writeln!(file, "[{agent_id}] {prefix} {line}");
    }
}
