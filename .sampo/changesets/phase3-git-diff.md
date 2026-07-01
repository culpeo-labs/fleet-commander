---
cargo/fleet-commander: minor
---

View a file's git diff from the explorer: press `Shift+D` on a changed file to open its working-tree diff in the side pane. Backed by a new in-container `git.diff` method (untracked files render as all-additions), so diffs reflect what the agent sees inside the container.
