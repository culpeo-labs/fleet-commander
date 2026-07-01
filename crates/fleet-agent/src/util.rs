//! Small shared helpers for the daemon: JSON-RPC (de)serialization,
//! path canonicalization, frame sending, and error/status conversions.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use fleet_protocol::{Request, RpcError, WireStatus, error_codes};

/// Canonicalize `path`, tolerating a non-existent leaf: if the full path
/// doesn't exist, resolve the nearest existing ancestor (expanding symlinks)
/// and re-append the missing trailing components. Errors other than
/// "not found" (e.g. a permission error) propagate unchanged.
pub(crate) fn canonicalize_existing_prefix(path: &Path) -> io::Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(real) => Ok(real),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or(e)?;
            let leaf = path
                .file_name()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            Ok(canonicalize_existing_prefix(parent)?.join(leaf))
        }
        Err(e) => Err(e),
    }
}

pub(crate) fn ok(value: impl serde::Serialize) -> Result<serde_json::Value, RpcError> {
    serde_json::to_value(value)
        .map_err(|e| RpcError::new(error_codes::INTERNAL_ERROR, format!("serialize: {e}")))
}
pub(crate) fn parse_params<T: serde::de::DeserializeOwned>(req: &Request) -> Result<T, RpcError> {
    let params = req
        .params
        .clone()
        .ok_or_else(|| RpcError::new(error_codes::INVALID_PARAMS, "missing params"))?;
    serde_json::from_value(params)
        .map_err(|e| RpcError::new(error_codes::INVALID_PARAMS, format!("invalid params: {e}")))
}

/// Encode `body` as a frame and hand it to the writer thread. A send error
/// means the writer thread is gone (broken transport), surfaced as EOF-ish.
pub(crate) fn send_body(out: &mpsc::Sender<Vec<u8>>, body: &[u8]) -> io::Result<()> {
    out.send(body.to_vec())
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "writer thread ended"))
}

/// Serialize a value we already know is serializable (protocol envelopes),
/// falling back to an empty vec on the impossible error so callers on the
/// teardown path don't have to thread another `Result`.
pub(crate) fn to_vec_lossy(value: &impl serde::Serialize) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_default()
}

pub(crate) fn io_error(e: io::Error) -> RpcError {
    let code = if e.kind() == io::ErrorKind::NotFound {
        error_codes::NOT_FOUND
    } else {
        error_codes::IO_ERROR
    };
    RpcError::new(code, e.to_string())
}

pub(crate) fn git_error(e: fleet_git::StatusError) -> RpcError {
    let code = match e {
        // A non-zero exit almost always means "not a git repo"; surface it
        // as such so the client can fall back to showing no markers.
        fleet_git::StatusError::NonZeroExit { .. } => error_codes::NOT_A_REPO,
        fleet_git::StatusError::SpawnFailed(_) => error_codes::IO_ERROR,
        fleet_git::StatusError::InvalidOutput => error_codes::INTERNAL_ERROR,
    };
    RpcError::new(code, e.to_string())
}

pub(crate) fn to_wire(kind: fleet_git::StatusKind) -> WireStatus {
    use fleet_git::StatusKind as K;
    match kind {
        K::Modified => WireStatus::Modified,
        K::Added => WireStatus::Added,
        K::Deleted => WireStatus::Deleted,
        K::Renamed => WireStatus::Renamed,
        K::Untracked => WireStatus::Untracked,
        K::Ignored => WireStatus::Ignored,
        K::Conflicted => WireStatus::Conflicted,
    }
}
