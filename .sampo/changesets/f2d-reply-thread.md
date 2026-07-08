---
cargo/fleet-commander: minor
---

Route cross-workspace replies back to the sender via correlation ids. The
`send_to_workspace` tool now takes an optional `thread` id: omit it to start a
new exchange (the ack reports the generated id), or echo a received `thread`
to reply. Delivered messages are framed with the sender's workspace id, the
thread id, and an instruction to reply via `send_to_workspace` — so a reply
flows back through the same inbox + approval path, letting two agents hold a
threaded request/response conversation.
