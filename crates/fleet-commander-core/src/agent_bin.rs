//! Locating the host-built `fleet-agent` binary to inject into a container,
//! and the fixed path it is mounted at inside the container.
//!
//! Phase 1 ships the daemon into a dev container by bind-mounting a
//! statically-linked (musl) host binary read-only at [`CONTAINER_AGENT_PATH`],
//! then driving it over `docker exec` (see [`crate::service_fs`]).
//!
//! The host binary is selected by architecture: a `uname -m` probe (or, for
//! the not-yet-created container, the host's own arch) is mapped to a slug via
//! [`arch_slug`], and the matching `fleet-agent-<slug>` file is resolved from
//! the conventional [`host_bin_dir`]. If no matching binary exists the caller
//! falls back to the host-side [`crate::workspace_fs::LocalFs`].

use std::path::{Path, PathBuf};

/// Absolute path inside the container where the agent binary is mounted and
/// executed from.
pub const CONTAINER_AGENT_PATH: &str = "/opt/fleet/bin/fleet-agent";

/// Map a `uname -m` machine string (or [`std::env::consts::ARCH`]) to the
/// architecture slug used in the host binary filename.
///
/// Returns `None` for architectures we don't ship a binary for, so the caller
/// can degrade to a host-side filesystem.
pub fn arch_slug(machine: &str) -> Option<&'static str> {
    match machine.trim() {
        "x86_64" | "amd64" => Some("x86_64"),
        "aarch64" | "arm64" => Some("aarch64"),
        _ => None,
    }
}

/// The host architecture slug for the machine Fleet Commander is running on.
///
/// Used when injecting the bind-mount at container-create time, before any
/// container exists to probe — the container shares the Docker host's
/// architecture in the common (non-emulated) case.
pub fn host_arch_slug() -> Option<&'static str> {
    arch_slug(std::env::consts::ARCH)
}

/// Directory where host-built agent binaries are kept:
/// `<data_dir>/fleet-commander/bin` (e.g. `~/.local/share/fleet-commander/bin`).
pub fn host_bin_dir() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("fleet-commander").join("bin"))
}

/// Resolve the agent binary for `arch` within `dir`, returning the path only
/// when the file exists. The conventional name is `fleet-agent-<arch>`.
pub fn resolve_in_dir(dir: &Path, arch: &str) -> Option<PathBuf> {
    let path = dir.join(format!("fleet-agent-{arch}"));
    path.is_file().then_some(path)
}

/// Resolve the host agent binary to mount for `arch`.
///
/// Honors the `FLEET_AGENT_BIN` override (an explicit path to a binary) before
/// falling back to the conventional `<data_dir>/fleet-commander/bin/fleet-agent-<arch>`.
/// Returns `None` when neither is present, signalling the caller to use the
/// host-side filesystem instead.
pub fn resolve_host_bin(arch: &str) -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("FLEET_AGENT_BIN") {
        let path = PathBuf::from(raw);
        if path.is_file() {
            return Some(path);
        }
    }
    resolve_in_dir(&host_bin_dir()?, arch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_slug_maps_known_machines() {
        assert_eq!(arch_slug("x86_64"), Some("x86_64"));
        assert_eq!(arch_slug("amd64"), Some("x86_64"));
        assert_eq!(arch_slug("aarch64"), Some("aarch64"));
        assert_eq!(arch_slug("arm64"), Some("aarch64"));
        // uname output may carry a trailing newline.
        assert_eq!(arch_slug("x86_64\n"), Some("x86_64"));
    }

    #[test]
    fn arch_slug_rejects_unknown() {
        assert_eq!(arch_slug("riscv64"), None);
        assert_eq!(arch_slug(""), None);
    }

    #[test]
    fn host_arch_slug_is_some_on_supported_targets() {
        // CI runs on x86_64/aarch64, both of which we ship.
        assert!(host_arch_slug().is_some());
    }

    #[test]
    fn resolve_in_dir_finds_matching_binary() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve_in_dir(dir.path(), "x86_64"), None);

        let bin = dir.path().join("fleet-agent-x86_64");
        std::fs::write(&bin, b"#!/bin/true\n").unwrap();
        assert_eq!(resolve_in_dir(dir.path(), "x86_64"), Some(bin));
        // A different arch in the same dir is not matched.
        assert_eq!(resolve_in_dir(dir.path(), "aarch64"), None);
    }
}
