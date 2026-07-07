---
cargo/fleet-commander: minor
---

Add the `mcp.*` tunnel wire protocol to `fleet-protocol` (Feature 2 foundation):
`mcp.open`/`mcp.data`/`mcp.close` frames, a `capabilities.mcp` flag, a
`SessionStartParams.mcp` opt-in, and `McpTunnelParams`/`McpDataParams`. This is
protocol scaffolding only — it lets the daemon relay an in-container MCP stream to
the host over the existing session connection (no host port exposed), so
cross-workspace tooling can reach the host MCP server. The relay itself lands in
follow-up changes.
