//! Per-session state machine.
//!
//! Translates the flat ACP wire stream (notifications keyed by id, chunked
//! text bounded by turn markers) into the typed handle model defined in
//! [`crate::session`].
//!
//! Rules:
//!
//! * **Tool calls** are parallel. Each `tool_call_id` gets its own
//!   `(title, status)` pair, retired when status becomes terminal.
//! * **Streamed text entities** (assistant/thought/user) use single-slot
//!   active state. Chunk arrival closes the entities whose "stream" the
//!   new chunk supersedes:
//!     - `assistant_chunk` closes any active thought and user.
//!     - `thought_chunk` closes any active user (but not assistant — thoughts
//!       can interleave with an assistant turn).
//!     - `user_chunk` closes any active thought and assistant.
//! * **`prompt_complete`** closes everything still streaming.

use std::collections::HashMap;

use tokio::sync::{mpsc, watch};

use crate::session::{
    AgentId, AssistantMessage, MessageStatus, SessionEvent, Thought, ToolCall, ToolCallStatusKind,
    UserMessage,
};

struct AssistantSenders {
    text: watch::Sender<String>,
    status: watch::Sender<MessageStatus>,
}

struct ThoughtSenders {
    text: watch::Sender<String>,
    status: watch::Sender<MessageStatus>,
}

struct UserSenders {
    text: watch::Sender<String>,
    status: watch::Sender<MessageStatus>,
}

struct ToolCallSenders {
    title: watch::Sender<String>,
    status: watch::Sender<ToolCallStatusKind>,
}

pub(crate) struct SessionStateMachine {
    agent_id: AgentId,
    event_tx: mpsc::UnboundedSender<SessionEvent>,
    active_assistant: Option<AssistantSenders>,
    active_thought: Option<ThoughtSenders>,
    active_user: Option<UserSenders>,
    tool_calls: HashMap<String, ToolCallSenders>,
}

impl SessionStateMachine {
    pub fn new(agent_id: AgentId, event_tx: mpsc::UnboundedSender<SessionEvent>) -> Self {
        Self {
            agent_id,
            event_tx,
            active_assistant: None,
            active_thought: None,
            active_user: None,
            tool_calls: HashMap::new(),
        }
    }

    // ── Streaming text ────────────────────────────────────────────────────

    pub fn assistant_chunk(&mut self, text: &str) {
        self.close_thought(MessageStatus::Completed);
        self.close_user(MessageStatus::Completed);
        if self.active_assistant.is_none() {
            let (text_tx, text_rx) = watch::channel(String::new());
            let (status_tx, status_rx) = watch::channel(MessageStatus::Streaming);
            let _ = self.event_tx.send(SessionEvent::AssistantMessage {
                agent_id: self.agent_id.clone(),
                message: AssistantMessage {
                    text: text_rx,
                    status: status_rx,
                },
            });
            self.active_assistant = Some(AssistantSenders {
                text: text_tx,
                status: status_tx,
            });
        }
        if let Some(senders) = &self.active_assistant {
            senders.text.send_modify(|s| s.push_str(text));
        }
    }

    pub fn thought_chunk(&mut self, text: &str) {
        self.close_user(MessageStatus::Completed);
        if self.active_thought.is_none() {
            let (text_tx, text_rx) = watch::channel(String::new());
            let (status_tx, status_rx) = watch::channel(MessageStatus::Streaming);
            let _ = self.event_tx.send(SessionEvent::Thought {
                agent_id: self.agent_id.clone(),
                thought: Thought {
                    text: text_rx,
                    status: status_rx,
                },
            });
            self.active_thought = Some(ThoughtSenders {
                text: text_tx,
                status: status_tx,
            });
        }
        if let Some(senders) = &self.active_thought {
            senders.text.send_modify(|s| s.push_str(text));
        }
    }

    pub fn user_chunk(&mut self, text: &str) {
        self.close_thought(MessageStatus::Completed);
        self.close_assistant(MessageStatus::Completed);
        if self.active_user.is_none() {
            let (text_tx, text_rx) = watch::channel(String::new());
            let (status_tx, status_rx) = watch::channel(MessageStatus::Streaming);
            let _ = self.event_tx.send(SessionEvent::UserMessage {
                agent_id: self.agent_id.clone(),
                message: UserMessage {
                    text: text_rx,
                    status: status_rx,
                },
            });
            self.active_user = Some(UserSenders {
                text: text_tx,
                status: status_tx,
            });
        }
        if let Some(senders) = &self.active_user {
            senders.text.send_modify(|s| s.push_str(text));
        }
    }

    /// Marks every active streaming entity as completed. Called when the
    /// `prompt` RPC returns (turn boundary) and again at session end.
    pub fn prompt_complete(&mut self) {
        self.close_thought(MessageStatus::Completed);
        self.close_assistant(MessageStatus::Completed);
        self.close_user(MessageStatus::Completed);
    }

    /// Marks every active entity as failed with the given message.
    pub fn fail_active(&mut self, message: impl Into<String>) {
        let msg = message.into();
        self.close_thought(MessageStatus::Failed(msg.clone()));
        self.close_assistant(MessageStatus::Failed(msg.clone()));
        self.close_user(MessageStatus::Failed(msg));
    }

    fn close_assistant(&mut self, status: MessageStatus) {
        if let Some(senders) = self.active_assistant.take() {
            let _ = senders.status.send(status);
            // Drop senders -> watch::Receivers' changed() will resolve Err
            // after one more borrow, signalling end of stream to consumers.
        }
    }

    fn close_thought(&mut self, status: MessageStatus) {
        if let Some(senders) = self.active_thought.take() {
            let _ = senders.status.send(status);
        }
    }

    fn close_user(&mut self, status: MessageStatus) {
        if let Some(senders) = self.active_user.take() {
            let _ = senders.status.send(status);
        }
    }

    // ── Tool calls ────────────────────────────────────────────────────────

    /// Initial registration of a tool call. Emits `ToolCall` if this
    /// is the first time we've seen this id; subsequent calls (which can
    /// happen if the agent re-sends the initial frame) are coalesced into
    /// the existing handle via `tool_call_update`.
    pub fn tool_call(&mut self, id: &str, title: String, status: ToolCallStatusKind) {
        if self.tool_calls.contains_key(id) {
            self.tool_call_update(id, Some(title), Some(status));
            return;
        }
        let (title_tx, title_rx) = watch::channel(title);
        let (status_tx, status_rx) = watch::channel(status);
        let _ = self.event_tx.send(SessionEvent::ToolCall {
            agent_id: self.agent_id.clone(),
            call: ToolCall {
                id: id.to_string(),
                title: title_rx,
                status: status_rx,
            },
        });
        let senders = ToolCallSenders {
            title: title_tx,
            status: status_tx,
        };
        if status.is_terminal() {
            // Terminal on first sight — emit the started event with the
            // handle still alive so consumers can read final state, then
            // retire it on the next update tick by leaving the senders
            // dropped.
            self.tool_calls.insert(id.to_string(), senders);
            self.retire_tool_call(id);
        } else {
            self.tool_calls.insert(id.to_string(), senders);
        }
    }

    /// Apply a follow-up update to an existing tool call. Silently ignored
    /// if the id was never registered (defensive against out-of-order
    /// notifications).
    pub fn tool_call_update(
        &mut self,
        id: &str,
        title: Option<String>,
        status: Option<ToolCallStatusKind>,
    ) {
        let Some(senders) = self.tool_calls.get(id) else {
            return;
        };
        if let Some(t) = title {
            let _ = senders.title.send(t);
        }
        if let Some(s) = status {
            let _ = senders.status.send(s);
            if s.is_terminal() {
                self.retire_tool_call(id);
            }
        }
    }

    fn retire_tool_call(&mut self, id: &str) {
        // Drop the senders so the consumer's `changed()` futures complete.
        // The status watch already holds the terminal value.
        self.tool_calls.remove(id);
    }
}

impl Drop for SessionStateMachine {
    fn drop(&mut self) {
        // Best-effort: mark anything still live as failed if the runtime is
        // shutting down without a clean prompt_complete.
        self.close_assistant(MessageStatus::Failed("session ended".into()));
        self.close_thought(MessageStatus::Failed("session ended".into()));
        self.close_user(MessageStatus::Failed("session ended".into()));
        // tool_calls senders drop naturally; consumers see watch::Receivers' Err.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (SessionStateMachine, mpsc::UnboundedReceiver<SessionEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (SessionStateMachine::new("a1".into(), tx), rx)
    }

    #[tokio::test]
    async fn assistant_chunks_accumulate_into_single_message() {
        let (mut sm, mut rx) = fixture();
        sm.assistant_chunk("Hello, ");
        sm.assistant_chunk("world!");
        let event = rx.recv().await.unwrap();
        let SessionEvent::AssistantMessage { message, .. } = event else {
            panic!("expected AssistantMessage, got {event:?}");
        };
        assert_eq!(*message.text.borrow(), "Hello, world!");
        assert_eq!(*message.status.borrow(), MessageStatus::Streaming);
        // Closing should flip status to Completed.
        sm.prompt_complete();
        assert_eq!(*message.status.borrow(), MessageStatus::Completed);
        // No second AssistantMessage was emitted.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn user_chunk_closes_active_assistant() {
        let (mut sm, mut rx) = fixture();
        sm.assistant_chunk("answer");
        let assistant_event = rx.recv().await.unwrap();
        let SessionEvent::AssistantMessage {
            message: assistant, ..
        } = assistant_event
        else {
            panic!()
        };
        sm.user_chunk("next prompt");
        let user_event = rx.recv().await.unwrap();
        assert!(matches!(user_event, SessionEvent::UserMessage { .. }));
        assert_eq!(*assistant.status.borrow(), MessageStatus::Completed);
    }

    #[tokio::test]
    async fn thought_does_not_close_assistant() {
        let (mut sm, mut rx) = fixture();
        sm.assistant_chunk("the answer is ");
        let assistant_event = rx.recv().await.unwrap();
        let SessionEvent::AssistantMessage {
            message: assistant, ..
        } = assistant_event
        else {
            panic!()
        };
        sm.thought_chunk("(thinking)");
        let thought_event = rx.recv().await.unwrap();
        assert!(matches!(thought_event, SessionEvent::Thought { .. }));
        assert_eq!(*assistant.status.borrow(), MessageStatus::Streaming);
        sm.assistant_chunk("42");
        assert_eq!(*assistant.text.borrow(), "the answer is 42");
    }

    #[tokio::test]
    async fn tool_call_updates_route_to_existing_handle() {
        let (mut sm, mut rx) = fixture();
        sm.tool_call("call-1", "Reading file".into(), ToolCallStatusKind::Pending);
        let event = rx.recv().await.unwrap();
        let SessionEvent::ToolCall { call, .. } = event else {
            panic!()
        };
        assert_eq!(*call.status.borrow(), ToolCallStatusKind::Pending);
        sm.tool_call_update("call-1", None, Some(ToolCallStatusKind::InProgress));
        assert_eq!(*call.status.borrow(), ToolCallStatusKind::InProgress);
        sm.tool_call_update("call-1", None, Some(ToolCallStatusKind::Completed));
        assert_eq!(*call.status.borrow(), ToolCallStatusKind::Completed);
        // Only one Started event was emitted.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn tool_call_unknown_id_is_ignored() {
        let (mut sm, mut rx) = fixture();
        sm.tool_call_update("never-seen", None, Some(ToolCallStatusKind::Completed));
        assert!(rx.try_recv().is_err());
    }
}
