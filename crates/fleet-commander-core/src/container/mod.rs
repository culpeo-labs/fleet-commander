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
//!
//! The implementation is split across submodules by concern:
//! - [`config`] — loading and base-layer merging of `devcontainer.json`.
//! - [`image`] — resolving the container image (pull/build + features).
//! - [`mounts`] — building env vars and bind mounts.
//! - [`lifecycle`] — starting, stopping, and removing containers.

mod config;
mod image;
mod lifecycle;
mod mounts;

use std::path::PathBuf;

pub use lifecycle::{remove_workspace_container, start_container, stop_workspace_container};

/// Configuration for running an agent inside a dev container.
#[derive(Debug, Clone)]
pub struct ContainerConfig {
    /// Path to the repository with `.devcontainer/` config.
    pub workspace_folder: PathBuf,
}

/// Result of starting a container.
#[derive(Debug)]
pub struct ContainerInfo {
    pub container_id: String,
    pub workspace_folder: PathBuf,
    pub remote_workspace_folder: String,
    pub remote_user: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ContainerError {
    #[error("Container failed to start: {0}")]
    Start(String),
    #[error("Failed to parse devcontainer config: {0}")]
    Parse(String),
}
