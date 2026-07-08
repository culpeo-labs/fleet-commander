//! MCP server that agents connect to for pushing UI content.
//!
//! Exposes tools (`show_diff`, `show_file`, `notify`) over streamable HTTP.
//! Each tool call is translated into an [`AppEvent`] and sent through the
//! shared channel so the TUI reacts in its normal event loop.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rmcp::{
    ErrorData as McpError, ServerHandler, handler::server::router::tool::ToolRouter,
    handler::server::wrapper::Parameters, model::*, schemars, tool, tool_handler, tool_router,
};
use tokio::sync::mpsc;

use crate::agent::AgentId;
use crate::event::AppEvent;
use crate::pairing::PairingStore;

/// Parameters for the `show_diff` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ShowDiffParams {
    /// Agent identifier (must match an agent known to the TUI).
    pub agent_id: String,
    /// File path the diff applies to.
    pub path: String,
    /// The diff or file content to display.
    pub content: String,
}

/// Parameters for the `show_file` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ShowFileParams {
    /// Agent identifier.
    pub agent_id: String,
    /// File path to display.
    pub path: String,
    /// Full file content (syntax-highlighted based on extension).
    pub content: String,
}

/// Parameters for the `notify` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct NotifyParams {
    /// Agent identifier.
    pub agent_id: String,
    /// Notification message to show in the agent's conversation.
    pub message: String,
}

/// Optional cross-workspace context (Feature 2). Present only when the server
/// is served over a per-agent MCP tunnel — the caller's identity is the agent
/// whose session opened the tunnel, so cross-workspace tools can be scoped to
/// it and filtered against the live [`PairingStore`]. Absent on the legacy
/// always-on HTTP server, where those tools are unavailable.
#[derive(Clone)]
struct CrossWorkspace {
    /// The agent that opened this tunnel (the tool caller).
    caller: AgentId,
    /// Live, shared pairing set — the same one the TUI's `:connect` commands
    /// mutate — so `list_connected` always reflects the current pairings.
    pairings: Arc<Mutex<PairingStore>>,
}

/// The MCP server handler. One instance is created per MCP session, but they
/// all share the same `tx` sender into the TUI event loop.
#[derive(Clone)]
pub struct TuiMcpServer {
    tx: Arc<mpsc::UnboundedSender<AppEvent>>,
    /// Cross-workspace scope, when served over a per-agent tunnel (Feature 2).
    cross_workspace: Option<CrossWorkspace>,
    /// Held so the `#[tool_router]` macro infrastructure can dispatch
    /// incoming tool calls. Not read directly by our code.
    #[allow(dead_code)]
    tool_router: ToolRouter<TuiMcpServer>,
}

impl TuiMcpServer {
    pub fn new(tx: mpsc::UnboundedSender<AppEvent>) -> Self {
        let tx = Arc::new(tx);
        Self {
            tx,
            cross_workspace: None,
            tool_router: Self::tool_router(),
        }
    }

    /// Construct a server scoped to a cross-workspace tunnel (Feature 2): the
    /// `caller` is the agent that opened the tunnel and `pairings` is the live
    /// shared set used to answer/authorize cross-workspace tool calls.
    pub fn for_tunnel(
        tx: mpsc::UnboundedSender<AppEvent>,
        caller: AgentId,
        pairings: Arc<Mutex<PairingStore>>,
    ) -> Self {
        let tx = Arc::new(tx);
        Self {
            tx,
            cross_workspace: Some(CrossWorkspace { caller, pairings }),
            tool_router: Self::tool_router(),
        }
    }
}

/// A connected peer as reported by `list_connected`.
#[derive(Debug, serde::Serialize)]
struct ConnectedPeer {
    /// The peer agent's id (pass this to `send_to_workspace`).
    id: AgentId,
    /// A friendly workspace name derived from the id.
    name: String,
}

/// Derive a friendly workspace name from an [`AgentId`] (`copilot-{dir}` → `dir`).
fn display_name(id: &str) -> String {
    id.strip_prefix("copilot-").unwrap_or(id).to_string()
}

#[tool_router]
impl TuiMcpServer {
    /// Show a diff in the TUI side pane for the given agent.
    #[tool(description = "Display a diff in the TUI side pane for a specific agent")]
    fn show_diff(
        &self,
        Parameters(params): Parameters<ShowDiffParams>,
    ) -> Result<CallToolResult, McpError> {
        self.tx
            .send(AppEvent::McpShowDiff {
                agent_id: params.agent_id,
                path: PathBuf::from(params.path),
                content: params.content,
            })
            .map_err(|_| McpError::internal_error("TUI event loop closed", None))?;
        Ok(CallToolResult::success(vec![Content::text(
            "diff displayed",
        )]))
    }

    /// Show a file with syntax highlighting in the TUI side pane.
    #[tool(description = "Display a file with syntax highlighting in the TUI side pane")]
    fn show_file(
        &self,
        Parameters(params): Parameters<ShowFileParams>,
    ) -> Result<CallToolResult, McpError> {
        self.tx
            .send(AppEvent::McpShowFile {
                agent_id: params.agent_id,
                path: PathBuf::from(params.path),
                content: params.content,
            })
            .map_err(|_| McpError::internal_error("TUI event loop closed", None))?;
        Ok(CallToolResult::success(vec![Content::text(
            "file displayed",
        )]))
    }

    /// Send a notification message to an agent's conversation history.
    #[tool(description = "Send a notification message to an agent's conversation in the TUI")]
    fn notify(
        &self,
        Parameters(params): Parameters<NotifyParams>,
    ) -> Result<CallToolResult, McpError> {
        self.tx
            .send(AppEvent::McpNotify {
                agent_id: params.agent_id,
                message: params.message,
            })
            .map_err(|_| McpError::internal_error("TUI event loop closed", None))?;
        Ok(CallToolResult::success(vec![Content::text("notified")]))
    }

    /// List the workspaces the calling agent is connected to (Feature 2).
    #[tool(
        description = "List the other workspaces this agent is connected to. Only connected \
                       workspaces can be messaged. Returns a JSON array of {id, name}."
    )]
    fn list_connected(&self) -> Result<CallToolResult, McpError> {
        let cw = self.cross_workspace.as_ref().ok_or_else(|| {
            McpError::invalid_request(
                "cross-workspace tools are not available on this connection",
                None,
            )
        })?;
        let peers = cw
            .pairings
            .lock()
            .map_err(|_| McpError::internal_error("pairing store poisoned", None))?
            .peers(&cw.caller);
        let list: Vec<ConnectedPeer> = peers
            .into_iter()
            .map(|id| {
                let name = display_name(&id);
                ConnectedPeer { id, name }
            })
            .collect();
        let json = serde_json::to_string(&list)
            .map_err(|e| McpError::internal_error(format!("serialize failed: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}

#[tool_handler]
impl ServerHandler for TuiMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "fleet-commander",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "MCP server for the multi-agent TUI. \
                 Tools: show_diff (display a diff), show_file (display a file), \
                 notify (send a message to an agent's conversation), \
                 list_connected (list connected workspaces for cross-workspace messaging)."
                    .to_string(),
            )
    }
}

/// Start the MCP streamable HTTP server on the given address.
/// Returns a `JoinHandle` that runs until the cancellation token is triggered.
pub async fn start_mcp_server(
    bind_addr: &str,
    tx: mpsc::UnboundedSender<AppEvent>,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    };

    let ct = tokio_util::sync::CancellationToken::new();
    let ct_clone = ct.clone();

    let service = StreamableHttpService::new(
        move || Ok(TuiMcpServer::new(tx.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token()),
    );

    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;

    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                ct_clone.cancelled().await;
            })
            .await;
    });

    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_diff_sends_event() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let server = TuiMcpServer::new(tx);
        let params = ShowDiffParams {
            agent_id: "test-agent".into(),
            path: "src/main.rs".into(),
            content: "+new line".into(),
        };
        let result = server.show_diff(Parameters(params));
        assert!(result.is_ok());

        let event = rx.try_recv().unwrap();
        match event {
            AppEvent::McpShowDiff {
                agent_id,
                path,
                content,
            } => {
                assert_eq!(agent_id, "test-agent");
                assert_eq!(path, PathBuf::from("src/main.rs"));
                assert_eq!(content, "+new line");
            }
            _ => panic!("expected McpShowDiff"),
        }
    }

    #[test]
    fn notify_sends_event() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let server = TuiMcpServer::new(tx);
        let params = NotifyParams {
            agent_id: "a1".into(),
            message: "hello world".into(),
        };
        let result = server.notify(Parameters(params));
        assert!(result.is_ok());

        let event = rx.try_recv().unwrap();
        match event {
            AppEvent::McpNotify { agent_id, message } => {
                assert_eq!(agent_id, "a1");
                assert_eq!(message, "hello world");
            }
            _ => panic!("expected McpNotify"),
        }
    }

    #[test]
    fn closed_channel_returns_error() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let server = TuiMcpServer::new(tx);
        let params = NotifyParams {
            agent_id: "a1".into(),
            message: "should fail".into(),
        };
        let result = server.notify(Parameters(params));
        assert!(result.is_err());
    }

    #[test]
    fn list_connected_requires_tunnel_scope() {
        let (tx, _rx) = mpsc::unbounded_channel();
        // The always-on HTTP server has no caller identity, so the tool errors.
        let server = TuiMcpServer::new(tx);
        assert!(server.list_connected().is_err());
    }

    #[test]
    fn list_connected_returns_paired_peers() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut store = PairingStore::default();
        store.connect("copilot-web", "copilot-feature");
        store.connect("copilot-feature", "copilot-docs");
        let pairings = Arc::new(Mutex::new(store));

        let server = TuiMcpServer::for_tunnel(tx, "copilot-feature".into(), pairings);
        let result = server.list_connected().expect("tool should succeed");

        // Extract the JSON payload from the tool result.
        let text = match &result.content[0].raw {
            RawContent::Text(t) => t.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        let peers: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
        let ids: Vec<&str> = peers.iter().map(|p| p["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["copilot-docs", "copilot-web"]);
        // Names are the id with the `copilot-` prefix stripped.
        assert_eq!(peers[0]["name"], "docs");
        assert_eq!(peers[1]["name"], "web");
    }
}
