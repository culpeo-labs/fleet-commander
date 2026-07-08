# fleet-commander

## 0.4.0 — 2026-07-08

### Minor changes

- [b626d67](https://github.com/culpeo-labs/fleet-commander/commit/b626d6760f792f380b2096dc78012e53cf35742c) Add cross-workspace pairing state and connect UX. New `:connect`,
  `:disconnect`, and `:connections` commands let you link two workspace agents
  into an undirected pair. Pairings are persisted globally to
  `~/.config/fleet-commander/pairings.yaml` and gate the upcoming
  cross-workspace messaging tools.
- [572da1a](https://github.com/culpeo-labs/fleet-commander/commit/572da1a155716204bacde7f051ae7a10ddff426a) Collapse a container agent's explorer `fs`/`git` traffic and its ACP `session.*` protocol onto a **single** `docker exec` bridge (Phase 4b2 y3, final part).
  
  Previously the host opened up to three separate `docker exec` connections per started agent: a watched explorer `ServiceFs`, a one-shot git-branch read, and the daemon-owned session. Now the daemon-owned session driver builds the explorer `ServiceFs` over a shared clone of the **same** transport, starts its `fs.watch` subscription, reads the git branch, and delivers them to the app via new `SessionEvent::ExplorerFs`/`AgentBranch` events; live `fs.didChange` pushes and `fs.search` results ride the same connection and are routed through new `ExplorerFsChanged`/`SearchResults`/`SearchDone` events. The app stores the delivered filesystem per-agent so re-entering a session re-installs it instead of dialing a fresh bridge, and drops it when the agent exits so the underlying `docker exec` is torn down. This builds on the daemon- and host-side request concurrency landed earlier in y3, so a slow `session.start` handshake no longer blocks the explorer on the shared connection.
- [cb04b17](https://github.com/culpeo-labs/fleet-commander/commit/cb04b17a4ce206762d464e885b9d67929f52d3f6) Run `fleet-agent` as a **persistent in-container daemon** the host reattaches to, instead of a fresh `docker exec` process per connection. The devcontainer `postStartCommand` now starts `fleet-agent serve --socket …` (idempotently — a redundant launch exits when it finds a live socket), and the host connects through a new `fleet-agent bridge` relay over `docker exec -i`, portable across native Docker and Docker Desktop. The daemon serves the fs/watch channel and the ACP tunnel concurrently (a thread per connection) and keeps listening across client disconnects, so it survives a TUI restart. This is the transport foundation for daemon-scoped state and session reattach.
  
  Note: existing workspaces must be re-initialized (`fleet-commander init`) and their container restarted to pick up the daemon `postStartCommand`.
- [8b6c4cd](https://github.com/culpeo-labs/fleet-commander/commit/8b6c4cd5aa11d334736c64bafd6f4f8cbd8cfa74) Route MCP relay tunnels through the fleet-agent daemon (Feature 2 F2a2b-1). The
  daemon now bridges an in-container `fleet-agent mcp` relay connection to its
  session's attached host: `mcp.bind{token}` resolves the owning session (keyed by
  cwd) and opens a tunnel (`mcp.open` to the host), then `mcp.data` flows both ways
  — stamped with the daemon-assigned tunnel id on the host-facing hop — and
  `mcp.close` (or a relay disconnect) tears it down. Tunnel frames are delivered
  "live" so they never pollute the session's replay buffer. `session/new`
  injection that auto-spawns the relay follows in the next change.
- [20d4ca3](https://github.com/culpeo-labs/fleet-commander/commit/20d4ca364171d1a152d1ec592d7bc6369e8736fa) Define the daemon-owned **session protocol** (`session.*`) that lets `fleet-agent` own the ACP client and expose a higher-level session-observer contract to the host, superseding the raw `acp.*` byte tunnel for the container path. The host asks the daemon to start (or resume) a session with `session.start` and drives it with a `session.prompt` notification, `session.cancel`, and `session.permissionRespond`; the daemon streams progress back as `session.update` (raw ACP `session/update` JSON the host aggregates itself), `session.permissionRequest`, `session.promptResult`, `session.connected`, `session.output`, `session.error`, `session.exit`, and `session.authRequired`. A new `capabilities.session` flag advertises support (defaulting to `false` for older daemons). This is the wire contract for making sessions survive TUI restarts and, later, fan out to multiple clients for cross-workspace injection.
- [8c8fb81](https://github.com/culpeo-labs/fleet-commander/commit/8c8fb818d6f7e7b0951b18d90f031d79c5a89969) Add the `fleet-agent mcp` relay subcommand (Feature 2 F2a2a): an in-container
  process the coding agent spawns as a stdio MCP server, which translates MCP's
  newline-delimited JSON stdio ↔ the daemon's `Content-Length`-framed `mcp.*`
  notifications. It announces itself with an `mcp.bind{token}` handshake so the
  daemon can resolve which session's host to bridge to, then relays MCP requests
  out (`mcp.data`) and host responses back in, terminating on `mcp.close` or when
  the agent's MCP client disconnects. Daemon-side routing + `session/new`
  injection land in the next change.
- [53c2a0e](https://github.com/culpeo-labs/fleet-commander/commit/53c2a0e33539893b89cc3488f30a9f972ef77115) Make the host's `fleet-agent` transport serve **concurrent requests** (Phase 4b2 y3, part 2).
  
  Previously `ProcessTransport.call` was strictly serial: a single locked call channel meant "the next response on the wire is mine", so a slow request (e.g. a `session.start` handshake) head-of-line-blocked every other call on the same connection. The transport now multiplexes: each in-flight request registers a per-id waiter before it writes, the reader thread routes each response to its matching waiter by id, and writes only briefly hold the stdin lock (released before awaiting the reply). On EOF or a protocol error the reader drains all pending waiters with a broken-pipe error so no call sits out its full deadline. This lets the explorer's `fs.*` traffic and a long-running `session.*` handshake share one connection without freezing each other — the prerequisite for collapsing the per-agent `fs`/`git`/`session` bridges onto a single `docker exec`.
  
  The `ServiceFs` now also holds its transport behind an `Arc` so it can be shared with the session driver in the follow-up unification.
- [26ac981](https://github.com/culpeo-labs/fleet-commander/commit/26ac981293223846183c6b5fc52fb096b89d3c83) Inject the in-container MCP relay into the ACP session (Feature 2 F2a2b-2). When
  the host opts in (`SessionStartParams.mcp`), the daemon now injects a stdio MCP
  server into `session/new` (and `session/resume`/`session/load`) pointing the
  in-container agent at `fleet-agent mcp --socket <daemon-socket> --token <cwd>`,
  so the agent's MCP client dials back into the daemon and its frames tunnel to the
  attached host over the existing session connection. The daemon records its own
  socket path (`DaemonState::with_socket`) and flips `capabilities.mcp` to true. An
  end-to-end test drives a recording fake ACP agent and asserts the injected stdio
  server's command and args.
- [c6e1f5d](https://github.com/culpeo-labs/fleet-commander/commit/c6e1f5d688f0d25f9d22fed047b3f679448421d6) Add the `mcp.*` tunnel wire protocol to `fleet-protocol` (Feature 2 foundation):
  `mcp.open`/`mcp.data`/`mcp.close` frames, a `capabilities.mcp` flag, a
  `SessionStartParams.mcp` opt-in, and `McpTunnelParams`/`McpDataParams`. This is
  protocol scaffolding only — it lets the daemon relay an in-container MCP stream to
  the host over the existing session connection (no host port exposed), so
  cross-workspace tooling can reach the host MCP server. The relay itself lands in
  follow-up changes.
- [b253ae7](https://github.com/culpeo-labs/fleet-commander/commit/b253ae7fb2f0d4faeed66830c2ec52534fd0cb18) Drive the **daemon-owned ACP session from the host** for container agents (Phase 4b2). The host container path now speaks the higher-level `session.*` protocol instead of tunnelling raw ACP stdio: it connects a dedicated `fleet-agent` connection, issues `session.start`, and — on success — forwards TUI prompts as `session.prompt` notifications. Progress flows back through a notification sink that feeds forwarded `session.update`s into the host's existing `SessionStateMachine`, relays `session.permissionRequest` (answered via `session.permissionRespond`), and surfaces `session.connected`/`output`/`error`/`exit` as the usual `SessionEvent`s. Interactive login is taken from the `session.start` result and wrapped with `docker exec -it`.
  
  Because the daemon now owns the ACP client and session, the session survives a TUI exit/restart. Daemons that predate `capabilities.session` transparently fall back to the Phase 4a `acp.*` tunnel; the host (non-container) path is unchanged and still drives the ACP client directly.
- [9dc1131](https://github.com/culpeo-labs/fleet-commander/commit/9dc1131c0c6e59f78f90be1b7ea2fb386f45fbb8) Make `fleet-agent` **own the ACP coding-agent session** (Phase 4b2). The daemon now runs the ACP client itself: on `session.start` it spawns the agent, runs the `initialize` → authenticate → resume-or-`session/new` handshake once, and keeps the connection alive at daemon scope. Host prompts arrive as `session.prompt` notifications and are forwarded to the agent (`session.promptResult` reports each turn); the agent's `session/update` stream, permission requests, and diagnostics are relayed back as `session.update` / `session.permissionRequest` / `session.output`, with the host answering permissions via `session.permissionRespond`. Session resume (`session/resume`/`session/load`/`session/list`) now lives in the daemon too, so a resumed id survives independently of the host connection. The daemon advertises `capabilities.session = true`.
  
  The ACP client is async (adds `tokio` + `agent-client-protocol` to the daemon) but runs on a dedicated per-session runtime thread behind a synchronous handle, so the existing serve loop is unchanged. Covered by an end-to-end test that drives `session.start`/`session.prompt` against a fake ACP agent. The host still uses the Phase 4a `acp.*` tunnel for the container path until the host rewire lands next.
- [1b8994b](https://github.com/culpeo-labs/fleet-commander/commit/1b8994b2e7efd6e3634a097df10b344766cb5387) Add the `send_to_workspace` MCP tool and a cross-workspace message inbox. A
  connected agent can now send a message to a paired workspace's agent; the tool
  authorizes the target against the pairing set and queues the message. Incoming
  messages surface as a per-message approval modal — the user approves or rejects
  each one, and only approved messages are injected into the target agent's
  session (framed as a cross-workspace message). Cross-workspace traffic is thus
  always human-gated.
- [5c0d453](https://github.com/culpeo-labs/fleet-commander/commit/5c0d453dfd8155a057f32e8936de75eb7fed9e61) Add the `list_connected` MCP tool for cross-workspace messaging. When an
  in-container agent's MCP client is served over a per-agent tunnel, it can now
  call `list_connected` to discover which other workspaces the user has paired it
  with (via `:connect`). The tool is scoped to the calling agent's identity and
  reads the live pairing store, so results always reflect the current pairings.
  It is unavailable on the legacy always-on HTTP server (no caller identity).
- [1070fe9](https://github.com/culpeo-labs/fleet-commander/commit/1070fe96adc85acb7f995461ad3078c84b6e4f9d) Tunnel the ACP coding agent through `fleet-agent` inside the container. Instead of opening a separate `docker exec copilot --acp --stdio` channel, the host now asks the in-container daemon to spawn and own the ACP child: `acp.start` launches it, `acp.send`/`acp.recv` relay stdio as fire-and-forget notifications (so a long-running prompt never head-of-line-blocks the request/response channel), `acp.stderr` forwards diagnostics, and `acp.exit`/`acp.stop` handle teardown. The daemon advertises the new capability via `capabilities.acp`, and the host drives the ACP client over this tunnel as a first-class transport. This consolidates the container agent under `fleet-agent`, setting up a persistent daemon and session reattach in the next phase.
- [7bd3458](https://github.com/culpeo-labs/fleet-commander/commit/7bd34582fad617a8a7798221ae59dc2e01ec09da) Route cross-workspace replies back to the sender via correlation ids. The
  `send_to_workspace` tool now takes an optional `thread` id: omit it to start a
  new exchange (the ack reports the generated id), or echo a received `thread`
  to reply. Delivered messages are framed with the sender's workspace id, the
  thread id, and an instruction to reply via `send_to_workspace` — so a reply
  flows back through the same inbox + approval path, letting two agents hold a
  threaded request/response conversation.
- [59082a4](https://github.com/culpeo-labs/fleet-commander/commit/59082a496ed868e24e62cca4bdf3363defd4bb5e) Serve the host `TuiMcpServer` over the cross-workspace MCP tunnel (Feature 2
  F2a3). The host now opts into the tunnel (`SessionStartParams.mcp = true`) and,
  when the daemon opens one (`mcp.open`), bridges the in-container agent's MCP
  frames onto an in-process duplex: agent→host `mcp.data` messages are fed into
  the stream and the server's newline-JSON responses are sent back as
  host→agent `mcp.data`. The TUI serves a `TuiMcpServer` over the duplex per
  tunnel (rmcp `serve_with_ct`), cancelling it on `mcp.close`. This lets an
  in-container coding agent reach the TUI's MCP tools without any host port,
  closing the loop opened by the daemon relay/injection changes.
- [b71d50d](https://github.com/culpeo-labs/fleet-commander/commit/b71d50d8f9059a1973a9f5e63a51b1525e5af079) Serve `fs.*`/`git.*` requests **concurrently with a session's ACP handshake** (Phase 4b2 y3, part 1).
  
  Previously the `fleet-agent` connection's dispatch loop handled `session.start` inline, blocking until the in-container ACP handshake (initialize + auth + resume) resolved — several seconds. Any filesystem/git request on the same connection had to wait behind it. `session.start` now runs on its own worker thread: it publishes the resulting session into a shared per-connection slot and writes the response frame itself once the handshake resolves, leaving the read loop free to answer `fs.*`/`git.*` immediately. Ordering is preserved — the slot is set before the response is sent, so a `session.prompt` that follows the reply always finds the session.
  
  This unblocks unifying the host's explorer/git traffic and the session onto a single `docker exec` bridge (next change) without freezing the explorer during agent startup. Covered by a new integration test that keeps `session.start` in flight against a slow-initializing agent and asserts an `fs.list` on the same connection is answered promptly.
- [4b6ef82](https://github.com/culpeo-labs/fleet-commander/commit/4b6ef820eb1fe13675328bdb9331ced3d183d898) Make daemon-owned ACP sessions **survive a TUI restart** (Phase 4b2 y2-reattach) — the bug that started this work.
  
  `fleet-agent` now holds sessions in a **daemon-scoped registry** shared across every client connection (`DaemonState`), instead of per-connection state that died when the client disconnected. Each session buffers its outbound `session.*` history and forwards it to whichever host is currently attached. When a host disconnects the connection is **detached** (not torn down), so the ACP agent and conversation keep running in the container. When a host reconnects, `session.start` for the same cwd **reattaches** to the live session and **replays the buffered history**, so the reconnecting TUI rebuilds the full conversation without spawning a new agent. A session is only retired when its ACP child exits on its own; the next `session.start` then starts fresh.
  
  Covered by a new socket-daemon integration test that starts a session + prompt turn on one bridge client, disconnects it, and asserts a second client reattaches to the same session id and replays the prior turn's update without prompting. (Follow-up: the replay buffer is currently unbounded; a future change can cap/compact it for very long sessions.)

### Patch changes

- [2fa9d6d](https://github.com/culpeo-labs/fleet-commander/commit/2fa9d6d4be17578521a3e82405f2062118d1fd29) Upgrade the `rmcp` MCP SDK to 2.1 and `agent-client-protocol` to 1.2. rmcp 2.0
  is a breaking release that renames the `Content` content type to `ContentBlock`
  and reshapes `CallToolResult.content` into a `Vec<ContentBlock>`; the TUI MCP
  server was updated accordingly. No user-facing behavior changes.

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

