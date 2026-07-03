//! The `fleet-agent` daemon: serves filesystem and git inspection for a
//! single workspace root over JSON-RPC 2.0 on stdio.
//!
//! Phase 0 runs this as a local host child process to prove the protocol
//! end-to-end; Phase 1 bind-mounts the same binary into a dev container and
//! drives it via `docker exec -i`. The serve loop is therefore written
//! against generic [`BufRead`]/[`Write`] so the transport is interchangeable.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use fleet_protocol::{Notification, Request, Response, RpcError, error_codes, framing, methods};

mod acp;
mod handlers;
mod search;
mod search_stream;
mod session;
mod util;
mod watch;

use acp::{AcpChild, handle_acp_send, handle_acp_stop};
use search_stream::{SearchState, handle_cancel_search};
use session::{
    SessionHandle, handle_session_cancel, handle_session_permission_respond, handle_session_prompt,
};
use util::send_body;
use watch::WatchHandle;

/// Serves requests against a fixed workspace `root`.
pub struct Server {
    root: PathBuf,
    /// `root` with all symlinks resolved, used to verify that a resolved
    /// request path stays inside the workspace even when it traverses a
    /// symlink. Falls back to `root` if the workspace can't be canonicalized.
    canonical_root: PathBuf,
}

impl Server {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        Self {
            root,
            canonical_root,
        }
    }

    /// Read framed requests from `reader` until EOF, dispatching each and
    /// writing a framed response to `writer`. A frame that fails to parse as
    /// a request yields a best-effort JSON-RPC parse error.
    pub fn serve<R: BufRead, W: Write>(&self, reader: &mut R, writer: &mut W) -> io::Result<()> {
        while let Some(body) = framing::read_frame(reader)? {
            let response = match serde_json::from_slice::<Request>(&body) {
                Ok(req) => self.handle(&req),
                Err(e) => Response::err(
                    0,
                    RpcError::new(error_codes::PARSE_ERROR, format!("invalid request: {e}")),
                ),
            };
            let out = serde_json::to_vec(&response)?;
            framing::write_frame(writer, &out)?;
        }
        Ok(())
    }

    /// Like [`serve`](Self::serve) but watch-aware: outbound frames
    /// (responses **and** server-initiated notifications) are serialized
    /// through a dedicated writer thread, so an [`methods::FS_WATCH`]
    /// subscription can push [`methods::FS_DID_CHANGE`] notifications
    /// concurrently with request handling. This is the production entry
    /// point (stdio over `docker exec -i`); [`serve`](Self::serve) remains
    /// the simple synchronous path for the no-watch case.
    pub fn serve_stdio<R, W>(&self, reader: &mut R, writer: W) -> io::Result<()>
    where
        R: BufRead,
        W: Write + Send + 'static,
    {
        let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
        let writer_handle = thread::Builder::new()
            .name("fleet-agent-writer".into())
            .spawn(move || {
                let mut writer = writer;
                while let Ok(frame) = out_rx.recv() {
                    if framing::write_frame(&mut writer, &frame).is_err() {
                        break;
                    }
                }
            })?;

        let mut watch: Option<WatchHandle> = None;
        let mut searches = SearchState::default();
        let mut acp: Option<AcpChild> = None;
        let mut session: Option<SessionHandle> = None;
        let result = self.dispatch_loop(
            reader,
            &out_tx,
            &mut watch,
            &mut searches,
            &mut acp,
            &mut session,
        );

        // Tear down in order: cancel + join any in-flight searches, stop the
        // ACP child / session, and stop the watcher (all hold `out` senders),
        // then drop our sender so the writer thread sees the channel close and
        // exits, then join it.
        searches.shutdown();
        drop(session);
        drop(acp);
        drop(watch);
        drop(out_tx);
        let _ = writer_handle.join();
        result
    }

    /// Read framed requests, dispatching each and sending its response to the
    /// outbound `out` channel. Long-running/streaming methods are handled
    /// out-of-band: `fs.watch` manages a subscription, and `fs.search` spawns
    /// a worker that streams results and sends the final response itself.
    fn dispatch_loop<R: BufRead>(
        &self,
        reader: &mut R,
        out: &mpsc::Sender<Vec<u8>>,
        watch: &mut Option<WatchHandle>,
        searches: &mut SearchState,
        acp: &mut Option<AcpChild>,
        session: &mut Option<SessionHandle>,
    ) -> io::Result<()> {
        while let Some(body) = framing::read_frame(reader)? {
            // Peek at the raw object to tell requests (have an `id`, expect a
            // response) from notifications (no `id`, fire-and-forget, e.g. the
            // high-frequency `acp.send`).
            let value: serde_json::Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => {
                    let response = Response::err(
                        0,
                        RpcError::new(error_codes::PARSE_ERROR, format!("invalid request: {e}")),
                    );
                    send_body(out, &serde_json::to_vec(&response)?)?;
                    continue;
                }
            };

            if value.get("id").is_none() {
                // Client→server notification: no response is sent.
                if let Ok(note) = serde_json::from_value::<Notification>(value) {
                    match note.method.as_str() {
                        methods::ACP_SEND => handle_acp_send(&note, acp),
                        methods::SESSION_PROMPT => handle_session_prompt(&note, session),
                        methods::SESSION_PERMISSION_RESPOND => {
                            handle_session_permission_respond(&note, session)
                        }
                        _ => {}
                    }
                }
                continue;
            }

            let req = match serde_json::from_value::<Request>(value) {
                Ok(req) => req,
                Err(e) => {
                    let response = Response::err(
                        0,
                        RpcError::new(error_codes::PARSE_ERROR, format!("invalid request: {e}")),
                    );
                    send_body(out, &serde_json::to_vec(&response)?)?;
                    continue;
                }
            };
            match req.method.as_str() {
                methods::FS_WATCH => {
                    let response = self.handle_watch(&req, out, watch);
                    send_body(out, &serde_json::to_vec(&response)?)?;
                }
                // `start_search` returns an immediate ack Response; results
                // (and the terminal summary) stream as notifications.
                methods::FS_SEARCH => {
                    let response = self.start_search(&req, out, searches);
                    send_body(out, &serde_json::to_vec(&response)?)?;
                }
                methods::FS_CANCEL_SEARCH => {
                    let response = handle_cancel_search(&req, searches);
                    send_body(out, &serde_json::to_vec(&response)?)?;
                }
                methods::ACP_START => {
                    let response = self.handle_acp_start(&req, out, acp);
                    send_body(out, &serde_json::to_vec(&response)?)?;
                }
                methods::ACP_STOP => {
                    let response = handle_acp_stop(&req, acp);
                    send_body(out, &serde_json::to_vec(&response)?)?;
                }
                methods::SESSION_START => {
                    let response = self.handle_session_start(&req, out, session);
                    send_body(out, &serde_json::to_vec(&response)?)?;
                }
                methods::SESSION_CANCEL => {
                    let response = handle_session_cancel(&req, session);
                    send_body(out, &serde_json::to_vec(&response)?)?;
                }
                _ => {
                    let response = self.handle(&req);
                    send_body(out, &serde_json::to_vec(&response)?)?;
                }
            }
        }
        Ok(())
    }

    /// Dispatch a single request to its handler.
    pub fn handle(&self, req: &Request) -> Response {
        let result = match req.method.as_str() {
            methods::INITIALIZE => self.initialize(),
            methods::FS_LIST => self.fs_list(req),
            methods::FS_READ => self.fs_read(req),
            methods::FS_STAT => self.fs_stat(req),
            methods::GIT_STATUS => self.git_status(req),
            methods::GIT_BRANCH => self.git_branch(),
            methods::GIT_DIFF => self.git_diff(req),
            other => Err(RpcError::new(
                error_codes::METHOD_NOT_FOUND,
                format!("unknown method: {other}"),
            )),
        };
        match result {
            Ok(value) => Response::ok(req.id, value),
            Err(error) => Response::err(req.id, error),
        }
    }
}

#[cfg(test)]
mod tests;
