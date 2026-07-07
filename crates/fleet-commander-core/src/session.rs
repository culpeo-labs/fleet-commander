//! High-level session API.
//!
//! The ACP wire protocol delivers many small notifications keyed by id
//! (`tool_call_update`s share a `tool_call_id`; assistant/thought/user
//! messages stream as chunks bounded by turn markers). Asking every
//! frontend to dedupe-by-id and buffer-then-flush is wasted complexity.
//!
//! This module exposes a *typed handle* per logical entity instead. The
//! runtime emits a single `*Started` event when an entity first appears
//! and routes follow-up updates through `watch` channels owned by the
//! handle. Frontends just:
//!
//! 1. Store the handle (e.g. in a render list).
//! 2. Read `handle.text.borrow()` / `handle.status.borrow()` at render time
//!    (no async).
//! 3. (Optionally) `await handle.text.changed()` / `status.changed()` to
//!    know when to redraw.
//!
//! Completion is signalled by the `status` channel transitioning out of
//! `Streaming` (for messages/thoughts) or out of `Pending`/`InProgress`
//! (for tool calls). When the runtime drops a handle's senders the
//! receivers' `changed()` futures resolve with `Err`, giving consumers a
//! way to retire tracker tasks.

use std::sync::{Arc, Mutex};

use tokio::sync::{oneshot, watch};

pub type AgentId = String;

/// A oneshot reply channel for permission responses, wrapped so events
/// stay `Clone` (the sender is taken once when the consumer responds).
pub type PermissionReply = Arc<Mutex<Option<oneshot::Sender<Option<String>>>>>;

/// Status of a streamed text entity (assistant message, thought, replayed
/// user message). `Streaming` while chunks are still arriving; transitions
/// once to a terminal value when the runtime closes the handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageStatus {
    Streaming,
    Completed,
    Failed(String),
}

impl MessageStatus {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, MessageStatus::Streaming)
    }
}

/// Execution status of a tool call. Mirrors ACP's `ToolCallStatus` and
/// lives here so the public API stays free of ACP types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallStatusKind {
    Pending,
    InProgress,
    Completed,
    Failed,
}

impl ToolCallStatusKind {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            ToolCallStatusKind::Completed | ToolCallStatusKind::Failed
        )
    }
}

/// A single in-flight tool call. Title and status update in place via
/// `watch::Receiver`s; the call is finished once `status.borrow().is_terminal()`.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub title: watch::Receiver<String>,
    pub status: watch::Receiver<ToolCallStatusKind>,
}

/// A streamed assistant message. `text` is the full accumulated body so far.
#[derive(Debug, Clone)]
pub struct AssistantMessage {
    pub text: watch::Receiver<String>,
    pub status: watch::Receiver<MessageStatus>,
}

/// A streamed "agent thought" (Copilot's internal reasoning chunks).
#[derive(Debug, Clone)]
pub struct Thought {
    pub text: watch::Receiver<String>,
    pub status: watch::Receiver<MessageStatus>,
}

/// A user message replayed by the agent (typically during `session/load`
/// or `session/resume`, when the agent re-emits prior conversation history).
#[derive(Debug, Clone)]
pub struct UserMessage {
    pub text: watch::Receiver<String>,
    pub status: watch::Receiver<MessageStatus>,
}

/// A command advertised by the agent via ACP's `available_commands_update`
/// notification. Kept independent of ACP schema types so consumers don't
/// have to depend on `agent-client-protocol`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableCommand {
    /// Command name without the leading slash (e.g. `model`, `session`).
    pub name: String,
    /// Human-readable summary shown alongside the name in pickers.
    pub description: String,
    /// Optional placeholder shown when the command takes an argument
    /// (e.g. "directory", "[on|off]"). `None` for argument-less commands.
    pub hint: Option<String>,
}

/// Events emitted by the runtime. Streamed entities are delivered once via
/// the `*Started` variants; their content updates through the handle's
/// `watch` channels.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// The ACP session is open and ready to accept prompts.
    Connected {
        agent_id: AgentId,
        session_id: Option<String>,
    },
    /// A new tool call has been registered. Status updates flow through
    /// `call.status`; rename events through `call.title`.
    ToolCall { agent_id: AgentId, call: ToolCall },
    /// A new assistant message has begun streaming.
    AssistantMessage {
        agent_id: AgentId,
        message: AssistantMessage,
    },
    /// A new "agent thought" has begun streaming.
    Thought { agent_id: AgentId, thought: Thought },
    /// A replayed user message has begun streaming (only during session
    /// load/resume — live user input is sent by the consumer via
    /// [`crate::agent_runtime::send_message`] and not echoed back as an
    /// event).
    UserMessage {
        agent_id: AgentId,
        message: UserMessage,
    },
    /// Runtime status/log line: container progress, stderr, etc. Not part
    /// of the model's conversation.
    Output { agent_id: AgentId, line: String },
    /// The agent's dev container is up and reachable. Carries the details
    /// needed to reach the in-container `fleet-agent` service so the
    /// consumer can point the explorer at the container's filesystem.
    ContainerReady {
        agent_id: AgentId,
        container_id: String,
        remote_user: String,
        remote_workspace_folder: String,
    },
    /// The agent needs interactive authentication (e.g. `copilot login`).
    /// The consumer should suspend its UI and run the command interactively.
    AuthRequired {
        agent_id: AgentId,
        command: Vec<String>,
    },
    /// The agent is requesting tool-use permission from the user.
    /// Send `Some(option_id)` to approve or `None` to cancel via the reply channel.
    PermissionRequest {
        agent_id: AgentId,
        tool_name: String,
        /// Human-readable option labels: `(option_id, display_name, kind)`.
        options: Vec<(String, String, String)>,
        reply: PermissionReply,
    },
    /// A session-level error from the runtime.
    Error { agent_id: AgentId, message: String },
    /// The agent process exited.
    Exited {
        agent_id: AgentId,
        code: Option<i32>,
    },
    /// The agent has advertised the set of slash commands it supports
    /// (via ACP `available_commands_update`). Sent once per update; the
    /// agent may replace the list at any time.
    AvailableCommands {
        agent_id: AgentId,
        commands: Vec<AvailableCommand>,
    },
    /// A container-backed [`crate::workspace_fs::WorkspaceFs`] is ready for the
    /// agent's explorer. Delivered by the daemon-owned session driver, which
    /// builds it over the **same** `fleet-agent` bridge that carries the
    /// `session.*` protocol (Phase 4b2 y3 unification) — so the explorer no
    /// longer opens its own `docker exec`. The consumer installs it if the
    /// agent is still viewed on the same container.
    ExplorerFs {
        agent_id: AgentId,
        container_id: String,
        fs: Arc<dyn crate::workspace_fs::WorkspaceFs>,
    },
    /// A coalesced `fs.didChange` push from the shared bridge's live
    /// `fs.watch` subscription: the in-container filesystem changed.
    ExplorerFsChanged {
        agent_id: AgentId,
        container_id: String,
    },
    /// A batch of streamed content-search matches (`fs.searchResult`).
    SearchResults {
        agent_id: AgentId,
        search_id: u64,
        matches: Vec<crate::fleet_protocol::SearchMatch>,
    },
    /// A content search finished (`fs.searchDone`) with its terminal summary.
    SearchDone {
        agent_id: AgentId,
        search_id: u64,
        summary: crate::fleet_protocol::SearchSummary,
    },
    /// The agent's in-container git branch, read over the shared bridge.
    /// `branch` is `None` when the workspace isn't a git tree (or the read
    /// failed).
    AgentBranch {
        agent_id: AgentId,
        container_id: String,
        branch: Option<String>,
    },
}

impl SessionEvent {
    pub fn agent_id(&self) -> &AgentId {
        match self {
            SessionEvent::Connected { agent_id, .. }
            | SessionEvent::ToolCall { agent_id, .. }
            | SessionEvent::AssistantMessage { agent_id, .. }
            | SessionEvent::Thought { agent_id, .. }
            | SessionEvent::UserMessage { agent_id, .. }
            | SessionEvent::Output { agent_id, .. }
            | SessionEvent::ContainerReady { agent_id, .. }
            | SessionEvent::AuthRequired { agent_id, .. }
            | SessionEvent::PermissionRequest { agent_id, .. }
            | SessionEvent::Error { agent_id, .. }
            | SessionEvent::Exited { agent_id, .. }
            | SessionEvent::AvailableCommands { agent_id, .. }
            | SessionEvent::ExplorerFs { agent_id, .. }
            | SessionEvent::ExplorerFsChanged { agent_id, .. }
            | SessionEvent::SearchResults { agent_id, .. }
            | SessionEvent::SearchDone { agent_id, .. }
            | SessionEvent::AgentBranch { agent_id, .. } => agent_id,
        }
    }
}
