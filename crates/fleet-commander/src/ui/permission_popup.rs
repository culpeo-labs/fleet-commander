//! Tool-permission request modal.
//!
//! When an agent asks to run a tool, the runtime emits a
//! `PermissionRequest` and blocks on the user's answer. We surface it as
//! a centered modal popup that lists every option the agent offered
//! (allow once, allow always, reject once, …) rather than a cramped
//! yes/no bar. While the popup is open it owns all keyboard input — see
//! the permission branch in `App::handle_key` — so keystrokes can never
//! leak into the message input box behind it.
//!
//! State (the option list and the highlighted index) lives on
//! [`crate::app::PendingPermission`]; this module stays a pure renderer.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::app::PendingPermission;

/// Render the permission modal centered over `area`. No-op when there is
/// no pending request.
pub fn render(frame: &mut Frame<'_>, area: Rect, perm: &PendingPermission) {
    let popup = centered_rect(area, perm);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" 🔐 Permission required ")
        .border_style(
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Header line: which tool wants to run, then the option list, then a
    // one-line key hint at the bottom.
    let header_h = 2u16;
    let hint_h = 1u16;
    let list_h = inner.height.saturating_sub(header_h + hint_h);

    let header_area = Rect {
        height: header_h,
        ..inner
    };
    let list_area = Rect {
        y: inner.y + header_h,
        height: list_h,
        ..inner
    };
    let hint_area = Rect {
        y: inner.y + header_h + list_h,
        height: hint_h,
        ..inner
    };

    let header = Paragraph::new(Line::from(vec![
        Span::raw("Allow "),
        Span::styled(
            perm.tool_name.clone(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("?"),
    ]))
    .wrap(Wrap { trim: false });
    frame.render_widget(header, header_area);

    let items: Vec<ListItem> = perm
        .options
        .iter()
        .enumerate()
        .map(|(i, (_, label, kind))| {
            let color = if kind.starts_with("allow") {
                Color::Green
            } else {
                Color::Red
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {}. ", i + 1),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(label.clone(), Style::default().fg(color)),
            ]))
        })
        .collect();

    let list = List::new(items)
        .highlight_style(Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED));
    let mut state = ListState::default();
    if !perm.options.is_empty() {
        state.select(Some(perm.selected.min(perm.options.len() - 1)));
    }
    frame.render_stateful_widget(list, list_area, &mut state);

    let hint = Paragraph::new(Span::styled(
        "↑/↓ select · Enter confirm · 1-9 quick pick · Esc reject",
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(hint, hint_area);
}

/// Compute a centered popup rect sized to fit the option list, clamped to
/// the available `area`.
fn centered_rect(area: Rect, perm: &PendingPermission) -> Rect {
    // 2 borders + header (2) + hint (1) + one row per option.
    let desired_h = perm.options.len() as u16 + 5;
    let height = desired_h.min(area.height).max(5);
    let width = (area.width as f32 * 0.6) as u16;
    let width = width.clamp(20.min(area.width), area.width);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use crate::app::PendingPermission;
    use crate::ui::test_support::{render_to_string, test_app};
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot;

    fn pending(tool: &str, options: Vec<(&str, &str, &str)>, selected: usize) -> PendingPermission {
        let (tx, _rx) = oneshot::channel::<Option<String>>();
        PendingPermission {
            tool_name: tool.into(),
            options: options
                .into_iter()
                .map(|(a, b, c)| (a.into(), b.into(), c.into()))
                .collect(),
            reply: Arc::new(Mutex::new(Some(tx))),
            selected,
        }
    }

    #[test]
    fn popup_shows_tool_name_and_all_options() {
        let mut app = test_app();
        app.permission_pending = Some(pending(
            "write_file",
            vec![
                ("a1", "Allow once", "allow once"),
                ("a2", "Allow always", "allow always"),
                ("r1", "Reject", "reject once"),
            ],
            0,
        ));
        let text = render_to_string(&app, 80, 20);
        assert!(
            text.contains("Permission required"),
            "title missing:\n{text}"
        );
        assert!(text.contains("write_file"), "tool missing:\n{text}");
        assert!(text.contains("Allow once"), "opt1 missing:\n{text}");
        assert!(text.contains("Allow always"), "opt2 missing:\n{text}");
        assert!(text.contains("Reject"), "opt3 missing:\n{text}");
    }

    #[test]
    fn popup_renders_key_hint() {
        let mut app = test_app();
        app.permission_pending = Some(pending("rm", vec![("a", "Allow", "allow once")], 0));
        let text = render_to_string(&app, 80, 20);
        assert!(text.contains("Enter confirm"), "hint missing:\n{text}");
    }
}
