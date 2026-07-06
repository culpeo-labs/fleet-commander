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
    Notification, PROTOCOL_VERSION, Request, Response, SearchAck, SearchDoneParams, SearchParams,
    SearchResultParams, SessionConnectedParams, SessionPromptParams, SessionPromptResultParams,
    SessionStartParams, SessionStartResult, SessionUpdateParams, framing, methods,
};

/// Generous ceiling for any single blocking read so a protocol regression
/// fails the test instead of hanging CI.
const RECV_TIMEOUT: Duration = Duration::from_secs(10);

struct AgentProcess {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Incoming>,
    pending: Vec<Notification>,
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
