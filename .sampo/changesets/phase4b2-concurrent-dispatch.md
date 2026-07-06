---
cargo/fleet-commander: minor
---

Serve `fs.*`/`git.*` requests **concurrently with a session's ACP handshake** (Phase 4b2 y3, part 1).

Previously the `fleet-agent` connection's dispatch loop handled `session.start` inline, blocking until the in-container ACP handshake (initialize + auth + resume) resolved — several seconds. Any filesystem/git request on the same connection had to wait behind it. `session.start` now runs on its own worker thread: it publishes the resulting session into a shared per-connection slot and writes the response frame itself once the handshake resolves, leaving the read loop free to answer `fs.*`/`git.*` immediately. Ordering is preserved — the slot is set before the response is sent, so a `session.prompt` that follows the reply always finds the session.

This unblocks unifying the host's explorer/git traffic and the session onto a single `docker exec` bridge (next change) without freezing the explorer during agent startup. Covered by a new integration test that keeps `session.start` in flight against a slow-initializing agent and asserts an `fs.list` on the same connection is answered promptly.
