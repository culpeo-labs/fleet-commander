//! `fleet-commander init` command.
//!
//! Walks the user through workspace initialization:
//! 1. Select which ACP agent to use
//! 2. Scan subdirectories for `.devcontainer/` configs
//! 3. Confirm which projects to add
//! 4. Generate a base credential layer for the chosen agent
//! 5. Persist everything to workspaces.yaml

use std::path::{Path, PathBuf};

use anyhow::Result;
use dialoguer::{Confirm, Select};

use crate::agent::Agent;
use crate::agent_kind::AgentKind;
use crate::workspace;

/// Run the init command for the given workspace root.
pub fn run(workspace_root: &Path) -> Result<()> {
    let workspace_root = workspace_root.canonicalize()?;
    println!("Initializing workspace: {}", workspace_root.display());
    println!();

    // 1. Select agent.
    let agent_kind = select_agent()?;
    println!();
    println!("Using agent: {agent_kind}");
    println!();

    // 2. Scan for devcontainer projects.
    let projects = scan_projects(&workspace_root);
    if projects.is_empty() {
        anyhow::bail!(
            "No directories with .devcontainer/ found under {}.\n\
             Each project needs a .devcontainer/devcontainer.json to be managed by Fleet Commander.",
            workspace_root.display()
        );
    }

    // 3. Confirm which projects to add.
    let selected = confirm_projects(&projects)?;
    if selected.is_empty() {
        println!("No projects selected. Exiting.");
        return Ok(());
    }

    // 4. Generate base credential layer.
    generate_base_layer(agent_kind)?;

    // 5. Build agents and persist.
    let mut agents: Vec<Agent> = workspace::load()
        .into_iter()
        .map(|e| {
            Agent::new(e.path.to_string_lossy(), e.path.to_string_lossy())
                .with_acp_command(&e.acp_command)
                .with_workspace(&e.path)
        })
        .collect();

    for project in &selected {
        let name = project
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| project.display().to_string());

        // Skip if already registered.
        let path_str = project.display().to_string();
        if agents.iter().any(|a| a.id == path_str) {
            println!("  ⏭ {name} (already registered)");
            continue;
        }

        let agent = Agent::new(&path_str, &name)
            .with_acp_command(agent_kind.acp_command())
            .with_workspace(project);
        agents.push(agent);
        println!("  ✓ {name}");
    }

    workspace::save(&workspace::from_agents(&agents))
        .map_err(|e| anyhow::anyhow!(e))?;
    println!();
    println!("Workspace initialized with {} project(s).", selected.len());
    println!("Run `fleet-commander` to launch the TUI.");

    Ok(())
}

/// Interactive agent selection prompt.
fn select_agent() -> Result<AgentKind> {
    let items: Vec<&str> = AgentKind::ALL.iter().map(|k| k.display_name()).collect();

    if items.len() == 1 {
        // Only one option — just confirm it.
        println!("Agent: {} (only supported agent)", items[0]);
        return Ok(AgentKind::ALL[0]);
    }

    let selection = Select::new()
        .with_prompt("Which ACP agent should workspaces use?")
        .items(&items)
        .default(0)
        .interact()?;

    Ok(AgentKind::ALL[selection])
}

/// Scan a directory for subdirectories containing `.devcontainer/devcontainer.json`.
/// Also checks the root directory itself.
fn scan_projects(root: &Path) -> Vec<PathBuf> {
    let mut projects = Vec::new();

    // Check root itself.
    if root.join(".devcontainer/devcontainer.json").is_file() {
        projects.push(root.to_path_buf());
    }

    // Check immediate subdirectories.
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && !path.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.'))
                && path.join(".devcontainer/devcontainer.json").is_file()
            {
                projects.push(path);
            }
        }
    }

    projects.sort();
    projects
}

/// Ask the user to confirm each discovered project.
fn confirm_projects(projects: &[PathBuf]) -> Result<Vec<PathBuf>> {
    println!("Found {} project(s) with devcontainer configs:", projects.len());
    println!();

    let mut selected = Vec::new();
    for project in projects {
        let name = project
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| project.display().to_string());

        let add = Confirm::new()
            .with_prompt(format!("  Add {name}?"))
            .default(true)
            .interact()?;

        if add {
            selected.push(project.clone());
        }
    }

    Ok(selected)
}

/// Generate the base devcontainer credential layer for the selected agent.
fn generate_base_layer(agent_kind: AgentKind) -> Result<()> {
    let config_dir = fleet_commander_config_dir()?;
    std::fs::create_dir_all(&config_dir)?;

    let base_path = config_dir.join("base-devcontainer.json");

    let mut base = serde_json::Map::new();

    // Add credential environment variables.
    let cred_env = agent_kind.credential_env();
    if !cred_env.is_empty() {
        let env_obj: serde_json::Value = serde_json::to_value(&cred_env)?;
        base.insert("remoteEnv".to_string(), env_obj);
    }

    // Add credential mounts.
    let cred_mounts = agent_kind.credential_mounts();
    if !cred_mounts.is_empty() {
        let mounts_arr: serde_json::Value = serde_json::to_value(&cred_mounts)?;
        base.insert("mounts".to_string(), mounts_arr);
    }

    let json = serde_json::to_string_pretty(&base)?;
    std::fs::write(&base_path, &json)?;

    println!("Base credential layer written to {}", base_path.display());

    Ok(())
}

/// Path to fleet-commander's config directory.
pub fn fleet_commander_config_dir() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine config directory"))?
        .join("fleet-commander");
    Ok(dir)
}

/// Path to the base devcontainer layer, if it exists.
pub fn base_layer_path() -> Option<PathBuf> {
    let path = fleet_commander_config_dir().ok()?.join("base-devcontainer.json");
    if path.is_file() { Some(path) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn scan_finds_devcontainer_projects() {
        let tmp = TempDir::new().unwrap();
        let project_a = tmp.path().join("project-a/.devcontainer");
        let project_b = tmp.path().join("project-b/.devcontainer");
        let project_c = tmp.path().join("project-c"); // no devcontainer

        std::fs::create_dir_all(&project_a).unwrap();
        std::fs::write(project_a.join("devcontainer.json"), "{}").unwrap();
        std::fs::create_dir_all(&project_b).unwrap();
        std::fs::write(project_b.join("devcontainer.json"), "{}").unwrap();
        std::fs::create_dir_all(&project_c).unwrap();

        let projects = scan_projects(tmp.path());
        assert_eq!(projects.len(), 2);
        assert!(projects.iter().any(|p| p.ends_with("project-a")));
        assert!(projects.iter().any(|p| p.ends_with("project-b")));
    }

    #[test]
    fn scan_includes_root_if_has_devcontainer() {
        let tmp = TempDir::new().unwrap();
        let dc = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dc).unwrap();
        std::fs::write(dc.join("devcontainer.json"), "{}").unwrap();

        let projects = scan_projects(tmp.path());
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0], tmp.path());
    }

    #[test]
    fn scan_skips_hidden_dirs() {
        let tmp = TempDir::new().unwrap();
        let hidden = tmp.path().join(".hidden/.devcontainer");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::write(hidden.join("devcontainer.json"), "{}").unwrap();

        let projects = scan_projects(tmp.path());
        assert!(projects.is_empty());
    }

    #[test]
    fn generate_base_layer_creates_file() {
        let tmp = TempDir::new().unwrap();
        // Override config dir for test.
        let base_path = tmp.path().join("base-devcontainer.json");

        let mut base = serde_json::Map::new();
        let cred_env = AgentKind::Copilot.credential_env();
        base.insert("remoteEnv".to_string(), serde_json::to_value(&cred_env).unwrap());
        let cred_mounts = AgentKind::Copilot.credential_mounts();
        base.insert("mounts".to_string(), serde_json::to_value(&cred_mounts).unwrap());

        let json = serde_json::to_string_pretty(&base).unwrap();
        std::fs::write(&base_path, &json).unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&base_path).unwrap()).unwrap();
        assert!(parsed["remoteEnv"]["COPILOT_GITHUB_TOKEN"].is_string());
        assert!(parsed["mounts"].is_array());
    }
}
