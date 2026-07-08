//! `fleet-agent` entry point.
//!
//! Two transports, so the daemon can outlive any single client:
//!
//! - `fleet-agent serve --root <path> [--socket <path>]` — without `--socket`,
//!   reads JSON-RPC over stdin/stdout (the original stdio transport). With
//!   `--socket`, binds a unix socket and serves one client connection at a
//!   time, looping back to accept the next when a client disconnects. This is
//!   how the persistent in-container daemon runs (started by the devcontainer
//!   `postStartCommand`).
//! - `fleet-agent bridge --socket <path>` — a thin relay that pipes this
//!   process's stdin/stdout to the daemon's unix socket. The host runs it via
//!   `docker exec -i` to reach the container-internal socket portably (works
//!   on native Docker and Docker Desktop alike).
//!
//! Args are hand-parsed (no `clap`) to keep the injected binary's
//! dependency footprint and cold-start as small as possible.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;

use fleet_agent::{DaemonState, Server};
use fleet_protocol::{
    McpBindParams, McpDataParams, McpTunnelParams, Notification, framing, methods,
};
use serde_json::{Value, to_vec};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("fleet-agent: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let mut iter = args.iter();
    let command = iter.next().map(String::as_str);
    match command {
        Some("serve") => {
            let opts = parse_serve(iter)?;
            match opts.socket {
                Some(socket) => serve_socket(&opts.root, &socket),
                None => serve_stdio(&opts.root),
            }
        }
        Some("bridge") => {
            let socket = parse_socket(iter)?;
            bridge(&socket)
        }
        Some("mcp") => {
            let opts = parse_mcp(iter)?;
            mcp_relay(&opts.socket, &opts.token)
        }
        Some("--help") | Some("-h") | None => {
            eprintln!(
                "usage:\n  \
                 fleet-agent serve --root <path> [--socket <path>]\n  \
                 fleet-agent bridge --socket <path>\n  \
                 fleet-agent mcp --socket <path> --token <token>"
            );
            Ok(())
        }
        Some(other) => Err(format!("unknown command: {other}")),
    }
}

/// Serve a single client over stdin/stdout (the original transport).
fn serve_stdio(root: &Path) -> Result<(), String> {
    let server = Server::new(root.to_path_buf());
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    // Pass owned `Stdout` (which is `Send + 'static`) so the watch writer
    // thread can own the sink; `StdoutLock` is not `'static`.
    server
        .serve_stdio(&mut reader, io::stdout())
        .map_err(|e| format!("serve loop failed: {e}"))
}

/// Bind a unix socket and serve connected clients one at a time. Each client
/// gets a fresh serve loop; when it disconnects we loop back and accept the
/// next, so the daemon survives a client (TUI) restart.
fn serve_socket(root: &Path, socket: &Path) -> Result<(), String> {
    // Idempotent start: if a daemon is already listening here, do nothing.
    // This lets `postStartCommand` launch us unconditionally on every container
    // start without risking a second daemon stealing the socket from a live one
    // (which would orphan an in-flight ACP child).
    if socket.exists() {
        if UnixStream::connect(socket).is_ok() {
            eprintln!(
                "fleet-agent: daemon already listening on {}",
                socket.display()
            );
            return Ok(());
        }
        // Stale socket from a crashed daemon; remove it so `bind` can succeed.
        let _ = std::fs::remove_file(socket);
    }
    if let Some(parent) = socket.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let listener =
        UnixListener::bind(socket).map_err(|e| format!("bind {}: {e}", socket.display()))?;
    eprintln!("fleet-agent: listening on {}", socket.display());

    // Daemon-scoped state shared by every connection. Its session registry is
    // what lets an ACP session outlive the client (TUI) that started it: a
    // reconnecting client reattaches to the same session instead of respawning.
    let state = DaemonState::new();

    // One thread per client connection. A single session holds *two* concurrent
    // connections (the fs/watch channel and the ACP session channel), so the
    // daemon must serve them in parallel rather than one-at-a-time. Each
    // connection gets its own `Server` for watch/ACP/search state, but they
    // share the daemon-scoped session registry via `DaemonState`.
    for conn in listener.incoming() {
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                eprintln!("fleet-agent: accept failed: {e}");
                continue;
            }
        };
        let root = root.to_path_buf();
        let state = state.clone();
        thread::spawn(move || {
            let server = Server::with_state(root, &state);
            let read_half = match conn.try_clone() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("fleet-agent: clone socket failed: {e}");
                    return;
                }
            };
            let mut reader = BufReader::new(read_half);
            if let Err(e) = server.serve_stdio(&mut reader, conn) {
                eprintln!("fleet-agent: client serve loop ended: {e}");
            }
        });
    }
    Ok(())
}

/// Relay this process's stdin/stdout to the daemon's unix socket. Runs until
/// either direction hits EOF, then tears the other down by closing the socket.
fn bridge(socket: &Path) -> Result<(), String> {
    let stream =
        UnixStream::connect(socket).map_err(|e| format!("connect {}: {e}", socket.display()))?;
    let mut to_socket = stream
        .try_clone()
        .map_err(|e| format!("clone socket: {e}"))?;
    let mut from_socket = stream;

    // stdin -> socket
    let up = thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let _ = io::copy(&mut stdin, &mut to_socket);
        // Signal the daemon we're done writing so its read side sees EOF.
        let _ = to_socket.shutdown(std::net::Shutdown::Write);
    });

    // socket -> stdout (drives termination: when the daemon closes, we exit)
    let mut stdout = io::stdout().lock();
    let _ = copy_all(&mut from_socket, &mut stdout);

    // The daemon side closed; drop our read half so the stdin pump unblocks.
    let _ = from_socket.shutdown(std::net::Shutdown::Both);
    let _ = up.join();
    Ok(())
}

/// Like `io::copy` but flushes the writer as data arrives, so framed responses
/// reach the host promptly rather than sitting in a buffer.
fn copy_all<R: Read, W: Write>(reader: &mut R, writer: &mut W) -> io::Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        writer.write_all(&buf[..n])?;
        writer.flush()?;
    }
}

/// Relay an MCP stdio server (spawned inside the container by the coding agent
/// via the ACP `session/new` `mcp_servers`) to the fleet-agent daemon, which
/// tunnels it out to the host over the session connection (Feature 2).
///
/// MCP's stdio transport is newline-delimited JSON: one JSON-RPC message per
/// line. We translate between that and the daemon's `Content-Length`-framed
/// `mcp.*` notifications:
///
/// - `stdin` (agent → host): each MCP line is parsed and wrapped in an
///   [`methods::MCP_DATA`] frame written to the socket.
/// - `socket` (host → agent): each [`methods::MCP_DATA`] frame is unwrapped and
///   the MCP message is written to `stdout` as one line; an
///   [`methods::MCP_CLOSE`] frame ends the relay.
///
/// A one-shot [`methods::MCP_BIND`] frame announces the daemon-minted `token`
/// first so the daemon can resolve which session's host to bridge to.
fn mcp_relay(socket: &Path, token: &str) -> Result<(), String> {
    let stream =
        UnixStream::connect(socket).map_err(|e| format!("connect {}: {e}", socket.display()))?;
    let mut to_socket = stream
        .try_clone()
        .map_err(|e| format!("clone socket: {e}"))?;
    let from_socket = stream;

    // Announce ourselves before relaying any MCP traffic.
    let bind = Notification::new(
        methods::MCP_BIND,
        McpBindParams {
            token: token.into(),
        },
    );
    framing::write_frame(&mut to_socket, &to_vec(&bind).unwrap_or_default())
        .map_err(|e| format!("write mcp.bind: {e}"))?;

    // stdin (MCP requests from the agent) → socket as mcp.data frames. Detached:
    // this thread may block indefinitely in `read_line` when the tunnel is torn
    // down host-side, so we never `join` it — the process exit reaps it.
    thread::spawn(move || {
        let stdin = io::stdin();
        let mut reader = BufReader::new(stdin.lock());
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    if trimmed.is_empty() {
                        continue;
                    }
                    let Ok(message) = serde_json::from_str::<Value>(trimmed) else {
                        // Skip anything that isn't a JSON message rather than
                        // corrupting the tunnel.
                        continue;
                    };
                    let note = Notification::new(
                        methods::MCP_DATA,
                        McpDataParams {
                            tunnel_id: 0,
                            message,
                        },
                    );
                    if framing::write_frame(&mut to_socket, &to_vec(&note).unwrap_or_default())
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        // Agent's MCP client closed stdin → tell the daemon the tunnel is done,
        // then close the socket so our read loop below unblocks and we exit.
        let close = Notification::new(methods::MCP_CLOSE, McpTunnelParams { tunnel_id: 0 });
        let _ = framing::write_frame(&mut to_socket, &to_vec(&close).unwrap_or_default());
        let _ = to_socket.shutdown(std::net::Shutdown::Both);
    });

    // socket (host → agent) → stdout as MCP lines. Returning from this loop ends
    // the relay; any still-blocked stdin thread is reaped on process exit.
    let mut reader = BufReader::new(from_socket);
    let mut stdout = io::stdout().lock();
    while let Ok(Some(body)) = framing::read_frame(&mut reader) {
        let Ok(note) = serde_json::from_slice::<Notification>(&body) else {
            continue;
        };
        match note.method.as_str() {
            methods::MCP_DATA => {
                if let Some(params) = note
                    .params
                    .and_then(|p| serde_json::from_value::<McpDataParams>(p).ok())
                {
                    // MCP stdio framing: one compact JSON object per line.
                    if writeln!(stdout, "{}", params.message).is_err() {
                        break;
                    }
                    let _ = stdout.flush();
                }
            }
            methods::MCP_CLOSE => break,
            _ => {}
        }
    }

    Ok(())
}

struct ServeOpts {
    root: PathBuf,
    socket: Option<PathBuf>,
}

/// Parse `serve` args: `--root <path>` (defaults to cwd) and optional
/// `--socket <path>`. Both accept the `--flag=value` form too.
fn parse_serve<'a>(mut iter: impl Iterator<Item = &'a String>) -> Result<ServeOpts, String> {
    let mut root: Option<PathBuf> = None;
    let mut socket: Option<PathBuf> = None;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--root" => {
                let value = iter.next().ok_or("--root requires a value")?;
                root = Some(PathBuf::from(value));
            }
            other if other.starts_with("--root=") => {
                root = Some(PathBuf::from(&other["--root=".len()..]));
            }
            "--socket" => {
                let value = iter.next().ok_or("--socket requires a value")?;
                socket = Some(PathBuf::from(value));
            }
            other if other.starts_with("--socket=") => {
                socket = Some(PathBuf::from(&other["--socket=".len()..]));
            }
            other => return Err(format!("unexpected argument: {other}")),
        }
    }
    let root = match root {
        Some(r) => r,
        None => std::env::current_dir().map_err(|e| format!("cannot read cwd: {e}"))?,
    };
    Ok(ServeOpts { root, socket })
}

/// Parse a required `--socket <path>` (used by `bridge`).
fn parse_socket<'a>(mut iter: impl Iterator<Item = &'a String>) -> Result<PathBuf, String> {
    let mut socket: Option<PathBuf> = None;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket" => {
                let value = iter.next().ok_or("--socket requires a value")?;
                socket = Some(PathBuf::from(value));
            }
            other if other.starts_with("--socket=") => {
                socket = Some(PathBuf::from(&other["--socket=".len()..]));
            }
            other => return Err(format!("unexpected argument: {other}")),
        }
    }
    socket.ok_or_else(|| "bridge requires --socket <path>".to_string())
}

struct McpOpts {
    socket: PathBuf,
    token: String,
}

/// Parse `mcp` args: required `--socket <path>` and `--token <token>` (both
/// also accept the `--flag=value` form).
fn parse_mcp<'a>(mut iter: impl Iterator<Item = &'a String>) -> Result<McpOpts, String> {
    let mut socket: Option<PathBuf> = None;
    let mut token: Option<String> = None;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket" => {
                let value = iter.next().ok_or("--socket requires a value")?;
                socket = Some(PathBuf::from(value));
            }
            other if other.starts_with("--socket=") => {
                socket = Some(PathBuf::from(&other["--socket=".len()..]));
            }
            "--token" => {
                let value = iter.next().ok_or("--token requires a value")?;
                token = Some(value.clone());
            }
            other if other.starts_with("--token=") => {
                token = Some(other["--token=".len()..].to_string());
            }
            other => return Err(format!("unexpected argument: {other}")),
        }
    }
    Ok(McpOpts {
        socket: socket.ok_or("mcp requires --socket <path>")?,
        token: token.ok_or("mcp requires --token <token>")?,
    })
}
