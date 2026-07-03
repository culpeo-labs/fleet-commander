---
cargo/fleet-commander: minor
---

Define the daemon-owned **session protocol** (`session.*`) that lets `fleet-agent` own the ACP client and expose a higher-level session-observer contract to the host, superseding the raw `acp.*` byte tunnel for the container path. The host asks the daemon to start (or resume) a session with `session.start` and drives it with a `session.prompt` notification, `session.cancel`, and `session.permissionRespond`; the daemon streams progress back as `session.update` (raw ACP `session/update` JSON the host aggregates itself), `session.permissionRequest`, `session.promptResult`, `session.connected`, `session.output`, `session.error`, `session.exit`, and `session.authRequired`. A new `capabilities.session` flag advertises support (defaulting to `false` for older daemons). This is the wire contract for making sessions survive TUI restarts and, later, fan out to multiple clients for cross-workspace injection.
