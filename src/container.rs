//! Dev container lifecycle management.
//!
//! Wraps the `@devcontainers/cli` to build, start, and execute commands
//! inside dev containers. Each agent can optionally run inside a container
//! built from a repo's `.devcontainer/` configuration.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

/// Configuration for running an agent inside a dev container.
#[derive(Debug, Clone)]
pub struct ContainerConfig {
    /// Path to the repository with `.devcontainer/` config.
    pub workspace_folder: PathBuf,
}

/// Result of `devcontainer up`.
#[derive(Debug)]
#[allow(dead_code)]
pub struct ContainerInfo {
    pub workspace_folder: PathBuf,
    pub remote_workspace_folder: String,
    pub remote_user: String,
}

/// Start a dev container for the given workspace.
///
/// Runs `devcontainer up --workspace-folder <path>` and parses the JSON output.
/// This may take a while on first run (image build + container creation).
pub async fn start_container(config: &ContainerConfig) -> Result<ContainerInfo, ContainerError> {
    let output = Command::new("devcontainer")
        .args([
            "up",
            "--workspace-folder",
            config.workspace_folder.to_str().unwrap_or("."),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| ContainerError::Spawn(format!("devcontainer up: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ContainerError::Start(stderr.to_string()));
    }

    // devcontainer up outputs JSON on the last line of stdout.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json_line = stdout
        .lines()
        .rev()
        .find(|line| line.starts_with('{'))
        .ok_or_else(|| ContainerError::Parse("No JSON in devcontainer up output".into()))?;

    let parsed: serde_json::Value =
        serde_json::from_str(json_line).map_err(|e| ContainerError::Parse(e.to_string()))?;

    let outcome = parsed.get("outcome").and_then(|v| v.as_str()).unwrap_or("");
    if outcome != "success" {
        return Err(ContainerError::Start(format!(
            "devcontainer up outcome: {outcome}"
        )));
    }

    Ok(ContainerInfo {
        workspace_folder: config.workspace_folder.clone(),
        remote_workspace_folder: parsed
            .get("remoteWorkspaceFolder")
            .and_then(|v| v.as_str())
            .unwrap_or("/workspace")
            .to_string(),
        remote_user: parsed
            .get("remoteUser")
            .and_then(|v| v.as_str())
            .unwrap_or("root")
            .to_string(),
    })
}

/// Build the command string for running an ACP agent inside a dev container.
///
/// Forwards GitHub auth env vars (`GITHUB_TOKEN`, `GH_TOKEN`) into the
/// container so that tools like `copilot` and `gh` can authenticate.
///
/// Returns a command like:
/// `devcontainer exec --workspace-folder /path/to/repo --remote-env GITHUB_TOKEN=... copilot --acp --stdio`
pub fn build_exec_command(workspace_folder: &Path, acp_command: &str) -> String {
    let mut parts = vec![
        "devcontainer".to_string(),
        "exec".to_string(),
        "--workspace-folder".to_string(),
        workspace_folder.display().to_string(),
    ];

    // Forward auth-related env vars so the agent can authenticate inside the container.
    for var in &["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(val) = std::env::var(var) {
            if val.is_empty() {
                continue;
            }
            parts.push("--remote-env".to_string());
            parts.push(format!("{var}={val}"));
        }
    }

    parts.push(acp_command.to_string());
    parts.join(" ")
}

#[derive(Debug, thiserror::Error)]
pub enum ContainerError {
    #[error("Failed to spawn devcontainer CLI: {0}")]
    Spawn(String),
    #[error("Container failed to start: {0}")]
    Start(String),
    #[error("Failed to parse devcontainer output: {0}")]
    Parse(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn build_exec_command_formats_correctly() {
        // Clear auth env vars so the test is deterministic.
        // SAFETY: test runs single-threaded; no other thread reads these vars.
        unsafe {
            std::env::remove_var("GITHUB_TOKEN");
            std::env::remove_var("GH_TOKEN");
        }
        let cmd = build_exec_command(
            &PathBuf::from("/home/user/my-repo"),
            "copilot --acp --stdio",
        );
        assert_eq!(
            cmd,
            "devcontainer exec --workspace-folder /home/user/my-repo copilot --acp --stdio"
        );
    }

    #[test]
    fn build_exec_command_with_claude() {
        unsafe {
            std::env::remove_var("GITHUB_TOKEN");
            std::env::remove_var("GH_TOKEN");
        }
        let cmd = build_exec_command(&PathBuf::from("/projects/web-app"), "claude-agent-acp");
        assert_eq!(
            cmd,
            "devcontainer exec --workspace-folder /projects/web-app claude-agent-acp"
        );
    }

    #[test]
    fn build_exec_command_forwards_auth_env() {
        unsafe {
            std::env::set_var("GITHUB_TOKEN", "ghp_test123");
            std::env::remove_var("GH_TOKEN");
        }
        let cmd = build_exec_command(&PathBuf::from("/repo"), "copilot --acp --stdio");
        assert!(cmd.contains("--remote-env GITHUB_TOKEN=ghp_test123"));
        assert!(cmd.starts_with("devcontainer exec --workspace-folder /repo"));
        assert!(cmd.ends_with("copilot --acp --stdio"));
        unsafe {
            std::env::remove_var("GITHUB_TOKEN");
        }
    }
}
