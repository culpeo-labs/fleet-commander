//! Pure action-dispatch helpers and streaming-handle trackers for
//! [`super::App`]. These are free functions (no `&App` receiver) so they can
//! be unit-reasoned in isolation and reused across screens.

use std::path::PathBuf;

use tokio::sync::mpsc;

use fleet_commander_core::session::{MessageStatus, ToolCallStatusKind};

use crate::agent::{Agent, AgentId};
use crate::config::Action;
use crate::event::AppEvent;
use crate::explorer::ExplorerState;

use super::{Screen, SessionFocus, SidePane};

/// Spawn a tracker task for a streaming text handle (assistant, thought,
/// user). Sends `AppEvent::Repaint` whenever the handle's text or status
/// changes; terminates when the status reaches a terminal state or either
/// watch channel is closed.
pub(crate) fn spawn_text_tracker(
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
pub(crate) fn spawn_tool_tracker(
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

pub(crate) fn handle_list_action(
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

pub(crate) fn handle_session_action(
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
pub(crate) fn next_focus(
    current: SessionFocus,
    explorer_open: bool,
    side_pane_open: bool,
) -> SessionFocus {
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

pub(crate) fn handle_explorer_action(
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
