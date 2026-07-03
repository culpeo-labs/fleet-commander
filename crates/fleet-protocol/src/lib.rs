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
    /// Request: start a streaming content search over the workspace (Phase 3).
    pub const FS_SEARCH: &str = "fs.search";
    /// Request: cancel an in-flight [`FS_SEARCH`] by its `searchId` (Phase 3).
    pub const FS_CANCEL_SEARCH: &str = "fs.cancelSearch";
    /// Server→client notification: a batch of search matches (Phase 3).
    pub const FS_SEARCH_RESULT: &str = "fs.searchResult";
    /// Server→client notification: an [`FS_SEARCH`] finished (Phase 3). Carries
    /// the terminal [`SearchDoneParams`] summary; the request itself only acks.
    pub const FS_SEARCH_DONE: &str = "fs.searchDone";
    /// Request: start or stop watching the workspace for changes.
    pub const FS_WATCH: &str = "fs.watch";
    /// Server→client notification: the workspace changed (Phase 2).
    pub const FS_DID_CHANGE: &str = "fs.didChange";
    /// Request: spawn an ACP coding-agent child process inside the workspace
    /// and begin tunnelling its stdio through this connection (Phase 4a).
    pub const ACP_START: &str = "acp.start";
    /// Client→server notification: one line of ACP wire data destined for the
    /// child's stdin. ACP's stdio transport is newline-delimited JSON, so each
    /// notification carries exactly one message.
    pub const ACP_SEND: &str = "acp.send";
    /// Server→client notification: one line of ACP wire data read from the
    /// child's stdout.
    pub const ACP_RECV: &str = "acp.recv";
    /// Server→client notification: one line from the child's stderr (diagnostic
    /// output, e.g. device-code login URLs). Surfaced to the operator.
    pub const ACP_STDERR: &str = "acp.stderr";
    /// Server→client notification: the ACP child exited.
    pub const ACP_EXIT: &str = "acp.exit";
    /// Request: terminate the ACP child if one is running.
    pub const ACP_STOP: &str = "acp.stop";

    // ─── Session protocol (Phase 4b2) ───────────────────────────────────
    // The daemon owns the ACP client/session; the host drives it through
    // these higher-level methods instead of the raw `acp.*` byte tunnel.
    /// Request: start (or resume) an ACP coding-agent session owned by the
    /// daemon. Spawns the agent, runs the ACP handshake, and returns the
    /// session id. See [`SessionStartParams`] / [`SessionStartResult`].
    pub const SESSION_START: &str = "session.start";
    /// Client→server notification: submit a prompt turn. A notification (not a
    /// request) because a turn can run far longer than the request timeout;
    /// completion arrives via [`SESSION_PROMPT_RESULT`].
    pub const SESSION_PROMPT: &str = "session.prompt";
    /// Request: cancel the in-flight prompt turn, if any.
    pub const SESSION_CANCEL: &str = "session.cancel";
    /// Client→server notification: the operator's answer to a
    /// [`SESSION_PERMISSION_REQUEST`], keyed by its `request_id`.
    pub const SESSION_PERMISSION_RESPOND: &str = "session.permissionRespond";
    /// Server→client notification: a forwarded ACP `session/update` (opaque ACP
    /// JSON in [`SessionUpdateParams::update`]). The host feeds it into its own
    /// update aggregation.
    pub const SESSION_UPDATE: &str = "session.update";
    /// Server→client notification: the agent is requesting tool-use permission.
    /// The host must reply with [`SESSION_PERMISSION_RESPOND`].
    pub const SESSION_PERMISSION_REQUEST: &str = "session.permissionRequest";
    /// Server→client notification: a prompt turn finished (ok, or with error).
    pub const SESSION_PROMPT_RESULT: &str = "session.promptResult";
    /// Server→client notification: the session is open and ready for prompts.
    pub const SESSION_CONNECTED: &str = "session.connected";
    /// Server→client notification: a diagnostic/stderr line from the agent
    /// (e.g. device-code login URLs).
    pub const SESSION_OUTPUT: &str = "session.output";
    /// Server→client notification: a session-level error.
    pub const SESSION_ERROR: &str = "session.error";
    /// Server→client notification: the agent process exited.
    pub const SESSION_EXIT: &str = "session.exit";
    /// Server→client notification: interactive authentication is required; the
    /// host should run the carried terminal command.
    pub const SESSION_AUTH_REQUIRED: &str = "session.authRequired";
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
    /// The server supports streaming content search via [`methods::FS_SEARCH`]
    /// (Phase 3). Defaults to `false` for older daemons.
    #[serde(default)]
    pub search: bool,
    /// The server can spawn and tunnel an ACP coding-agent child via
    /// [`methods::ACP_START`] (Phase 4a). Defaults to `false` for older
    /// daemons.
    #[serde(default)]
    pub acp: bool,
    /// The server owns the ACP client/session and speaks the higher-level
    /// `session.*` protocol (Phase 4b2). Defaults to `false` for older daemons.
    #[serde(default)]
    pub session: bool,
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

/// Params for [`methods::FS_SEARCH`]: start a streaming content search.
///
/// `search_id` is a client-assigned handle used to correlate the streamed
/// [`SearchResultParams`] notifications and to target a later
/// [`methods::FS_CANCEL_SEARCH`]. `max_results` caps the total number of
/// matches streamed before the server stops early and reports `truncated`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchParams {
    pub search_id: u64,
    pub query: String,
    /// Treat `query` as a regular expression instead of a literal.
    #[serde(default)]
    pub is_regex: bool,
    /// Match case-sensitively. Defaults to smart/insensitive (`false`).
    #[serde(default)]
    pub case_sensitive: bool,
    /// Stop after this many matches (server reports `truncated`). `None`
    /// means unbounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_results: Option<u64>,
}

/// A single content-search hit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchMatch {
    /// Workspace-relative path (forward slashes).
    pub path: String,
    /// 1-based line number of the match.
    pub line: u64,
    /// 1-based column (byte offset within the line) of the match start.
    pub column: u64,
    /// The full matching line, with trailing newline stripped.
    pub text: String,
}

/// Params for the [`methods::FS_SEARCH_RESULT`] notification: a coalesced
/// batch of matches for one `search_id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchResultParams {
    pub search_id: u64,
    pub matches: Vec<SearchMatch>,
}

/// Ack returned immediately by [`methods::FS_SEARCH`]. The search runs
/// asynchronously; results stream as [`SearchResultParams`] notifications and
/// finish with a [`SearchDoneParams`] notification. `accepted` is `false` only
/// if the backend refused to start (e.g. an invalid pattern).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchAck {
    pub accepted: bool,
}

/// Final result of an [`methods::FS_SEARCH`] request: how many matches were
/// emitted and whether the search stopped early (hit `max_results` or was
/// cancelled).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchSummary {
    pub count: u64,
    pub truncated: bool,
    /// `true` when the search ended because of a
    /// [`methods::FS_CANCEL_SEARCH`] request.
    #[serde(default)]
    pub cancelled: bool,
}

/// Params for the terminal [`methods::FS_SEARCH_DONE`] notification: the
/// `search_id` that finished plus its [`SearchSummary`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchDoneParams {
    pub search_id: u64,
    #[serde(flatten)]
    pub summary: SearchSummary,
}

/// Params for [`methods::FS_CANCEL_SEARCH`]: which in-flight search to stop.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CancelSearchParams {
    pub search_id: u64,
}

/// Result of [`methods::FS_CANCEL_SEARCH`]: whether a matching in-flight
/// search was found and signalled.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CancelSearchResult {
    pub cancelled: bool,
}

/// Params for [`methods::ACP_START`]: spawn an ACP coding-agent child.
///
/// `command` is a shell-free argv-style command line (parsed the same way the
/// host would parse an ACP command, e.g. `copilot --acp --stdio`). `cwd` is the
/// working directory for the child — normally the workspace root. `env` is a
/// list of extra environment variables to set on the child.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AcpStartParams {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<AcpEnvVar>,
}

/// A single environment variable for the ACP child.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcpEnvVar {
    pub name: String,
    pub value: String,
}

/// Result of [`methods::ACP_START`]: whether a child was spawned. `started` is
/// `false` if a child was already running (the existing one is left in place).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcpStartResult {
    pub started: bool,
}

/// Params for the [`methods::ACP_SEND`] / [`methods::ACP_RECV`] notifications:
/// one line of ACP wire data (no trailing newline).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcpDataParams {
    pub data: String,
}

/// Params for the [`methods::ACP_EXIT`] notification: the child's exit code,
/// or `None` if it was terminated by a signal / the code was unavailable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AcpExitParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
}

/// Result of [`methods::ACP_STOP`]: whether a running child was signalled.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcpStopResult {
    pub stopped: bool,
}

// ─── Session protocol (Phase 4b2) ──────────────────────────────────────

/// Params for [`methods::SESSION_START`]: launch (or resume) a daemon-owned
/// ACP session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionStartParams {
    /// The ACP agent command line, e.g. `copilot --acp --stdio`.
    pub command: String,
    /// Working directory / session cwd inside the container.
    pub cwd: String,
    /// A prior session id to resume, if any (best-effort — the daemon falls
    /// back to listing or creating a session when it cannot be resumed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_session_id: Option<String>,
    /// Extra environment variables for the agent child.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<AcpEnvVar>,
}

/// Result of [`methods::SESSION_START`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionStartResult {
    /// The active session id (freshly created or resumed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// If set, interactive authentication is required before a session can be
    /// established; the host should run this terminal command and retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_required: Option<Vec<String>>,
}

/// Params for the [`methods::SESSION_PROMPT`] notification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionPromptParams {
    pub text: String,
}

/// Params for the [`methods::SESSION_UPDATE`] notification: one forwarded ACP
/// `session/update`, carried as the raw ACP JSON value so the host can feed it
/// straight into its own update aggregation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionUpdateParams {
    pub update: Value,
}

/// Params for the [`methods::SESSION_PERMISSION_REQUEST`] notification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionPermissionRequestParams {
    /// Correlates with the [`SessionPermissionRespondParams::request_id`] the
    /// host sends back.
    pub request_id: String,
    /// Human-readable tool title shown to the operator.
    pub tool_name: String,
    /// Selectable options: `(option_id, display_name, kind_label)`.
    pub options: Vec<PermissionOption>,
}

/// A single permission option offered to the operator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: String,
}

/// Params for the [`methods::SESSION_PERMISSION_RESPOND`] notification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionPermissionRespondParams {
    pub request_id: String,
    /// The chosen `option_id`, or `None` to cancel/reject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_id: Option<String>,
}

/// Params for the [`methods::SESSION_PROMPT_RESULT`] notification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SessionPromptResultParams {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Params for the [`methods::SESSION_CONNECTED`] notification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionConnectedParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Params for the [`methods::SESSION_OUTPUT`] notification: one diagnostic line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionOutputParams {
    pub line: String,
}

/// Params for the [`methods::SESSION_ERROR`] notification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionErrorParams {
    pub message: String,
}

/// Params for the [`methods::SESSION_EXIT`] notification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SessionExitParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
}

/// Params for the [`methods::SESSION_AUTH_REQUIRED`] notification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionAuthRequiredParams {
    /// A terminal command the operator should run to authenticate.
    pub command: Vec<String>,
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
    fn capabilities_session_defaults_to_false_when_absent() {
        // A daemon predating the `session` field must deserialize as
        // non-session-capable rather than failing.
        let caps: Capabilities =
            serde_json::from_str(r#"{"fs":true,"git":true,"acp":true}"#).unwrap();
        assert!(caps.acp && !caps.session);
    }

    #[test]
    fn session_start_params_round_trip() {
        let p = SessionStartParams {
            command: "copilot --acp --stdio".into(),
            cwd: "/workspaces/demo".into(),
            previous_session_id: Some("abc-123".into()),
            env: vec![AcpEnvVar {
                name: "FOO".into(),
                value: "bar".into(),
            }],
        };
        let back: SessionStartParams =
            serde_json::from_value(serde_json::to_value(&p).unwrap()).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn session_start_result_omits_absent_fields() {
        let r = SessionStartResult {
            session_id: Some("s1".into()),
            auth_required: None,
        };
        let value = serde_json::to_value(&r).unwrap();
        assert_eq!(value["session_id"], "s1");
        assert!(value.get("auth_required").is_none());
        let back: SessionStartResult = serde_json::from_value(value).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn session_update_carries_raw_acp_json() {
        let p = SessionUpdateParams {
            update: serde_json::json!({ "sessionUpdate": "agent_message_chunk" }),
        };
        let note = Notification::new(methods::SESSION_UPDATE, &p);
        let value: serde_json::Value = serde_json::to_value(&note).unwrap();
        assert_eq!(value["method"], methods::SESSION_UPDATE);
        assert_eq!(
            value["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
    }

    #[test]
    fn session_permission_request_round_trips() {
        let p = SessionPermissionRequestParams {
            request_id: "req-1".into(),
            tool_name: "shell".into(),
            options: vec![PermissionOption {
                option_id: "allow".into(),
                name: "Allow".into(),
                kind: "allow_once".into(),
            }],
        };
        let back: SessionPermissionRequestParams =
            serde_json::from_value(serde_json::to_value(&p).unwrap()).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn fs_watch_params_round_trip() {
        let p = FsWatchParams { enable: true };
        let back: FsWatchParams =
            serde_json::from_value(serde_json::to_value(&p).unwrap()).unwrap();
        assert_eq!(back, p);
    }
}
