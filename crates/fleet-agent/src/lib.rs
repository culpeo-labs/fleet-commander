//! The `fleet-agent` daemon: serves filesystem and git inspection for a
//! single workspace root over JSON-RPC 2.0 on stdio.
//!
//! Phase 0 runs this as a local host child process to prove the protocol
//! end-to-end; Phase 1 bind-mounts the same binary into a dev container and
//! drives it via `docker exec -i`. The serve loop is therefore written
//! against generic [`BufRead`]/[`Write`] so the transport is interchangeable.

use std::collections::BTreeSet;
use std::io::{self, BufRead, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use fleet_protocol::{
    Capabilities, FsDidChangeParams, FsEntry, FsListParams, FsListResult, FsReadParams,
    FsReadResult, FsStatParams, FsStatResult, FsWatchParams, FsWatchResult, GitBranchResult,
    GitDiffParams, GitDiffResult, GitStatusEntry, GitStatusParams, GitStatusResult,
    InitializeResult, Notification, PROTOCOL_VERSION, Request, Response, RpcError, ServerInfo,
    WireStatus, error_codes, framing, methods,
};
use notify::{RecursiveMode, Watcher};

/// Serves requests against a fixed workspace `root`.
pub struct Server {
    root: PathBuf,
    /// `root` with all symlinks resolved, used to verify that a resolved
    /// request path stays inside the workspace even when it traverses a
    /// symlink. Falls back to `root` if the workspace can't be canonicalized.
    canonical_root: PathBuf,
}

impl Server {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        Self {
            root,
            canonical_root,
        }
    }

    /// Read framed requests from `reader` until EOF, dispatching each and
    /// writing a framed response to `writer`. A frame that fails to parse as
    /// a request yields a best-effort JSON-RPC parse error.
    pub fn serve<R: BufRead, W: Write>(&self, reader: &mut R, writer: &mut W) -> io::Result<()> {
        while let Some(body) = framing::read_frame(reader)? {
            let response = match serde_json::from_slice::<Request>(&body) {
                Ok(req) => self.handle(&req),
                Err(e) => Response::err(
                    0,
                    RpcError::new(error_codes::PARSE_ERROR, format!("invalid request: {e}")),
                ),
            };
            let out = serde_json::to_vec(&response)?;
            framing::write_frame(writer, &out)?;
        }
        Ok(())
    }

    /// Like [`serve`](Self::serve) but watch-aware: outbound frames
    /// (responses **and** server-initiated notifications) are serialized
    /// through a dedicated writer thread, so an [`methods::FS_WATCH`]
    /// subscription can push [`methods::FS_DID_CHANGE`] notifications
    /// concurrently with request handling. This is the production entry
    /// point (stdio over `docker exec -i`); [`serve`](Self::serve) remains
    /// the simple synchronous path for the no-watch case.
    pub fn serve_stdio<R, W>(&self, reader: &mut R, writer: W) -> io::Result<()>
    where
        R: BufRead,
        W: Write + Send + 'static,
    {
        let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
        let writer_handle = thread::Builder::new()
            .name("fleet-agent-writer".into())
            .spawn(move || {
                let mut writer = writer;
                while let Ok(frame) = out_rx.recv() {
                    if framing::write_frame(&mut writer, &frame).is_err() {
                        break;
                    }
                }
            })?;

        let mut watch: Option<WatchHandle> = None;
        let result = self.dispatch_loop(reader, &out_tx, &mut watch);

        // Tear down in order: stop the watcher (closes its path channel and
        // exits the coalescer), then drop our sender so the writer thread
        // sees the channel close and exits, then join it.
        drop(watch);
        drop(out_tx);
        let _ = writer_handle.join();
        result
    }

    /// Read framed requests, dispatching each and sending its response (and,
    /// for `fs.watch`, managing the subscription) to the outbound `out`
    /// channel. Returns on clean EOF or a transport error.
    fn dispatch_loop<R: BufRead>(
        &self,
        reader: &mut R,
        out: &mpsc::Sender<Vec<u8>>,
        watch: &mut Option<WatchHandle>,
    ) -> io::Result<()> {
        while let Some(body) = framing::read_frame(reader)? {
            let response = match serde_json::from_slice::<Request>(&body) {
                Ok(req) if req.method == methods::FS_WATCH => self.handle_watch(&req, out, watch),
                Ok(req) => self.handle(&req),
                Err(e) => Response::err(
                    0,
                    RpcError::new(error_codes::PARSE_ERROR, format!("invalid request: {e}")),
                ),
            };
            send_body(out, &serde_json::to_vec(&response)?)?;
        }
        Ok(())
    }

    /// Start or stop the workspace watcher per [`FsWatchParams`].
    fn handle_watch(
        &self,
        req: &Request,
        out: &mpsc::Sender<Vec<u8>>,
        watch: &mut Option<WatchHandle>,
    ) -> Response {
        let params: FsWatchParams = match parse_params(req) {
            Ok(p) => p,
            Err(e) => return Response::err(req.id, e),
        };
        if params.enable {
            if watch.is_none() {
                match WatchHandle::start(self.canonical_root.clone(), out.clone()) {
                    Ok(handle) => *watch = Some(handle),
                    Err(e) => {
                        return Response::err(
                            req.id,
                            RpcError::new(error_codes::IO_ERROR, format!("watch failed: {e}")),
                        );
                    }
                }
            }
        } else {
            *watch = None; // dropping the handle stops watching.
        }
        Response::ok(
            req.id,
            FsWatchResult {
                watching: watch.is_some(),
            },
        )
    }

    /// Dispatch a single request to its handler.
    pub fn handle(&self, req: &Request) -> Response {
        let result = match req.method.as_str() {
            methods::INITIALIZE => self.initialize(),
            methods::FS_LIST => self.fs_list(req),
            methods::FS_READ => self.fs_read(req),
            methods::FS_STAT => self.fs_stat(req),
            methods::GIT_STATUS => self.git_status(req),
            methods::GIT_BRANCH => self.git_branch(),
            methods::GIT_DIFF => self.git_diff(req),
            other => Err(RpcError::new(
                error_codes::METHOD_NOT_FOUND,
                format!("unknown method: {other}"),
            )),
        };
        match result {
            Ok(value) => Response::ok(req.id, value),
            Err(error) => Response::err(req.id, error),
        }
    }

    fn initialize(&self) -> Result<serde_json::Value, RpcError> {
        ok(InitializeResult {
            protocol_version: PROTOCOL_VERSION,
            server_info: ServerInfo {
                name: "fleet-agent".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            capabilities: Capabilities {
                fs: true,
                git: true,
                watch: true,
            },
        })
    }

    fn fs_list(&self, req: &Request) -> Result<serde_json::Value, RpcError> {
        let params: FsListParams = parse_params(req)?;
        let abs = self.resolve(&params.path)?;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&abs).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let is_dir = entry.file_type().map_err(io_error)?.is_dir();
            entries.push(FsEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir,
            });
        }
        ok(FsListResult { entries })
    }

    fn fs_read(&self, req: &Request) -> Result<serde_json::Value, RpcError> {
        let params: FsReadParams = parse_params(req)?;
        let abs = self.resolve(&params.path)?;
        let total_size = std::fs::metadata(&abs).map_err(io_error)?.len();

        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(&abs).map_err(io_error)?;
        if params.offset > 0 {
            file.seek(SeekFrom::Start(params.offset))
                .map_err(io_error)?;
        }
        let mut buf = Vec::new();
        match params.len {
            Some(len) => {
                file.take(len).read_to_end(&mut buf).map_err(io_error)?;
            }
            None => {
                file.read_to_end(&mut buf).map_err(io_error)?;
            }
        }
        let eof = params.offset.saturating_add(buf.len() as u64) >= total_size;
        ok(FsReadResult {
            content_base64: BASE64.encode(&buf),
            eof,
            total_size,
        })
    }

    fn fs_stat(&self, req: &Request) -> Result<serde_json::Value, RpcError> {
        let params: FsStatParams = parse_params(req)?;
        let abs = self.resolve(&params.path)?;
        let meta = std::fs::metadata(&abs).map_err(io_error)?;
        ok(FsStatResult {
            is_dir: meta.is_dir(),
            len: meta.len(),
        })
    }

    fn git_status(&self, req: &Request) -> Result<serde_json::Value, RpcError> {
        let params: GitStatusParams = parse_params(req)?;
        let map = fleet_git::status(&self.root, params.include_ignored).map_err(git_error)?;
        let entries = map
            .into_iter()
            .map(|(path, kind)| GitStatusEntry {
                path: path.to_string_lossy().into_owned(),
                status: to_wire(kind),
            })
            .collect();
        ok(GitStatusResult { entries })
    }

    fn git_branch(&self) -> Result<serde_json::Value, RpcError> {
        ok(GitBranchResult {
            branch: fleet_git::current_branch(&self.root),
        })
    }

    fn git_diff(&self, req: &Request) -> Result<serde_json::Value, RpcError> {
        let params: GitDiffParams = parse_params(req)?;
        // Validate the path stays inside the workspace before handing it to
        // git (rejects `..`, absolute paths, and symlink escapes).
        self.resolve(&params.path)?;
        let diff = fleet_git::diff(&self.root, Path::new(&params.path), params.staged)
            .map_err(git_error)?;
        ok(GitDiffResult { diff })
    }

    /// Resolve a workspace-relative request path to an absolute path under
    /// `root`, rejecting anything that would escape it. Two layers of defence:
    ///
    /// 1. **Lexical:** reject absolute paths, `..`, and prefixes up front.
    /// 2. **Symlink-aware:** canonicalize the result (resolving any symlinks
    ///    along the way) and verify it still lives under the canonicalized
    ///    workspace root. This stops an *in-workspace* symlink — e.g.
    ///    `secrets -> /run/secrets` — from being followed out of the root.
    ///
    /// The server never trusts the client's path.
    fn resolve(&self, rel: &str) -> Result<PathBuf, RpcError> {
        let rel_path = Path::new(rel);
        let mut safe = PathBuf::new();
        for component in rel_path.components() {
            match component {
                Component::Normal(c) => safe.push(c),
                Component::CurDir => {}
                Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                    return Err(RpcError::new(
                        error_codes::FORBIDDEN_PATH,
                        format!("path escapes workspace root: {rel}"),
                    ));
                }
            }
        }
        let candidate = self.root.join(safe);
        // Resolve symlinks to a real path. The leaf may legitimately not exist
        // (e.g. reading a missing file), so fall back to the nearest existing
        // ancestor and re-append the remainder — this preserves NotFound
        // semantics while still catching escapes via a symlinked ancestor.
        let real = canonicalize_existing_prefix(&candidate).map_err(io_error)?;
        if !real.starts_with(&self.canonical_root) {
            return Err(RpcError::new(
                error_codes::FORBIDDEN_PATH,
                format!("path escapes workspace root: {rel}"),
            ));
        }
        Ok(candidate)
    }
}

/// Canonicalize `path`, tolerating a non-existent leaf: if the full path
/// doesn't exist, resolve the nearest existing ancestor (expanding symlinks)
/// and re-append the missing trailing components. Errors other than
/// "not found" (e.g. a permission error) propagate unchanged.
fn canonicalize_existing_prefix(path: &Path) -> io::Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(real) => Ok(real),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or(e)?;
            let leaf = path
                .file_name()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            Ok(canonicalize_existing_prefix(parent)?.join(leaf))
        }
        Err(e) => Err(e),
    }
}

fn ok(value: impl serde::Serialize) -> Result<serde_json::Value, RpcError> {
    serde_json::to_value(value)
        .map_err(|e| RpcError::new(error_codes::INTERNAL_ERROR, format!("serialize: {e}")))
}
fn parse_params<T: serde::de::DeserializeOwned>(req: &Request) -> Result<T, RpcError> {
    let params = req
        .params
        .clone()
        .ok_or_else(|| RpcError::new(error_codes::INVALID_PARAMS, "missing params"))?;
    serde_json::from_value(params)
        .map_err(|e| RpcError::new(error_codes::INVALID_PARAMS, format!("invalid params: {e}")))
}

/// How long the coalescer batches incoming change events before emitting a
/// single `fs.didChange`. Smooths bursts (a `git checkout` or editor save
/// touches many files) into one client refresh.
const WATCH_COALESCE_WINDOW: Duration = Duration::from_millis(150);

/// Encode `body` as a frame and hand it to the writer thread. A send error
/// means the writer thread is gone (broken transport), surfaced as EOF-ish.
fn send_body(out: &mpsc::Sender<Vec<u8>>, body: &[u8]) -> io::Result<()> {
    out.send(body.to_vec())
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "writer thread ended"))
}

/// Owns an active filesystem watch: the `notify` watcher plus the coalescer
/// thread that turns raw events into `fs.didChange` notifications. Dropping
/// it drops the watcher (which closes the path channel and lets the
/// coalescer exit), so a watch is torn down simply by dropping the handle.
struct WatchHandle {
    watcher: Option<notify::RecommendedWatcher>,
    coalescer: Option<JoinHandle<()>>,
}

impl WatchHandle {
    /// Start watching `canonical_root` recursively, emitting coalesced
    /// `fs.didChange` notifications (paths relative to the root) to `out`.
    fn start(canonical_root: PathBuf, out: mpsc::Sender<Vec<u8>>) -> notify::Result<Self> {
        let (path_tx, path_rx) = mpsc::channel::<PathBuf>();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(event) = res {
                    for path in event.paths {
                        // A closed receiver (watch stopped) just means we stop
                        // forwarding; nothing to do.
                        let _ = path_tx.send(path);
                    }
                }
            })?;
        watcher.watch(&canonical_root, RecursiveMode::Recursive)?;

        let coalescer = thread::Builder::new()
            .name("fleet-agent-watch".into())
            .spawn(move || coalesce_loop(&path_rx, &canonical_root, &out))
            .ok();

        Ok(Self {
            watcher: Some(watcher),
            coalescer,
        })
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        // Drop the watcher *first* so its closure (which owns `path_tx`) is
        // released, closing the path channel; the coalescer then sees
        // `Disconnected` and returns. Only then can we join it without
        // deadlocking. Doing this in field-drop order would join while the
        // watcher is still alive and block forever.
        self.watcher.take();
        if let Some(handle) = self.coalescer.take() {
            let _ = handle.join();
        }
    }
}

/// Batch raw changed paths over [`WATCH_COALESCE_WINDOW`] and emit one
/// `fs.didChange` notification per batch. Exits when the watcher is dropped
/// (the path channel disconnects).
fn coalesce_loop(rx: &mpsc::Receiver<PathBuf>, canonical_root: &Path, out: &mpsc::Sender<Vec<u8>>) {
    while let Ok(first) = rx.recv() {
        let mut batch = BTreeSet::new();
        push_relative(&mut batch, canonical_root, first);

        let deadline = Instant::now() + WATCH_COALESCE_WINDOW;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(path) => push_relative(&mut batch, canonical_root, path),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    emit_did_change(out, &batch);
                    return;
                }
            }
        }
        emit_did_change(out, &batch);
    }
}

/// Insert `path` into `batch` as a workspace-relative, forward-slash string.
/// Paths outside the root are dropped (the recursive watch shouldn't produce
/// them, but a coalesced empty batch still reads as a generic "refresh").
fn push_relative(batch: &mut BTreeSet<String>, canonical_root: &Path, path: PathBuf) {
    if let Ok(rel) = path.strip_prefix(canonical_root) {
        let rel = rel.to_string_lossy().replace('\\', "/");
        batch.insert(rel);
    }
}

/// Emit a single `fs.didChange` notification for `batch` to the writer.
fn emit_did_change(out: &mpsc::Sender<Vec<u8>>, batch: &BTreeSet<String>) {
    let note = Notification::new(
        methods::FS_DID_CHANGE,
        FsDidChangeParams {
            paths: batch.iter().cloned().collect(),
        },
    );
    if let Ok(body) = serde_json::to_vec(&note) {
        let _ = out.send(body);
    }
}

fn io_error(e: io::Error) -> RpcError {
    let code = if e.kind() == io::ErrorKind::NotFound {
        error_codes::NOT_FOUND
    } else {
        error_codes::IO_ERROR
    };
    RpcError::new(code, e.to_string())
}

fn git_error(e: fleet_git::StatusError) -> RpcError {
    let code = match e {
        // A non-zero exit almost always means "not a git repo"; surface it
        // as such so the client can fall back to showing no markers.
        fleet_git::StatusError::NonZeroExit { .. } => error_codes::NOT_A_REPO,
        fleet_git::StatusError::SpawnFailed(_) => error_codes::IO_ERROR,
        fleet_git::StatusError::InvalidOutput => error_codes::INTERNAL_ERROR,
    };
    RpcError::new(code, e.to_string())
}

fn to_wire(kind: fleet_git::StatusKind) -> WireStatus {
    use fleet_git::StatusKind as K;
    match kind {
        K::Modified => WireStatus::Modified,
        K::Added => WireStatus::Added,
        K::Deleted => WireStatus::Deleted,
        K::Renamed => WireStatus::Renamed,
        K::Untracked => WireStatus::Untracked,
        K::Ignored => WireStatus::Ignored,
        K::Conflicted => WireStatus::Conflicted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_protocol::{FsListResult, FsReadResult, GitBranchResult, InitializeResult};
    use std::fs;
    use tempfile::TempDir;

    fn fixture() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("README.md"), "hi").unwrap();
        fs::write(tmp.path().join("src/lib.rs"), "fn a(){}").unwrap();
        tmp
    }

    fn call(server: &Server, method: &str, params: serde_json::Value) -> Response {
        server.handle(&Request {
            jsonrpc: "2.0".into(),
            id: 1,
            method: method.into(),
            params: Some(params),
        })
    }

    #[test]
    fn initialize_reports_version_and_capabilities() {
        let tmp = fixture();
        let server = Server::new(tmp.path());
        let resp = call(&server, methods::INITIALIZE, serde_json::json!({}));
        let result: InitializeResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(result.protocol_version, PROTOCOL_VERSION);
        assert!(result.capabilities.fs && result.capabilities.git);
        assert_eq!(result.server_info.name, "fleet-agent");
    }

    #[test]
    fn fs_list_returns_children() {
        let tmp = fixture();
        let server = Server::new(tmp.path());
        let resp = call(&server, methods::FS_LIST, serde_json::json!({ "path": "" }));
        let mut result: FsListResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        result.entries.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(result.entries.len(), 2);
        assert_eq!(result.entries[0].name, "README.md");
        assert!(!result.entries[0].is_dir);
        assert_eq!(result.entries[1].name, "src");
        assert!(result.entries[1].is_dir);
    }

    #[test]
    fn fs_read_returns_base64() {
        let tmp = fixture();
        let server = Server::new(tmp.path());
        let resp = call(
            &server,
            methods::FS_READ,
            serde_json::json!({ "path": "README.md" }),
        );
        let result: FsReadResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        let bytes = BASE64.decode(result.content_base64).unwrap();
        assert_eq!(bytes, b"hi");
        assert!(result.eof);
        assert_eq!(result.total_size, 2);
    }

    #[test]
    fn fs_read_honors_offset_and_len() {
        let tmp = fixture();
        fs::write(tmp.path().join("data.txt"), "abcdefghij").unwrap();
        let server = Server::new(tmp.path());

        // Middle window: bytes [3, 6) → "def", not yet EOF.
        let resp = call(
            &server,
            methods::FS_READ,
            serde_json::json!({ "path": "data.txt", "offset": 3, "len": 3 }),
        );
        let result: FsReadResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(BASE64.decode(result.content_base64).unwrap(), b"def");
        assert!(!result.eof);
        assert_eq!(result.total_size, 10);

        // Tail window past the end clamps and reports EOF.
        let resp = call(
            &server,
            methods::FS_READ,
            serde_json::json!({ "path": "data.txt", "offset": 8, "len": 100 }),
        );
        let result: FsReadResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(BASE64.decode(result.content_base64).unwrap(), b"ij");
        assert!(result.eof);
    }

    #[test]
    fn fs_read_missing_file_is_not_found() {
        let tmp = fixture();
        let server = Server::new(tmp.path());
        let resp = call(
            &server,
            methods::FS_READ,
            serde_json::json!({ "path": "nope.txt" }),
        );
        assert_eq!(resp.error.unwrap().code, error_codes::NOT_FOUND);
    }

    #[test]
    fn path_escape_is_forbidden() {
        let tmp = fixture();
        let server = Server::new(tmp.path());
        for bad in ["../secret", "/etc/passwd", "src/../../oops"] {
            let resp = call(
                &server,
                methods::FS_LIST,
                serde_json::json!({ "path": bad }),
            );
            assert_eq!(
                resp.error.expect("should be rejected").code,
                error_codes::FORBIDDEN_PATH,
                "path {bad} should be forbidden",
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_forbidden() {
        // A symlink that lives *inside* the workspace but points outside it
        // passes the lexical filter (all Normal components), so the resolver
        // must catch it by canonicalizing and checking containment.
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("secret.txt"), "top secret").unwrap();

        let tmp = fixture();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("escape")).unwrap();

        // Reading through the symlink (a file under it) must be forbidden.
        let resp = call(
            &Server::new(tmp.path()),
            methods::FS_READ,
            serde_json::json!({ "path": "escape/secret.txt" }),
        );
        assert_eq!(
            resp.error.expect("symlink escape should be rejected").code,
            error_codes::FORBIDDEN_PATH,
        );

        // Listing the symlinked directory itself must also be forbidden.
        let resp = call(
            &Server::new(tmp.path()),
            methods::FS_LIST,
            serde_json::json!({ "path": "escape" }),
        );
        assert_eq!(
            resp.error.expect("symlink escape should be rejected").code,
            error_codes::FORBIDDEN_PATH,
        );
    }

    #[cfg(unix)]
    #[test]
    fn in_workspace_symlink_is_allowed() {
        // A symlink pointing to another location *within* the workspace is
        // legitimate and must still resolve.
        let tmp = fixture();
        std::os::unix::fs::symlink(tmp.path().join("README.md"), tmp.path().join("link.md"))
            .unwrap();
        let resp = call(
            &Server::new(tmp.path()),
            methods::FS_READ,
            serde_json::json!({ "path": "link.md" }),
        );
        let result: FsReadResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(BASE64.decode(result.content_base64).unwrap(), b"hi");
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let tmp = fixture();
        let server = Server::new(tmp.path());
        let resp = call(&server, "bogus.method", serde_json::json!({}));
        assert_eq!(resp.error.unwrap().code, error_codes::METHOD_NOT_FOUND);
    }

    #[test]
    fn git_branch_is_none_outside_repo() {
        let tmp = fixture();
        let server = Server::new(tmp.path());
        let resp = call(&server, methods::GIT_BRANCH, serde_json::json!(null));
        let result: GitBranchResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(result.branch, None);
    }

    #[test]
    fn git_status_outside_repo_maps_to_not_a_repo() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: git not installed");
            return;
        }
        let tmp = fixture();
        let server = Server::new(tmp.path());
        let resp = call(
            &server,
            methods::GIT_STATUS,
            serde_json::json!({ "include_ignored": false }),
        );
        assert_eq!(resp.error.unwrap().code, error_codes::NOT_A_REPO);
    }

    #[test]
    fn git_diff_outside_repo_maps_to_not_a_repo() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: git not installed");
            return;
        }
        let tmp = fixture();
        let server = Server::new(tmp.path());
        let resp = call(
            &server,
            methods::GIT_DIFF,
            serde_json::json!({ "path": "README.md" }),
        );
        assert_eq!(resp.error.unwrap().code, error_codes::NOT_A_REPO);
    }

    #[test]
    fn git_diff_path_escape_is_forbidden() {
        let tmp = fixture();
        let server = Server::new(tmp.path());
        let resp = call(
            &server,
            methods::GIT_DIFF,
            serde_json::json!({ "path": "../escape" }),
        );
        assert_eq!(resp.error.unwrap().code, error_codes::FORBIDDEN_PATH);
    }

    #[test]
    fn serve_processes_framed_requests() {
        let tmp = fixture();
        let server = Server::new(tmp.path());
        let mut input = Vec::new();
        let req = Request::new(
            42,
            methods::FS_READ,
            FsReadParams {
                path: "README.md".into(),
                offset: 0,
                len: None,
            },
        );
        framing::write_frame(&mut input, &serde_json::to_vec(&req).unwrap()).unwrap();

        let mut output = Vec::new();
        let mut reader = io::Cursor::new(input);
        server.serve(&mut reader, &mut output).unwrap();

        let mut out_reader = io::Cursor::new(output);
        let body = framing::read_frame(&mut out_reader).unwrap().unwrap();
        let resp: Response = serde_json::from_slice(&body).unwrap();
        assert_eq!(resp.id, 42);
        let result: FsReadResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(BASE64.decode(result.content_base64).unwrap(), b"hi");
    }
}
