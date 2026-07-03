---
cargo/fleet-commander: minor
---

Make `fleet-agent` **own the ACP coding-agent session** (Phase 4b2). The daemon now runs the ACP client itself: on `session.start` it spawns the agent, runs the `initialize` → authenticate → resume-or-`session/new` handshake once, and keeps the connection alive at daemon scope. Host prompts arrive as `session.prompt` notifications and are forwarded to the agent (`session.promptResult` reports each turn); the agent's `session/update` stream, permission requests, and diagnostics are relayed back as `session.update` / `session.permissionRequest` / `session.output`, with the host answering permissions via `session.permissionRespond`. Session resume (`session/resume`/`session/load`/`session/list`) now lives in the daemon too, so a resumed id survives independently of the host connection. The daemon advertises `capabilities.session = true`.

The ACP client is async (adds `tokio` + `agent-client-protocol` to the daemon) but runs on a dedicated per-session runtime thread behind a synchronous handle, so the existing serve loop is unchanged. Covered by an end-to-end test that drives `session.start`/`session.prompt` against a fake ACP agent. The host still uses the Phase 4a `acp.*` tunnel for the container path until the host rewire lands next.
