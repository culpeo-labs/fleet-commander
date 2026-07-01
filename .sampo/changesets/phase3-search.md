---
cargo/fleet-commander: minor
---

Content search in the in-container service: `fs.search` walks the workspace (backed by ripgrep's `ignore` + `grep` crates, so it honors `.gitignore` and skips hidden files) and streams `fs.searchResult` match batches followed by a final summary reporting the total count and whether the run was truncated or cancelled. In-flight searches can be stopped with `fs.cancelSearch`. This lands the search engine and wire protocol; the search UI follows.
