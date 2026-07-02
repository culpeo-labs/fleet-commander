# In-Container Service (a.k.a. `fleet-agent`)

Status: **proposed** · Owner: @glecaros · Created: 2026-06-22

## Motivation

The git/file explorer (and, later, search, terminals, LSP, …) must reflect what
the **agent** sees — i.e. the workspace *inside* the dev container — not the host
filesystem. Today the explorer uses `LocalFs`, which reads the host path; this is
only correct because dev containers usually bind-mount the host workspace. As soon
as a container uses a named volume, clones the repo itself, or its in-container git
state diverges, the explorer silently shows the wrong view.

Rather than proxy every operation through one-off `docker exec` calls, we inject a
small resident **service** into the container (in the spirit of the VS Code Server)
and talk to it over a single persistent connection. The service is a *platform*:
the explorer is its first tenant, not its purpose.

## What the service unlocks

- **Live everything** — inotify-backed push: live explorer + live git status, no polling.
- **Search** — in-container ripgrep-grade search, streamed.
- **Real git pane** — status, diff/hunks, blame, stage/unstage, commit, branch ops.
- **Terminal multiplexing** — PTYs in the container, streamed to TUI panes.
- **LSP hosting** — language servers in-container, proxied diagnostics/definitions/symbols.
- **Multi-session efficiency** — one daemon per container serving N agent sessions.
- **Workspace ops** — upload/download, snapshot/checkpoint, process & resource stats.
- **Transport uniformity** — same protocol over docker-exec stdio, SSH, or locally.

## Architecture

Three crates:

- **`fleet-protocol`** — wire types. **JSON-RPC 2.0 over stdio**, in the style of ACP
  (which we already speak to the agent): request/response + server→client
  notifications. One mental model across both channels; can likely reuse the
  `agent-client-protocol` crate's generic JSON-RPC connection/transport layer rather
  than hand-rolling or pulling `jsonrpsee`. See *Protocol (detail)* and
  *Deferred: CulpeoStream* for why CulpeoStream was considered and deferred.
- **`fleet-agent`** — the daemon. Depends only on `fleet-protocol` + fs/git/watch
  crates. **Dependency footprint kept deliberately small** (binary size matters —
  it's bind-mounted in; cold start matters too). Not the TUI binary.
- **TUI (client)** — a new `ServiceFs` impl of `WorkspaceFs` speaks the protocol.
  `LocalFs` remains for the no-daemon path.

`WorkspaceFs` (already in `fleet-commander-core`) is the insulation seam: `ServiceFs`
drops in behind it, so the explorer is untouched and every later capability is
additive behind a capability flag.

### Key decisions

- **Transport = stdio over `docker exec -i`.** No ports, no network auth (inherits
  docker's), no firewall story. Exactly the ACP pattern (`copilot --acp --stdio`).
  Framing = LSP/DAP/ACP-style `Content-Length: N\r\n\r\n<json>`.
- **`initialize` handshake from day one** — protocol version + capability negotiation
  (mirrors ACP/LSP). Client degrades gracefully; protocol can evolve.
- **Paths are workspace-relative**; the server holds the root. Keeps the client
  transport-agnostic and avoids leaking absolute container paths.
- **Injection = devcontainer base-layer bind-mount (preferred over `docker cp`).**
  Fleet Commander already merges a base layer (`base_layer.rs` → `merge_layer`) into
  every project's devcontainer, including injected mounts. Extend it to bind-mount the
  host-built `fleet-agent` binary (read-only) at a fixed path (e.g.
  `/opt/fleet/bin/fleet-agent`), then `docker exec -i … /opt/fleet/bin/fleet-agent
  serve --stdio`. No image rebuild, no `docker cp`; updating Fleet Commander updates
  the agent automatically. CI builds static musl binaries (amd64 + arm64); the mount
  selects the binary matching the container arch (one `uname -m` probe for the
  host≠container case). Version handshake on connect.

## Roadmap

Each phase ships value and de-risks the next.

- **Phase 0 — protocol + daemon, run locally first.** ✅ *Done.* `fleet-protocol`
  (wire types + `Content-Length` stdio framing), `fleet-git` (extracted std-only git
  helpers, shared with the daemon), `fleet-agent` (daemon serving
  `fs.list`/`fs.read`/`fs.stat`/`git.status`/`git.branch` + `initialize`), and a
  `ServiceFs` client behind `WorkspaceFs` (generic over a `Transport`; `ProcessTransport`
  spawns the agent). Proven end-to-end across a real process boundary in tests.
- **Phase 1 — inject into containers via the base-layer bind-mount.** ✅ *Done.*
  A statically-linked (musl) `fleet-agent` is built via `scripts/build-fleet-agent.sh`
  into `~/.local/share/fleet-commander/bin/fleet-agent-<arch>` (override with
  `FLEET_AGENT_BIN`; see `core::agent_bin`). On fresh container create the base layer
  bind-mounts it read-only at `/opt/fleet/bin/fleet-agent`; the commander connects over
  `docker exec -i` stdio (`ServiceFs::connect_docker`) and the explorer/git pane is
  upgraded from the host `LocalFs` to the container-backed `ServiceFs` once
  `SessionEvent::ContainerReady` fires. Delivers the original correctness goal (the
  agent's real view). Guardrails from the architecture review: remote fs calls are
  served from a render-safe cache (background `spawn_blocking` loads); the transport has
  a request deadline and marks itself unhealthy on hang/EOF; and the `ServiceFs` is tied
  to the container generation (the `container_id` is validated on install, and a
  `:rebuild`/`:close` drops the remote fs and its `docker exec` child). Git branch
  **and** status are both read from the container service (never the host bind-mount)
  so they can never disagree; branch is shown only while a container is started.
  Arch selection happens **inside** the container: every available per-arch binary is
  mounted at `/opt/fleet/bin/fleet-agent-<slug>` and a launcher at
  `/opt/fleet/bin/fleet-agent` `exec`s the one matching `uname -m`, so it stays correct
  under qemu emulation / `--platform` (not just the Docker host's arch). The daemon's
  path resolver canonicalizes and re-checks containment, so an in-workspace symlink
  can't escape the root. *Caveats:* pre-existing containers created before Phase 1 lack
  the mount and silently fall back to `LocalFs` until rebuilt (`:rebuild`); and
  bind-mount delivery doesn't cover remote Docker (`DOCKER_HOST`), which would need
  `docker cp` or a registry image. *(Resolved:* the dual-arch cross-compile is now
  done — the release workflow `cargo-zigbuild`s both `x86_64`/`aarch64` musl agents and
  embeds them via the `embed-agent` feature, so the emulation win materializes in
  release builds.*)*
- **Phase 2 — file watching / push.** ✅ *Done.* The daemon advertises a `watch`
  capability; when the client connects with a notification sink
  (`ServiceFs::connect_docker_watched`) it sends `fs.watch { enable: true }`, and the
  daemon uses `notify` (inotify) to watch the workspace root recursively, coalesces
  bursts over a short window, and pushes `fs.didChange` notifications carrying
  workspace-relative paths. `ProcessTransport`'s reader thread demultiplexes responses
  from notifications (`Incoming::from_slice`), routing pushes to the sink; the TUI maps
  them to `AppEvent::ExplorerFsChanged`, which (when the agent is still viewed and on
  the same container) re-lists the tree and refreshes git status. Notifications are
  treated as refresh hints, not authoritative diffs; the non-watched `connect_docker`
  path and `LocalFs` fallback remain available.
- **Phase 3 — search, streaming reads/diffs, git pane.** Streamed as notifications;
  large reads chunked. (Re-evaluate CulpeoStream here if binary throughput hurts.)
  _Chunked reads landed: `fs.read` is ranged (`offset`/`len` → `eof`/`totalSize`); the
  client pages large files and the explorer preview is capped. Per-file diffs landed:
  `git.diff` + a `Shift+D` explorer binding open a file's working-tree diff in the side
  pane. Streaming search landed (engine + client): `fs.search` walks the workspace
  (ripgrep's `ignore` + `grep` crates, gitignore-aware), returns an immediate ack, then
  streams `fs.searchResult` batches ending with an `fs.searchDone` summary notification;
  `fs.cancelSearch { searchId }` stops an in-flight search. `ServiceFs` exposes
  `start_search`/`cancel_search`, routing results through its notification sink. The
  search UI landed: `/` in the explorer opens a query prompt, matches stream into a
  side pane (navigate with `↑/↓`, Enter jumps the preview to the hit's line), and the
  pane shows a running indicator plus a final count/truncated/cancelled summary;
  starting a new search or dismissing the pane cancels the previous run._
- **Phase 4 — PTY/terminal multiplexing.** First strong case for binary streams —
  evaluate a CulpeoStream side-channel (see *Deferred: CulpeoStream*).
- **Phase 5 — LSP hosting + SSH/WebSocket transport.**

## Protocol (detail)

> Working assumptions; refined as Phase 0 lands.

- **Base: JSON-RPC 2.0 over stdio, in the style of ACP** (which we already speak to
  the agent — `agent-client-protocol` 0.14). Three message kinds: Request
  (`id`,`method`,`params`), Response (`id`,`result`|`error`), Notification
  (`method`,`params`). ACP itself proves this shape handles fs + terminals fine — it
  defines `fs/read_text_file`, `fs/write_text_file`, `terminal/create`,
  `terminal/output`, `terminal/wait_for_exit`, etc. (Those are *agent→client* and
  domain-specific, so we don't reuse ACP's method set — only its style/transport, and
  ideally its generic JSON-RPC connection layer.)
- **Framing:** LSP/DAP/ACP-style `Content-Length: N\r\n\r\n<json>` over stdio. Robust
  to embedded newlines; ecosystem-standard; no custom binding to author.
- **Lifecycle:** `initialize` → `initialized`; `shutdown`/`exit`. `initialize`
  advertises `protocolVersion`, `serverInfo`, and `capabilities`
  (`watch`, `search`, `git`, `acp`, `pty`, `lsp`, …). Client degrades gracefully.
- **Streaming (watch/search):** server→client **notifications** (how LSP streams
  diagnostics and ACP streams `session/update`). No multi-stream substrate needed for
  Phases 0-3.
- **Errors:** JSON-RPC error objects with an app code space (NotFound, NotARepo,
  PermissionDenied, Io, …).
- **Cancellation:** in-flight searches are cancelled by a domain method,
  `fs.cancelSearch { searchId }`, keyed on the caller-supplied `searchId` (simpler than
  LSP `$/cancelRequest` given the client transport assigns JSON-RPC ids internally).
- **Methods (initial):**
  - `fs.list { path, depth? }` → `{ entries: [{ name, isDir }] }`
  - `fs.read { path, offset?, len? }` → `{ contentBase64, eof, totalSize }` (ranged/chunked reads; client pages large files, explorer preview is capped)
  - `fs.stat { path }`
  - `fs.watch { path }` / `fs.unwatch` → server `fs.didChange { changes: [{path, kind}] }` (debounced/coalesced)
  - `fs.search { searchId, query, isRegex?, caseSensitive?, maxResults? }` → immediate `{ accepted }` ack, then streams `fs.searchResult { searchId, matches: [{path, line, column, text}] }` notifications, ending with `fs.searchDone { searchId, count, truncated, cancelled }`
  - `fs.cancelSearch { searchId }` → `{ cancelled }`
  - `git.status { includeIgnored }` → `{ entries: { path: kind } }`
  - `git.diff { path, staged? }` → `{ diff }` (unified diff for one path; untracked files render as all-additions)
  - `git.branch` → `{ branch? }`
  - `acp.start { command, cwd?, env? }` → `{ started }` — spawn the ACP coding agent (`copilot --acp --stdio`) *inside* the container, owned by `fleet-agent`. Idempotent: a second call while a child is running returns `{ started: false }`.
  - `acp.send { data }` (client→server **notification**) / `acp.recv { data }` (server→client **notification**) — relay one newline-delimited ACP wire line each way. Deliberately notifications, not requests, so a long-running prompt never head-of-line-blocks the request/response channel (nor trips the request timeout).
  - `acp.stderr { data }` (server→client notification) — one line of the child's stderr (device-code/auth prompts, diagnostics).
  - `acp.exit { code? }` (server→client notification) — the child exited; `code` is `None` if killed by signal.
  - `acp.stop {}` → `{ stopped }` — terminate the running child.
- **ACP tunnel (Phase 4a):** the host no longer opens a separate `docker exec copilot --acp --stdio` channel. It runs the coding agent through `fleet-agent` via the `acp.*` methods above, and drives the ACP `Client` over the tunnel as a first-class line-based transport. Capability gate: `capabilities.acp`. This consolidates the container agent under one daemon connection, setting up a persistent daemon + session reattach in Phase 4b.
- **Modeling:** reuse the `agent-client-protocol` crate's generic JSON-RPC
  connection/transport if it's cleanly separable; otherwise hand-rolled serde types
  for the 3 message kinds + typed params/results per method (keeps `fleet-agent`
  dep-light; avoids `jsonrpsee`).

## Deferred: CulpeoStream

[CulpeoStream](https://github.com/culpeo-labs/culpeostream-spec) (our org's real-time
media streaming protocol — multiplexed `input`/`output`/`duplex` streams, binary
frames, per-stream resumption, keepalive, transport-agnostic) was evaluated as the
substrate. **Deferred, not rejected.**

- **Why it's tempting:** native binary multiplexed streams (PTY raw bytes, large
  streamed reads) and session resumption with per-stream offsets.
- **Why deferred:** those strengths only land on the critical path at **Phase 4+**
  (PTYs, large/streamed reads, remote-reconnect). For Phases 0-3, JSON-RPC
  notifications cover watch/search, and over `docker exec -i` a dropped pipe kills the
  exec anyway (resumption is moot until a persistent reconnectable daemon exists).
  Adopting it now would mean authoring a missing **stdio binding** (the spec only
  ships a WebSocket binding) and carrying offset/buffer/media machinery just to list a
  directory — over-engineering.
- **When to revisit:** Phase 4 (terminals) / Phase 5 (remote/WebSocket). Likely as a
  **binary side-channel** for PTY/media streams alongside the JSON-RPC control
  channel, or as the full substrate if multiplexing/resumption become pervasive. The
  `WorkspaceFs`/capability seam keeps this swappable.
