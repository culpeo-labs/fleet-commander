---
cargo/fleet-commander: minor
---

Collapse a container agent's explorer `fs`/`git` traffic and its ACP `session.*` protocol onto a **single** `docker exec` bridge (Phase 4b2 y3, final part).

Previously the host opened up to three separate `docker exec` connections per started agent: a watched explorer `ServiceFs`, a one-shot git-branch read, and the daemon-owned session. Now the daemon-owned session driver builds the explorer `ServiceFs` over a shared clone of the **same** transport, starts its `fs.watch` subscription, reads the git branch, and delivers them to the app via new `SessionEvent::ExplorerFs`/`AgentBranch` events; live `fs.didChange` pushes and `fs.search` results ride the same connection and are routed through new `ExplorerFsChanged`/`SearchResults`/`SearchDone` events. The app stores the delivered filesystem per-agent so re-entering a session re-installs it instead of dialing a fresh bridge, and drops it when the agent exits so the underlying `docker exec` is torn down. This builds on the daemon- and host-side request concurrency landed earlier in y3, so a slow `session.start` handshake no longer blocks the explorer on the shared connection.
