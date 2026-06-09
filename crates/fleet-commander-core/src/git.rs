//! Lightweight git inspection helpers.
//!
//! Used by the TUI to display the current branch of a workspace
//! without pulling in `git2` (which would add a C dependency) or
//! shelling out to `git` on every render.
//!
//! Only enough of the git layout is understood to answer "what's the
//! current branch?" — that means parsing `.git/HEAD` directly, and
//! following the `gitdir:` pointer when `.git` is a file (worktrees,
//! submodules).

use std::fs;
use std::path::{Path, PathBuf};

/// Return the current branch of the repository at `workspace`, or
/// `None` if the path is not a git working tree.
///
/// For a normal branch checkout this returns the short branch name
/// (e.g. `"main"`). For a detached HEAD this returns `"(<short sha>)"`.
pub fn current_branch(workspace: &Path) -> Option<String> {
    let head_path = resolve_head_path(workspace)?;
    let raw = fs::read_to_string(&head_path).ok()?;
    let head = raw.trim();

    if let Some(rest) = head.strip_prefix("ref:") {
        let r = rest.trim();
        // refs/heads/<branch>
        if let Some(branch) = r.strip_prefix("refs/heads/") {
            return Some(branch.to_string());
        }
        // Some other ref (tag, remote tracking, etc.); show the last component.
        return r.rsplit('/').next().map(|s| s.to_string());
    }

    // Detached HEAD: HEAD contains a SHA.
    if head.len() >= 7 && head.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(format!("({})", &head[..7]));
    }

    None
}

/// Resolve the path of the `HEAD` file for the given workspace,
/// transparently following `.git` files that point at a real gitdir
/// (worktrees / submodules).
fn resolve_head_path(workspace: &Path) -> Option<PathBuf> {
    let git = workspace.join(".git");
    let metadata = fs::metadata(&git).ok()?;
    if metadata.is_dir() {
        return Some(git.join("HEAD"));
    }
    if metadata.is_file() {
        // Worktree / submodule: file contains `gitdir: <path>`.
        let contents = fs::read_to_string(&git).ok()?;
        let path = contents.trim().strip_prefix("gitdir:")?.trim();
        let resolved = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            workspace.join(path)
        };
        return Some(resolved.join("HEAD"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn returns_none_outside_a_repo() {
        let tmp = tempdir().unwrap();
        assert_eq!(current_branch(tmp.path()), None);
    }

    #[test]
    fn parses_branch_from_head_ref() {
        let tmp = tempdir().unwrap();
        write(&tmp.path().join(".git/HEAD"), "ref: refs/heads/main\n");
        assert_eq!(current_branch(tmp.path()), Some("main".into()));
    }

    #[test]
    fn parses_branch_with_slashes() {
        let tmp = tempdir().unwrap();
        write(
            &tmp.path().join(".git/HEAD"),
            "ref: refs/heads/feature/long-name\n",
        );
        assert_eq!(current_branch(tmp.path()), Some("feature/long-name".into()));
    }

    #[test]
    fn detached_head_shows_short_sha() {
        let tmp = tempdir().unwrap();
        write(
            &tmp.path().join(".git/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        );
        assert_eq!(current_branch(tmp.path()), Some("(0123456)".into()));
    }

    #[test]
    fn follows_worktree_gitdir_file() {
        let tmp = tempdir().unwrap();
        let real_gitdir = tmp.path().join("real-gitdir");
        write(&real_gitdir.join("HEAD"), "ref: refs/heads/work\n");
        write(
            &tmp.path().join("worktree/.git"),
            &format!("gitdir: {}\n", real_gitdir.display()),
        );
        assert_eq!(
            current_branch(&tmp.path().join("worktree")),
            Some("work".into())
        );
    }
}
