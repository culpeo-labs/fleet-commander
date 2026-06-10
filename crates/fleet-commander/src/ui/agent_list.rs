//! Top-level "all agents" screen.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use crate::agent::Agent;
use crate::ui::style::status_color;

pub fn render(frame: &mut Frame<'_>, area: Rect, agents: &[Agent], selected: usize) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
        .split(area);

    let items: Vec<ListItem> = agents
        .iter()
        .map(|agent| {
            let mut spans = vec![
                Span::styled(
                    format!(" {:<20} ", agent.name),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    format!("[{}]", agent.status.label()),
                    Style::default().fg(status_color(&agent.status)),
                ),
            ];
            if let Some(branch) = agent.git_branch() {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    format!("⎇ {branch}"),
                    Style::default().fg(Color::Magenta),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Agents ")
                .title_alignment(Alignment::Center),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    let mut state = ListState::default();
    state.select(Some(selected));
    frame.render_stateful_widget(list, layout[0], &mut state);

    let footer = Paragraph::new("↑/k  ↓/j  Enter open  :open <path>  q quit")
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL).title(" Keys "));
    frame.render_widget(footer, layout[1]);
}

#[cfg(test)]
mod tests {
    use crate::ui::test_support::{render_to_string, test_app};

    #[test]
    fn renders_each_agent() {
        let app = test_app();
        let text = render_to_string(&app, 60, 12);
        assert!(text.contains("Agents"));
        assert!(text.contains("First"));
        assert!(text.contains("Second"));
    }

    #[test]
    fn shows_git_branch_when_workspace_is_repo() {
        use crate::agent::Agent;
        use crate::app::App;
        use crate::config::Config;
        use tokio::sync::mpsc;

        let tmp = tempfile::tempdir().unwrap();
        let head = tmp.path().join(".git").join("HEAD");
        std::fs::create_dir_all(head.parent().unwrap()).unwrap();
        std::fs::write(head, "ref: refs/heads/topic/widgets\n").unwrap();

        let (tx, _rx) = mpsc::unbounded_channel();
        let agents = vec![
            Agent::new("a1", "First").with_workspace(tmp.path()),
            Agent::new("a2", "Second"),
        ];
        let app = App::new(Config::default(), agents, tx);
        let text = render_to_string(&app, 80, 12);
        assert!(
            text.contains("⎇ topic/widgets"),
            "branch missing from agent list:\n{text}"
        );
    }

    #[test]
    fn marks_the_currently_selected_agent_with_an_arrow() {
        let app = test_app();
        let text = render_to_string(&app, 60, 12);
        let first_row = text
            .lines()
            .find(|l| l.contains("First"))
            .expect("first agent row");
        assert!(
            first_row.contains('▶'),
            "selection marker missing on first row: {first_row}"
        );
        let second_row = text
            .lines()
            .find(|l| l.contains("Second"))
            .expect("second agent row");
        assert!(
            !second_row.contains('▶'),
            "selection marker leaked onto second row: {second_row}"
        );
    }

    #[test]
    fn shows_status_label_next_to_agent_name() {
        let app = test_app();
        let text = render_to_string(&app, 60, 12);
        // Default agent status is `Idle` and labels render as `[idle]`.
        assert!(text.contains("[idle]"), "status label missing:\n{text}");
    }

    #[test]
    fn footer_advertises_open_command_and_quit() {
        let app = test_app();
        let text = render_to_string(&app, 80, 12);
        assert!(text.contains(":open <path>"), "open hint missing:\n{text}");
        assert!(text.contains("q quit"), "quit hint missing:\n{text}");
    }
}
