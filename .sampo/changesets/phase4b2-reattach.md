---
cargo/fleet-commander: minor
---

Make daemon-owned ACP sessions **survive a TUI restart** (Phase 4b2 y2-reattach) — the bug that started this work.

`fleet-agent` now holds sessions in a **daemon-scoped registry** shared across every client connection (`DaemonState`), instead of per-connection state that died when the client disconnected. Each session buffers its outbound `session.*` history and forwards it to whichever host is currently attached. When a host disconnects the connection is **detached** (not torn down), so the ACP agent and conversation keep running in the container. When a host reconnects, `session.start` for the same cwd **reattaches** to the live session and **replays the buffered history**, so the reconnecting TUI rebuilds the full conversation without spawning a new agent. A session is only retired when its ACP child exits on its own; the next `session.start` then starts fresh.

Covered by a new socket-daemon integration test that starts a session + prompt turn on one bridge client, disconnects it, and asserts a second client reattaches to the same session id and replays the prior turn's update without prompting. (Follow-up: the replay buffer is currently unbounded; a future change can cap/compact it for very long sessions.)
