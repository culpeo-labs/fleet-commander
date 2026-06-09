//! Persistent workspace registry.
//!
//! Stores opened workspaces in `~/.config/fleet-commander/workspaces.yaml`
//! so they reappear on next launch. Each workspace becomes a Copilot agent
//! in the TUI.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{info, debug};

use crate::agent::Agent;

/// A single workspace entry persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceEntry {
    /// Absolute path to the repo with (optional) `.devcontainer/` config.
    pub path: PathBuf,
    /// ACP command to launch (default: `copilot --acp --stdio`).
    #[serde(default = "default_acp_command")]
    pub acp_command: String,
}

/// Per-workspace runtime state, stored in the data directory alongside
/// the container's persistent data. This file survives container rebuilds
/// and app restarts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceState {
    /// Last-used ACP session ID — used to resume on reconnect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

fn default_acp_command() -> String {
    "copilot --acp --stdio".into()
}

/// The on-disk format.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceFile {
    #[serde(default)]
    pub workspaces: Vec<WorkspaceEntry>,
}

/// Where the file lives: `~/.config/fleet-commander/workspaces.yaml`.
pub fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("fleet-commander").join("workspaces.yaml"))
}

/// Load workspaces from disk.  Returns an empty list on any error.
pub fn load() -> Vec<WorkspaceEntry> {
    let Some(path) = config_path() else {
        return Vec::new();
    };
    load_from(&path)
}

fn load_from(path: &Path) -> Vec<WorkspaceEntry> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let file: WorkspaceFile = serde_yaml::from_str(&contents).unwrap_or_default();
    file.workspaces
}

/// Save workspaces to disk, creating parent dirs if needed.
pub fn save(entries: &[WorkspaceEntry]) -> Result<(), String> {
    let path = config_path().ok_or("Could not determine config directory")?;
    save_to(&path, entries)
}

/// Load runtime state for a workspace from its data directory.
pub fn load_state(workspace: &std::path::Path) -> WorkspaceState {
    let Some(data_dir) = fleet_commander_core::base_layer::workspace_data_dir(workspace) else {
        debug!(workspace = %workspace.display(), "No data dir for workspace");
        return WorkspaceState::default();
    };
    let state_path = data_dir.join("state.yaml");
    let Ok(contents) = std::fs::read_to_string(&state_path) else {
        debug!(path = %state_path.display(), "No state file found");
        return WorkspaceState::default();
    };
    let state: WorkspaceState = serde_yaml::from_str(&contents).unwrap_or_default();
    info!(
        workspace = %workspace.display(),
        session_id = ?state.session_id,
        "Loaded workspace state"
    );
    state
}

/// Save runtime state for a workspace to its data directory.
pub fn save_state(workspace: &std::path::Path, state: &WorkspaceState) -> Result<(), String> {
    let data_dir = fleet_commander_core::base_layer::workspace_data_dir(workspace)
        .ok_or("Could not determine data directory")?;
    std::fs::create_dir_all(&data_dir)
        .map_err(|e| format!("Failed to create data dir: {e}"))?;
    let state_path = data_dir.join("state.yaml");
    info!(
        path = %state_path.display(),
        session_id = ?state.session_id,
        "Saving workspace state"
    );
    let yaml = serde_yaml::to_string(state).map_err(|e| format!("Failed to serialize: {e}"))?;
    std::fs::write(&state_path, yaml)
        .map_err(|e| format!("Failed to write {}: {e}", state_path.display()))?;
    Ok(())
}

fn save_to(path: &Path, entries: &[WorkspaceEntry]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Failed to create config dir: {e}"))?;
    }
    let file = WorkspaceFile {
        workspaces: entries.to_vec(),
    };
    let yaml = serde_yaml::to_string(&file).map_err(|e| format!("Failed to serialize: {e}"))?;
    std::fs::write(path, yaml).map_err(|e| format!("Failed to write {}: {e}", path.display()))?;
    Ok(())
}

/// Convert workspace entries to Agent structs.
pub fn to_agents(entries: &[WorkspaceEntry]) -> Vec<Agent> {
    entries
        .iter()
        .map(|ws| {
            let dir_name = ws
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("workspace");
            let agent_id = format!("copilot-{dir_name}");
            // Load session_id from per-workspace data dir (not from workspaces.yaml).
            let state = load_state(&ws.path);
            let mut agent = Agent::new(&agent_id, format!("Copilot ({dir_name})"))
                .with_acp_command(&ws.acp_command)
                .with_workspace(&ws.path);
            agent.session_id = state.session_id;
            agent
        })
        .collect()
}

/// Build workspace entries from the current agent list (only agents with workspaces).
pub fn from_agents(agents: &[Agent]) -> Vec<WorkspaceEntry> {
    agents
        .iter()
        .filter_map(|a| {
            a.workspace_folder.as_ref().map(|ws| WorkspaceEntry {
                path: ws.clone(),
                acp_command: a.acp_command.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspaces.yaml");

        let entries = vec![
            WorkspaceEntry {
                path: PathBuf::from("/home/user/repo-a"),
                acp_command: "copilot --acp --stdio".into(),
            },
            WorkspaceEntry {
                path: PathBuf::from("/projects/repo-b"),
                acp_command: "claude-agent-acp".into(),
            },
        ];

        save_to(&path, &entries).unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].path, PathBuf::from("/home/user/repo-a"));
        assert_eq!(loaded[1].acp_command, "claude-agent-acp");
    }

    #[test]
    fn to_agents_creates_correct_ids() {
        let entries = vec![WorkspaceEntry {
            path: PathBuf::from("/home/user/my-cool-repo"),
            acp_command: "copilot --acp --stdio".into(),
        }];
        let agents = to_agents(&entries);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].id, "copilot-my-cool-repo");
        assert_eq!(agents[0].name, "Copilot (my-cool-repo)");
        assert_eq!(
            agents[0].workspace_folder,
            Some(PathBuf::from("/home/user/my-cool-repo"))
        );
    }

    #[test]
    fn from_agents_filters_workspaces_only() {
        let agents = vec![
            Agent::new("plain", "Plain Agent").with_acp_command("copilot --acp --stdio"),
            Agent::new("ws", "WS Agent")
                .with_acp_command("copilot --acp --stdio")
                .with_workspace("/repo"),
        ];
        let entries = from_agents(&agents);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, PathBuf::from("/repo"));
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let entries = load_from(Path::new("/nonexistent/workspaces.yaml"));
        assert!(entries.is_empty());
    }

    #[test]
    fn workspace_state_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.yaml");

        let state = WorkspaceState {
            session_id: Some("sess_abc123".into()),
        };
        let yaml = serde_yaml::to_string(&state).unwrap();
        std::fs::write(&state_path, &yaml).unwrap();

        let loaded: WorkspaceState =
            serde_yaml::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
        assert_eq!(loaded.session_id, Some("sess_abc123".into()));
    }

    #[test]
    fn workspace_state_defaults_when_missing() {
        let state = WorkspaceState::default();
        assert_eq!(state.session_id, None);
    }

    #[test]
    fn workspace_state_skips_none_in_yaml() {
        let state = WorkspaceState { session_id: None };
        let yaml = serde_yaml::to_string(&state).unwrap();
        assert!(!yaml.contains("session_id"));
    }

    #[test]
    fn from_agents_does_not_include_session_id() {
        let mut agent = Agent::new("ws", "WS")
            .with_acp_command("copilot --acp --stdio")
            .with_workspace("/repo");
        agent.session_id = Some("sess_xyz".into());

        let entries = from_agents(&[agent]);
        assert_eq!(entries.len(), 1);
        // WorkspaceEntry no longer has session_id — it's in state.yaml
        let yaml = serde_yaml::to_string(&entries[0]).unwrap();
        assert!(!yaml.contains("session_id"));
    }
}
