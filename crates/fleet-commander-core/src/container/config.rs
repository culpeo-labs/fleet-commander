//! Loading and base-layer merging of `devcontainer.json`.

use std::path::Path;

use devcontainer_lib::devcontainer::config::DevcontainerConfig;
use devcontainer_lib::devcontainer::merge::merge_layer;
use devcontainer_lib::parse_jsonc;
use tracing::debug;

use crate::base_layer;

use super::ContainerError;

/// Load a devcontainer.json file, merging the fleet-commander base layer if present.
pub(super) fn load_merged_config(config_path: &Path) -> Result<DevcontainerConfig, ContainerError> {
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
