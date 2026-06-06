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
            workspace_folder: None,
            pending_response: String::new(),
            prompt_tx: None,
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

    /// The effective command to run — wraps with `devcontainer exec` if
    /// a workspace folder is configured.
    pub fn effective_acp_command(&self) -> String {
        match &self.workspace_folder {
            Some(ws) => crate::container::build_exec_command(ws, &self.acp_command),
            None => self.acp_command.clone(),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_command_without_workspace() {
        let agent = Agent::new("test", "Test")
            .with_acp_command("copilot --acp --stdio");
        assert_eq!(agent.effective_acp_command(), "copilot --acp --stdio");
    }

    #[test]
    fn effective_command_with_workspace() {
        let agent = Agent::new("test", "Test")
            .with_acp_command("copilot --acp --stdio")
            .with_workspace("/home/user/my-repo");
        assert_eq!(
            agent.effective_acp_command(),
            "devcontainer exec --workspace-folder /home/user/my-repo copilot --acp --stdio"
        );
    }
}
