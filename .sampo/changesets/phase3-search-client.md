---
cargo/fleet-commander: minor
---

Wire the in-container search into the client: `ServiceFs` now exposes `start_search`/`cancel_search`, and the daemon streams results as notifications so a long-running search never blocks other requests (including its own cancel). `fs.search` returns an immediate ack, streams `fs.searchResult` batches, and finishes with an `fs.searchDone` summary; an invalid pattern is rejected up front. Results flow through the existing notification sink, setting up the search UI to follow.
