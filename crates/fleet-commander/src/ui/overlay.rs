//! Bottom-row overlays drawn on top of every screen: command bar,
//! transient status message, and the pending-permission prompt.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Span,
    widgets::Paragraph,
};

use crate::app::App;

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let bar_area = Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(1),
        width: area.width,
        height: 1,
    };

    if app.command_mode {
        render_command_bar(frame, bar_area, &app.command_buffer);
    } else if let Some(msg) = &app.status_message {
        render_status_bar(frame, bar_area, msg);
    } else if let Some(perm) = &app.permission_pending {
        render_permission_bar(frame, bar_area, &perm.tool_name);
    }
}

fn render_command_bar(frame: &mut Frame<'_>, area: Rect, buffer: &str) {
    let text = format!(":{buffer}");
    let bar = Paragraph::new(Span::styled(
        text,
        Style::default()
            .fg(Color::White)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    ))
    .style(Style::default().bg(Color::DarkGray));
    frame.render_widget(bar, area);
}

fn render_status_bar(frame: &mut Frame<'_>, area: Rect, message: &str) {
    let bar = Paragraph::new(Span::styled(
        message,
        Style::default().fg(Color::Yellow).bg(Color::DarkGray),
    ))
    .style(Style::default().bg(Color::DarkGray));
    frame.render_widget(bar, area);
}

fn render_permission_bar(frame: &mut Frame<'_>, area: Rect, tool_name: &str) {
    let text = format!("🔐 Allow {tool_name}? (y)es / (n)o");
    let bar = Paragraph::new(Span::styled(
        text,
        Style::default()
            .fg(Color::White)
            .bg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
    ))
    .style(Style::default().bg(Color::Magenta));
    frame.render_widget(bar, area);
}
