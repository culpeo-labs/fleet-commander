# Fleet Commander

A terminal UI for orchestrating multiple AI coding agents. Point it at a
directory of repos, spin up dev containers, and command a fleet of agents —
Copilot, Claude, or any [ACP](https://agentclientprotocol.com)-compatible
agent — from a single keyboard-driven interface.

Inspired by [Norton Commander](https://en.wikipedia.org/wiki/Norton_Commander).

![Rust](https://img.shields.io/badge/rust-2024_edition-orange)
![License](https://img.shields.io/badge/license-MIT-blue)
[![CI](https://github.com/culpeo-labs/fleet-commander/actions/workflows/ci.yml/badge.svg)](https://github.com/culpeo-labs/fleet-commander/actions/workflows/ci.yml)

> ⚠️ Early development — APIs, on-disk formats, and keybindings can still
> change without notice.

## Features

- **Agent-agnostic** — works with any [ACP](https://agentclientprotocol.com)
  agent: GitHub Copilot CLI (`copilot --acp --stdio`), Claude Code
  (`claude-agent-acp`), and more.
- **Dev container isolation** — each agent runs inside a
  [dev container](https://containers.dev) built from the repo's
  `.devcontainer/` config. No Node-based `@devcontainers/cli` needed —
  containers are managed natively via [`devcontainer-lib`] and the Docker
  API.
- **Session resume** — Fleet Commander remembers the last session id per
  workspace and asks the agent to rehydrate it on reconnect, so prior
  turns reappear in the conversation pane.
- **Streaming UI with sticky scroll** — incoming messages never yank the
  viewport; press `G` to re-engage follow-bottom (vim style). Tool calls
  collapse to a single line that flips from `⏳` to `✓`/`✗` in place.
  Assistant messages render through a markdown pipeline once complete.
- **MCP server** — built-in [MCP](https://modelcontextprotocol.io) server on
  `127.0.0.1:6100` so agents can push diffs, files, and notifications back to
  the TUI.
- **Vim-style keybindings** — fully configurable via TOML, with a small
  modifier DSL (`C-`, `S-`, `M-`).
- **ACP wire logging** — capture every protocol message to a file with
  `--acp-log`, optionally filtered to a single agent with
  `--acp-log-filter`.

## Quick start

### Prerequisites

- **Rust** (edition 2024, stable toolchain)
- **Docker** (any engine reachable via the default socket)
- At least one ACP-compatible agent installed locally or available inside
  the dev container images you point at. For example:
  - [GitHub Copilot CLI](https://docs.github.com/en/copilot/github-copilot-in-the-cli)
    (`copilot --acp --stdio`)
  - [Claude Agent ACP](https://github.com/agentclientprotocol/claude-agent-acp)

### Install

```bash
git clone https://github.com/culpeo-labs/fleet-commander.git
cd fleet-commander
cargo install --path crates/fleet-commander
```

### Initialize a workspace

Point Fleet Commander at a directory that contains one or more repos with
`.devcontainer/devcontainer.json` files:

```bash
cd ~/projects
fleet-commander init
```

The `init` flow:

1. Asks which ACP agent you want to use across this workspace.
2. Scans the current directory (one level deep) for projects with a
   `.devcontainer/` folder.
3. Confirms which projects to add as agents.
4. Generates a per-workspace base credential layer the dev containers can
   mount, so the chosen agent's local credentials are available inside.
5. Persists the selection to
   `~/.config/fleet-commander/workspaces.yaml`.

### Run the TUI

```bash
fleet-commander
```

The TUI loads the agents from `workspaces.yaml`. Pick one with `j/k`,
press `Enter` to open the session screen, press `i` to enter input mode,
type a prompt, and hit `Enter` again to send it. The agent's dev container
is started on demand the first time you connect.

## Usage

### CLI flags

| Flag                          | Description                                                                                                                |
| ----------------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| `fleet-commander init [path]` | Run the onboarding flow against `path` (default: `.`).                                                                     |
| `--acp-log <FILE>`            | Append every ACP wire message (both directions) to `FILE`, one line per message prefixed with `>>` (sent) or `<<` (received). |
| `--acp-log-filter <PATTERN>`  | When `--acp-log` is set, only log messages for agents whose id contains `PATTERN`. Useful with multiple agents.            |

### Keybindings

| Key       | Context     | Action                                              |
| --------- | ----------- | --------------------------------------------------- |
| `q`/`C-c` | Anywhere    | Quit                                                |
| `j` / `↓` | Agent list  | Move selection down                                 |
| `k` / `↑` | Agent list  | Move selection up                                   |
| `Enter`   | Agent list  | Open the selected agent's session                   |
| `i`       | Session     | Enter input mode                                    |
| `Enter`   | Input mode  | Send the message                                    |
| `Esc`     | Input mode  | Cancel input                                        |
| `Esc`     | Session     | Back to agent list                                  |
| `↑` / `k` | Session     | Scroll conversation up (exits follow-bottom mode)   |
| `↓` / `j` | Session     | Scroll conversation down                            |
| `G`       | Session     | Re-engage follow-bottom (snap to and track newest)  |
| `Tab`     | Session     | Toggle focus between conversation and side pane     |
| `d`       | Session     | Dismiss side pane                                   |
| `:`       | Session     | Enter command mode                                  |

All bindings live in [`config/default.toml`](config/default.toml). A
user-level override at `~/.config/fleet-commander/config.toml` is merged
on top — missing fields fall back to defaults, so partial configs are
safe.

Modifier syntax: `C-` (Ctrl), `S-` (Shift), `M-` (Alt/Meta). A bare
uppercase letter (e.g. `G`) implies Shift.

### Conversation rendering

| Marker                                 | Meaning                                              |
| -------------------------------------- | ---------------------------------------------------- |
| `> your message`                       | Prompts you sent                                     |
| Streaming text (no marker)             | Assistant response while it is being received        |
| Markdown-rendered block                | Assistant response after it completes                |
| `💭 thinking preview…`                 | Agent thought (collapsed to 80 chars)                |
| `⏳ tool title`                        | Tool call in progress                                |
| `✓ tool title` / `✗ tool title`        | Tool call completed / failed                         |
| `🔐 Permission requested: …`           | Tool wants permission — press `y` to allow, `n` to deny |
| `[error] …`                            | Error from the agent or runtime                      |
| Plain text                             | Runtime log line (container start, etc.)             |

## Architecture

Fleet Commander is split into two crates:

```text
crates/
├── fleet-commander-core/   # frontend-agnostic: containers + ACP runtime
│   ├── container          (devcontainer lifecycle via devcontainer-lib + bollard)
│   ├── agent_runtime      (spawn agent, drive prompt loop, emit SessionEvent)
│   ├── session            (handle-based public API — see crate README)
│   └── base_layer         (per-workspace credential layer paths)
│
└── fleet-commander/        # the TUI binary
    ├── app                (state machine, screens, event dispatch)
    ├── ui                 (ratatui renderer, sticky scroll, markdown pipeline)
    ├── agent / workspace  (agent registry + workspaces.yaml persistence)
    ├── init               (onboarding flow)
    ├── mcp_server         (MCP tools: show_diff, show_file, notify)
    ├── config / keybind   (TOML config, keybind DSL)
    └── markdown           (syntect-backed code-fence highlighting)
```

`fleet-commander-core` is what you'd depend on if you wanted to build an
alternative frontend (a GUI, a VS Code extension, a headless harness):
it exposes a handle-based session API where each streaming entity
(assistant message, thought, tool call) is a single typed value whose
contents update through `tokio::sync::watch` channels. See
[`crates/fleet-commander-core/README.md`](crates/fleet-commander-core/README.md)
for details.

```text
┌─────────────────────────────────────────────────────────┐
│  fleet-commander  (TUI)                                 │
│  ─ keybindings / sticky scroll / markdown rendering     │
│  ─ workspaces.yaml + per-workspace state.yaml           │
├─────────────────────────────────────────────────────────┤
│  fleet-commander-core                                   │
│  ─ container::up()      → starts dev container          │
│  ─ agent_runtime::run() → spawns ACP agent in container │
│  ─ session::{ToolCall, AssistantMessage, …} handles     │
├─────────────────────────────────────────────────────────┤
│  devcontainer-lib (Rust) + bollard → Docker engine      │
│  ACP over stdio   → copilot / claude-agent-acp / …      │
│  MCP over HTTP    → 127.0.0.1:6100 (agent → TUI tools)  │
└─────────────────────────────────────────────────────────┘
```

### Protocols

- **[ACP](https://agentclientprotocol.com)** (Agent Client Protocol) —
  JSON-RPC over stdio for talking to coding agents. Fleet Commander is
  the ACP client; agents speak it via e.g. `copilot --acp --stdio`.
- **[MCP](https://modelcontextprotocol.io)** (Model Context Protocol) —
  Streamable HTTP server on `127.0.0.1:6100` exposing
  `show_diff`, `show_file`, and `notify` tools agents can call back into.

### On-disk layout

| Path                                                       | What                                                                  |
| ---------------------------------------------------------- | --------------------------------------------------------------------- |
| `~/.config/fleet-commander/workspaces.yaml`                | List of registered workspaces and the ACP command for each.           |
| `~/.config/fleet-commander/config.toml` (optional)         | User keybinding/config overrides.                                     |
| `~/.local/share/fleet-commander/<slug>/`                   | Per-workspace data: credential base layer, `state.yaml` (session id). |
| `~/.local/share/fleet-commander/fleet-commander.log`       | Runtime log (set `RUST_LOG=debug` for more detail).                   |

## Development

```bash
# All workspace tests
cargo test --workspace

# Format / lint as CI does
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings

# Run the TUI with debug logging
RUST_LOG=debug cargo run -p fleet-commander
```

CI runs `check`, `test`, `clippy`, and `fmt` against the entire workspace
on every push and PR — see [`.github/workflows/ci.yml`](.github/workflows/ci.yml).

## License

[MIT](LICENSE)

[`devcontainer-lib`]: https://github.com/glecaros/dev
