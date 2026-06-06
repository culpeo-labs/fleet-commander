//! Agent state held by the UI. The actual process-spawning is intentionally
//! left as a follow-up; for now agents are populated as placeholder data so
//! the screen state machine and rendering can be exercised end-to-end.

pub type AgentId = String;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    Idle,
    Running,
    Stopped,
}

impl AgentStatus {
    pub fn label(&self) -> &'static str {
        match self {
            AgentStatus::Idle => "idle",
            AgentStatus::Running => "running",
            AgentStatus::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Agent {
    pub id: AgentId,
    pub name: String,
    pub status: AgentStatus,
    pub history: Vec<String>,
}

impl Agent {
    pub fn new(id: impl Into<AgentId>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            status: AgentStatus::Idle,
            history: Vec::new(),
        }
    }
}

/// Sample data used until real agent execution is wired up.
pub fn sample_agents() -> Vec<Agent> {
    vec![
        Agent {
            status: AgentStatus::Idle,
            history: vec!["Welcome — say `hello` to begin.".into()],
            ..Agent::new("reviewer", "Code reviewer")
        },
        Agent {
            status: AgentStatus::Running,
            history: vec!["Refactoring src/main.rs…".into()],
            ..Agent::new("refactor", "Refactorer")
        },
        Agent {
            status: AgentStatus::Idle,
            history: vec![],
            ..Agent::new("tester", "Test writer")
        },
    ]
}
