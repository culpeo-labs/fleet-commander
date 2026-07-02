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

/// Directory inside the container holding the agent binaries + launcher.
pub const CONTAINER_AGENT_DIR: &str = "/opt/fleet/bin";

/// Fixed path of the persistent daemon's unix socket inside the container.
///
/// The daemon (started by the devcontainer `postStartCommand`) binds this
/// socket; the host reaches it via `fleet-agent bridge --socket <this>` over
/// `docker exec`. Kept in `/tmp` because it is always writable by the remote
/// user without extra ownership fixups, and each container hosts one workspace.
pub const CONTAINER_AGENT_SOCKET: &str = "/tmp/fleet-agent.sock";

/// Architecture slugs we ship `fleet-agent` binaries for.
pub const KNOWN_ARCH_SLUGS: &[&str] = &["x86_64", "aarch64"];

/// The launcher script mounted at [`CONTAINER_AGENT_PATH`]. It selects the
/// matching per-arch binary at exec time using the container's own `uname -m`
/// (which prints `x86_64`/`aarch64` on Linux — exactly our slugs). Doing the
/// dispatch *inside* the container makes selection correct even under qemu
/// emulation or an explicit `--platform` that differs from the Docker host's
/// architecture.
pub const LAUNCHER_SCRIPT: &str =
    "#!/bin/sh\nexec \"/opt/fleet/bin/fleet-agent-$(uname -m)\" \"$@\"\n";

/// Container path of the per-architecture binary for `slug`
/// (`/opt/fleet/bin/fleet-agent-<slug>`), where the launcher dispatches.
pub fn container_agent_path_for(slug: &str) -> String {
    format!("{CONTAINER_AGENT_DIR}/fleet-agent-{slug}")
}

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
    if let Some(path) = agent_override_bin() {
        return Some(path);
    }
    resolve_in_dir(&host_bin_dir()?, arch)
}

/// The explicit `FLEET_AGENT_BIN` override, when it points at a real file.
///
/// This is the single-binary developer escape hatch: it bypasses arch
/// resolution and the launcher, mounting exactly this binary as the agent.
pub fn agent_override_bin() -> Option<PathBuf> {
    let raw = std::env::var_os("FLEET_AGENT_BIN")?;
    let path = PathBuf::from(raw);
    path.is_file().then_some(path)
}

/// Resolve every per-architecture host binary that is present, as
/// `(slug, host_path)` pairs. Empty when none have been built. Ignores the
/// `FLEET_AGENT_BIN` override (handled separately by [`agent_override_bin`]).
pub fn resolve_all_host_bins() -> Vec<(&'static str, PathBuf)> {
    let Some(dir) = host_bin_dir() else {
        return Vec::new();
    };
    KNOWN_ARCH_SLUGS
        .iter()
        .filter_map(|slug| resolve_in_dir(&dir, slug).map(|p| (*slug, p)))
        .collect()
}

/// Materialize the launcher script in [`host_bin_dir`] so it can be
/// bind-mounted into the container, returning its host path. Idempotent;
/// marked executable on Unix.
pub fn ensure_launcher_script() -> std::io::Result<PathBuf> {
    let dir =
        host_bin_dir().ok_or_else(|| std::io::Error::other("no host data dir for launcher"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("fleet-agent-launcher.sh");
    std::fs::write(&path, LAUNCHER_SCRIPT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(path)
}

/// Install an embedded agent binary (shipped inside the commander via the
/// `embed-agent` build feature) into [`host_bin_dir`] as `fleet-agent-<slug>`,
/// returning its host path.
///
/// Idempotent: if an identical file already exists it's left untouched.
/// Otherwise the bytes are written to a temp file and atomically renamed over
/// the destination, so a container currently `exec`ing the old inode keeps
/// running (and we avoid `ETXTBSY` on the bind-mounted binary).
pub fn install_embedded_binary(slug: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    let dir =
        host_bin_dir().ok_or_else(|| std::io::Error::other("no host data dir for agent binary"))?;
    install_embedded_binary_in(&dir, slug, bytes)
}

/// [`install_embedded_binary`] against an explicit directory (testable).
fn install_embedded_binary_in(dir: &Path, slug: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let dest = dir.join(format!("fleet-agent-{slug}"));
    if let Ok(existing) = std::fs::read(&dest)
        && existing == bytes
    {
        return Ok(dest);
    }
    let tmp = dir.join(format!(".fleet-agent-{slug}.tmp"));
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
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
    fn container_agent_path_is_under_the_agent_dir() {
        assert_eq!(
            container_agent_path_for("x86_64"),
            "/opt/fleet/bin/fleet-agent-x86_64"
        );
        assert_eq!(
            container_agent_path_for("aarch64"),
            "/opt/fleet/bin/fleet-agent-aarch64"
        );
        // The launcher lives in the same dir and dispatches to these.
        for slug in KNOWN_ARCH_SLUGS {
            assert!(container_agent_path_for(slug).starts_with(CONTAINER_AGENT_DIR));
        }
    }

    #[test]
    fn launcher_dispatches_on_uname_to_the_per_arch_binary() {
        // The launcher must exec `fleet-agent-$(uname -m)` from the agent dir;
        // Linux `uname -m` yields our slugs verbatim, so no mapping is needed.
        assert!(LAUNCHER_SCRIPT.starts_with("#!/bin/sh\n"));
        assert!(LAUNCHER_SCRIPT.contains("$(uname -m)"));
        assert!(LAUNCHER_SCRIPT.contains("/opt/fleet/bin/fleet-agent-"));
        assert!(LAUNCHER_SCRIPT.contains("\"$@\""));
    }

    #[test]
    fn host_arch_slug_is_some_on_supported_targets() {
        // CI runs on x86_64/aarch64, both of which we ship.
        assert!(host_arch_slug().is_some());
    }

    #[test]
    fn install_embedded_binary_writes_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = install_embedded_binary_in(dir.path(), "x86_64", b"BINARY").unwrap();
        assert_eq!(path, dir.path().join("fleet-agent-x86_64"));
        assert_eq!(std::fs::read(&path).unwrap(), b"BINARY");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        // Re-installing identical bytes is a no-op that still returns the path.
        let again = install_embedded_binary_in(dir.path(), "x86_64", b"BINARY").unwrap();
        assert_eq!(again, path);
        // New bytes replace the file.
        install_embedded_binary_in(dir.path(), "x86_64", b"NEWER").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"NEWER");
        // No temp file is left behind.
        assert!(!dir.path().join(".fleet-agent-x86_64.tmp").exists());
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
