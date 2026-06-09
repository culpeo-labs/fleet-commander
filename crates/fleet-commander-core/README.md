# fleet-commander-core

Devcontainer lifecycle + ACP ([Agent Client Protocol](https://agentclientprotocol.com/)) runtime
for Fleet Commander. Frontend-agnostic: a TUI, GUI, VS Code extension, or
headless test harness can all sit on top of the same crate.

## Modules

| Module          | Responsibility                                                              |
| --------------- | --------------------------------------------------------------------------- |
| `container`     | Devcontainer image build / start / exec via [`devcontainer-lib`].           |
| `agent_runtime` | Spawn an ACP agent (`copilot --acp --stdio`, …) inside a container, drive the prompt loop, surface high-level events. |
| `session`       | The public handle-based session API (see below).                            |
| `base_layer`    | Per-workspace credential layer paths (`~/.local/share/fleet-commander/…`).  |

## The session abstraction

ACP delivers many small wire notifications keyed by id:
`tool_call_update`s share a `tool_call_id`, assistant/thought/user
messages stream as chunks bounded by turn markers, and tool calls run in
parallel. Asking every frontend to dedupe-by-id, buffer chunks, and flush
on turn boundaries duplicates a lot of state machinery.

`fleet_commander_core::session` exposes a *typed handle per logical
entity* instead. The runtime emits a single `*Started` event when an
entity first appears and routes follow-up updates through `tokio::sync::watch`
channels owned by the handle. Frontends just store the handle and read
it at render time.

### Events

```rust
pub enum SessionEvent {
    Connected           { agent_id, session_id },
    ToolCall     { agent_id, call: ToolCall },
    AssistantMessage { agent_id, message: AssistantMessage },
    Thought      { agent_id, thought: Thought },
    UserMessage  { agent_id, message: UserMessage },  // replay only
    Output              { agent_id, line },                  // log / stderr
    AuthRequired        { agent_id, command },
    PermissionRequest   { agent_id, tool_name, options, reply },
    Error               { agent_id, message },
    Exited              { agent_id, code },
}
```

Notice what is **not** there: no `AssistantDelta`, no `ToolCallUpdate`,
no `AssistantDone`. Streaming/lifecycle updates flow through the handle.

### Handles

```rust
pub struct AssistantMessage {
    pub text:   watch::Receiver<String>,         // full body so far
    pub status: watch::Receiver<MessageStatus>,  // Streaming | Completed | Failed(reason)
}

pub struct Thought       { pub text: watch::Receiver<String>, pub status: watch::Receiver<MessageStatus> }
pub struct UserMessage   { pub text: watch::Receiver<String>, pub status: watch::Receiver<MessageStatus> }

pub struct ToolCall {
    pub id:     String,
    pub title:  watch::Receiver<String>,
    pub status: watch::Receiver<ToolCallStatusKind>, // Pending | InProgress | Completed | Failed
}
```

All handles are `Clone` (cloning a `watch::Receiver` is cheap). A
handle's status reaching a terminal value (`MessageStatus::is_terminal()` /
`ToolCallStatusKind::is_terminal()`) means no further updates will arrive.

### Consumer pattern

```rust
use fleet_commander_core::session::{SessionEvent, MessageStatus};

while let Some(event) = rx.recv().await {
    match event {
        SessionEvent::AssistantMessage { agent_id, message } => {
            history.push(HistoryEntry::Assistant(message.clone()));
            // Spawn a tracker so the UI repaints as chunks arrive.
            let mut text   = message.text;
            let mut status = message.status;
            let repaint    = ui_tx.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        r = text.changed()   => if r.is_err() { break; },
                        r = status.changed() => if r.is_err() { break; },
                    }
                    let _ = repaint.send(UiEvent::Repaint(agent_id.clone()));
                    if status.borrow().is_terminal() { break; }
                }
            });
        }
        SessionEvent::ToolCall { agent_id, call } => { /* same pattern */ }
        // …
    }
}
```

At render time the consumer is purely synchronous:

```rust
let body  = message.text.borrow();
let done  = message.status.borrow().is_terminal();
```

### Lossy on purpose

`watch` channels keep only the latest value. That is the right contract
here: a UI only ever renders the most recent state of a streaming message
or tool call. Intermediate chunks that the UI didn't observe are
irrelevant — `text.borrow()` already contains the accumulated body.

### Parallelism

Tool calls run in parallel and are keyed by `tool_call_id`; each gets
its own handle. Streamed text entities (assistant, thought, user) use
single-slot active state. The arrival of a new chunk closes the entities
whose stream it supersedes:

- `assistant_chunk` closes any active thought and user message.
- `thought_chunk` closes any active user message (but not the assistant —
  thoughts can interleave with an assistant turn).
- `user_chunk` closes any active thought and assistant.
- `prompt_complete` closes everything still streaming.

All of this lives in the private `session_state` module — consumers only
see `SessionEvent`s and handles.

### Live prompts vs replayed user messages

`agent_runtime::send_message(...)` forwards the user's live prompt to the
agent **without** echoing it back as a `SessionEvent`. Local echo is
purely a frontend concern. The agent does, however, replay prior user
messages during `session/load` or `session/resume` — those arrive as
`SessionEvent::UserMessage` so the same render path can show
them.

[`devcontainer-lib`]: https://github.com/glecaros/dev
