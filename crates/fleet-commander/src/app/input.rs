//! Keyboard input and change-event handling for [`super::App`]: the modal
//! key dispatcher, permission-popup resolution, slash-command matching, and
//! filesystem change reactions.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use fleet_commander_core::agent_runtime;

use crate::change_source::ChangeEvent;
use crate::completion::split_command_and_path;
use crate::config::Action;

use super::{App, Screen, SessionFocus, SidePane};
use super::{handle_list_action, handle_session_action};

impl App {
    /// Answer the pending permission request and tear down the popup.
    /// `choice` is `Some(index)` of the option to allow/select, or `None`
    /// to reject (no option chosen). Sends the option id back through the
    /// oneshot the runtime is awaiting.
    pub(super) fn resolve_permission(&mut self, choice: Option<usize>) {
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
    pub(super) fn handle_key(&mut self, key: KeyEvent) {
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
    pub(super) fn handle_change(&mut self, change: ChangeEvent) {
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
}
