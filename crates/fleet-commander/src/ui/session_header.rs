//! Title bar at the top of the agent-session screen.

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use crate::agent::Agent;

pub fn render(frame: &mut Frame<'_>, area: Rect, agent: Option<&Agent>, agent_id: &str) {
    let title = agent
        .map(|a| {
            let branch = a
                .git_branch()
                .map(|b| format!(" ⎇ {b} "))
                .unwrap_or_default();
            format!(" {} [{}]{branch} ", a.name, a.status.label())
        })
        .unwrap_or_else(|| format!(" {agent_id} "));
    let header = Paragraph::new(Line::from(vec![Span::styled(
        title,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )]))
    .alignment(Alignment::Center)
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, area);
}

#[cfg(test)]
mod tests {
    use crate::agent::Agent;
    use crate::app::{App, Screen, SessionFocus};
    use crate::config::Config;
    use crate::ui::test_support::render_to_string;
    use tokio::sync::mpsc;

    fn make_repo(branch: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let head = tmp.path().join(".git").join("HEAD");
        std::fs::create_dir_all(head.parent().unwrap()).unwrap();
        std::fs::write(head, format!("ref: refs/heads/{branch}\n")).unwrap();
        tmp
    }

    fn app_in_session(workspace: Option<&std::path::Path>) -> App {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut a1 = Agent::new("a1", "First");
        if let Some(ws) = workspace {
            a1 = a1.with_workspace(ws);
        }
        let mut app = App::new(Config::default(), vec![a1], tx);
        app.screen = Screen::AgentSession {
            agent_id: "a1".into(),
            focus: SessionFocus::Conversation,
            side_pane: None,
            scroll: 0,
            input_mode: false,
        };
        app
    }

    #[test]
    fn shows_git_branch() {
        let tmp = make_repo("main");
        let app = app_in_session(Some(tmp.path()));
        let text = render_to_string(&app, 90, 20);
        assert!(text.contains("⎇ main"), "branch missing:\n{text}");
    }

    #[test]
    fn omits_branch_outside_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app_in_session(Some(tmp.path()));
        let text = render_to_string(&app, 90, 20);
        assert!(!text.contains("⎇"), "branch glyph leaked:\n{text}");
    }
}
