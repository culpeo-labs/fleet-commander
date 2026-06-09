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

use crate::agent::{Agent, HistoryEntry};
use crate::app::{App, Screen, SessionFocus, SidePane};
use crate::markdown;

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
            input_mode,
        } => render_agent_session(
            frame,
            area,
            app,
            agent_id,
            *focus,
            side_pane.as_ref(),
            *scroll,
            *input_mode,
            &app.input_buffer,
        ),
    }

    // Command bar overlay — drawn last so it sits on top of the footer.
    if app.command_mode {
        let bar_area = Rect {
            x: area.x,
            y: area.y + area.height.saturating_sub(1),
            width: area.width,
            height: 1,
        };
        let text = format!(":{}", app.command_buffer);
        let bar = Paragraph::new(Span::styled(
            text,
            Style::default()
                .fg(Color::White)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(Color::DarkGray));
        frame.render_widget(bar, bar_area);
    } else if let Some(msg) = &app.status_message {
        let bar_area = Rect {
            x: area.x,
            y: area.y + area.height.saturating_sub(1),
            width: area.width,
            height: 1,
        };
        let bar = Paragraph::new(Span::styled(
            msg.as_str(),
            Style::default().fg(Color::Yellow).bg(Color::DarkGray),
        ))
        .style(Style::default().bg(Color::DarkGray));
        frame.render_widget(bar, bar_area);
    } else if let Some(perm) = &app.permission_pending {
        let bar_area = Rect {
            x: area.x,
            y: area.y + area.height.saturating_sub(1),
            width: area.width,
            height: 1,
        };
        let text = format!("🔐 Allow {}? (y)es / (n)o", perm.tool_name);
        let bar = Paragraph::new(Span::styled(
            text,
            Style::default()
                .fg(Color::White)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(Color::Magenta));
        frame.render_widget(bar, bar_area);
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

    let footer = Paragraph::new("↑/k  ↓/j  Enter open  :open <path>  q quit")
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL).title(" Keys "));
    frame.render_widget(footer, layout[1]);
}

fn status_color(status: &crate::agent::AgentStatus) -> Color {
    match status {
        crate::agent::AgentStatus::Idle => Color::Gray,
        crate::agent::AgentStatus::Running => Color::Green,
        crate::agent::AgentStatus::Stopped => Color::DarkGray,
        crate::agent::AgentStatus::Error => Color::Red,
    }
}

#[allow(clippy::too_many_arguments)]
fn render_agent_session(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    agent_id: &str,
    focus: SessionFocus,
    side_pane: Option<&SidePane>,
    scroll: usize,
    input_mode: bool,
    input_buffer: &str,
) {
    let constraints = if input_mode {
        vec![
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
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

    // Body: conversation on the left, side pane on the right when present.
    let body_area = layout[1];
    if let Some(pane) = side_pane {
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(body_area);
        render_conversation(
            frame,
            body[0],
            agent,
            scroll,
            focus == SessionFocus::Conversation,
        );
        render_side_pane(frame, body[1], pane, focus == SessionFocus::SidePane);
    } else {
        render_conversation(frame, body_area, agent, scroll, true);
    }

    let hint = if input_mode {
        "Enter send  Esc cancel"
    } else if side_pane.is_some() {
        "Esc back  Tab switch focus  d dismiss pane  i input  ↑/↓ scroll"
    } else {
        "Esc back  i input  ↑/↓ scroll"
    };
    let footer = Paragraph::new(hint)
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL).title(" Keys "));
    let footer_idx = if input_mode { 3 } else { 2 };
    frame.render_widget(footer, layout[footer_idx]);

    // Render input box when in input mode.
    if input_mode {
        let input = Paragraph::new(input_buffer).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Message ")
                .border_style(Style::default().fg(Color::Yellow)),
        );
        frame.render_widget(input, layout[2]);
    }
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
            let mut result: Vec<Line> = Vec::new();
            if a.history.is_empty() {
                result.push(Line::from(Span::styled(
                    "(no messages yet)",
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                for entry in &a.history {
                    render_history_entry(entry, &mut result);
                }
            }
            result
        })
        .unwrap_or_default();

    // Compute effective scroll: usize::MAX means "follow bottom".
    let viewport_height = area.height.saturating_sub(2) as usize; // minus borders
    let total_lines = lines.len();
    let effective_scroll = if scroll == usize::MAX {
        total_lines.saturating_sub(viewport_height)
    } else {
        scroll.min(total_lines.saturating_sub(viewport_height))
    };

    let paragraph = Paragraph::new(lines)
        .scroll((effective_scroll as u16, 0))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Conversation ")
                .border_style(style),
        );
    frame.render_widget(paragraph, area);
}

/// Append a single history entry to the rendered line buffer.
fn render_history_entry(entry: &HistoryEntry, out: &mut Vec<Line<'static>>) {
    use fleet_commander_core::session::{MessageStatus, ToolCallStatusKind};

    match entry {
        HistoryEntry::Info(text) => {
            for line in text.lines() {
                out.push(Line::from(Span::raw(line.to_string())));
            }
        }
        HistoryEntry::Error(text) => {
            let style = Style::default().fg(Color::Red);
            for line in text.lines() {
                out.push(Line::from(Span::styled(line.to_string(), style)));
            }
        }
        HistoryEntry::Prompt(text) => {
            let style = Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD);
            for line in text.lines() {
                out.push(Line::from(Span::styled(format!("> {line}"), style)));
            }
        }
        HistoryEntry::Assistant(msg) => {
            let text = msg.text.borrow();
            let status_ref = msg.status.borrow();
            let terminal = status_ref.is_terminal();
            let failed = matches!(&*status_ref, MessageStatus::Failed(_));
            let body: &str = text.as_str();
            if body.is_empty() {
                if !terminal {
                    out.push(Line::from(Span::styled(
                        "▊",
                        Style::default().fg(Color::Green),
                    )));
                }
                return;
            }
            if terminal && (markdown::looks_like_markdown(body) || markdown::looks_like_json(body))
            {
                let md_lines = if markdown::looks_like_json(body) {
                    let wrapped = format!("```json\n{body}\n```");
                    markdown::render_markdown(&wrapped)
                } else {
                    markdown::render_markdown(body)
                };
                out.extend(md_lines);
            } else {
                let style = if terminal {
                    Style::default()
                } else {
                    Style::default().fg(Color::Green)
                };
                for line in body.lines() {
                    out.push(Line::from(Span::styled(line.to_string(), style)));
                }
                if !terminal {
                    out.push(Line::from(Span::styled(
                        "▊",
                        Style::default().fg(Color::Green),
                    )));
                }
            }
            if failed {
                out.push(Line::from(Span::styled(
                    "[message failed]",
                    Style::default().fg(Color::Red),
                )));
            }
        }
        HistoryEntry::Thought(thought) => {
            let text = thought.text.borrow();
            let terminal = thought.status.borrow().is_terminal();
            let body: &str = text.as_str();
            let style = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC);
            if terminal {
                let trimmed = body.trim();
                if trimmed.is_empty() {
                    return;
                }
                out.push(Line::from(Span::styled(format!("💭 {trimmed}"), style)));
            } else {
                let preview: String = body.chars().take(80).collect();
                out.push(Line::from(Span::styled(format!("💭 {preview}…"), style)));
            }
        }
        HistoryEntry::User(msg) => {
            let text = msg.text.borrow();
            let body: &str = text.as_str();
            let style = Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD);
            for line in body.lines() {
                out.push(Line::from(Span::styled(format!("> {line}"), style)));
            }
        }
        HistoryEntry::Tool(tc) => {
            let title = tc.title.borrow();
            let status = *tc.status.borrow();
            let (marker, color) = match status {
                ToolCallStatusKind::Pending | ToolCallStatusKind::InProgress => {
                    ("⏳", Color::Yellow)
                }
                ToolCallStatusKind::Completed => ("✓", Color::Green),
                ToolCallStatusKind::Failed => ("✗", Color::Red),
            };
            let display_title = if title.is_empty() {
                "(tool)"
            } else {
                title.as_str()
            };
            out.push(Line::from(Span::styled(
                format!("{marker} {display_title}"),
                Style::default().fg(color),
            )));
        }
    }
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
    use tokio::sync::mpsc;

    fn agents() -> Vec<Agent> {
        vec![Agent::new("a1", "First"), Agent::new("a2", "Second")]
    }

    fn test_app() -> App {
        let (tx, _rx) = mpsc::unbounded_channel();
        App::new(Config::default(), agents(), tx)
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
        let app = test_app();
        terminal.draw(|f| render(f, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Agents"));
        assert!(text.contains("First"));
        assert!(text.contains("Second"));
    }

    #[test]
    fn agent_session_without_side_pane_does_not_show_diff_or_editor() {
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        let mut app = test_app();
        app.screen = Screen::AgentSession {
            agent_id: "a1".into(),
            focus: SessionFocus::Conversation,
            side_pane: None,
            scroll: 0,
            input_mode: false,
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

    // ─── render_history_entry coverage ────────────────────────────────────
    //
    // These tests exercise the new handle-based conversation rendering.
    // We construct watch channels by hand (the same way the runtime does
    // internally) and drive them synchronously, then render to a
    // TestBackend and scrape the resulting cell buffer.

    use crate::agent::HistoryEntry;
    use fleet_commander_core::session::{
        AssistantMessage as AssistantHandle, MessageStatus, Thought as ThoughtHandle,
        ToolCall as ToolCallHandle, ToolCallStatusKind, UserMessage as UserHandle,
    };
    use tokio::sync::watch;

    /// Build an app whose first agent is parked in a session screen so
    /// `render_conversation` is the target of any frame draw.
    fn app_in_session_with(history: Vec<HistoryEntry>) -> App {
        let mut app = test_app();
        if let Some(agent) = app.agents.iter_mut().find(|a| a.id == "a1") {
            agent.history = history;
        }
        app.screen = Screen::AgentSession {
            agent_id: "a1".into(),
            focus: SessionFocus::Conversation,
            side_pane: None,
            // usize::MAX = follow bottom — same as the live app.
            scroll: usize::MAX,
            input_mode: false,
        };
        app
    }

    fn render_to_string(app: &App, cols: u16, rows: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        buffer_text(&terminal)
    }

    fn assistant(
        initial: &str,
        status: MessageStatus,
    ) -> (
        HistoryEntry,
        watch::Sender<String>,
        watch::Sender<MessageStatus>,
    ) {
        let (text_tx, text_rx) = watch::channel(initial.to_string());
        let (status_tx, status_rx) = watch::channel(status);
        (
            HistoryEntry::Assistant(AssistantHandle {
                text: text_rx,
                status: status_rx,
            }),
            text_tx,
            status_tx,
        )
    }

    fn thought(
        initial: &str,
        status: MessageStatus,
    ) -> HistoryEntry {
        let (_text_tx, text_rx) = watch::channel(initial.to_string());
        let (_status_tx, status_rx) = watch::channel(status);
        // Leak the senders so the receivers keep yielding the initial value
        // for the lifetime of the test render.
        Box::leak(Box::new((_text_tx, _status_tx)));
        HistoryEntry::Thought(ThoughtHandle {
            text: text_rx,
            status: status_rx,
        })
    }

    fn user(initial: &str) -> HistoryEntry {
        let (text_tx, text_rx) = watch::channel(initial.to_string());
        let (status_tx, status_rx) = watch::channel(MessageStatus::Completed);
        Box::leak(Box::new((text_tx, status_tx)));
        HistoryEntry::User(UserHandle {
            text: text_rx,
            status: status_rx,
        })
    }

    fn tool(
        id: &str,
        title: &str,
        status: ToolCallStatusKind,
    ) -> (
        HistoryEntry,
        watch::Sender<String>,
        watch::Sender<ToolCallStatusKind>,
    ) {
        let (title_tx, title_rx) = watch::channel(title.to_string());
        let (status_tx, status_rx) = watch::channel(status);
        (
            HistoryEntry::Tool(ToolCallHandle {
                id: id.to_string(),
                title: title_rx,
                status: status_rx,
            }),
            title_tx,
            status_tx,
        )
    }

    #[test]
    fn empty_conversation_shows_placeholder() {
        let app = app_in_session_with(vec![]);
        let text = render_to_string(&app, 60, 12);
        assert!(
            text.contains("(no messages yet)"),
            "expected placeholder, got:\n{text}"
        );
    }

    #[test]
    fn info_and_error_entries_render_plain_text() {
        let app = app_in_session_with(vec![
            HistoryEntry::Info("ACP session connected.".into()),
            HistoryEntry::Error("[error] connection lost".into()),
        ]);
        let text = render_to_string(&app, 60, 12);
        assert!(text.contains("ACP session connected."));
        assert!(text.contains("connection lost"));
    }

    #[test]
    fn prompt_entry_has_caret_prefix() {
        let app = app_in_session_with(vec![HistoryEntry::Prompt("hello there".into())]);
        let text = render_to_string(&app, 60, 12);
        assert!(
            text.contains("> hello there"),
            "expected `> hello there`, got:\n{text}"
        );
    }

    #[test]
    fn streaming_assistant_shows_cursor() {
        let (entry, text_tx, _status_tx) = assistant("", MessageStatus::Streaming);
        let app = app_in_session_with(vec![entry]);
        // Empty body: cursor `▊` should be visible.
        let text = render_to_string(&app, 60, 12);
        assert!(text.contains('▊'), "expected cursor while streaming");

        // Push some content; cursor remains because still streaming.
        text_tx.send("Hello, wor".to_string()).unwrap();
        let text = render_to_string(&app, 60, 12);
        assert!(text.contains("Hello, wor"));
        assert!(text.contains('▊'));
    }

    #[test]
    fn completed_assistant_renders_without_cursor() {
        let (entry, text_tx, status_tx) = assistant("", MessageStatus::Streaming);
        let app = app_in_session_with(vec![entry]);

        text_tx.send("Plain reply".to_string()).unwrap();
        status_tx.send(MessageStatus::Completed).unwrap();

        let text = render_to_string(&app, 60, 12);
        assert!(text.contains("Plain reply"));
        assert!(!text.contains('▊'), "cursor should be gone once terminal");
    }

    #[test]
    fn completed_assistant_renders_markdown_heading() {
        let (entry, text_tx, status_tx) = assistant("", MessageStatus::Streaming);
        let app = app_in_session_with(vec![entry]);

        text_tx
            .send("# Big Heading\n\nbody line".to_string())
            .unwrap();
        status_tx.send(MessageStatus::Completed).unwrap();

        let text = render_to_string(&app, 60, 12);
        // Both heading text and body survive the markdown pipeline.
        assert!(text.contains("Big Heading"), "got:\n{text}");
        assert!(text.contains("body line"));
    }

    #[test]
    fn streaming_assistant_does_not_run_markdown_pipeline() {
        // A streaming assistant with markdown-looking content should NOT
        // collapse paragraph breaks — we render it as raw streaming text.
        let (entry, text_tx, _status_tx) = assistant("", MessageStatus::Streaming);
        let app = app_in_session_with(vec![entry]);
        text_tx
            .send("first paragraph\n\nsecond paragraph".to_string())
            .unwrap();

        let text = render_to_string(&app, 60, 12);
        assert!(text.contains("first paragraph"));
        assert!(text.contains("second paragraph"));
        // Cursor visible because still streaming.
        assert!(text.contains('▊'));
    }

    #[test]
    fn streaming_thought_collapses_to_preview() {
        let long = "x".repeat(200);
        let app = app_in_session_with(vec![thought(&long, MessageStatus::Streaming)]);
        let text = render_to_string(&app, 100, 12);
        assert!(text.contains('💭'), "expected thought marker");
        assert!(text.contains('…'), "streaming thought should be truncated");
    }

    #[test]
    fn completed_thought_renders_full_body() {
        let app = app_in_session_with(vec![thought(
            "I should call read_file next.",
            MessageStatus::Completed,
        )]);
        let text = render_to_string(&app, 80, 12);
        assert!(text.contains('💭'));
        assert!(text.contains("I should call read_file next."));
        assert!(
            !text.contains('…'),
            "completed thought should not be truncated"
        );
    }

    #[test]
    fn user_message_uses_caret_prefix() {
        let app = app_in_session_with(vec![user("previous question")]);
        let text = render_to_string(&app, 60, 12);
        assert!(text.contains("> previous question"));
    }

    #[test]
    fn tool_call_marker_reflects_status_transitions() {
        let (entry, title_tx, status_tx) =
            tool("call_1", "read_file", ToolCallStatusKind::InProgress);
        let app = app_in_session_with(vec![entry]);

        // InProgress: ⏳ marker.
        let text = render_to_string(&app, 60, 12);
        assert!(text.contains('⏳'), "got:\n{text}");
        assert!(text.contains("read_file"));

        // Title update + status transition reflect on next render without
        // a new HistoryEntry.
        title_tx.send("read_file (cached)".to_string()).unwrap();
        status_tx.send(ToolCallStatusKind::Completed).unwrap();

        let text = render_to_string(&app, 60, 12);
        assert!(text.contains('✓'));
        assert!(text.contains("read_file (cached)"));
        assert!(!text.contains('⏳'));
    }

    #[test]
    fn failed_tool_call_uses_cross_marker() {
        let (entry, _title_tx, _status_tx) =
            tool("call_2", "shell", ToolCallStatusKind::Failed);
        // Keep the senders alive for the duration of the render.
        Box::leak(Box::new((_title_tx, _status_tx)));
        let app = app_in_session_with(vec![entry]);
        let text = render_to_string(&app, 60, 12);
        assert!(text.contains('✗'));
    }

    #[test]
    fn empty_tool_title_falls_back_to_placeholder() {
        let (entry, _title_tx, _status_tx) =
            tool("call_3", "", ToolCallStatusKind::InProgress);
        Box::leak(Box::new((_title_tx, _status_tx)));
        let app = app_in_session_with(vec![entry]);
        let text = render_to_string(&app, 60, 12);
        assert!(text.contains("(tool)"));
    }

    #[test]
    fn mixed_history_renders_in_order() {
        let (assistant_entry, assistant_text, assistant_status) =
            assistant("", MessageStatus::Streaming);
        assistant_text.send("All done.".to_string()).unwrap();
        assistant_status.send(MessageStatus::Completed).unwrap();
        let (tool_entry, _tt, _ts) =
            tool("t1", "list_files", ToolCallStatusKind::Completed);
        Box::leak(Box::new((_tt, _ts)));

        let app = app_in_session_with(vec![
            HistoryEntry::Prompt("show me everything".into()),
            HistoryEntry::Info("ACP session connected.".into()),
            tool_entry,
            assistant_entry,
        ]);
        let text = render_to_string(&app, 80, 12);
        let prompt_pos = text.find("show me everything").expect("prompt");
        let info_pos = text.find("ACP session connected.").expect("info");
        let tool_pos = text.find("list_files").expect("tool");
        let assistant_pos = text.find("All done.").expect("assistant");
        assert!(
            prompt_pos < info_pos && info_pos < tool_pos && tool_pos < assistant_pos,
            "entries rendered out of order:\n{text}"
        );
    }

    // ─── scroll / follow-bottom rendering ─────────────────────────────────

    /// Build many Info entries so the conversation overflows the viewport.
    fn many_info_entries(n: usize) -> Vec<HistoryEntry> {
        (0..n)
            .map(|i| HistoryEntry::Info(format!("line {i}")))
            .collect()
    }

    /// Render with an explicit `scroll` value (instead of `usize::MAX`).
    fn render_with_scroll(app: &mut App, scroll: usize, cols: u16, rows: u16) -> String {
        if let Screen::AgentSession {
            scroll: s, ..
        } = &mut app.screen
        {
            *s = scroll;
        }
        render_to_string(app, cols, rows)
    }

    #[test]
    fn follow_bottom_shows_most_recent_entries_when_overflowing() {
        // 50 entries, viewport ~10 lines (12 rows - header - footer - borders).
        let mut app = app_in_session_with(many_info_entries(50));
        // usize::MAX => follow bottom.
        let text = render_with_scroll(&mut app, usize::MAX, 60, 16);
        // The latest entries should be visible.
        assert!(text.contains("line 49"), "expected last entry, got:\n{text}");
        assert!(text.contains("line 48"));
        // The earliest must have scrolled out of view.
        assert!(
            !text.contains("line 0\n") && !text.contains("line 1\n"),
            "early entries should be off-screen:\n{text}"
        );
    }

    #[test]
    fn scroll_at_zero_shows_oldest_entries() {
        let mut app = app_in_session_with(many_info_entries(50));
        let text = render_with_scroll(&mut app, 0, 60, 16);
        assert!(text.contains("line 0"), "expected first entry, got:\n{text}");
        assert!(text.contains("line 1"));
        // Last lines are clearly off-screen for a 50-entry buffer with a
        // viewport of ~10 lines.
        assert!(!text.contains("line 49"));
    }

    #[test]
    fn scroll_clamps_when_set_past_end() {
        // A scroll value far beyond the actual content length should clamp
        // to the bottom, not produce a blank pane.
        let mut app = app_in_session_with(many_info_entries(50));
        let text = render_with_scroll(&mut app, 10_000, 60, 16);
        assert!(
            text.contains("line 49"),
            "out-of-range scroll must clamp to bottom, got:\n{text}"
        );
    }

    #[tokio::test]
    async fn rehydration_renders_latest_turn_visible() {
        // Simulate session/load: a long replayed conversation. After all
        // events are processed, the latest exchange must be the one shown.
        use crate::event::AppEvent;
        use fleet_commander_core::session::SessionEvent;

        let mut app = test_app();
        app.screen = Screen::AgentSession {
            agent_id: "a1".into(),
            focus: SessionFocus::Conversation,
            side_pane: None,
            scroll: 0,
            input_mode: false,
        };

        // 20 prior turns (40 entries) — easily overflows any reasonable viewport.
        for i in 0..20 {
            let (text_tx, text_rx) = watch::channel(format!("question {i}"));
            let (status_tx, status_rx) = watch::channel(MessageStatus::Completed);
            let _ = (text_tx, status_tx);
            app.handle(AppEvent::Session(SessionEvent::UserMessage {
                agent_id: "a1".into(),
                message: UserHandle {
                    text: text_rx,
                    status: status_rx,
                },
            }));

            let (text_tx, text_rx) = watch::channel(format!("answer {i}"));
            let (status_tx, status_rx) = watch::channel(MessageStatus::Completed);
            let _ = (text_tx, status_tx);
            app.handle(AppEvent::Session(SessionEvent::AssistantMessage {
                agent_id: "a1".into(),
                message: AssistantHandle {
                    text: text_rx,
                    status: status_rx,
                },
            }));
        }

        let text = render_to_string(&app, 80, 18);
        assert!(
            text.contains("answer 19"),
            "expected most recent answer to be visible, got:\n{text}"
        );
        assert!(text.contains("question 19"));
        // The first turn must have scrolled off-screen.
        assert!(
            !text.contains("question 0\n") && !text.contains("answer 0\n"),
            "oldest turn should not be visible after rehydration:\n{text}"
        );
    }
}
