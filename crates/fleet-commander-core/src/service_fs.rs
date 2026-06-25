//! [`ServiceFs`] — a [`WorkspaceFs`] backed by a `fleet-agent` daemon
//! reached over JSON-RPC (see the `fleet-protocol` crate).
//!
//! This is the client half of the in-container service. In Phase 0 the
//! daemon is a local host child process ([`ProcessTransport`]); Phase 1
//! swaps in a `docker exec -i` transport without touching this file — the
//! [`Transport`] trait is the seam.
//!
//! The [`WorkspaceFs`] methods are synchronous (they're consulted on the
//! render path), so the transport performs a blocking request/response.
//! Calls are serialized through a mutex; a single daemon handles one
//! in-flight request at a time, which is ample for the explorer's needs.

use std::collections::HashMap;
use std::fmt::Debug;
use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use fleet_protocol::{
    FsListParams, FsListResult, FsReadParams, FsReadResult, FsWatchParams, GitBranchResult,
    GitStatusParams, GitStatusResult, Incoming, InitializeParams, InitializeResult, Notification,
    PROTOCOL_VERSION, Request, Response, RpcError, WireStatus, error_codes, framing, methods,
};
use serde_json::Value;

use crate::git::{StatusError, StatusKind};
use crate::workspace_fs::{DirEntry, WorkspaceFs};

/// What can go wrong issuing a single RPC.
#[derive(Debug)]
pub enum TransportError {
    /// The daemon returned a JSON-RPC error.
    Rpc(RpcError),
    /// A transport-level IO failure (broken pipe, spawn failure, …).
    Io(io::Error),
    /// A malformed/unexpected response that broke the protocol contract.
    Protocol(String),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::Rpc(e) => write!(f, "rpc error {}: {}", e.code, e.message),
            TransportError::Io(e) => write!(f, "io error: {e}"),
            TransportError::Protocol(m) => write!(f, "protocol error: {m}"),
        }
    }
}

impl std::error::Error for TransportError {}

/// A request/response channel to a `fleet-agent`. Abstracted so the
/// process-spawning client and an in-memory test double share one code
/// path, and so Phase 1's `docker exec` transport drops straight in.
pub trait Transport: Send + Sync + Debug {
    /// Issue `method` with `params` and return the JSON-RPC `result`
    /// (or [`TransportError::Rpc`] when the daemon reports an error).
    fn call(&self, method: &str, params: Value) -> Result<Value, TransportError>;
}

/// A [`WorkspaceFs`] that proxies every operation to a `fleet-agent`.
#[derive(Debug)]
pub struct ServiceFs {
    root: PathBuf,
    transport: Box<dyn Transport>,
}

impl ServiceFs {
    /// Wrap an already-connected transport. `root` is used only for
    /// [`WorkspaceFs::root_display`]; all path semantics live server-side.
    pub fn new(root: impl Into<PathBuf>, transport: Box<dyn Transport>) -> Self {
        Self {
            root: root.into(),
            transport,
        }
    }

    /// Spawn `agent_bin serve --root <root>` as a child process, perform the
    /// `initialize` handshake, and return a connected `ServiceFs`.
    pub fn spawn(root: impl Into<PathBuf>, agent_bin: impl AsRef<Path>) -> io::Result<Self> {
        Self::spawn_watched(root, agent_bin, None)
    }

    /// Like [`spawn`](Self::spawn), but installs a live `fs.watch`
    /// subscription delivering [`Notification`]s to `sink` (see
    /// [`connect_docker_watched`](Self::connect_docker_watched)). Mainly used
    /// for the host-local transport in tests; production uses the docker path.
    pub fn spawn_watched(
        root: impl Into<PathBuf>,
        agent_bin: impl AsRef<Path>,
        sink: Option<NotificationSink>,
    ) -> io::Result<Self> {
        let root = root.into();
        let transport = ProcessTransport::spawn(agent_bin.as_ref(), &root, sink)?;
        Ok(Self::new(root, Box::new(transport)))
    }

    /// Connect to a `fleet-agent` running **inside a container** via
    /// `docker exec -i`, perform the `initialize` handshake, and return a
    /// connected `ServiceFs`.
    ///
    /// `root_display` is the host-side path shown to the user (so the
    /// explorer's root label stays stable across the `LocalFs` → `ServiceFs`
    /// upgrade); `remote_root` is the in-container workspace the daemon
    /// resolves paths against; `agent_path` is the in-container binary path
    /// (typically [`crate::agent_bin::CONTAINER_AGENT_PATH`]).
    pub fn connect_docker(
        root_display: impl Into<PathBuf>,
        remote_root: &str,
        container_id: &str,
        remote_user: &str,
        agent_path: &str,
    ) -> io::Result<Self> {
        Self::connect_docker_watched(
            root_display,
            remote_root,
            container_id,
            remote_user,
            agent_path,
            None,
        )
    }

    /// Like [`connect_docker`](Self::connect_docker), but installs a live
    /// `fs.watch` subscription: when the daemon advertises the capability,
    /// `sink` is invoked (on the transport's reader thread) for every
    /// [`Notification`] it pushes — primarily `fs.didChange`. Used by the
    /// explorer to refresh in response to in-container filesystem changes.
    pub fn connect_docker_watched(
        root_display: impl Into<PathBuf>,
        remote_root: &str,
        container_id: &str,
        remote_user: &str,
        agent_path: &str,
        sink: Option<NotificationSink>,
    ) -> io::Result<Self> {
        let transport = ProcessTransport::docker_exec(
            container_id,
            remote_user,
            agent_path,
            remote_root,
            sink,
        )?;
        Ok(Self::new(root_display, Box::new(transport)))
    }

    fn call_typed<P, R>(&self, method: &str, params: P) -> Result<R, TransportError>
    where
        P: serde::Serialize,
        R: serde::de::DeserializeOwned,
    {
        let params = serde_json::to_value(params)
            .map_err(|e| TransportError::Protocol(format!("serialize params: {e}")))?;
        let result = self.transport.call(method, params)?;
        serde_json::from_value(result)
            .map_err(|e| TransportError::Protocol(format!("decode result: {e}")))
    }
}

/// Map a wire status back into the explorer's [`StatusKind`].
fn from_wire(status: WireStatus) -> StatusKind {
    match status {
        WireStatus::Modified => StatusKind::Modified,
        WireStatus::Added => StatusKind::Added,
        WireStatus::Deleted => StatusKind::Deleted,
        WireStatus::Renamed => StatusKind::Renamed,
        WireStatus::Untracked => StatusKind::Untracked,
        WireStatus::Ignored => StatusKind::Ignored,
        WireStatus::Conflicted => StatusKind::Conflicted,
    }
}

/// Convert a transport error into an [`io::Error`] for the fs methods,
/// preserving "not found" so callers can distinguish it.
fn to_io_error(e: TransportError) -> io::Error {
    match e {
        TransportError::Io(io) => io,
        TransportError::Rpc(rpc) if rpc.code == error_codes::NOT_FOUND => {
            io::Error::new(io::ErrorKind::NotFound, rpc.message)
        }
        other => io::Error::other(other.to_string()),
    }
}

/// Render a workspace-relative path for the wire (forward slashes, `""`
/// for the root).
fn rel_to_wire(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
}

impl WorkspaceFs for ServiceFs {
    fn root_display(&self) -> &Path {
        &self.root
    }

    fn is_remote(&self) -> bool {
        true
    }

    fn list_dir(&self, rel: &Path) -> io::Result<Vec<DirEntry>> {
        let result: FsListResult = self
            .call_typed(
                methods::FS_LIST,
                FsListParams {
                    path: rel_to_wire(rel),
                },
            )
            .map_err(to_io_error)?;
        Ok(result
            .entries
            .into_iter()
            .map(|e| DirEntry {
                name: e.name,
                is_dir: e.is_dir,
            })
            .collect())
    }

    fn read_file(&self, rel: &Path) -> io::Result<Vec<u8>> {
        let result: FsReadResult = self
            .call_typed(
                methods::FS_READ,
                FsReadParams {
                    path: rel_to_wire(rel),
                },
            )
            .map_err(to_io_error)?;
        BASE64
            .decode(result.content_base64)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad base64: {e}")))
    }

    fn git_branch(&self) -> Option<String> {
        let result: GitBranchResult = self.call_typed(methods::GIT_BRANCH, Value::Null).ok()?;
        result.branch
    }

    fn git_status(
        &self,
        include_ignored: bool,
    ) -> Result<HashMap<PathBuf, StatusKind>, StatusError> {
        let result: GitStatusResult = self
            .call_typed(methods::GIT_STATUS, GitStatusParams { include_ignored })
            .map_err(|e| match e {
                // The daemon reports "not a repo" as a specific RPC error;
                // surface it as the same NonZeroExit the local backend would
                // produce so the UI falls back to showing no markers.
                TransportError::Rpc(rpc) if rpc.code == error_codes::NOT_A_REPO => {
                    StatusError::NonZeroExit {
                        code: None,
                        stderr: rpc.message,
                    }
                }
                TransportError::Io(io) => StatusError::SpawnFailed(io),
                other => StatusError::SpawnFailed(io::Error::other(other.to_string())),
            })?;
        Ok(result
            .entries
            .into_iter()
            .map(|e| (PathBuf::from(e.path), from_wire(e.status)))
            .collect())
    }
}

// ─── Process transport ─────────────────────────────────────────────────

/// How long a single RPC waits for the daemon to respond before the
/// transport gives up, tears the child down, and marks itself unhealthy.
/// Generous because operations like `git status` on a large repo are
/// legitimately slow — and, since all `ServiceFs` calls run off the UI
/// thread, a slow call delays only the explorer, never the TUI.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A [`Transport`] that talks to a spawned `fleet-agent` child over its
/// stdio pipes.
///
/// A dedicated reader thread owns the child's stdout and forwards framed
/// responses over a channel, so [`Transport::call`] can wait with a
/// deadline ([`mpsc::Receiver::recv_timeout`]) instead of blocking
/// forever on a read. On timeout, EOF, or IO error the transport is
/// marked **unhealthy**: the child is killed and every subsequent call
/// fails fast, so one wedged request can never permanently block the
/// explorer or leak a blocking-pool thread.
#[derive(Debug)]
pub struct ProcessTransport {
    call: Mutex<CallChannel>,
    child: Mutex<Child>,
    reader: Mutex<Option<JoinHandle<()>>>,
    next_id: AtomicU64,
    healthy: AtomicBool,
    timeout: Duration,
    /// Whether a [`NotificationSink`] was installed on the reader thread.
    has_sink: bool,
}

#[derive(Debug)]
struct CallChannel {
    stdin: ChildStdin,
    rx: mpsc::Receiver<io::Result<Vec<u8>>>,
}

/// A callback invoked for every server-initiated notification (e.g.
/// [`methods::FS_DID_CHANGE`]). Runs on the transport's reader thread, so it
/// must not block; the App uses it to post a non-blocking refresh event.
pub type NotificationSink = Box<dyn Fn(Notification) + Send>;

/// Read framed messages from the child's stdout until EOF or error. Each
/// frame is classified ([`Incoming::from_slice`]): **responses** are
/// forwarded to the call side (`tx`), while server-initiated
/// **notifications** are handed to `sink` (if any). This demux is what lets
/// the daemon push `fs.didChange` while requests are in flight without
/// breaking the "next frame is my response" invariant on the call side.
fn reader_loop(
    mut stdout: BufReader<ChildStdout>,
    tx: mpsc::Sender<io::Result<Vec<u8>>>,
    sink: Option<NotificationSink>,
) {
    loop {
        match framing::read_frame(&mut stdout) {
            Ok(Some(body)) => match Incoming::from_slice(&body) {
                Ok(Incoming::Notification(note)) => {
                    if let Some(sink) = &sink {
                        sink(note);
                    }
                }
                // Responses (and anything we can't classify as a
                // notification) go to the call side, which decodes and
                // validates the id. Forwarding the raw body keeps the
                // existing error handling unchanged.
                Ok(Incoming::Response(_)) | Err(_) => {
                    if tx.send(Ok(body)).is_err() {
                        break;
                    }
                }
            },
            Ok(None) => break, // EOF — the daemon closed stdout.
            Err(e) => {
                let _ = tx.send(Err(e));
                break;
            }
        }
    }
}

impl ProcessTransport {
    /// Spawn `agent_bin serve --root <root>` and complete the `initialize`
    /// handshake, verifying the daemon speaks our protocol version. When
    /// `sink` is provided and the daemon advertises the `watch` capability,
    /// a live `fs.watch` subscription is started (see
    /// [`from_command_with_timeout`](Self::from_command_with_timeout)).
    pub fn spawn(
        agent_bin: &Path,
        root: &Path,
        sink: Option<NotificationSink>,
    ) -> io::Result<Self> {
        let mut cmd = Command::new(agent_bin);
        cmd.arg("serve").arg("--root").arg(root);
        Self::from_command(cmd, sink)
    }

    /// Connect to a `fleet-agent` running inside a container by shelling
    /// `docker exec -i …` and complete the `initialize` handshake.
    ///
    /// `agent_path` and `remote_root` are paths **inside** the container.
    /// When `sink` is provided and the daemon advertises the `watch`
    /// capability, a live `fs.watch` subscription is started and its
    /// `fs.didChange` notifications are delivered to `sink`.
    pub fn docker_exec(
        container_id: &str,
        remote_user: &str,
        agent_path: &str,
        remote_root: &str,
        sink: Option<NotificationSink>,
    ) -> io::Result<Self> {
        let argv = docker_exec_argv(container_id, remote_user, agent_path, remote_root);
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        Self::from_command(cmd, sink)
    }

    fn from_command(cmd: Command, sink: Option<NotificationSink>) -> io::Result<Self> {
        Self::from_command_with_timeout(cmd, REQUEST_TIMEOUT, sink)
    }

    /// Spawn an already-configured command with piped stdio (stderr is
    /// discarded so daemon logs never corrupt the framed protocol stream or
    /// the host TUI), start the reader thread, then perform the `initialize`
    /// handshake with the given per-call `timeout`. When `sink` is set and
    /// the daemon supports it, also activate a live `fs.watch` subscription.
    fn from_command_with_timeout(
        mut cmd: Command,
        timeout: Duration,
        sink: Option<NotificationSink>,
    ) -> io::Result<Self> {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("agent stdin not captured"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("agent stdout not captured"))?;

        let (tx, rx) = mpsc::channel();
        let has_sink = sink.is_some();
        let reader = std::thread::Builder::new()
            .name("fleet-agent-reader".into())
            .spawn(move || reader_loop(BufReader::new(stdout), tx, sink))?;

        let transport = Self {
            call: Mutex::new(CallChannel { stdin, rx }),
            child: Mutex::new(child),
            reader: Mutex::new(Some(reader)),
            next_id: AtomicU64::new(0),
            healthy: AtomicBool::new(true),
            timeout,
            has_sink,
        };

        let init: InitializeResult = {
            let value = transport
                .call(
                    methods::INITIALIZE,
                    serde_json::to_value(InitializeParams {
                        protocol_version: PROTOCOL_VERSION,
                    })
                    .expect("serialize init params"),
                )
                .map_err(|e| io::Error::other(format!("initialize failed: {e}")))?;
            serde_json::from_value(value)
                .map_err(|e| io::Error::other(format!("bad initialize result: {e}")))?
        };
        if init.protocol_version != PROTOCOL_VERSION {
            return Err(io::Error::other(format!(
                "protocol version mismatch: agent={} client={PROTOCOL_VERSION}",
                init.protocol_version
            )));
        }

        // If the caller wants live updates and the daemon can watch, start
        // the subscription now. Best-effort: a watch failure must not sink
        // the whole connection (the explorer still works via polling).
        if transport.has_notification_sink() && init.capabilities.watch {
            let params = serde_json::to_value(FsWatchParams { enable: true })
                .expect("serialize fs.watch params");
            if let Err(e) = transport.call(methods::FS_WATCH, params) {
                tracing::warn!(error = %e, "fs.watch subscription failed; explorer falls back to polling");
            }
        }

        Ok(transport)
    }

    /// Whether a notification sink was wired in (i.e. the reader thread will
    /// deliver `fs.didChange`). Tracked separately because the sink itself is
    /// owned by the reader thread.
    fn has_notification_sink(&self) -> bool {
        self.has_sink
    }

    /// Mark the transport dead and tear down the child so no further call
    /// can block on it. Idempotent.
    fn mark_unhealthy(&self) {
        self.healthy.store(false, Ordering::Release);
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Build the `docker exec -i` argv that launches the in-container agent.
///
/// The daemon resolves all paths against `--root`, so no working directory is
/// set; `-i` keeps stdin open for the framed request/response stream.
pub fn docker_exec_argv(
    container_id: &str,
    remote_user: &str,
    agent_path: &str,
    remote_root: &str,
) -> Vec<String> {
    vec![
        "docker".into(),
        "exec".into(),
        "-i".into(),
        "-u".into(),
        remote_user.into(),
        container_id.into(),
        agent_path.into(),
        "serve".into(),
        "--root".into(),
        remote_root.into(),
    ]
}

impl Transport for ProcessTransport {
    fn call(&self, method: &str, params: Value) -> Result<Value, TransportError> {
        if !self.healthy.load(Ordering::Acquire) {
            return Err(TransportError::Io(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "agent transport is no longer healthy",
            )));
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let request = Request {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params: Some(params),
        };
        let body = serde_json::to_vec(&request)
            .map_err(|e| TransportError::Protocol(format!("encode request: {e}")))?;

        let mut chan = self
            .call
            .lock()
            .map_err(|_| TransportError::Protocol("transport mutex poisoned".into()))?;

        if let Err(e) =
            framing::write_frame(&mut chan.stdin, &body).and_then(|()| chan.stdin.flush())
        {
            drop(chan);
            self.mark_unhealthy();
            return Err(TransportError::Io(e));
        }

        // Calls are serialized by the `call` mutex, so the next frame from
        // the reader is unambiguously this request's response.
        let resp_body = match chan.rx.recv_timeout(self.timeout) {
            Ok(Ok(body)) => body,
            Ok(Err(e)) => {
                drop(chan);
                self.mark_unhealthy();
                return Err(TransportError::Io(e));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                drop(chan);
                self.mark_unhealthy();
                return Err(TransportError::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("agent did not respond within {:?}", self.timeout),
                )));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                drop(chan);
                self.mark_unhealthy();
                return Err(TransportError::Protocol(
                    "agent closed the connection".into(),
                ));
            }
        };

        let response: Response = serde_json::from_slice(&resp_body)
            .map_err(|e| TransportError::Protocol(format!("decode response: {e}")))?;

        if response.id != id {
            return Err(TransportError::Protocol(format!(
                "response id mismatch: expected {id}, got {}",
                response.id
            )));
        }
        if let Some(error) = response.error {
            return Err(TransportError::Rpc(error));
        }
        Ok(response.result.unwrap_or(Value::Null))
    }
}

impl Drop for ProcessTransport {
    fn drop(&mut self) {
        // Best-effort shutdown. Drop only runs once every `Arc<ServiceFs>`
        // clone (including any held by an in-flight `spawn_blocking` call)
        // is gone, so no call can be holding the locks below.
        self.healthy.store(false, Ordering::Release);
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Ok(mut reader) = self.reader.lock()
            && let Some(handle) = reader.take()
        {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_protocol::{FsEntry, GitStatusEntry};
    use std::sync::Mutex as StdMutex;

    /// In-memory transport that records calls and replays canned responses,
    /// so the `WorkspaceFs` mapping logic is testable without a process.
    struct FakeTransport {
        responses: StdMutex<HashMap<String, Result<Value, RpcError>>>,
        calls: StdMutex<Vec<(String, Value)>>,
    }

    impl Debug for FakeTransport {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("FakeTransport")
        }
    }

    impl FakeTransport {
        fn new() -> Self {
            Self {
                responses: StdMutex::new(HashMap::new()),
                calls: StdMutex::new(Vec::new()),
            }
        }

        fn with(mut self, method: &str, result: impl serde::Serialize) -> Self {
            self.responses
                .get_mut()
                .unwrap()
                .insert(method.into(), Ok(serde_json::to_value(result).unwrap()));
            self
        }

        fn with_error(mut self, method: &str, error: RpcError) -> Self {
            self.responses
                .get_mut()
                .unwrap()
                .insert(method.into(), Err(error));
            self
        }
    }

    impl Transport for FakeTransport {
        fn call(&self, method: &str, params: Value) -> Result<Value, TransportError> {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_string(), params));
            match self.responses.lock().unwrap().get(method) {
                Some(Ok(v)) => Ok(v.clone()),
                Some(Err(e)) => Err(TransportError::Rpc(e.clone())),
                None => Err(TransportError::Protocol(format!(
                    "no canned response for {method}"
                ))),
            }
        }
    }

    fn service(transport: FakeTransport) -> ServiceFs {
        ServiceFs::new("/workspace", Box::new(transport))
    }

    #[test]
    fn list_dir_maps_entries() {
        let fs = service(FakeTransport::new().with(
            methods::FS_LIST,
            FsListResult {
                entries: vec![
                    FsEntry {
                        name: "src".into(),
                        is_dir: true,
                    },
                    FsEntry {
                        name: "README.md".into(),
                        is_dir: false,
                    },
                ],
            },
        ));
        let entries = fs.list_dir(Path::new("")).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "src");
        assert!(entries[0].is_dir);
        assert!(!entries[1].is_dir);
    }

    #[test]
    fn read_file_decodes_base64() {
        let fs = service(FakeTransport::new().with(
            methods::FS_READ,
            FsReadResult {
                content_base64: BASE64.encode(b"hello"),
            },
        ));
        assert_eq!(fs.read_file(Path::new("a.txt")).unwrap(), b"hello");
    }

    #[test]
    fn read_file_not_found_maps_to_io_not_found() {
        let fs = service(FakeTransport::new().with_error(
            methods::FS_READ,
            RpcError::new(error_codes::NOT_FOUND, "nope"),
        ));
        let err = fs.read_file(Path::new("missing")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn git_branch_returns_value_and_none_on_error() {
        let fs = service(FakeTransport::new().with(
            methods::GIT_BRANCH,
            GitBranchResult {
                branch: Some("main".into()),
            },
        ));
        assert_eq!(fs.git_branch(), Some("main".into()));

        let fs = service(FakeTransport::new());
        assert_eq!(fs.git_branch(), None);
    }

    #[test]
    fn git_status_maps_entries() {
        let fs = service(FakeTransport::new().with(
            methods::GIT_STATUS,
            GitStatusResult {
                entries: vec![
                    GitStatusEntry {
                        path: "src/lib.rs".into(),
                        status: WireStatus::Modified,
                    },
                    GitStatusEntry {
                        path: "new.txt".into(),
                        status: WireStatus::Untracked,
                    },
                ],
            },
        ));
        let map = fs.git_status(false).unwrap();
        assert_eq!(
            map.get(Path::new("src/lib.rs")),
            Some(&StatusKind::Modified)
        );
        assert_eq!(map.get(Path::new("new.txt")), Some(&StatusKind::Untracked));
    }

    #[test]
    fn git_status_not_a_repo_maps_to_non_zero_exit() {
        let fs = service(FakeTransport::new().with_error(
            methods::GIT_STATUS,
            RpcError::new(error_codes::NOT_A_REPO, "not a repo"),
        ));
        let err = fs.git_status(false).unwrap_err();
        assert!(matches!(err, StatusError::NonZeroExit { .. }), "{err}");
    }

    #[test]
    fn rel_to_wire_uses_forward_slashes() {
        assert_eq!(rel_to_wire(Path::new("")), "");
        assert_eq!(rel_to_wire(Path::new("src/lib.rs")), "src/lib.rs");
    }

    #[test]
    fn docker_exec_argv_shape() {
        let argv = docker_exec_argv(
            "abc123",
            "vscode",
            "/opt/fleet/bin/fleet-agent",
            "/workspaces/repo",
        );
        assert_eq!(
            argv,
            vec![
                "docker",
                "exec",
                "-i",
                "-u",
                "vscode",
                "abc123",
                "/opt/fleet/bin/fleet-agent",
                "serve",
                "--root",
                "/workspaces/repo",
            ]
        );
    }

    #[test]
    fn transport_times_out_on_unresponsive_agent() {
        // `sleep` ignores stdin and never writes a framed response, so the
        // `initialize` handshake must hit the deadline and fail rather than
        // block forever. The test completing quickly is itself the assertion
        // that there's no indefinite hang.
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let start = std::time::Instant::now();
        let result =
            ProcessTransport::from_command_with_timeout(cmd, Duration::from_millis(300), None);
        assert!(result.is_err(), "expected handshake to time out");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "transport hung instead of timing out"
        );
    }
}
