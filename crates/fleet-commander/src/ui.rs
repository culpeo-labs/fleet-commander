//! TUI rendering layer.
//!
//! The render code is split per-component — each child module owns
//! exactly one widget or screen and its own substring-style tests, so
//! we can iterate on (and reason about) one piece at a time.
//!
//! The entry point [`render`] dispatches on `app.screen` and finally
//! draws the bottom-row overlays.

mod agent_list;
mod agent_session;
mod conversation;
mod explorer;
mod input_box;
mod keys_footer;
mod overlay;
mod permission_popup;
mod session_header;
mod side_pane;
pub(crate) mod slash_popover;
mod style;
mod syntax;

#[cfg(test)]
mod test_support;

use ratatui::Frame;

use crate::app::{App, Screen};

pub fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    match &app.screen {
        Screen::AgentList { selected } => {
            agent_list::render(frame, area, &app.agents, *selected);
        }
        Screen::AgentSession {
            agent_id,
            focus,
            side_pane,
            scroll,
            input_mode,
        } => agent_session::render(
            frame,
            area,
            app,
            agent_id,
            *focus,
            side_pane.as_ref(),
            *scroll,
            *input_mode,
            &app.input_buffer,
        ),
    }

    // Bottom-row overlays sit on top of every screen.
    overlay::render(frame, area, app);

    // The permission modal sits above everything else (including the
    // input box) and owns input while it's open.
    if let Some(perm) = &app.permission_pending {
        permission_popup::render(frame, area, perm);
    }
}
