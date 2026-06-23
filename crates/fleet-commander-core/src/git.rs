//! Git inspection helpers.
//!
//! The implementation lives in the standalone, dependency-light
//! [`fleet_git`] crate so it can be shared with `fleet-agent` (which runs
//! inside the container and must stay lean) without pulling in the rest of
//! this crate's dependency tree. Re-exported here so existing
//! `crate::git::…` call sites are unchanged.

pub use fleet_git::*;
