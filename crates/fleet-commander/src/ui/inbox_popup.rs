//! Cross-workspace message approval modal (Feature 2c).
//!
//! When another workspace's agent calls `send_to_workspace`, the message is
//! queued in [`crate::app::Inbox`] and surfaced here for the user's approval
//! before it is injected into the target agent. Like the permission popup,
//! this modal owns keyboard input while open (see the inbox branch in
//! `App::handle_key`) so keystrokes can't leak into the input box behind it.
//! This module is a pure renderer; the queue lives on [`crate::app::App`].

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use crate::app::InboxMessage;

/// Render the inbox approval modal centered over `area` for the front pending
/// message. `remaining` is the total number of queued messages (including this
/// one) so the user knows how many more follow.
pub fn render(frame: &mut Frame<'_>, area: Rect, msg: &InboxMessage, remaining: usize) {
    let popup = centered_rect(area);
    frame.render_widget(Clear, popup);

    let title = if remaining > 1 {
        format!(" 📨 Incoming message (1 of {remaining}) ")
    } else {
        " 📨 Incoming message ".to_string()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let hint_h = 1u16;
    let header_h = 2u16;
    let body_h = inner.height.saturating_sub(header_h + hint_h);

    let header_area = Rect {
        height: header_h,
        ..inner
    };
    let body_area = Rect {
        y: inner.y + header_h,
        height: body_h,
        ..inner
    };
    let hint_area = Rect {
        y: inner.y + header_h + body_h,
        height: hint_h,
        ..inner
    };

    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            msg.sender_name.clone(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" → "),
        Span::styled(
            msg.target_name.clone(),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
    ]))
    .wrap(Wrap { trim: false });
    frame.render_widget(header, header_area);

    let body = Paragraph::new(msg.body.clone())
        .style(Style::default().fg(Color::Gray))
        .wrap(Wrap { trim: false });
    frame.render_widget(body, body_area);

    let hint = Paragraph::new(Span::styled(
        "Enter/y approve & deliver · Esc/n reject",
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(hint, hint_area);
}

/// A centered popup rect, 60% wide and up to a third of the screen tall.
fn centered_rect(area: Rect) -> Rect {
    let width = (area.width as f32 * 0.6) as u16;
    let width = width.clamp(20.min(area.width), area.width);
    let height = (area.height / 3).clamp(6.min(area.height), area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use crate::app::InboxMessage;
    use crate::ui::test_support::{render_to_string, test_app};

    fn msg() -> InboxMessage {
        InboxMessage {
            sender_id: "copilot-feature".into(),
            sender_name: "feature".into(),
            target_id: "a2".into(),
            target_name: "Second".into(),
            body: "please update the changelog".into(),
        }
    }

    #[test]
    fn renders_sender_target_and_body() {
        let mut app = test_app();
        app.inbox.push(msg());
        let text = render_to_string(&app, 80, 24);
        assert!(text.contains("feature"), "missing sender:\n{text}");
        assert!(text.contains("Second"), "missing target:\n{text}");
        assert!(
            text.contains("please update the changelog"),
            "missing body:\n{text}"
        );
        assert!(text.contains("approve"), "missing hint:\n{text}");
    }

    #[test]
    fn shows_queue_count_when_multiple_pending() {
        let mut app = test_app();
        app.inbox.push(msg());
        app.inbox.push(msg());
        app.inbox.push(msg());
        let text = render_to_string(&app, 80, 24);
        assert!(text.contains("1 of 3"), "missing queue count:\n{text}");
    }
}
