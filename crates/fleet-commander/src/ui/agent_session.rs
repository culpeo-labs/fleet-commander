//! Agent session screen — orchestrates the title, conversation, side
//! pane, optional input box, and footer.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
};

use crate::app::{App, SessionFocus, SidePane};
use crate::ui::{
    conversation, explorer, input_box, keys_footer, session_header, side_pane, slash_popover,
};

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
    let explorer_open = app.explorer.open;
    // Compose horizontal layout: optional explorer | conversation | optional side pane.
    let mut constraints: Vec<Constraint> = Vec::new();
    if explorer_open {
        constraints.push(Constraint::Length(30));
    }
    if side.is_some() {
        constraints.push(Constraint::Percentage(55));
        constraints.push(Constraint::Percentage(45));
    } else {
        constraints.push(Constraint::Min(0));
    }
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(body_area);

    let mut idx = 0;
    if explorer_open {
        explorer::render(
            frame,
            body[idx],
            &app.explorer,
            focus == SessionFocus::Explorer,
        );
        idx += 1;
    }
    if let Some(pane) = side {
        conversation::render(
            frame,
            body[idx],
            agent,
            scroll,
            focus == SessionFocus::Conversation,
        );
        side_pane::render(frame, body[idx + 1], pane, focus == SessionFocus::SidePane);
    } else {
        conversation::render(
            frame,
            body[idx],
            agent,
            scroll,
            focus != SessionFocus::Explorer,
        );
    }

    let following = scroll == usize::MAX;
    let hint = keys_footer::session_hint(input_mode, side.is_some(), following);
    let footer_idx = if input_mode { 3 } else { 2 };
    keys_footer::render(frame, layout[footer_idx], hint);

    if input_mode {
        input_box::render(frame, layout[2], input_buffer);
        // Slash-command popover overlays the body just above the input
        // box so the input cursor stays visible while the user picks.
        if let Some(matches) = app.slash_matches_for(agent_id) {
            slash_popover::render(frame, layout[2], &matches, app.slash_selected);
        }
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
                scroll: 0,
            }),
            scroll: 0,
            input_mode: false,
        };
        let text = render_to_string(&app, 100, 16);
        assert!(text.contains("Diff:"));
        assert!(text.contains("foo.rs"));
        assert!(text.contains("Conversation"));
    }

    #[test]
    fn slash_input_renders_command_popover() {
        // When the buffer starts with `/`, the popover lists the agent's
        // advertised commands so the user can pick one. Commands not
        // matching the typed prefix are filtered out.
        use crate::agent::AvailableCommand;
        let mut app = test_app();
        if let Some(agent) = app.agents.iter_mut().find(|a| a.id == "a1") {
            agent.available_commands = vec![
                AvailableCommand {
                    name: "model".into(),
                    description: "Select AI model".into(),
                    hint: Some("model".into()),
                },
                AvailableCommand {
                    name: "memory".into(),
                    description: "Show memory status".into(),
                    hint: None,
                },
                AvailableCommand {
                    name: "plan".into(),
                    description: "Create a plan".into(),
                    hint: None,
                },
            ];
        }
        app.screen = Screen::AgentSession {
            agent_id: "a1".into(),
            focus: SessionFocus::Conversation,
            side_pane: None,
            scroll: 0,
            input_mode: true,
        };
        app.input_buffer = "/me".into();
        let text = render_to_string(&app, 100, 24);
        assert!(text.contains("Commands"), "popover missing:\n{text}");
        assert!(text.contains("/memory"), "matching cmd missing:\n{text}");
        assert!(text.contains("Show memory status"));
        assert!(!text.contains("/plan"), "non-matching cmd leaked:\n{text}");
    }

    #[test]
    fn slash_popover_hidden_when_no_slash_or_argument_started() {
        let mut app = test_app();
        if let Some(agent) = app.agents.iter_mut().find(|a| a.id == "a1") {
            agent.available_commands = vec![crate::agent::AvailableCommand {
                name: "model".into(),
                description: "Select AI model".into(),
                hint: None,
            }];
        }
        app.screen = Screen::AgentSession {
            agent_id: "a1".into(),
            focus: SessionFocus::Conversation,
            side_pane: None,
            scroll: 0,
            input_mode: true,
        };
        // No leading slash → no popover.
        app.input_buffer = "hello".into();
        assert!(!render_to_string(&app, 100, 24).contains("Commands"));
        // Slash but already typed an argument → popover closed.
        app.input_buffer = "/model gpt-5".into();
        assert!(!render_to_string(&app, 100, 24).contains("Commands"));
    }

    #[test]
    fn input_mode_inserts_message_box_above_footer() {
        let mut app = test_app();
        app.screen = Screen::AgentSession {
            agent_id: "a1".into(),
            focus: SessionFocus::Conversation,
            side_pane: None,
            scroll: 0,
            input_mode: true,
        };
        app.input_buffer = "draft".into();
        let text = render_to_string(&app, 80, 16);
        assert!(text.contains("Message"), "Message box missing:\n{text}");
        assert!(text.contains("draft"), "buffer content missing:\n{text}");
        // Footer must still be present below the input box.
        assert!(text.contains("Enter send"), "footer hint missing:\n{text}");
    }

    #[test]
    fn input_box_height_grows_for_multi_line_buffer() {
        // With a single-line buffer the input box is 3 rows; with a
        // 5-line buffer it must grow to fit (capped at area.height/3).
        let mut app = test_app();
        app.screen = Screen::AgentSession {
            agent_id: "a1".into(),
            focus: SessionFocus::Conversation,
            side_pane: None,
            scroll: 0,
            input_mode: true,
        };
        app.input_buffer = "a\nb\nc\nd\ne".into();
        let text = render_to_string(&app, 80, 30);
        // All 5 lines must be visible inside the box.
        for letter in ["a", "b", "c", "d", "e"] {
            assert!(text.contains(letter), "missing '{letter}':\n{text}");
        }
    }

    #[test]
    fn explorer_pane_appears_when_open() {
        let mut app = test_app();
        app.screen = Screen::AgentSession {
            agent_id: "a1".into(),
            focus: SessionFocus::Conversation,
            side_pane: None,
            scroll: 0,
            input_mode: false,
        };
        app.explorer.open = true;
        let text = render_to_string(&app, 100, 16);
        assert!(
            text.contains("Explorer:"),
            "explorer title missing:\n{text}"
        );
        assert!(text.contains("Conversation"));
    }

    #[test]
    fn explorer_hidden_when_closed() {
        let app = test_app_in_session(SessionFocus::Conversation);
        let text = render_to_string(&app, 100, 16);
        assert!(!text.contains("Explorer:"));
    }

    fn test_app_in_session(focus: SessionFocus) -> crate::app::App {
        let mut app = test_app();
        app.screen = Screen::AgentSession {
            agent_id: "a1".into(),
            focus,
            side_pane: None,
            scroll: 0,
            input_mode: false,
        };
        app
    }
}
