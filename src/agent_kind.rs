//! Agent kind registry.
//!
//! Defines the supported ACP agent types. Each kind knows its command,
//! display name, and what container environment it needs for in-container
//! authentication (device-flow login with plaintext token storage).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Supported ACP agent types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Copilot,
}

impl AgentKind {
    /// All available agent kinds.
    pub const ALL: &[AgentKind] = &[AgentKind::Copilot];

    /// Human-readable name shown in the selection prompt.
    pub fn display_name(self) -> &'static str {
        match self {
            AgentKind::Copilot => "GitHub Copilot",
        }
    }

    /// The ACP command to launch this agent.
    pub fn acp_command(self) -> &'static str {
        match self {
            AgentKind::Copilot => "copilot --acp --stdio",
        }
    }

    /// Environment variables to inject into containers.
    ///
    /// These are written into the base devcontainer layer's `containerEnv`
    /// so they are available for the in-container auth flow.
    pub fn container_env(self) -> HashMap<String, String> {
        match self {
            AgentKind::Copilot => HashMap::new(),
        }
    }
}

impl std::fmt::Display for AgentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copilot_has_acp_command() {
        assert_eq!(AgentKind::Copilot.acp_command(), "copilot --acp --stdio");
    }

    #[test]
    fn copilot_container_env() {
        // Should not panic; may be empty if no extra env is needed.
        let _env = AgentKind::Copilot.container_env();
    }

    #[test]
    fn all_agents_listed() {
        assert!(!AgentKind::ALL.is_empty());
    }
}
