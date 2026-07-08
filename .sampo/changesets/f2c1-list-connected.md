---
cargo/fleet-commander: minor
---

Add the `list_connected` MCP tool for cross-workspace messaging. When an
in-container agent's MCP client is served over a per-agent tunnel, it can now
call `list_connected` to discover which other workspaces the user has paired it
with (via `:connect`). The tool is scoped to the calling agent's identity and
reads the live pairing store, so results always reflect the current pairings.
It is unavailable on the legacy always-on HTTP server (no caller identity).
