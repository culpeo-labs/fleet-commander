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
use devcontainer_lib::devcontainer::features::{
    download_features, generate_feature_dockerfile_with_opts, order_features, resolve_features,
    stage_feature_context,
};
use devcontainer_lib::devcontainer::lifecycle::run_lifecycle_hooks;
use devcontainer_lib::devcontainer::merge::merge_layer;
use devcontainer_lib::devcontainer::variables::{
    substitute_variables, substitute_variables_with_user,
};
use devcontainer_lib::parse_jsonc;
use devcontainer_lib::runtime::{
    self, BindMount, ContainerRuntime, ContainerState, PortMapping, WorkspaceMount,
};
use devcontainer_lib::util::{container_name, workspace_folder_name, workspace_labels};
use tracing::{debug, error, info, warn};

use crate::base_layer;

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

/// Load a devcontainer.json file, merging the fleet-commander base layer if present.
fn load_merged_config(config_path: &Path) -> Result<DevcontainerConfig, ContainerError> {
    debug!(path = %config_path.display(), "Loading devcontainer config");
    let raw = std::fs::read_to_string(config_path).map_err(|e| {
        ContainerError::Parse(format!("Failed to read {}: {e}", config_path.display()))
    })?;
    let mut project_json: serde_json::Value =
        parse_jsonc(&raw).map_err(|e| ContainerError::Parse(e.to_string()))?;

    // Try per-workspace layer first, then fall back to global legacy layer.
    let workspace = config_path
        .parent()
        .and_then(|p| p.parent()) // .devcontainer/ -> project root
        .unwrap_or(Path::new("/"));

    let base_path = base_layer::base_layer_path_for(workspace).or_else(base_layer::base_layer_path);

    if let Some(ref base_path) = base_path {
        debug!(layer = %base_path.display(), "Merging base layer");
    } else {
        debug!("No base layer found");
    }

    if let Some(base_path) = base_path
        && let Ok(base_raw) = std::fs::read_to_string(&base_path)
        && let Ok(base_json) = parse_jsonc::<serde_json::Value>(&base_raw)
    {
        merge_layer(&mut project_json, &base_json);
    }

    serde_json::from_value(project_json).map_err(|e| ContainerError::Parse(e.to_string()))
}

/// Start a dev container for the given workspace.
///
/// Loads the project's devcontainer.json, merges the base credential layer,
/// then builds/creates/starts the container using the Docker API (bollard).
///
/// `on_progress` is called with human-readable status messages at each phase
/// so the caller can update the UI.
pub async fn start_container(
    config: &ContainerConfig,
    on_progress: impl Fn(&str),
) -> Result<ContainerInfo, ContainerError> {
    let workspace = &config.workspace_folder;
    info!(workspace = %workspace.display(), "Starting container");

    let rt = runtime::detect_runtime(None).await.map_err(|e| {
        error!(error = %e, "Failed to detect container runtime");
        ContainerError::Start(e.to_string())
    })?;

    // Load devcontainer config, merging base layer if present.
    let config_path = workspace.join(".devcontainer/devcontainer.json");
    if !config_path.is_file() {
        error!(path = %config_path.display(), "No devcontainer.json found");
        return Err(ContainerError::Parse(format!(
            "No .devcontainer/devcontainer.json found in {}",
            workspace.display()
        )));
    }
    let dc_config = load_merged_config(&config_path)?;

    // Check if a container already exists for this workspace.
    let labels_list = workspace_labels(workspace, Some(&config_path));
    let filters: Vec<String> = labels_list
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    debug!(filters = ?filters, "Searching for existing containers");
    let existing = rt
        .list_containers(&filters)
        .await
        .map_err(|e| ContainerError::Start(e.to_string()))?;

    info!(count = existing.len(), "Found existing containers");

    if let Some(container) = existing.first() {
        info!(id = %container.id, state = ?container.state, "Found existing container");
        match container.state {
            ContainerState::Running => {
                on_progress("Container already running, reattaching…");
                let remote_user =
                    resolve_remote_user(rt.as_ref(), &container.image, &dc_config).await?;
                let folder_name = workspace_folder_name(workspace);
                info!(id = %container.id, "Reusing running container");
                return Ok(ContainerInfo {
                    container_id: container.id.clone(),
                    workspace_folder: workspace.clone(),
                    remote_workspace_folder: dc_config
                        .workspace_folder
                        .clone()
                        .unwrap_or_else(|| format!("/workspaces/{folder_name}")),
                    remote_user: remote_user.unwrap_or_else(|| "root".to_string()),
                });
            }
            ContainerState::Stopped => {
                on_progress("Starting stopped container…");
                info!(id = %container.id, "Starting stopped container");
                rt.start_container(&container.id)
                    .await
                    .map_err(|e| ContainerError::Start(e.to_string()))?;
                let remote_user =
                    resolve_remote_user(rt.as_ref(), &container.image, &dc_config).await?;
                let user_str = remote_user.unwrap_or_else(|| "root".to_string());
                let folder_name = workspace_folder_name(workspace);

                // Run postStartCommand on restart (per devcontainer spec,
                // postStartCommand runs on every start, not just creation).
                run_post_start_command(
                    rt.as_ref(),
                    &container.id,
                    &dc_config,
                    &user_str,
                    &on_progress,
                )
                .await;

                return Ok(ContainerInfo {
                    container_id: container.id.clone(),
                    workspace_folder: workspace.clone(),
                    remote_workspace_folder: dc_config
                        .workspace_folder
                        .clone()
                        .unwrap_or_else(|| format!("/workspaces/{folder_name}")),
                    remote_user: user_str,
                });
            }
            ContainerState::NotFound => {
                debug!("Container state is NotFound, proceeding to build");
            }
        }
    }

    // No existing container — build image and create one.
    info!("No existing container — building image");
    let image = resolve_image(
        rt.as_ref(),
        workspace,
        &dc_config,
        &config_path,
        &on_progress,
    )
    .await?;
    let name = container_name(workspace);
    let folder_name = workspace_folder_name(workspace);
    info!(image = %image, name = %name, "Image ready");

    let remote_user = resolve_remote_user(rt.as_ref(), &image, &dc_config).await?;
    debug!(remote_user = ?remote_user, "Resolved remote user");

    // Merge base credential layer into env/mounts.
    let (env, mounts) = build_env_and_mounts(workspace, &dc_config, remote_user.as_deref());
    debug!(
        env_count = env.len(),
        mount_count = mounts.len(),
        "Built env and mounts"
    );
    for mount in &mounts {
        debug!(source = %mount.source.display(), target = %mount.target, "Mount");
        // Ensure bind mount source directories exist on the host.
        if !mount.source.exists() {
            info!(path = %mount.source.display(), "Creating bind mount source directory");
            if let Err(e) = std::fs::create_dir_all(&mount.source) {
                warn!(path = %mount.source.display(), error = %e, "Failed to create mount source dir");
            }
        }
    }

    let mut labels = HashMap::new();
    for (k, v) in &labels_list {
        labels.insert(k.clone(), v.clone());
    }

    let ports: Vec<PortMapping> = dc_config.forward_ports.clone().unwrap_or_default();

    let remote_workspace = dc_config
        .workspace_folder
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

    on_progress("Creating container…");
    let container_id = rt.create_container(&container_config).await.map_err(|e| {
        error!(error = %e, "Failed to create container");
        ContainerError::Start(e.to_string())
    })?;
    info!(id = %container_id, "Container created");
    on_progress("Starting container…");
    rt.start_container(&container_id).await.map_err(|e| {
        error!(id = %container_id, error = %e, "Failed to start container");
        ContainerError::Start(e.to_string())
    })?;
    info!(id = %container_id, "Container started");

    // Run lifecycle hooks (postCreateCommand, postStartCommand, etc.).
    // These execute inside the running container as the remote user.
    let user_str = remote_user.clone().unwrap_or_else(|| "root".to_string());
    on_progress("Running lifecycle hooks…");
    info!(id = %container_id, user = %user_str, "Running lifecycle hooks");
    if let Err(e) = run_lifecycle_hooks(
        rt.as_ref(),
        &container_id,
        &dc_config,
        Some(user_str.as_str()),
        None,
    )
    .await
    {
        warn!(error = %e, "Lifecycle hook failed");
        on_progress(&format!("⚠ Lifecycle hook failed: {e}"));
    }

    Ok(ContainerInfo {
        container_id,
        workspace_folder: workspace.clone(),
        remote_workspace_folder: remote_workspace,
        remote_user: user_str,
    })
}

/// Run only the postStartCommand from the devcontainer config.
///
/// Used when restarting a stopped container — postCreateCommand should NOT
/// re-run, but postStartCommand runs on every start per the spec.
async fn run_post_start_command(
    rt: &dyn ContainerRuntime,
    container_id: &str,
    config: &DevcontainerConfig,
    user: &str,
    on_progress: &impl Fn(&str),
) {
    use devcontainer_lib::devcontainer::config::LifecycleCommand;

    let cmd = match &config.post_start_command {
        Some(cmd) => cmd,
        None => return,
    };

    on_progress("Running postStartCommand…");
    info!(id = %container_id, "Running postStartCommand on restart");

    let commands = match cmd {
        LifecycleCommand::Single(c) => vec![c.as_str()],
        LifecycleCommand::Multiple(cs) => cs.iter().map(|c| c.as_str()).collect(),
        LifecycleCommand::Parallel(map) => map.values().map(|c| c.as_str()).collect(),
    };

    for command in commands {
        let args = vec!["sh".to_string(), "-c".to_string(), command.to_string()];
        match rt.exec(container_id, &args, Some(user)).await {
            Ok(result) if result.exit_code != 0 => {
                warn!(
                    exit_code = result.exit_code,
                    stderr = %result.stderr,
                    "postStartCommand failed"
                );
                on_progress(&format!(
                    "⚠ postStartCommand failed (exit {})",
                    result.exit_code
                ));
            }
            Err(e) => {
                warn!(error = %e, "postStartCommand exec failed");
                on_progress(&format!("⚠ postStartCommand failed: {e}"));
            }
            Ok(_) => {
                info!("postStartCommand completed successfully");
            }
        }
    }
}

/// Resolve the container image — pull/build base, then layer features if present.
async fn resolve_image(
    rt: &dyn ContainerRuntime,
    workspace: &Path,
    config: &DevcontainerConfig,
    config_path: &Path,
    on_progress: &impl Fn(&str),
) -> Result<String, ContainerError> {
    let folder_image = container_name(workspace);
    let features = resolve_features(config).unwrap_or_default();
    let has_features = !features.is_empty();
    let devcontainer_dir = config_path.parent().map(|p| p.to_path_buf());
    debug!(
        has_features,
        feature_count = features.len(),
        "Resolving image"
    );

    // 1. Pull or build the base image.
    let base_image = if let Some(ref image) = config.image {
        info!(image = %image, "Resolving base image");
        let cached = rt.image_exists(image).await.unwrap_or(false);
        if cached {
            on_progress("Base image cached locally ✓");
            info!(image = %image, "Base image found locally, skipping pull");
        } else {
            on_progress(&format!("Pulling image {image}…"));
            info!(image = %image, "Pulling base image");
            rt.pull_image(image)
                .await
                .map_err(|e| ContainerError::Start(format!("Failed to pull image: {e}")))?;
            on_progress("Image pull complete ✓");
            info!(image = %image, "Base image pull complete");
        }
        image.clone()
    } else if let Some(ref build) = config.build {
        let context_dir = config_path
            .parent()
            .unwrap()
            .join(build.context.as_deref().unwrap_or("."));
        let dockerfile_path = config_path.parent().unwrap().join(&build.dockerfile);
        info!(dockerfile = %dockerfile_path.display(), context = %context_dir.display(), "Building base image");
        on_progress("Building image from Dockerfile…");
        let dockerfile_content = std::fs::read_to_string(&dockerfile_path)
            .map_err(|e| ContainerError::Parse(format!("Failed to read Dockerfile: {e}")))?;
        let base_tag = if has_features {
            folder_image.clone()
        } else {
            format!("{folder_image}-base")
        };
        rt.build_image(
            &dockerfile_content,
            &context_dir,
            &base_tag,
            &HashMap::new(),
            false,
            false,
        )
        .await
        .map_err(|e| ContainerError::Start(format!("Image build failed: {e}")))?;
        base_tag
    } else {
        return Err(ContainerError::Parse(
            "devcontainer.json must specify 'image' or 'build.dockerfile'".into(),
        ));
    };

    if !has_features {
        info!(image = %base_image, "No features, using base image directly");
        return Ok(base_image);
    }

    // 2. Download and stage features.
    info!(count = features.len(), "Downloading features");
    on_progress(&format!("Downloading {} feature(s)…", features.len()));
    let mut features = features;
    download_features(&mut features, devcontainer_dir.as_deref())
        .await
        .map_err(|e| ContainerError::Start(format!("Failed to download features: {e}")))?;

    let ordered = order_features(&features);
    let staging_dir = stage_feature_context(&ordered)
        .map_err(|e| ContainerError::Start(format!("Failed to stage features: {e}")))?;

    // 3. Generate and build the feature layer.
    let feature_user = runtime::resolve_remote_user(rt, &base_image, config.remote_user.as_deref())
        .await
        .ok()
        .flatten();
    let dockerfile = generate_feature_dockerfile_with_opts(
        &base_image,
        &ordered,
        feature_user.as_deref(),
        config,
    );
    let final_tag = format!("{folder_image}-features");
    info!(tag = %final_tag, "Building feature layer image");
    debug!(dockerfile = %dockerfile, "Feature layer Dockerfile");
    on_progress("Building feature layer…");
    let result = rt
        .build_image(
            &dockerfile,
            &staging_dir,
            &final_tag,
            &HashMap::new(),
            false,
            false,
        )
        .await;
    let _ = std::fs::remove_dir_all(&staging_dir);

    match result {
        Ok(()) => {
            info!(tag = %final_tag, "Feature image ready");
            Ok(final_tag)
        }
        Err(e) => {
            warn!(error = %e, "Feature layer build failed, falling back to base image");
            on_progress("⚠ Feature build failed, using base image…");
            Ok(base_image)
        }
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

/// Stop (but don't remove) any running container for the given workspace.
///
/// Used during graceful shutdown so containers don't keep running after
/// Fleet Commander exits.
pub async fn stop_workspace_container(workspace: &Path) -> Result<(), ContainerError> {
    let rt = runtime::detect_runtime(None)
        .await
        .map_err(|e| ContainerError::Start(e.to_string()))?;

    let config_path = workspace.join(".devcontainer/devcontainer.json");
    let labels_list = workspace_labels(workspace, Some(&config_path));
    let filters: Vec<String> = labels_list
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    let existing = rt
        .list_containers(&filters)
        .await
        .map_err(|e| ContainerError::Start(e.to_string()))?;

    for container in &existing {
        if container.state == ContainerState::Running {
            info!(id = %container.id, "Stopping container");
            let _ = rt.stop_container(&container.id).await;
        }
    }

    Ok(())
}

/// Stop and remove any existing container for the given workspace.
///
/// Used by the `:rebuild` command to force a fresh container start.
pub async fn remove_workspace_container(workspace: &Path) -> Result<(), ContainerError> {
    info!(workspace = %workspace.display(), "Removing container");
    let rt = runtime::detect_runtime(None)
        .await
        .map_err(|e| ContainerError::Start(e.to_string()))?;

    let config_path = workspace.join(".devcontainer/devcontainer.json");
    let labels_list = workspace_labels(workspace, Some(&config_path));
    let filters: Vec<String> = labels_list
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    let existing = rt
        .list_containers(&filters)
        .await
        .map_err(|e| ContainerError::Start(e.to_string()))?;

    for container in &existing {
        info!(id = %container.id, state = ?container.state, "Removing container");
        if container.state == ContainerState::Running {
            let _ = rt.stop_container(&container.id).await;
        }
        let _ = rt.remove_container(&container.id).await;
    }

    Ok(())
}

/// Resolve a GitHub auth token from the host environment.
///
/// Used to inject `COPILOT_GITHUB_TOKEN` into the agent process so the
/// copilot CLI can authenticate in headless / keychain-less environments.
///
/// Checks, in order:
/// 1. `COPILOT_GITHUB_TOKEN` / `GH_TOKEN` / `GITHUB_TOKEN` env vars
/// 2. `gh auth token` — uses the GitHub CLI credential store
pub fn resolve_host_github_token() -> Option<String> {
    // Check environment variables first (same precedence as copilot CLI).
    for var in ["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(val) = std::env::var(var)
            && !val.is_empty()
        {
            return Some(val);
        }
    }

    // Try `gh auth token`.
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

    None
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
        let m = parse_mount_string("source=/home/user/.ssh,target=/home/vscode/.ssh,type=bind")
            .unwrap();
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
}
