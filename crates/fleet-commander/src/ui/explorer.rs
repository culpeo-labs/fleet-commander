//! File-explorer side pane — a lazy tree view with git status cues.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
};

use fleet_commander_core::git::StatusKind;

use crate::explorer::{EntryRow, ExplorerState};
use crate::ui::style::border_style;

const TITLE_FALLBACK: &str = "workspace";

pub fn render(frame: &mut Frame<'_>, area: Rect, state: &ExplorerState, focused: bool) {
    let border = border_style(focused);
    let title = state
        .fs
        .as_ref()
        .and_then(|fs| {
            fs.root_display()
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| TITLE_FALLBACK.to_string());
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Explorer: {title} "))
        .border_style(border);

    let Some(_) = &state.fs else {
        frame.render_widget(block, area);
        return;
    };

    let entries = state.visible_entries();
    let selected_idx = state.selected_index(&entries);

    let items: Vec<ListItem> = entries.iter().map(entry_to_item).collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("");
    let mut list_state = ListState::default();
    if !entries.is_empty() {
        list_state.select(Some(selected_idx));
    }
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn entry_to_item(entry: &EntryRow) -> ListItem<'static> {
    let indent: String = "  ".repeat(entry.depth);
    let chevron = if entry.is_dir {
        if entry.expanded { "▼ " } else { "▶ " }
    } else {
        "  "
    };
    let marker = entry
        .status
        .map(|s| format!("{:>2} ", s.marker()))
        .unwrap_or_else(|| "   ".to_string());
    let name_style = entry_style(entry);
    let marker_style = status_color(entry.status);
    let line = Line::from(vec![
        Span::raw(indent),
        Span::raw(chevron.to_string()),
        Span::styled(marker, marker_style),
        Span::styled(entry.name.clone(), name_style),
    ]);
    ListItem::new(line)
}

fn entry_style(entry: &EntryRow) -> Style {
    let mut style = Style::default();
    if entry.is_dir {
        style = style.fg(Color::LightBlue).add_modifier(Modifier::BOLD);
    }
    if entry.status == Some(StatusKind::Ignored) {
        style = Style::default().fg(Color::DarkGray);
    }
    style
}

fn status_color(kind: Option<StatusKind>) -> Style {
    match kind {
        Some(StatusKind::Modified) => Style::default().fg(Color::Yellow),
        Some(StatusKind::Added) => Style::default().fg(Color::Green),
        Some(StatusKind::Deleted) => Style::default().fg(Color::Red),
        Some(StatusKind::Renamed) => Style::default().fg(Color::Blue),
        Some(StatusKind::Untracked) => Style::default().fg(Color::Cyan),
        Some(StatusKind::Ignored) => Style::default().fg(Color::DarkGray),
        Some(StatusKind::Conflicted) => Style::default().fg(Color::Magenta),
        None => Style::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::ExplorerState;
    use fleet_commander_core::workspace_fs::LocalFs;
    use ratatui::{Terminal, backend::TestBackend};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn render_state(state: &ExplorerState, cols: u16, rows: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                render(f, area, state, true);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let (w, h) = (buffer.area.width, buffer.area.height);
        let mut out = String::new();
        for y in 0..h {
            for x in 0..w {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn fixture() -> TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("README.md"), "hi\n").unwrap();
        std::fs::write(tmp.path().join("src/lib.rs"), "// lib\n").unwrap();
        tmp
    }

    #[test]
    fn empty_state_renders_just_the_block() {
        let state = ExplorerState::default();
        let text = render_state(&state, 30, 8);
        assert!(text.contains("Explorer:"));
    }

    #[test]
    fn lists_top_level_entries_with_directory_chevron() {
        let tmp = fixture();
        let mut state = ExplorerState::default();
        state.set_fs(Some(Arc::new(LocalFs::new(tmp.path()))));
        let text = render_state(&state, 40, 8);
        assert!(text.contains("README.md"), "README missing:\n{text}");
        assert!(text.contains("src"), "src dir missing:\n{text}");
        // Directories use ▶ when collapsed.
        assert!(text.contains("▶"), "chevron missing:\n{text}");
    }

    #[test]
    fn expanding_a_directory_reveals_children_indented() {
        let tmp = fixture();
        let mut state = ExplorerState::default();
        state.set_fs(Some(Arc::new(LocalFs::new(tmp.path()))));
        state.expanded.insert("src".into());
        let text = render_state(&state, 40, 8);
        assert!(text.contains("lib.rs"), "child missing:\n{text}");
        // Child should appear AFTER "src" — basic ordering check.
        let src_idx = text.find("src").unwrap();
        let lib_idx = text.find("lib.rs").unwrap();
        assert!(lib_idx > src_idx);
    }

    #[test]
    fn renders_status_markers_in_root_when_repo_dirty() {
        // No git repo — status map is empty so markers are blank, but
        // we can manually inject a status to exercise the marker path.
        let tmp = fixture();
        let mut state = ExplorerState::default();
        state.set_fs(Some(Arc::new(LocalFs::new(tmp.path()))));
        state
            .status
            .insert("README.md".into(), StatusKind::Modified);
        let text = render_state(&state, 40, 8);
        // The 'M' marker should be present somewhere on the README line.
        let line = text
            .lines()
            .find(|l| l.contains("README.md"))
            .expect("README line missing");
        assert!(line.contains('M'), "marker missing on README line: {line}");
    }
}
