# Architecture

Fleet Commander is a terminal UI for orchestrating multiple AI coding agents. The
operator points it at a directory of repositories, Fleet Commander discovers repos
with `.devcontainer/devcontainer.json`, starts dev containers for them, and runs an
ACP-compatible coding agent inside each container from one keyboard-driven UI.

The host-side TUI owns input, rendering, workspace configuration, and high-level
session state. The core crate owns dev-container lifecycle, ACP connections, and
the in-container file/git service client. A small `fleet-agent` daemon is injected
into containers so the explorer and git state reflect what the coding agent sees
inside the container, not just the host bind mount.

```text
┌─────────────────────────────────────────────────────────────────────┐
│ Host: fleet-commander TUI (ratatui + crossterm)                     │
│  ├─ App/AppEvent event loop, screens, keybindings, MCP server        │
│  └─ fleet-commander-core                                            │
│      ├─ container lifecycle via devcontainer-lib + Docker API        │
│      ├─ ACP runtime for coding agents                               │
│      └─ ServiceFs client for fleet-agent                             │
└───────────────┬───────────────────────────────────────┬─────────────┘
                │ docker exec -i                         │ docker exec -i
                │ JSON-RPC Content-Length frames          │ ACP stdio
                ▼                                         ▼
┌──────────────────────────────────┐      ┌───────────────────────────┐
│ Container: fleet-agent daemon     │      │ Container: coding agent    │
│  fs.list/read/stat, git status,   │      │  copilot --acp --stdio,    │
│  git branch, fs.watch → didChange │      │  claude-agent-acp, …       │
└──────────────────────────────────┘      └───────────────────────────┘
```

## Workspace crates

The workspace members are declared in `Cargo.toml`.

| Crate | Role | Depends on |
| --- | --- | --- |
| `crates/fleet-commander` | The binary TUI. It contains CLI/init flow, workspace persistence, `App` state machine, `AppEvent`, ratatui rendering, MCP tools, keybindings, and embedded-agent install hook. See `src/main.rs`, `src/app.rs`, `src/event.rs`, and `src/ui.rs`. | `fleet-commander-core`, `agent-client-protocol`, ratatui/crossterm, rmcp/axum, config/rendering deps |
| `crates/fleet-commander-core` | Frontend-agnostic runtime: dev-container lifecycle, ACP agent runtime, session handle model, workspace filesystem abstraction, `ServiceFs`, git helpers re-export, and `fleet-agent` binary resolution. See `src/lib.rs`. | `fleet-protocol`, `fleet-git`, `agent-client-protocol`, `devcontainer-lib` |
| `crates/fleet-protocol` | Dependency-light wire types and `Content-Length` stdio framing for the in-container service. Defines JSON-RPC requests, responses, notifications, capabilities, and methods such as `fs.list`, `git.status`, and `fs.watch`. See `src/lib.rs`. | `serde`, `serde_json` |
| `crates/fleet-agent` | Injected in-container daemon binary. Serves filesystem and git inspection for one workspace root over JSON-RPC stdio. See `src/lib.rs` and `src/main.rs`. | `fleet-protocol`, `fleet-git`, `notify`, `base64` |
| `crates/fleet-git` | Small git inspection helper shared by host and daemon. It parses `.git/HEAD` directly for branch names and shells out to `git status --porcelain=v1 -z` for status. See `src/lib.rs`. | std only at runtime |

## Host TUI and app loop

`crates/fleet-commander/src/main.rs` starts the TUI, installs embedded
`fleet-agent` binaries when present, loads `workspaces.yaml`, starts a shared MCP
server on `127.0.0.1:6100`, and bridges runtime `SessionEvent`s into `AppEvent`s.

`crates/fleet-commander/src/app.rs` is the state machine. The UI has two screens:
`AgentList` and `AgentSession`. A session screen contains the conversation, optional
file explorer, optional side pane (`Diff`, `FileView`, or `Commands`), input mode,
command mode, and a focus model (`Conversation`, `SidePane`, `Explorer`). Rendering
is split by component under `crates/fleet-commander/src/ui/`, with `ui.rs` selecting
the current screen.

All UI stimuli flow through `AppEvent` (`crates/fleet-commander/src/event.rs`):
keyboard input, MCP tool calls, runtime session events, repaint ticks from watched
message/tool handles, explorer background jobs, and container-backed filesystem
installation. The explorer state in `src/explorer.rs` is deliberately behind the
`WorkspaceFs` trait, so it can render either a host `LocalFs` or a container-backed
`ServiceFs`. Remote directory listing, file reads, and git status are dispatched via
blocking background tasks so the render path stays responsive.

## ACP agent runtime

Coding-agent communication uses ACP through the `agent-client-protocol` crate
(`Cargo.toml`, `crates/fleet-commander-core/Cargo.toml`). The configured command is
stored per workspace (`crates/fleet-commander/src/workspace.rs`); Copilot currently
maps to `copilot --acp --stdio` in `src/agent_kind.rs`.

`crates/fleet-commander-core/src/agent_runtime/mod.rs` starts a persistent agent
connection. If an agent has a workspace folder, it first starts the dev container,
emits `SessionEvent::ContainerReady`, and wraps the ACP command with:

```text
docker exec -i -u <remote_user> -w <remote_workspace> <container_id> <acp_command>
```

`agent_runtime/connection.rs` then builds an ACP client, sends `initialize`, handles
agent authentication, resumes or loads prior sessions when supported, creates a new
session otherwise, forwards prompts as `PromptRequest`, and maps ACP notifications
into Fleet Commander's handle-based `SessionEvent` model.

## Dev-container orchestration

Container work lives in `crates/fleet-commander-core/src/container/`. The code uses
`devcontainer-lib` from `https://github.com/glecaros/dev.git` on the `staging`
branch (`Cargo.toml`) and the crate docs describe it as bollard-based Docker API
integration (`container/mod.rs`, `container/lifecycle.rs`). The README explicitly
states that Fleet Commander does not require the Node `@devcontainers/cli`.

The `init` command (`crates/fleet-commander/src/init.rs`) scans the chosen root and
its immediate children for `.devcontainer/devcontainer.json`, asks which projects to
register, writes `~/.config/fleet-commander/workspaces.yaml`, and generates a
per-workspace base devcontainer layer under Fleet Commander's config directory.
That layer can inject agent features, environment, mounts, and post-start fixups.
At startup, `container/config.rs` merges the project devcontainer with that base
layer, `container/image.rs` resolves or builds the image, and `container/lifecycle.rs`
creates/starts/reuses containers, runs lifecycle hooks, and returns the container id,
remote user, and remote workspace path.

## In-container file/git service

The service design is documented in `docs/design/in-container-service.md` and
implemented by three crates:

- `fleet-protocol` defines JSON-RPC 2.0 envelopes, method params/results,
  notifications, error codes, capability negotiation, and LSP/DAP/ACP-style
  `Content-Length: N\r\n\r\n<json>` framing.
- `fleet-agent` serves a fixed workspace root. It handles `initialize`, `fs.list`,
  `fs.read` (base64 bytes), `fs.stat`, `git.status`, `git.branch`, and `fs.watch`.
  Its resolver rejects absolute paths, `..`, and symlink escapes from the workspace
  root.
- `ServiceFs` in `crates/fleet-commander-core/src/service_fs.rs` implements
  `WorkspaceFs` by sending typed RPCs to `fleet-agent`. If the daemon is unavailable,
  callers keep using `LocalFs` (`workspace_fs.rs`).

Transport is stdio over `docker exec -i`, not a port. `ProcessTransport` owns the
child process, serializes calls with a mutex, enforces a 30-second request timeout,
and kills/marks the transport unhealthy on EOF, timeout, or I/O failure. Its reader
thread demultiplexes incoming frames: responses go back to the pending call, while
server-initiated notifications go to an optional `NotificationSink`.

Phase 2 adds live filesystem push. A watched connection sends `fs.watch { enable:
true }` when the daemon advertises `capabilities.watch`; the daemon uses `notify` to
watch recursively, coalesces bursts, and pushes `fs.didChange` notifications with
workspace-relative paths. The client treats these notifications as refresh hints,
not authoritative diffs. The non-watched `connect_docker` path remains available, so
manual/background refresh and `LocalFs` fallback still work.

## Agent binary injection and release builds

Release builds can embed static-musl `fleet-agent` binaries into the commander with
the `embed-agent` feature (`crates/fleet-commander/Cargo.toml`).
`crates/fleet-commander/build.rs` reads absolute `FLEET_AGENT_X86_64` and
`FLEET_AGENT_AARCH64` paths, stages them into `OUT_DIR`, and generates the table used
by `src/embedded_agent.rs`. At TUI startup, `embedded_agent::install_embedded_agents`
writes those bytes into Fleet Commander's host data bin directory.

`crates/fleet-commander-core/src/agent_bin.rs` defines the in-container location
`/opt/fleet/bin/fleet-agent`. In normal embedded builds, every available per-arch
binary is mounted read-only at `/opt/fleet/bin/fleet-agent-<arch>`, plus a small
launcher script at `/opt/fleet/bin/fleet-agent` that executes the binary matching
`uname -m` inside the container. `FLEET_AGENT_BIN` is a developer override for
mounting a single explicit binary. If no suitable binary is found, the explorer uses
host-side `LocalFs`.

The release workflow (`.github/workflows/release.yml`) installs `cargo-zigbuild`,
cross-builds `fleet-agent` for `x86_64-unknown-linux-musl` and
`aarch64-unknown-linux-musl`, builds `fleet-commander` with `--features embed-agent`,
and uses Sampo to prepare/publish the release and upload the commander binary.
