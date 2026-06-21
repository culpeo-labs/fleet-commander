//! Building env vars and bind mounts for a container.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use devcontainer_lib::devcontainer::config::DevcontainerConfig;
use devcontainer_lib::devcontainer::variables::{
    substitute_variables, substitute_variables_with_user,
};
use devcontainer_lib::runtime::BindMount;

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
}
