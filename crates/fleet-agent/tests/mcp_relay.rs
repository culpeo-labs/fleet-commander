//! End-to-end test for the `fleet-agent mcp` relay subcommand: spawn the real
//! binary, act as the daemon on the other end of its unix socket, and prove it
//! translates MCP stdio (newline JSON) ↔ `mcp.*` frames in both directions.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::time::Duration;

use fleet_protocol::{
    McpBindParams, McpDataParams, McpTunnelParams, Notification, framing, methods,
};

/// Read the next framed `mcp.*` notification from the daemon-side socket.
fn next_note<R: BufRead>(reader: &mut R) -> Notification {
    let body = framing::read_frame(reader)
        .expect("read frame")
        .expect("frame present");
    serde_json::from_slice(&body).expect("parse notification")
}

#[test]
fn mcp_relay_translates_both_directions() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_fleet-agent"))
        .arg("mcp")
        .arg("--socket")
        .arg(&socket)
        .arg("--token")
        .arg("tok-xyz")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn fleet-agent mcp");

    let mut child_stdin = child.stdin.take().unwrap();
    let mut child_stdout = BufReader::new(child.stdout.take().unwrap());

    let (conn, _) = listener.accept().expect("accept relay connection");
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut daemon_read = BufReader::new(conn.try_clone().unwrap());
    let mut daemon_write = conn;

    // 1. The relay announces itself with its token before anything else.
    let bind = next_note(&mut daemon_read);
    assert_eq!(bind.method, methods::MCP_BIND);
    let bind: McpBindParams = serde_json::from_value(bind.params.unwrap()).unwrap();
    assert_eq!(bind.token, "tok-xyz");

    // 2. Agent → host: an MCP request line becomes an mcp.data frame.
    writeln!(
        child_stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list"}}"#
    )
    .unwrap();
    child_stdin.flush().unwrap();

    let data = next_note(&mut daemon_read);
    assert_eq!(data.method, methods::MCP_DATA);
    let data: McpDataParams = serde_json::from_value(data.params.unwrap()).unwrap();
    assert_eq!(data.message["method"], "tools/list");
    assert_eq!(data.message["id"], 1);

    // 3. Host → agent: an mcp.data frame becomes an MCP stdout line.
    let response = Notification::new(
        methods::MCP_DATA,
        McpDataParams {
            tunnel_id: 42,
            message: serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "tools": [] }
            }),
        },
    );
    framing::write_frame(&mut daemon_write, &serde_json::to_vec(&response).unwrap()).unwrap();
    daemon_write.flush().unwrap();

    let mut line = String::new();
    child_stdout.read_line(&mut line).expect("read stdout line");
    let echoed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(echoed["result"]["tools"], serde_json::json!([]));
    assert_eq!(echoed["id"], 1);

    // 4. Host closes the tunnel → the relay process exits.
    let close = Notification::new(methods::MCP_CLOSE, McpTunnelParams { tunnel_id: 42 });
    framing::write_frame(&mut daemon_write, &serde_json::to_vec(&close).unwrap()).unwrap();
    daemon_write.flush().unwrap();

    let status = child.wait().expect("relay exits");
    assert!(status.success());
}

#[test]
fn mcp_relay_emits_close_when_agent_stdin_ends() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_fleet-agent"))
        .arg("mcp")
        .arg("--socket")
        .arg(&socket)
        .arg("--token")
        .arg("tok")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn fleet-agent mcp");

    let child_stdin = child.stdin.take().unwrap();
    let (conn, _) = listener.accept().unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut daemon_read = BufReader::new(conn);

    // Consume the bind frame.
    assert_eq!(next_note(&mut daemon_read).method, methods::MCP_BIND);

    // Closing the agent's stdin (its MCP client went away) must surface as an
    // mcp.close so the daemon can tear the host tunnel down.
    drop(child_stdin);
    assert_eq!(next_note(&mut daemon_read).method, methods::MCP_CLOSE);

    let _ = child.wait();
}
