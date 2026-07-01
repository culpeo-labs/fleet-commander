//! Wire types and stdio framing for the Fleet Commander in-container
//! service (`fleet-agent`).
//!
//! The protocol is **JSON-RPC 2.0 over stdio**, in the style of ACP (which
//! the TUI already speaks to the agent). Messages are length-delimited with
//! an LSP/DAP-style `Content-Length` header so the framing is robust to
//! embedded newlines and trivially portable to other byte-stream transports
//! (`docker exec -i`, SSH) in later phases.
//!
//! This crate is intentionally dependency-light (just `serde` /
//! `serde_json`) and transport-agnostic: [`framing`] operates on any
//! [`std::io::Read`]/[`std::io::Write`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The protocol version implemented by this crate. Exchanged in the
/// [`initialize`](methods::INITIALIZE) handshake so client and server can
/// detect a mismatch before doing any real work.
pub const PROTOCOL_VERSION: u32 = 1;

/// Reserved JSON-RPC method names.
pub mod methods {
    pub const INITIALIZE: &str = "initialize";
    pub const FS_LIST: &str = "fs.list";
    pub const FS_READ: &str = "fs.read";
    pub const FS_STAT: &str = "fs.stat";
    pub const GIT_STATUS: &str = "git.status";
    pub const GIT_BRANCH: &str = "git.branch";
    /// Request: unified diff for a single path (Phase 3).
    pub const GIT_DIFF: &str = "git.diff";
    /// Request: start or stop watching the workspace for changes.
    pub const FS_WATCH: &str = "fs.watch";
    /// Server→client notification: the workspace changed (Phase 2).
    pub const FS_DID_CHANGE: &str = "fs.didChange";
}

/// JSON-RPC + application error codes.
pub mod error_codes {
    // Standard JSON-RPC 2.0 codes.
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;

    // Application-defined codes (positive space).
    /// A requested path does not exist.
    pub const NOT_FOUND: i64 = 1;
    /// The workspace is not a git repository (or `git` failed).
    pub const NOT_A_REPO: i64 = 2;
    /// A filesystem operation failed.
    pub const IO_ERROR: i64 = 3;
    /// A path escaped the workspace root.
    pub const FORBIDDEN_PATH: i64 = 4;
}

// ─── JSON-RPC envelopes ────────────────────────────────────────────────

/// A JSON-RPC request. `params` is left as a raw [`Value`] so the framing
/// layer stays method-agnostic; typed params live in [`InitializeParams`]
/// and friends.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Request {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    /// Build a `"2.0"` request with the given id, method and typed params.
    pub fn new(id: u64, method: impl Into<String>, params: impl Serialize) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params: Some(serde_json::to_value(params).expect("params serialize")),
        }
    }
}

/// A JSON-RPC response: exactly one of `result` / `error` is present.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Response {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    /// A successful response carrying `result`.
    pub fn ok(id: u64, result: impl Serialize) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(serde_json::to_value(result).expect("result serialize")),
            error: None,
        }
    }

    /// A failed response carrying an [`RpcError`].
    pub fn err(id: u64, error: RpcError) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// A JSON-RPC notification: a `method`/`params` message with **no `id`**,
/// so it is never matched to a request. The daemon uses these for
/// server-initiated pushes (e.g. [`methods::FS_DID_CHANGE`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Notification {
    /// Build a `"2.0"` notification with the given method and typed params.
    pub fn new(method: impl Into<String>, params: impl Serialize) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params: Some(serde_json::to_value(params).expect("params serialize")),
        }
    }
}

/// A message received from the peer on a shared channel: either a
/// [`Response`] to one of our requests, or a server-initiated
/// [`Notification`]. Lets a single reader demultiplex the two without
/// guessing — the discriminator is the presence of an `id` field.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Response(Response),
    Notification(Notification),
}

impl Incoming {
    /// Classify and parse a single JSON-RPC frame body. A message carrying
    /// an `id` is a [`Response`]; one with a `method` and no `id` is a
    /// [`Notification`].
    pub fn from_slice(body: &[u8]) -> serde_json::Result<Self> {
        let value: Value = serde_json::from_slice(body)?;
        let has_id = value.get("id").is_some_and(|v| !v.is_null());
        if has_id {
            Ok(Incoming::Response(serde_json::from_value(value)?))
        } else {
            Ok(Incoming::Notification(serde_json::from_value(value)?))
        }
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

// ─── Method params / results ───────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InitializeParams {
    pub protocol_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InitializeResult {
    pub protocol_version: u32,
    pub server_info: ServerInfo,
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Capabilities {
    pub fs: bool,
    pub git: bool,
    /// The server can push [`methods::FS_DID_CHANGE`] notifications after an
    /// [`methods::FS_WATCH`] request (Phase 2). Defaults to `false` so an
    /// older daemon that omits the field is treated as non-watching.
    #[serde(default)]
    pub watch: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FsListParams {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FsListResult {
    pub entries: Vec<FsEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FsEntry {
    pub name: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FsReadParams {
    pub path: String,
    /// Byte offset to start reading from. Defaults to `0`. Enables paging
    /// through large files in bounded chunks instead of one giant frame.
    #[serde(default)]
    pub offset: u64,
    /// Maximum number of bytes to return from `offset`. `None` reads to the
    /// end of the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub len: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FsReadResult {
    /// Base64-encoded file contents for the requested range. Base64 keeps
    /// arbitrary bytes safe over the JSON channel.
    pub content_base64: String,
    /// `true` when this chunk reached the end of the file (i.e. no more
    /// bytes follow `offset + returned_len`). Lets a client stop paging.
    #[serde(default)]
    pub eof: bool,
    /// Total size of the file in bytes, independent of the requested range.
    #[serde(default)]
    pub total_size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FsStatParams {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FsStatResult {
    pub is_dir: bool,
    pub len: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitStatusParams {
    pub include_ignored: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitStatusResult {
    pub entries: Vec<GitStatusEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitStatusEntry {
    pub path: String,
    pub status: WireStatus,
}

/// Wire-stable mirror of the explorer's status kinds. Kept independent of
/// `fleet-git`'s `StatusKind` so the protocol owns its own serialization
/// contract; the edges map between the two.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WireStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Ignored,
    Conflicted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitBranchResult {
    pub branch: Option<String>,
}

/// Params for [`methods::GIT_DIFF`]: the workspace-relative path to diff
/// and whether to show the staged (index-vs-HEAD) diff instead of the
/// working-tree diff.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitDiffParams {
    pub path: String,
    #[serde(default)]
    pub staged: bool,
}

/// Result of [`methods::GIT_DIFF`]: the raw unified diff (empty when the
/// path has no changes).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitDiffResult {
    pub diff: String,
}

/// Params for [`methods::FS_WATCH`]: start (`true`) or stop (`false`)
/// watching the workspace root for changes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FsWatchParams {
    pub enable: bool,
}

/// Result of [`methods::FS_WATCH`]: whether the server is now watching.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FsWatchResult {
    pub watching: bool,
}

/// Params for the [`methods::FS_DID_CHANGE`] notification: the set of
/// workspace-relative paths that changed since the last notification.
///
/// The list is coalesced and best-effort: an empty list means "something
/// changed but the precise paths are unknown — refresh". Clients should
/// treat it as a hint to re-fetch, not an authoritative diff.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct FsDidChangeParams {
    pub paths: Vec<String>,
}

// ─── Framing ───────────────────────────────────────────────────────────

/// `Content-Length`-delimited framing over an arbitrary byte stream.
pub mod framing {
    use std::io::{self, BufRead, Write};

    /// Largest header block / body we will buffer, mirroring CulpeoStream's
    /// recommended parser limits. Guards against a malformed or hostile peer
    /// asking us to allocate unbounded memory.
    pub const MAX_HEADER_BYTES: usize = 8 * 1024;
    pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

    /// Write a single frame: `Content-Length: N\r\n\r\n<body>`, flushed.
    pub fn write_frame<W: Write>(w: &mut W, body: &[u8]) -> io::Result<()> {
        write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
        w.write_all(body)?;
        w.flush()
    }

    /// Read a single frame's body. Returns `Ok(None)` on a clean EOF at a
    /// frame boundary (no bytes buffered), or an error on a truncated /
    /// malformed / oversized frame.
    pub fn read_frame<R: BufRead>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
        let mut content_length: Option<usize> = None;
        let mut header_bytes = 0usize;
        let mut line = String::new();

        loop {
            line.clear();
            let n = r.read_line(&mut line)?;
            if n == 0 {
                // EOF. Clean only if it lands exactly on a frame boundary
                // (we have not started reading a frame's headers).
                if header_bytes == 0 {
                    return Ok(None);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "EOF inside frame header block",
                ));
            }
            header_bytes += n;
            if header_bytes > MAX_HEADER_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "header block exceeds limit",
                ));
            }

            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                // End of header block.
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':') {
                if name.trim().eq_ignore_ascii_case("content-length") {
                    let len: usize = value.trim().parse().map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "invalid Content-Length")
                    })?;
                    content_length = Some(len);
                }
                // Unknown headers are ignored.
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "malformed header line",
                ));
            }
        }

        let len = content_length.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length header")
        })?;
        if len > MAX_BODY_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "body exceeds limit",
            ));
        }
        let mut body = vec![0u8; len];
        r.read_exact(&mut body)?;
        Ok(Some(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn write_frame_emits_content_length_and_body() {
        let mut buf = Vec::new();
        framing::write_frame(&mut buf, b"{\"hi\":1}").unwrap();
        assert_eq!(buf, b"Content-Length: 8\r\n\r\n{\"hi\":1}");
    }

    #[test]
    fn read_frame_round_trips_a_written_frame() {
        let mut buf = Vec::new();
        framing::write_frame(&mut buf, b"hello world").unwrap();
        let mut cursor = Cursor::new(buf);
        let body = framing::read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(body, b"hello world");
        // A second read at the boundary is a clean EOF.
        assert!(framing::read_frame(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn read_frame_handles_back_to_back_frames() {
        let mut buf = Vec::new();
        framing::write_frame(&mut buf, b"one").unwrap();
        framing::write_frame(&mut buf, b"two").unwrap();
        let mut cursor = Cursor::new(buf);
        assert_eq!(framing::read_frame(&mut cursor).unwrap().unwrap(), b"one");
        assert_eq!(framing::read_frame(&mut cursor).unwrap().unwrap(), b"two");
        assert!(framing::read_frame(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn read_frame_ignores_unknown_headers_and_is_case_insensitive() {
        let raw = b"X-Trace: abc\r\ncontent-length: 3\r\n\r\nabc";
        let mut cursor = Cursor::new(raw.to_vec());
        assert_eq!(framing::read_frame(&mut cursor).unwrap().unwrap(), b"abc");
    }

    #[test]
    fn read_frame_clean_eof_on_empty_input() {
        let mut cursor = Cursor::new(Vec::new());
        assert!(framing::read_frame(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn read_frame_errors_on_missing_content_length() {
        let raw = b"X-Trace: abc\r\n\r\nbody";
        let mut cursor = Cursor::new(raw.to_vec());
        assert!(framing::read_frame(&mut cursor).is_err());
    }

    #[test]
    fn read_frame_errors_on_truncated_body() {
        let raw = b"Content-Length: 10\r\n\r\nshort";
        let mut cursor = Cursor::new(raw.to_vec());
        assert!(framing::read_frame(&mut cursor).is_err());
    }

    #[test]
    fn request_round_trips_through_json() {
        let req = Request::new(7, methods::FS_LIST, FsListParams { path: "src".into() });
        let bytes = serde_json::to_vec(&req).unwrap();
        let back: Request = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, req);
        assert_eq!(back.jsonrpc, "2.0");
        let params: FsListParams = serde_json::from_value(back.params.unwrap()).unwrap();
        assert_eq!(params.path, "src");
    }

    #[test]
    fn response_ok_and_err_are_mutually_exclusive() {
        let ok = Response::ok(
            1,
            GitBranchResult {
                branch: Some("main".into()),
            },
        );
        assert!(ok.result.is_some() && ok.error.is_none());
        let err = Response::err(1, RpcError::new(error_codes::NOT_FOUND, "nope"));
        assert!(err.result.is_none() && err.error.is_some());
    }

    #[test]
    fn wire_status_serializes_snake_case() {
        let json = serde_json::to_string(&WireStatus::Untracked).unwrap();
        assert_eq!(json, "\"untracked\"");
        let back: WireStatus = serde_json::from_str("\"conflicted\"").unwrap();
        assert_eq!(back, WireStatus::Conflicted);
    }

    #[test]
    fn notification_serializes_without_an_id() {
        let note = Notification::new(
            methods::FS_DID_CHANGE,
            FsDidChangeParams {
                paths: vec!["src/lib.rs".into()],
            },
        );
        let value: serde_json::Value = serde_json::to_value(&note).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["method"], methods::FS_DID_CHANGE);
        assert!(value.get("id").is_none(), "notifications carry no id");
        let back: Notification = serde_json::from_value(value).unwrap();
        assert_eq!(back, note);
    }

    #[test]
    fn incoming_classifies_response_vs_notification() {
        let resp = Response::ok(7, GitBranchResult { branch: None });
        let resp_bytes = serde_json::to_vec(&resp).unwrap();
        match Incoming::from_slice(&resp_bytes).unwrap() {
            Incoming::Response(r) => assert_eq!(r.id, 7),
            other => panic!("expected response, got {other:?}"),
        }

        let note = Notification::new(methods::FS_DID_CHANGE, FsDidChangeParams::default());
        let note_bytes = serde_json::to_vec(&note).unwrap();
        match Incoming::from_slice(&note_bytes).unwrap() {
            Incoming::Notification(n) => assert_eq!(n.method, methods::FS_DID_CHANGE),
            other => panic!("expected notification, got {other:?}"),
        }
    }

    #[test]
    fn capabilities_watch_defaults_to_false_when_absent() {
        // An older daemon that predates the `watch` field must deserialize
        // as non-watching rather than failing.
        let caps: Capabilities = serde_json::from_str(r#"{"fs":true,"git":true}"#).unwrap();
        assert!(caps.fs && caps.git && !caps.watch);
    }

    #[test]
    fn fs_watch_params_round_trip() {
        let p = FsWatchParams { enable: true };
        let back: FsWatchParams =
            serde_json::from_value(serde_json::to_value(&p).unwrap()).unwrap();
        assert_eq!(back, p);
    }
}
