use anyhow::Result;
use clap::Parser;
use crossterm::{
    cursor,
    event::{Event, EventStream, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{io, path::PathBuf, process::Stdio};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use fleet_commander_core::container;
use fleet_commander_core::session::SessionEvent;

mod agent;
mod agent_kind;
mod app;
mod change_source;
mod cli;
mod completion;
mod config;
mod embedded_agent;
mod event;
mod explorer;
mod init;
mod keybind;
mod markdown;
mod mcp_server;
mod ui;
mod workspace;

use crate::app::App;
use crate::change_source::{ChangeSource, ChangeSourceHandle, FsWatcher};
use crate::config::Config;
use crate::event::AppEvent;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::Cli::parse();

    match cli.command {
        Some(cli::Command::Init { path }) => {
            init::run(&path)?;
            return Ok(());
        }
        None => {}
    }

    // Default: launch TUI.
    init_logging();
    run_tui(cli.acp_log, cli.acp_log_filter).await
}

/// Initialize file-based logging via `tracing`.
///
/// Logs go to `~/.local/share/fleet-commander/fleet-commander.log`.
/// The TUI owns stdout, so we never log there.
fn init_logging() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let log_dir = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("fleet-commander");
    let _ = std::fs::create_dir_all(&log_dir);

    let file_appender = tracing_appender::rolling::never(&log_dir, "fleet-commander.log");

    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(file_appender)
                .with_ansi(false)
                .with_target(true)
                .with_thread_ids(false),
        )
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    info!("Fleet Commander starting");
}

async fn run_tui(acp_log_path: Option<PathBuf>, acp_log_filter: Option<String>) -> Result<()> {
    let config = load_config_or_default();
    install_panic_hook();
    // Write any embedded (release-bundled) fleet-agent binaries to disk so the
    // container mount + launcher can pick the right arch. No-op otherwise.
    embedded_agent::install_embedded_agents();
    let mut terminal = setup_terminal()?;

    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();
    let (runtime_tx, runtime_rx) = mpsc::unbounded_channel::<SessionEvent>();
    spawn_runtime_forwarder(runtime_rx, tx.clone());

    // Load persisted workspaces — no more hardcoded mock agents.
    let saved = workspace::load();
    info!(count = saved.len(), "Loaded workspaces");
    let agents = workspace::to_agents(&saved);

    let acp_log = open_acp_log(acp_log_path.as_deref())?;
    if let Some(ref pattern) = acp_log_filter {
        info!(pattern = %pattern, "ACP log filter active");
    }

    let mut app = App::with_acp_log(
        config,
        agents,
        tx.clone(),
        runtime_tx,
        acp_log,
        acp_log_filter,
    );

    let mut input_task = spawn_input_task(tx.clone());
    let _change_handle = start_default_change_source(tx.clone())?;

    let mcp_addr = "127.0.0.1:6100";
    let _mcp_handle = mcp_server::start_mcp_server(mcp_addr, tx.clone()).await?;

    let result = run(&mut terminal, &mut app, &mut rx, &mut input_task, &tx).await;

    // Teardown TUI first so shutdown messages go to the normal terminal.
    teardown_terminal(&mut terminal)?;

    // Gracefully stop agent tasks and their containers.
    shutdown_agents(&mut app).await;

    result
}

/// Gracefully shut down all agents: abort background tasks and stop containers.
async fn shutdown_agents(app: &mut App) {
    info!("Shutting down agents");

    // Abort all running agent tasks first.
    for agent in &mut app.agents {
        if let Some(handle) = agent.task_handle.take() {
            handle.abort();
        }
        agent.prompt_tx = None;
    }

    // Stop containers for workspace agents (in parallel).
    let workspaces: Vec<PathBuf> = app
        .agents
        .iter()
        .filter_map(|a| a.workspace_folder.clone())
        .collect();

    if workspaces.is_empty() {
        return;
    }

    let count = workspaces.len();
    eprintln!("Stopping {count} container(s)…");
    info!(count, "Stopping containers");
    let futures: Vec<_> = workspaces
        .into_iter()
        .map(|ws| async move {
            let name = ws.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");
            if let Err(e) = container::stop_workspace_container(&ws).await {
                warn!(workspace = %ws.display(), error = %e, "Failed to stop container");
                eprintln!("  ⚠ {name}: failed to stop ({e})");
            } else {
                info!(workspace = %ws.display(), "Container stopped");
                eprintln!("  ✓ {name}: stopped");
            }
        })
        .collect();
    futures_util::future::join_all(futures).await;
}

fn load_config_or_default() -> Config {
    let path = PathBuf::from("config/default.toml");
    match Config::load(&path) {
        Ok(cfg) => {
            info!(path = %path.display(), "Loaded config");
            cfg
        }
        Err(err) => {
            warn!(path = %path.display(), error = %err, "Falling back to default config");
            Config::default()
        }
    }
}

/// Open the ACP wire-log file in append mode if a path was supplied via
/// `--acp-log`. Returns a shared, lockable handle so every agent task writes
/// into the same file without interleaving lines.
fn open_acp_log(
    path: Option<&std::path::Path>,
) -> Result<Option<std::sync::Arc<std::sync::Mutex<std::fs::File>>>> {
    let Some(path) = path else { return Ok(None) };
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    info!(path = %path.display(), "ACP wire log enabled");
    Ok(Some(std::sync::Arc::new(std::sync::Mutex::new(file))))
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
    execute!(stdout, EnterAlternateScreen, cursor::Hide)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn teardown_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, cursor::Show)?;
    Ok(())
}

/// Pump `SessionEvent`s from the core runtime into the TUI's `AppEvent`
/// channel. The runtime crate is frontend-agnostic, so this small adapter
/// is where we wrap its events into our application loop.
fn spawn_runtime_forwarder(
    mut rx: mpsc::UnboundedReceiver<SessionEvent>,
    tx: mpsc::UnboundedSender<AppEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if tx.send(AppEvent::Session(event)).is_err() {
                break;
            }
        }
    })
}

fn spawn_input_task(tx: mpsc::UnboundedSender<AppEvent>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut events = EventStream::new();
        while let Some(event) = events.next().await {
            let Ok(event) = event else { break };
            match event {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    if tx.send(AppEvent::Input(key)).is_err() {
                        break;
                    }
                }
                Event::Resize(_, _) if tx.send(AppEvent::Repaint).is_err() => {
                    // The terminal was resized — wake the event loop so it
                    // re-renders at the new dimensions. Without this the
                    // visible frame is stale until the user types something.
                    break;
                }
                _ => {}
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
    input_task: &mut tokio::task::JoinHandle<()>,
    tx: &mpsc::UnboundedSender<AppEvent>,
) -> Result<()> {
    terminal.draw(|f| ui::render(f, app))?;
    while let Some(event) = rx.recv().await {
        app.handle(event);
        if app.should_quit {
            break;
        }
        // If auth is needed, suspend the TUI and run the login command interactively.
        if let Some((agent_id, command)) = app.auth_pending.take() {
            run_auth_terminal(terminal, &command, input_task, tx, rx).await?;
            // After auth completes, reconnect the agent.
            app.ensure_agent_connected(agent_id);
        }
        terminal.draw(|f| ui::render(f, app))?;
    }
    Ok(())
}

/// Suspend the TUI and run an auth command with full interactive I/O.
///
/// Leaves the alternate screen, disables raw mode, aborts the crossterm
/// `EventStream` so it stops consuming stdin (otherwise the spawned child
/// would compete with the TUI for keystrokes and terminal response bytes —
/// causing errors like "The cursor position could not be read within a
/// normal duration"), spawns the command with inherited stdin/stdout/stderr
/// so the user can complete the login flow, then restores the TUI and
/// respawns the input task.
async fn run_auth_terminal(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    command: &[String],
    input_task: &mut tokio::task::JoinHandle<()>,
    tx: &mpsc::UnboundedSender<AppEvent>,
    rx: &mut mpsc::UnboundedReceiver<AppEvent>,
) -> Result<()> {
    // Stop the input task so its `EventStream` releases stdin. Wait for the
    // task to fully finish so the underlying crossterm reader thread is no
    // longer polling stdin before we hand it to the child process.
    input_task.abort();
    let _ = (&mut *input_task).await;

    // Leave TUI mode so the user gets a normal terminal.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, cursor::Show)?;

    let (program, args) = command.split_first().expect("auth command cannot be empty");

    eprintln!("\n── Fleet Commander: Authentication ──");
    eprintln!("Running: {}\n", command.join(" "));

    let status = std::process::Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();

    match status {
        Ok(s) if s.success() => {
            info!("Auth command completed successfully");
            eprintln!("\n✓ Auth command completed successfully. Resuming...\n");
        }
        Ok(s) => {
            let code = s.code().unwrap_or(-1);
            warn!(exit_code = code, "Auth command exited with non-zero code");
            eprintln!("\n⚠ Auth command exited with code {code}. Resuming...\n",);
        }
        Err(e) => {
            error!(error = %e, "Failed to run auth command");
            eprintln!("\n✗ Failed to run auth command: {e}. Resuming...\n");
        }
    }

    // Brief pause so the user can see the status.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Restore TUI mode.
    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen, cursor::Hide)?;
    terminal.clear()?;

    // Drain any stray input events that may have arrived during the auth
    // flow (e.g. terminal response bytes the child didn't consume) so they
    // don't get interpreted as keystrokes in the TUI.
    while rx.try_recv().is_ok() {}

    // Respawn the input task so the TUI receives keystrokes again.
    *input_task = spawn_input_task(tx.clone());

    Ok(())
}
