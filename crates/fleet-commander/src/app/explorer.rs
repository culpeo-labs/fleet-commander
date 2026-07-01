//! Explorer-facing side of [`super::App`]: servicing the lazy remote tree,
//! opening file previews and diffs, and refreshing git status. The reads may
//! hit a container, so they run off the UI thread and report back as
//! `AppEvent`s.

use std::path::PathBuf;

use crate::event::AppEvent;

use super::App;

impl App {
    /// Keep the explorer's remote view consistent after any event:
    /// schedule background fetches for directories the current tree
    /// needs but hasn't cached, and service a pending file-open. Cheap
    /// no-op for a closed explorer or a local (synchronous) filesystem.
    pub(super) fn sync_explorer(&mut self) {
        if !self.explorer.open {
            return;
        }
        if let Some(rel) = self.explorer.pending_open.take() {
            match self.explorer.pending_open_line.take() {
                Some(line) => self.open_search_result(rel, line),
                None => self.open_explorer_file(rel),
            }
        }
        let Some(fs) = self.explorer.fs.clone() else {
            return;
        };
        if !fs.is_remote() {
            return;
        }
        let root = fs.root_display().to_path_buf();
        for rel in self.explorer.missing_dirs() {
            self.explorer.mark_dir_loading(rel.clone());
            let fs = fs.clone();
            let root = root.clone();
            let tx = self.tx.clone();
            tokio::task::spawn_blocking(move || {
                let result = fs.list_dir(&rel).map_err(|e| e.to_string());
                let _ = tx.send(AppEvent::ExplorerDirReady { root, rel, result });
            });
        }
    }

    /// Read a file for the explorer's side-pane preview off the UI
    /// thread (the read may be a remote RPC) and deliver it as
    /// [`AppEvent::ExplorerFileReady`].
    pub(super) fn open_explorer_file(&self, rel: PathBuf) {
        let Some(fs) = self.explorer.fs.clone() else {
            return;
        };
        let Some(agent_id) = self.viewed_agent_id() else {
            return;
        };
        let root = fs.root_display().to_path_buf();
        let full_path = root.join(&rel);
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            // Cap the preview so opening a huge file never transfers or
            // buffers it in full on the UI path. 256 KiB is plenty for a
            // glance; larger files show a truncation marker.
            const PREVIEW_CAP: u64 = 256 * 1024;
            let result = fs
                .read_file_capped(&rel, PREVIEW_CAP)
                .map(|capped| {
                    let mut text = String::from_utf8_lossy(&capped.bytes).into_owned();
                    if capped.truncated {
                        text.push_str("\n\n… [truncated preview — file larger than 256 KiB]");
                    }
                    text
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::ExplorerFileReady {
                agent_id,
                root,
                full_path,
                result,
                scroll_to: 0,
            });
        });
    }

    /// Fetch a `git diff` for an explorer-selected file off the UI
    /// thread (the diff may be a remote RPC) and deliver it as
    /// [`AppEvent::ExplorerDiffReady`]. Shows the working-tree diff.
    pub(super) fn request_explorer_diff(&self, rel: PathBuf) {
        let Some(fs) = self.explorer.fs.clone() else {
            return;
        };
        let Some(agent_id) = self.viewed_agent_id() else {
            return;
        };
        let root = fs.root_display().to_path_buf();
        let full_path = root.join(&rel);
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = fs.git_diff(&rel, false).map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::ExplorerDiffReady {
                agent_id,
                root,
                full_path,
                result,
            });
        });
    }

    /// Spawn a background `git status` for the active workspace and
    /// pump the result back into the event loop as
    /// [`AppEvent::ExplorerStatusReady`]. Coalesces bursty callers:
    /// if a refresh is already in flight, sets a pending flag so a
    /// follow-up runs once the in-flight one lands.
    ///
    /// Cheap no-op when the explorer has no filesystem attached **or
    /// is closed** — there's no point spending cycles updating git
    /// state the user can't see.
    pub fn request_explorer_refresh(&mut self) {
        if !self.explorer.open {
            return;
        }
        let Some(fs) = self.explorer.fs.clone() else {
            return;
        };
        if self.explorer.is_refreshing {
            self.explorer.refresh_pending = true;
            return;
        }
        self.explorer.is_refreshing = true;
        self.explorer.refresh_pending = false;
        let include_ignored = self.explorer.show_ignored;
        let root = fs.root_display().to_path_buf();
        let tx = self.tx.clone();
        // `git status` is a sync subprocess; off-load it to the blocking
        // pool so the UI loop keeps draining events while it runs.
        tokio::task::spawn_blocking(move || {
            let result = fs.git_status(include_ignored).map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::ExplorerStatusReady {
                root,
                include_ignored,
                result,
            });
        });
    }
}
