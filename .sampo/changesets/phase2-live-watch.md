---
cargo/fleet-commander: minor
---

Live file explorer: the in-container `fleet-agent` now watches the workspace
(inotify) and pushes coalesced `fs.didChange` notifications, so the explorer
tree and git status refresh automatically when files change inside the
container — no manual `r` needed. The `ServiceFs` transport demultiplexes
responses from these server-initiated notifications, and the watch falls back
cleanly to on-demand refresh when unavailable.
