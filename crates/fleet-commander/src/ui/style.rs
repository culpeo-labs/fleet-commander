//! Shared colour/style helpers.

use ratatui::style::{Color, Style};

use crate::agent::AgentStatus;

/// Border colour for a focused vs unfocused pane.
pub fn border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

/// Colour used to render an agent's `[status]` chip.
pub fn status_color(status: &AgentStatus) -> Color {
    match status {
        AgentStatus::Idle => Color::Gray,
        AgentStatus::Running => Color::Green,
        AgentStatus::Stopped => Color::DarkGray,
        AgentStatus::Error => Color::Red,
    }
}
