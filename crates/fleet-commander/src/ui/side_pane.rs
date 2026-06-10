//! Right-hand side panel — currently the diff viewer.

use ratatui::{
    Frame,
    layout::Rect,
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::app::SidePane;
use crate::ui::style::border_style;
use crate::ui::syntax::highlight_for_path;

pub fn render(frame: &mut Frame<'_>, area: Rect, pane: &SidePane, focused: bool) {
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
    }
}
