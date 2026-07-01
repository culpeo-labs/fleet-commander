//! End-to-end test: spawn the real `fleet-agent` binary and drive it over
//! its stdio pipes using the wire protocol, exactly as the TUI's
//! `ServiceFs` will. Proves framing + dispatch across a process boundary.

use std::io::{BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use fleet_protocol::{
    CancelSearchParams, CancelSearchResult, FsDidChangeParams, FsListParams, FsListResult,
    FsReadParams, FsReadResult, FsWatchParams, FsWatchResult, GitBranchResult, Incoming,
    InitializeParams, InitializeResult, Notification, PROTOCOL_VERSION, Request, Response,
    SearchAck, SearchDoneParams, SearchParams, SearchResultParams, framing, methods,
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
        let mut child = Command::new(env!("CARGO_BIN_EXE_fleet-agent"))
            .arg("serve")
            .arg("--root")
            .arg(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn fleet-agent");
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
