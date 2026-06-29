//! Parsing and building the interactive login command.

use agent_client_protocol::schema::v1::AuthMethod;

pub(super) fn terminal_auth_command(method: &AuthMethod) -> Option<(String, Vec<String>)> {
    let terminal_auth = method.meta()?.get("terminal-auth")?;
    let command = terminal_auth.get("command")?.as_str()?.to_string();
    let args = terminal_auth
        .get("args")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| a.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Some((command, args))
}

/// Construct the interactive auth command for `copilot login`.
///
/// For container agents, wraps with `docker exec -it` so login runs inside
/// the container where copilot stores its credentials. For host agents,
/// runs `copilot login` directly.
pub(super) fn build_auth_command(
    container_info: Option<&(String, String, String)>,
    terminal: &(String, Vec<String>),
) -> Vec<String> {
    let (program, args) = terminal;
    let mut v = if let Some((container_id, remote_user, remote_workdir)) = container_info {
        vec![
            "docker".into(),
            "exec".into(),
            "-it".into(),
            "-u".into(),
            remote_user.clone(),
            "-w".into(),
            remote_workdir.clone(),
            container_id.clone(),
            program.clone(),
        ]
    } else {
        vec![program.clone()]
    };
    v.extend(args.iter().cloned());
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn auth_method_from(value: serde_json::Value) -> AuthMethod {
        serde_json::from_value(value).expect("Valid auth method")
    }

    #[test]
    fn terminal_auth_command_parses_meta() {
        let method = auth_method_from(json!({
            "id": "copilot-login",
            "name": "Log in with Copilot CLI",
            "description": "Run `copilot login` in the terminal",
            "_meta": {
                "terminal-auth": {
                    "command": "/usr/local/bin/copilot",
                    "args": ["login"],
                    "label": "Copilot Login"
                }
            }
        }));
        let parsed = terminal_auth_command(&method);
        assert_eq!(
            parsed,
            Some((
                "/usr/local/bin/copilot".to_string(),
                vec!["login".to_string()]
            ))
        );
    }

    #[test]
    fn terminal_auth_command_none_without_meta() {
        let method = auth_method_from(json!({
            "id": "copilot-login",
            "name": "Log in with Copilot CLI"
        }));
        assert_eq!(terminal_auth_command(&method), None);
    }

    #[test]
    fn build_auth_command_host_uses_advertised_command() {
        let terminal = (
            "/usr/local/bin/copilot".to_string(),
            vec!["login".to_string()],
        );
        let cmd = build_auth_command(None, &terminal);
        assert_eq!(cmd, vec!["/usr/local/bin/copilot", "login"]);
    }

    #[test]
    fn build_auth_command_container_wraps_with_docker_exec() {
        let terminal = (
            "/usr/local/bin/copilot".to_string(),
            vec!["login".to_string()],
        );
        let ci = (
            "container123".to_string(),
            "vscode".to_string(),
            "/workspaces/proj".to_string(),
        );
        let cmd = build_auth_command(Some(&ci), &terminal);
        assert_eq!(
            cmd,
            vec![
                "docker",
                "exec",
                "-it",
                "-u",
                "vscode",
                "-w",
                "/workspaces/proj",
                "container123",
                "/usr/local/bin/copilot",
                "login",
            ]
        );
    }
}
