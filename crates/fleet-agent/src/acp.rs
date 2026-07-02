//! ACP coding-agent relay for [`crate::Server`]: the `acp.start`/`acp.send`/
//! `acp.stop` handlers and the [`AcpChild`] that owns the spawned agent
//! process and tunnels its stdio through the daemon connection.
//!
//! ACP's stdio transport is newline-delimited JSON, so each child stdout line
//! is forwarded verbatim as an `acp.recv` notification and each inbound
//! `acp.send` notification is written as one line to the child's stdin. When
//! the child's stdout closes (it exited), an `acp.exit` notification is
//! emitted. stderr is forwarded as `acp.stderr` so device-code login URLs and
//! other diagnostics reach the operator.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use fleet_protocol::{
    AcpDataParams, AcpExitParams, AcpStartParams, AcpStartResult, AcpStopResult, Notification,
    Request, Response, RpcError, error_codes, methods,
};

use crate::Server;
use crate::util::{parse_params, to_vec_lossy};

impl Server {
    /// Spawn the ACP agent child described by [`AcpStartParams`] and begin
    /// tunnelling its stdio. If a child is already running, leaves it in place
    /// and reports `started: false`.
    pub(super) fn handle_acp_start(
        &self,
        req: &Request,
        out: &mpsc::Sender<Vec<u8>>,
        acp: &mut Option<AcpChild>,
    ) -> Response {
        let params: AcpStartParams = match parse_params(req) {
            Ok(p) => p,
            Err(e) => return Response::err(req.id, e),
        };
        if acp.is_some() {
            return Response::ok(req.id, AcpStartResult { started: false });
        }
        match AcpChild::spawn(&params, &self.root, out.clone()) {
            Ok(child) => {
                *acp = Some(child);
                Response::ok(req.id, AcpStartResult { started: true })
            }
            Err(e) => Response::err(
                req.id,
                RpcError::new(error_codes::INTERNAL_ERROR, format!("acp spawn: {e}")),
            ),
        }
    }
}

/// Write one line of ACP wire data to the running child's stdin. A missing
/// child or a broken pipe is ignored — the peer will observe the child's
/// absence via the (already sent or forthcoming) `acp.exit` notification.
pub(crate) fn handle_acp_send(note: &Notification, acp: &mut Option<AcpChild>) {
    let params: AcpDataParams = match note
        .params
        .clone()
        .and_then(|p| serde_json::from_value(p).ok())
    {
        Some(p) => p,
        None => return,
    };
    if let Some(child) = acp.as_mut() {
        let _ = child.write_line(&params.data);
    }
}

/// Terminate the running child, if any. Dropping the [`AcpChild`] kills the
/// process and joins its reader threads.
pub(crate) fn handle_acp_stop(req: &Request, acp: &mut Option<AcpChild>) -> Response {
    let stopped = acp.take().is_some();
    Response::ok(req.id, AcpStopResult { stopped })
}

/// Owns a spawned ACP agent process and the two threads reading its stdout and
/// stderr. Dropping it kills the child (which closes its pipes, letting the
/// reader threads reach EOF) and then joins the readers.
pub(crate) struct AcpChild {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout_reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
}

impl AcpChild {
    fn spawn(
        params: &AcpStartParams,
        root: &Path,
        out: mpsc::Sender<Vec<u8>>,
    ) -> std::io::Result<Self> {
        let mut argv = params.command.split_whitespace();
        let program = argv.next().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty acp command")
        })?;

        let mut cmd = Command::new(program);
        cmd.args(argv);
        cmd.current_dir(params.cwd.as_deref().map(Path::new).unwrap_or(root));
        for var in &params.env {
            cmd.env(&var.name, &var.value);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let stdout_reader = stdout.map(|stdout| {
            let out = out.clone();
            thread::Builder::new()
                .name("fleet-agent-acp-out".into())
                .spawn(move || {
                    let mut reader = BufReader::new(stdout);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let data = line.trim_end_matches(['\r', '\n']).to_string();
                                let note =
                                    Notification::new(methods::ACP_RECV, AcpDataParams { data });
                                if out.send(to_vec_lossy(&note)).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    // stdout closed → the child has exited (or is exiting).
                    let note = Notification::new(methods::ACP_EXIT, AcpExitParams { code: None });
                    let _ = out.send(to_vec_lossy(&note));
                })
                .ok()
        });

        let stderr_reader = stderr.map(|stderr| {
            let out = out.clone();
            thread::Builder::new()
                .name("fleet-agent-acp-err".into())
                .spawn(move || {
                    let mut reader = BufReader::new(stderr);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let data = line.trim_end_matches(['\r', '\n']).to_string();
                                let note =
                                    Notification::new(methods::ACP_STDERR, AcpDataParams { data });
                                if out.send(to_vec_lossy(&note)).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                })
                .ok()
        });

        Ok(Self {
            child,
            stdin,
            stdout_reader: stdout_reader.flatten(),
            stderr_reader: stderr_reader.flatten(),
        })
    }

    fn write_line(&mut self, data: &str) -> std::io::Result<()> {
        if let Some(stdin) = self.stdin.as_mut() {
            stdin.write_all(data.as_bytes())?;
            stdin.write_all(b"\n")?;
            stdin.flush()?;
        }
        Ok(())
    }
}

impl Drop for AcpChild {
    fn drop(&mut self) {
        // Drop stdin first so the child sees EOF on its input, then kill it to
        // guarantee the stdout/stderr pipes close, letting the reader threads
        // reach EOF. Only then join them.
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.stdout_reader.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_reader.take() {
            let _ = handle.join();
        }
    }
}
