//! The single event the application loop reacts to. Everything that wants
//! to nudge the UI (input, agent output, file changes) flows through this
//! enum into the main `select!` loop.

use crossterm::event::KeyEvent;

use crate::agent::AgentId;
use crate::change_source::ChangeEvent;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum AppEvent {
    Input(KeyEvent),
    Change(ChangeEvent),
    AgentOutput {
        agent_id: AgentId,
        line: String,
    },
    AgentExited {
        agent_id: AgentId,
        code: Option<i32>,
    },
    /// An MCP client called the `show_diff` tool.
    McpShowDiff {
        agent_id: AgentId,
        path: std::path::PathBuf,
        content: String,
    },
    /// An MCP client called the `show_file` tool.
    McpShowFile {
        agent_id: AgentId,
        path: std::path::PathBuf,
        content: String,
    },
    /// An MCP client called the `notify` tool.
    McpNotify {
        agent_id: AgentId,
        message: String,
    },
    /// Streaming text delta from the Copilot SDK assistant.
    AssistantDelta {
        agent_id: AgentId,
        text: String,
    },
    /// The Copilot SDK assistant finished its response.
    AssistantDone {
        agent_id: AgentId,
    },
    /// A Copilot SDK session encountered an error.
    SessionError {
        agent_id: AgentId,
        message: String,
    },
    /// The Copilot SDK client failed to start.
    CopilotClientError {
        message: String,
    },
}
