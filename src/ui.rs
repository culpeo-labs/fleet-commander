//! Rendering for the two screens. Kept deliberately pure — no I/O, no event
//! handling — so it can be exercised with ratatui's `TestBackend`.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use std::sync::LazyLock;
use syntect::{easy::HighlightLines, highlighting::ThemeSet, parsing::SyntaxSet};

use crate::agent::Agent;
use crate::app::{App, Screen, SessionFocus, SidePane};

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

pub fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    match &app.screen {
        Screen::AgentList { selected } => render_agent_list(frame, area, &app.agents, *selected),
        Screen::AgentSession {
            agent_id,
            focus,
            side_pane,
            scroll,
        } => render_agent_session(
            frame,
            area,
            app,
            agent_id,
            *focus,
            side_pane.as_ref(),
            *scroll,
        ),
    }
}

fn render_agent_list(frame: &mut Frame<'_>, area: Rect, agents: &[Agent], selected: usize) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
        .split(area);

    let items: Vec<ListItem> = agents
        .iter()
        .map(|agent| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {:<20} ", agent.name),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    format!("[{}]", agent.status.label()),
                    Style::default().fg(status_color(&agent.status)),
                ),
            ]))
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

    let footer = Paragraph::new("↑/k  ↓/j  Enter open  q quit")
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL).title(" Keys "));
    frame.render_widget(footer, layout[1]);
}

fn status_color(status: &crate::agent::AgentStatus) -> Color {
    match status {
        crate::agent::AgentStatus::Idle => Color::Gray,
        crate::agent::AgentStatus::Running => Color::Green,
        crate::agent::AgentStatus::Stopped => Color::DarkGray,
    }
}

fn render_agent_session(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    agent_id: &str,
    focus: SessionFocus,
    side_pane: Option<&SidePane>,
    scroll: usize,
) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(area);

    let agent = app.agents.iter().find(|a| a.id == agent_id);
    let title = agent
        .map(|a| format!(" {} [{}] ", a.name, a.status.label()))
        .unwrap_or_else(|| format!(" {agent_id} "));
    let header = Paragraph::new(Line::from(vec![Span::styled(
        title,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )]))
    .alignment(Alignment::Center)
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, layout[0]);

    // Body: side pane (left) only when present.
    let body_area = layout[1];
    if let Some(pane) = side_pane {
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(body_area);
        render_side_pane(frame, body[0], pane, focus == SessionFocus::SidePane);
        render_conversation(
            frame,
            body[1],
            agent,
            scroll,
            focus == SessionFocus::Conversation,
        );
    } else {
        render_conversation(frame, body_area, agent, scroll, true);
    }

    let hint = if side_pane.is_some() {
        "Esc back  Tab switch focus  d dismiss pane  ↑/↓ scroll"
    } else {
        "Esc back  ↑/↓ scroll  (side pane opens when agent changes a file)"
    };
    let footer = Paragraph::new(hint)
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL).title(" Keys "));
    frame.render_widget(footer, layout[2]);
}

fn render_conversation(
    frame: &mut Frame<'_>,
    area: Rect,
    agent: Option<&Agent>,
    scroll: usize,
    focused: bool,
) {
    let style = border_style(focused);
    let lines: Vec<Line> = agent
        .map(|a| {
            if a.history.is_empty() {
                vec![Line::from(Span::styled(
                    "(no messages yet)",
                    Style::default().fg(Color::DarkGray),
                ))]
            } else {
                a.history
                    .iter()
                    .map(|line| Line::from(line.as_str()))
                    .collect()
            }
        })
        .unwrap_or_default();

    let paragraph = Paragraph::new(lines)
        .scroll((scroll as u16, 0))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Conversation ")
                .border_style(style),
        );
    frame.render_widget(paragraph, area);
}

fn render_side_pane(frame: &mut Frame<'_>, area: Rect, pane: &SidePane, focused: bool) {
    let style = border_style(focused);
    match pane {
        SidePane::Diff { path, content } => {
            let title = format!(" Diff: {} ", path.display());
            let lines = highlight_for_path(content, path);
            let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(style),
            );
            frame.render_widget(paragraph, area);
        }
        SidePane::Editor { path, buffer } => {
            let title = format!(" Editor: {} ", path.display());
            let lines = highlight_for_path(buffer, path);
            let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(style),
            );
            frame.render_widget(paragraph, area);
        }
    }
}

fn border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn highlight_for_path(source: &str, path: &std::path::Path) -> Vec<Line<'static>> {
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let syntax = SYNTAX_SET
        .find_syntax_by_extension(ext)
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let theme = THEME_SET
        .themes
        .get("base16-ocean.dark")
        .or_else(|| THEME_SET.themes.values().next())
        .expect("at least one theme is bundled");
    let mut highlighter = HighlightLines::new(syntax, theme);

    source
        .lines()
        .map(|line| {
            let ranges = highlighter
                .highlight_line(line, &SYNTAX_SET)
                .unwrap_or_default();
            let spans: Vec<Span<'static>> = ranges
                .into_iter()
                .map(|(style, text)| {
                    Span::styled(
                        text.to_string(),
                        Style::default().fg(Color::Rgb(
                            style.foreground.r,
                            style.foreground.g,
                            style.foreground.b,
                        )),
                    )
                })
                .collect();
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::app::{App, Screen, SessionFocus, SidePane};
    use crate::config::Config;
    use ratatui::{Terminal, backend::TestBackend};
    use std::path::PathBuf;

    fn agents() -> Vec<Agent> {
        vec![Agent::new("a1", "First"), Agent::new("a2", "Second")]
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let (cols, rows) = (buffer.area.width, buffer.area.height);
        let mut out = String::new();
        for y in 0..rows {
            for x in 0..cols {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn agent_list_renders_each_agent() {
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let app = App::new(Config::default(), agents());
        terminal.draw(|f| render(f, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Agents"));
        assert!(text.contains("First"));
        assert!(text.contains("Second"));
    }

    #[test]
    fn agent_session_without_side_pane_does_not_show_diff_or_editor() {
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        let mut app = App::new(Config::default(), agents());
        app.screen = Screen::AgentSession {
            agent_id: "a1".into(),
            focus: SessionFocus::Conversation,
            side_pane: None,
            scroll: 0,
        };
        terminal.draw(|f| render(f, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Conversation"));
        assert!(!text.contains("Diff:"), "diff pane should be hidden");
        assert!(!text.contains("Editor:"), "editor pane should be hidden");
    }

    #[test]
    fn agent_session_with_diff_side_pane_renders_diff_title() {
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        let mut app = App::new(Config::default(), agents());
        app.screen = Screen::AgentSession {
            agent_id: "a1".into(),
            focus: SessionFocus::Conversation,
            side_pane: Some(SidePane::Diff {
                path: PathBuf::from("foo.rs"),
                content: "fn main() {}\n".into(),
            }),
            scroll: 0,
        };
        terminal.draw(|f| render(f, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Diff:"), "expected diff pane title");
        assert!(
            text.contains("foo.rs"),
            "expected diff pane to show file path"
        );
        assert!(
            text.contains("Conversation"),
            "conversation must remain visible"
        );
    }
}
