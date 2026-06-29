# fleet-commander

## 0.2.0 — 2026-06-25

### Minor changes

- [874fbbf](https://github.com/culpeo-labs/fleet-commander/commit/874fbbfce2a168c3d532c7439d8c50b2ce5ea1fd) Live file explorer: the in-container `fleet-agent` now watches the workspace
  (inotify) and pushes coalesced `fs.didChange` notifications, so the explorer
  tree and git status refresh automatically when files change inside the
  container — no manual `r` needed. The `ServiceFs` transport demultiplexes
  responses from these server-initiated notifications, and the watch falls back
  cleanly to on-demand refresh when unavailable.
- [892fe3c](https://github.com/culpeo-labs/fleet-commander/commit/892fe3c0b7f8c7b7dae03ef3f31805d6dbcde451) Inject the in-container `fleet-agent` daemon across architectures: per-arch
  static-musl binaries are mounted into the container and the right one is picked
  at exec time by a `uname -m` launcher. Release builds now embed both agents (the
  `embed-agent` feature), so the file/git explorer reflects the container's
  filesystem out of the box instead of falling back to the host. Also hardened the
  agent's path resolver against symlink escapes outside the workspace root.

