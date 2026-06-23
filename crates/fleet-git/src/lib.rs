//! Lightweight git inspection helpers.
//!
//! Used by the TUI to display the current branch of a workspace and
//! to mark files in the explorer with their git status. Branch
//! inspection is done by parsing `.git/HEAD` directly (no `git2`, no
//! subprocess). Status inspection shells out to `git status` since
//! reimplementing the gitignore + index walk in Rust is not worth the
//! complexity.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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

/// One-line summary of a path's git state. Combinations from the
/// index (X) and worktree (Y) columns of `git status --porcelain` are
/// collapsed down to whichever side has the more interesting story —
/// e.g. an indexed-then-modified file shows up as `Modified`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatusKind {
    /// Tracked file with worktree or index changes.
    Modified,
    /// Newly staged file that doesn't yet exist in HEAD.
    Added,
    /// Tracked file removed in the index or worktree.
    Deleted,
    /// Tracked file renamed; `path` is the new location.
    Renamed,
    /// File present on disk but not tracked.
    Untracked,
    /// File matched by `.gitignore`.
    Ignored,
    /// Unmerged path (active merge conflict).
    Conflicted,
}

impl StatusKind {
    /// Single-character marker used by the explorer column.
    pub fn marker(&self) -> &'static str {
        match self {
            StatusKind::Modified => "M",
            StatusKind::Added => "A",
            StatusKind::Deleted => "D",
            StatusKind::Renamed => "R",
            StatusKind::Untracked => "?",
            StatusKind::Ignored => "!",
            StatusKind::Conflicted => "U",
        }
    }
}

/// One entry from `status()`. `path` is relative to the workspace
/// root (matches what git emits) and uses `/` as the separator on
/// every platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStatus {
    pub path: PathBuf,
    pub kind: StatusKind,
}

/// What can go wrong fetching git status. Distinguishing these lets
/// the UI fall back gracefully (e.g. silently show no markers when
/// the workspace isn't a repo, surface a status-bar warning when git
/// isn't installed at all).
#[derive(Debug)]
pub enum StatusError {
    /// `git` binary not on PATH (or some other spawn-time failure).
    SpawnFailed(std::io::Error),
    /// `git` returned a non-zero exit code. Almost always means
    /// `workspace` is not inside a repo, but we surface stderr for
    /// debugging.
    NonZeroExit { code: Option<i32>, stderr: String },
    /// The stdout stream wasn't valid UTF-8 (very unusual: git emits
    /// paths byte-for-byte and our `-z` parser is byte-aware, so this
    /// only fires if status text contains invalid sequences).
    InvalidOutput,
}

impl std::fmt::Display for StatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StatusError::SpawnFailed(e) => write!(f, "could not run git: {e}"),
            StatusError::NonZeroExit { code, stderr } => {
                let c = code.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
                write!(f, "git exited with code {c}: {}", stderr.trim())
            }
            StatusError::InvalidOutput => write!(f, "git status produced invalid output"),
        }
    }
}

impl std::error::Error for StatusError {}

/// Return a map of `path -> StatusKind` for every non-clean file
/// inside `workspace`. Tracked files with no changes are absent from
/// the map (the explorer treats absence as "clean").
///
/// `include_ignored` controls whether `--ignored=traditional` and
/// `--untracked-files=all` are added to the git invocation. Both are
/// **off** by default because on a Rust/Node repo with `target/` or
/// `node_modules/` they balloon the output to hundreds of thousands
/// of entries and add multiple seconds of latency for a feature
/// (showing ignored files in the tree) that's behind a toggle.
pub fn status(
    workspace: &Path,
    include_ignored: bool,
) -> Result<HashMap<PathBuf, StatusKind>, StatusError> {
    let entries = status_entries(workspace, include_ignored)?;
    let mut map = HashMap::with_capacity(entries.len());
    for entry in entries {
        map.insert(entry.path, entry.kind);
    }
    Ok(map)
}

/// Lower-level variant returning the parsed entries in the order git
/// reported them. Useful when the consumer wants the raw list (e.g.
/// for a "changes" summary) rather than a lookup table.
pub fn status_entries(
    workspace: &Path,
    include_ignored: bool,
) -> Result<Vec<FileStatus>, StatusError> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(workspace)
        .args(["status", "--porcelain=v1", "-z"]);
    if include_ignored {
        // Listing ignored entries is only useful when the user has
        // asked to see them; otherwise it's pure overhead.
        cmd.args(["--ignored=traditional", "--untracked-files=all"]);
    }
    let output = cmd.output().map_err(StatusError::SpawnFailed)?;

    if !output.status.success() {
        return Err(StatusError::NonZeroExit {
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }

    parse_porcelain_z(&output.stdout).ok_or(StatusError::InvalidOutput)
}

/// Parse the NUL-separated output of `git status --porcelain=v1 -z`.
///
/// Each entry is `XY <path>\0`, except rename/copy entries which are
/// `XY <new_path>\0<old_path>\0` (the old path comes second under
/// `-z`). We keep the new path and drop the old.
fn parse_porcelain_z(bytes: &[u8]) -> Option<Vec<FileStatus>> {
    let mut out = Vec::new();
    let mut iter = bytes.split(|&b| b == 0);
    while let Some(record) = iter.next() {
        if record.is_empty() {
            continue;
        }
        // Smallest legal record is `XY <path>` → 4 bytes.
        if record.len() < 4 || record[2] != b' ' {
            // Malformed; skip rather than fail the whole call.
            continue;
        }
        let x = record[0];
        let y = record[1];
        let path_bytes = &record[3..];
        let path = std::str::from_utf8(path_bytes).ok()?;
        let kind = classify(x, y);
        // For renames/copies, git emits the old path as the next
        // record under `-z`. Consume and discard it.
        if matches!(x, b'R' | b'C') || matches!(y, b'R' | b'C') {
            let _old = iter.next();
        }
        out.push(FileStatus {
            path: PathBuf::from(path),
            kind,
        });
    }
    Some(out)
}

/// Collapse the two-letter porcelain code into a single [`StatusKind`].
/// Priority is: conflicted > ignored > untracked > rename/add/delete >
/// modified, since "more interesting" should win when shown in a
/// single column.
fn classify(x: u8, y: u8) -> StatusKind {
    // Unmerged combinations per git's porcelain spec.
    let unmerged = matches!(
        (x, y),
        (b'D', b'D')
            | (b'A', b'U')
            | (b'U', b'D')
            | (b'U', b'A')
            | (b'D', b'U')
            | (b'A', b'A')
            | (b'U', b'U')
            | (b'U', b'R')
            | (b'R', b'U')
    );
    if unmerged {
        return StatusKind::Conflicted;
    }
    if x == b'!' || y == b'!' {
        return StatusKind::Ignored;
    }
    if x == b'?' || y == b'?' {
        return StatusKind::Untracked;
    }
    if x == b'R' || y == b'R' || x == b'C' || y == b'C' {
        return StatusKind::Renamed;
    }
    if x == b'A' {
        return StatusKind::Added;
    }
    if x == b'D' || y == b'D' {
        return StatusKind::Deleted;
    }
    StatusKind::Modified
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

    // ─── Porcelain parser ──────────────────────────────────────────────

    #[test]
    fn parses_simple_modified_and_untracked_entries() {
        // " M src/lib.rs\0?? new.txt\0"
        let bytes = b" M src/lib.rs\0?? new.txt\0";
        let entries = parse_porcelain_z(bytes).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, PathBuf::from("src/lib.rs"));
        assert_eq!(entries[0].kind, StatusKind::Modified);
        assert_eq!(entries[1].path, PathBuf::from("new.txt"));
        assert_eq!(entries[1].kind, StatusKind::Untracked);
    }

    #[test]
    fn parses_added_deleted_ignored_and_conflict() {
        let bytes = b"A  added.rs\0 D removed.rs\0!! target/foo\0UU merge.txt\0";
        let entries = parse_porcelain_z(bytes).unwrap();
        let kinds: Vec<_> = entries.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                StatusKind::Added,
                StatusKind::Deleted,
                StatusKind::Ignored,
                StatusKind::Conflicted,
            ]
        );
    }

    #[test]
    fn rename_record_drops_old_path() {
        // Under -z a rename emits new path then a follow-up record
        // with the old path; the old path must be consumed but not
        // surfaced as a separate entry.
        let bytes = b"R  new.rs\0old.rs\0 M plain.rs\0";
        let entries = parse_porcelain_z(bytes).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, PathBuf::from("new.rs"));
        assert_eq!(entries[0].kind, StatusKind::Renamed);
        assert_eq!(entries[1].path, PathBuf::from("plain.rs"));
        assert_eq!(entries[1].kind, StatusKind::Modified);
    }

    #[test]
    fn parser_skips_malformed_records_rather_than_failing() {
        // Second record is missing the space separator.
        let bytes = b" M ok.rs\0XX\0?? also-ok\0";
        let entries = parse_porcelain_z(bytes).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, PathBuf::from("ok.rs"));
        assert_eq!(entries[1].path, PathBuf::from("also-ok"));
    }

    #[test]
    fn classify_picks_index_side_when_more_informative() {
        // Added in index, then modified — should report as Added (the
        // "newer in HEAD" story wins).
        assert_eq!(classify(b'A', b'M'), StatusKind::Added);
        // Modified in both — just Modified.
        assert_eq!(classify(b'M', b'M'), StatusKind::Modified);
    }

    #[test]
    fn status_kind_marker_letters_match_porcelain() {
        assert_eq!(StatusKind::Modified.marker(), "M");
        assert_eq!(StatusKind::Added.marker(), "A");
        assert_eq!(StatusKind::Untracked.marker(), "?");
        assert_eq!(StatusKind::Ignored.marker(), "!");
    }

    // ─── Real-git integration ──────────────────────────────────────────

    /// Run a git subcommand inside `cwd`, panicking on any failure
    /// so the test fails with a useful message rather than a silent
    /// state mismatch.
    fn run_git(cwd: &Path, args: &[&str]) {
        let out = Command::new("git").arg("-C").arg(cwd).args(args).output();
        let out = match out {
            Ok(o) => o,
            Err(e) => panic!("could not exec git: {e}"),
        };
        if !out.status.success() {
            panic!(
                "git {args:?} failed: stdout={} stderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
        }
    }

    fn init_repo() -> tempfile::TempDir {
        let tmp = tempdir().unwrap();
        run_git(tmp.path(), &["init", "-q", "-b", "main"]);
        run_git(tmp.path(), &["config", "user.email", "t@example.com"]);
        run_git(tmp.path(), &["config", "user.name", "t"]);
        run_git(tmp.path(), &["config", "commit.gpgsign", "false"]);
        tmp
    }

    #[test]
    fn status_marks_untracked_modified_and_ignored() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("skipping: git not installed");
            return;
        }
        let tmp = init_repo();
        write(&tmp.path().join(".gitignore"), "ignored.txt\n");
        write(&tmp.path().join("tracked.rs"), "fn a() {}\n");
        run_git(tmp.path(), &["add", ".gitignore", "tracked.rs"]);
        run_git(tmp.path(), &["commit", "-q", "-m", "init"]);

        // Mutate the tracked file, add a new untracked file, and
        // create an ignored file.
        write(&tmp.path().join("tracked.rs"), "fn b() {}\n");
        write(&tmp.path().join("untracked.rs"), "fn c() {}\n");
        write(&tmp.path().join("ignored.txt"), "noise");

        let map = status(tmp.path(), true).expect("status");
        assert_eq!(
            map.get(&PathBuf::from("tracked.rs")),
            Some(&StatusKind::Modified),
            "map: {map:?}"
        );
        assert_eq!(
            map.get(&PathBuf::from("untracked.rs")),
            Some(&StatusKind::Untracked),
        );
        assert_eq!(
            map.get(&PathBuf::from("ignored.txt")),
            Some(&StatusKind::Ignored),
        );
    }

    #[test]
    fn status_errors_outside_a_repo() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("skipping: git not installed");
            return;
        }
        let tmp = tempdir().unwrap();
        let err = status(tmp.path(), false).expect_err("expected NonZeroExit");
        assert!(matches!(err, StatusError::NonZeroExit { .. }), "{err}");
    }
}
