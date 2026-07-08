//! MCP relay tunnel routing (Feature 2: cross-workspace connect).
//!
//! An in-container `fleet-agent mcp` relay (spawned by the coding agent as a
//! stdio MCP server) connects to the daemon socket as its own connection and
//! sends `mcp.bind{token}`. The daemon resolves the owning session (keyed by
//! cwd == token), assigns a tunnel id, and bridges the two connections:
//!
//! - relay → host: `mcp.data`/`mcp.close` frames from the relay connection are
//!   forwarded to the session's attached host, stamped with the tunnel id;
//! - host → relay: `mcp.data`/`mcp.close` frames the host sends on its session
//!   connection are routed back to the relay connection via [`McpTunnels`].
//!
//! Tunnel frames are sent "live" (never buffered into the session's replay
//! history) since they are ephemeral and tunnel-scoped.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use fleet_protocol::{McpBindParams, McpDataParams, McpTunnelParams, Notification, methods};
use serde::Serialize;

use crate::session::{SessionRegistry, SharedSession};

/// Daemon-scoped registry mapping a tunnel id to the relay connection's
/// outbound frame channel, so a host connection can route host→agent frames
/// back to the matching in-container relay. Cloned into every connection's
/// [`Server`](crate::Server) via [`DaemonState`](crate::DaemonState).
#[derive(Clone, Default)]
pub struct McpTunnels {
    inner: Arc<Mutex<HashMap<u64, mpsc::Sender<Vec<u8>>>>>,
    next: Arc<AtomicU64>,
}

impl McpTunnels {
    pub fn new() -> Self {
        Self::default()
    }

    fn register(&self, out: mpsc::Sender<Vec<u8>>) -> u64 {
        // Start ids at 1 so 0 stays a reserved "unset" sentinel on the
        // agent↔daemon hop.
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.inner
            .lock()
            .expect("mcp tunnels poisoned")
            .insert(id, out);
        id
    }

    fn remove(&self, id: u64) {
        self.inner.lock().expect("mcp tunnels poisoned").remove(&id);
    }

    fn get(&self, id: u64) -> Option<mpsc::Sender<Vec<u8>>> {
        self.inner
            .lock()
            .expect("mcp tunnels poisoned")
            .get(&id)
            .cloned()
    }
}

fn frame(method: &str, params: impl Serialize) -> Vec<u8> {
    serde_json::to_vec(&Notification::new(method, params)).unwrap_or_default()
}

/// Per-connection state for a bound relay connection: the tunnel id and the
/// session whose host it bridges to. Dropping it tears the tunnel down —
/// notifying the host with `mcp.close` and unregistering — so a relay
/// disconnect always propagates.
pub struct McpConn {
    tunnel_id: u64,
    session: Arc<SharedSession>,
    tunnels: McpTunnels,
}

impl Drop for McpConn {
    fn drop(&mut self) {
        self.session.send_live(frame(
            methods::MCP_CLOSE,
            McpTunnelParams {
                tunnel_id: self.tunnel_id,
            },
        ));
        self.tunnels.remove(self.tunnel_id);
    }
}

/// Handle an `mcp.bind` from a relay connection: resolve the owning session by
/// token (== session cwd), allocate a tunnel, notify the host with `mcp.open`,
/// and return the connection's tunnel state. Returns `None` (leaving the relay
/// unbound, so its connection soon closes) when no live session matches.
pub fn handle_mcp_bind(
    note: &Notification,
    sessions: &SessionRegistry,
    tunnels: &McpTunnels,
    out: &mpsc::Sender<Vec<u8>>,
) -> Option<McpConn> {
    let params: McpBindParams = note
        .params
        .clone()
        .and_then(|p| serde_json::from_value(p).ok())?;
    let session = {
        let reg = sessions.lock().expect("session registry poisoned");
        let session = reg.get(&params.token)?;
        if !session.is_alive() {
            return None;
        }
        session.clone()
    };
    let tunnel_id = tunnels.register(out.clone());
    session.send_live(frame(methods::MCP_OPEN, McpTunnelParams { tunnel_id }));
    Some(McpConn {
        tunnel_id,
        session,
        tunnels: tunnels.clone(),
    })
}

/// Forward an `mcp.data` from the relay to the session's host (agent → host),
/// stamping the daemon-assigned tunnel id so the host can demultiplex.
pub fn handle_mcp_relay_data(note: &Notification, conn: &McpConn) {
    let Some(params) = note
        .params
        .clone()
        .and_then(|p| serde_json::from_value::<McpDataParams>(p).ok())
    else {
        return;
    };
    conn.session.send_live(frame(
        methods::MCP_DATA,
        McpDataParams {
            tunnel_id: conn.tunnel_id,
            message: params.message,
        },
    ));
}

/// Route an `mcp.data` the host sent to the matching relay (host → agent). The
/// tunnel id is unused on the agent↔daemon hop, so it is reset to 0.
pub fn handle_mcp_host_data(note: &Notification, tunnels: &McpTunnels) {
    let Some(params) = note
        .params
        .clone()
        .and_then(|p| serde_json::from_value::<McpDataParams>(p).ok())
    else {
        return;
    };
    if let Some(out) = tunnels.get(params.tunnel_id) {
        let _ = out.send(frame(
            methods::MCP_DATA,
            McpDataParams {
                tunnel_id: 0,
                message: params.message,
            },
        ));
    }
}

/// Route an `mcp.close` the host sent to the matching relay and unregister the
/// tunnel (host → agent teardown).
pub fn handle_mcp_host_close(note: &Notification, tunnels: &McpTunnels) {
    let Some(params) = note
        .params
        .clone()
        .and_then(|p| serde_json::from_value::<McpTunnelParams>(p).ok())
    else {
        return;
    };
    if let Some(out) = tunnels.get(params.tunnel_id) {
        let _ = out.send(frame(methods::MCP_CLOSE, McpTunnelParams { tunnel_id: 0 }));
        tunnels.remove(params.tunnel_id);
    }
}
