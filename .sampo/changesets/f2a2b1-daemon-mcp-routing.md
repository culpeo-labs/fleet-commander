---
cargo/fleet-commander: minor
---

Route MCP relay tunnels through the fleet-agent daemon (Feature 2 F2a2b-1). The
daemon now bridges an in-container `fleet-agent mcp` relay connection to its
session's attached host: `mcp.bind{token}` resolves the owning session (keyed by
cwd) and opens a tunnel (`mcp.open` to the host), then `mcp.data` flows both ways
— stamped with the daemon-assigned tunnel id on the host-facing hop — and
`mcp.close` (or a relay disconnect) tears it down. Tunnel frames are delivered
"live" so they never pollute the session's replay buffer. `session/new`
injection that auto-spawns the relay follows in the next change.
