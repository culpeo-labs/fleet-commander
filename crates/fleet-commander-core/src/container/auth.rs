//! Host environment variables injected into agent processes for headless auth.

/// Read a GitHub auth token from the host environment.
///
/// Checks `COPILOT_GITHUB_TOKEN`, then `GH_TOKEN`, then `GITHUB_TOKEN` (the
/// same precedence as the copilot CLI). Returns `None` when none are set —
/// there is no interactive or `gh`-based fallback.
fn resolve_host_github_token() -> Option<String> {
    for var in ["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(val) = std::env::var(var)
            && !val.is_empty()
        {
            return Some(val);
        }
    }

    None
}

/// Build the list of environment variables to inject into an agent process so
/// the copilot CLI can authenticate in headless / keychain-less environments.
///
/// Currently this is just `COPILOT_GITHUB_TOKEN` when a token is found on the
/// host; returns an empty list when no token is available.
pub fn agent_auth_env() -> Vec<(String, String)> {
    let mut env = Vec::new();
    if let Some(token) = resolve_host_github_token() {
        env.push(("COPILOT_GITHUB_TOKEN".to_string(), token));
    }
    env
}

/// Render env vars as `docker exec` flags — ` -e NAME=VALUE` per entry, each
/// with a leading space so the result can be spliced into a command string.
///
/// Returns an empty string for an empty list.
pub fn docker_env_flags(env: &[(String, String)]) -> String {
    env.iter().map(|(k, v)| format!(" -e {k}={v}")).collect()
}

/// Render env vars as a shell command prefix — `NAME=VALUE ` per entry. The
/// ACP crate parses these `NAME=value` prefixes off the front of a command.
///
/// Returns an empty string for an empty list.
pub fn command_env_prefix(env: &[(String, String)]) -> String {
    env.iter().map(|(k, v)| format!("{k}={v} ")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_env_flags_empty_is_blank() {
        assert_eq!(docker_env_flags(&[]), "");
    }

    #[test]
    fn docker_env_flags_renders_leading_space_per_entry() {
        let env = vec![
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "2".to_string()),
        ];
        assert_eq!(docker_env_flags(&env), " -e A=1 -e B=2");
    }

    #[test]
    fn command_env_prefix_empty_is_blank() {
        assert_eq!(command_env_prefix(&[]), "");
    }

    #[test]
    fn command_env_prefix_renders_trailing_space_per_entry() {
        let env = vec![
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "2".to_string()),
        ];
        assert_eq!(command_env_prefix(&env), "A=1 B=2 ");
    }
}
