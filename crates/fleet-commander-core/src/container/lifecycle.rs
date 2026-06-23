//! Starting, stopping, and removing dev containers.

use std::collections::HashMap;
use std::path::Path;

use devcontainer_lib::devcontainer::config::DevcontainerConfig;
use devcontainer_lib::devcontainer::lifecycle::run_lifecycle_hooks;
use devcontainer_lib::runtime::{
    self, ContainerRuntime, ContainerState, PortMapping, WorkspaceMount,
};
use devcontainer_lib::util::{container_name, workspace_folder_name, workspace_labels};
use tracing::{debug, error, info, warn};

use super::config::load_merged_config;
use super::image::{resolve_image, resolve_remote_user};
use super::mounts::build_env_and_mounts;
use super::{ContainerConfig, ContainerError, ContainerInfo};

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
    let (env, mut mounts) = build_env_and_mounts(workspace, &dc_config, remote_user.as_deref());

    // Inject the in-container service binary as a read-only bind mount so the
    // explorer can serve files/git from inside the container. Best-effort:
    // if no host binary for the container arch is available, the explorer
    // falls back to the host-side filesystem.
    match crate::agent_bin::host_arch_slug().and_then(crate::agent_bin::resolve_host_bin) {
        Some(host_bin) => {
            info!(path = %host_bin.display(), "Injecting fleet-agent binary mount");
            mounts.push(super::mounts::agent_bind_mount(&host_bin));
        }
        None => {
            debug!("No host fleet-agent binary found; explorer will use host filesystem");
        }
    }
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
