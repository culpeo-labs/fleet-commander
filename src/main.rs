use anyhow::Result;
use crossterm::{
    event::{Event, EventStream, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{io, path::PathBuf};
use tokio::sync::mpsc;

mod agent;
mod agent_runtime;
mod app;
mod change_source;
mod completion;
mod config;
mod container;
mod event;
mod keybind;
mod mcp_server;
mod ui;
mod workspace;

use crate::app::App;
use crate::change_source::{ChangeSource, ChangeSourceHandle, FsWatcher};
use crate::config::Config;
use crate::event::AppEvent;

#[tokio::main]
async fn main() -> Result<()> {
    let config = load_config_or_default();
    install_panic_hook();
    let mut terminal = setup_terminal()?;

    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();

    // Load persisted workspaces — no more hardcoded mock agents.
    let saved = workspace::load();
    let agents = workspace::to_agents(&saved);

    let mut app = App::new(config, agents, tx.clone());

    let _input_task = spawn_input_task(tx.clone());
    let _change_handle = start_default_change_source(tx.clone())?;

    let mcp_addr = "127.0.0.1:6100";
    let _mcp_handle = mcp_server::start_mcp_server(mcp_addr, tx.clone()).await?;

    let result = run(&mut terminal, &mut app, &mut rx).await;

    teardown_terminal(&mut terminal)?;
    result
}

fn load_config_or_default() -> Config {
    let path = PathBuf::from("config/default.toml");
    match Config::load(&path) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("warning: falling back to default config: {err:#}");
            Config::default()
        }
    }
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original(info);
    }));
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn teardown_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn spawn_input_task(tx: mpsc::UnboundedSender<AppEvent>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut events = EventStream::new();
        while let Some(event) = events.next().await {
            let Ok(event) = event else { break };
            if let Event::Key(key) = event {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if tx.send(AppEvent::Input(key)).is_err() {
                    break;
                }
            }
        }
    })
}

fn start_default_change_source(tx: mpsc::UnboundedSender<AppEvent>) -> Result<ChangeSourceHandle> {
    let root = std::env::current_dir()?;
    let (change_tx, mut change_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(event) = change_rx.recv().await {
            if tx.send(AppEvent::Change(event)).is_err() {
                break;
            }
        }
    });
    let source: Box<dyn ChangeSource> = Box::new(FsWatcher::new(root));
    source.start(change_tx)
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    rx: &mut mpsc::UnboundedReceiver<AppEvent>,
) -> Result<()> {
    terminal.draw(|f| ui::render(f, app))?;
    while let Some(event) = rx.recv().await {
        app.handle(event);
        if app.should_quit {
            break;
        }
        terminal.draw(|f| ui::render(f, app))?;
    }
    Ok(())
}
