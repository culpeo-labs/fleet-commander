//! Syntect-backed syntax highlighting for the diff side pane.

use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use std::path::Path;
use std::sync::LazyLock;
use syntect::{easy::HighlightLines, highlighting::ThemeSet, parsing::SyntaxSet};

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// Highlight `source` as if it were the contents of `path`. Falls back
/// to plain text when the extension isn't recognised.
pub fn highlight_for_path(source: &str, path: &Path) -> Vec<Line<'static>> {
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
