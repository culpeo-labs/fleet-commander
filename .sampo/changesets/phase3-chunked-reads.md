---
cargo/fleet-commander: minor
---

Read large files from the in-container service in bounded chunks instead of one giant frame: `fs.read` now accepts an `offset`/`len` range and reports `eof`/`total_size`. The explorer file preview is capped (256 KiB) so opening a huge file no longer transfers or buffers it in full, showing a truncation marker instead.
