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

#[cfg(test)]
mod tests {
    use crate::app::PendingPermission;
    use crate::ui::test_support::{render_to_string, test_app};
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot;

    fn pending_perm(tool: &str) -> PendingPermission {
        let (tx, _rx) = oneshot::channel::<Option<String>>();
        PendingPermission {
            tool_name: tool.into(),
            options: Vec::new(),
            reply: Arc::new(Mutex::new(Some(tx))),
        }
    }

    fn bottom_row(text: &str) -> String {
        text.lines()
            .rfind(|l| !l.is_empty())
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn command_mode_renders_prompt_on_bottom_row() {
        let mut app = test_app();
        app.command_mode = true;
        app.command_buffer = "open foo".into();
        let text = render_to_string(&app, 40, 8);
        assert!(
            bottom_row(&text).contains(":open foo"),
            "command prompt missing from bottom row:\n{text}"
        );
    }

    #[test]
    fn status_message_renders_on_bottom_row_when_not_in_command_mode() {
        let mut app = test_app();
        app.status_message = Some("did the thing".into());
        let text = render_to_string(&app, 40, 8);
        assert!(
            bottom_row(&text).contains("did the thing"),
            "status text missing from bottom row:\n{text}"
        );
    }

    #[test]
    fn permission_pending_renders_on_bottom_row() {
        let mut app = test_app();
        app.permission_pending = Some(pending_perm("write_file"));
        let text = render_to_string(&app, 60, 8);
        let bottom = bottom_row(&text);
        assert!(bottom.contains("write_file"), "tool name missing: {bottom}");
        assert!(bottom.contains("(y)es"), "y option missing: {bottom}");
        assert!(bottom.contains("(n)o"), "n option missing: {bottom}");
    }

    #[test]
    fn command_mode_takes_precedence_over_status_message() {
        let mut app = test_app();
        app.command_mode = true;
        app.command_buffer = "live".into();
        app.status_message = Some("stale".into());
        let text = render_to_string(&app, 40, 8);
        let bottom = bottom_row(&text);
        assert!(bottom.contains(":live"), "command bar missing: {bottom}");
        assert!(!bottom.contains("stale"), "status leaked through: {bottom}");
    }

    #[test]
    fn status_message_takes_precedence_over_permission_pending() {
        let mut app = test_app();
        app.status_message = Some("hi there".into());
        app.permission_pending = Some(pending_perm("rm_rf"));
        let text = render_to_string(&app, 50, 8);
        let bottom = bottom_row(&text);
        assert!(bottom.contains("hi there"), "status missing: {bottom}");
        assert!(!bottom.contains("rm_rf"), "permission bar leaked: {bottom}");
    }

    #[test]
    fn no_overlay_when_nothing_pending() {
        // Sanity: when none of the three are set we don't accidentally
        // wipe the footer's bottom border row.
        let app = test_app();
        let text = render_to_string(&app, 40, 8);
        let bottom = bottom_row(&text);
        // The Keys footer's bottom border row is all `─` and corners.
        assert!(
            bottom.contains('─') || bottom.contains('└') || bottom.contains('┘'),
            "expected border row, got: {bottom}"
        );
    }
}
