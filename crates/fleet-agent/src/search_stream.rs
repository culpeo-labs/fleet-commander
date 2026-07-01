//! Streaming content-search orchestration for [`crate::Server`]: the
//! `fs.search` handler (immediate ack + spawned worker), the in-flight
//! [`SearchState`] registry, the worker body, and `fs.cancelSearch`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};

use fleet_protocol::{
    CancelSearchParams, CancelSearchResult, Notification, Request, Response, SearchAck,
    SearchDoneParams, SearchParams, SearchResultParams, SearchSummary, methods,
};

use crate::Server;
use crate::search::{self, SearchRequest, run_search};
use crate::util::{parse_params, send_body, to_vec_lossy};

impl Server {
    /// Register and spawn a streaming content-search worker, returning an
    /// immediate [`SearchAck`] [`Response`]. The worker pushes
    /// [`methods::FS_SEARCH_RESULT`] notification batches to `out` and a
    /// terminal [`methods::FS_SEARCH_DONE`] notification carrying the
    /// [`SearchSummary`]. The search's cancel flag is registered in `searches`
    /// under its `searchId` so a later [`methods::FS_CANCEL_SEARCH`] can stop it.
    pub(super) fn start_search(
        &self,
        req: &Request,
        out: &mpsc::Sender<Vec<u8>>,
        searches: &mut SearchState,
    ) -> Response {
        let params: SearchParams = match parse_params(req) {
            Ok(p) => p,
            Err(e) => return Response::err(req.id, e),
        };
        // Reject an invalid pattern up front so the client gets a clean
        // negative ack instead of a silently-empty streamed result.
        let search_req = SearchRequest {
            query: params.query.clone(),
            is_regex: params.is_regex,
            case_sensitive: params.case_sensitive,
            max_results: params.max_results,
        };
        if search::validate(&search_req).is_err() {
            return Response::ok(req.id, SearchAck { accepted: false });
        }
        let search_id = params.search_id;
        let cancel = Arc::new(AtomicBool::new(false));
        searches.register(search_id, cancel.clone());

        let root = self.root.clone();
        let out = out.clone();
        let active = searches.active.clone();
        let handle = thread::Builder::new()
            .name("fleet-agent-search".into())
            .spawn(move || {
                run_search_worker(search_id, root, params, cancel, out);
                // Drop our registration so a stale searchId can't be cancelled
                // (and to bound the map to live searches).
                active.lock().unwrap().remove(&search_id);
            })
            .ok();
        if let Some(handle) = handle {
            searches.workers.push(handle);
        }
        Response::ok(req.id, SearchAck { accepted: true })
    }
}

/// How many matches to accumulate before flushing an `fs.searchResult`
/// notification. Bounds per-frame size for a match-dense search while
/// keeping the stream responsive.
const SEARCH_BATCH: usize = 128;

/// Tracks in-flight [`methods::FS_SEARCH`] workers so they can be cancelled
/// individually (by `searchId`) and joined on shutdown.
#[derive(Default)]
pub(crate) struct SearchState {
    /// `searchId` → its cancellation flag. Workers remove their own entry on
    /// completion, so a present entry means "still running".
    active: Arc<std::sync::Mutex<HashMap<u64, Arc<AtomicBool>>>>,
    workers: Vec<JoinHandle<()>>,
}

impl SearchState {
    fn register(&self, search_id: u64, cancel: Arc<AtomicBool>) {
        self.active.lock().unwrap().insert(search_id, cancel);
    }

    /// Signal every in-flight search to stop and join all worker threads.
    /// Called on daemon teardown so no worker outlives the writer channel.
    pub(crate) fn shutdown(&mut self) {
        for cancel in self.active.lock().unwrap().values() {
            cancel.store(true, Ordering::Relaxed);
        }
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}

/// Body of a search worker thread: walk + match, streaming coalesced
/// result batches, then a terminal [`methods::FS_SEARCH_DONE`] notification.
/// The pattern is validated before the worker is spawned, so `run_search`
/// only fails here on an unexpected build error, which we surface as a
/// finished (empty) search rather than crashing the daemon.
fn run_search_worker(
    search_id: u64,
    root: PathBuf,
    params: SearchParams,
    cancel: Arc<AtomicBool>,
    out: mpsc::Sender<Vec<u8>>,
) {
    let req = SearchRequest {
        query: params.query,
        is_regex: params.is_regex,
        case_sensitive: params.case_sensitive,
        max_results: params.max_results,
    };

    let mut batch: Vec<fleet_protocol::SearchMatch> = Vec::with_capacity(SEARCH_BATCH);
    let flush = |batch: &mut Vec<fleet_protocol::SearchMatch>| {
        if batch.is_empty() {
            return;
        }
        let note = Notification::new(
            methods::FS_SEARCH_RESULT,
            SearchResultParams {
                search_id,
                matches: std::mem::take(batch),
            },
        );
        let _ = send_body(&out, &to_vec_lossy(&note));
    };

    let outcome = run_search(&root, &req, &cancel, |m| {
        batch.push(m);
        if batch.len() >= SEARCH_BATCH {
            flush(&mut batch);
        }
    });

    // Flush any tail matches before the terminal notification so ordering holds.
    flush(&mut batch);

    let summary = outcome.unwrap_or_default();
    let done = Notification::new(
        methods::FS_SEARCH_DONE,
        SearchDoneParams {
            search_id,
            summary: SearchSummary {
                count: summary.count,
                truncated: summary.truncated,
                cancelled: summary.cancelled,
            },
        },
    );
    let _ = send_body(&out, &to_vec_lossy(&done));
}

/// Handle [`methods::FS_CANCEL_SEARCH`]: flip the target search's cancel
/// flag if it's still running.
pub(crate) fn handle_cancel_search(req: &Request, searches: &SearchState) -> Response {
    let params: CancelSearchParams = match parse_params(req) {
        Ok(p) => p,
        Err(e) => return Response::err(req.id, e),
    };
    let cancelled = match searches.active.lock().unwrap().get(&params.search_id) {
        Some(cancel) => {
            cancel.store(true, Ordering::Relaxed);
            true
        }
        None => false,
    };
    Response::ok(req.id, CancelSearchResult { cancelled })
}
