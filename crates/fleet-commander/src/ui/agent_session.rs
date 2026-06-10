//! Agent session screen — orchestrates the title, conversation, side
//! pane, optional input box, and footer.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
};

use crate::app::{App, SessionFocus, SidePane};
use crate::ui::{conversation, input_box, keys_footer, session_header, side_pane};

#[allow(clippy::too_many_arguments)]
pub fn render(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    agent_id: &str,
    focus: SessionFocus,
    side: Option<&SidePane>,
    scroll: usize,
    input_mode: bool,
    input_buffer: &str,
) {
    let input_height = if input_mode {
        let inner_width = area.width.saturating_sub(2).max(1);
        let max_rows: u16 = (area.height / 3).clamp(3, 12);
        input_box::compute_height(input_buffer, inner_width as usize, max_rows)
    } else {
        3
    };
    let constraints = if input_mode {
        vec![
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(input_height),
            Constraint::Length(3),
        ]
    } else {
        vec![
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ]
    };

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let agent = app.agents.iter().find(|a| a.id == agent_id);
    session_header::render(frame, layout[0], agent, agent_id);

    let body_area = layout[1];
    if let Some(pane) = side {
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(body_area);
        conversation::render(
            frame,
            body[0],
            agent,
            scroll,
            focus == SessionFocus::Conversation,
        );
        side_pane::render(frame, body[1], pane, focus == SessionFocus::SidePane);
    } else {
        conversation::render(frame, body_area, agent, scroll, true);
    }

    let following = scroll == usize::MAX;
    let hint = keys_footer::session_hint(input_mode, side.is_some(), following);
    let footer_idx = if input_mode { 3 } else { 2 };
    keys_footer::render(frame, layout[footer_idx], hint);

    if input_mode {
        input_box::render(frame, layout[2], input_buffer);
    }
}

#[cfg(test)]
mod tests {
    use crate::app::{Screen, SessionFocus, SidePane};
    use crate::ui::test_support::{render_to_string, test_app};
    use std::path::PathBuf;

    #[test]
    fn without_side_pane_does_not_show_diff_pane() {
        let mut app = test_app();
        app.screen = Screen::AgentSession {
            agent_id: "a1".into(),
            focus: SessionFocus::Conversation,
            side_pane: None,
            scroll: 0,
            input_mode: false,
        };
        let text = render_to_string(&app, 80, 16);
        assert!(text.contains("Conversation"));
        assert!(!text.contains("Diff:"), "diff pane should be hidden");
    }

    #[test]
    fn with_diff_side_pane_renders_diff_title() {
        let mut app = test_app();
        app.screen = Screen::AgentSession {
            agent_id: "a1".into(),
            focus: SessionFocus::Conversation,
            side_pane: Some(SidePane::Diff {
                path: PathBuf::from("foo.rs"),
                content: "fn main() {}\n".into(),
            }),
            scroll: 0,
            input_mode: false,
        };
        let text = render_to_string(&app, 100, 16);
        assert!(text.contains("Diff:"));
        assert!(text.contains("foo.rs"));
        assert!(text.contains("Conversation"));
    }
}
