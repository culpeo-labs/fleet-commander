//! End-to-end test: spawn the real `fleet-agent` binary and drive it over
//! its stdio pipes using the wire protocol, exactly as the TUI's
//! `ServiceFs` will. Proves framing + dispatch across a process boundary.

use std::io::{BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use fleet_protocol::{
    AcpDataParams, AcpStartParams, AcpStartResult, AcpStopResult, CancelSearchParams,
    CancelSearchResult, FsDidChangeParams, FsListParams, FsListResult, FsReadParams, FsReadResult,
    FsWatchParams, FsWatchResult, GitBranchResult, Incoming, InitializeParams, InitializeResult,
    McpBindParams, McpDataParams, McpTunnelParams, Notification, PROTOCOL_VERSION, Request,
    Response, SearchAck, SearchDoneParams, SearchParams, SearchResultParams,
    SessionConnectedParams, SessionPromptParams, SessionPromptResultParams, SessionStartParams,
    SessionStartResult, SessionUpdateParams, framing, methods,
};

/// Generous ceiling for any single blocking read so a protocol regression
/// fails the test instead of hanging CI.
const RECV_TIMEOUT: Duration = Duration::from_secs(10);

struct AgentProcess {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Incoming>,
    pending: Vec<Notification>,
    pending_responses: Vec<Response>,
    next_id: u64,
}

impl AgentProcess {
    fn spawn(root: &std::path::Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_fleet-agent"))
            .arg("serve")
            .arg("--root")
            .arg(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn fleet-agent");
        Self::from_child(child)
    }

    /// Connect to an already-running socket daemon through a `bridge` relay,
    /// exactly as the host does via `docker exec`.
    fn spawn_bridge(socket: &std::path::Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_fleet-agent"))
            .arg("bridge")
            .arg("--socket")
            .arg(socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn fleet-agent bridge");
        Self::from_child(child)
    }

    fn from_child(mut child: Child) -> Self {
        let stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());

        // Reader thread: classify every inbound frame and forward it. This
        // mirrors the client's response/notification demux and means the
        // test never blocks directly on the child's pipe.
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(Some(body)) = framing::read_frame(&mut stdout) {
                match Incoming::from_slice(&body) {
                    Ok(incoming) => {
                        if tx.send(incoming).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            child,
            stdin,
            rx,
            pending: Vec::new(),
            pending_responses: Vec::new(),
            next_id: 0,
        }
    }

    fn send(&mut self, method: &str, params: impl serde::Serialize) -> u64 {
        self.next_id += 1;
        let req = Request::new(self.next_id, method, params);
        let body = serde_json::to_vec(&req).unwrap();
        framing::write_frame(&mut self.stdin, &body).unwrap();
        self.stdin.flush().unwrap();
        self.next_id
    }

    /// Send a fire-and-forget notification (no `id`, no response), as the host
    /// does for the high-frequency `acp.send` tunnel stream.
    fn send_notification(&mut self, method: &str, params: impl serde::Serialize) {
        let note = Notification::new(method, params);
        let body = serde_json::to_vec(&note).unwrap();
        framing::write_frame(&mut self.stdin, &body).unwrap();
        self.stdin.flush().unwrap();
    }

    /// Send a request and wait for its response, buffering any notifications
    /// that arrive in the meantime.
    fn call(&mut self, method: &str, params: impl serde::Serialize) -> Response {
        self.send(method, params);
        loop {
            match self.rx.recv_timeout(RECV_TIMEOUT) {
                Ok(Incoming::Response(resp)) => return resp,
                Ok(Incoming::Notification(note)) => self.pending.push(note),
                Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for response"),
                Err(RecvTimeoutError::Disconnected) => panic!("agent stdout closed"),
            }
        }
    }

    /// Wait for the response to a specific request `id`, buffering any
    /// notifications and out-of-order responses that arrive first. Lets a test
    /// issue several requests without waiting and then collect their responses
    /// in any order — e.g. to prove `fs.list` is answered before an in-flight
    /// `session.start` resolves.
    fn await_response(&mut self, id: u64) -> Response {
        if let Some(pos) = self.pending_responses.iter().position(|r| r.id == id) {
            return self.pending_responses.remove(pos);
        }
        loop {
            match self.rx.recv_timeout(RECV_TIMEOUT) {
                Ok(Incoming::Response(resp)) if resp.id == id => return resp,
                Ok(Incoming::Response(resp)) => self.pending_responses.push(resp),
                Ok(Incoming::Notification(note)) => self.pending.push(note),
                Err(RecvTimeoutError::Timeout) => {
                    panic!("timed out waiting for response id {id}")
                }
                Err(RecvTimeoutError::Disconnected) => panic!("agent stdout closed"),
            }
        }
    }

    /// Wait for the next server-initiated notification of `method`.
    fn next_notification(&mut self, method: &str) -> Notification {
        if let Some(pos) = self.pending.iter().position(|n| n.method == method) {
            return self.pending.remove(pos);
        }
        loop {
            match self.rx.recv_timeout(RECV_TIMEOUT) {
                Ok(Incoming::Notification(note)) if note.method == method => return note,
                Ok(Incoming::Notification(note)) => self.pending.push(note),
                Ok(Incoming::Response(_)) => {} // stray response; not expected here
                Err(RecvTimeoutError::Timeout) => {
                    panic!("timed out waiting for {method} notification")
                }
                Err(RecvTimeoutError::Disconnected) => panic!("agent stdout closed"),
            }
        }
    }

    /// Drain all currently-buffered notifications of `method`.
    fn drain_notifications(&mut self, method: &str) -> Vec<Notification> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].method == method {
                out.push(self.pending.remove(i));
            } else {
                i += 1;
            }
        }
        out
    }
}
impl Drop for AgentProcess {
    fn drop(&mut self) {
        // Closing stdin ends the serve loop; then reap the child.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn full_protocol_round_trip_over_process_stdio() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("hello.txt"), b"world").unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), b"fn a(){}").unwrap();

    let mut agent = AgentProcess::spawn(tmp.path());

    // initialize
    let resp = agent.call(
        methods::INITIALIZE,
        InitializeParams {
            protocol_version: PROTOCOL_VERSION,
        },
    );
    let init: InitializeResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(init.protocol_version, PROTOCOL_VERSION);
    assert!(init.capabilities.fs && init.capabilities.git);
    assert!(init.capabilities.watch);

    // fs.list root
    let resp = agent.call(methods::FS_LIST, FsListParams { path: "".into() });
    let mut list: FsListResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    list.entries.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<_> = list.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["hello.txt", "src"]);

    // fs.read
    let resp = agent.call(
        methods::FS_READ,
        FsReadParams {
            path: "hello.txt".into(),
            offset: 0,
            len: None,
        },
    );
    let read: FsReadResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(read.content_base64)
        .unwrap();
    assert_eq!(bytes, b"world");

    // git.branch (not a repo → None)
    let resp = agent.call(methods::GIT_BRANCH, serde_json::Value::Null);
    let branch: GitBranchResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(branch.branch, None);
}

#[test]
fn fs_watch_pushes_did_change_on_filesystem_mutation() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("seed.txt"), b"seed").unwrap();

    let mut agent = AgentProcess::spawn(tmp.path());

    // Subscribe to watch notifications.
    let resp = agent.call(methods::FS_WATCH, FsWatchParams { enable: true });
    let watch: FsWatchResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(watch.watching);

    // Mutate the workspace; the daemon should push an fs.didChange.
    std::fs::write(tmp.path().join("created.txt"), b"new").unwrap();

    let note = agent.next_notification(methods::FS_DID_CHANGE);
    let params: FsDidChangeParams = serde_json::from_value(note.params.unwrap()).unwrap();
    assert!(
        params.paths.iter().any(|p| p == "created.txt"),
        "expected created.txt in changed paths, got {:?}",
        params.paths
    );

    // Unsubscribe; the server reports it is no longer watching.
    let resp = agent.call(methods::FS_WATCH, FsWatchParams { enable: false });
    let watch: FsWatchResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(!watch.watching);
}

#[test]
fn fs_search_streams_matches_then_summary() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("a.txt"), b"needle here\nno match\n").unwrap();
    std::fs::write(tmp.path().join("src/b.rs"), b"// needle in code\n").unwrap();
    std::fs::write(tmp.path().join("c.txt"), b"nothing to see\n").unwrap();
    // Gitignored file must be skipped by the walk.
    std::fs::write(tmp.path().join(".gitignore"), b"ignored.txt\n").unwrap();
    std::fs::write(tmp.path().join("ignored.txt"), b"needle ignored\n").unwrap();

    let mut agent = AgentProcess::spawn(tmp.path());

    // fs.search returns an immediate ack; matches stream as fs.searchResult
    // notifications and a terminal fs.searchDone carries the summary.
    let resp = agent.call(
        methods::FS_SEARCH,
        SearchParams {
            search_id: 1,
            query: "needle".into(),
            is_regex: false,
            case_sensitive: false,
            max_results: None,
        },
    );
    let ack: SearchAck = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(ack.accepted);

    // Wait for the terminal summary notification.
    let done = agent.next_notification(methods::FS_SEARCH_DONE);
    let done: SearchDoneParams = serde_json::from_value(done.params.unwrap()).unwrap();
    assert_eq!(done.search_id, 1);
    assert!(!done.summary.truncated);
    assert!(!done.summary.cancelled);
    assert_eq!(
        done.summary.count, 2,
        "gitignored file should not be matched"
    );

    // Collect streamed matches (buffered while waiting for fs.searchDone).
    let mut paths: Vec<String> = agent
        .drain_notifications(methods::FS_SEARCH_RESULT)
        .into_iter()
        .flat_map(|n| {
            let p: SearchResultParams = serde_json::from_value(n.params.unwrap()).unwrap();
            assert_eq!(p.search_id, 1);
            p.matches.into_iter().map(|m| m.path)
        })
        .collect();
    paths.sort();
    assert_eq!(paths, vec!["a.txt".to_string(), "src/b.rs".to_string()]);
}

#[test]
fn fs_search_rejects_invalid_regex() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), b"x\n").unwrap();
    let mut agent = AgentProcess::spawn(tmp.path());

    let resp = agent.call(
        methods::FS_SEARCH,
        SearchParams {
            search_id: 1,
            query: "(".into(), // unbalanced group
            is_regex: true,
            case_sensitive: false,
            max_results: None,
        },
    );
    let ack: SearchAck = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(!ack.accepted, "invalid regex should be rejected in the ack");
}

#[test]
fn fs_cancel_search_stops_in_flight_search() {
    let tmp = tempfile::TempDir::new().unwrap();
    // A large fixture so the walk is still running when the cancel arrives.
    for i in 0..4000 {
        std::fs::write(
            tmp.path().join(format!("f{i}.txt")),
            b"needle on every line\nneedle again\n",
        )
        .unwrap();
    }

    let mut agent = AgentProcess::spawn(tmp.path());

    let search_id = 7;
    let resp = agent.call(
        methods::FS_SEARCH,
        SearchParams {
            search_id,
            query: "needle".into(),
            is_regex: false,
            case_sensitive: false,
            max_results: None,
        },
    );
    let ack: SearchAck = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(ack.accepted);

    // Cancel; the daemon responds on a separate id while the search runs.
    let cancel_resp = agent.call(methods::FS_CANCEL_SEARCH, CancelSearchParams { search_id });
    let cancel: CancelSearchResult = serde_json::from_value(cancel_resp.result.unwrap()).unwrap();
    assert!(
        cancel.cancelled,
        "cancel should find the in-flight search still registered"
    );

    // The search must still terminate and deliver its terminal summary,
    // flagged cancelled since it stopped short of the full 8000 matches.
    let done = agent.next_notification(methods::FS_SEARCH_DONE);
    let done: SearchDoneParams = serde_json::from_value(done.params.unwrap()).unwrap();
    assert_eq!(done.search_id, search_id);
    assert!(done.summary.cancelled, "summary should report cancellation");
    assert!(
        done.summary.count < 8000,
        "cancelled search should stop short of all matches, got {}",
        done.summary.count
    );
}

#[test]
fn acp_tunnel_relays_child_stdio_and_reports_exit() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut agent = AgentProcess::spawn(tmp.path());

    // initialize — the daemon must advertise the ACP tunnel capability.
    let resp = agent.call(
        methods::INITIALIZE,
        InitializeParams {
            protocol_version: PROTOCOL_VERSION,
        },
    );
    let init: InitializeResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(init.capabilities.acp, "daemon should advertise acp support");

    // Spawn `cat` as a stand-in ACP child: it echoes each stdin line back on
    // stdout, which is all we need to prove the tunnel relays both directions.
    let resp = agent.call(
        methods::ACP_START,
        AcpStartParams {
            command: "cat".into(),
            cwd: None,
            env: Vec::new(),
        },
    );
    let start: AcpStartResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(start.started, "acp.start should spawn the child");

    // A second start is a no-op while a child is running.
    let resp = agent.call(
        methods::ACP_START,
        AcpStartParams {
            command: "cat".into(),
            cwd: None,
            env: Vec::new(),
        },
    );
    let again: AcpStartResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(!again.started, "a second acp.start must not spawn a child");

    // Host→agent line arrives on the child's stdin; the child echoes it and we
    // receive it back as an acp.recv notification.
    agent.send_notification(
        methods::ACP_SEND,
        AcpDataParams {
            data: "{\"jsonrpc\":\"2.0\"}".into(),
        },
    );
    let note = agent.next_notification(methods::ACP_RECV);
    let echoed: AcpDataParams = serde_json::from_value(note.params.unwrap()).unwrap();
    assert_eq!(echoed.data, "{\"jsonrpc\":\"2.0\"}");

    // Stopping kills the child; its closed stdout yields an acp.exit.
    let resp = agent.call(methods::ACP_STOP, ());
    let stop: AcpStopResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(stop.stopped, "acp.stop should signal the running child");
    let _ = agent.next_notification(methods::ACP_EXIT);
}

/// A minimal ACP coding agent, in Python (preinstalled on the CI runners),
/// speaking newline-delimited JSON-RPC — enough for the daemon to complete the
/// `initialize` → `session/new` → `session/prompt` handshake. On a prompt it
/// streams one `session/update` (agent message chunk) then answers the request.
const FAKE_ACP_AGENT: &str = r#"
import sys, json

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid,
              "result": {"protocolVersion": 1, "agentCapabilities": {}, "authMethods": []}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": "test-session-1"}})
    elif method == "session/prompt":
        send({"jsonrpc": "2.0", "method": "session/update",
              "params": {"sessionId": "test-session-1",
                         "update": {"sessionUpdate": "agent_message_chunk",
                                    "content": {"type": "text", "text": "pong"}}}})
        send({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "end_turn"}})
    elif method == "authenticate":
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
"#;

/// Drive the daemon-owned session protocol end-to-end against the fake ACP
/// agent: `session.start` runs the handshake and returns the session id, a
/// prompt is forwarded, and the agent's streamed update plus the turn's
/// completion come back as `session.update` / `session.promptResult`.
#[test]
fn daemon_owns_acp_session_end_to_end() {
    let tmp = tempfile::TempDir::new().unwrap();
    let script = tmp.path().join("fake_acp.py");
    std::fs::write(&script, FAKE_ACP_AGENT).unwrap();

    let mut agent = AgentProcess::spawn(tmp.path());

    // The daemon must advertise it owns the ACP session.
    let resp = agent.call(
        methods::INITIALIZE,
        InitializeParams {
            protocol_version: PROTOCOL_VERSION,
        },
    );
    let init: InitializeResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(
        init.capabilities.session,
        "daemon should advertise session support"
    );

    // Start a session: the daemon spawns the fake agent, runs the handshake,
    // and returns the session id.
    let resp = agent.call(
        methods::SESSION_START,
        SessionStartParams {
            command: format!("python3 {}", script.display()),
            cwd: tmp.path().display().to_string(),
            previous_session_id: None,
            env: Vec::new(),
            mcp: false,
        },
    );
    let started: SessionStartResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(
        started.session_id.as_deref(),
        Some("test-session-1"),
        "session.start should return the agent's session id"
    );
    assert!(started.auth_required.is_none());

    // The daemon announces readiness.
    let note = agent.next_notification(methods::SESSION_CONNECTED);
    let connected: SessionConnectedParams = serde_json::from_value(note.params.unwrap()).unwrap();
    assert_eq!(connected.session_id.as_deref(), Some("test-session-1"));

    // Send a prompt; the agent streams an update and completes the turn.
    agent.send_notification(
        methods::SESSION_PROMPT,
        SessionPromptParams {
            text: "ping".into(),
        },
    );

    let note = agent.next_notification(methods::SESSION_UPDATE);
    let update: SessionUpdateParams = serde_json::from_value(note.params.unwrap()).unwrap();
    assert_eq!(
        update.update["sessionUpdate"], "agent_message_chunk",
        "forwarded update should be the agent message chunk"
    );

    let note = agent.next_notification(methods::SESSION_PROMPT_RESULT);
    let result: SessionPromptResultParams = serde_json::from_value(note.params.unwrap()).unwrap();
    assert!(result.ok, "prompt turn should complete successfully");
}

/// Like [`FAKE_ACP_AGENT`] but deliberately slow to `initialize`, so the
/// daemon's `session.start` stays blocked in the ACP handshake long enough to
/// observe that concurrent `fs.*` requests are still served.
const SLOW_ACP_AGENT: &str = r#"
import sys, json, time

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if method == "initialize":
        time.sleep(2)
        send({"jsonrpc": "2.0", "id": mid,
              "result": {"protocolVersion": 1, "agentCapabilities": {}, "authMethods": []}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": "test-session-1"}})
    elif method == "authenticate":
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
"#;

/// `session.start` runs a multi-second ACP handshake in the daemon; it must not
/// stall the connection's dispatch loop. This drives a slow-initializing agent
/// and proves an `fs.list` on the **same connection** is answered promptly while
/// `session.start` is still in flight — the concurrency the host relies on so a
/// single bridge can carry both the explorer's `fs.*` traffic and the session.
#[test]
fn fs_requests_are_served_while_session_start_is_in_flight() {
    let tmp = tempfile::TempDir::new().unwrap();
    let script = tmp.path().join("slow_acp.py");
    std::fs::write(&script, SLOW_ACP_AGENT).unwrap();
    std::fs::write(tmp.path().join("hello.txt"), b"world").unwrap();

    let mut agent = AgentProcess::spawn(tmp.path());
    agent.call(
        methods::INITIALIZE,
        InitializeParams {
            protocol_version: PROTOCOL_VERSION,
        },
    );

    // Kick off session.start without waiting for its (slow) response.
    let start_id = agent.send(
        methods::SESSION_START,
        SessionStartParams {
            command: format!("python3 {}", script.display()),
            cwd: tmp.path().display().to_string(),
            previous_session_id: None,
            env: Vec::new(),
            mcp: false,
        },
    );

    // fs.list must be answered well before the ~2s handshake resolves.
    let began = std::time::Instant::now();
    let fs_id = agent.send(methods::FS_LIST, FsListParams { path: "".into() });
    let resp = agent.await_response(fs_id);
    let elapsed = began.elapsed();
    assert!(
        elapsed < Duration::from_millis(1500),
        "fs.list was blocked behind session.start ({}ms)",
        elapsed.as_millis()
    );
    let list: FsListResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(
        list.entries.iter().any(|e| e.name == "hello.txt"),
        "fs.list should return the workspace contents"
    );

    // session.start still resolves correctly afterward.
    let resp = agent.await_response(start_id);
    let started: SessionStartResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(started.session_id.as_deref(), Some("test-session-1"));
}

/// Owns a `serve --socket` daemon process and kills it on drop.
struct SocketDaemon {
    child: Child,
    socket: std::path::PathBuf,
    _tmp: tempfile::TempDir,
}

impl SocketDaemon {
    fn start(root: &std::path::Path) -> Self {
        let tmp = tempfile::TempDir::new().unwrap();
        let socket = tmp.path().join("agent.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_fleet-agent"))
            .arg("serve")
            .arg("--root")
            .arg(root)
            .arg("--socket")
            .arg(&socket)
            .spawn()
            .expect("spawn fleet-agent daemon");

        // Wait for the daemon to bind before any bridge tries to connect.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "daemon never created its socket"
            );
            thread::sleep(Duration::from_millis(20));
        }
        Self {
            child,
            socket,
            _tmp: tmp,
        }
    }
}

impl Drop for SocketDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn socket_daemon_serves_bridge_clients_and_survives_reconnect() {
    let root = tempfile::TempDir::new().unwrap();
    std::fs::write(root.path().join("hello.txt"), b"world").unwrap();
    let daemon = SocketDaemon::start(root.path());

    // First client: connect through a bridge and drive the protocol.
    {
        let mut agent = AgentProcess::spawn_bridge(&daemon.socket);
        let resp = agent.call(
            methods::INITIALIZE,
            InitializeParams {
                protocol_version: PROTOCOL_VERSION,
            },
        );
        let init: InitializeResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(init.protocol_version, PROTOCOL_VERSION);
        assert!(init.capabilities.fs && init.capabilities.git);

        let resp = agent.call(methods::FS_LIST, FsListParams { path: "".into() });
        let list: FsListResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        let names: Vec<_> = list.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["hello.txt"]);
        // Dropping `agent` kills the bridge; the daemon should keep listening.
    }

    // Second client: the daemon must accept a fresh connection after the first
    // one disconnected — the reattach guarantee Phase B is built on.
    {
        let mut agent = AgentProcess::spawn_bridge(&daemon.socket);
        let resp = agent.call(
            methods::INITIALIZE,
            InitializeParams {
                protocol_version: PROTOCOL_VERSION,
            },
        );
        let init: InitializeResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(init.protocol_version, PROTOCOL_VERSION);
    }
}

#[test]
fn socket_daemon_serves_two_clients_concurrently() {
    // A live session holds two connections at once (fs/watch + ACP tunnel), so
    // the daemon must serve them in parallel — a second client must not block
    // behind the first one's still-open connection.
    let root = tempfile::TempDir::new().unwrap();
    std::fs::write(root.path().join("a.txt"), b"1").unwrap();
    let daemon = SocketDaemon::start(root.path());

    let mut first = AgentProcess::spawn_bridge(&daemon.socket);
    let mut second = AgentProcess::spawn_bridge(&daemon.socket);

    // Both connections are open simultaneously; drive them interleaved.
    for agent in [&mut first, &mut second] {
        let resp = agent.call(
            methods::INITIALIZE,
            InitializeParams {
                protocol_version: PROTOCOL_VERSION,
            },
        );
        let init: InitializeResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(init.protocol_version, PROTOCOL_VERSION);
    }
    // The first connection still works after the second one connected.
    let resp = first.call(methods::FS_LIST, FsListParams { path: "".into() });
    let list: FsListResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(list.entries.len(), 1);
}

/// A daemon-owned session must **survive a client disconnect** and let a
/// reconnecting client reattach to it — replaying the session's history rather
/// than spawning a fresh agent. This is the restart-survival guarantee (Phase
/// 4b2 y2-reattach): the TUI can exit and relaunch without losing the session.
#[test]
fn daemon_session_survives_disconnect_and_replays_on_reattach() {
    let root = tempfile::TempDir::new().unwrap();
    let script = root.path().join("fake_acp.py");
    std::fs::write(&script, FAKE_ACP_AGENT).unwrap();
    let daemon = SocketDaemon::start(root.path());

    let command = format!("python3 {}", script.display());
    let cwd = root.path().display().to_string();

    // First client: start a session and run one prompt turn.
    {
        let mut agent = AgentProcess::spawn_bridge(&daemon.socket);
        agent.call(
            methods::INITIALIZE,
            InitializeParams {
                protocol_version: PROTOCOL_VERSION,
            },
        );
        let resp = agent.call(
            methods::SESSION_START,
            SessionStartParams {
                command: command.clone(),
                cwd: cwd.clone(),
                previous_session_id: None,
                env: Vec::new(),
                mcp: false,
            },
        );
        let started: SessionStartResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(started.session_id.as_deref(), Some("test-session-1"));

        agent.next_notification(methods::SESSION_CONNECTED);
        agent.send_notification(
            methods::SESSION_PROMPT,
            SessionPromptParams {
                text: "ping".into(),
            },
        );
        let note = agent.next_notification(methods::SESSION_UPDATE);
        let update: SessionUpdateParams = serde_json::from_value(note.params.unwrap()).unwrap();
        assert_eq!(update.update["content"]["text"], "pong");
        agent.next_notification(methods::SESSION_PROMPT_RESULT);
        // Dropping `agent` disconnects the client; the session must live on.
    }

    // Second client: reattaching via `session.start` for the same cwd must NOT
    // spawn a new agent. It returns the existing session id and replays the
    // buffered history — including the prior turn's "pong" update — even though
    // this client never sent a prompt.
    {
        let mut agent = AgentProcess::spawn_bridge(&daemon.socket);
        agent.call(
            methods::INITIALIZE,
            InitializeParams {
                protocol_version: PROTOCOL_VERSION,
            },
        );
        let resp = agent.call(
            methods::SESSION_START,
            SessionStartParams {
                command: command.clone(),
                cwd: cwd.clone(),
                previous_session_id: None,
                env: Vec::new(),
                mcp: false,
            },
        );
        let started: SessionStartResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(
            started.session_id.as_deref(),
            Some("test-session-1"),
            "reattach should return the existing session id"
        );

        // Replayed history: the original connected + the prior turn's update.
        let note = agent.next_notification(methods::SESSION_CONNECTED);
        let connected: SessionConnectedParams =
            serde_json::from_value(note.params.unwrap()).unwrap();
        assert_eq!(connected.session_id.as_deref(), Some("test-session-1"));

        let note = agent.next_notification(methods::SESSION_UPDATE);
        let update: SessionUpdateParams = serde_json::from_value(note.params.unwrap()).unwrap();
        assert_eq!(
            update.update["content"]["text"], "pong",
            "reattach should replay the prior turn's update from the buffer"
        );
    }
}

/// Helper: start a daemon-owned session on a fresh bridge connection and wait
/// until it is connected, so the session is registered (keyed by `cwd`) and the
/// connection is the attached host.
fn start_session_host(daemon: &SocketDaemon, command: &str, cwd: &str) -> AgentProcess {
    let mut host = AgentProcess::spawn_bridge(&daemon.socket);
    host.call(
        methods::INITIALIZE,
        InitializeParams {
            protocol_version: PROTOCOL_VERSION,
        },
    );
    host.call(
        methods::SESSION_START,
        SessionStartParams {
            command: command.to_string(),
            cwd: cwd.to_string(),
            previous_session_id: None,
            env: Vec::new(),
            mcp: false,
        },
    );
    host.next_notification(methods::SESSION_CONNECTED);
    host
}

/// The daemon must bridge an in-container MCP relay connection to the session's
/// attached host: `mcp.bind` opens a tunnel (`mcp.open` to the host), and
/// `mcp.data` flows in both directions, stamped with the daemon-assigned tunnel
/// id on the host-facing hop (Feature 2 F2a2b).
#[test]
fn daemon_bridges_mcp_relay_to_session_host() {
    let root = tempfile::TempDir::new().unwrap();
    let script = root.path().join("fake_acp.py");
    std::fs::write(&script, FAKE_ACP_AGENT).unwrap();
    let daemon = SocketDaemon::start(root.path());

    let command = format!("python3 {}", script.display());
    let cwd = root.path().display().to_string();

    let mut host = start_session_host(&daemon, &command, &cwd);

    // A second connection plays the in-container `fleet-agent mcp` relay: it
    // binds using the session cwd as its token.
    let mut relay = AgentProcess::spawn_bridge(&daemon.socket);
    relay.send_notification(methods::MCP_BIND, McpBindParams { token: cwd.clone() });

    // The host is told a tunnel opened and learns its id.
    let open = host.next_notification(methods::MCP_OPEN);
    let open: McpTunnelParams = serde_json::from_value(open.params.unwrap()).unwrap();
    let tunnel_id = open.tunnel_id;
    assert!(tunnel_id > 0);

    // Agent → host: a relay `mcp.data` reaches the host stamped with the id.
    relay.send_notification(
        methods::MCP_DATA,
        McpDataParams {
            tunnel_id: 0,
            message: serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        },
    );
    let note = host.next_notification(methods::MCP_DATA);
    let data: McpDataParams = serde_json::from_value(note.params.unwrap()).unwrap();
    assert_eq!(data.tunnel_id, tunnel_id);
    assert_eq!(data.message["method"], "tools/list");

    // Host → agent: an `mcp.data` the host sends is routed back to the relay.
    host.send_notification(
        methods::MCP_DATA,
        McpDataParams {
            tunnel_id,
            message: serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": { "tools": [] } }),
        },
    );
    let note = relay.next_notification(methods::MCP_DATA);
    let data: McpDataParams = serde_json::from_value(note.params.unwrap()).unwrap();
    assert_eq!(data.message["result"]["tools"], serde_json::json!([]));

    // Dropping the relay connection tears the tunnel down: the host is told.
    drop(relay);
    let close = host.next_notification(methods::MCP_CLOSE);
    let close: McpTunnelParams = serde_json::from_value(close.params.unwrap()).unwrap();
    assert_eq!(close.tunnel_id, tunnel_id);
}

/// When the host closes a tunnel (`mcp.close`), the daemon forwards the close to
/// the in-container relay so it can shut its MCP server down.
#[test]
fn daemon_forwards_host_mcp_close_to_relay() {
    let root = tempfile::TempDir::new().unwrap();
    let script = root.path().join("fake_acp.py");
    std::fs::write(&script, FAKE_ACP_AGENT).unwrap();
    let daemon = SocketDaemon::start(root.path());

    let command = format!("python3 {}", script.display());
    let cwd = root.path().display().to_string();

    let mut host = start_session_host(&daemon, &command, &cwd);

    let mut relay = AgentProcess::spawn_bridge(&daemon.socket);
    relay.send_notification(methods::MCP_BIND, McpBindParams { token: cwd.clone() });
    let open = host.next_notification(methods::MCP_OPEN);
    let open: McpTunnelParams = serde_json::from_value(open.params.unwrap()).unwrap();

    host.send_notification(
        methods::MCP_CLOSE,
        McpTunnelParams {
            tunnel_id: open.tunnel_id,
        },
    );
    // The relay observes the close it can act on.
    assert_eq!(
        relay.next_notification(methods::MCP_CLOSE).method,
        methods::MCP_CLOSE
    );
}

/// Like [`FAKE_ACP_AGENT`] but records the `mcp_servers` it receives in
/// `session/new` to `session_new.json` next to the script, so a test can assert
/// what the daemon injected (Feature 2 F2a2b-2). The recording dir is passed via
/// the `FLEET_TEST_RECORD` env var.
const RECORDING_ACP_AGENT: &str = r#"
import sys, json, os

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid,
              "result": {"protocolVersion": 1, "agentCapabilities": {}, "authMethods": []}})
    elif method == "session/new":
        rec = os.environ.get("FLEET_TEST_RECORD")
        if rec:
            with open(rec, "w") as f:
                json.dump(msg.get("params", {}).get("mcpServers", []), f)
        send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": "test-session-1"}})
    elif method == "authenticate":
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
"#;

/// With `mcp` opted in, the daemon injects a stdio MCP server into `session/new`
/// pointing the in-container agent back at the daemon's own socket, tokened with
/// the session cwd (Feature 2 F2a2b-2).
#[test]
fn daemon_injects_mcp_server_into_session_new() {
    let root = tempfile::TempDir::new().unwrap();
    let script = root.path().join("recording_acp.py");
    std::fs::write(&script, RECORDING_ACP_AGENT).unwrap();
    let record = root.path().join("session_new.json");
    let daemon = SocketDaemon::start(root.path());

    let cwd = root.path().display().to_string();
    // Pass the recording path to the agent child through the session env.
    let command = format!("python3 {}", script.display());

    let mut host = AgentProcess::spawn_bridge(&daemon.socket);
    host.call(
        methods::INITIALIZE,
        InitializeParams {
            protocol_version: PROTOCOL_VERSION,
        },
    );
    host.call(
        methods::SESSION_START,
        SessionStartParams {
            command,
            cwd: cwd.clone(),
            previous_session_id: None,
            env: vec![fleet_protocol::AcpEnvVar {
                name: "FLEET_TEST_RECORD".into(),
                value: record.display().to_string(),
            }],
            mcp: true,
        },
    );
    host.next_notification(methods::SESSION_CONNECTED);

    // The recording agent wrote the mcp_servers it saw in session/new.
    let recorded: serde_json::Value = {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(bytes) = std::fs::read(&record) {
                break serde_json::from_slice(&bytes).unwrap();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "agent never recorded session/new mcp_servers"
            );
            thread::sleep(Duration::from_millis(20));
        }
    };

    let servers = recorded.as_array().expect("mcpServers should be an array");
    assert_eq!(servers.len(), 1, "exactly one MCP server injected");
    let server = &servers[0];
    // ACP serializes McpServer::Stdio untagged (no "type" discriminator).
    assert_eq!(server["name"], "fleet-commander");
    assert!(
        server["command"].as_str().unwrap().ends_with("fleet-agent"),
        "command should be the fleet-agent binary, got {:?}",
        server["command"]
    );
    let args: Vec<String> = server["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        args,
        vec![
            "mcp".to_string(),
            "--socket".to_string(),
            daemon.socket.display().to_string(),
            "--token".to_string(),
            cwd,
        ],
    );
}
