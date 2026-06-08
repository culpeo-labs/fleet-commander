//! Persistent workspace registry.
//!
//! Stores opened workspaces in `~/.config/fleet-commander/workspaces.yaml`
//! so they reappear on next launch. Each workspace becomes a Copilot agent
//! in the TUI.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent::Agent;

/// A single workspace entry persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceEntry {
    /// Absolute path to the repo with (optional) `.devcontainer/` config.
    pub path: PathBuf,
    /// ACP command to launch (default: `copilot --acp --stdio`).
    #[serde(default = "default_acp_command")]
    pub acp_command: String,
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
            let mut agent = Agent::new(&agent_id, format!("Copilot ({dir_name})"))
                .with_acp_command(&ws.acp_command)
                .with_workspace(&ws.path);
            agent.session_id = ws.session_id.clone();
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
                session_id: a.session_id.clone(),
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
                session_id: None,
            },
            WorkspaceEntry {
                path: PathBuf::from("/projects/repo-b"),
                acp_command: "claude-agent-acp".into(),
                session_id: Some("sess_123".into()),
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
            session_id: None,
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
}
