//! Right-hand side panel — diff viewer and slash-commands browser.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::app::SidePane;
use crate::ui::style::border_style;
use crate::ui::syntax::highlight_for_path;

pub fn render(frame: &mut Frame<'_>, area: Rect, pane: &SidePane, focused: bool) {
    let style = border_style(focused);
    match pane {
        SidePane::Diff {
            path,
            content,
            scroll,
        } => {
            let title = format!(" Diff: {} ", path.display());
            let lines = highlight_for_path(content, path);
            let paragraph = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((*scroll, 0))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(title)
                        .border_style(style),
                );
            frame.render_widget(paragraph, area);
        }
        SidePane::FileView {
            path,
            content,
            scroll,
        } => {
            let title = format!(" File: {} ", path.display());
            let lines = highlight_for_path(content, path);
            let paragraph = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((*scroll, 0))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(title)
                        .border_style(style),
                );
            frame.render_widget(paragraph, area);
        }
        SidePane::Commands { commands, scroll } => {
            let title = format!(" Commands ({}) ", commands.len());
            // Two lines per entry: header (name + optional hint) and
            // description on the next, indented line.
            let mut lines: Vec<Line<'_>> = Vec::with_capacity(commands.len() * 2);
            let mut sorted: Vec<_> = commands.iter().collect();
            sorted.sort_by(|a, b| a.name.cmp(&b.name));
            for c in sorted {
                let mut header = vec![Span::styled(
                    format!("/{}", c.name),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )];
                if let Some(hint) = &c.hint {
                    header.push(Span::styled(
                        format!(" <{hint}>"),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                lines.push(Line::from(header));
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(c.description.clone(), Style::default().fg(Color::Gray)),
                ]));
            }
            let paragraph = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((*scroll, 0))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(title)
                        .border_style(style),
                );
            frame.render_widget(paragraph, area);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::app::{Screen, SessionFocus, SidePane};
    use crate::ui::test_support::{render_to_string, test_app};
    use std::path::PathBuf;

    fn app_with_side_pane(pane: SidePane, focus: SessionFocus) -> crate::app::App {
        let mut app = test_app();
        app.screen = Screen::AgentSession {
            agent_id: "a1".into(),
            focus,
            side_pane: Some(pane),
            scroll: 0,
            input_mode: false,
        };
        app
    }

    #[test]
    fn file_view_renders_file_title_and_content() {
        let pane = SidePane::FileView {
            path: PathBuf::from("src/main.rs"),
            content: "fn greet() {}\n".into(),
            scroll: 0,
        };
        let app = app_with_side_pane(pane, SessionFocus::SidePane);
        let text = render_to_string(&app, 100, 12);
        assert!(text.contains("File:"), "File title missing:\n{text}");
        assert!(text.contains("src/main.rs"), "path missing:\n{text}");
        assert!(text.contains("fn greet()"), "body missing:\n{text}");
    }

    #[test]
    fn file_view_scroll_offset_hides_top_lines() {
        // With a scroll offset the first lines should drop out of view.
        let content = (0..30)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let unscrolled = SidePane::FileView {
            path: PathBuf::from("f.txt"),
            content: content.clone(),
            scroll: 0,
        };
        let app = app_with_side_pane(unscrolled, SessionFocus::SidePane);
        assert!(render_to_string(&app, 100, 12).contains("line0"));

        let scrolled = SidePane::FileView {
            path: PathBuf::from("f.txt"),
            content,
            scroll: 10,
        };
        let app = app_with_side_pane(scrolled, SessionFocus::SidePane);
        let text = render_to_string(&app, 100, 12);
        assert!(!text.contains("line0\n") && !text.contains("line0 "));
        assert!(
            text.contains("line10"),
            "expected scrolled content:\n{text}"
        );
    }

    #[test]
    fn diff_renders_path_in_title_and_content_in_body() {
        let pane = SidePane::Diff {
            path: PathBuf::from("src/lib.rs"),
            content: "fn answer() -> i32 { 42 }\n".into(),
            scroll: 0,
        };
        let app = app_with_side_pane(pane, SessionFocus::SidePane);
        let text = render_to_string(&app, 100, 12);
        assert!(text.contains("Diff:"), "Diff title missing:\n{text}");
        assert!(text.contains("src/lib.rs"), "path missing:\n{text}");
        assert!(text.contains("fn answer()"), "body missing:\n{text}");
    }

    #[test]
    fn long_diff_lines_wrap_inside_pane() {
        // Single long line should wrap within the side pane width
        // rather than being truncated to one row.
        let long = "x".repeat(80);
        let pane = SidePane::Diff {
            path: PathBuf::from("wide.txt"),
            content: long,
            scroll: 0,
        };
        let app = app_with_side_pane(pane, SessionFocus::SidePane);
        // Side pane gets 45% of 80 cols = ~36 cols. A 80-char line must
        // wrap onto multiple rows; both halves should still be present.
        let text = render_to_string(&app, 80, 12);
        let x_count = text.chars().filter(|&c| c == 'x').count();
        assert!(
            x_count >= 80,
            "expected wrapped content (>=80 'x'): {x_count}"
        );
    }

    #[test]
    fn commands_pane_lists_each_command_with_description() {
        use crate::agent::AvailableCommand;
        let pane = SidePane::Commands {
            commands: vec![
                AvailableCommand {
                    name: "model".into(),
                    description: "Select AI model to use".into(),
                    hint: Some("model".into()),
                },
                AvailableCommand {
                    name: "session".into(),
                    description: "View and manage sessions".into(),
                    hint: Some("info|rename".into()),
                },
            ],
            scroll: 0,
        };
        let app = app_with_side_pane(pane, SessionFocus::SidePane);
        let text = render_to_string(&app, 100, 16);
        assert!(text.contains("Commands"), "title missing:\n{text}");
        assert!(text.contains("/model"), "model missing:\n{text}");
        assert!(
            text.contains("Select AI model"),
            "model desc missing:\n{text}"
        );
        assert!(text.contains("/session"), "session missing:\n{text}");
        assert!(
            text.contains("View and manage"),
            "session desc missing:\n{text}"
        );
    }
}
