//! Application state machine.
//!
//! The UI is structured as two screens:
//!
//!   * `Screen::AgentList` — top-level overview of all agents.
//!   * `Screen::AgentSession` — immersive view of a single agent. The
//!     conversation/history is the main pane; a `SidePane` (Diff or Editor)
//!     can appear on the left, but only when invoked by a change event or
//!     by the user.
//!
//! Input handling is dispatched per-screen so a keypress can never silently
//! mutate a hidden buffer.

use crossterm::event::{KeyCode, KeyEvent};
use std::path::PathBuf;
use tokio::sync::mpsc;

use crate::agent::{Agent, AgentId, AgentStatus};
use crate::agent_runtime;
use crate::change_source::ChangeEvent;
use crate::config::{Action, Config};
use crate::event::AppEvent;

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
}

#[derive(Debug, Clone)]
pub enum SidePane {
    Diff {
        path: PathBuf,
        content: String,
    },
    #[allow(dead_code)] // Editor variant is a stub for now.
    Editor {
        path: PathBuf,
        buffer: String,
    },
}

impl SidePane {
    #[allow(dead_code)] // exposed for future actions on the side pane.
    pub fn path(&self) -> &PathBuf {
        match self {
            SidePane::Diff { path, .. } | SidePane::Editor { path, .. } => path,
        }
    }
}

pub struct App {
    pub config: Config,
    pub agents: Vec<Agent>,
    pub screen: Screen,
    pub should_quit: bool,
    /// Text the user is composing in insert mode.
    pub input_buffer: String,
    /// Channel for sending events (used to dispatch messages to agents).
    pub tx: mpsc::UnboundedSender<AppEvent>,
}

impl App {
    pub fn new(config: Config, agents: Vec<Agent>, tx: mpsc::UnboundedSender<AppEvent>) -> Self {
        Self {
            config,
            agents,
            screen: Screen::AgentList { selected: 0 },
            should_quit: false,
            input_buffer: String::new(),
            tx,
        }
    }

    pub fn handle(&mut self, event: AppEvent) {
        match event {
            AppEvent::Input(key) => self.handle_key(key),
            AppEvent::Change(change) => self.handle_change(change),
            AppEvent::AgentOutput { agent_id, line } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.history.push(line);
                }
            }
            AppEvent::AgentExited { agent_id, .. } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.status = AgentStatus::Stopped;
                }
            }
            AppEvent::McpShowDiff {
                agent_id,
                path,
                content,
            } => self.handle_mcp_side_pane(agent_id, SidePane::Diff { path, content }),
            AppEvent::McpShowFile {
                agent_id,
                path,
                content,
            } => self.handle_mcp_side_pane(agent_id, SidePane::Diff { path, content }),
            AppEvent::McpNotify { agent_id, message } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.history.push(message);
                }
            }
            AppEvent::AssistantDelta { agent_id, text } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.status = AgentStatus::Running;
                    agent.pending_response.push_str(&text);
                }
            }
            AppEvent::AssistantDone { agent_id } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    if !agent.pending_response.is_empty() {
                        let response = std::mem::take(&mut agent.pending_response);
                        agent.history.push(response);
                    }
                    agent.status = AgentStatus::Idle;
                }
            }
            AppEvent::SessionError { agent_id, message } => {
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    agent.history.push(format!("[error] {message}"));
                    agent.status = AgentStatus::Error;
                }
            }
            AppEvent::CopilotClientError { message } => {
                // Show error on all agents.
                for agent in &mut self.agents {
                    agent.history.push(format!("[copilot] {message}"));
                }
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
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
                }
                KeyCode::Enter => {
                    let message = std::mem::take(&mut self.input_buffer);
                    if !message.is_empty() {
                        if let Some(agent) =
                            self.agents.iter().find(|a| a.id == *agent_id)
                        {
                            agent_runtime::send_message(agent, message, self.tx.clone());
                        }
                    }
                    if let Screen::AgentSession { input_mode, .. } = &mut self.screen {
                        *input_mode = false;
                    }
                }
                KeyCode::Backspace => {
                    self.input_buffer.pop();
                }
                KeyCode::Char(c) => {
                    self.input_buffer.push(c);
                }
                _ => {}
            }
            return;
        }

        let Some(action) = self.config.bindings.action_for(&key) else {
            return;
        };

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
            } => handle_session_action(action, agent_id, focus, side_pane, scroll, &self.agents),
        };
        if let Some(next) = next {
            self.screen = next;
        }
    }

    fn handle_change(&mut self, change: ChangeEvent) {
        if let Screen::AgentSession { side_pane, .. } = &mut self.screen {
            let content = std::fs::read_to_string(&change.path).unwrap_or_default();
            *side_pane = Some(SidePane::Diff {
                path: change.path,
                content,
            });
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
    }
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
) -> Option<Screen> {
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
        Action::TogglePane if side_pane.is_some() => {
            *focus = match *focus {
                SessionFocus::Conversation => SessionFocus::SidePane,
                SessionFocus::SidePane => SessionFocus::Conversation,
            };
            None
        }
        Action::Down => {
            *scroll = scroll.saturating_add(1);
            None
        }
        Action::Up => {
            *scroll = scroll.saturating_sub(1);
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
        app.handle(AppEvent::AgentOutput {
            agent_id: "a2".into(),
            line: "hello".into(),
        });
        let a2 = app.agents.iter().find(|a| a.id == "a2").unwrap();
        assert_eq!(a2.history, vec!["hello".to_string()]);
    }

    #[test]
    fn agent_exited_marks_status_stopped() {
        let mut app = app_with_agents();
        app.handle(AppEvent::AgentExited {
            agent_id: "a1".into(),
            code: Some(0),
        });
        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(a1.status, AgentStatus::Stopped);
    }

    #[test]
    fn assistant_delta_accumulates_pending_response() {
        let mut app = app_with_agents();
        app.handle(AppEvent::AssistantDelta {
            agent_id: "a1".into(),
            text: "Hello".into(),
        });
        app.handle(AppEvent::AssistantDelta {
            agent_id: "a1".into(),
            text: " world".into(),
        });
        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(a1.pending_response, "Hello world");
        assert_eq!(a1.status, AgentStatus::Running);
    }

    #[test]
    fn assistant_done_flushes_pending_to_history() {
        let mut app = app_with_agents();
        app.handle(AppEvent::AssistantDelta {
            agent_id: "a1".into(),
            text: "response text".into(),
        });
        app.handle(AppEvent::AssistantDone {
            agent_id: "a1".into(),
        });
        let a1 = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert!(a1.pending_response.is_empty());
        assert_eq!(a1.history.last().unwrap(), "response text");
        assert_eq!(a1.status, AgentStatus::Idle);
    }

    #[test]
    fn session_error_appends_to_history() {
        let mut app = app_with_agents();
        app.handle(AppEvent::SessionError {
            agent_id: "a2".into(),
            message: "connection lost".into(),
        });
        let a2 = app.agents.iter().find(|a| a.id == "a2").unwrap();
        assert!(a2.history.last().unwrap().contains("connection lost"));
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
}
