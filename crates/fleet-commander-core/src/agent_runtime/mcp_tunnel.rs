//! Host side of the Feature 2 MCP tunnel.
//!
//! The in-container agent's MCP client reaches the host through the daemon: the
//! daemon opens a tunnel (`mcp.open`), relays the agent's MCP messages as
//! `mcp.data{tunnel_id, message}`, and tears it down with `mcp.close`. This
//! module bridges those tunnel frames onto an in-process duplex stream so the
//! host can run an ordinary MCP *server* (the TUI's `TuiMcpServer`) over it.
//!
//! For each tunnel we create a [`tokio::io::duplex`] pair, keep one half, and
//! run a pump between it and the tunnel:
//!   - **agent→host**: an `mcp.data` message is written (newline-delimited JSON)
//!     into our half so the server — reading the other half — sees a request;
//!   - **host→agent**: the server's newline-JSON responses (read from our half)
//!     are sent back to the agent as `mcp.data{tunnel_id, message}`.
//!
//! The other duplex half is handed to the caller, which serves an MCP server
//! over it. Closing a tunnel drops the feed, which EOFs the server's stream so
//! its `serve_server` task ends cleanly.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

/// Delivers one host→agent MCP message for a tunnel back to the in-container
/// relay (typically by sending an `mcp.data` notification over the transport).
pub(crate) type SendData = Arc<dyn Fn(u64, Value) + Send + Sync>;

/// Duplex buffer size — large enough that a single MCP message never blocks the
/// pump on backpressure in practice.
const DUPLEX_BUF: usize = 64 * 1024;

/// Registry of live host-side MCP tunnels, keyed by the daemon-assigned tunnel
/// id. Cheap to clone; the pump tasks run on the shared runtime `handle`.
pub(crate) struct McpTunnels {
    handle: Handle,
    send: SendData,
    feeds: Mutex<HashMap<u64, mpsc::UnboundedSender<Vec<u8>>>>,
}

impl McpTunnels {
    pub(crate) fn new(handle: Handle, send: SendData) -> Self {
        Self {
            handle,
            send,
            feeds: Mutex::new(HashMap::new()),
        }
    }

    /// Open a tunnel, returning the *server-side* duplex half to serve an MCP
    /// server over. Spawns the pump task bridging it to the tunnel frames.
    pub(crate) fn open(&self, tunnel_id: u64) -> DuplexStream {
        let (host_side, server_side) = tokio::io::duplex(DUPLEX_BUF);
        let (feed_tx, feed_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        self.feeds
            .lock()
            .expect("mcp tunnels poisoned")
            .insert(tunnel_id, feed_tx);
        let send = self.send.clone();
        self.handle.spawn(pump(tunnel_id, host_side, feed_rx, send));
        server_side
    }

    /// Feed an agent→host MCP message into the tunnel's server stream. A no-op
    /// for an unknown tunnel (e.g. one already closed).
    pub(crate) fn data(&self, tunnel_id: u64, message: Value) {
        let feeds = self.feeds.lock().expect("mcp tunnels poisoned");
        if let Some(tx) = feeds.get(&tunnel_id) {
            let mut bytes = serde_json::to_vec(&message).unwrap_or_default();
            bytes.push(b'\n');
            let _ = tx.send(bytes);
        }
    }

    /// Close a tunnel: drop the feed so the pump finishes and its write half is
    /// dropped, EOF-ing the served stream.
    pub(crate) fn close(&self, tunnel_id: u64) {
        self.feeds
            .lock()
            .expect("mcp tunnels poisoned")
            .remove(&tunnel_id);
    }
}

/// Bidirectional pump for one tunnel: agent→host bytes (from `feed_rx`) are
/// written into `stream`; the server's newline-JSON responses (read from
/// `stream`) are handed to `send` as host→agent messages. Ends when either side
/// closes.
async fn pump(
    tunnel_id: u64,
    stream: DuplexStream,
    mut feed_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    send: SendData,
) {
    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    loop {
        tokio::select! {
            maybe = feed_rx.recv() => match maybe {
                Some(bytes) => {
                    if write.write_all(&bytes).await.is_err() || write.flush().await.is_err() {
                        break;
                    }
                }
                // Tunnel closed by the daemon; stop so the write half drops and
                // the server's stream EOFs.
                None => break,
            },
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    if let Ok(value) = serde_json::from_str::<Value>(&line) {
                        (send)(tunnel_id, value);
                    }
                }
                // Server closed its side (or a read error): nothing left to pump.
                Ok(None) | Err(_) => break,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn tunnel_bridges_both_directions_and_closes() {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<(u64, Value)>();
        let send: SendData = Arc::new(move |id, v| {
            let _ = out_tx.send((id, v));
        });
        let tunnels = McpTunnels::new(Handle::current(), send);

        let server_side = tunnels.open(7);
        let (read, mut write) = tokio::io::split(server_side);
        let mut lines = BufReader::new(read).lines();

        // agent→host: `data` surfaces as a newline-JSON line on the server side.
        tunnels.data(
            7,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        );
        let line = lines.next_line().await.unwrap().expect("a request line");
        let req: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(req["method"], "tools/list");

        // host→agent: a response written by the server reaches `send`, stamped
        // with the tunnel id.
        write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n")
            .await
            .unwrap();
        let (id, msg) = out_rx.recv().await.unwrap();
        assert_eq!(id, 7);
        assert_eq!(msg["id"], 1);

        // Unknown tunnel is a no-op (does not panic, nothing delivered).
        tunnels.data(999, json!({ "x": 1 }));

        // Closing the tunnel EOFs the served stream.
        tunnels.close(7);
        assert!(lines.next_line().await.unwrap().is_none());
    }
}
