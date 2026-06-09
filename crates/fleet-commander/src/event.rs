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
#[allow(dead_code)]
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
    /// Nudge from a per-handle tracker task to redraw the given agent
    /// because one of its handles' `watch` channels ticked. Carries no
    /// state — the renderer reads the handle directly.
    Repaint {
        agent_id: AgentId,
    },
}
