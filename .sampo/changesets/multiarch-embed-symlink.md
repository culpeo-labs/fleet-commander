---
cargo/fleet-commander: minor
---

Inject the in-container `fleet-agent` daemon across architectures: per-arch
static-musl binaries are mounted into the container and the right one is picked
at exec time by a `uname -m` launcher. Release builds now embed both agents (the
`embed-agent` feature), so the file/git explorer reflects the container's
filesystem out of the box instead of falling back to the host. Also hardened the
agent's path resolver against symlink escapes outside the workspace root.
