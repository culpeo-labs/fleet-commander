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

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{info, warn};

use fleet_commander_core::service_fs::ServiceFs;
use fleet_commander_core::session::{MessageStatus, SessionEvent, ToolCallStatusKind};
use fleet_commander_core::workspace_fs::{LocalFs, WorkspaceFs};
use fleet_commander_core::{agent_runtime, container};

use crate::agent::{Agent, AgentId, AgentStatus, ContainerInfo, HistoryEntry};
use crate::agent_kind::AgentKind;
use crate::change_source::ChangeEvent;
use crate::completion::{PathCompleter, split_command_and_path};
use crate::config::{Action, Config};
use crate::event::AppEvent;
use crate::explorer::ExplorerState;
use crate::init;
use crate::workspace;

/// Cap on how many content-search hits the daemon streams back before it
/// stops and flags the result truncated. Keeps a broad match on a large tree
/// from flooding the UI.
const SEARCH_MAX_RESULTS: u64 = 2_000;

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

    /// Connect to the in-container `fleet-agent` on a background thread and,
    /// once the (blocking) handshake completes, hand the resulting
    /// [`ServiceFs`] back to the event loop via [`AppEvent::ExplorerFsReady`].
    ///
    /// On failure (no binary mounted, container gone, …) the explorer simply
    /// stays on the host-side [`LocalFs`].
    fn request_service_fs_upgrade(
        &self,
        agent_id: AgentId,
        info: ContainerInfo,
        workspace: PathBuf,
    ) {
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let container_id = info.container_id.clone();
            // Route live `fs.didChange` pushes back into the event loop so the
            // explorer refreshes itself when files change inside the container.
            // The sink runs on the transport's reader thread, so it only does
            // a cheap non-blocking channel send.
            let sink: fleet_commander_core::service_fs::NotificationSink = {
                use fleet_commander_core::fleet_protocol::{
                    Notification, SearchDoneParams, SearchResultParams, methods,
                };
                let tx = tx.clone();
                let agent_id = agent_id.clone();
                let container_id = container_id.clone();
                Box::new(move |note| {
                    let Notification { method, params, .. } = note;
                    match method.as_str() {
                        m if m == methods::FS_DID_CHANGE => {
                            let _ = tx.send(AppEvent::ExplorerFsChanged {
                                agent_id: agent_id.clone(),
                                container_id: container_id.clone(),
                            });
                        }
                        m if m == methods::FS_SEARCH_RESULT => {
                            if let Some(params) = params
                                .and_then(|p| serde_json::from_value::<SearchResultParams>(p).ok())
                            {
                                let _ = tx.send(AppEvent::SearchResults {
                                    agent_id: agent_id.clone(),
                                    search_id: params.search_id,
                                    matches: params.matches,
                                });
                            }
                        }
                        m if m == methods::FS_SEARCH_DONE => {
                            if let Some(params) = params
                                .and_then(|p| serde_json::from_value::<SearchDoneParams>(p).ok())
                            {
                                let _ = tx.send(AppEvent::SearchDone {
                                    agent_id: agent_id.clone(),
                                    search_id: params.search_id,
                                    summary: params.summary,
                                });
                            }
                        }
                        _ => {}
                    }
                })
            };
            match ServiceFs::connect_docker_watched(
                workspace,
                &info.remote_workspace_folder,
                &info.container_id,
                &info.remote_user,
                fleet_commander_core::agent_bin::CONTAINER_AGENT_PATH,
                Some(sink),
            ) {
                Ok(fs) => {
                    let _ = tx.send(AppEvent::ExplorerFsReady {
                        agent_id,
                        container_id,
                        fs: Arc::new(fs) as Arc<dyn WorkspaceFs>,
                    });
                }
                Err(e) => {
                    info!(error = %e, "Container service unavailable; explorer stays on host filesystem");
                }
            }
        });
    }

    /// Read the agent's git branch from inside its container on a background
    /// thread (a one-shot `docker exec` via the same `fleet-agent` service the
    /// explorer uses) and deliver it as [`AppEvent::AgentBranchReady`].
    ///
    /// No-op if the agent has no started container — we deliberately never read
    /// the host bind-mount, so the header/list branch and the explorer's git
    /// status always reflect the same (container) filesystem.
    fn refresh_agent_branch(&self, agent_id: AgentId) {
        let Some((info, workspace)) = self.agents.iter().find(|a| a.id == agent_id).and_then(|a| {
            let info = a.container.clone()?;
            let ws = a.workspace_folder.clone()?;
            Some((info, ws))
        }) else {
            return;
        };
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let container_id = info.container_id.clone();
            let branch = match ServiceFs::connect_docker(
                workspace,
                &info.remote_workspace_folder,
                &info.container_id,
                &info.remote_user,
                fleet_commander_core::agent_bin::CONTAINER_AGENT_PATH,
            ) {
                Ok(fs) => fs.git_branch(),
                Err(e) => {
                    info!(error = %e, "Branch fetch failed; container service unavailable");
                    return;
                }
            };
            let _ = tx.send(AppEvent::AgentBranchReady {
                agent_id,
                container_id,
                branch,
            });
        });
    }

    fn handle_session_event(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::Output { agent_id, line } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.info(line);
                }
            }
            SessionEvent::ContainerReady {
                agent_id,
                container_id,
                remote_user,
                remote_workspace_folder,
            } => {
                let info = ContainerInfo {
                    container_id,
                    workspace_folder: PathBuf::new(),
                    remote_workspace_folder,
                    remote_user,
                };
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.container = Some(info.clone());
                }
                // Fetch the branch from inside the new container (covers both
                // the viewed agent's header and any other agent's list row).
                self.refresh_agent_branch(agent_id.clone());
                // If this agent's explorer is on screen, upgrade it from the
                // host filesystem to the in-container service.
                if self.viewed_agent_id().as_ref() == Some(&agent_id)
                    && let Some(ws) = self
                        .agents
                        .iter()
                        .find(|a| a.id == agent_id)
                        .and_then(|a| a.workspace_folder.clone())
                {
                    self.request_service_fs_upgrade(agent_id, info, ws);
                }
            }
            SessionEvent::Exited { agent_id, .. } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.status = AgentStatus::Stopped;
                    agent.prompt_tx = None;
                    agent.task_handle = None;
                }
            }
            SessionEvent::Error { agent_id, message } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.error(format!("[error] {message}"));
                    agent.status = AgentStatus::Error;
                }
            }
            SessionEvent::Connected {
                agent_id,
                session_id,
            } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.status = AgentStatus::Idle;
                    agent.session_id = session_id.clone();
                    agent.info("ACP session connected.");
                    if let Some(ws) = &agent.workspace_folder {
                        let state = workspace::WorkspaceState { session_id };
                        if let Err(e) = workspace::save_state(ws, &state) {
                            warn!(error = %e, "Failed to save workspace state");
                        }
                    }
                }
            }
            SessionEvent::AuthRequired { agent_id, command } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.info("🔑 Authentication required — launching login flow...");
                    agent.status = AgentStatus::Stopped;
                    agent.prompt_tx = None;
                    // The runtime task already returned after sending this
                    // event; clear its handle so ensure_agent_connected can
                    // spawn a fresh task after login completes.
                    agent.task_handle = None;
                }
                self.auth_pending = Some((agent_id, command));
            }
            SessionEvent::PermissionRequest {
                agent_id,
                tool_name,
                options,
                reply,
            } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.info(format!("🔐 Permission requested: {tool_name}"));
                }
                self.permission_pending = Some(PendingPermission {
                    tool_name,
                    options,
                    reply,
                    selected: 0,
                });
            }
            SessionEvent::AssistantMessage { agent_id, message } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.status = AgentStatus::Running;
                    agent.history.push(HistoryEntry::Assistant(message.clone()));
                }
                spawn_text_tracker(message.text, message.status, self.tx.clone());
            }
            SessionEvent::Thought { agent_id, thought } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.history.push(HistoryEntry::Thought(thought.clone()));
                }
                spawn_text_tracker(thought.text, thought.status, self.tx.clone());
            }
            SessionEvent::UserMessage { agent_id, message } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.history.push(HistoryEntry::User(message.clone()));
                }
                spawn_text_tracker(message.text, message.status, self.tx.clone());
            }
            SessionEvent::ToolCall { agent_id, call } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.history.push(HistoryEntry::Tool(call.clone()));
                }
                spawn_tool_tracker(call.title, call.status, self.tx.clone());
            }
            SessionEvent::AvailableCommands { agent_id, commands } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.available_commands = commands;
                }
            }
        }
    }

    /// Answer the pending permission request and tear down the popup.
    /// `choice` is `Some(index)` of the option to allow/select, or `None`
    /// to reject (no option chosen). Sends the option id back through the
    /// oneshot the runtime is awaiting.
    fn resolve_permission(&mut self, choice: Option<usize>) {
        let Some(perm) = self.permission_pending.take() else {
            return;
        };
        let option_id = choice
            .and_then(|idx| perm.options.get(idx))
            .map(|(id, _, _)| id.clone());
        if let Ok(mut guard) = perm.reply.lock()
            && let Some(tx) = guard.take()
        {
            let _ = tx.send(option_id);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Clear status message on any keypress.
        self.status_message = None;

        // Permission prompt — a modal popup that fully owns keyboard input
        // while open. Up/Down (or j/k) move the highlight, Enter/Space picks
        // the highlighted option, number keys 1-9 pick directly, and Esc (or
        // 'n'/'q') rejects. Because this returns, no keystrokes can leak into
        // the input box or any other handler while the popup is up.
        if let Some(perm) = &mut self.permission_pending {
            let count = perm.options.len();
            match key.code {
                KeyCode::Up | KeyCode::Char('k') if count > 0 => {
                    perm.selected = perm.selected.checked_sub(1).unwrap_or(count - 1);
                }
                KeyCode::Down | KeyCode::Char('j') if count > 0 => {
                    perm.selected = (perm.selected + 1) % count;
                }
                KeyCode::Char(c @ '1'..='9') => {
                    let idx = (c as usize) - ('1' as usize);
                    if idx < count {
                        self.resolve_permission(Some(idx));
                    }
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    let idx = perm.selected;
                    self.resolve_permission(Some(idx));
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
                    self.resolve_permission(None);
                }
                _ => {}
            }
            return;
        }

        // Command mode (`:` prompt) — intercept all keys.
        if self.command_mode {
            match key.code {
                KeyCode::Esc => {
                    self.command_mode = false;
                    self.command_buffer.clear();
                    self.completer.reset();
                }
                KeyCode::Enter => {
                    let cmd = std::mem::take(&mut self.command_buffer);
                    self.command_mode = false;
                    self.completer.reset();
                    self.execute_command(&cmd);
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    let (verb, partial) = split_command_and_path(&self.command_buffer);
                    let verb = verb.to_string();
                    // Only complete paths for commands that take a path arg.
                    if matches!(verb.as_str(), "open" | "o") {
                        let partial = partial.to_string();
                        let completed = if key.code == KeyCode::Tab {
                            self.completer.complete(&partial).map(String::from)
                        } else {
                            self.completer.complete_prev(&partial).map(String::from)
                        };
                        if let Some(path) = completed {
                            self.command_buffer = format!("{verb} {path}");
                        }
                    }
                }
                KeyCode::Backspace => {
                    self.completer.reset();
                    if self.command_buffer.pop().is_none() {
                        self.command_mode = false;
                    }
                }
                KeyCode::Char(c) => {
                    self.completer.reset();
                    self.command_buffer.push(c);
                }
                _ => {}
            }
            return;
        }

        // Search input mode (`/` prompt in the explorer) — intercept keys to
        // build the query. Enter launches the search, Esc aborts.
        if self.search_mode {
            match key.code {
                KeyCode::Esc => {
                    self.search_mode = false;
                    self.search_query.clear();
                }
                KeyCode::Enter => {
                    self.search_mode = false;
                    let query = std::mem::take(&mut self.search_query);
                    self.launch_search(query);
                }
                KeyCode::Backspace => {
                    if self.search_query.pop().is_none() {
                        self.search_mode = false;
                    }
                }
                KeyCode::Char(c) => self.search_query.push(c),
                _ => {}
            }
            return;
        }

        // In input mode, capture text instead of dispatching actions.
        if let Screen::AgentSession {
            input_mode: true,
            agent_id,
            ..
        } = &self.screen
        {
            match key.code {
                KeyCode::Esc => {
                    if let Screen::AgentSession { input_mode, .. } = &mut self.screen {
                        *input_mode = false;
                    }
                    self.input_buffer.clear();
                    self.slash_selected = 0;
                }
                KeyCode::Up => {
                    if let Some(matches) = self.slash_matches_for(agent_id)
                        && !matches.is_empty()
                    {
                        self.slash_selected = self
                            .slash_selected
                            .checked_sub(1)
                            .unwrap_or(matches.len() - 1);
                    }
                }
                KeyCode::Down => {
                    if let Some(matches) = self.slash_matches_for(agent_id)
                        && !matches.is_empty()
                    {
                        self.slash_selected = (self.slash_selected + 1) % matches.len();
                    }
                }
                KeyCode::Tab => {
                    // Tab-completion only fires while a slash command is
                    // being typed; let other Tabs through (currently no-op
                    // in input mode).
                    if let Some(matches) = self.slash_matches_for(agent_id)
                        && let Some(picked) =
                            matches.get(self.slash_selected.min(matches.len().saturating_sub(1)))
                    {
                        self.input_buffer = crate::ui::slash_popover::completion_for(&picked.name);
                        self.slash_selected = 0;
                    }
                }
                KeyCode::Enter => {
                    // Alt+Enter / Shift+Enter insert a newline so the user
                    // can compose multi-line messages. Plain Enter sends.
                    if key
                        .modifiers
                        .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT)
                    {
                        self.input_buffer.push('\n');
                        return;
                    }
                    let message = std::mem::take(&mut self.input_buffer);
                    self.slash_selected = 0;
                    if !message.is_empty() {
                        let agent_id = agent_id.clone();
                        if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                            agent.prompt(message.clone());
                            agent_runtime::send_message(
                                agent.id.clone(),
                                agent.prompt_tx.as_ref(),
                                message,
                                self.runtime_tx.clone(),
                            );
                        }
                        self.auto_scroll_for(&agent_id);
                    }
                    if let Screen::AgentSession { input_mode, .. } = &mut self.screen {
                        *input_mode = false;
                    }
                }
                KeyCode::Backspace => {
                    self.input_buffer.pop();
                    self.slash_selected = 0;
                }
                KeyCode::Char(c) => {
                    self.input_buffer.push(c);
                    self.slash_selected = 0;
                }
                _ => {}
            }
            return;
        }

        let Some(action) = self.config.bindings.action_for(&key) else {
            // Explorer-focus-specific character keys that aren't part of
            // the global Action set: `r` refresh, `.` toggle ignored,
            // `@` insert reference + switch to input mode.
            if let Screen::AgentSession { focus, .. } = &self.screen
                && *focus == SessionFocus::Explorer
                && let KeyCode::Char(c) = key.code
                && !key.modifiers.contains(KeyModifiers::CONTROL)
            {
                match c {
                    'r' => {
                        // Manual refresh: also drop the remote directory
                        // cache so the tree re-lists (picking up files
                        // created/removed inside the container).
                        self.explorer.invalidate_dirs();
                        self.request_explorer_refresh();
                    }
                    '.' => {
                        self.explorer.show_ignored = !self.explorer.show_ignored;
                        // Re-query because the include_ignored flag changes
                        // what git returns.
                        self.request_explorer_refresh();
                    }
                    '@' => {
                        if let Some(entry) = self.explorer.selected_entry() {
                            let path = entry.path.display().to_string();
                            if !self.input_buffer.is_empty() && !self.input_buffer.ends_with(' ') {
                                self.input_buffer.push(' ');
                            }
                            self.input_buffer.push('@');
                            self.input_buffer.push_str(&path);
                            self.input_buffer.push(' ');
                            if let Screen::AgentSession {
                                input_mode, focus, ..
                            } = &mut self.screen
                            {
                                *input_mode = true;
                                *focus = SessionFocus::Conversation;
                            }
                        }
                    }
                    'D' => {
                        // Show the working-tree diff of the selected file in
                        // the side pane. Directories have no diff.
                        if let Some(entry) = self.explorer.selected_entry()
                            && !entry.is_dir
                        {
                            self.request_explorer_diff(entry.path);
                        }
                    }
                    '/' => {
                        // Begin composing a workspace content search. Only
                        // meaningful on a search-capable (remote) backend.
                        if self.explorer.fs.as_ref().is_some_and(|fs| fs.is_remote()) {
                            self.search_mode = true;
                            self.search_query.clear();
                        } else {
                            self.status_message =
                                Some("Search needs a container-backed workspace".into());
                        }
                    }
                    _ => {}
                }
            }
            return;
        };

        // Command mode activation works on any screen.
        if action == Action::Command {
            self.command_mode = true;
            self.command_buffer.clear();
            return;
        }

        // Snapshot a running search before dispatch so we can cancel it if
        // this action dismisses (or replaces) the pane. Spawning the cancel
        // RPC needs `&mut App`, so it can't happen inside the pure dispatcher.
        let running_before = self.running_search_id();

        let next = match &mut self.screen {
            Screen::AgentList { selected } => {
                handle_list_action(action, selected, &self.agents, &mut self.should_quit)
            }
            Screen::AgentSession {
                agent_id,
                focus,
                side_pane,
                scroll,
                ..
            } => handle_session_action(
                action,
                agent_id,
                focus,
                side_pane,
                scroll,
                &self.agents,
                &mut self.explorer,
            ),
        };
        if let Some(next) = next {
            self.screen = next;
            // Lazily start ACP connection when entering an agent session.
            if let Screen::AgentSession { agent_id, .. } = &self.screen {
                self.ensure_agent_connected(agent_id.clone());
            }
        }
        // If a running search's pane is no longer present (dismissed or
        // replaced), tell the daemon to stop it so it doesn't keep scanning.
        if let Some(id) = running_before
            && self.running_search_id() != Some(id)
        {
            self.cancel_search(id);
        }
        // Toggling the explorer open is the one mutation handle_session_action
        // makes that the user expects to see freshly-resolved git status for.
        // Issue the refresh from here because spawning the background task
        // needs `&mut App`.
        if action == Action::ToggleExplorer && self.explorer.open && self.explorer.fs.is_some() {
            self.request_explorer_refresh();
        }
    }

    /// Keep the explorer's remote view consistent after any event:
    /// schedule background fetches for directories the current tree
    /// needs but hasn't cached, and service a pending file-open. Cheap
    /// no-op for a closed explorer or a local (synchronous) filesystem.
    fn sync_explorer(&mut self) {
        if !self.explorer.open {
            return;
        }
        if let Some(rel) = self.explorer.pending_open.take() {
            match self.explorer.pending_open_line.take() {
                Some(line) => self.open_search_result(rel, line),
                None => self.open_explorer_file(rel),
            }
        }
        let Some(fs) = self.explorer.fs.clone() else {
            return;
        };
        if !fs.is_remote() {
            return;
        }
        let root = fs.root_display().to_path_buf();
        for rel in self.explorer.missing_dirs() {
            self.explorer.mark_dir_loading(rel.clone());
            let fs = fs.clone();
            let root = root.clone();
            let tx = self.tx.clone();
            tokio::task::spawn_blocking(move || {
                let result = fs.list_dir(&rel).map_err(|e| e.to_string());
                let _ = tx.send(AppEvent::ExplorerDirReady { root, rel, result });
            });
        }
    }

    /// Read a file for the explorer's side-pane preview off the UI
    /// thread (the read may be a remote RPC) and deliver it as
    /// [`AppEvent::ExplorerFileReady`].
    fn open_explorer_file(&self, rel: PathBuf) {
        let Some(fs) = self.explorer.fs.clone() else {
            return;
        };
        let Some(agent_id) = self.viewed_agent_id() else {
            return;
        };
        let root = fs.root_display().to_path_buf();
        let full_path = root.join(&rel);
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            // Cap the preview so opening a huge file never transfers or
            // buffers it in full on the UI path. 256 KiB is plenty for a
            // glance; larger files show a truncation marker.
            const PREVIEW_CAP: u64 = 256 * 1024;
            let result = fs
                .read_file_capped(&rel, PREVIEW_CAP)
                .map(|capped| {
                    let mut text = String::from_utf8_lossy(&capped.bytes).into_owned();
                    if capped.truncated {
                        text.push_str("\n\n… [truncated preview — file larger than 256 KiB]");
                    }
                    text
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::ExplorerFileReady {
                agent_id,
                root,
                full_path,
                result,
                scroll_to: 0,
            });
        });
    }

    /// Fetch a `git diff` for an explorer-selected file off the UI
    /// thread (the diff may be a remote RPC) and deliver it as
    /// [`AppEvent::ExplorerDiffReady`]. Shows the working-tree diff.
    fn request_explorer_diff(&self, rel: PathBuf) {
        let Some(fs) = self.explorer.fs.clone() else {
            return;
        };
        let Some(agent_id) = self.viewed_agent_id() else {
            return;
        };
        let root = fs.root_display().to_path_buf();
        let full_path = root.join(&rel);
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = fs.git_diff(&rel, false).map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::ExplorerDiffReady {
                agent_id,
                root,
                full_path,
                result,
            });
        });
    }

    /// Launch a streaming workspace content search for `query`. Cancels any
    /// still-running search first, opens a fresh [`SidePane::Search`] focused
    /// for navigation, and kicks off `fs.start_search` off the UI thread —
    /// results stream back via the notification sink as `SearchResults`/
    /// `SearchDone` events. No-op for an empty query or a non-search backend.
    fn launch_search(&mut self, query: String) {
        let query = query.trim().to_string();
        if query.is_empty() {
            return;
        }
        let Some(fs) = self.explorer.fs.clone() else {
            return;
        };
        if !fs.is_remote() {
            self.status_message = Some("Search needs a container-backed workspace".into());
            return;
        }
        // Stop a previous in-flight search so its late results can't bleed
        // into the new pane (they carry the old search_id and are dropped,
        // but cancelling also frees the daemon worker).
        if let Some(prev) = self.running_search_id() {
            self.cancel_search(prev);
        }

        let search_id = self.search_next_id;
        self.search_next_id += 1;

        if let Screen::AgentSession {
            side_pane, focus, ..
        } = &mut self.screen
        {
            *side_pane = Some(SidePane::Search {
                query: query.clone(),
                search_id,
                matches: Vec::new(),
                selected: 0,
                scroll: 0,
                running: true,
                summary: None,
            });
            *focus = SessionFocus::SidePane;
        }

        let params = fleet_commander_core::fleet_protocol::SearchParams {
            search_id,
            query,
            is_regex: false,
            case_sensitive: false,
            max_results: Some(SEARCH_MAX_RESULTS),
        };
        let tx = self.tx.clone();
        let agent_id = self.viewed_agent_id();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = fs.start_search(params) {
                info!(error = %e, "start_search failed");
                // Signal completion so the pane stops showing "searching…".
                if let Some(agent_id) = agent_id {
                    let _ = tx.send(AppEvent::SearchDone {
                        agent_id,
                        search_id,
                        summary: fleet_commander_core::fleet_protocol::SearchSummary {
                            count: 0,
                            truncated: false,
                            cancelled: true,
                        },
                    });
                }
            }
        });
    }

    /// The id of the currently-visible search if it is still running,
    /// otherwise `None`.
    fn running_search_id(&self) -> Option<u64> {
        match &self.screen {
            Screen::AgentSession {
                side_pane:
                    Some(SidePane::Search {
                        search_id,
                        running: true,
                        ..
                    }),
                ..
            } => Some(*search_id),
            _ => None,
        }
    }

    /// Ask the in-container service to stop `search_id` off the UI thread.
    /// Best-effort: the pane's `running` flag clears when the (still
    /// delivered) `fs.searchDone` summary arrives flagged cancelled.
    fn cancel_search(&self, search_id: u64) {
        let Some(fs) = self.explorer.fs.clone() else {
            return;
        };
        tokio::task::spawn_blocking(move || {
            let _ = fs.cancel_search(search_id);
        });
    }

    /// Open a search result in the side pane, jumping the preview to the
    /// match's line. Reads off the UI thread (a possibly-remote RPC) and
    /// delivers [`AppEvent::ExplorerFileReady`] with the target scroll.
    fn open_search_result(&self, rel: PathBuf, line: u64) {
        let Some(fs) = self.explorer.fs.clone() else {
            return;
        };
        let Some(agent_id) = self.viewed_agent_id() else {
            return;
        };
        let root = fs.root_display().to_path_buf();
        let full_path = root.join(&rel);
        // Center the match a few lines below the top of the viewport.
        let scroll_to = (line.saturating_sub(1)) as u16;
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            const PREVIEW_CAP: u64 = 256 * 1024;
            let result = fs
                .read_file_capped(&rel, PREVIEW_CAP)
                .map(|capped| {
                    let mut text = String::from_utf8_lossy(&capped.bytes).into_owned();
                    if capped.truncated {
                        text.push_str("\n\n… [truncated preview — file larger than 256 KiB]");
                    }
                    text
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::ExplorerFileReady {
                agent_id,
                root,
                full_path,
                result,
                scroll_to,
            });
        });
    }

    /// Spawn a background `git status` for the active workspace and
    /// pump the result back into the event loop as
    /// [`AppEvent::ExplorerStatusReady`]. Coalesces bursty callers:
    /// if a refresh is already in flight, sets a pending flag so a
    /// follow-up runs once the in-flight one lands.
    ///
    /// Cheap no-op when the explorer has no filesystem attached **or
    /// is closed** — there's no point spending cycles updating git
    /// state the user can't see.
    pub fn request_explorer_refresh(&mut self) {
        if !self.explorer.open {
            return;
        }
        let Some(fs) = self.explorer.fs.clone() else {
            return;
        };
        if self.explorer.is_refreshing {
            self.explorer.refresh_pending = true;
            return;
        }
        self.explorer.is_refreshing = true;
        self.explorer.refresh_pending = false;
        let include_ignored = self.explorer.show_ignored;
        let root = fs.root_display().to_path_buf();
        let tx = self.tx.clone();
        // `git status` is a sync subprocess; off-load it to the blocking
        // pool so the UI loop keeps draining events while it runs.
        tokio::task::spawn_blocking(move || {
            let result = fs.git_status(include_ignored).map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::ExplorerStatusReady {
                root,
                include_ignored,
                result,
            });
        });
    }

    /// Start the ACP connection for an agent if not already connected.
    pub fn ensure_agent_connected(&mut self, agent_id: AgentId) {
        // Point the explorer at this agent's workspace (no-op if same root).
        // This happens on every screen change into AgentSession, including
        // when the agent is already connected, so the explorer always reflects
        // the currently-viewed agent.
        if let Some(agent) = self.agents.iter().find(|a| a.id == agent_id) {
            let ws = agent.workspace_folder.clone();
            let container = agent.container.clone();
            let local = ws
                .as_ref()
                .map(|w| Arc::new(LocalFs::new(w)) as Arc<dyn WorkspaceFs>);
            // If the explorer already shows a container-backed fs for this
            // same root, don't downgrade it to LocalFs (and don't re-spawn
            // the upgrade) on a repeat entry into the session screen.
            let already_remote = match (&self.explorer.fs, &local) {
                (Some(cur), Some(l)) => cur.is_remote() && cur.root_display() == l.root_display(),
                _ => false,
            };
            if !already_remote {
                let had_fs = self.explorer.fs.is_some();
                self.explorer.set_fs(local);
                // Refresh status when the workspace is set for the first time
                // (or when switching to a new agent's workspace cleared state).
                if self.explorer.fs.is_some() && (!had_fs || self.explorer.status.is_empty()) {
                    self.request_explorer_refresh();
                }
                // Upgrade to the in-container service if the container is up.
                if let (Some(info), Some(w)) = (container, ws) {
                    self.request_service_fs_upgrade(agent_id.clone(), info, w);
                }
            }
        }
        let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) else {
            return;
        };
        // A task that already finished should not block a reconnect. This
        // can happen when the previous run exited cleanly (e.g. AuthRequired)
        // without the cleanup path clearing the handle.
        if let Some(handle) = &agent.task_handle
            && handle.is_finished()
        {
            agent.task_handle = None;
        }
        if agent.prompt_tx.is_some() || agent.task_handle.is_some() {
            return; // Already connected or connecting.
        }
        if agent.acp_command.is_empty() {
            return; // No command configured.
        }
        let log_for_agent = match (&self.acp_log, &self.acp_log_filter) {
            (Some(log), Some(pattern)) if agent.id.contains(pattern) => Some(log.clone()),
            (Some(log), None) => Some(log.clone()),
            _ => None,
        };
        let (prompt_tx, abort_handle) = agent_runtime::start_agent(
            agent.id.clone(),
            agent.effective_acp_command(),
            agent.workspace_folder.clone(),
            agent.session_id.clone(),
            self.runtime_tx.clone(),
            log_for_agent,
        );
        agent.prompt_tx = Some(prompt_tx);
        agent.task_handle = Some(abort_handle);
        agent.status = AgentStatus::Running;
        let label = match &agent.workspace_folder {
            Some(ws) => format!("Starting container ({})...", ws.display()),
            None => "Connecting...".into(),
        };
        agent.info(label);
    }

    /// Scroll to the bottom when content arrives for the currently viewed agent.
    /// Snap to the bottom of the conversation pane for the agent that the
    /// user is currently viewing.
    ///
    /// Called by callers (e.g. side-pane updates) that need to force an
    /// auto-scroll regardless of where the user has scrolled to. Routine
    /// streaming updates do **not** call this — they leave `scroll` alone
    /// so the user can read history without being yanked back to the bottom
    /// every time a chunk arrives. The user can re-engage follow-bottom
    /// with `Action::FollowBottom` (bound to `G` by default).
    fn auto_scroll_for(&mut self, agent_id: &str) {
        if let Screen::AgentSession {
            agent_id: current,
            scroll,
            ..
        } = &mut self.screen
        {
            if current != agent_id {
                return;
            }
            *scroll = usize::MAX;
        }
    }

    /// Returns the (alphabetically-ordered) slash-command matches for the
    /// current input buffer when the popover should be visible for
    /// `agent_id`, or `None` when the popover is closed (buffer doesn't
    /// look like a command, or the agent hasn't advertised any commands).
    pub fn slash_matches_for(
        &self,
        agent_id: &str,
    ) -> Option<Vec<&crate::agent::AvailableCommand>> {
        let prefix = crate::ui::slash_popover::extract_prefix(&self.input_buffer)?;
        let agent = self.agents.iter().find(|a| a.id == agent_id)?;
        if agent.available_commands.is_empty() {
            return None;
        }
        Some(crate::ui::slash_popover::filter(
            &agent.available_commands,
            prefix,
        ))
    }

    fn handle_change(&mut self, change: ChangeEvent) {
        if let Screen::AgentSession { side_pane, .. } = &mut self.screen {
            // Only auto-open / refresh the diff pane when it isn't already
            // showing something the user explicitly opened (a FileView or
            // the Commands browser). Clobbering those is the flicker bug:
            // a background fs change would yank the user's file preview
            // away and replace it with a diff.
            let may_replace = side_pane.as_ref().is_none_or(SidePane::yields_to_auto_diff);
            if may_replace {
                let content = std::fs::read_to_string(&change.path).unwrap_or_default();
                *side_pane = Some(SidePane::Diff {
                    path: change.path,
                    content,
                    scroll: 0,
                });
            }
        }
        if self.explorer.fs.is_some() {
            self.request_explorer_refresh();
        }
    }

    /// Open or replace the side pane when an MCP tool targets a specific agent.
    /// If that agent's session is currently visible, the pane updates immediately.
    /// If the agent list is showing, we navigate into the agent's session.
    fn handle_mcp_side_pane(&mut self, agent_id: AgentId, pane: SidePane) {
        match &mut self.screen {
            Screen::AgentSession {
                agent_id: current,
                side_pane,
                ..
            } if *current == agent_id => {
                *side_pane = Some(pane);
            }
            _ => {
                self.screen = Screen::AgentSession {
                    agent_id,
                    focus: SessionFocus::Conversation,
                    side_pane: Some(pane),
                    scroll: 0,
                    input_mode: false,
                };
            }
        }
        // A new diff means files on disk likely changed — refresh the
        // explorer's git cues so the user sees the same files highlighted.
        if self.explorer.fs.is_some() {
            self.request_explorer_refresh();
        }
    }

    /// Parse and execute a `:` command.
    fn execute_command(&mut self, cmd: &str) {
        let cmd = cmd.trim();
        if cmd.is_empty() {
            return;
        }
        let (verb, rest) = cmd.split_once(' ').unwrap_or((cmd, ""));
        let rest = rest.trim();
        match verb {
            "open" | "o" => {
                if rest.is_empty() {
                    self.status_message = Some("Usage: :open <path/to/repo>".into());
                } else {
                    self.open_workspace(rest);
                }
            }
            "close" => {
                self.close_current_workspace();
            }
            "rebuild" => {
                self.rebuild_current_container();
            }
            "commands" | "cmds" => {
                self.open_commands_view();
            }
            "q" | "quit" => {
                self.should_quit = true;
            }
            _ => {
                self.status_message = Some(format!("Unknown command: {verb}"));
            }
        }
    }

    /// Create a new Copilot agent for the given workspace path and navigate to it.
    pub fn open_workspace(&mut self, path: &str) {
        let workspace = PathBuf::from(path);
        let dir_name = workspace
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("workspace");
        let agent_id = format!("copilot-{dir_name}");

        // Check if an agent with this workspace already exists.
        if let Some(existing) = self.agents.iter().find(|a| a.id == agent_id) {
            self.screen = Screen::AgentSession {
                agent_id: existing.id.clone(),
                focus: SessionFocus::Conversation,
                side_pane: None,
                scroll: 0,
                input_mode: false,
            };
            self.ensure_agent_connected(agent_id);
            return;
        }

        let agent = Agent::new(&agent_id, format!("Copilot ({dir_name})"))
            .with_acp_command("copilot --acp --stdio")
            .with_workspace(&workspace);
        self.agents.push(agent);

        // Generate per-workspace base layer (mounts, env, etc.).
        if let Err(err) = init::generate_workspace_layer(&workspace, AgentKind::Copilot) {
            self.status_message = Some(format!("Layer warning: {err}"));
        }

        // Persist to workspaces.yaml.
        if let Err(err) = workspace::save(&workspace::from_agents(&self.agents)) {
            self.status_message = Some(format!("Warning: {err}"));
        }

        self.screen = Screen::AgentSession {
            agent_id: agent_id.clone(),
            focus: SessionFocus::Conversation,
            side_pane: None,
            scroll: 0,
            input_mode: false,
        };
        self.ensure_agent_connected(agent_id);
    }

    /// Open the slash-commands browser in the right side pane.
    ///
    /// Snapshots the current agent's `available_commands`. If the agent
    /// later updates them via ACP, the user can reopen with `:commands`
    /// to refresh.
    fn open_commands_view(&mut self) {
        let agent_id = match &self.screen {
            Screen::AgentSession { agent_id, .. } => agent_id.clone(),
            _ => {
                self.status_message =
                    Some("No workspace open — :commands needs an agent session".into());
                return;
            }
        };
        let commands = match self.agents.iter().find(|a| a.id == agent_id) {
            Some(a) if !a.available_commands.is_empty() => a.available_commands.clone(),
            Some(_) => {
                self.status_message =
                    Some("Agent has not advertised any slash commands yet".into());
                return;
            }
            None => return,
        };
        if let Screen::AgentSession { side_pane, .. } = &mut self.screen {
            *side_pane = Some(SidePane::Commands {
                commands,
                scroll: 0,
            });
        }
    }

    /// Remove the currently viewed workspace agent and go back to the list.
    fn close_current_workspace(&mut self) {
        let agent_id = match &self.screen {
            Screen::AgentSession { agent_id, .. } => agent_id.clone(),
            _ => {
                self.status_message = Some("No workspace open — use :close from a session".into());
                return;
            }
        };

        self.agents.retain(|a| a.id != agent_id);

        // Drop any container-backed explorer fs so its `docker exec` child is
        // torn down rather than leaked once we leave the session screen.
        if self
            .explorer
            .fs
            .as_ref()
            .map(|f| f.is_remote())
            .unwrap_or(false)
        {
            self.explorer.set_fs(None);
        }

        // Persist removal.
        if let Err(err) = workspace::save(&workspace::from_agents(&self.agents)) {
            self.status_message = Some(format!("Warning: {err}"));
        }

        self.screen = Screen::AgentList { selected: 0 };
    }

    /// Rebuild the container for the currently viewed workspace agent.
    ///
    /// Stops and removes the existing container, regenerates the base layer,
    /// clears session_id (a rebuild invalidates the session), then reconnects.
    fn rebuild_current_container(&mut self) {
        let agent_id = match &self.screen {
            Screen::AgentSession { agent_id, .. } => agent_id.clone(),
            _ => {
                self.status_message =
                    Some("No workspace open — use :rebuild from a session".into());
                return;
            }
        };

        let agent = match self.agents.iter_mut().find(|a| a.id == agent_id) {
            Some(a) => a,
            None => return,
        };

        let workspace = match &agent.workspace_folder {
            Some(ws) => ws.clone(),
            None => {
                self.status_message =
                    Some("Agent has no workspace — :rebuild needs a container agent".into());
                return;
            }
        };

        info!(agent_id = %agent_id, workspace = %workspace.display(), "Rebuilding container");

        // Abort the existing agent task so it doesn't compete with the new one.
        if let Some(handle) = agent.task_handle.take() {
            handle.abort();
        }
        // Drop existing connection.
        agent.prompt_tx = None;
        agent.status = AgentStatus::Stopped;
        agent.session_id = None;
        // The container is about to be removed, so the in-container service
        // (and any ServiceFs bound to it) is no longer valid. Clearing this
        // also makes a stale in-flight `ExplorerFsReady` install fail the
        // container-id check once the new container comes up.
        agent.container = None;
        // The branch came from that container; drop it until the rebuilt
        // container reports a fresh one (no host fallback).
        agent.git_branch = None;
        agent.info("🔄 Rebuilding container...");

        // Downgrade the explorer off the soon-to-be-dead container's service
        // back to the host filesystem (dropping the old `docker exec` child),
        // if this agent's explorer is the one on screen.
        if self.viewed_agent_id().as_ref() == Some(&agent_id)
            && self
                .explorer
                .fs
                .as_ref()
                .map(|f| f.is_remote())
                .unwrap_or(false)
        {
            let local = Some(Arc::new(LocalFs::new(&workspace)) as Arc<dyn WorkspaceFs>);
            self.explorer.set_fs(local);
        }

        // Regenerate base layer with latest mount config.
        if let Err(err) = init::generate_workspace_layer(&workspace, AgentKind::Copilot) {
            warn!(error = %err, "Failed to regenerate workspace layer");
            self.status_message = Some(format!("Layer warning: {err}"));
        }

        // Stop + remove the container asynchronously, then reconnect.
        let tx = self.tx.clone();
        let aid = agent_id;
        tokio::spawn(async move {
            if let Err(err) = container::remove_workspace_container(&workspace).await {
                let _ = tx.send(AppEvent::Session(SessionEvent::Output {
                    agent_id: aid.clone(),
                    line: format!("[warn] Failed to remove container: {err}"),
                }));
            }
            let _ = tx.send(AppEvent::Session(SessionEvent::Output {
                agent_id: aid.clone(),
                line: "Container removed. Reconnecting...".into(),
            }));
            let _ = tx.send(AppEvent::ReconnectAgent { agent_id: aid });
        });

        // Persist the cleared session_id.
        if let Err(err) = workspace::save(&workspace::from_agents(&self.agents)) {
            self.status_message = Some(format!("Warning: {err}"));
        }
    }
}

/// Spawn a tracker task for a streaming text handle (assistant, thought,
/// user). Sends `AppEvent::Repaint` whenever the handle's text or status
/// changes; terminates when the status reaches a terminal state or either
/// watch channel is closed.
fn spawn_text_tracker(
    mut text: tokio::sync::watch::Receiver<String>,
    mut status: tokio::sync::watch::Receiver<MessageStatus>,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                res = text.changed() => {
                    if res.is_err() {
                        let _ = tx.send(AppEvent::Repaint);
                        break;
                    }
                }
                res = status.changed() => {
                    if res.is_err() {
                        let _ = tx.send(AppEvent::Repaint);
                        break;
                    }
                }
            }
            let _ = tx.send(AppEvent::Repaint);
            if status.borrow().is_terminal() {
                break;
            }
        }
    });
}

/// Spawn a tracker task for a tool-call handle. Like `spawn_text_tracker`
/// but watches `title` + `status` instead.
fn spawn_tool_tracker(
    mut title: tokio::sync::watch::Receiver<String>,
    mut status: tokio::sync::watch::Receiver<ToolCallStatusKind>,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                res = title.changed() => {
                    if res.is_err() {
                        let _ = tx.send(AppEvent::Repaint);
                        break;
                    }
                }
                res = status.changed() => {
                    if res.is_err() {
                        let _ = tx.send(AppEvent::Repaint);
                        break;
                    }
                }
            }
            let _ = tx.send(AppEvent::Repaint);
            if status.borrow().is_terminal() {
                break;
            }
        }
    });
}

fn handle_list_action(
    action: Action,
    selected: &mut usize,
    agents: &[Agent],
    should_quit: &mut bool,
) -> Option<Screen> {
    match action {
        Action::Quit => {
            *should_quit = true;
            None
        }
        Action::Down if !agents.is_empty() => {
            *selected = (*selected + 1) % agents.len();
            None
        }
        Action::Up if !agents.is_empty() => {
            *selected = if *selected == 0 {
                agents.len() - 1
            } else {
                *selected - 1
            };
            None
        }
        Action::Activate => agents.get(*selected).map(|agent| Screen::AgentSession {
            agent_id: agent.id.clone(),
            focus: SessionFocus::Conversation,
            side_pane: None,
            scroll: 0,
            input_mode: false,
        }),
        _ => None,
    }
}

fn handle_session_action(
    action: Action,
    agent_id: &AgentId,
    focus: &mut SessionFocus,
    side_pane: &mut Option<SidePane>,
    scroll: &mut usize,
    agents: &[Agent],
    explorer: &mut ExplorerState,
) -> Option<Screen> {
    // Explorer-focused actions: arrows navigate the tree, Enter expands
    // dirs / opens files, DismissPane returns focus to conversation.
    if *focus == SessionFocus::Explorer {
        return handle_explorer_action(action, focus, side_pane, agents, agent_id, explorer);
    }
    match action {
        Action::Back => {
            let idx = agents.iter().position(|a| &a.id == agent_id).unwrap_or(0);
            Some(Screen::AgentList { selected: idx })
        }
        Action::Insert => Some(Screen::AgentSession {
            agent_id: agent_id.clone(),
            focus: SessionFocus::Conversation,
            side_pane: side_pane.clone(),
            scroll: *scroll,
            input_mode: true,
        }),
        Action::DismissPane if side_pane.is_some() => {
            *side_pane = None;
            *focus = SessionFocus::Conversation;
            None
        }
        Action::TogglePane => {
            // Cycle: Conversation -> Explorer (if open) -> SidePane (if open) -> Conversation
            *focus = next_focus(*focus, explorer.open, side_pane.is_some());
            None
        }
        Action::ToggleExplorer => {
            explorer.open = !explorer.open;
            if explorer.open {
                // Refresh happens in the caller because it's an async
                // operation that needs `&mut App` to spawn the task.
                *focus = SessionFocus::Explorer;
            } else if *focus == SessionFocus::Explorer {
                *focus = SessionFocus::Conversation;
            }
            None
        }
        Action::Down => {
            // When the side pane is focused, Down/Up move within it. The
            // search pane has a selectable result list; other panes scroll.
            if *focus == SessionFocus::SidePane
                && let Some(pane) = side_pane.as_mut()
            {
                if let SidePane::Search {
                    matches, selected, ..
                } = pane
                {
                    if !matches.is_empty() {
                        *selected = (*selected + 1).min(matches.len() - 1);
                    }
                } else {
                    let s = pane.scroll_mut();
                    *s = s.saturating_add(1);
                }
                return None;
            }
            *scroll = scroll.saturating_add(1);
            None
        }
        Action::Up => {
            if *focus == SessionFocus::SidePane
                && let Some(pane) = side_pane.as_mut()
            {
                if let SidePane::Search { selected, .. } = pane {
                    *selected = selected.saturating_sub(1);
                } else {
                    let s = pane.scroll_mut();
                    *s = s.saturating_sub(1);
                }
                return None;
            }
            if *scroll == usize::MAX {
                // Currently following the bottom — break out of follow mode
                // by anchoring at whatever line is currently at the top of
                // the viewport, then step up by one.
                let top = agents
                    .iter()
                    .find(|a| &a.id == agent_id)
                    .map(|a| a.last_effective_top.get())
                    .unwrap_or(0);
                *scroll = top.saturating_sub(1);
            } else {
                *scroll = scroll.saturating_sub(1);
            }
            None
        }
        Action::FollowBottom => {
            *scroll = usize::MAX;
            None
        }
        Action::Activate => {
            // Enter on a focused search result opens the file and jumps the
            // preview to the hit's line. The (possibly remote) read is
            // serviced by `App::sync_explorer` via the pending-open fields.
            if *focus == SessionFocus::SidePane
                && let Some(SidePane::Search {
                    matches, selected, ..
                }) = side_pane.as_ref()
                && let Some(hit) = matches.get(*selected)
            {
                explorer.pending_open = Some(PathBuf::from(&hit.path));
                explorer.pending_open_line = Some(hit.line);
            }
            None
        }
        _ => None,
    }
}

/// Tab cycle through visible panes. Skips panes that aren't currently
/// showing so the user never lands on an invisible focus target.
fn next_focus(current: SessionFocus, explorer_open: bool, side_pane_open: bool) -> SessionFocus {
    let order = [
        SessionFocus::Conversation,
        SessionFocus::Explorer,
        SessionFocus::SidePane,
    ];
    let idx = order.iter().position(|f| *f == current).unwrap_or(0);
    for step in 1..=order.len() {
        let candidate = order[(idx + step) % order.len()];
        let visible = match candidate {
            SessionFocus::Conversation => true,
            SessionFocus::Explorer => explorer_open,
            SessionFocus::SidePane => side_pane_open,
        };
        if visible {
            return candidate;
        }
    }
    SessionFocus::Conversation
}

fn handle_explorer_action(
    action: Action,
    focus: &mut SessionFocus,
    side_pane: &mut Option<SidePane>,
    agents: &[Agent],
    agent_id: &AgentId,
    explorer: &mut ExplorerState,
) -> Option<Screen> {
    match action {
        Action::ToggleExplorer => {
            explorer.open = false;
            *focus = SessionFocus::Conversation;
            None
        }
        Action::TogglePane => {
            *focus = next_focus(*focus, explorer.open, side_pane.is_some());
            None
        }
        Action::Back => {
            // Esc unfocuses the explorer without closing the pane, mirroring
            // how Esc dismisses other modal focus states.
            *focus = SessionFocus::Conversation;
            None
        }
        Action::Down => {
            explorer.move_cursor(1);
            None
        }
        Action::Up => {
            explorer.move_cursor(-1);
            None
        }
        Action::Activate => {
            if let Some(entry) = explorer.selected_entry() {
                if entry.is_dir {
                    explorer.toggle_selected_dir();
                } else {
                    // Defer the (possibly remote) read to the app, which
                    // runs it off the UI thread. See `App::sync_explorer`.
                    explorer.pending_open = Some(entry.path);
                }
            }
            // Touch agents/agent_id to keep the signature usable for future
            // per-agent dispatch (e.g. sending the file path to the agent).
            let _ = agents;
            let _ = agent_id;
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change_source::{ChangeEvent, ChangeKind};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn app_with_agents() -> App {
        let agents = vec![
            Agent::new("a1", "First"),
            Agent::new("a2", "Second"),
            Agent::new("a3", "Third"),
        ];
        let (tx, _rx) = mpsc::unbounded_channel();
        App::new(Config::default(), agents, tx)
    }

    fn press(code: KeyCode) -> AppEvent {
        AppEvent::Input(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn down_then_up_moves_selection() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Char('j')));
        match app.screen {
            Screen::AgentList { selected } => assert_eq!(selected, 1),
            _ => panic!("expected AgentList"),
        }
        app.handle(press(KeyCode::Char('k')));
        match app.screen {
            Screen::AgentList { selected } => assert_eq!(selected, 0),
            _ => panic!("expected AgentList"),
        }
    }

    #[test]
    fn down_wraps_around() {
        let mut app = app_with_agents();
        for _ in 0..app.agents.len() {
            app.handle(press(KeyCode::Char('j')));
        }
        match app.screen {
            Screen::AgentList { selected } => assert_eq!(selected, 0),
            _ => panic!("expected AgentList"),
        }
    }

    #[test]
    fn activate_enters_session_for_selected_agent() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Char('j')));
        app.handle(press(KeyCode::Enter));
        match &app.screen {
            Screen::AgentSession {
                agent_id,
                side_pane,
                focus,
                ..
            } => {
                assert_eq!(agent_id, "a2");
                assert!(side_pane.is_none(), "side pane should start hidden");
                assert_eq!(*focus, SessionFocus::Conversation);
            }
            other => panic!("expected AgentSession, got {other:?}"),
        }
    }

    #[test]
    fn back_returns_to_agent_list_with_prior_selection() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Char('j')));
        app.handle(press(KeyCode::Char('j')));
        app.handle(press(KeyCode::Enter));
        app.handle(press(KeyCode::Esc));
        match app.screen {
            Screen::AgentList { selected } => assert_eq!(selected, 2),
            _ => panic!("expected AgentList"),
        }
    }

    #[test]
    fn change_event_outside_session_is_ignored() {
        let mut app = app_with_agents();
        app.handle(AppEvent::Change(ChangeEvent {
            path: PathBuf::from("/nonexistent"),
            kind: ChangeKind::Modified,
        }));
        assert!(matches!(app.screen, Screen::AgentList { .. }));
    }

    #[test]
    fn change_event_in_session_opens_diff_side_pane() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        app.handle(AppEvent::Change(ChangeEvent {
            path: PathBuf::from("/definitely/missing.rs"),
            kind: ChangeKind::Modified,
        }));
        match &app.screen {
            Screen::AgentSession {
                side_pane: Some(SidePane::Diff { path, .. }),
                ..
            } => {
                assert_eq!(path, &PathBuf::from("/definitely/missing.rs"));
            }
            other => panic!("expected Diff side pane, got {other:?}"),
        }
    }

    #[test]
    fn change_event_does_not_clobber_user_opened_file_view() {
        // A FileView (user opened a file from the explorer) must survive a
        // background fs change event — otherwise the preview flickers away
        // and is replaced by an auto-diff.
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        if let Screen::AgentSession { side_pane, .. } = &mut app.screen {
            *side_pane = Some(SidePane::FileView {
                path: PathBuf::from("/opened.rs"),
                content: "fn main() {}\n".into(),
                scroll: 0,
            });
        }
        app.handle(AppEvent::Change(ChangeEvent {
            path: PathBuf::from("/other/changed.rs"),
            kind: ChangeKind::Modified,
        }));
        match &app.screen {
            Screen::AgentSession {
                side_pane: Some(SidePane::FileView { path, .. }),
                ..
            } => assert_eq!(path, &PathBuf::from("/opened.rs")),
            other => panic!("expected FileView to survive, got {other:?}"),
        }
    }

    #[test]
    fn change_event_does_not_clobber_commands_pane() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        if let Screen::AgentSession { side_pane, .. } = &mut app.screen {
            *side_pane = Some(SidePane::Commands {
                commands: vec![],
                scroll: 0,
            });
        }
        app.handle(AppEvent::Change(ChangeEvent {
            path: PathBuf::from("/x.rs"),
            kind: ChangeKind::Modified,
        }));
        assert!(matches!(
            &app.screen,
            Screen::AgentSession {
                side_pane: Some(SidePane::Commands { .. }),
                ..
            }
        ));
    }

    #[test]
    fn change_event_replaces_an_existing_auto_diff() {
        // The auto-diff pane is still allowed to refresh to the latest change.
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        app.handle(AppEvent::Change(ChangeEvent {
            path: PathBuf::from("/first.rs"),
            kind: ChangeKind::Modified,
        }));
        app.handle(AppEvent::Change(ChangeEvent {
            path: PathBuf::from("/second.rs"),
            kind: ChangeKind::Modified,
        }));
        match &app.screen {
            Screen::AgentSession {
                side_pane: Some(SidePane::Diff { path, .. }),
                ..
            } => assert_eq!(path, &PathBuf::from("/second.rs")),
            other => panic!("expected refreshed Diff, got {other:?}"),
        }
    }

    #[test]
    fn down_up_scroll_the_focused_side_pane() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        if let Screen::AgentSession {
            side_pane, focus, ..
        } = &mut app.screen
        {
            *side_pane = Some(SidePane::FileView {
                path: PathBuf::from("/big.rs"),
                content: "a\nb\nc\nd\n".into(),
                scroll: 0,
            });
            *focus = SessionFocus::SidePane;
        }
        app.handle(press(KeyCode::Char('j')));
        app.handle(press(KeyCode::Char('j')));
        match &mut app.screen {
            Screen::AgentSession {
                side_pane: Some(pane),
                ..
            } => assert_eq!(*pane.scroll_mut(), 2),
            _ => panic!("expected side pane"),
        }
        app.handle(press(KeyCode::Char('k')));
        match &mut app.screen {
            Screen::AgentSession {
                side_pane: Some(pane),
                ..
            } => assert_eq!(*pane.scroll_mut(), 1),
            _ => panic!("expected side pane"),
        }
    }

    fn permission_with_options(
        opts: Vec<(&str, &str, &str)>,
    ) -> (
        PendingPermission,
        tokio::sync::oneshot::Receiver<Option<String>>,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let perm = PendingPermission {
            tool_name: "write_file".into(),
            options: opts
                .into_iter()
                .map(|(a, b, c)| (a.into(), b.into(), c.into()))
                .collect(),
            reply: Arc::new(Mutex::new(Some(tx))),
            selected: 0,
        };
        (perm, rx)
    }

    #[test]
    fn permission_enter_sends_highlighted_option() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter)); // enter a session
        let (perm, mut rx) = permission_with_options(vec![
            ("id-allow", "Allow once", "allow once"),
            ("id-reject", "Reject", "reject once"),
        ]);
        app.permission_pending = Some(perm);
        // Move highlight to the second option, then confirm.
        app.handle(press(KeyCode::Down));
        app.handle(press(KeyCode::Enter));
        assert!(app.permission_pending.is_none(), "popup should close");
        assert_eq!(rx.try_recv().unwrap(), Some("id-reject".to_string()));
    }

    #[test]
    fn permission_number_key_picks_option_directly() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        let (perm, mut rx) = permission_with_options(vec![
            ("id-allow", "Allow once", "allow once"),
            ("id-always", "Allow always", "allow always"),
        ]);
        app.permission_pending = Some(perm);
        app.handle(press(KeyCode::Char('2')));
        assert!(app.permission_pending.is_none());
        assert_eq!(rx.try_recv().unwrap(), Some("id-always".to_string()));
    }

    #[test]
    fn permission_esc_rejects_with_no_option() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        let (perm, mut rx) = permission_with_options(vec![("id", "Allow", "allow once")]);
        app.permission_pending = Some(perm);
        app.handle(press(KeyCode::Esc));
        assert!(app.permission_pending.is_none());
        assert_eq!(rx.try_recv().unwrap(), None);
    }

    #[test]
    fn permission_popup_captures_input_no_leak_to_buffer() {
        // While the popup is open, typing must not leak into the message
        // input buffer behind it.
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        app.handle(press(KeyCode::Char('i'))); // enter insert mode
        let (perm, _rx) = permission_with_options(vec![("id", "Allow", "allow once")]);
        app.permission_pending = Some(perm);
        app.handle(press(KeyCode::Char('h')));
        app.handle(press(KeyCode::Char('i')));
        assert!(
            app.input_buffer.is_empty(),
            "keystrokes leaked into input buffer: {:?}",
            app.input_buffer
        );
        assert!(app.permission_pending.is_some(), "popup should stay open");
    }

    #[test]
    fn dismiss_pane_clears_side_pane_and_refocuses_conversation() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        app.handle(AppEvent::Change(ChangeEvent {
            path: PathBuf::from("/x"),
            kind: ChangeKind::Modified,
        }));
        app.handle(press(KeyCode::Tab));
        app.handle(press(KeyCode::Char('d')));
        match &app.screen {
            Screen::AgentSession {
                side_pane, focus, ..
            } => {
                assert!(side_pane.is_none());
                assert_eq!(*focus, SessionFocus::Conversation);
            }
            _ => panic!("expected AgentSession"),
        }
    }

    #[test]
    fn dismiss_with_no_side_pane_is_a_noop() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        app.handle(press(KeyCode::Char('d')));
        match &app.screen {
            Screen::AgentSession {
                side_pane, focus, ..
            } => {
                assert!(side_pane.is_none());
                assert_eq!(*focus, SessionFocus::Conversation);
            }
            _ => panic!("expected AgentSession"),
        }
    }

    #[test]
    fn quit_only_quits_from_agent_list() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        app.handle(press(KeyCode::Char('q')));
        assert!(!app.should_quit, "q in session should not quit the app");
        app.handle(press(KeyCode::Esc));
        app.handle(press(KeyCode::Char('q')));
        assert!(app.should_quit);
    }

    #[test]
    fn agent_output_appends_to_history() {
        let mut app = app_with_agents();
        app.handle(AppEvent::Session(SessionEvent::Output {
            agent_id: "a2".into(),
            line: "hello".into(),
        }));
        let a2 = app.agents.iter().find(|a| a.id == "a2").unwrap();
        assert_eq!(a2.history.len(), 1);
        assert!(matches!(&a2.history[0], HistoryEntry::Info(s) if s == "hello"));
    }

    #[test]
    fn agent_exited_marks_status_stopped() {
        let mut app = app_with_agents();
        app.handle(AppEvent::Session(SessionEvent::Exited {
            agent_id: "a1".into(),
            code: Some(0),
        }));
        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(a1.status, AgentStatus::Stopped);
    }

    #[tokio::test]
    async fn assistant_message_started_appends_handle() {
        use tokio::sync::watch;
        let mut app = app_with_agents();
        let (text_tx, text_rx) = watch::channel(String::new());
        let (status_tx, status_rx) = watch::channel(MessageStatus::Streaming);
        let message = fleet_commander_core::session::AssistantMessage {
            text: text_rx,
            status: status_rx,
        };
        app.handle(AppEvent::Session(SessionEvent::AssistantMessage {
            agent_id: "a1".into(),
            message,
        }));

        let _ = text_tx.send("Hello".to_string());
        let _ = status_tx.send(MessageStatus::Completed);

        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(a1.status, AgentStatus::Running);
        assert_eq!(a1.history.len(), 1);
        match &a1.history[0] {
            HistoryEntry::Assistant(m) => {
                assert_eq!(*m.text.borrow(), "Hello");
                assert_eq!(*m.status.borrow(), MessageStatus::Completed);
            }
            _ => panic!("expected assistant entry"),
        }
    }

    #[test]
    fn session_error_appends_to_history() {
        let mut app = app_with_agents();
        app.handle(AppEvent::Session(SessionEvent::Error {
            agent_id: "a2".into(),
            message: "connection lost".into(),
        }));
        let a2 = app.agents.iter().find(|a| a.id == "a2").unwrap();
        match a2.history.last().unwrap() {
            HistoryEntry::Error(s) => assert!(s.contains("connection lost")),
            other => panic!("expected error entry, got {other:?}"),
        }
        assert_eq!(a2.status, AgentStatus::Error);
    }

    #[test]
    fn insert_action_enters_input_mode() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter)); // enter session
        app.handle(press(KeyCode::Char('i'))); // insert mode
        match &app.screen {
            Screen::AgentSession { input_mode, .. } => assert!(*input_mode),
            _ => panic!("expected AgentSession"),
        }
    }

    #[test]
    fn esc_in_input_mode_cancels_input() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        app.handle(press(KeyCode::Char('i')));
        // Type some text
        app.handle(AppEvent::Input(KeyEvent::new(
            KeyCode::Char('h'),
            KeyModifiers::NONE,
        )));
        assert_eq!(app.input_buffer, "h");
        // Esc cancels
        app.handle(press(KeyCode::Esc));
        match &app.screen {
            Screen::AgentSession { input_mode, .. } => assert!(!*input_mode),
            _ => panic!("expected AgentSession"),
        }
        assert!(app.input_buffer.is_empty());
    }

    #[test]
    fn alt_enter_inserts_newline_without_sending() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        app.handle(press(KeyCode::Char('i')));
        app.handle(AppEvent::Input(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        )));
        app.handle(AppEvent::Input(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::ALT,
        )));
        app.handle(AppEvent::Input(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::NONE,
        )));
        assert_eq!(app.input_buffer, "a\nb");
        match &app.screen {
            Screen::AgentSession { input_mode, .. } => assert!(*input_mode),
            _ => panic!("expected AgentSession"),
        }
    }

    #[test]
    fn shift_enter_inserts_newline_without_sending() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        app.handle(press(KeyCode::Char('i')));
        app.handle(AppEvent::Input(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::SHIFT,
        )));
        assert_eq!(app.input_buffer, "\n");
    }

    #[test]
    fn plain_enter_sends_and_exits_input_mode() {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter));
        app.handle(press(KeyCode::Char('i')));
        app.handle(AppEvent::Input(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )));
        app.handle(press(KeyCode::Enter));
        assert!(app.input_buffer.is_empty());
        match &app.screen {
            Screen::AgentSession { input_mode, .. } => assert!(!*input_mode),
            _ => panic!("expected AgentSession"),
        }
    }

    #[test]
    fn agent_connected_sets_idle_status() {
        let mut app = app_with_agents();
        app.handle(AppEvent::Session(SessionEvent::Connected {
            agent_id: "a1".into(),
            session_id: Some("sess_test".into()),
        }));
        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(a1.status, AgentStatus::Idle);
        match a1.history.last().unwrap() {
            HistoryEntry::Info(s) => assert!(s.contains("connected")),
            other => panic!("expected info entry, got {other:?}"),
        }
    }

    #[test]
    fn agent_exited_clears_prompt_tx() {
        let mut app = app_with_agents();
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        app.agents[0].prompt_tx = Some(tx);
        app.handle(AppEvent::Session(SessionEvent::Exited {
            agent_id: "a1".into(),
            code: Some(0),
        }));
        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(a1.status, AgentStatus::Stopped);
        assert!(a1.prompt_tx.is_none());
    }

    #[tokio::test]
    async fn tool_call_started_appends_handle() {
        use tokio::sync::watch;
        let mut app = app_with_agents();
        let (title_tx, title_rx) = watch::channel("read_file".to_string());
        let (status_tx, status_rx) = watch::channel(ToolCallStatusKind::InProgress);
        let call = fleet_commander_core::session::ToolCall {
            id: "call_1".into(),
            title: title_rx,
            status: status_rx,
        };
        app.handle(AppEvent::Session(SessionEvent::ToolCall {
            agent_id: "a1".into(),
            call,
        }));

        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(a1.history.len(), 1);
        match &a1.history[0] {
            HistoryEntry::Tool(tc) => {
                assert_eq!(tc.id, "call_1");
                assert_eq!(*tc.title.borrow(), "read_file");
                assert_eq!(*tc.status.borrow(), ToolCallStatusKind::InProgress);
            }
            _ => panic!("expected tool entry"),
        }

        // Title rewrites + status flips reflect through the handle without
        // any extra history mutation.
        let _ = title_tx.send("read_file completed".to_string());
        let _ = status_tx.send(ToolCallStatusKind::Completed);

        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(a1.history.len(), 1);
        match &a1.history[0] {
            HistoryEntry::Tool(tc) => {
                assert_eq!(*tc.title.borrow(), "read_file completed");
                assert_eq!(*tc.status.borrow(), ToolCallStatusKind::Completed);
            }
            _ => panic!("expected tool entry"),
        }
    }

    // ─── scrolling ────────────────────────────────────────────────────────

    /// Drive the app into a session screen with a known scroll offset so we
    /// can verify how events / key actions mutate it.
    fn app_in_session(scroll: usize) -> App {
        let mut app = app_with_agents();
        app.screen = Screen::AgentSession {
            agent_id: "a1".into(),
            focus: super::SessionFocus::Conversation,
            side_pane: None,
            scroll,
            input_mode: false,
        };
        app
    }

    fn current_scroll(app: &App) -> usize {
        match &app.screen {
            Screen::AgentSession { scroll, .. } => *scroll,
            _ => panic!("expected AgentSession"),
        }
    }

    #[test]
    fn incoming_event_preserves_manual_scroll() {
        // New "sticky scroll" behaviour: an incoming event must NOT yank the
        // viewport back to the bottom. The user has scrolled to line 5;
        // they stay there until they explicitly press `G`.
        let mut app = app_in_session(5);
        app.handle(AppEvent::Session(SessionEvent::Output {
            agent_id: "a1".into(),
            line: "new line".into(),
        }));
        assert_eq!(
            current_scroll(&app),
            5,
            "manual scroll position must persist across incoming events"
        );
    }

    #[test]
    fn incoming_event_preserves_follow_bottom_sentinel() {
        // Conversely, if the user is already following the bottom
        // (scroll == usize::MAX), the sentinel is preserved and the
        // renderer will naturally show the newest content.
        let mut app = app_in_session(usize::MAX);
        app.handle(AppEvent::Session(SessionEvent::Output {
            agent_id: "a1".into(),
            line: "new line".into(),
        }));
        assert_eq!(current_scroll(&app), usize::MAX);
    }

    #[test]
    fn incoming_event_for_other_agent_does_not_change_scroll() {
        let mut app = app_in_session(5);
        // Viewing a1, event arrives for a2.
        app.handle(AppEvent::Session(SessionEvent::Output {
            agent_id: "a2".into(),
            line: "new line".into(),
        }));
        assert_eq!(
            current_scroll(&app),
            5,
            "scroll for a1 must not move when a2 receives content"
        );
    }

    #[test]
    fn repaint_event_preserves_scroll() {
        // Repaint events exist to wake the event loop when a tracked
        // handle ticks; they must not disturb the user's scroll position.
        let mut app = app_in_session(3);
        app.handle(AppEvent::Repaint);
        assert_eq!(current_scroll(&app), 3);
    }

    #[test]
    fn down_action_increments_scroll() {
        let mut app = app_in_session(0);
        app.handle(press(KeyCode::Char('j')));
        assert_eq!(current_scroll(&app), 1);
        app.handle(press(KeyCode::Char('j')));
        assert_eq!(current_scroll(&app), 2);
    }

    #[test]
    fn up_action_saturates_at_zero() {
        let mut app = app_in_session(1);
        app.handle(press(KeyCode::Char('k')));
        assert_eq!(current_scroll(&app), 0);
        app.handle(press(KeyCode::Char('k')));
        assert_eq!(
            current_scroll(&app),
            0,
            "scrolling up past 0 must saturate, not underflow"
        );
    }

    #[test]
    fn manual_scroll_persists_across_events() {
        // After the sticky-scroll refactor, once the user has scrolled
        // away (`scroll` is finite), no streaming event should move them.
        let mut app = app_in_session(usize::MAX);
        // Seed last_effective_top so the Up handler has a known anchor.
        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        a1.last_effective_top.set(7);
        app.handle(press(KeyCode::Char('k'))); // scroll up
        assert_eq!(
            current_scroll(&app),
            6,
            "Up from follow-bottom must anchor at last_effective_top - 1"
        );
        app.handle(AppEvent::Session(SessionEvent::Output {
            agent_id: "a1".into(),
            line: "interrupt".into(),
        }));
        assert_eq!(
            current_scroll(&app),
            6,
            "incoming event must not disturb manual scroll"
        );
    }

    #[test]
    fn follow_bottom_action_re_engages_follow() {
        // After the user has scrolled away, pressing `G` (Shift-G) re-engages
        // follow-bottom by resetting scroll to the sentinel.
        let mut app = app_in_session(5);
        app.handle(AppEvent::Input(KeyEvent::new(
            KeyCode::Char('G'),
            KeyModifiers::SHIFT,
        )));
        assert_eq!(current_scroll(&app), usize::MAX);
    }

    // ─── file explorer ────────────────────────────────────────────────────

    fn ctrl_e() -> AppEvent {
        AppEvent::Input(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL))
    }

    #[test]
    fn ctrl_e_toggles_the_explorer_pane() {
        let mut app = app_in_session(0);
        assert!(!app.explorer.open);
        app.handle(ctrl_e());
        assert!(app.explorer.open, "Ctrl+E must open the explorer");
        // Focus follows the new pane so arrows immediately navigate it.
        match &app.screen {
            Screen::AgentSession { focus, .. } => {
                assert_eq!(*focus, SessionFocus::Explorer);
            }
            _ => panic!("expected AgentSession"),
        }
        app.handle(ctrl_e());
        assert!(!app.explorer.open, "Ctrl+E must close the explorer");
    }

    #[test]
    fn esc_unfocuses_explorer_without_closing_it() {
        let mut app = app_in_session(0);
        app.handle(ctrl_e());
        app.handle(press(KeyCode::Esc));
        match &app.screen {
            Screen::AgentSession { focus, .. } => {
                assert_eq!(*focus, SessionFocus::Conversation);
            }
            _ => panic!("expected AgentSession"),
        }
        assert!(
            app.explorer.open,
            "Esc from explorer focus must keep the pane open"
        );
    }

    #[test]
    fn dot_toggles_show_ignored_when_explorer_focused() {
        let mut app = app_in_session(0);
        app.handle(ctrl_e());
        assert!(!app.explorer.show_ignored);
        app.handle(press(KeyCode::Char('.')));
        assert!(app.explorer.show_ignored, ". must toggle show_ignored on");
        app.handle(press(KeyCode::Char('.')));
        assert!(!app.explorer.show_ignored);
    }

    // ─── session rehydration ──────────────────────────────────────────────
    //
    // During session/load the agent replays prior turns as a sequence of
    // SessionEvent::UserMessage and SessionEvent::AssistantMessage events
    // (with handles whose status quickly transitions to Completed). The
    // app must:
    //   - append each entry to history in arrival order;
    //   - auto-follow to the bottom after each event so the most recent
    //     turn is the one the user sees.

    fn replayed_assistant(body: &str) -> fleet_commander_core::session::AssistantMessage {
        use tokio::sync::watch;
        let (text_tx, text_rx) = watch::channel(body.to_string());
        let (status_tx, status_rx) =
            watch::channel(fleet_commander_core::session::MessageStatus::Completed);
        // Senders are dropped after the channels are seeded, which is fine
        // for replayed (terminal) entries — the receiver still yields the
        // last value via `borrow()`.
        let _ = (text_tx, status_tx);
        fleet_commander_core::session::AssistantMessage {
            text: text_rx,
            status: status_rx,
        }
    }

    fn replayed_user(body: &str) -> fleet_commander_core::session::UserMessage {
        use tokio::sync::watch;
        let (text_tx, text_rx) = watch::channel(body.to_string());
        let (status_tx, status_rx) =
            watch::channel(fleet_commander_core::session::MessageStatus::Completed);
        let _ = (text_tx, status_tx);
        fleet_commander_core::session::UserMessage {
            text: text_rx,
            status: status_rx,
        }
    }

    #[tokio::test]
    async fn session_rehydration_appends_history_in_order_and_follows_bottom() {
        // Start in follow-bottom mode (the default on session entry) so
        // we can verify the sentinel persists across rehydration.
        let mut app = app_in_session(usize::MAX);

        // Simulate session/load replay: a few prior turns.
        let turns = [
            ("first question", "first answer"),
            ("second question", "second answer"),
            ("third question", "third answer"),
        ];
        for (q, a) in turns.iter() {
            app.handle(AppEvent::Session(SessionEvent::UserMessage {
                agent_id: "a1".into(),
                message: replayed_user(q),
            }));
            app.handle(AppEvent::Session(SessionEvent::AssistantMessage {
                agent_id: "a1".into(),
                message: replayed_assistant(a),
            }));
        }

        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(
            a1.history.len(),
            6,
            "all 6 replayed entries (3 turns) must be in history"
        );

        // Verify order: User, Assistant, User, Assistant, User, Assistant.
        let mut iter = a1.history.iter();
        for (q, a) in turns.iter() {
            match iter.next().unwrap() {
                HistoryEntry::User(u) => assert_eq!(u.text.borrow().as_str(), *q),
                other => panic!("expected User, got {other:?}"),
            }
            match iter.next().unwrap() {
                HistoryEntry::Assistant(m) => assert_eq!(m.text.borrow().as_str(), *a),
                other => panic!("expected Assistant, got {other:?}"),
            }
        }

        // Sticky scroll: rehydration events do not move the scroll.
        // The session starts in follow-bottom mode (usize::MAX) and stays
        // there because no manual scroll was performed; the renderer will
        // naturally show the bottom (i.e. the latest turn) — see the
        // `rehydration_renders_latest_turn_visible` UI test for the
        // visible end-to-end behaviour.
        assert_eq!(
            current_scroll(&app),
            usize::MAX,
            "scroll sentinel must be preserved when no user input intervenes"
        );
    }

    #[tokio::test]
    async fn session_rehydration_for_inactive_agent_does_not_move_scroll() {
        let mut app = app_in_session(7);
        // App is viewing a1; rehydration arrives for a2.
        app.handle(AppEvent::Session(SessionEvent::UserMessage {
            agent_id: "a2".into(),
            message: replayed_user("not me"),
        }));
        app.handle(AppEvent::Session(SessionEvent::AssistantMessage {
            agent_id: "a2".into(),
            message: replayed_assistant("nor me"),
        }));
        assert_eq!(
            current_scroll(&app),
            7,
            "scroll for a1 must not move when a2 rehydrates"
        );
        // But a2's history must have grown.
        let a2 = app.agents.iter().find(|a| a.id == "a2").unwrap();
        assert_eq!(a2.history.len(), 2);
    }

    fn enter_input_mode(app: &mut App) {
        // Activate first agent then enter insert mode.
        app.handle(press(KeyCode::Enter));
        app.handle(press(KeyCode::Char('i')));
    }

    fn seed_commands(app: &mut App, agent_id: &str) {
        if let Some(agent) = app.agents.iter_mut().find(|a| a.id == agent_id) {
            agent.available_commands = vec![
                crate::agent::AvailableCommand {
                    name: "model".into(),
                    description: "Select AI model".into(),
                    hint: Some("model".into()),
                },
                crate::agent::AvailableCommand {
                    name: "memory".into(),
                    description: "Show memory status".into(),
                    hint: None,
                },
                crate::agent::AvailableCommand {
                    name: "plan".into(),
                    description: "Create a plan".into(),
                    hint: None,
                },
            ];
        }
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle(AppEvent::Input(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        }
    }

    #[test]
    fn tab_completes_selected_slash_command_with_trailing_space() {
        let mut app = app_with_agents();
        seed_commands(&mut app, "a1");
        enter_input_mode(&mut app);
        type_str(&mut app, "/me");
        // Selection defaults to 0 → "memory" (only match for "me").
        app.handle(press(KeyCode::Tab));
        assert_eq!(app.input_buffer, "/memory ");
        // After completion, selection should reset.
        assert_eq!(app.slash_selected, 0);
    }

    #[test]
    fn down_and_up_navigate_slash_popover_with_wrap() {
        let mut app = app_with_agents();
        seed_commands(&mut app, "a1");
        enter_input_mode(&mut app);
        // Type just "/" → all three commands match.
        type_str(&mut app, "/");
        assert_eq!(app.slash_selected, 0);
        app.handle(press(KeyCode::Down));
        assert_eq!(app.slash_selected, 1);
        app.handle(press(KeyCode::Down));
        assert_eq!(app.slash_selected, 2);
        // Wrap-around.
        app.handle(press(KeyCode::Down));
        assert_eq!(app.slash_selected, 0);
        // Up from 0 wraps to last.
        app.handle(press(KeyCode::Up));
        assert_eq!(app.slash_selected, 2);
    }

    #[test]
    fn typing_after_completion_does_not_reopen_popover_in_argument_mode() {
        let mut app = app_with_agents();
        seed_commands(&mut app, "a1");
        enter_input_mode(&mut app);
        type_str(&mut app, "/mo");
        // Popover is open (matches: "model").
        assert!(app.slash_matches_for("a1").is_some());
        // After Tab, buffer is "/model " — popover closed because of the
        // trailing space (argument mode).
        app.handle(press(KeyCode::Tab));
        assert_eq!(app.input_buffer, "/model ");
        assert!(app.slash_matches_for("a1").is_none());
        // Typing into the argument doesn't reopen.
        type_str(&mut app, "gpt-5");
        assert!(app.slash_matches_for("a1").is_none());
    }

    /// A minimal remote [`WorkspaceFs`] double for the explorer-upgrade tests.
    #[derive(Debug)]
    struct FakeRemoteFs {
        root: PathBuf,
    }

    impl WorkspaceFs for FakeRemoteFs {
        fn root_display(&self) -> &std::path::Path {
            &self.root
        }
        fn list_dir(
            &self,
            _rel: &std::path::Path,
        ) -> std::io::Result<Vec<fleet_commander_core::workspace_fs::DirEntry>> {
            Ok(Vec::new())
        }
        fn read_file(&self, _rel: &std::path::Path) -> std::io::Result<Vec<u8>> {
            Ok(Vec::new())
        }
        fn git_branch(&self) -> Option<String> {
            None
        }
        fn git_status(
            &self,
            _include_ignored: bool,
        ) -> Result<
            std::collections::HashMap<PathBuf, fleet_commander_core::git::StatusKind>,
            fleet_commander_core::git::StatusError,
        > {
            Ok(std::collections::HashMap::new())
        }
        fn is_remote(&self) -> bool {
            true
        }
    }

    fn app_with_container_agent(ws: &str) -> App {
        let agent = Agent::new("a1", "First").with_workspace(PathBuf::from(ws));
        let (tx, _rx) = mpsc::unbounded_channel();
        App::new(Config::default(), vec![agent], tx)
    }

    #[tokio::test]
    async fn container_ready_stores_info_on_agent() {
        let mut app = app_with_container_agent("/ws/repo");
        app.handle(AppEvent::Session(SessionEvent::ContainerReady {
            agent_id: "a1".into(),
            container_id: "cid".into(),
            remote_user: "vscode".into(),
            remote_workspace_folder: "/workspaces/repo".into(),
        }));
        let agent = app.agents.iter().find(|a| a.id == "a1").unwrap();
        let info = agent.container.as_ref().expect("container info stored");
        assert_eq!(info.container_id, "cid");
        assert_eq!(info.remote_user, "vscode");
        assert_eq!(info.remote_workspace_folder, "/workspaces/repo");
    }

    #[tokio::test]
    async fn explorer_diff_ready_opens_diff_pane() {
        let mut app = app_with_container_agent("/ws/repo");
        app.handle(press(KeyCode::Enter));
        app.handle(AppEvent::ExplorerDiffReady {
            agent_id: "a1".into(),
            root: PathBuf::from("/ws/repo"),
            full_path: PathBuf::from("/ws/repo/a.txt"),
            result: Ok("@@ -1 +1 @@\n-a\n+b\n".into()),
        });
        match &app.screen {
            Screen::AgentSession {
                side_pane: Some(SidePane::Diff { content, path, .. }),
                ..
            } => {
                assert!(content.contains("+b"), "{content}");
                assert_eq!(path, &PathBuf::from("/ws/repo/a.txt"));
            }
            other => panic!("expected Diff pane, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn explorer_diff_ready_empty_shows_no_changes() {
        let mut app = app_with_container_agent("/ws/repo");
        app.handle(press(KeyCode::Enter));
        app.handle(AppEvent::ExplorerDiffReady {
            agent_id: "a1".into(),
            root: PathBuf::from("/ws/repo"),
            full_path: PathBuf::from("/ws/repo/clean.txt"),
            result: Ok(String::new()),
        });
        match &app.screen {
            Screen::AgentSession {
                side_pane: Some(SidePane::Diff { content, .. }),
                ..
            } => assert_eq!(content, "No changes."),
            other => panic!("expected Diff pane, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn explorer_diff_ready_ignored_for_different_root() {
        let mut app = app_with_container_agent("/ws/repo");
        app.handle(press(KeyCode::Enter));
        app.handle(AppEvent::ExplorerDiffReady {
            agent_id: "a1".into(),
            root: PathBuf::from("/some/other/root"),
            full_path: PathBuf::from("/some/other/root/a.txt"),
            result: Ok("diff".into()),
        });
        match &app.screen {
            Screen::AgentSession { side_pane, .. } => assert!(side_pane.is_none()),
            other => panic!("expected AgentSession, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn explorer_fs_ready_upgrades_viewed_agent_to_remote() {
        let mut app = app_with_container_agent("/ws/repo");
        // Enter the session so the explorer is on screen and points at the
        // host LocalFs for /ws/repo.
        app.handle(press(KeyCode::Enter));
        app.explorer.open = true;
        assert!(!app.explorer.fs.as_ref().unwrap().is_remote());

        // The container comes up; the agent records its id.
        app.handle(AppEvent::Session(SessionEvent::ContainerReady {
            agent_id: "a1".into(),
            container_id: "cid".into(),
            remote_user: "vscode".into(),
            remote_workspace_folder: "/workspaces/repo".into(),
        }));

        // The background connect lands with a remote fs for the same root and
        // the same container.
        app.handle(AppEvent::ExplorerFsReady {
            agent_id: "a1".into(),
            container_id: "cid".into(),
            fs: Arc::new(FakeRemoteFs {
                root: PathBuf::from("/ws/repo"),
            }) as Arc<dyn WorkspaceFs>,
        });
        assert!(
            app.explorer.fs.as_ref().unwrap().is_remote(),
            "explorer should now be backed by the container service"
        );
    }

    #[test]
    fn explorer_fs_ready_ignored_for_different_root() {
        let mut app = app_with_container_agent("/ws/repo");
        app.handle(press(KeyCode::Enter));
        app.explorer.open = true;

        // A stale upgrade for a different workspace must not clobber the view.
        app.handle(AppEvent::ExplorerFsReady {
            agent_id: "a1".into(),
            container_id: "cid".into(),
            fs: Arc::new(FakeRemoteFs {
                root: PathBuf::from("/ws/other"),
            }) as Arc<dyn WorkspaceFs>,
        });
        assert!(!app.explorer.fs.as_ref().unwrap().is_remote());
    }

    #[tokio::test]
    async fn explorer_fs_ready_rejected_for_stale_container() {
        let mut app = app_with_container_agent("/ws/repo");
        app.handle(press(KeyCode::Enter));
        app.explorer.open = true;

        // Agent is currently backed by container "new".
        app.handle(AppEvent::Session(SessionEvent::ContainerReady {
            agent_id: "a1".into(),
            container_id: "new".into(),
            remote_user: "vscode".into(),
            remote_workspace_folder: "/workspaces/repo".into(),
        }));

        // A handshake that started against the OLD container finally lands.
        // It must be dropped, not installed, to avoid binding the explorer to
        // a dead container.
        app.handle(AppEvent::ExplorerFsReady {
            agent_id: "a1".into(),
            container_id: "old".into(),
            fs: Arc::new(FakeRemoteFs {
                root: PathBuf::from("/ws/repo"),
            }) as Arc<dyn WorkspaceFs>,
        });
        assert!(
            !app.explorer.fs.as_ref().unwrap().is_remote(),
            "a fs bound to a stale container must be rejected"
        );
    }

    #[tokio::test]
    async fn agent_branch_ready_applies_for_matching_container() {
        let mut app = app_with_container_agent("/ws/repo");
        app.handle(AppEvent::Session(SessionEvent::ContainerReady {
            agent_id: "a1".into(),
            container_id: "cid".into(),
            remote_user: "vscode".into(),
            remote_workspace_folder: "/workspaces/repo".into(),
        }));
        app.handle(AppEvent::AgentBranchReady {
            agent_id: "a1".into(),
            container_id: "cid".into(),
            branch: Some("feat/x".into()),
        });
        let agent = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(agent.git_branch.as_deref(), Some("feat/x"));
    }

    #[tokio::test]
    async fn agent_branch_ready_rejected_for_stale_container() {
        let mut app = app_with_container_agent("/ws/repo");
        app.handle(AppEvent::Session(SessionEvent::ContainerReady {
            agent_id: "a1".into(),
            container_id: "new".into(),
            remote_user: "vscode".into(),
            remote_workspace_folder: "/workspaces/repo".into(),
        }));
        // A branch read from the OLD container must not be applied.
        app.handle(AppEvent::AgentBranchReady {
            agent_id: "a1".into(),
            container_id: "old".into(),
            branch: Some("stale".into()),
        });
        let agent = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(agent.git_branch, None);
    }

    #[tokio::test]
    async fn rebuild_downgrades_explorer_off_remote_fs() {
        let mut app = app_with_container_agent("/ws/repo");
        app.handle(press(KeyCode::Enter));
        app.explorer.open = true;

        // Bring the container up and install a remote fs for it.
        app.handle(AppEvent::Session(SessionEvent::ContainerReady {
            agent_id: "a1".into(),
            container_id: "cid".into(),
            remote_user: "vscode".into(),
            remote_workspace_folder: "/workspaces/repo".into(),
        }));
        app.handle(AppEvent::ExplorerFsReady {
            agent_id: "a1".into(),
            container_id: "cid".into(),
            fs: Arc::new(FakeRemoteFs {
                root: PathBuf::from("/ws/repo"),
            }) as Arc<dyn WorkspaceFs>,
        });
        assert!(app.explorer.fs.as_ref().unwrap().is_remote());

        // Rebuilding clears the container and drops the remote fs back to the
        // host filesystem.
        app.rebuild_current_container();
        assert!(
            !app.explorer.fs.as_ref().unwrap().is_remote(),
            "rebuild must downgrade the explorer off the dead container"
        );
        let agent = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert!(
            agent.container.is_none(),
            "rebuild must clear the agent's container handle"
        );
    }

    fn hit(path: &str, line: u64, text: &str) -> fleet_commander_core::fleet_protocol::SearchMatch {
        fleet_commander_core::fleet_protocol::SearchMatch {
            path: path.into(),
            line,
            column: 1,
            text: text.into(),
        }
    }

    /// Enter agent a1's session with an open, running search pane focused.
    fn app_with_search_pane(search_id: u64) -> App {
        let mut app = app_with_agents();
        app.handle(press(KeyCode::Enter)); // enter a1's session
        if let Screen::AgentSession {
            side_pane, focus, ..
        } = &mut app.screen
        {
            *side_pane = Some(SidePane::Search {
                query: "needle".into(),
                search_id,
                matches: Vec::new(),
                selected: 0,
                scroll: 0,
                running: true,
                summary: None,
            });
            *focus = SessionFocus::SidePane;
        }
        app
    }

    #[test]
    fn search_results_append_to_matching_pane() {
        let mut app = app_with_search_pane(7);
        app.handle(AppEvent::SearchResults {
            agent_id: "a1".into(),
            search_id: 7,
            matches: vec![hit("src/a.rs", 1, "a"), hit("src/b.rs", 2, "b")],
        });
        app.handle(AppEvent::SearchResults {
            agent_id: "a1".into(),
            search_id: 7,
            matches: vec![hit("src/c.rs", 3, "c")],
        });
        match &app.screen {
            Screen::AgentSession {
                side_pane: Some(SidePane::Search { matches, .. }),
                ..
            } => assert_eq!(matches.len(), 3),
            other => panic!("expected search pane, got {other:?}"),
        }
    }

    #[test]
    fn search_results_for_stale_id_are_dropped() {
        let mut app = app_with_search_pane(7);
        app.handle(AppEvent::SearchResults {
            agent_id: "a1".into(),
            search_id: 99, // does not match the pane's id
            matches: vec![hit("src/a.rs", 1, "a")],
        });
        match &app.screen {
            Screen::AgentSession {
                side_pane: Some(SidePane::Search { matches, .. }),
                ..
            } => assert!(matches.is_empty()),
            other => panic!("expected search pane, got {other:?}"),
        }
    }

    #[test]
    fn search_done_clears_running_and_records_summary() {
        let mut app = app_with_search_pane(7);
        app.handle(AppEvent::SearchDone {
            agent_id: "a1".into(),
            search_id: 7,
            summary: fleet_commander_core::fleet_protocol::SearchSummary {
                count: 4,
                truncated: true,
                cancelled: false,
            },
        });
        match &app.screen {
            Screen::AgentSession {
                side_pane:
                    Some(SidePane::Search {
                        running, summary, ..
                    }),
                ..
            } => {
                assert!(!running);
                assert_eq!(summary.as_ref().map(|s| s.count), Some(4));
            }
            other => panic!("expected search pane, got {other:?}"),
        }
    }

    #[test]
    fn down_moves_search_selection_not_scroll() {
        let mut app = app_with_search_pane(7);
        app.handle(AppEvent::SearchResults {
            agent_id: "a1".into(),
            search_id: 7,
            matches: vec![hit("a", 1, "a"), hit("b", 2, "b"), hit("c", 3, "c")],
        });
        app.handle(press(KeyCode::Char('j')));
        app.handle(press(KeyCode::Char('j')));
        // Extra Down must clamp at the last row, not overflow.
        app.handle(press(KeyCode::Char('j')));
        match &app.screen {
            Screen::AgentSession {
                side_pane: Some(SidePane::Search { selected, .. }),
                ..
            } => assert_eq!(*selected, 2),
            other => panic!("expected search pane, got {other:?}"),
        }
    }

    #[test]
    fn activate_search_hit_sets_pending_open_with_line() {
        let mut app = app_with_search_pane(7);
        app.handle(AppEvent::SearchResults {
            agent_id: "a1".into(),
            search_id: 7,
            matches: vec![hit("src/a.rs", 10, "x"), hit("src/b.rs", 42, "y")],
        });
        app.handle(press(KeyCode::Char('j'))); // select the second hit
        app.handle(press(KeyCode::Enter));
        assert_eq!(app.explorer.pending_open, Some(PathBuf::from("src/b.rs")));
        assert_eq!(app.explorer.pending_open_line, Some(42));
    }
}
