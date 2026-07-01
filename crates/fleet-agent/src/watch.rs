//! Filesystem-watch support for [`crate::Server`]: the `fs.watch` handler and
//! the [`WatchHandle`] that owns the `notify` watcher plus the coalescer
//! thread turning raw events into batched `fs.didChange` notifications.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use fleet_protocol::{
    FsDidChangeParams, FsWatchParams, FsWatchResult, Notification, Request, Response, RpcError,
    error_codes, methods,
};
use notify::{RecursiveMode, Watcher};

use crate::Server;
use crate::util::parse_params;

impl Server {
    /// Start or stop the workspace watcher per [`FsWatchParams`].
    pub(super) fn handle_watch(
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
}

/// How long the coalescer batches incoming change events before emitting a
/// single `fs.didChange`. Smooths bursts (a `git checkout` or editor save
/// touches many files) into one client refresh.
const WATCH_COALESCE_WINDOW: Duration = Duration::from_millis(150);

/// Owns an active filesystem watch: the `notify` watcher plus the coalescer
/// thread that turns raw events into `fs.didChange` notifications. Dropping
/// it drops the watcher (which closes the path channel and lets the
/// coalescer exit), so a watch is torn down simply by dropping the handle.
pub(crate) struct WatchHandle {
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
