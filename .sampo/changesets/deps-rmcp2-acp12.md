---
cargo/fleet-commander: patch
---

Upgrade the `rmcp` MCP SDK to 2.1 and `agent-client-protocol` to 1.2. rmcp 2.0
is a breaking release that renames the `Content` content type to `ContentBlock`
and reshapes `CallToolResult.content` into a `Vec<ContentBlock>`; the TUI MCP
server was updated accordingly. No user-facing behavior changes.
