# fleet-commander

## 0.3.0 — 2026-07-01

### Minor changes

- [835ce6b](https://github.com/culpeo-labs/fleet-commander/commit/835ce6bf5dc4826b2148de1d373f483e0503c46a) Read large files from the in-container service in bounded chunks instead of one giant frame: `fs.read` now accepts an `offset`/`len` range and reports `eof`/`total_size`. The explorer file preview is capped (256 KiB) so opening a huge file no longer transfers or buffers it in full, showing a truncation marker instead.
- [cdac2f1](https://github.com/culpeo-labs/fleet-commander/commit/cdac2f125e2e25e581929100124aec0019f26fdf) Search the container workspace from the explorer: press `/` (with a container-backed view focused) to type a query, then Enter to run it. Matches stream live into a new side pane — navigate with `↑/↓` and press Enter to jump the file preview straight to the hit's line. The pane shows a running indicator and a final summary (match count, truncated/cancelled); starting a new search or dismissing the pane cancels the previous run.
- [6a54e24](https://github.com/culpeo-labs/fleet-commander/commit/6a54e2459cae6928f2a857555f2980bd353b51cf) Content search in the in-container service: `fs.search` walks the workspace (backed by ripgrep's `ignore` + `grep` crates, so it honors `.gitignore` and skips hidden files) and streams `fs.searchResult` match batches followed by a final summary reporting the total count and whether the run was truncated or cancelled. In-flight searches can be stopped with `fs.cancelSearch`. This lands the search engine and wire protocol; the search UI follows.
- [124b69d](https://github.com/culpeo-labs/fleet-commander/commit/124b69d3b93757e3d8e0405d89b2b85ee92f3ce8) Wire the in-container search into the client: `ServiceFs` now exposes `start_search`/`cancel_search`, and the daemon streams results as notifications so a long-running search never blocks other requests (including its own cancel). `fs.search` returns an immediate ack, streams `fs.searchResult` batches, and finishes with an `fs.searchDone` summary; an invalid pattern is rejected up front. Results flow through the existing notification sink, setting up the search UI to follow.
- [a678221](https://github.com/culpeo-labs/fleet-commander/commit/a678221ff1a7f023d4df6fc66956eb2102389801) View a file's git diff from the explorer: press `Shift+D` on a changed file to open its working-tree diff in the side pane. Backed by a new in-container `git.diff` method (untracked files render as all-additions), so diffs reflect what the agent sees inside the container.

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

