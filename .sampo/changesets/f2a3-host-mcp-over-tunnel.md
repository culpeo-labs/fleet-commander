---
cargo/fleet-commander: minor
---

Serve the host `TuiMcpServer` over the cross-workspace MCP tunnel (Feature 2
F2a3). The host now opts into the tunnel (`SessionStartParams.mcp = true`) and,
when the daemon opens one (`mcp.open`), bridges the in-container agent's MCP
frames onto an in-process duplex: agent→host `mcp.data` messages are fed into
the stream and the server's newline-JSON responses are sent back as
host→agent `mcp.data`. The TUI serves a `TuiMcpServer` over the duplex per
tunnel (rmcp `serve_with_ct`), cancelling it on `mcp.close`. This lets an
in-container coding agent reach the TUI's MCP tools without any host port,
closing the loop opened by the daemon relay/injection changes.
