---
cargo/fleet-commander: patch
---

Fix a silent failure in the cross-workspace inbox: when a `send_to_workspace`
message's target agent was no longer available (e.g. its container/session
was closed after pairing), the message was dropped with only a
`tracing::warn!` — nothing was ever shown in the TUI, so the sender had no
indication delivery failed. This now surfaces a status message
("could not be delivered — target agent '<id>' is no longer available") so
the failure is visible instead of silent.
