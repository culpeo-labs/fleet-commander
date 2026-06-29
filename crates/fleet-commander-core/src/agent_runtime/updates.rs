//! Applying ACP `SessionUpdate`s to the per-session state machine.

use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate, ToolCallStatus};

use crate::session::ToolCallStatusKind;
use crate::session_state::SessionStateMachine;

/// Apply an ACP `SessionUpdate` to the per-session state machine. The state
/// machine handles emitting the `*Started` events on the first chunk of a
/// new entity and routing follow-up updates through the live handles.
pub(super) fn apply_session_update(state: &mut SessionStateMachine, update: &SessionUpdate) {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            if let ContentBlock::Text(text) = &chunk.content {
                state.assistant_chunk(&text.text);
            }
        }
        SessionUpdate::UserMessageChunk(chunk) => {
            if let ContentBlock::Text(text) = &chunk.content {
                state.user_chunk(&text.text);
            }
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            if let ContentBlock::Text(text) = &chunk.content {
                state.thought_chunk(&text.text);
            }
        }
        SessionUpdate::ToolCall(tool_call) => {
            state.tool_call(
                &tool_call.tool_call_id.0,
                tool_call.title.clone(),
                map_tool_status(&tool_call.status),
            );
        }
        SessionUpdate::ToolCallUpdate(update) => {
            state.tool_call_update(
                &update.tool_call_id.0,
                update.fields.title.clone(),
                update.fields.status.as_ref().map(map_tool_status),
            );
        }
        _ => {}
    }
}

/// Map the ACP tool-call status enum to our display-side enum so events stay
/// free of ACP types.
fn map_tool_status(status: &ToolCallStatus) -> ToolCallStatusKind {
    match status {
        ToolCallStatus::Pending => ToolCallStatusKind::Pending,
        ToolCallStatus::InProgress => ToolCallStatusKind::InProgress,
        ToolCallStatus::Completed => ToolCallStatusKind::Completed,
        ToolCallStatus::Failed => ToolCallStatusKind::Failed,
        _ => ToolCallStatusKind::InProgress,
    }
}
