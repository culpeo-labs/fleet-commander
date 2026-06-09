//! Fleet Commander core: devcontainer lifecycle and ACP (Agent Client
//! Protocol) runtime.
//!
//! This crate is deliberately frontend-agnostic. It exposes:
//!
//! * [`container`] — start, exec, and inspect dev containers (via
//!   `devcontainer-lib`).
//! * [`agent_runtime`] — spawn an ACP agent subprocess (optionally inside
//!   a dev container), drive the protocol, and forward events.
//! * [`event::RuntimeEvent`] — the event stream consumers subscribe to.
//! * [`base_layer`] — paths for per-workspace state shared with the writer
//!   side (typically the TUI's `init` command).
//!
//! A TUI, GUI, or VSCode extension all consume the same surface: hand
//! [`agent_runtime::start_agent`] an [`event::AgentSpec`] plus an
//! `mpsc::UnboundedSender<RuntimeEvent>` and react to whatever comes back.

pub mod agent_runtime;
pub mod base_layer;
pub mod container;
pub mod event;
