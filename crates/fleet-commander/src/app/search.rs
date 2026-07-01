//! Content-search side of [`super::App`]: launching a streaming
//! `fs.search`, cancelling it, and opening a hit in the file preview.

use std::path::PathBuf;

use tracing::info;

use crate::event::AppEvent;

use super::{App, Screen, SessionFocus, SidePane};

/// Cap on how many content-search hits the daemon streams back before it
/// stops and flags the result truncated. Keeps a broad match on a large tree
/// from flooding the UI.
const SEARCH_MAX_RESULTS: u64 = 2_000;

impl App {
    /// Launch a streaming workspace content search for `query`. Cancels any
    /// still-running search first, opens a fresh [`SidePane::Search`] focused
    /// for navigation, and kicks off `fs.start_search` off the UI thread —
    /// results stream back via the notification sink as `SearchResults`/
    /// `SearchDone` events. No-op for an empty query or a non-search backend.
    pub(super) fn launch_search(&mut self, query: String) {
        let query = query.trim().to_string();
        if query.is_empty() {
            return;
        }
        let Some(fs) = self.explorer.fs.clone() else {
            return;
        };
        if !fs.is_remote() {
            self.status_message = Some("Search needs a container-backed workspace".into());
            return;
        }
        // Stop a previous in-flight search so its late results can't bleed
        // into the new pane (they carry the old search_id and are dropped,
        // but cancelling also frees the daemon worker).
        if let Some(prev) = self.running_search_id() {
            self.cancel_search(prev);
        }

        let search_id = self.search_next_id;
        self.search_next_id += 1;

        if let Screen::AgentSession {
            side_pane, focus, ..
        } = &mut self.screen
        {
            *side_pane = Some(SidePane::Search {
                query: query.clone(),
                search_id,
                matches: Vec::new(),
                selected: 0,
                scroll: 0,
                running: true,
                summary: None,
            });
            *focus = SessionFocus::SidePane;
        }

        let params = fleet_commander_core::fleet_protocol::SearchParams {
            search_id,
            query,
            is_regex: false,
            case_sensitive: false,
            max_results: Some(SEARCH_MAX_RESULTS),
        };
        let tx = self.tx.clone();
        let agent_id = self.viewed_agent_id();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = fs.start_search(params) {
                info!(error = %e, "start_search failed");
                // Signal completion so the pane stops showing "searching…".
                if let Some(agent_id) = agent_id {
                    let _ = tx.send(AppEvent::SearchDone {
                        agent_id,
                        search_id,
                        summary: fleet_commander_core::fleet_protocol::SearchSummary {
                            count: 0,
                            truncated: false,
                            cancelled: true,
                        },
                    });
                }
            }
        });
    }

    /// The id of the currently-visible search if it is still running,
    /// otherwise `None`.
    pub(super) fn running_search_id(&self) -> Option<u64> {
        match &self.screen {
            Screen::AgentSession {
                side_pane:
                    Some(SidePane::Search {
                        search_id,
                        running: true,
                        ..
                    }),
                ..
            } => Some(*search_id),
            _ => None,
        }
    }

    /// Ask the in-container service to stop `search_id` off the UI thread.
    /// Best-effort: the pane's `running` flag clears when the (still
    /// delivered) `fs.searchDone` summary arrives flagged cancelled.
    pub(super) fn cancel_search(&self, search_id: u64) {
        let Some(fs) = self.explorer.fs.clone() else {
            return;
        };
        tokio::task::spawn_blocking(move || {
            let _ = fs.cancel_search(search_id);
        });
    }

    /// Open a search result in the side pane, jumping the preview to the
    /// match's line. Reads off the UI thread (a possibly-remote RPC) and
    /// delivers [`AppEvent::ExplorerFileReady`] with the target scroll.
    pub(super) fn open_search_result(&self, rel: PathBuf, line: u64) {
        let Some(fs) = self.explorer.fs.clone() else {
            return;
        };
        let Some(agent_id) = self.viewed_agent_id() else {
            return;
        };
        let root = fs.root_display().to_path_buf();
        let full_path = root.join(&rel);
        // Center the match a few lines below the top of the viewport.
        let scroll_to = (line.saturating_sub(1)) as u16;
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
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
                scroll_to,
            });
        });
    }
}
