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
    /// Streaming text chunk from the ACP agent.
    AssistantDelta {
        agent_id: AgentId,
        text: String,
    },
    /// The ACP agent finished its response (prompt completed).
    AssistantDone {
        agent_id: AgentId,
    },
    /// An ACP session or connection error.
    SessionError {
        agent_id: AgentId,
        message: String,
    },
    /// The ACP agent reported a tool call.
    ToolCallUpdate {
        agent_id: AgentId,
        tool_name: String,
        status: String,
    },
    /// ACP agent connected and session created.
    AgentConnected {
        agent_id: AgentId,
    },
}
