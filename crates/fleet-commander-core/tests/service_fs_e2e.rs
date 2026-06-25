//! End-to-end test for [`ServiceFs`] over the real [`ProcessTransport`],
//! spawning the actual `fleet-agent` binary.
//!
//! The agent binary lives in a sibling crate, so its `CARGO_BIN_EXE_…`
//! env var isn't available here. We locate it next to the test executable
//! (`target/<profile>/fleet-agent`) and skip when it hasn't been built —
//! the `fleet-agent` crate's own integration test covers the wire format,
//! and `cargo test --workspace` builds the binary so this runs in CI.

use std::path::PathBuf;

use fleet_commander_core::service_fs::ServiceFs;
use fleet_commander_core::workspace_fs::WorkspaceFs;

fn agent_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // exe = target/<profile>/deps/<test-bin>; go up to target/<profile>.
    let dir = exe.parent()?.parent()?;
    let bin = dir.join(if cfg!(windows) {
        "fleet-agent.exe"
    } else {
        "fleet-agent"
    });
    bin.exists().then_some(bin)
}

#[test]
fn service_fs_drives_a_real_agent_process() {
    let Some(agent) = agent_binary() else {
        eprintln!("skipping: fleet-agent binary not built");
        return;
    };

    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("README.md"), b"hello world").unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), b"fn a(){}").unwrap();

    let fs = ServiceFs::spawn(tmp.path(), &agent).expect("spawn + initialize");

    let mut entries = fs.list_dir(std::path::Path::new("")).unwrap();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["README.md", "src"]);
    assert!(entries.iter().find(|e| e.name == "src").unwrap().is_dir);

    let bytes = fs.read_file(std::path::Path::new("README.md")).unwrap();
    assert_eq!(bytes, b"hello world");

    // Not a git repo → branch is None, status falls back cleanly.
    assert_eq!(fs.git_branch(), None);
    assert!(fs.git_status(false).is_err());

    // Path traversal is rejected by the daemon and surfaces as an error.
    assert!(fs.read_file(std::path::Path::new("../escape")).is_err());
}

#[test]
fn service_fs_delivers_live_fs_did_change_notifications() {
    use std::sync::mpsc;

    let Some(agent) = agent_binary() else {
        eprintln!("skipping: fleet-agent binary not built");
        return;
    };

    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("seed.txt"), b"seed").unwrap();

    // The sink runs on the transport's reader thread; forward each
    // notification's method name over a channel so the test can await it.
    let (tx, rx) = mpsc::channel::<String>();
    let sink: fleet_commander_core::service_fs::NotificationSink = Box::new(move |note| {
        let _ = tx.send(note.method);
    });

    // Keep the connection alive for the duration of the test; dropping it
    // would tear down the watch.
    let _fs = ServiceFs::spawn_watched(tmp.path(), &agent, Some(sink)).expect("spawn + watch");

    // Mutate the workspace; the daemon should push an fs.didChange that the
    // sink forwards to us.
    std::fs::write(tmp.path().join("created.txt"), b"new").unwrap();

    let method = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("expected an fs.didChange notification");
    assert_eq!(method, "fs.didChange");
}
