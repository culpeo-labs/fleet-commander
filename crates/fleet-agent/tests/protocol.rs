//! End-to-end test: spawn the real `fleet-agent` binary and drive it over
//! its stdio pipes using the wire protocol, exactly as the TUI's
//! `ServiceFs` will. Proves framing + dispatch across a process boundary.

use std::io::{BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use fleet_protocol::{
    FsListParams, FsListResult, FsReadParams, FsReadResult, GitBranchResult, InitializeParams,
    InitializeResult, PROTOCOL_VERSION, Request, Response, framing, methods,
};

struct AgentProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
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
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        }
    }

    fn call(&mut self, method: &str, params: impl serde::Serialize) -> Response {
        self.next_id += 1;
        let req = Request::new(self.next_id, method, params);
        let body = serde_json::to_vec(&req).unwrap();
        framing::write_frame(&mut self.stdin, &body).unwrap();
        self.stdin.flush().unwrap();
        let resp_body = framing::read_frame(&mut self.stdout)
            .unwrap()
            .expect("expected a response frame");
        serde_json::from_slice(&resp_body).unwrap()
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
