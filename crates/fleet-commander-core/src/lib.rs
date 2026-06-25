//! Fleet Commander core: devcontainer lifecycle and ACP (Agent Client
//! Protocol) runtime.
//!
//! This crate is deliberately frontend-agnostic. It exposes:
//!
//! * [`container`] — start, exec, and inspect dev containers (via
//!   `devcontainer-lib`).
//! * [`agent_runtime`] — spawn an ACP agent subprocess (optionally inside
//!   a dev container), drive the protocol, and emit
//!   [`session::SessionEvent`]s.
//! * [`session`] — typed handle model for streamed entities (assistant
//!   messages, thoughts, replayed user messages, tool calls). Each entity
//!   appears once as a `*Started` event whose handle carries `watch`
//!   channels that update in place.
//! * [`base_layer`] — paths for per-workspace state shared with the writer
//!   side (typically the TUI's `init` command).
//!
//! A TUI, GUI, or VSCode extension all consume the same surface: hand
//! [`agent_runtime::start_agent`] an
//! `mpsc::UnboundedSender<session::SessionEvent>` plus an agent id +
//! ACP command, and react to whatever comes back.

pub mod agent_bin;
pub mod agent_runtime;
pub mod base_layer;
pub mod container;
pub mod git;
pub mod service_fs;
pub mod session;
pub mod workspace_fs;

// Re-exported so downstream crates (the TUI) can name protocol constants
// (e.g. notification method names) without depending on `fleet-protocol`
// directly.
pub use fleet_protocol;

mod session_state;
