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
}
