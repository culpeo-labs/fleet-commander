//! Screen / pane state types for the application state machine.

use std::path::PathBuf;

use crate::agent::AgentId;

#[derive(Debug, Clone)]
pub enum Screen {
    AgentList {
        selected: usize,
    },
    AgentSession {
        agent_id: AgentId,
        focus: SessionFocus,
        side_pane: Option<SidePane>,
        scroll: usize,
        /// When true, the user is typing a message to send to the agent.
        input_mode: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionFocus {
    Conversation,
    SidePane,
    Explorer,
}

#[derive(Debug, Clone)]
pub enum SidePane {
    /// Auto-opened diff of a file the agent (or the fs watcher) just
    /// changed. May be replaced whenever a fresh change event arrives.
    Diff {
        path: PathBuf,
        content: String,
        scroll: u16,
    },
    /// A file the user explicitly opened from the explorer (Enter on a
    /// file). Unlike [`SidePane::Diff`], this is **not** clobbered by
    /// background change events — the user asked to read this file and
    /// keeps looking at it until they dismiss it or open something else.
    FileView {
        path: PathBuf,
        content: String,
        scroll: u16,
    },
    /// Browsable list of slash commands the active agent advertised
    /// (via ACP `available_commands_update`). Opened with `:commands`.
    Commands {
        commands: Vec<crate::agent::AvailableCommand>,
        scroll: u16,
    },
    /// Streaming content-search results for the workspace. Populated
    /// incrementally as `fs.searchResult` batches arrive and finalized
    /// by `fs.searchDone`. `selected` is the highlighted result row (for
    /// jump-to-file); `running` is true until the terminal summary lands.
    Search {
        query: String,
        search_id: u64,
        matches: Vec<fleet_commander_core::fleet_protocol::SearchMatch>,
        selected: usize,
        scroll: u16,
        running: bool,
        summary: Option<fleet_commander_core::fleet_protocol::SearchSummary>,
    },
}

impl SidePane {
    /// Mutable handle to the pane's scroll offset, for key handlers.
    pub fn scroll_mut(&mut self) -> &mut u16 {
        match self {
            SidePane::Diff { scroll, .. }
            | SidePane::FileView { scroll, .. }
            | SidePane::Commands { scroll, .. }
            | SidePane::Search { scroll, .. } => scroll,
        }
    }

    /// Whether a background change event is allowed to replace this pane
    /// with an auto-diff. Only the auto-managed [`SidePane::Diff`] yields;
    /// user-opened panes ([`FileView`], [`Commands`]) keep their place.
    pub fn yields_to_auto_diff(&self) -> bool {
        matches!(self, SidePane::Diff { .. })
    }
}
