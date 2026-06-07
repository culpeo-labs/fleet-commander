//! Lightweight Markdown → ratatui `Line` renderer.
//!
//! Converts Markdown text into styled `Line`s for the conversation pane.
//! Supports: headings, bold, italic, code spans, fenced code blocks (with
//! syntax highlighting via syntect), blockquotes, and lists.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::sync::LazyLock;
use syntect::{easy::HighlightLines, highlighting::ThemeSet, parsing::SyntaxSet};

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// Render markdown text into a vector of styled lines for the TUI.
pub fn render_markdown(text: &str) -> Vec<Line<'static>> {
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let parser = Parser::new_ext(text, options);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut style_stack: Vec<Style> = vec![Style::default()];
    let mut in_code_block = false;
    let mut code_block_lang = String::new();
    let mut code_block_buf = String::new();
    let mut list_depth: usize = 0;
    let mut list_item_started = false;
    let mut in_heading = false;
    let mut blockquote_depth: usize = 0;

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    flush_line(&mut current_spans, &mut lines);
                    let marker = "#".repeat(level as usize);
                    current_spans.push(Span::styled(
                        format!("{marker} "),
                        Style::default()
                            .fg(Color::Magenta)
                            .add_modifier(Modifier::BOLD),
                    ));
                    style_stack.push(
                        Style::default()
                            .fg(Color::Magenta)
                            .add_modifier(Modifier::BOLD),
                    );
                    in_heading = true;
                }
                Tag::Emphasis => {
                    style_stack.push(current_style(&style_stack).add_modifier(Modifier::ITALIC));
                }
                Tag::Strong => {
                    style_stack.push(current_style(&style_stack).add_modifier(Modifier::BOLD));
                }
                Tag::Strikethrough => {
                    style_stack
                        .push(current_style(&style_stack).add_modifier(Modifier::CROSSED_OUT));
                }
                Tag::CodeBlock(kind) => {
                    flush_line(&mut current_spans, &mut lines);
                    in_code_block = true;
                    code_block_buf.clear();
                    code_block_lang = match kind {
                        CodeBlockKind::Fenced(lang) => lang.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                }
                Tag::List(_) => {
                    list_depth += 1;
                }
                Tag::Item => {
                    flush_line(&mut current_spans, &mut lines);
                    let indent = "  ".repeat(list_depth.saturating_sub(1));
                    current_spans.push(Span::styled(
                        format!("{indent}• "),
                        Style::default().fg(Color::Yellow),
                    ));
                    list_item_started = true;
                }
                Tag::BlockQuote(_) => {
                    blockquote_depth += 1;
                }
                Tag::Paragraph => {
                    if !list_item_started {
                        flush_line(&mut current_spans, &mut lines);
                    }
                }
                Tag::Link { dest_url, .. } => {
                    // We'll render the link text, then append the URL after.
                    style_stack.push(
                        Style::default()
                            .fg(Color::Blue)
                            .add_modifier(Modifier::UNDERLINED),
                    );
                    // Store URL for later — push a marker we'll use in End.
                    // For simplicity, just style the text as a link.
                    let _ = dest_url;
                }
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Heading(_) => {
                    style_stack.pop();
                    in_heading = false;
                    flush_line(&mut current_spans, &mut lines);
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                    style_stack.pop();
                }
                TagEnd::CodeBlock => {
                    // Render the code block with syntax highlighting.
                    render_code_block(&code_block_lang, &code_block_buf, &mut lines);
                    in_code_block = false;
                }
                TagEnd::List(_) => {
                    list_depth = list_depth.saturating_sub(1);
                }
                TagEnd::Item => {
                    flush_line(&mut current_spans, &mut lines);
                    list_item_started = false;
                }
                TagEnd::BlockQuote(_) => {
                    blockquote_depth = blockquote_depth.saturating_sub(1);
                }
                TagEnd::Paragraph => {
                    flush_line(&mut current_spans, &mut lines);
                    // Add blank line after paragraphs (unless in a list item).
                    if !list_item_started {
                        lines.push(Line::from(""));
                    }
                }
                _ => {}
            },
            Event::Text(text) => {
                if in_code_block {
                    code_block_buf.push_str(&text);
                } else {
                    let style = current_style(&style_stack);
                    // Handle text that contains newlines.
                    let parts: Vec<&str> = text.split('\n').collect();
                    for (i, part) in parts.iter().enumerate() {
                        if blockquote_depth > 0 && (i > 0 || current_spans.is_empty()) {
                            let prefix = "│ ".repeat(blockquote_depth);
                            current_spans.push(Span::styled(
                                prefix,
                                Style::default().fg(Color::DarkGray),
                            ));
                        }
                        if !part.is_empty() {
                            current_spans.push(Span::styled(part.to_string(), style));
                        }
                        if i < parts.len() - 1 {
                            flush_line(&mut current_spans, &mut lines);
                        }
                    }
                }
            }
            Event::Code(code) => {
                current_spans.push(Span::styled(
                    format!("`{code}`"),
                    Style::default().fg(Color::Rgb(209, 154, 102)), // warm orange for inline code
                ));
            }
            Event::SoftBreak => {
                current_spans.push(Span::raw(" "));
            }
            Event::HardBreak => {
                flush_line(&mut current_spans, &mut lines);
            }
            Event::Rule => {
                flush_line(&mut current_spans, &mut lines);
                lines.push(Line::from(Span::styled(
                    "─".repeat(40),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            _ => {}
        }
    }

    // Flush any remaining content.
    flush_line(&mut current_spans, &mut lines);

    // Remove trailing empty lines.
    while lines.last().is_some_and(|l| l.spans.is_empty() || l.to_string().is_empty()) {
        lines.pop();
    }

    // If rendering produced nothing meaningful, treat as plain text.
    if lines.is_empty() && !text.is_empty() {
        return text
            .lines()
            .map(|l| Line::from(l.to_string()))
            .collect();
    }

    lines
}

fn current_style(stack: &[Style]) -> Style {
    stack.last().copied().unwrap_or_default()
}

fn flush_line(spans: &mut Vec<Span<'static>>, lines: &mut Vec<Line<'static>>) {
    if !spans.is_empty() {
        lines.push(Line::from(std::mem::take(spans)));
    }
}

fn render_code_block(lang: &str, code: &str, lines: &mut Vec<Line<'static>>) {
    // Header bar.
    let label = if lang.is_empty() {
        " code ".to_string()
    } else {
        format!(" {lang} ")
    };
    lines.push(Line::from(Span::styled(
        format!("┌─{label}─"),
        Style::default().fg(Color::DarkGray),
    )));

    // Try syntax highlighting.
    let syntax = if !lang.is_empty() {
        SYNTAX_SET.find_syntax_by_token(lang)
    } else {
        None
    };
    let syntax = syntax.unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let theme = THEME_SET
        .themes
        .get("base16-ocean.dark")
        .or_else(|| THEME_SET.themes.values().next())
        .expect("at least one theme is bundled");
    let mut highlighter = HighlightLines::new(syntax, theme);

    for line in code.lines() {
        let mut spans: Vec<Span<'static>> = vec![Span::styled(
            "│ ",
            Style::default().fg(Color::DarkGray),
        )];
        let ranges = highlighter
            .highlight_line(line, &SYNTAX_SET)
            .unwrap_or_default();
        for (style, text) in ranges {
            spans.push(Span::styled(
                text.to_string(),
                Style::default().fg(Color::Rgb(
                    style.foreground.r,
                    style.foreground.g,
                    style.foreground.b,
                )),
            ));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(Span::styled(
        "└─",
        Style::default().fg(Color::DarkGray),
    )));
}

/// Detect if a string looks like it contains markdown formatting.
pub fn looks_like_markdown(text: &str) -> bool {
    // Quick heuristic: check for common markdown patterns.
    text.contains("```")
        || text.contains("## ")
        || text.contains("**")
        || text.contains("- ")
        || text.contains("* ")
        || text.contains("> ")
        || text.contains("1. ")
        || text.contains("[](")
        || text.starts_with("# ")
}

/// Detect if text looks like JSON.
pub fn looks_like_json(text: &str) -> bool {
    let trimmed = text.trim();
    (trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passthrough() {
        let lines = render_markdown("Hello world");
        assert!(!lines.is_empty());
        let text: String = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("Hello world"));
    }

    #[test]
    fn heading_renders_with_marker() {
        let lines = render_markdown("# Title\n\nBody text");
        let text: String = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("# Title"));
        assert!(text.contains("Body text"));
    }

    #[test]
    fn code_block_renders_with_border() {
        let input = "```rust\nfn main() {}\n```";
        let lines = render_markdown(input);
        let text: String = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("rust"));
        assert!(text.contains("fn main()"));
        assert!(text.contains("│"));
    }

    #[test]
    fn list_renders_bullets() {
        let lines = render_markdown("- one\n- two\n- three");
        let text: String = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("•"));
        assert!(text.contains("one"));
        assert!(text.contains("three"));
    }

    #[test]
    fn inline_code_renders() {
        let lines = render_markdown("Use `foo()` here");
        let text: String = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("`foo()`"));
    }

    #[test]
    fn looks_like_markdown_detects_patterns() {
        assert!(looks_like_markdown("# Hello"));
        assert!(looks_like_markdown("Some **bold** text"));
        assert!(looks_like_markdown("```\ncode\n```"));
        assert!(!looks_like_markdown("plain text here"));
    }

    #[test]
    fn looks_like_json_detects_objects() {
        assert!(looks_like_json(r#"{"key": "value"}"#));
        assert!(looks_like_json(r#"[1, 2, 3]"#));
        assert!(!looks_like_json("not json"));
    }
}
