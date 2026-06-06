# Fleet Commander

A terminal UI for orchestrating multiple AI coding agents. Point it at a repo,
spin up dev containers, and command a fleet of agents — Copilot, Claude, or any
[ACP](https://agentclientprotocol.com)-compatible agent — from a single
keyboard-driven interface.

Inspired by [Norton Commander](https://en.wikipedia.org/wiki/Norton_Commander).

![Rust](https://img.shields.io/badge/rust-2024_edition-orange)
![License](https://img.shields.io/badge/license-MIT-blue)

## Features

- **Agent-agnostic** — works with any [ACP](https://agentclientprotocol.com)
  agent: GitHub Copilot CLI (`copilot --acp --stdio`), Claude Code
  (`claude-agent-acp`), Gemini CLI, and more
- **Dev container isolation** — each agent runs inside a
  [dev container](https://containers.dev) built from the repo's
  `.devcontainer/` config
- **Persistent sessions** — multi-turn conversations with full context
  retention across prompts
- **Streaming UI** — live response rendering with color-coded messages, tool
  call visibility, and auto-scroll
- **MCP server** — built-in [MCP](https://modelcontextprotocol.io) server so
  agents can push diffs, files, and notifications back to the TUI
- **Vim-style keybindings** — fully configurable via TOML
- **Two-screen layout** — agent list overview + immersive session view with
  optional side pane for diffs/files

## Quick Start

### Prerequisites

- Rust (edition 2024)
- At least one ACP-compatible agent installed:
  - [GitHub Copilot CLI](https://docs.github.com/en/copilot/github-copilot-in-the-cli)
  - [Claude Code ACP adapter](https://github.com/agentclientprotocol/claude-agent-acp)
- (Optional) [Dev Container CLI](https://github.com/devcontainers/cli) for
  container-based isolation

### Install & Run

```bash
git clone https://github.com/culpeo-labs/term.git fleet-commander
cd fleet-commander
cargo run
```

## Usage

Fleet Commander starts on the **Agent List** screen. Each agent shows its name
and connection status.

### Keybindings

| Key       | Context      | Action                     |
|-----------|--------------|----------------------------|
| `j` / `↓` | Agent list   | Move selection down        |
| `k` / `↑` | Agent list   | Move selection up          |
| `Enter`   | Agent list   | Open agent session         |
| `q`       | Agent list   | Quit                       |
| `i`       | Session      | Enter input mode           |
| `Enter`   | Input mode   | Send message               |
| `Esc`     | Input mode   | Cancel input               |
| `Esc`     | Session      | Back to agent list         |
| `Tab`     | Session      | Toggle focus (side pane)   |
| `d`       | Session      | Dismiss side pane          |
| `↑` / `↓` | Session      | Scroll conversation        |

All keybindings are configurable in [`config/default.toml`](config/default.toml).

### Conversation Colors

| Color  | Meaning                        |
|--------|--------------------------------|
| Cyan   | Your messages (`> ...`)        |
| Green  | Streaming agent response       |
| Yellow | Tool calls (`[tool: ...]`)     |
| Red    | Errors (`[error] ...`)         |
| Gray   | Thoughts, permissions          |

## Architecture

```
┌────────────────────────────────────────────────────┐
│  Fleet Commander (ACP Client)                      │
│                                                    │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐         │
│  │ Copilot  │  │ Claude   │  │ Agent N  │          │
│  │ --acp    │  │ --acp    │  │ --acp    │          │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘         │
│       │stdio         │stdio        │stdio          │
│  ┌────┴─────┐  ┌────┴─────┐  ┌────┴─────┐         │
│  │ Container│  │ Container│  │ Local    │          │
│  │(optional)│  │(optional)│  │process   │          │
│  └──────────┘  └──────────┘  └──────────┘          │
│                                                    │
│  MCP Server :6100  ← agents push diffs/files back  │
│  Event Channel     ← unified AppEvent stream       │
└────────────────────────────────────────────────────┘
```

### Key Components

| Module            | Purpose                                              |
|-------------------|------------------------------------------------------|
| `agent.rs`        | Agent model with ACP command and workspace config    |
| `agent_runtime.rs`| ACP lifecycle: spawn → initialize → session → prompt |
| `container.rs`    | Dev container lifecycle (devcontainer up/exec)       |
| `mcp_server.rs`   | MCP server for agent → TUI communication            |
| `app.rs`          | State machine, input handling, screen transitions    |
| `ui.rs`           | Rendering with ratatui (syntax highlighting, etc.)   |
| `event.rs`        | Unified event enum (keyboard, ACP, MCP, filesystem)  |
| `config.rs`       | TOML configuration with keybinding DSL               |

### Protocols

- **[ACP](https://agentclientprotocol.com)** (Agent Client Protocol) — JSON-RPC
  over stdio for communicating with coding agents. Fleet Commander acts as an ACP
  client.
- **[MCP](https://modelcontextprotocol.io)** (Model Context Protocol) —
  Streamable HTTP server on port 6100 with tools: `show_diff`, `show_file`,
  `notify`.

## Configuration

### Agent Definitions

Agents are defined in `src/agent.rs`. To add a new agent or point one at a
dev container:

```rust
Agent::new("my-agent", "My Custom Agent")
    .with_acp_command("my-agent-binary --acp --stdio")
    .with_workspace("/path/to/repo")  // optional: runs in dev container
```

### Keybindings

Edit `config/default.toml`:

```toml
[bindings]
quit         = ["q", "C-c"]
up           = ["k", "Up"]
down         = ["j", "Down"]
activate     = ["Enter"]
back         = ["Esc"]
insert       = ["i"]
```

Modifier syntax: `C-` (Ctrl), `S-` (Shift), `M-` (Alt/Meta).

## Dev Container Support

When an agent has a `workspace_folder`, Fleet Commander:

1. Runs `devcontainer up --workspace-folder <path>` to start the container
2. Wraps the ACP command: `devcontainer exec --workspace-folder <path> <acp_command>`
3. Uses the container's remote workspace as the session working directory

This gives each agent a fully isolated dev environment with all the tools
defined in the repo's `.devcontainer/` configuration.

### Prerequisites

```bash
npm install -g @devcontainers/cli
```

## Development

```bash
# Run tests
cargo test

# Build release
cargo build --release

# Run with logging
RUST_LOG=debug cargo run
```

## License

[MIT](LICENSE)
