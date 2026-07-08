---
cargo/fleet-commander: minor
---

Inject the in-container MCP relay into the ACP session (Feature 2 F2a2b-2). When
the host opts in (`SessionStartParams.mcp`), the daemon now injects a stdio MCP
server into `session/new` (and `session/resume`/`session/load`) pointing the
in-container agent at `fleet-agent mcp --socket <daemon-socket> --token <cwd>`,
so the agent's MCP client dials back into the daemon and its frames tunnel to the
attached host over the existing session connection. The daemon records its own
socket path (`DaemonState::with_socket`) and flips `capabilities.mcp` to true. An
end-to-end test drives a recording fake ACP agent and asserts the injected stdio
server's command and args.
