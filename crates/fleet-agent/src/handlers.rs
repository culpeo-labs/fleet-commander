//! Request handlers for [`crate::Server`]: the `initialize`, `fs.*`, and
//! `git.*` methods, plus the workspace-root path resolver they share.

use std::path::{Component, Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use fleet_protocol::{
    Capabilities, FsEntry, FsListParams, FsListResult, FsReadParams, FsReadResult, FsStatParams,
    FsStatResult, GitBranchResult, GitDiffParams, GitDiffResult, GitStatusEntry, GitStatusParams,
    GitStatusResult, InitializeResult, PROTOCOL_VERSION, Request, RpcError, ServerInfo,
    error_codes,
};

use crate::Server;
use crate::util::{canonicalize_existing_prefix, git_error, io_error, ok, parse_params, to_wire};

impl Server {
    pub(super) fn initialize(&self) -> Result<serde_json::Value, RpcError> {
        ok(InitializeResult {
            protocol_version: PROTOCOL_VERSION,
            server_info: ServerInfo {
                name: "fleet-agent".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            capabilities: Capabilities {
                fs: true,
                git: true,
                watch: true,
                search: true,
                acp: true,
                // Flipped to `true` once the daemon owns the ACP session
                // (Phase 4b2 y1-agent-session).
                session: false,
            },
        })
    }

    pub(super) fn fs_list(&self, req: &Request) -> Result<serde_json::Value, RpcError> {
        let params: FsListParams = parse_params(req)?;
        let abs = self.resolve(&params.path)?;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&abs).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let is_dir = entry.file_type().map_err(io_error)?.is_dir();
            entries.push(FsEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir,
            });
        }
        ok(FsListResult { entries })
    }

    pub(super) fn fs_read(&self, req: &Request) -> Result<serde_json::Value, RpcError> {
        let params: FsReadParams = parse_params(req)?;
        let abs = self.resolve(&params.path)?;
        let total_size = std::fs::metadata(&abs).map_err(io_error)?.len();

        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(&abs).map_err(io_error)?;
        if params.offset > 0 {
            file.seek(SeekFrom::Start(params.offset))
                .map_err(io_error)?;
        }
        let mut buf = Vec::new();
        match params.len {
            Some(len) => {
                file.take(len).read_to_end(&mut buf).map_err(io_error)?;
            }
            None => {
                file.read_to_end(&mut buf).map_err(io_error)?;
            }
        }
        let eof = params.offset.saturating_add(buf.len() as u64) >= total_size;
        ok(FsReadResult {
            content_base64: BASE64.encode(&buf),
            eof,
            total_size,
        })
    }

    pub(super) fn fs_stat(&self, req: &Request) -> Result<serde_json::Value, RpcError> {
        let params: FsStatParams = parse_params(req)?;
        let abs = self.resolve(&params.path)?;
        let meta = std::fs::metadata(&abs).map_err(io_error)?;
        ok(FsStatResult {
            is_dir: meta.is_dir(),
            len: meta.len(),
        })
    }

    pub(super) fn git_status(&self, req: &Request) -> Result<serde_json::Value, RpcError> {
        let params: GitStatusParams = parse_params(req)?;
        let map = fleet_git::status(&self.root, params.include_ignored).map_err(git_error)?;
        let entries = map
            .into_iter()
            .map(|(path, kind)| GitStatusEntry {
                path: path.to_string_lossy().into_owned(),
                status: to_wire(kind),
            })
            .collect();
        ok(GitStatusResult { entries })
    }

    pub(super) fn git_branch(&self) -> Result<serde_json::Value, RpcError> {
        ok(GitBranchResult {
            branch: fleet_git::current_branch(&self.root),
        })
    }

    pub(super) fn git_diff(&self, req: &Request) -> Result<serde_json::Value, RpcError> {
        let params: GitDiffParams = parse_params(req)?;
        // Validate the path stays inside the workspace before handing it to
        // git (rejects `..`, absolute paths, and symlink escapes).
        self.resolve(&params.path)?;
        let diff = fleet_git::diff(&self.root, Path::new(&params.path), params.staged)
            .map_err(git_error)?;
        ok(GitDiffResult { diff })
    }

    /// Resolve a workspace-relative request path to an absolute path under
    /// `root`, rejecting anything that would escape it. Two layers of defence:
    ///
    /// 1. **Lexical:** reject absolute paths, `..`, and prefixes up front.
    /// 2. **Symlink-aware:** canonicalize the result (resolving any symlinks
    ///    along the way) and verify it still lives under the canonicalized
    ///    workspace root. This stops an *in-workspace* symlink — e.g.
    ///    `secrets -> /run/secrets` — from being followed out of the root.
    ///
    /// The server never trusts the client's path.
    fn resolve(&self, rel: &str) -> Result<PathBuf, RpcError> {
        let rel_path = Path::new(rel);
        let mut safe = PathBuf::new();
        for component in rel_path.components() {
            match component {
                Component::Normal(c) => safe.push(c),
                Component::CurDir => {}
                Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                    return Err(RpcError::new(
                        error_codes::FORBIDDEN_PATH,
                        format!("path escapes workspace root: {rel}"),
                    ));
                }
            }
        }
        let candidate = self.root.join(safe);
        // Resolve symlinks to a real path. The leaf may legitimately not exist
        // (e.g. reading a missing file), so fall back to the nearest existing
        // ancestor and re-append the remainder — this preserves NotFound
        // semantics while still catching escapes via a symlinked ancestor.
        let real = canonicalize_existing_prefix(&candidate).map_err(io_error)?;
        if !real.starts_with(&self.canonical_root) {
            return Err(RpcError::new(
                error_codes::FORBIDDEN_PATH,
                format!("path escapes workspace root: {rel}"),
            ));
        }
        Ok(candidate)
    }
}
