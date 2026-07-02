---
cargo/fleet-commander: minor
---

Run `fleet-agent` as a **persistent in-container daemon** the host reattaches to, instead of a fresh `docker exec` process per connection. The devcontainer `postStartCommand` now starts `fleet-agent serve --socket …` (idempotently — a redundant launch exits when it finds a live socket), and the host connects through a new `fleet-agent bridge` relay over `docker exec -i`, portable across native Docker and Docker Desktop. The daemon serves the fs/watch channel and the ACP tunnel concurrently (a thread per connection) and keeps listening across client disconnects, so it survives a TUI restart. This is the transport foundation for daemon-scoped state and session reattach.

Note: existing workspaces must be re-initialized (`fleet-commander init`) and their container restarted to pick up the daemon `postStartCommand`.
