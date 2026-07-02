---
cargo/fleet-commander: minor
---

Tunnel the ACP coding agent through `fleet-agent` inside the container. Instead of opening a separate `docker exec copilot --acp --stdio` channel, the host now asks the in-container daemon to spawn and own the ACP child: `acp.start` launches it, `acp.send`/`acp.recv` relay stdio as fire-and-forget notifications (so a long-running prompt never head-of-line-blocks the request/response channel), `acp.stderr` forwards diagnostics, and `acp.exit`/`acp.stop` handle teardown. The daemon advertises the new capability via `capabilities.acp`, and the host drives the ACP client over this tunnel as a first-class transport. This consolidates the container agent under `fleet-agent`, setting up a persistent daemon and session reattach in the next phase.
