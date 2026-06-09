//! The single event the application loop reacts to. Everything that wants
//! to nudge the UI (input, agent output, file changes) flows through this
//! enum into the main `select!` loop.
//!
//! Runtime-emitted events ([`fleet_commander_core::event::RuntimeEvent`])
//! are bridged into [`AppEvent`] via [`From`]; UI-only variants (`Input`,
//! `Change`, MCP server events, `ReconnectAgent`) live only here.

use crossterm::event::KeyEvent;

use fleet_commander_core::event::RuntimeEvent;

use crate::agent::AgentId;
use crate::change_source::ChangeEvent;

// Re-export the runtime contract types so the rest of the app keeps
// importing them via `crate::event::`.
pub use fleet_commander_core::event::{PermissionReply, ToolCallStatusKind};

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
    /// Streaming thought chunk from the ACP agent.
    ThoughtDelta {
        agent_id: AgentId,
        text: String,
    },
    /// Streaming text chunk for a historical user message (replayed during
    /// `session/load` or `session/resume`).
    UserMessageDelta {
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
    /// The ACP agent reported a tool call (initial registration or status
    /// update). Identified by `tool_call_id`; the app upserts a single
    /// history entry per id so completion replaces the running entry
    /// in-place instead of appending duplicates.
    ToolCallUpdate {
        agent_id: AgentId,
        tool_call_id: String,
        /// Set on initial registration; on later updates it's `Some` only
        /// when the agent renames the call.
        title: Option<String>,
        /// `None` when the agent only updated content/locations etc.
        status: Option<ToolCallStatusKind>,
    },
    /// ACP agent connected and session created.
    AgentConnected {
        agent_id: AgentId,
        /// The ACP session ID (for resume on reconnect).
        session_id: Option<String>,
    },
    /// The agent needs interactive authentication (e.g. `copilot login`).
    /// The main loop should suspend the TUI and run the command interactively.
    AuthRequired {
        agent_id: AgentId,
        command: Vec<String>,
    },
    /// Request to reconnect an agent (e.g. after container rebuild).
    ReconnectAgent {
        agent_id: AgentId,
    },
    /// The agent is requesting tool-use permission from the user.
    /// Send `Some(option_id)` to approve or `None` to cancel via the reply channel.
    PermissionRequest {
        agent_id: AgentId,
        tool_name: String,
        /// Human-readable option labels: Vec<(option_id, display_name, kind)>.
        options: Vec<(String, String, String)>,
        reply: PermissionReply,
    },
}

impl From<RuntimeEvent> for AppEvent {
    fn from(event: RuntimeEvent) -> Self {
        match event {
            RuntimeEvent::AgentOutput { agent_id, line } => {
                AppEvent::AgentOutput { agent_id, line }
            }
            RuntimeEvent::AgentExited { agent_id, code } => {
                AppEvent::AgentExited { agent_id, code }
            }
            RuntimeEvent::AssistantDelta { agent_id, text } => {
                AppEvent::AssistantDelta { agent_id, text }
            }
            RuntimeEvent::ThoughtDelta { agent_id, text } => {
                AppEvent::ThoughtDelta { agent_id, text }
            }
            RuntimeEvent::UserMessageDelta { agent_id, text } => {
                AppEvent::UserMessageDelta { agent_id, text }
            }
            RuntimeEvent::AssistantDone { agent_id } => AppEvent::AssistantDone { agent_id },
            RuntimeEvent::SessionError { agent_id, message } => {
                AppEvent::SessionError { agent_id, message }
            }
            RuntimeEvent::ToolCallUpdate {
                agent_id,
                tool_call_id,
                title,
                status,
            } => AppEvent::ToolCallUpdate {
                agent_id,
                tool_call_id,
                title,
                status,
            },
            RuntimeEvent::AgentConnected {
                agent_id,
                session_id,
            } => AppEvent::AgentConnected {
                agent_id,
                session_id,
            },
            RuntimeEvent::AuthRequired { agent_id, command } => {
                AppEvent::AuthRequired { agent_id, command }
            }
            RuntimeEvent::PermissionRequest {
                agent_id,
                tool_name,
                options,
                reply,
            } => AppEvent::PermissionRequest {
                agent_id,
                tool_name,
                options,
                reply,
            },
        }
    }
}
