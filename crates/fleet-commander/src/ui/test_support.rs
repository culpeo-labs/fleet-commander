//! Test scaffolding shared by per-module render tests.
//!
//! `#[cfg(test)]`-only — none of this leaks into release builds.

use ratatui::{Terminal, backend::TestBackend};
use tokio::sync::mpsc;

use crate::agent::Agent;
use crate::app::App;
use crate::config::Config;

pub fn agents() -> Vec<Agent> {
    vec![Agent::new("a1", "First"), Agent::new("a2", "Second")]
}

pub fn test_app() -> App {
    let (tx, _rx) = mpsc::unbounded_channel();
    App::new(Config::default(), agents(), tx)
}

pub fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    let (cols, rows) = (buffer.area.width, buffer.area.height);
    let mut out = String::new();
    for y in 0..rows {
        for x in 0..cols {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

pub fn render_to_string(app: &App, cols: u16, rows: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap();
    terminal.draw(|f| super::render(f, app)).unwrap();
    buffer_text(&terminal)
}
