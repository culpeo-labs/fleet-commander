#!/usr/bin/env bash
#
# Build the `fleet-agent` daemon as a static musl binary and install it to the
# conventional location Fleet Commander resolves at container-start time:
#
#     ${XDG_DATA_HOME:-$HOME/.local/share}/fleet-commander/bin/fleet-agent-<arch>
#
# Fleet Commander bind-mounts this binary read-only into each dev container at
# /opt/fleet/bin/fleet-agent (see crates/fleet-commander-core/src/agent_bin.rs).
# A static musl build runs on any Linux distro regardless of the container's
# glibc; only the architecture must match the container.
#
# Usage:
#   scripts/build-fleet-agent.sh [<arch>]
#
#   <arch>   x86_64 (default) or aarch64 — the *container* architecture.
#            Defaults to the host architecture (`uname -m`).
#
# Requires the matching Rust musl target:
#   rustup target add x86_64-unknown-linux-musl
#   rustup target add aarch64-unknown-linux-musl
set -euo pipefail

arch="${1:-$(uname -m)}"
case "$arch" in
  x86_64|amd64)   arch="x86_64";  target="x86_64-unknown-linux-musl" ;;
  aarch64|arm64)  arch="aarch64"; target="aarch64-unknown-linux-musl" ;;
  *) echo "error: unsupported arch '$arch' (expected x86_64 or aarch64)" >&2; exit 1 ;;
esac

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}/fleet-commander/bin"
dest="$data_dir/fleet-agent-$arch"

echo "Building fleet-agent for $target ..."
cargo build --release --locked -p fleet-agent --target "$target" --manifest-path "$repo_root/Cargo.toml"

mkdir -p "$data_dir"
install -m 0755 "$repo_root/target/$target/release/fleet-agent" "$dest"
echo "Installed $dest"
