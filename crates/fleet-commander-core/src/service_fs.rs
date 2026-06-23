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
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use fleet_protocol::{
    FsListParams, FsListResult, FsReadParams, FsReadResult, GitBranchResult, GitStatusParams,
    GitStatusResult, InitializeParams, InitializeResult, PROTOCOL_VERSION, Request, Response,
    RpcError, WireStatus, error_codes, framing, methods,
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
        let root = root.into();
        let transport = ProcessTransport::spawn(agent_bin.as_ref(), &root)?;
        Ok(Self::new(root, Box::new(transport)))
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

/// A [`Transport`] that talks to a spawned `fleet-agent` child over its
/// stdio pipes.
#[derive(Debug)]
pub struct ProcessTransport {
    inner: Mutex<Pipe>,
    next_id: AtomicU64,
}

#[derive(Debug)]
struct Pipe {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl ProcessTransport {
    /// Spawn `agent_bin serve --root <root>` and complete the `initialize`
    /// handshake, verifying the daemon speaks our protocol version.
    pub fn spawn(agent_bin: &Path, root: &Path) -> io::Result<Self> {
        let mut child = Command::new(agent_bin)
            .arg("serve")
            .arg("--root")
            .arg(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("agent stdin not captured"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("agent stdout not captured"))?;
        let transport = Self {
            inner: Mutex::new(Pipe {
                child,
                stdin,
                stdout: BufReader::new(stdout),
            }),
            next_id: AtomicU64::new(0),
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
        Ok(transport)
    }
}

impl Transport for ProcessTransport {
    fn call(&self, method: &str, params: Value) -> Result<Value, TransportError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let request = Request {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params: Some(params),
        };
        let body = serde_json::to_vec(&request)
            .map_err(|e| TransportError::Protocol(format!("encode request: {e}")))?;

        let mut pipe = self
            .inner
            .lock()
            .map_err(|_| TransportError::Protocol("transport mutex poisoned".into()))?;

        framing::write_frame(&mut pipe.stdin, &body).map_err(TransportError::Io)?;
        pipe.stdin.flush().map_err(TransportError::Io)?;

        let resp_body = framing::read_frame(&mut pipe.stdout)
            .map_err(TransportError::Io)?
            .ok_or_else(|| TransportError::Protocol("agent closed the connection".into()))?;
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
        if let Ok(mut pipe) = self.inner.lock() {
            // Best-effort shutdown: kill the child and reap it so we don't
            // leak a process or a zombie.
            let _ = pipe.child.kill();
            let _ = pipe.child.wait();
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
}
