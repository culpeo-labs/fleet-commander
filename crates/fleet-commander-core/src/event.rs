//! Events emitted by the ACP runtime to its consumer (typically a TUI).
//!
//! The runtime is decoupled from any specific frontend: it pushes
//! `RuntimeEvent`s onto an `mpsc::UnboundedSender` provided at startup and
//! lets the consumer translate them into application-level events.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub type AgentId = String;

/// A oneshot reply channel for permission responses, wrapped so events stay
/// `Clone` (the sender is taken once when the consumer responds).
pub type PermissionReply = Arc<Mutex<Option<tokio::sync::oneshot::Sender<Option<String>>>>>;

/// Execution status reported by the agent for a tool call. Mirrors the ACP
/// `ToolCallStatus` enum but lives here so events stay free of ACP types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallStatusKind {
    Pending,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum RuntimeEvent {
    AgentOutput {
        agent_id: AgentId,
        line: String,
    },
    AgentExited {
        agent_id: AgentId,
        code: Option<i32>,
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
    /// update). Identified by `tool_call_id`; consumers should upsert a
    /// single entry per id so completion replaces the running entry
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
    /// The consumer should suspend its UI and run the command interactively.
    AuthRequired {
        agent_id: AgentId,
        command: Vec<String>,
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

#[allow(dead_code)]
impl RuntimeEvent {
    /// The agent id this event is about.
    pub fn agent_id(&self) -> &AgentId {
        match self {
            RuntimeEvent::AgentOutput { agent_id, .. }
            | RuntimeEvent::AgentExited { agent_id, .. }
            | RuntimeEvent::AssistantDelta { agent_id, .. }
            | RuntimeEvent::ThoughtDelta { agent_id, .. }
            | RuntimeEvent::UserMessageDelta { agent_id, .. }
            | RuntimeEvent::AssistantDone { agent_id }
            | RuntimeEvent::SessionError { agent_id, .. }
            | RuntimeEvent::ToolCallUpdate { agent_id, .. }
            | RuntimeEvent::AgentConnected { agent_id, .. }
            | RuntimeEvent::AuthRequired { agent_id, .. }
            | RuntimeEvent::PermissionRequest { agent_id, .. } => agent_id,
        }
    }
}

/// Reference for what the runtime needs to know about an agent at startup.
/// Kept tiny to avoid leaking TUI-side state into the runtime crate.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AgentSpec {
    pub id: AgentId,
    pub acp_command: String,
    pub workspace_folder: Option<PathBuf>,
    pub previous_session_id: Option<String>,
}
