//! Slash-command autocomplete popover.
//!
//! When the user is composing a message that starts with `/`, this
//! popover floats above the input box showing commands advertised by
//! the agent via ACP's `available_commands_update`. The user filters
//! by typing more of the name; `Tab` completes; `Up`/`Down` navigates.
//!
//! State is owned by the consumer (so the popover module stays pure):
//! - input text comes from `App::input_buffer`,
//! - the selected index from `App::slash_selected`,
//! - the command list from the focused `Agent::available_commands`.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState},
};

use crate::agent::AvailableCommand;

/// Parse the slash-command prefix out of `buffer`.
///
/// Returns `Some(prefix)` (the substring after the leading `/`, up to
/// the first whitespace) when the buffer is a command in progress and
/// the popover should be visible. Returns `None` when the buffer is
/// empty, doesn't start with `/`, contains a newline (multi-line
/// drafts aren't commands), or already has whitespace after the
/// command name (the user is typing the argument).
pub fn extract_prefix(buffer: &str) -> Option<&str> {
    let rest = buffer.strip_prefix('/')?;
    if rest.contains('\n') {
        return None;
    }
    // If the user has typed whitespace, the command name is fixed and
    // we're now writing the argument — close the popover.
    if rest.contains(char::is_whitespace) {
        return None;
    }
    Some(rest)
}

/// Filter `commands` by name prefix (case-insensitive).
pub fn filter<'a>(commands: &'a [AvailableCommand], prefix: &str) -> Vec<&'a AvailableCommand> {
    let needle = prefix.to_lowercase();
    let mut matches: Vec<&AvailableCommand> = commands
        .iter()
        .filter(|c| c.name.to_lowercase().starts_with(&needle))
        .collect();
    // Stable alphabetical order — agents may emit commands in any order.
    matches.sort_by(|a, b| a.name.cmp(&b.name));
    matches
}

/// Render the popover above `input_area`, returning the area that was
/// painted so the caller knows where the input cursor needs to go.
/// Does nothing if there are no matches.
pub fn render(
    frame: &mut Frame<'_>,
    input_area: Rect,
    matches: &[&AvailableCommand],
    selected: usize,
) {
    if matches.is_empty() {
        return;
    }

    // Reserve up to 8 rows for matches; the popover never exceeds the
    // space available above the input box.
    let desired_rows = (matches.len() as u16 + 2).min(10);
    let available_above = input_area.y.saturating_sub(1);
    let rows = desired_rows.min(available_above).max(3);
    if rows < 3 {
        return;
    }

    let popover_area = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(rows),
        width: input_area.width,
        height: rows,
    };

    frame.render_widget(Clear, popover_area);

    let items: Vec<ListItem> = matches
        .iter()
        .map(|c| {
            let name = Span::styled(
                format!("/{}", c.name),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
            let hint = c
                .hint
                .as_deref()
                .map(|h| Span::styled(format!(" <{h}>"), Style::default().fg(Color::DarkGray)))
                .unwrap_or_else(|| Span::raw(""));
            let desc = Span::styled(
                format!(" — {}", c.description),
                Style::default().fg(Color::Gray),
            );
            ListItem::new(Line::from(vec![name, hint, desc]))
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(selected.min(matches.len().saturating_sub(1))));

    // Two-column layout: title row implied by block border.
    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1)])
        .split(popover_area)[0];

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(format!(" Commands ({}) ", matches.len())),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, inner, &mut state);
}

/// Complete `buffer` to the given command's name plus a trailing space
/// (so the user can continue typing the argument). Returns the new
/// buffer; caller is responsible for replacing the input.
pub fn completion_for(name: &str) -> String {
    format!("/{name} ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(name: &str, hint: Option<&str>) -> AvailableCommand {
        AvailableCommand {
            name: name.into(),
            description: format!("describe {name}"),
            hint: hint.map(str::to_string),
        }
    }

    #[test]
    fn extract_prefix_returns_none_for_non_slash() {
        assert_eq!(extract_prefix(""), None);
        assert_eq!(extract_prefix("hello"), None);
        assert_eq!(extract_prefix(" /not-a-command"), None);
    }

    #[test]
    fn extract_prefix_returns_text_after_slash() {
        assert_eq!(extract_prefix("/"), Some(""));
        assert_eq!(extract_prefix("/mo"), Some("mo"));
        assert_eq!(extract_prefix("/model"), Some("model"));
    }

    #[test]
    fn extract_prefix_closes_on_whitespace_after_name() {
        // Once an argument is being typed, the popover should close so
        // it doesn't get in the way.
        assert_eq!(extract_prefix("/cwd "), None);
        assert_eq!(extract_prefix("/cwd /tmp"), None);
        assert_eq!(extract_prefix("/cmd\nline2"), None);
    }

    #[test]
    fn filter_is_case_insensitive_and_prefix_only() {
        let cs = vec![
            cmd("model", None),
            cmd("memory", None),
            cmd("mcp", None),
            cmd("plan", None),
        ];
        let m = filter(&cs, "me");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "memory");
        let m = filter(&cs, "M");
        assert_eq!(m.len(), 3);
        // Stable alphabetical order regardless of input order.
        assert_eq!(m[0].name, "mcp");
        assert_eq!(m[1].name, "memory");
        assert_eq!(m[2].name, "model");
    }

    #[test]
    fn filter_empty_prefix_returns_all() {
        let cs = vec![cmd("z", None), cmd("a", None)];
        let m = filter(&cs, "");
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].name, "a");
    }

    #[test]
    fn completion_for_appends_trailing_space() {
        assert_eq!(completion_for("model"), "/model ");
    }
}
