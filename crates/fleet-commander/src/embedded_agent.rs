//! Optionally-embedded `fleet-agent` musl binaries (release `embed-agent`
//! feature; see `build.rs`).
//!
//! Embedding ships the agent *inside* the commander, so its protocol version
//! can never skew from the client's. On startup the binaries are written into
//! the conventional host bin dir, where the container mount + `uname -m`
//! launcher pick the right arch (see `fleet_commander_core::agent_bin`).
//!
//! When the feature is off, `EMBEDDED_AGENTS` is empty and install is a no-op;
//! the agent is then resolved from a developer build (`FLEET_AGENT_BIN` /
//! `scripts/build-fleet-agent.sh`) or the explorer falls back to `LocalFs`.

include!(concat!(env!("OUT_DIR"), "/embedded_agents.rs"));

use tracing::{info, warn};

/// Materialize every embedded agent binary into the host bin dir. Best-effort:
/// failures are logged and leave the resolver to fall back.
pub fn install_embedded_agents() {
    for (slug, bytes) in EMBEDDED_AGENTS {
        match fleet_commander_core::agent_bin::install_embedded_binary(slug, bytes) {
            Ok(path) => {
                info!(arch = slug, path = %path.display(), "Installed embedded fleet-agent")
            }
            Err(e) => warn!(arch = slug, error = %e, "Failed to install embedded fleet-agent"),
        }
    }
}
