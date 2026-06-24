//! Building env vars and bind mounts for a container.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use devcontainer_lib::devcontainer::config::DevcontainerConfig;
use devcontainer_lib::devcontainer::variables::{
    substitute_variables, substitute_variables_with_user,
};
use devcontainer_lib::runtime::BindMount;

use crate::agent_bin::CONTAINER_AGENT_PATH;

/// Build the environment variables and bind mounts for a container,
/// merging the base credential layer with the project's devcontainer config.
pub(super) fn build_env_and_mounts(
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

/// Build a read-only bind mount of a host file/dir at a fixed container path.
pub(super) fn read_only_mount(source: &Path, target: &str) -> BindMount {
    BindMount {
        source: source.to_path_buf(),
        target: target.to_string(),
        readonly: true,
    }
}

/// Build the read-only bind mount that injects the host-built `fleet-agent`
/// binary into the container at [`CONTAINER_AGENT_PATH`], where it is launched
/// over `docker exec` to serve the explorer's filesystem/git requests.
pub(super) fn agent_bind_mount(host_bin: &Path) -> BindMount {
    read_only_mount(host_bin, CONTAINER_AGENT_PATH)
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

    #[test]
    fn agent_bind_mount_is_readonly_at_fixed_target() {
        let m = agent_bind_mount(Path::new(
            "/home/u/.local/share/fleet-commander/bin/fleet-agent-x86_64",
        ));
        assert_eq!(
            m.source,
            PathBuf::from("/home/u/.local/share/fleet-commander/bin/fleet-agent-x86_64")
        );
        assert_eq!(m.target, CONTAINER_AGENT_PATH);
        assert!(m.readonly);
    }

    #[test]
    fn read_only_mount_targets_arbitrary_container_path() {
        let target = crate::agent_bin::container_agent_path_for("aarch64");
        let m = read_only_mount(Path::new("/host/fleet-agent-aarch64"), &target);
        assert_eq!(m.source, PathBuf::from("/host/fleet-agent-aarch64"));
        assert_eq!(m.target, "/opt/fleet/bin/fleet-agent-aarch64");
        assert!(m.readonly);
    }
}
