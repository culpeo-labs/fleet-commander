//! Bottom "Keys" footer hint shared by both screens.

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    widgets::{Block, Borders, Paragraph},
};

pub fn render(frame: &mut Frame<'_>, area: Rect, hint: &str) {
    let footer = Paragraph::new(hint)
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL).title(" Keys "));
    frame.render_widget(footer, area);
}

/// Build the hint string for the agent-session screen. Centralised so
/// every render path returns the same canonical strings — the tests
/// can grep for them without reaching into private rendering code.
pub fn session_hint(input_mode: bool, side_pane: bool, following: bool) -> &'static str {
    if input_mode {
        return "Enter send  Alt/Shift+Enter newline  Esc cancel";
    }
    match (side_pane, following) {
        (true, true) => "Esc back  Tab switch focus  d dismiss pane  i input  ↑/↓ scroll",
        (true, false) => {
            "Esc back  Tab switch focus  d dismiss pane  i input  ↑/↓ scroll  G follow"
        }
        (false, true) => "Esc back  i input  ↑/↓ scroll",
        (false, false) => "Esc back  i input  ↑/↓ scroll  G follow",
    }
}

#[cfg(test)]
mod tests {
    use super::session_hint;

    #[test]
    fn input_mode_hint_mentions_newline_modifier() {
        let hint = session_hint(true, false, false);
        assert!(hint.contains("Alt/Shift+Enter newline"));
        assert!(hint.contains("Enter send"));
    }

    #[test]
    fn following_mode_omits_follow_hint() {
        assert!(!session_hint(false, false, true).contains("G follow"));
        assert!(session_hint(false, false, false).contains("G follow"));
    }
}
