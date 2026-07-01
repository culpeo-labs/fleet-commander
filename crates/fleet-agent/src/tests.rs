use super::*;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use fleet_protocol::{
    FsListResult, FsReadParams, FsReadResult, GitBranchResult, InitializeResult, PROTOCOL_VERSION,
};
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
    std::os::unix::fs::symlink(tmp.path().join("README.md"), tmp.path().join("link.md")).unwrap();
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
