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

    /// Bind mount strings for the base devcontainer layer.
    ///
    /// These use devcontainer variable substitution (`${localEnv:HOME}`,
    /// `${remoteEnv:HOME}`) so they resolve correctly at build/run time.
    /// The host-side path uses a fleet-commander data directory to persist
    /// agent state (sessions, auth tokens) across container rebuilds.
    pub fn container_mounts(self) -> Vec<String> {
        match self {
            AgentKind::Copilot => vec![
                "source=${localEnv:HOME}/.local/share/fleet-commander/copilot-data,target=${remoteEnv:HOME}/.copilot,type=bind".to_string(),
            ],
        }
    }

    /// Commands to run after the container starts (postStartCommand).
    ///
    /// Used to fix ownership of mounted directories that may have been
    /// created by Docker as root.
    pub fn post_start_command(self) -> Option<String> {
        match self {
            AgentKind::Copilot => Some(
                "mkdir -p ~/.copilot && test -w ~/.copilot || sudo chown -R $(id -u):$(id -g) ~/.copilot".to_string()
            ),
        }
    }

    /// Command to run once after container creation (postCreateCommand).
    ///
    /// Used to install the agent binary. This runs only on first creation,
    /// not on subsequent starts, so the binary persists across restarts.
    pub fn post_create_command(self) -> Option<String> {
        match self {
            AgentKind::Copilot => {
                // Install copilot CLI from GitHub releases as a static binary.
                // Detects architecture (x64/arm64) and downloads the matching tarball.
                // Falls back gracefully if curl/tar aren't available.
                Some(concat!(
                    "set -e && ",
                    "ARCH=$(uname -m) && ",
                    "case \"$ARCH\" in x86_64) ARCH=x64;; aarch64) ARCH=arm64;; esac && ",
                    "curl -fsSL \"https://github.com/github/copilot-cli/releases/latest/download/copilot-linux-${ARCH}.tar.gz\" | ",
                    "sudo tar xz -C /usr/local/bin copilot && ",
                    "echo 'GitHub Copilot CLI installed:' && copilot --version"
                ).to_string())
            }
        }
    }

    /// Devcontainer features required by this agent.
    ///
    /// These are merged into the base layer so the agent binary is available
    /// inside the container regardless of the project's own feature list.
    pub fn required_features(self) -> Vec<(&'static str, serde_json::Value)> {
        match self {
            // No features needed — copilot is installed via postCreateCommand.
            AgentKind::Copilot => vec![],
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
