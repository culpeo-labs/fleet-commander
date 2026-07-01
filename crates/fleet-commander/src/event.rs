//! The single event the application loop reacts to. Everything that wants
//! to nudge the UI (input, agent output, file changes) flows through this
//! enum into the main `select!` loop.
//!
//! Runtime-emitted events ([`fleet_commander_core::session::SessionEvent`])
//! are forwarded as [`AppEvent::Session`]; the app appends the entity
//! handle to history once and then spawns a tracker task that nudges
//! [`AppEvent::Repaint`] whenever the handle's `watch` channels tick.
//! UI-only variants (`Input`, `Change`, MCP server events, `ReconnectAgent`)
//! live only here.

use crossterm::event::KeyEvent;

use fleet_commander_core::session::SessionEvent;

use crate::agent::AgentId;
use crate::change_source::ChangeEvent;

// Re-export the runtime contract types so the rest of the app keeps
// importing them via `crate::event::`.
pub use fleet_commander_core::session::PermissionReply;

#[derive(Debug, Clone)]
pub enum AppEvent {
    Input(KeyEvent),
    Change(ChangeEvent),
    /// An MCP client called the `show_diff` tool.
    McpShowDiff {
        agent_id: AgentId,
        path: std::path::PathBuf,
        content: String,
    },
    /// An MCP client called the `show_file` tool.
    McpShowFile {
        agent_id: AgentId,
        path: std::path::PathBuf,
        content: String,
    },
    /// An MCP client called the `notify` tool.
    McpNotify {
        agent_id: AgentId,
        message: String,
    },
    /// Request to reconnect an agent (e.g. after container rebuild).
    ReconnectAgent {
        agent_id: AgentId,
    },
    /// A high-level event from the runtime. Either a one-off (Connected,
    /// AuthRequired, Output, …) or the `*Started` introduction of a
    /// streamed entity whose handle then drives its own updates.
    Session(SessionEvent),
    /// Result of an explorer status refresh that ran on a background
    /// thread. Carries the new `path -> status` map (empty on error)
    /// and the `include_ignored` flag the refresh was issued with —
    /// the app drops responses that no longer match the current
    /// toggle state to avoid stale overlays.
    ExplorerStatusReady {
        root: std::path::PathBuf,
        include_ignored: bool,
        result: std::result::Result<
            std::collections::HashMap<std::path::PathBuf, fleet_commander_core::git::StatusKind>,
            String,
        >,
    },
    /// A container-backed [`fleet_commander_core::workspace_fs::WorkspaceFs`]
    /// finished connecting on a background thread (the `connect_docker`
    /// handshake is blocking). The app installs it on the explorer if the
    /// agent is still the one being viewed; otherwise it's dropped (which
    /// tears down the underlying `docker exec` process).
    ExplorerFsReady {
        agent_id: AgentId,
        /// The container the `fs` is bound to. Used to reject stale installs:
        /// if the agent's current container has changed (e.g. a `:rebuild`
        /// happened while this handshake was in flight) the fs is dropped.
        container_id: String,
        fs: std::sync::Arc<dyn fleet_commander_core::workspace_fs::WorkspaceFs>,
    },
    /// A background remote directory listing completed. The app installs
    /// it into the explorer's cache if it still matches the active
    /// workspace root. `result` is `Err` on a transport failure (the
    /// in-flight marker is then cleared so it can be retried).
    ExplorerDirReady {
        root: std::path::PathBuf,
        rel: std::path::PathBuf,
        result: std::result::Result<Vec<fleet_commander_core::workspace_fs::DirEntry>, String>,
    },
    /// A background file read (for the explorer's side-pane preview)
    /// completed. Opened in the side pane if the agent is still viewed
    /// and the workspace root still matches.
    ExplorerFileReady {
        agent_id: AgentId,
        root: std::path::PathBuf,
        full_path: std::path::PathBuf,
        result: std::result::Result<String, String>,
    },
    /// A background `git diff` for an explorer-selected file completed.
    /// Shown in the [`crate::app::SidePane::Diff`] pane if the agent is
    /// still viewed and the workspace root still matches.
    ExplorerDiffReady {
        agent_id: AgentId,
        root: std::path::PathBuf,
        full_path: std::path::PathBuf,
        result: std::result::Result<String, String>,
    },
    /// A background fetch of an agent's in-container git branch finished.
    /// `branch` is `None` when the workspace isn't a git tree (or the read
    /// failed). The app stores it on the agent so the header/list reflect the
    /// container's branch — the same filesystem the explorer's git status
    /// comes from.
    AgentBranchReady {
        agent_id: AgentId,
        container_id: String,
        branch: Option<String>,
    },
    /// The in-container filesystem changed (a coalesced `fs.didChange` push
    /// from the live `fs.watch` subscription on the explorer's `ServiceFs`).
    /// The app re-lists the explorer tree and refreshes git status — but only
    /// while this agent is still viewed and still backed by the same
    /// container the watch was established on (mirrors `ExplorerFsReady`).
    ExplorerFsChanged {
        agent_id: AgentId,
        container_id: String,
    },
    /// Nudge from a per-handle tracker task to redraw because one of its
    /// handles' `watch` channels ticked. Carries no state — the renderer
    /// reads the handle directly. (The redraw itself is performed by the
    /// main loop after `App::handle` returns.)
    Repaint,
}
