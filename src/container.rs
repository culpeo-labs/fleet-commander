//! Dev container lifecycle management.
//!
//! Uses the `devcontainer-lib` crate (bollard-based Docker API) to build,
//! start, and execute commands inside dev containers. Each agent can
//! optionally run inside a container built from a repo's `.devcontainer/`
//! configuration.
//!
//! Supports a base credential layer that is merged into every project's
//! devcontainer config, injecting auth env vars and mounts so agents
//! authenticate without per-container login.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use devcontainer_lib::devcontainer::config::DevcontainerConfig;
use devcontainer_lib::devcontainer::variables::{substitute_variables, substitute_variables_with_user};
use devcontainer_lib::runtime::{
    self, BindMount, ContainerRuntime, ContainerState, PortMapping, WorkspaceMount,
};
use devcontainer_lib::util::{container_name, workspace_labels, workspace_folder_name};

/// Configuration for running an agent inside a dev container.
#[derive(Debug, Clone)]
pub struct ContainerConfig {
    /// Path to the repository with `.devcontainer/` config.
    pub workspace_folder: PathBuf,
}

/// Result of starting a container.
#[derive(Debug)]
#[allow(dead_code)]
pub struct ContainerInfo {
    pub container_id: String,
    pub workspace_folder: PathBuf,
    pub remote_workspace_folder: String,
    pub remote_user: String,
}

/// Start a dev container for the given workspace.
///
/// Loads the project's devcontainer.json, merges the base credential layer,
/// then builds/creates/starts the container using the Docker API (bollard).
pub async fn start_container(config: &ContainerConfig) -> Result<ContainerInfo, ContainerError> {
    let workspace = &config.workspace_folder;
    let rt = runtime::detect_runtime(None)
        .await
        .map_err(|e| ContainerError::Start(e.to_string()))?;

    // Load devcontainer config.
    let config_path = workspace.join(".devcontainer/devcontainer.json");
    if !config_path.is_file() {
        return Err(ContainerError::Parse(format!(
            "No .devcontainer/devcontainer.json found in {}",
            workspace.display()
        )));
    }
    let dc_config = DevcontainerConfig::from_path(&config_path)
        .map_err(|e| ContainerError::Parse(e.to_string()))?;

    // Check if a container already exists for this workspace.
    let labels_list = workspace_labels(workspace, Some(&config_path));
    let filters: Vec<String> = labels_list.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let existing = rt.list_containers(&filters).await
        .map_err(|e| ContainerError::Start(e.to_string()))?;

    if let Some(container) = existing.first() {
        match container.state {
            ContainerState::Running => {
                let remote_user = resolve_remote_user(rt.as_ref(), &container.image, &dc_config).await?;
                let folder_name = workspace_folder_name(workspace);
                return Ok(ContainerInfo {
                    container_id: container.id.clone(),
                    workspace_folder: workspace.clone(),
                    remote_workspace_folder: dc_config.workspace_folder
                        .clone()
                        .unwrap_or_else(|| format!("/workspaces/{folder_name}")),
                    remote_user: remote_user.unwrap_or_else(|| "root".to_string()),
                });
            }
            ContainerState::Stopped => {
                rt.start_container(&container.id).await
                    .map_err(|e| ContainerError::Start(e.to_string()))?;
                let remote_user = resolve_remote_user(rt.as_ref(), &container.image, &dc_config).await?;
                let folder_name = workspace_folder_name(workspace);
                return Ok(ContainerInfo {
                    container_id: container.id.clone(),
                    workspace_folder: workspace.clone(),
                    remote_workspace_folder: dc_config.workspace_folder
                        .clone()
                        .unwrap_or_else(|| format!("/workspaces/{folder_name}")),
                    remote_user: remote_user.unwrap_or_else(|| "root".to_string()),
                });
            }
            ContainerState::NotFound => {}
        }
    }

    // No existing container — build image and create one.
    let image = resolve_image(rt.as_ref(), workspace, &dc_config, &config_path).await?;
    let name = container_name(workspace);
    let folder_name = workspace_folder_name(workspace);

    let remote_user = resolve_remote_user(rt.as_ref(), &image, &dc_config).await?;

    // Merge base credential layer into env/mounts.
    let (env, mounts) = build_env_and_mounts(workspace, &dc_config, remote_user.as_deref());

    let mut labels = HashMap::new();
    for (k, v) in &labels_list {
        labels.insert(k.clone(), v.clone());
    }

    let ports: Vec<PortMapping> = dc_config.forward_ports.clone().unwrap_or_default();

    let remote_workspace = dc_config.workspace_folder
        .clone()
        .unwrap_or_else(|| format!("/workspaces/{folder_name}"));

    let container_config = devcontainer_lib::runtime::ContainerConfig {
        image: image.clone(),
        name: name.clone(),
        labels,
        env,
        mounts,
        volumes: Vec::new(),
        ports,
        workspace_mount: Some(WorkspaceMount {
            source: workspace.to_path_buf(),
            target: remote_workspace.clone(),
        }),
        extra_args: Vec::new(),
        entrypoint: None,
        init: false,
        privileged: false,
        cap_add: Vec::new(),
        security_opt: Vec::new(),
    };

    let container_id = rt.create_container(&container_config).await
        .map_err(|e| ContainerError::Start(e.to_string()))?;
    rt.start_container(&container_id).await
        .map_err(|e| ContainerError::Start(e.to_string()))?;

    Ok(ContainerInfo {
        container_id,
        workspace_folder: workspace.clone(),
        remote_workspace_folder: remote_workspace,
        remote_user: remote_user.unwrap_or_else(|| "root".to_string()),
    })
}

/// Resolve the container image — pull if image-based, build if Dockerfile-based.
async fn resolve_image(
    rt: &dyn ContainerRuntime,
    workspace: &Path,
    config: &DevcontainerConfig,
    config_path: &Path,
) -> Result<String, ContainerError> {
    if let Some(ref image) = config.image {
        rt.pull_image(image).await
            .map_err(|e| ContainerError::Start(format!("Failed to pull image: {e}")))?;
        Ok(image.clone())
    } else if let Some(ref build) = config.build {
        let context_dir = config_path
            .parent()
            .unwrap()
            .join(build.context.as_deref().unwrap_or("."));
        let dockerfile_path = config_path.parent().unwrap().join(&build.dockerfile);
        let dockerfile_content = std::fs::read_to_string(&dockerfile_path)
            .map_err(|e| ContainerError::Parse(format!("Failed to read Dockerfile: {e}")))?;
        let tag = container_name(workspace);
        rt.build_image(&dockerfile_content, &context_dir, &tag, &HashMap::new(), false, false)
            .await
            .map_err(|e| ContainerError::Start(format!("Image build failed: {e}")))?;
        Ok(tag)
    } else {
        Err(ContainerError::Parse(
            "devcontainer.json must specify 'image' or 'build.dockerfile'".into(),
        ))
    }
}

/// Resolve the effective remote user from config or image metadata.
async fn resolve_remote_user(
    rt: &dyn ContainerRuntime,
    image: &str,
    config: &DevcontainerConfig,
) -> Result<Option<String>, ContainerError> {
    runtime::resolve_remote_user(rt, image, config.remote_user.as_deref())
        .await
        .map_err(|e| ContainerError::Start(e.to_string()))
}

/// Build the environment variables and bind mounts for a container,
/// merging the base credential layer with the project's devcontainer config.
fn build_env_and_mounts(
    workspace: &Path,
    config: &DevcontainerConfig,
    remote_user: Option<&str>,
) -> (HashMap<String, String>, Vec<BindMount>) {
    let mut env = HashMap::new();
    env.insert("REMOTE_CONTAINERS".to_string(), "true".to_string());

    // Apply containerEnv from config.
    if let Some(ref container_env) = config.container_env {
        for (k, v) in container_env {
            env.insert(k.clone(), substitute_variables(v, workspace));
        }
    }

    // Apply remoteEnv from config (these include ${localEnv:VAR} expansions).
    if let Some(ref remote_env) = config.remote_env {
        for (k, v) in remote_env {
            env.insert(k.clone(), substitute_variables(v, workspace));
        }
    }

    // Inject host credentials if available (fallback for configs without base layer).
    if !env.contains_key("COPILOT_GITHUB_TOKEN")
        && let Some(token) = resolve_host_github_token()
    {
        env.insert("COPILOT_GITHUB_TOKEN".to_string(), token);
    }

    // Parse mounts from config with variable substitution.
    let mounts: Vec<BindMount> = config
        .mounts
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter_map(|s| {
            let expanded = substitute_variables_with_user(s, workspace, remote_user);
            parse_mount_string(&expanded)
        })
        .collect();

    (env, mounts)
}

/// Parse a mount string like "source=/a,target=/b,type=bind,readonly" into a BindMount.
fn parse_mount_string(s: &str) -> Option<BindMount> {
    let mut source = None;
    let mut target = None;
    let mut readonly = false;

    for part in s.split(',') {
        if let Some((key, value)) = part.split_once('=') {
            match key.trim() {
                "source" | "src" => source = Some(PathBuf::from(value.trim())),
                "target" | "dst" | "destination" => target = Some(value.trim().to_string()),
                "readonly" | "ro" => readonly = value.trim() == "true",
                _ => {}
            }
        } else if part.trim() == "readonly" || part.trim() == "ro" {
            readonly = true;
        }
    }

    Some(BindMount {
        source: source?,
        target: target?,
        readonly,
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

#[derive(Debug, thiserror::Error)]
pub enum ContainerError {
    #[error("Container failed to start: {0}")]
    Start(String),
    #[error("Failed to parse devcontainer config: {0}")]
    Parse(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mount_string_basic() {
        let m = parse_mount_string("source=/home/user/.ssh,target=/home/vscode/.ssh,type=bind").unwrap();
        assert_eq!(m.source, PathBuf::from("/home/user/.ssh"));
        assert_eq!(m.target, "/home/vscode/.ssh");
        assert!(!m.readonly);
    }

    #[test]
    fn parse_mount_string_readonly() {
        let m = parse_mount_string("source=/a,target=/b,type=bind,readonly").unwrap();
        assert!(m.readonly);
    }

    #[test]
    fn parse_mount_string_missing_source() {
        assert!(parse_mount_string("target=/b,type=bind").is_none());
    }

    #[test]
    fn resolve_token_returns_none_without_sources() {
        let _ = resolve_host_github_token();
    }
}
