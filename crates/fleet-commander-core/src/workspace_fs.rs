//! Abstraction over the filesystem + git tooling that backs an
//! agent's workspace.
//!
//! Today there's a single implementation, [`LocalFs`], which walks
//! the host filesystem and shells out to the host's `git`. This is
//! enough whenever the dev container is a bind-mount of a host path
//! (the common case).
//!
//! The trait exists so that future implementations — e.g. a
//! `ContainerFs` that runs `git status` / `ls` inside a dev container
//! via `docker exec`, or a `RemoteFs` that proxies over SSH — can be
//! dropped in without rewriting the explorer or the session header.
//! Both of those scenarios are out of scope for this PR; the trait
//! shape was chosen with them in mind nonetheless.

use std::collections::HashMap;
use std::fmt::Debug;
use std::io;
use std::path::{Path, PathBuf};

use crate::git::{self, StatusError, StatusKind};

/// A single entry returned by [`WorkspaceFs::list_dir`]. Intentionally
/// minimal — the explorer only needs the name and a directory flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

/// Read-only view over a workspace, abstracting where the bytes
/// actually live. Implementations must be cheap to clone via `Arc`
/// (they're stored in the App-level explorer state and consulted
/// every render) and must be safe to call from any thread.
pub trait WorkspaceFs: Send + Sync + Debug {
    /// Absolute path of the workspace root. Used only for display
    /// (e.g. the empty-state line). Path semantics are otherwise
    /// fully encapsulated.
    fn root_display(&self) -> &Path;

    /// Direct children of `rel` (a path **relative to the workspace
    /// root**, `""` for the root itself). Order is not guaranteed —
    /// the caller (explorer) sorts.
    fn list_dir(&self, rel: &Path) -> io::Result<Vec<DirEntry>>;

    /// Read a file as bytes. Used for the side-pane preview.
    fn read_file(&self, rel: &Path) -> io::Result<Vec<u8>>;

    /// Current branch name (or `None` outside a repo).
    fn git_branch(&self) -> Option<String>;

    /// Map of `rel -> status` for every non-clean path. Tracked-and-
    /// clean files are absent. Ignored files **are** included; the
    /// UI hides them by default and surfaces them on toggle.
    fn git_status(&self) -> Result<HashMap<PathBuf, StatusKind>, StatusError>;
}

/// Backed by the local filesystem and the host's `git` binary.
#[derive(Debug, Clone)]
pub struct LocalFs {
    root: PathBuf,
}

impl LocalFs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl WorkspaceFs for LocalFs {
    fn root_display(&self) -> &Path {
        &self.root
    }

    fn list_dir(&self, rel: &Path) -> io::Result<Vec<DirEntry>> {
        let abs = if rel.as_os_str().is_empty() {
            self.root.clone()
        } else {
            self.root.join(rel)
        };
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&abs)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            out.push(DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir: file_type.is_dir(),
            });
        }
        Ok(out)
    }

    fn read_file(&self, rel: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(self.root.join(rel))
    }

    fn git_branch(&self) -> Option<String> {
        git::current_branch(&self.root)
    }

    fn git_status(&self) -> Result<HashMap<PathBuf, StatusKind>, StatusError> {
        git::status(&self.root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn fixture() -> tempfile::TempDir {
        let tmp = tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("src/nested")).unwrap();
        fs::write(tmp.path().join("README.md"), "hi").unwrap();
        fs::write(tmp.path().join("src/lib.rs"), "fn a(){}").unwrap();
        fs::write(tmp.path().join("src/nested/deep.rs"), "fn b(){}").unwrap();
        tmp
    }

    #[test]
    fn list_dir_returns_immediate_children_for_root() {
        let tmp = fixture();
        let fs = LocalFs::new(tmp.path());
        let mut entries = fs.list_dir(Path::new("")).unwrap();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "README.md");
        assert!(!entries[0].is_dir);
        assert_eq!(entries[1].name, "src");
        assert!(entries[1].is_dir);
    }

    #[test]
    fn list_dir_for_nested_path_returns_its_children() {
        let tmp = fixture();
        let fs = LocalFs::new(tmp.path());
        let entries = fs.list_dir(Path::new("src/nested")).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "deep.rs");
    }

    #[test]
    fn list_dir_propagates_io_errors() {
        let tmp = fixture();
        let fs = LocalFs::new(tmp.path());
        assert!(fs.list_dir(Path::new("does-not-exist")).is_err());
    }

    #[test]
    fn read_file_returns_bytes() {
        let tmp = fixture();
        let fs = LocalFs::new(tmp.path());
        assert_eq!(fs.read_file(Path::new("README.md")).unwrap(), b"hi");
    }

    #[test]
    fn git_branch_returns_none_outside_a_repo() {
        let tmp = fixture();
        let fs = LocalFs::new(tmp.path());
        assert_eq!(fs.git_branch(), None);
    }
}
