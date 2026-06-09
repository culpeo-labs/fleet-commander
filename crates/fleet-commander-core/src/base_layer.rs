//! Filesystem layout for Fleet Commander's per-workspace state.
//!
//! Both the runtime (which reads a workspace's base devcontainer layer when
//! starting a container) and the TUI (which writes that layer during
//! `fleet-commander init`) need to agree on these paths. They live here so
//! neither side has to depend on the other.

use std::path::{Path, PathBuf};

use anyhow::Result;

/// Path to fleet-commander's config directory (e.g. `~/.config/fleet-commander`).
pub fn fleet_commander_config_dir() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine config directory"))?
        .join("fleet-commander");
    Ok(dir)
}

/// Stable slug for a workspace path (last component, or hash if ambiguous).
pub fn workspace_slug(workspace: &Path) -> String {
    workspace
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("{:x}", fxhash(workspace)))
}

fn fxhash(path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

/// Directory for a workspace's base layer.
pub fn workspace_layer_dir(workspace: &Path) -> Result<PathBuf> {
    let slug = workspace_slug(workspace);
    let dir = fleet_commander_config_dir()?.join("layers").join(slug);
    Ok(dir)
}

/// Per-workspace data directory for persisting runtime state (sessions, tokens).
///
/// Located at `~/.local/share/fleet-commander/<slug>/`.
pub fn workspace_data_dir(workspace: &Path) -> Option<PathBuf> {
    let slug = workspace_slug(workspace);
    let dir = dirs::data_dir()?.join("fleet-commander").join(slug);
    Some(dir)
}

/// Path to the base devcontainer layer for a workspace, if it exists.
pub fn base_layer_path_for(workspace: &Path) -> Option<PathBuf> {
    let path = workspace_layer_dir(workspace).ok()?.join("devcontainer.json");
    if path.is_file() { Some(path) } else { None }
}

/// Legacy: global base layer path (kept for backward compat during transition).
pub fn base_layer_path() -> Option<PathBuf> {
    let path = fleet_commander_config_dir().ok()?.join("base-devcontainer.json");
    if path.is_file() { Some(path) } else { None }
}
