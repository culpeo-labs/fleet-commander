//! Cross-workspace message inbox (Feature 2c).
//!
//! When an agent calls the `send_to_workspace` MCP tool, the message does not
//! reach the target agent directly. Instead it is queued here and surfaced to
//! the user as a per-message approval modal (mirroring the tool-permission
//! popup). Only when the user approves is the message injected into the target
//! agent's session as a prompt — so cross-workspace traffic is always
//! human-gated and cannot be used to silently drive another workspace.

use std::collections::VecDeque;

use fleet_commander_core::agent_runtime;

use crate::agent::AgentId;

use super::App;

/// A pending cross-workspace message awaiting the user's approval before it is
/// injected into the target agent's session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxMessage {
    /// Agent that sent the message (the tunnel caller).
    pub sender_id: AgentId,
    /// Friendly name of the sender's workspace (id with `copilot-` stripped).
    pub sender_name: String,
    /// Agent the message is addressed to (paired with the sender).
    pub target_id: AgentId,
    /// Friendly name of the target's workspace.
    pub target_name: String,
    /// The message body the sender wants delivered.
    pub body: String,
    /// Correlation id (Feature 2d) threading a request/reply exchange between
    /// the two agents. Shown to the target so it can echo it when replying.
    pub thread: String,
}

/// FIFO queue of pending cross-workspace messages. The front entry is the one
/// currently surfaced in the approval modal.
#[derive(Debug, Default)]
pub struct Inbox {
    pending: VecDeque<InboxMessage>,
}

impl Inbox {
    /// Queue a message at the back of the inbox.
    pub fn push(&mut self, msg: InboxMessage) {
        self.pending.push_back(msg);
    }

    /// The message currently awaiting approval (front of the queue).
    pub fn front(&self) -> Option<&InboxMessage> {
        self.pending.front()
    }

    /// Remove and return the front message.
    pub fn pop(&mut self) -> Option<InboxMessage> {
        self.pending.pop_front()
    }

    /// Whether any messages are awaiting approval.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Number of messages awaiting approval.
    pub fn len(&self) -> usize {
        self.pending.len()
    }
}

impl App {
    /// Handle a `send_to_workspace` tool call (Feature 2c): queue the message
    /// for the user's approval. Authorization (the sender and target must be a
    /// connected pair) is enforced in the MCP tool before this event is emitted;
    /// here we only require that the target agent actually exists.
    pub(super) fn handle_send_to_workspace(
        &mut self,
        sender_id: AgentId,
        sender_name: String,
        target_id: AgentId,
        message: String,
        thread: String,
    ) {
        let Some(target) = self.agents.iter().find(|a| a.id == target_id) else {
            tracing::warn!(
                %sender_id,
                %target_id,
                "Dropping cross-workspace message: target agent not found"
            );
            return;
        };
        let target_name = target.name.clone();
        self.inbox.push(InboxMessage {
            sender_id,
            sender_name: sender_name.clone(),
            target_id,
            target_name: target_name.clone(),
            body: message,
            thread,
        });
        self.status_message = Some(format!(
            "📨 Message from {sender_name} to {target_name} awaiting approval ({} pending)",
            self.inbox.len()
        ));
    }

    /// Resolve the front inbox message. On `approve`, inject it into the target
    /// agent's session as a prompt (framed so the target knows it is a
    /// cross-workspace message, from whom, and how to reply); otherwise discard.
    pub(super) fn resolve_inbox(&mut self, approve: bool) {
        let Some(msg) = self.inbox.pop() else {
            return;
        };
        if !approve {
            self.status_message = Some(format!(
                "Rejected message from {} to {}",
                msg.sender_name, msg.target_name
            ));
            return;
        }
        // Frame the delivered message with the sender's id and thread id, plus
        // an instruction on how to reply (Feature 2d) — a reply flows back
        // through the same `send_to_workspace` → inbox → approval path.
        let framed = format!(
            "[cross-workspace message from workspace '{sender}' (id: {sender_id}, thread: {thread})]\n\n\
             {body}\n\n\
             ---\n\
             To reply, call send_to_workspace with target=\"{sender_id}\" and thread=\"{thread}\".",
            sender = msg.sender_name,
            sender_id = msg.sender_id,
            thread = msg.thread,
            body = msg.body,
        );
        if let Some(agent) = self.agents.iter_mut().find(|a| a.id == msg.target_id) {
            agent.prompt(framed.clone());
            agent_runtime::send_message(
                agent.id.clone(),
                agent.prompt_tx.as_ref(),
                framed,
                self.runtime_tx.clone(),
            );
            self.status_message = Some(format!(
                "Delivered message from {} to {}",
                msg.sender_name, msg.target_name
            ));
        } else {
            // The target vanished between queueing and approval.
            self.status_message = Some(format!(
                "Could not deliver to {} — agent no longer available",
                msg.target_name
            ));
        }
    }
}
