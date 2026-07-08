---
cargo/fleet-commander: minor
---

Add the `fleet-agent mcp` relay subcommand (Feature 2 F2a2a): an in-container
process the coding agent spawns as a stdio MCP server, which translates MCP's
newline-delimited JSON stdio ↔ the daemon's `Content-Length`-framed `mcp.*`
notifications. It announces itself with an `mcp.bind{token}` handshake so the
daemon can resolve which session's host to bridge to, then relays MCP requests
out (`mcp.data`) and host responses back in, terminating on `mcp.close` or when
the agent's MCP client disconnects. Daemon-side routing + `session/new`
injection land in the next change.
