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

    let remote_user = parsed
        .get("remoteUser")
        .and_then(|v| v.as_str())
        .unwrap_or("root")
        .to_string();

    Ok(ContainerInfo {
        workspace_folder: config.workspace_folder.clone(),
        remote_workspace_folder: parsed
            .get("remoteWorkspaceFolder")
            .and_then(|v| v.as_str())
            .unwrap_or("/workspace")
            .to_string(),
        remote_user,
    })
}

/// Resolve a GitHub auth token from the host using a fallback chain:
/// 1. `gh auth token` — cleanest, uses the `gh` CLI credential store
/// 2. `~/.copilot/config.json` — reads the Copilot CLI's stored OAuth token
///
/// Returns `None` if neither source has a token.
fn resolve_host_github_token() -> Option<String> {
    // Try `gh auth token` first — clean subprocess, no file parsing.
    if let Some(output) = std::process::Command::new("gh")
        .args(["auth", "token"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
    {
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !token.is_empty() {
            return Some(token);
        }
    }

    // Fall back to reading ~/.copilot/config.json.
    let config_path = dirs::home_dir()?.join(".copilot").join("config.json");
    let contents = std::fs::read_to_string(config_path).ok()?;
    // Strip JS-style line comments that copilot puts in the file.
    let cleaned: String = contents
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let parsed: serde_json::Value = serde_json::from_str(&cleaned).ok()?;
    let tokens = parsed.get("copilotTokens")?.as_object()?;
    tokens.values().next()?.as_str().map(String::from)
}

/// Build the command string for running an ACP agent inside a dev container.
///
/// Resolves a GitHub token from the host (`gh auth token` or
/// `~/.copilot/config.json`) and passes it as `COPILOT_GITHUB_TOKEN` via
/// `--remote-env`. This env var has highest precedence in the Copilot CLI,
/// so agents authenticate without needing their own login inside the
/// container. Also forwards any explicit `GITHUB_TOKEN`/`GH_TOKEN` env vars.
pub fn build_exec_command(workspace_folder: &Path, acp_command: &str) -> String {
    let mut parts = vec![
        "devcontainer".to_string(),
        "exec".to_string(),
        "--workspace-folder".to_string(),
        workspace_folder.display().to_string(),
    ];

    // Forward host's GitHub token so container agents don't need separate login.
    if let Some(token) = resolve_host_github_token() {
        parts.push("--remote-env".to_string());
        parts.push(format!("COPILOT_GITHUB_TOKEN={token}"));
    }

    // Also forward explicit auth env vars if set.
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
    use std::sync::Mutex;

    // Tests that manipulate env vars must not run in parallel.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn build_exec_command_formats_correctly() {
        let _lock = ENV_LOCK.lock().unwrap();
        // SAFETY: tests using ENV_LOCK run sequentially; no other thread reads these vars.
        unsafe {
            std::env::remove_var("GITHUB_TOKEN");
            std::env::remove_var("GH_TOKEN");
        }
        let cmd = build_exec_command(
            &PathBuf::from("/home/user/my-repo"),
            "copilot --acp --stdio",
        );
        assert!(cmd.starts_with("devcontainer exec --workspace-folder /home/user/my-repo"));
        assert!(cmd.ends_with("copilot --acp --stdio"));
    }

    #[test]
    fn build_exec_command_with_claude() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("GITHUB_TOKEN");
            std::env::remove_var("GH_TOKEN");
        }
        let cmd = build_exec_command(&PathBuf::from("/projects/web-app"), "claude-agent-acp");
        assert!(cmd.starts_with("devcontainer exec --workspace-folder /projects/web-app"));
        assert!(cmd.ends_with("claude-agent-acp"));
    }

    #[test]
    fn build_exec_command_forwards_auth_env() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("GITHUB_TOKEN", "ghp_test123");
            std::env::remove_var("GH_TOKEN");
        }
        let cmd = build_exec_command(&PathBuf::from("/repo"), "copilot --acp --stdio");
        assert!(
            cmd.contains("--remote-env GITHUB_TOKEN=ghp_test123"),
            "Expected GITHUB_TOKEN in command: {cmd}"
        );
        assert!(cmd.starts_with("devcontainer exec --workspace-folder /repo"));
        assert!(cmd.ends_with("copilot --acp --stdio"));
        unsafe {
            std::env::remove_var("GITHUB_TOKEN");
        }
    }

    #[test]
    fn resolve_token_returns_none_without_sources() {
        // When neither `gh` is authed nor config.json exists, we get None.
        // This test just verifies it doesn't panic.
        let _ = resolve_host_github_token();
    }
}
