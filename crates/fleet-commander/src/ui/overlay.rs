//! Bottom-row overlays drawn on top of every screen: command bar and
//! transient status message. (Permission requests render as their own
//! centered modal — see `ui::permission_popup`.)

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
    } else if app.search_mode {
        render_search_bar(frame, bar_area, &app.search_query);
    } else if let Some(msg) = &app.status_message {
        render_status_bar(frame, bar_area, msg);
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

fn render_search_bar(frame: &mut Frame<'_>, area: Rect, query: &str) {
    let text = format!("/{query}");
    let bar = Paragraph::new(Span::styled(
        text,
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
    .style(Style::default().bg(Color::Cyan));
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

#[cfg(test)]
mod tests {
    use crate::ui::test_support::{render_to_string, test_app};

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
    fn search_mode_renders_query_on_bottom_row() {
        let mut app = test_app();
        app.search_mode = true;
        app.search_query = "needle".into();
        let text = render_to_string(&app, 40, 8);
        assert!(
            bottom_row(&text).contains("/needle"),
            "search prompt missing from bottom row:\n{text}"
        );
    }

    #[test]
    fn command_mode_takes_precedence_over_search_mode() {
        let mut app = test_app();
        app.command_mode = true;
        app.command_buffer = "live".into();
        app.search_mode = true;
        app.search_query = "stale".into();
        let text = render_to_string(&app, 40, 8);
        let bottom = bottom_row(&text);
        assert!(bottom.contains(":live"), "command bar missing: {bottom}");
        assert!(
            !bottom.contains("/stale"),
            "search leaked through: {bottom}"
        );
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
