---
cargo/fleet-commander: minor
---

Add the `send_to_workspace` MCP tool and a cross-workspace message inbox. A
connected agent can now send a message to a paired workspace's agent; the tool
authorizes the target against the pairing set and queues the message. Incoming
messages surface as a per-message approval modal — the user approves or rejects
each one, and only approved messages are injected into the target agent's
session (framed as a cross-workspace message). Cross-workspace traffic is thus
always human-gated.
