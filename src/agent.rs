//! Agent state held by the UI.
//!
//! Each agent wraps a Copilot SDK session. The `session` field is `None`
//! until the Copilot client is connected and sessions are created; the UI
//! works fine either way (placeholder or live).

use github_copilot_sdk::session::Session;
use std::sync::Arc;

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
    /// System prompt sent when creating the Copilot session.
    pub system_prompt: String,
    /// Live Copilot SDK session handle, if connected.
    pub session: Option<Arc<Session>>,
    /// Accumulates streaming deltas for the current assistant turn.
    pub pending_response: String,
}

impl Agent {
    pub fn new(id: impl Into<AgentId>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            status: AgentStatus::Idle,
            history: Vec::new(),
            system_prompt: String::new(),
            session: None,
            pending_response: String::new(),
        }
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }
}

/// Agent definitions used by the TUI. Sessions are attached later by the
/// agent runtime once the Copilot client is connected.
pub fn default_agents() -> Vec<Agent> {
    vec![
        Agent::new("reviewer", "Code reviewer").with_system_prompt(
            "You are a code reviewer. When asked, review code for bugs, \
             style issues, and suggest improvements. Be concise and actionable.",
        ),
        Agent::new("refactor", "Refactorer").with_system_prompt(
            "You are a refactoring assistant. Suggest and apply refactorings \
             to improve code quality, readability, and performance.",
        ),
        Agent::new("tester", "Test writer").with_system_prompt(
            "You are a test-writing assistant. Write comprehensive tests \
             for the code you are given. Prefer unit tests and cover edge cases.",
        ),
    ]
}
