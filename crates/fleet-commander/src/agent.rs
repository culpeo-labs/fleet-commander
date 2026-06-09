//! Agent state held by the UI.
//!
//! Each agent wraps an ACP connection. The `acp_command` field specifies
//! what command to launch (e.g. "copilot --acp --stdio"). When `workspace_folder`
//! is set, the agent runs inside a dev container for that repo.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::AbortHandle;

pub use fleet_commander_core::session::{
    AgentId, AssistantMessage, Thought, ToolCall, UserMessage,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    Idle,
    Running,
    Stopped,
    Error,
}

impl AgentStatus {
    pub fn label(&self) -> &'static str {
        match self {
            AgentStatus::Idle => "idle",
            AgentStatus::Running => "running",
            AgentStatus::Stopped => "stopped",
            AgentStatus::Error => "error",
        }
    }
}

/// A single visible item in an agent's conversation pane.
///
/// `Info` / `Error` / `Prompt` are static text the TUI itself authored;
/// the other variants carry live handles whose state updates in place
/// through `watch` channels.
#[derive(Debug, Clone)]
pub enum HistoryEntry {
    /// Informational line authored by the TUI (e.g. "ACP session connected").
    Info(String),
    /// Error line authored by the TUI.
    Error(String),
    /// User prompt the operator typed and dispatched.
    Prompt(String),
    /// A streamed assistant message. While `status` is `Streaming` it
    /// renders as plain text; once it reaches `Completed`, it re-renders
    /// through the markdown pipeline.
    Assistant(AssistantMessage),
    /// A streamed agent thought (Copilot's internal reasoning).
    Thought(Thought),
    /// A user message replayed from session history during load/resume.
    User(UserMessage),
    /// A tool call. Title and status update through `watch` channels.
    Tool(ToolCall),
}

pub struct Agent {
    pub id: AgentId,
    pub name: String,
    pub status: AgentStatus,
    pub history: Vec<HistoryEntry>,
    /// Command to launch the ACP agent (e.g. "copilot --acp --stdio").
    pub acp_command: String,
    /// Optional repo path with `.devcontainer/` config.
    /// When set, the agent runs inside a dev container.
    pub workspace_folder: Option<PathBuf>,
    /// Channel for sending prompts to the persistent ACP connection.
    /// `None` until the connection is established.
    pub prompt_tx: Option<mpsc::UnboundedSender<String>>,
    /// Handle to abort the agent's background task (container start + ACP loop).
    /// Used by `:rebuild` to cancel the old task before starting a new one.
    pub task_handle: Option<AbortHandle>,
    /// ACP session ID — persisted across reconnections for session resume.
    pub session_id: Option<String>,
    /// Updated by `render_conversation` each frame: the line index that
    /// ended up at the top of the viewport after clamping. Read by the
    /// Up/Down handlers so they can decrement from "wherever you actually
    /// were on screen" instead of from the abstract `scroll` field
    /// (which is a sentinel like `usize::MAX` while auto-following).
    ///
    /// `Cell` because `render_conversation` only has `&Agent`.
    pub last_effective_top: Cell<usize>,
    /// Cached current git branch for the workspace, with the
    /// [`Instant`] it was last read. `None` if the workspace isn't a
    /// git working tree. Recomputed by [`Agent::git_branch`] when the
    /// cache is older than [`GIT_BRANCH_TTL`].
    git_branch_cache: RefCell<Option<(Instant, Option<String>)>>,
}

const GIT_BRANCH_TTL: Duration = Duration::from_secs(2);

impl Agent {
    pub fn new(id: impl Into<AgentId>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            status: AgentStatus::Idle,
            history: Vec::new(),
            acp_command: String::new(),
            workspace_folder: None,
            prompt_tx: None,
            task_handle: None,
            session_id: None,
            last_effective_top: Cell::new(0),
            git_branch_cache: RefCell::new(None),
        }
    }

    pub fn with_acp_command(mut self, command: impl Into<String>) -> Self {
        self.acp_command = command.into();
        self
    }

    pub fn with_workspace(mut self, path: impl Into<PathBuf>) -> Self {
        self.workspace_folder = Some(path.into());
        self.git_branch_cache.replace(None);
        self
    }

    /// The effective command to run.
    ///
    /// When no workspace is set, returns the raw ACP command.
    /// When a workspace is set, the container is started separately by
    /// `agent_runtime` and this just returns the raw ACP command — the
    /// runtime handles exec via the container ID.
    pub fn effective_acp_command(&self) -> String {
        self.acp_command.clone()
    }

    /// Current git branch of [`workspace_folder`], if any. Cached for
    /// [`GIT_BRANCH_TTL`] so renders don't hit the filesystem on every
    /// frame; the value still updates promptly if the user checks out
    /// another branch from outside the TUI.
    pub fn git_branch(&self) -> Option<String> {
        let workspace = self.workspace_folder.as_ref()?;
        let now = Instant::now();
        if let Some((at, value)) = self.git_branch_cache.borrow().as_ref()
            && now.duration_since(*at) < GIT_BRANCH_TTL
        {
            return value.clone();
        }
        let value = fleet_commander_core::git::current_branch(workspace);
        self.git_branch_cache.replace(Some((now, value.clone())));
        value
    }

    /// Append an informational line.
    pub fn info(&mut self, line: impl Into<String>) {
        self.history.push(HistoryEntry::Info(line.into()));
    }

    /// Append an error line.
    pub fn error(&mut self, line: impl Into<String>) {
        self.history.push(HistoryEntry::Error(line.into()));
    }

    /// Append a user prompt.
    pub fn prompt(&mut self, line: impl Into<String>) {
        self.history.push(HistoryEntry::Prompt(line.into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_command_without_workspace() {
        let agent = Agent::new("test", "Test").with_acp_command("copilot --acp --stdio");
        assert_eq!(agent.effective_acp_command(), "copilot --acp --stdio");
    }

    #[test]
    fn effective_command_with_workspace() {
        let agent = Agent::new("test", "Test")
            .with_acp_command("copilot --acp --stdio")
            .with_workspace("/home/user/my-repo");
        assert_eq!(agent.effective_acp_command(), "copilot --acp --stdio");
    }
}
