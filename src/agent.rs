//! Agent state held by the UI.
//!
//! Each agent wraps an ACP connection. The `acp_command` field specifies
//! what command to launch (e.g. "copilot --acp --stdio"). Once connected,
//! `prompt_tx` holds a channel for sending messages into the persistent
//! ACP session without respawning the process.

use tokio::sync::mpsc;

pub type AgentId = String;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    Idle,
    Running,
    Stopped,
    Error,
}

impl AgentStatus {
    pub fn label(&self) -> &'static str {
        match self {
            AgentStatus::Idle => "idle",
            AgentStatus::Running => "running",
            AgentStatus::Stopped => "stopped",
            AgentStatus::Error => "error",
        }
    }
}

pub struct Agent {
    pub id: AgentId,
    pub name: String,
    pub status: AgentStatus,
    pub history: Vec<String>,
    /// Command to launch the ACP agent (e.g. "copilot --acp --stdio").
    pub acp_command: String,
    /// Accumulates streaming deltas for the current assistant turn.
    pub pending_response: String,
    /// Channel for sending prompts to the persistent ACP connection.
    /// `None` until the connection is established.
    pub prompt_tx: Option<mpsc::UnboundedSender<String>>,
}

impl Agent {
    pub fn new(id: impl Into<AgentId>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            status: AgentStatus::Idle,
            history: Vec::new(),
            acp_command: String::new(),
            pending_response: String::new(),
            prompt_tx: None,
        }
    }

    pub fn with_acp_command(mut self, command: impl Into<String>) -> Self {
        self.acp_command = command.into();
        self
    }
}

/// Agent definitions used by the TUI. ACP connections are established
/// later by the agent runtime.
pub fn default_agents() -> Vec<Agent> {
    vec![
        Agent::new("copilot", "Copilot Agent")
            .with_acp_command("copilot --acp --stdio"),
        Agent::new("claude", "Claude Agent")
            .with_acp_command("claude-agent-acp"),
    ]
}
