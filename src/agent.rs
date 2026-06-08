//! Agent state held by the UI.
//!
//! Each agent wraps an ACP connection. The `acp_command` field specifies
//! what command to launch (e.g. "copilot --acp --stdio"). When `workspace_folder`
//! is set, the agent runs inside a dev container for that repo.

use std::path::PathBuf;

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
    /// Optional repo path with `.devcontainer/` config.
    /// When set, the agent runs inside a dev container.
    pub workspace_folder: Option<PathBuf>,
    /// Accumulates streaming deltas for the current assistant turn.
    pub pending_response: String,
    /// Accumulates thought chunks until the thought stream ends.
    pub pending_thought: String,
    /// Channel for sending prompts to the persistent ACP connection.
    /// `None` until the connection is established.
    pub prompt_tx: Option<mpsc::UnboundedSender<String>>,
    /// ACP session ID — persisted across reconnections for session resume.
    pub session_id: Option<String>,
}

impl Agent {
    pub fn new(id: impl Into<AgentId>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            status: AgentStatus::Idle,
            history: Vec::new(),
            acp_command: String::new(),
            workspace_folder: None,
            pending_response: String::new(),
            pending_thought: String::new(),
            prompt_tx: None,
            session_id: None,
        }
    }

    pub fn with_acp_command(mut self, command: impl Into<String>) -> Self {
        self.acp_command = command.into();
        self
    }

    #[allow(dead_code)] // Used when configuring agents with dev containers.
    pub fn with_workspace(mut self, path: impl Into<PathBuf>) -> Self {
        self.workspace_folder = Some(path.into());
        self
    }

    /// The effective command to run.
    ///
    /// When no workspace is set, returns the raw ACP command.
    /// When a workspace is set, the container is started separately by
    /// `agent_runtime` and this just returns the raw ACP command — the
    /// runtime handles exec via the container ID.
    pub fn effective_acp_command(&self) -> String {
        self.acp_command.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_command_without_workspace() {
        let agent = Agent::new("test", "Test").with_acp_command("copilot --acp --stdio");
        assert_eq!(agent.effective_acp_command(), "copilot --acp --stdio");
    }

    #[test]
    fn effective_command_with_workspace() {
        let agent = Agent::new("test", "Test")
            .with_acp_command("copilot --acp --stdio")
            .with_workspace("/home/user/my-repo");
        assert_eq!(agent.effective_acp_command(), "copilot --acp --stdio");
    }
}
