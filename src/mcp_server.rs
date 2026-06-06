//! MCP server that agents connect to for pushing UI content.
//!
//! Exposes tools (`show_diff`, `show_file`, `notify`) over streamable HTTP.
//! Each tool call is translated into an [`AppEvent`] and sent through the
//! shared channel so the TUI reacts in its normal event loop.

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::router::tool::ToolRouter,
    model::*,
    schemars, tool, tool_handler, tool_router,
    handler::server::wrapper::Parameters,
};
use tokio::sync::mpsc;

use crate::event::AppEvent;

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

/// The MCP server handler. One instance is created per MCP session, but they
/// all share the same `tx` sender into the TUI event loop.
#[derive(Clone)]
pub struct TuiMcpServer {
    tx: Arc<mpsc::UnboundedSender<AppEvent>>,
    #[allow(dead_code)] // read by the rmcp tool_router macro infrastructure
    tool_router: ToolRouter<TuiMcpServer>,
}

impl TuiMcpServer {
    pub fn new(tx: mpsc::UnboundedSender<AppEvent>) -> Self {
        let tx = Arc::new(tx);
        Self {
            tx,
            tool_router: Self::tool_router(),
        }
    }
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
}

#[tool_handler]
impl ServerHandler for TuiMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("multi-agent-tui", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "MCP server for the multi-agent TUI. \
                 Tools: show_diff (display a diff), show_file (display a file), \
                 notify (send a message to an agent's conversation)."
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
        StreamableHttpServerConfig, StreamableHttpService,
        session::local::LocalSessionManager,
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
}
