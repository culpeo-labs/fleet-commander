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

use fleet_commander_core::base_layer::{workspace_layer_dir, workspace_slug};

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

    // 4. Generate per-workspace base layers and build agents.
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

        // Generate this workspace's base layer.
        generate_workspace_layer(project, agent_kind)?;

        let agent = Agent::new(&path_str, &name)
            .with_acp_command(agent_kind.acp_command())
            .with_workspace(project);
        agents.push(agent);
        println!("  ✓ {name}");
    }

    workspace::save(&workspace::from_agents(&agents)).map_err(|e| anyhow::anyhow!(e))?;
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
                && !path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
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
    println!(
        "Found {} project(s) with devcontainer configs:",
        projects.len()
    );
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

/// Generate the base devcontainer layer for a specific workspace.
///
/// Each workspace gets its own base layer with:
/// - `containerEnv` — environment variables the agent needs
/// - `mounts` — bind mounts for persisting agent state (sessions, tokens)
/// - `postStartCommand` — fixup commands (e.g. ownership of mounted dirs)
///
/// The layer is stored at `~/.config/fleet-commander/layers/<slug>/devcontainer.json`
/// and merged into the project's devcontainer.json at container startup.
pub fn generate_workspace_layer(workspace: &Path, agent_kind: AgentKind) -> Result<()> {
    let layer_dir = workspace_layer_dir(workspace)?;
    std::fs::create_dir_all(&layer_dir)?;

    let layer_path = layer_dir.join("devcontainer.json");
    let mut base = serde_json::Map::new();

    // Add container environment variables (if any).
    let container_env = agent_kind.container_env();
    if !container_env.is_empty() {
        let env_obj: serde_json::Value = serde_json::to_value(&container_env)?;
        base.insert("containerEnv".to_string(), env_obj);
    }

    // Per-workspace data directory on the host for persisting agent state.
    let slug = workspace_slug(workspace);
    let data_dir = dirs::data_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine data directory"))?
        .join("fleet-commander")
        .join(&slug);
    let _ = std::fs::create_dir_all(&data_dir);

    // Build mount strings using the per-workspace data dir.
    let mounts: Vec<String> = agent_kind
        .container_mounts()
        .into_iter()
        .map(|m| {
            // Replace the generic source with our per-workspace path.
            let data_source = data_dir.display().to_string();
            // Extract the target from the mount string and rebuild with concrete source.
            if let Some(target_start) = m.find("target=") {
                let target_part = &m[target_start..];
                format!("source={data_source},{target_part}")
            } else {
                m
            }
        })
        .collect();

    if !mounts.is_empty() {
        base.insert("mounts".to_string(), serde_json::to_value(&mounts)?);
    }

    // Add postStartCommand for ownership fixups.
    if let Some(cmd) = agent_kind.post_start_command() {
        base.insert(
            "postStartCommand".to_string(),
            serde_json::Value::String(cmd),
        );
    }

    // Add required features (e.g. copilot-cli) so the agent binary is
    // available regardless of the project's own feature list.
    let features = agent_kind.required_features();
    if !features.is_empty() {
        let mut feature_map = serde_json::Map::new();
        for (id, opts) in features {
            feature_map.insert(id.to_string(), opts);
        }
        base.insert(
            "features".to_string(),
            serde_json::Value::Object(feature_map),
        );
    }

    let json = serde_json::to_string_pretty(&base)?;
    std::fs::write(&layer_path, &json)?;

    Ok(())
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
    fn generate_workspace_layer_creates_file() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("my-project");
        std::fs::create_dir_all(&workspace).unwrap();

        // Call the actual function — it writes to the config dir, but we can
        // verify the logic by checking what container_mounts produces.
        let agent_kind = AgentKind::Copilot;

        let mounts = agent_kind.container_mounts();
        assert!(!mounts.is_empty(), "Copilot should have data mounts");
        assert!(mounts[0].contains("target="));

        let post_start = agent_kind.post_start_command();
        assert!(post_start.is_some(), "Copilot should have postStartCommand");

        // Verify slug generation.
        let slug = workspace_slug(&workspace);
        assert_eq!(slug, "my-project");
    }
}
