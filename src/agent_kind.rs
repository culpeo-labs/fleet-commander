//! Agent kind registry.
//!
//! Defines the supported ACP agent types. Each kind knows its command,
//! display name, and what credential environment variables it needs
//! injected into containers.

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

    /// Environment variables to inject into containers for authentication.
    /// Keys are env var names, values are devcontainer variable expressions
    /// (e.g. `${localEnv:VAR}`) that get expanded at container start.
    pub fn credential_env(self) -> HashMap<String, String> {
        let mut env = HashMap::new();
        match self {
            AgentKind::Copilot => {
                env.insert(
                    "COPILOT_GITHUB_TOKEN".to_string(),
                    "${localEnv:COPILOT_GITHUB_TOKEN}".to_string(),
                );
            }
        }
        env
    }

    /// Bind mounts to inject into containers for credential sharing.
    /// Each entry is a devcontainer mount string with variable expressions.
    pub fn credential_mounts(self) -> Vec<String> {
        match self {
            AgentKind::Copilot => vec![
                "source=${localEnv:HOME}/.copilot,target=/home/vscode/.copilot,type=bind,readonly".to_string(),
            ],
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
    fn copilot_credential_env() {
        let env = AgentKind::Copilot.credential_env();
        assert!(env.contains_key("COPILOT_GITHUB_TOKEN"));
    }

    #[test]
    fn copilot_credential_mounts() {
        let mounts = AgentKind::Copilot.credential_mounts();
        assert_eq!(mounts.len(), 1);
        assert!(mounts[0].contains(".copilot"));
    }

    #[test]
    fn all_agents_listed() {
        assert!(!AgentKind::ALL.is_empty());
    }
}
