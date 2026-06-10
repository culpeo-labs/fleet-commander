//! Auto-sizing wrapped Message input box.
//!
//! The session screen sizes the box by calling [`compute_height`] and
//! then draws it with [`render`]. Helpers ([`wrapped_row_count`],
//! [`caret_position`]) are exposed so the agent-session module can
//! also reason about the layout when it needs to.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph, Wrap},
};

/// Render the input box at `area` showing `buffer`. The terminal
/// cursor is placed at the end of the buffer so the user can see where
/// their next keystroke lands; the box scrolls if `buffer` is taller
/// than `area`.
pub fn render(frame: &mut Frame<'_>, area: Rect, buffer: &str) {
    let inner_width = area.width.saturating_sub(2).max(1) as usize;
    let visible_rows = area.height.saturating_sub(2).max(1) as usize;
    let needed_rows = wrapped_row_count(buffer, inner_width);
    let scroll_offset = needed_rows.saturating_sub(visible_rows);

    let input = Paragraph::new(buffer)
        .wrap(Wrap { trim: false })
        .scroll((scroll_offset as u16, 0))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Message ")
                .border_style(Style::default().fg(Color::Yellow)),
        );
    frame.render_widget(input, area);

    let (caret_row, caret_col) = caret_position(buffer, inner_width);
    let visible_caret_row = caret_row.saturating_sub(scroll_offset);
    let cx = area.x + 1 + caret_col.min(inner_width.saturating_sub(1)) as u16;
    let cy = area.y + 1 + visible_caret_row as u16;
    frame.set_cursor_position((cx, cy));
}

/// Number of terminal rows the wrapped `buffer` will occupy when laid
/// out into a paragraph of `width` columns. Approximates wide chars as
/// width-1 — good enough for the input-box sizing heuristic.
pub fn wrapped_row_count(buffer: &str, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let mut rows = 0usize;
    for line in buffer.split('\n') {
        let cols = line.chars().count().max(1);
        rows += cols.div_ceil(width);
    }
    rows.max(1)
}

/// Height (in rows) the input box should request, clamped to
/// `[3, max_rows]`. Includes top/bottom borders.
pub fn compute_height(buffer: &str, inner_width: usize, max_rows: u16) -> u16 {
    let content_rows = wrapped_row_count(buffer, inner_width) as u16;
    (content_rows + 2).clamp(3, max_rows)
}

/// Wrapped (row, col) position of the caret (assumed to be at the end
/// of `buffer`).
pub fn caret_position(buffer: &str, width: usize) -> (usize, usize) {
    if width == 0 {
        return (0, 0);
    }
    let mut row = 0usize;
    let mut col = 0usize;
    for ch in buffer.chars() {
        if ch == '\n' {
            row += 1;
            col = 0;
        } else if col >= width {
            row += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (row, col)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Screen, SessionFocus};
    use crate::ui::test_support::{render_to_string, test_app};

    #[test]
    fn height_floor_is_three_rows() {
        assert_eq!(compute_height("", 40, 12), 3);
        assert_eq!(compute_height("hi", 40, 12), 3);
    }

    #[test]
    fn height_grows_with_newlines() {
        assert_eq!(compute_height("a\nb\nc", 40, 12), 5);
    }

    #[test]
    fn height_grows_with_wrapped_long_line() {
        let long = "x".repeat(25);
        assert_eq!(compute_height(&long, 10, 12), 5);
    }

    #[test]
    fn height_is_capped_at_max() {
        let huge = "line\n".repeat(50);
        assert_eq!(compute_height(&huge, 40, 8), 8);
    }

    #[test]
    fn caret_position_tracks_newlines_and_wrap() {
        assert_eq!(caret_position("hello", 20), (0, 5));
        assert_eq!(caret_position("a\nb", 20), (1, 1));
        let s = "x".repeat(11);
        assert_eq!(caret_position(&s, 10), (1, 1));
    }

    #[test]
    fn renders_multi_line_content_with_footer_hint() {
        let mut app = test_app();
        app.screen = Screen::AgentSession {
            agent_id: "a1".into(),
            focus: SessionFocus::Conversation,
            side_pane: None,
            scroll: 0,
            input_mode: true,
        };
        app.input_buffer = "first line\nsecond line".into();
        let text = render_to_string(&app, 60, 20);
        assert!(text.contains("first line"), "first line missing:\n{text}");
        assert!(text.contains("second line"), "second line missing:\n{text}");
        assert!(
            text.contains("Alt/Shift+Enter newline"),
            "footer hint missing:\n{text}"
        );
    }

    #[test]
    fn buffer_taller_than_box_keeps_tail_visible() {
        // When the buffer overflows the (capped) input-box height, the
        // box should scroll so the most recent rows — and therefore the
        // caret — stay in view.
        let mut app = test_app();
        app.screen = Screen::AgentSession {
            agent_id: "a1".into(),
            focus: SessionFocus::Conversation,
            side_pane: None,
            scroll: 0,
            input_mode: true,
        };
        // 30 lines into a 12-row tall terminal; max_rows=clamp(12/3,3,12)=4.
        let body: String = (0..30).map(|i| format!("row{i}\n")).collect();
        app.input_buffer = body;
        let text = render_to_string(&app, 80, 12);
        // The freshest rows must appear; the oldest must have scrolled off.
        assert!(text.contains("row29"), "tail missing:\n{text}");
        assert!(!text.contains("row0\n"), "head leaked:\n{text}");
    }
}
