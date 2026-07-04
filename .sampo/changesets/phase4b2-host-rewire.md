---
cargo/fleet-commander: minor
---

Drive the **daemon-owned ACP session from the host** for container agents (Phase 4b2). The host container path now speaks the higher-level `session.*` protocol instead of tunnelling raw ACP stdio: it connects a dedicated `fleet-agent` connection, issues `session.start`, and — on success — forwards TUI prompts as `session.prompt` notifications. Progress flows back through a notification sink that feeds forwarded `session.update`s into the host's existing `SessionStateMachine`, relays `session.permissionRequest` (answered via `session.permissionRespond`), and surfaces `session.connected`/`output`/`error`/`exit` as the usual `SessionEvent`s. Interactive login is taken from the `session.start` result and wrapped with `docker exec -it`.

Because the daemon now owns the ACP client and session, the session survives a TUI exit/restart. Daemons that predate `capabilities.session` transparently fall back to the Phase 4a `acp.*` tunnel; the host (non-container) path is unchanged and still drives the ACP client directly.
