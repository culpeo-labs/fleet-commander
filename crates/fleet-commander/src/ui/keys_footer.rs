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
        return "Enter send  Alt/Shift+Enter newline  / commands  Tab complete  Esc cancel";
    }
    match (side_pane, following) {
        (true, true) => {
            "Esc back  Tab switch focus  d dismiss pane  i input  C-e explorer  ↑/↓ scroll"
        }
        (true, false) => {
            "Esc back  Tab switch focus  d dismiss pane  i input  C-e explorer  ↑/↓ scroll  G follow"
        }
        (false, true) => "Esc back  i input  C-e explorer  ↑/↓ scroll",
        (false, false) => "Esc back  i input  C-e explorer  ↑/↓ scroll  G follow",
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

    #[test]
    fn side_pane_hints_mention_tab_and_dismiss() {
        let with_side = session_hint(false, true, false);
        assert!(
            with_side.contains("Tab"),
            "Tab focus toggle missing: {with_side}"
        );
        assert!(
            with_side.contains("d dismiss"),
            "dismiss missing: {with_side}"
        );
        // Without a side pane those affordances should disappear.
        let no_side = session_hint(false, false, false);
        assert!(!no_side.contains("Tab"));
        assert!(!no_side.contains("dismiss"));
    }

    #[test]
    fn input_mode_overrides_other_dimensions() {
        // input_mode should produce the same hint regardless of side
        // pane / follow state — those bindings aren't reachable from
        // inside the input box.
        let a = session_hint(true, false, false);
        let b = session_hint(true, true, true);
        assert_eq!(a, b);
    }
}
