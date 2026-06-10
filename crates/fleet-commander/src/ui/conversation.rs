//! Scrollable conversation pane on the agent-session screen.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap},
};

use crate::agent::{Agent, HistoryEntry};
use crate::markdown;
use crate::ui::style::border_style;

pub fn render(
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

    // The paragraph wraps long lines, so the visible row count is *not*
    // equal to `lines.len()`. Build the paragraph first and ask it how
    // many rendered rows it will produce at the inner width, then use
    // that to clamp scroll. Without this, follow-bottom mode crops the
    // last N rows whenever any line wraps.
    //
    // Note: `Paragraph::line_count` includes the block's vertical border
    // space, so we measure *before* attaching the block to avoid
    // double-counting the two border rows.
    let inner_width = area.width.saturating_sub(2); // minus borders
    let viewport_height = area.height.saturating_sub(2) as usize; // minus borders
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total_rows = paragraph.line_count(inner_width);
    let max_scroll = total_rows.saturating_sub(viewport_height);
    let effective_scroll = if scroll == usize::MAX {
        max_scroll
    } else {
        scroll.min(max_scroll)
    };

    // Stash the computed top-of-viewport line so that the synchronous
    // handler for `Action::Up` can break out of follow-bottom mode by
    // anchoring at exactly the line the user is currently looking at.
    if let Some(a) = agent {
        a.last_effective_top.set(effective_scroll);
    }

    let paragraph = paragraph
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Conversation ")
                .border_style(style),
        )
        .scroll((effective_scroll as u16, 0));
    frame.render_widget(paragraph, area);

    // Render a scrollbar on the right border whenever content overflows
    // the viewport. The scrollbar overlays the block's right border so it
    // doesn't steal any text column.
    if max_scroll > 0 {
        let mut scrollbar_state = ScrollbarState::new(max_scroll).position(effective_scroll);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .style(style)
            .begin_symbol(None)
            .end_symbol(None);
        frame.render_stateful_widget(scrollbar, area, &mut scrollbar_state);
    }
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

#[cfg(test)]
mod tests {
    use crate::agent::HistoryEntry;
    use crate::app::{App, Screen, SessionFocus};
    use crate::ui::test_support::{render_to_string, test_app};
    use fleet_commander_core::session::{
        AssistantMessage as AssistantHandle, MessageStatus, Thought as ThoughtHandle,
        ToolCall as ToolCallHandle, ToolCallStatusKind, UserMessage as UserHandle,
    };
    use tokio::sync::watch;

    /// Build an app whose first agent is parked in a session screen so
    /// the conversation renderer is the target of any frame draw.
    fn app_in_session_with(history: Vec<HistoryEntry>) -> App {
        let mut app = test_app();
        if let Some(agent) = app.agents.iter_mut().find(|a| a.id == "a1") {
            agent.history = history;
        }
        app.screen = Screen::AgentSession {
            agent_id: "a1".into(),
            focus: SessionFocus::Conversation,
            side_pane: None,
            scroll: usize::MAX,
            input_mode: false,
        };
        app
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
        let entry = HistoryEntry::Assistant(AssistantHandle {
            text: text_rx,
            status: status_rx,
        });
        (entry, text_tx, status_tx)
    }

    fn thought(initial: &str, status: MessageStatus) -> HistoryEntry {
        let (_tx, rx) = watch::channel(initial.to_string());
        let (_stx, srx) = watch::channel(status);
        HistoryEntry::Thought(ThoughtHandle {
            text: rx,
            status: srx,
        })
    }

    fn user(initial: &str) -> HistoryEntry {
        let (_tx, rx) = watch::channel(initial.to_string());
        let (_stx, srx) = watch::channel(MessageStatus::Completed);
        HistoryEntry::User(UserHandle {
            text: rx,
            status: srx,
        })
    }

    fn tool(
        title: &str,
        status: ToolCallStatusKind,
    ) -> (
        HistoryEntry,
        watch::Sender<String>,
        watch::Sender<ToolCallStatusKind>,
    ) {
        let (title_tx, title_rx) = watch::channel(title.to_string());
        let (status_tx, status_rx) = watch::channel(status);
        let entry = HistoryEntry::Tool(ToolCallHandle {
            id: "tc1".to_string(),
            title: title_rx,
            status: status_rx,
        });
        (entry, title_tx, status_tx)
    }

    #[test]
    fn empty_conversation_shows_placeholder() {
        let app = app_in_session_with(Vec::new());
        let text = render_to_string(&app, 60, 12);
        assert!(
            text.contains("(no messages yet)"),
            "placeholder missing:\n{text}"
        );
    }

    #[test]
    fn info_and_error_entries_render_plain_text() {
        let app = app_in_session_with(vec![
            HistoryEntry::Info("hello".into()),
            HistoryEntry::Error("kaboom".into()),
        ]);
        let text = render_to_string(&app, 60, 12);
        assert!(text.contains("hello"));
        assert!(text.contains("kaboom"));
    }

    #[test]
    fn prompt_entry_has_caret_prefix() {
        let app = app_in_session_with(vec![HistoryEntry::Prompt("hi".into())]);
        let text = render_to_string(&app, 60, 12);
        assert!(text.contains("> hi"), "expected caret prefix:\n{text}");
    }

    #[test]
    fn streaming_assistant_shows_cursor() {
        let (entry, _t, _s) = assistant("streaming…", MessageStatus::Streaming);
        let app = app_in_session_with(vec![entry]);
        let text = render_to_string(&app, 60, 12);
        assert!(text.contains("streaming"));
        assert!(text.contains("▊"), "expected streaming cursor:\n{text}");
    }

    #[test]
    fn completed_assistant_renders_without_cursor() {
        let (entry, _t, _s) = assistant("done.", MessageStatus::Completed);
        let app = app_in_session_with(vec![entry]);
        let text = render_to_string(&app, 60, 12);
        assert!(text.contains("done."));
        assert!(!text.contains("▊"));
    }

    #[test]
    fn completed_assistant_renders_markdown_heading() {
        let (entry, _t, _s) = assistant("# Heading\nbody", MessageStatus::Completed);
        let app = app_in_session_with(vec![entry]);
        let text = render_to_string(&app, 80, 12);
        // The exact rendering depends on markdown.rs, but a heading
        // shouldn't appear with a literal `#` prefix.
        assert!(text.contains("Heading"));
    }

    #[test]
    fn streaming_assistant_does_not_run_markdown_pipeline() {
        let (entry, _t, _s) = assistant("# Heading", MessageStatus::Streaming);
        let app = app_in_session_with(vec![entry]);
        let text = render_to_string(&app, 80, 12);
        assert!(
            text.contains("# Heading"),
            "streaming should print verbatim:\n{text}"
        );
    }

    #[test]
    fn streaming_thought_collapses_to_preview() {
        let entry = thought("partial reasoning…", MessageStatus::Streaming);
        let app = app_in_session_with(vec![entry]);
        let text = render_to_string(&app, 60, 12);
        assert!(text.contains("partial reasoning"));
        assert!(text.contains("…"), "preview ellipsis missing:\n{text}");
    }

    #[test]
    fn completed_thought_renders_full_body() {
        let entry = thought("all of the reasoning", MessageStatus::Completed);
        let app = app_in_session_with(vec![entry]);
        let text = render_to_string(&app, 80, 12);
        assert!(text.contains("all of the reasoning"));
    }

    #[test]
    fn user_message_uses_caret_prefix() {
        let entry = user("hello");
        let app = app_in_session_with(vec![entry]);
        let text = render_to_string(&app, 60, 12);
        assert!(text.contains("> hello"));
    }

    #[test]
    fn tool_call_marker_reflects_status_transitions() {
        let (entry, _t, status_tx) = tool("doing-thing", ToolCallStatusKind::InProgress);
        let app = app_in_session_with(vec![entry]);
        let text = render_to_string(&app, 60, 12);
        assert!(text.contains("⏳"));
        assert!(text.contains("doing-thing"));

        status_tx.send(ToolCallStatusKind::Completed).unwrap();
        let text = render_to_string(&app, 60, 12);
        assert!(text.contains("✓"));
    }

    #[test]
    fn failed_tool_call_uses_cross_marker() {
        let (entry, _t, _s) = tool("oops", ToolCallStatusKind::Failed);
        let app = app_in_session_with(vec![entry]);
        let text = render_to_string(&app, 60, 12);
        assert!(text.contains("✗"));
    }

    #[test]
    fn empty_tool_title_falls_back_to_placeholder() {
        let (entry, _t, _s) = tool("", ToolCallStatusKind::Completed);
        let app = app_in_session_with(vec![entry]);
        let text = render_to_string(&app, 60, 12);
        assert!(text.contains("(tool)"), "fallback missing:\n{text}");
    }

    #[test]
    fn mixed_history_renders_in_order() {
        let (a, _t, _s) = assistant("hi", MessageStatus::Completed);
        let history = vec![
            HistoryEntry::Prompt("question".into()),
            a,
            user("follow-up"),
        ];
        let app = app_in_session_with(history);
        let text = render_to_string(&app, 60, 12);
        let q = text.find("question").expect("prompt present");
        let h = text.find("hi").expect("assistant present");
        let f = text.find("follow-up").expect("user present");
        assert!(q < h && h < f, "out of order:\n{text}");
    }

    // ─── Scroll behaviour ────────────────────────────────────────────────

    fn many_info_entries(n: usize) -> Vec<HistoryEntry> {
        (0..n)
            .map(|i| HistoryEntry::Info(format!("entry {i}")))
            .collect()
    }

    fn render_with_scroll(app: &mut App, scroll: usize, cols: u16, rows: u16) -> String {
        if let Screen::AgentSession { scroll: s, .. } = &mut app.screen {
            *s = scroll;
        }
        render_to_string(app, cols, rows)
    }

    #[test]
    fn follow_bottom_shows_most_recent_entries_when_overflowing() {
        let mut app = app_in_session_with(many_info_entries(50));
        let text = render_with_scroll(&mut app, usize::MAX, 60, 10);
        assert!(text.contains("entry 49"), "newest missing:\n{text}");
        assert!(!text.contains("entry 0\n"), "oldest leaked:\n{text}");
    }

    #[test]
    fn scroll_at_zero_shows_oldest_entries() {
        let mut app = app_in_session_with(many_info_entries(50));
        let text = render_with_scroll(&mut app, 0, 60, 10);
        assert!(text.contains("entry 0"), "oldest missing:\n{text}");
        assert!(!text.contains("entry 49"), "newest leaked:\n{text}");
    }

    #[test]
    fn scroll_clamps_when_set_past_end() {
        // A scroll value far beyond the actual content length should clamp
        // to the bottom, not produce a blank pane.
        let mut app = app_in_session_with(many_info_entries(50));
        let text = render_with_scroll(&mut app, 9_999, 60, 10);
        assert!(
            text.contains("entry 49"),
            "out-of-range scroll must clamp to bottom:\n{text}"
        );
    }

    #[test]
    fn scrollbar_appears_when_content_overflows_viewport() {
        let mut app = app_in_session_with(many_info_entries(50));
        let text = render_with_scroll(&mut app, usize::MAX, 60, 10);
        assert!(
            text.contains('█') || text.contains('▆') || text.contains('▼') || text.contains('▲'),
            "expected scrollbar glyph:\n{text}"
        );
    }

    #[test]
    fn scrollbar_hidden_when_content_fits_in_viewport() {
        let mut app = app_in_session_with(many_info_entries(3));
        let text = render_with_scroll(&mut app, 0, 60, 12);
        // Scrollbar glyphs from ratatui's default thumb/track.
        assert!(
            !text.contains('█') && !text.contains('▆'),
            "scrollbar leaked when content fits:\n{text}"
        );
    }

    #[test]
    fn manual_scroll_position_persists_when_new_entries_arrive() {
        // Build a history that overflows the viewport, then scroll to
        // an arbitrary position (not follow-bottom). After appending
        // more entries the user's chosen line should still be at the
        // top of the viewport.
        let mut app = app_in_session_with(many_info_entries(30));
        let baseline = render_with_scroll(&mut app, 5, 60, 8);
        // Add one more entry.
        if let Some(agent) = app.agents.iter_mut().find(|a| a.id == "a1") {
            agent.history.push(HistoryEntry::Info("entry 30".into()));
        }
        let after = render_with_scroll(&mut app, 5, 60, 8);
        // The viewport's top line should still be the one anchored by
        // scroll=5 — i.e. the topmost visible row in both renderings
        // shows the same "entry N" string.
        let top_baseline = baseline.lines().nth(2).unwrap_or("");
        let top_after = after.lines().nth(2).unwrap_or("");
        assert_eq!(top_baseline, top_after);
    }

    // ─── Wrap-aware sizing ───────────────────────────────────────────────

    #[test]
    fn single_long_line_wraps_and_counts_toward_total_rows() {
        // A single Info entry whose body is much wider than the
        // viewport should still drive the scrollbar — the wrap-aware
        // line_count() call is what guarantees this. If it regressed
        // back to lines.len() the bar would never appear.
        let long = "y".repeat(200);
        let mut app = app_in_session_with(vec![HistoryEntry::Info(long)]);
        let text = render_with_scroll(&mut app, usize::MAX, 30, 8);
        assert!(
            text.contains('█') || text.contains('▆'),
            "scrollbar missing for wrapped single line:\n{text}"
        );
    }

    #[test]
    fn follow_bottom_with_wrapped_lines_shows_tail() {
        // Build entries whose body wraps onto several visual rows. In
        // follow-bottom mode the very last entry must still be visible
        // — this catches the regression where `lines.len()` was used
        // instead of `paragraph.line_count(width)` for clamping scroll.
        let history: Vec<HistoryEntry> = (0..30)
            .map(|i| HistoryEntry::Info(format!("entry{i}-{}", "z".repeat(120))))
            .collect();
        let mut app = app_in_session_with(history);
        // 16 rows total leaves the conversation pane ~10 inner rows,
        // enough to hold the last entry's wrapped body.
        let text = render_with_scroll(&mut app, usize::MAX, 30, 16);
        assert!(
            text.contains("entry29"),
            "newest wrapped entry missing in follow-bottom:\n{text}"
        );
    }

    // ─── last_effective_top anchor for breaking follow-bottom on `Up` ────

    #[test]
    fn last_effective_top_is_recorded_on_each_render() {
        let mut app = app_in_session_with(many_info_entries(50));
        // Follow-bottom should anchor at max_scroll (>0 with 50 lines
        // in a small viewport).
        render_with_scroll(&mut app, usize::MAX, 60, 8);
        let agent = app.agents.iter().find(|a| a.id == "a1").unwrap();
        let top_following = agent.last_effective_top.get();
        assert!(
            top_following > 0,
            "follow-bottom should anchor below 0 with overflowing content"
        );

        // Scroll explicitly to 5 — anchor should now reflect that.
        render_with_scroll(&mut app, 5, 60, 8);
        let agent = app.agents.iter().find(|a| a.id == "a1").unwrap();
        assert_eq!(agent.last_effective_top.get(), 5);
    }
}
