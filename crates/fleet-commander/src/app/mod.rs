//! Application state machine.
//!
//! The UI is structured as two screens:
//!
//!   * `Screen::AgentList` — top-level overview of all agents.
//!   * `Screen::AgentSession` — immersive view of a single agent. The
//!     conversation/history is the main pane; a `SidePane` (Diff or Editor)
//!     can appear on the right, but only when invoked by a change event or
//!     by the user.
//!
//! Input handling is dispatched per-screen so a keypress can never silently
//! mutate a hidden buffer.

use std::fs::File;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::info;

use fleet_commander_core::session::SessionEvent;

use crate::agent::{Agent, AgentId};
use crate::completion::PathCompleter;
use crate::config::Config;
use crate::event::AppEvent;
use crate::explorer::ExplorerState;

mod actions;
mod commands;
mod explorer;
mod input;
mod search;
mod session;
mod types;
pub use types::{Screen, SessionFocus, SidePane};

use actions::{handle_list_action, handle_session_action, spawn_text_tracker, spawn_tool_tracker};

pub struct App {
    pub config: Config,
    pub agents: Vec<Agent>,
    pub screen: Screen,
    pub should_quit: bool,
    /// Text the user is composing in insert mode.
    pub input_buffer: String,
    /// When true, the user is typing a `:` command (vim-style command mode).
    pub command_mode: bool,
    /// Buffer for the current command being typed.
    pub command_buffer: String,
    /// Ephemeral status message shown in the footer (e.g. error from a command).
    pub status_message: Option<String>,
    /// Tab-completion state for command mode paths.
    pub completer: PathCompleter,
    /// Channel for sending events (used to dispatch messages to agents).
    pub tx: mpsc::UnboundedSender<AppEvent>,
    /// Channel handed to the runtime crate for it to emit `RuntimeEvent`s.
    /// A bridge task in `main.rs` forwards these into `tx` as `AppEvent`s.
    pub runtime_tx: mpsc::UnboundedSender<SessionEvent>,
    /// Set when an agent needs interactive auth — the main loop suspends the
    /// TUI and runs this command with inherited stdio.
    pub auth_pending: Option<(AgentId, Vec<String>)>,
    /// Pending tool permission request awaiting user response.
    /// Contains (tool_name, options: Vec<(id, label, kind)>, reply_channel).
    pub permission_pending: Option<PendingPermission>,
    /// Shared handle to the ACP wire-log file when `--acp-log` was passed.
    /// `None` disables logging.
    pub acp_log: Option<Arc<Mutex<File>>>,
    /// Optional substring filter applied to agent id when deciding whether
    /// to enable ACP logging for a given agent.
    pub acp_log_filter: Option<String>,
    /// Lazy tree-view state for the current agent's workspace. Reset
    /// whenever the active workspace changes. Toggle visibility with
    /// `Ctrl+E`.
    pub explorer: ExplorerState,
    /// Selected index in the slash-command autocomplete popover.
    /// Re-clamped against the filtered list every time the buffer
    /// changes. Meaningful only while [`ui::slash_popover::extract_prefix`]
    /// returns `Some` for [`Self::input_buffer`].
    pub slash_selected: usize,
    /// When true, the user is typing a workspace search query (opened with
    /// `/` while the explorer is focused). Captures keys like command mode.
    pub search_mode: bool,
    /// Buffer for the search query being typed while [`Self::search_mode`].
    pub search_query: String,
    /// Monotonic id handed to each launched search so streamed
    /// `fs.searchResult`/`fs.searchDone` events can be correlated to the
    /// pane that started them (and stale results dropped).
    pub search_next_id: u64,
}

/// A tool permission request waiting for the user's decision. Rendered
/// as a centered modal popup that fully captures keyboard input while
/// it's open (see [`ui::permission_popup`]).
pub struct PendingPermission {
    pub tool_name: String,
    /// Options offered by the agent as `(id, label, kind)`. `kind` is one
    /// of `allow once`/`allow always`/`reject once`/`reject always`.
    pub options: Vec<(String, String, String)>,
    pub reply: crate::event::PermissionReply,
    /// Highlighted option in the popup, navigated with Up/Down.
    pub selected: usize,
}

impl App {
    #[cfg(test)]
    pub fn new(config: Config, agents: Vec<Agent>, tx: mpsc::UnboundedSender<AppEvent>) -> Self {
        let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
        Self::with_acp_log(config, agents, tx, runtime_tx, None, None)
    }

    pub fn with_acp_log(
        config: Config,
        agents: Vec<Agent>,
        tx: mpsc::UnboundedSender<AppEvent>,
        runtime_tx: mpsc::UnboundedSender<SessionEvent>,
        acp_log: Option<Arc<Mutex<File>>>,
        acp_log_filter: Option<String>,
    ) -> Self {
        Self {
            config,
            agents,
            screen: Screen::AgentList { selected: 0 },
            should_quit: false,
            input_buffer: String::new(),
            command_mode: false,
            command_buffer: String::new(),
            status_message: None,
            completer: PathCompleter::default(),
            tx,
            runtime_tx,
            auth_pending: None,
            permission_pending: None,
            acp_log,
            acp_log_filter,
            explorer: ExplorerState::default(),
            slash_selected: 0,
            search_mode: false,
            search_query: String::new(),
            search_next_id: 0,
        }
    }

    pub fn handle(&mut self, event: AppEvent) {
        match event {
            AppEvent::Input(key) => self.handle_key(key),
            AppEvent::Change(change) => self.handle_change(change),
            AppEvent::McpShowDiff {
                agent_id,
                path,
                content,
            } => self.handle_mcp_side_pane(
                agent_id,
                SidePane::Diff {
                    path,
                    content,
                    scroll: 0,
                },
            ),
            AppEvent::McpShowFile {
                agent_id,
                path,
                content,
            } => self.handle_mcp_side_pane(
                agent_id,
                SidePane::FileView {
                    path,
                    content,
                    scroll: 0,
                },
            ),
            AppEvent::McpNotify { agent_id, message } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.info(message);
                }
            }
            AppEvent::ReconnectAgent { agent_id } => {
                info!(agent_id = %agent_id, "Reconnecting agent after rebuild");
                self.ensure_agent_connected(agent_id);
            }
            AppEvent::Repaint => {
                // No-op: the redraw is performed by the main loop after this
                // handler returns. Repaint events exist purely to wake the
                // event loop when a tracked handle (tool call, streaming
                // text, etc.) ticks. We deliberately do not snap scroll
                // here — if the user is reading history, leave them alone.
            }
            AppEvent::ExplorerStatusReady {
                root,
                include_ignored,
                result,
            } => {
                // Drop responses from a previous workspace or a stale
                // include_ignored value — they would overwrite the
                // current state with the wrong data.
                let root_matches = self
                    .explorer
                    .fs
                    .as_ref()
                    .map(|fs| fs.root_display() == root)
                    .unwrap_or(false);
                if root_matches && include_ignored == self.explorer.show_ignored {
                    self.explorer.apply_status(result);
                } else {
                    self.explorer.is_refreshing = false;
                }
                if self.explorer.refresh_pending {
                    self.request_explorer_refresh();
                }
            }
            AppEvent::ExplorerFsReady {
                agent_id,
                container_id,
                fs,
            } => {
                // Install only if this agent is still the one on screen, the
                // explorer is pointed at the same workspace root (the user may
                // have navigated away while we connected), and the agent is
                // still backed by the *same* container this fs was bound to.
                // The container check rejects a stale install whose handshake
                // raced a `:rebuild` (which swaps in a new container).
                let same_root = self
                    .explorer
                    .fs
                    .as_ref()
                    .map(|cur| cur.root_display() == fs.root_display())
                    .unwrap_or(false);
                let same_container = self
                    .agents
                    .iter()
                    .find(|a| a.id == agent_id)
                    .and_then(|a| a.container.as_ref())
                    .map(|c| c.container_id == container_id)
                    .unwrap_or(false);
                if self.viewed_agent_id().as_ref() == Some(&agent_id) && same_root && same_container
                {
                    self.explorer.set_fs(Some(fs));
                    self.request_explorer_refresh();
                }
            }
            AppEvent::ExplorerFsChanged {
                agent_id,
                container_id,
            } => {
                // A live filesystem change inside the container. Re-list (the
                // set of files may have changed) and refresh git status, but
                // only while this agent is still on screen and still backed by
                // the same container the watch was bound to — a stale push
                // from a torn-down container must not disturb the new view.
                let same_container = self
                    .agents
                    .iter()
                    .find(|a| a.id == agent_id)
                    .and_then(|a| a.container.as_ref())
                    .map(|c| c.container_id == container_id)
                    .unwrap_or(false);
                if self.viewed_agent_id().as_ref() == Some(&agent_id)
                    && same_container
                    && self.explorer.open
                    && self.explorer.fs.is_some()
                {
                    self.explorer.invalidate_dirs();
                    self.request_explorer_refresh();
                }
            }
            AppEvent::ExplorerDirReady { root, rel, result } => {
                let root_matches = self
                    .explorer
                    .fs
                    .as_ref()
                    .map(|fs| fs.root_display() == root)
                    .unwrap_or(false);
                if root_matches {
                    self.explorer.apply_dir(rel, result);
                }
            }
            AppEvent::ExplorerFileReady {
                agent_id,
                root,
                full_path,
                result,
                scroll_to,
            } => {
                let root_matches = self
                    .explorer
                    .fs
                    .as_ref()
                    .map(|fs| fs.root_display() == root)
                    .unwrap_or(false);
                if let Ok(content) = result
                    && root_matches
                    && self.viewed_agent_id().as_ref() == Some(&agent_id)
                    && let Screen::AgentSession { side_pane, .. } = &mut self.screen
                {
                    *side_pane = Some(SidePane::FileView {
                        path: full_path,
                        content,
                        scroll: scroll_to,
                    });
                }
            }
            AppEvent::ExplorerDiffReady {
                agent_id,
                root,
                full_path,
                result,
            } => {
                let root_matches = self
                    .explorer
                    .fs
                    .as_ref()
                    .map(|fs| fs.root_display() == root)
                    .unwrap_or(false);
                if let Ok(diff) = result
                    && root_matches
                    && self.viewed_agent_id().as_ref() == Some(&agent_id)
                    && let Screen::AgentSession { side_pane, .. } = &mut self.screen
                {
                    let content = if diff.trim().is_empty() {
                        "No changes.".to_string()
                    } else {
                        diff
                    };
                    *side_pane = Some(SidePane::Diff {
                        path: full_path,
                        content,
                        scroll: 0,
                    });
                }
            }
            AppEvent::SearchResults {
                agent_id,
                search_id,
                matches,
            } => {
                if self.viewed_agent_id().as_ref() == Some(&agent_id)
                    && let Screen::AgentSession {
                        side_pane:
                            Some(SidePane::Search {
                                search_id: pane_id,
                                matches: pane_matches,
                                ..
                            }),
                        ..
                    } = &mut self.screen
                    && *pane_id == search_id
                {
                    pane_matches.extend(matches);
                }
            }
            AppEvent::SearchDone {
                agent_id,
                search_id,
                summary,
            } => {
                if self.viewed_agent_id().as_ref() == Some(&agent_id)
                    && let Screen::AgentSession {
                        side_pane:
                            Some(SidePane::Search {
                                search_id: pane_id,
                                running,
                                summary: pane_summary,
                                ..
                            }),
                        ..
                    } = &mut self.screen
                    && *pane_id == search_id
                {
                    *running = false;
                    *pane_summary = Some(summary);
                }
            }
            AppEvent::AgentBranchReady {
                agent_id,
                container_id,
                branch,
            } => {
                // Only apply if the agent still has the same container the
                // branch was read from (a `:rebuild` swaps containers and
                // clears the branch; a stale read must not resurrect it).
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id)
                    && agent
                        .container
                        .as_ref()
                        .map(|c| c.container_id == container_id)
                        .unwrap_or(false)
                {
                    agent.git_branch = branch;
                }
            }
            AppEvent::Session(event) => self.handle_session_event(event),
        }
        self.sync_explorer();
    }

    /// The agent currently shown in the immersive session screen, if any.
    fn viewed_agent_id(&self) -> Option<AgentId> {
        match &self.screen {
            Screen::AgentSession { agent_id, .. } => Some(agent_id.clone()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
