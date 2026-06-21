//! Resolving a host GitHub token for headless auth.

use std::process::Stdio;

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
