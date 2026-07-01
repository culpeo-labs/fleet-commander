//! State + lazy tree-walk for the file-explorer side pane.
//!
//! Filesystem and git access go through a
//! [`fleet_commander_core::workspace_fs::WorkspaceFs`] handle so that
//! the explorer is agnostic to where the workspace actually lives —
//! the only impl today is `LocalFs`, but a future `ContainerFs` /
//! `RemoteFs` should be a drop-in replacement.
//!
//! All path keys in `expanded`, `selected`, and `status` are
//! **relative to the workspace root**, with `/` as the separator
//! (matching what `git status` emits).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fleet_commander_core::git::StatusKind;
use fleet_commander_core::workspace_fs::{DirEntry, WorkspaceFs};

/// One visible entry in the rendered tree, after applying the
/// expansion state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryRow {
    /// Path relative to the workspace root.
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub expanded: bool,
    pub status: Option<StatusKind>,
}

#[derive(Default)]
pub struct ExplorerState {
    /// Pane visible at all? Toggled by Ctrl+E.
    pub open: bool,
    /// Filesystem handle for the active workspace. `None` when the
    /// active agent has no workspace folder set.
    pub fs: Option<Arc<dyn WorkspaceFs>>,
    /// Currently selected entry path (relative to the root). `None`
    /// means "the cursor is implicitly on the first visible row".
    pub selected: Option<PathBuf>,
    /// Expanded directories, by path relative to the root. The root
    /// itself is implicitly always expanded and is absent from this
    /// set.
    pub expanded: HashSet<PathBuf>,
    /// Map of relative path → git status. Tracked-and-clean files
    /// are absent from the map.
    pub status: HashMap<PathBuf, StatusKind>,
    /// Pre-computed aggregate status for every ancestor directory of
    /// a non-clean entry — the most "interesting" status among its
    /// descendants. Built once per `refresh_status` so that rendering
    /// each directory row is O(1) instead of O(status_count).
    dir_status: HashMap<PathBuf, StatusKind>,
    /// Whether ignored files are visible. Off by default.
    pub show_ignored: bool,
    /// True while a background `git status` is in flight. The app
    /// uses this to coalesce bursty refresh requests (e.g. many
    /// diffs landing in a row) into a single follow-up refresh.
    pub is_refreshing: bool,
    /// True when a refresh was requested while one was already in
    /// flight. We honour it by re-issuing the refresh as soon as the
    /// in-flight one completes.
    pub refresh_pending: bool,
    /// Last error from a status fetch, surfaced once via the status
    /// bar.
    pub last_error: Option<String>,
    /// Cached directory listings (`rel -> children`) for **remote**
    /// filesystems, where `list_dir` is a blocking RPC that must never
    /// run on the render path. Populated by background loads delivered
    /// as `AppEvent::ExplorerDirReady`. Unused for `LocalFs`, which is
    /// cheap enough to list synchronously during the walk.
    dir_cache: HashMap<PathBuf, Vec<DirEntry>>,
    /// Directories with a background listing in flight, so the app
    /// doesn't enqueue duplicate fetches for the same path.
    dir_loading: HashSet<PathBuf>,
    /// A file the user activated (Enter) that should be opened in the
    /// side pane. Read (and cleared) by the app, which performs the
    /// actual — possibly remote, hence backgrounded — read.
    pub pending_open: Option<PathBuf>,
    /// Set alongside [`Self::pending_open`] when the open originated from a
    /// search hit: the 1-based line the preview should scroll to. `None`
    /// for a plain explorer open (top of file).
    pub pending_open_line: Option<u64>,
}

impl std::fmt::Debug for ExplorerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExplorerState")
            .field("open", &self.open)
            .field("fs", &self.fs.as_ref().map(|f| f.root_display().to_owned()))
            .field("selected", &self.selected)
            .field("expanded", &self.expanded)
            .field("status_count", &self.status.len())
            .field("show_ignored", &self.show_ignored)
            .field("last_error", &self.last_error)
            .finish()
    }
}

impl ExplorerState {
    /// Reconfigure the explorer for a new workspace. Resets state
    /// when the root changes; preserves expansion/selection if the
    /// caller passes the same handle (or an `Arc::ptr_eq`-equal one).
    pub fn set_fs(&mut self, fs: Option<Arc<dyn WorkspaceFs>>) {
        let same = match (&self.fs, &fs) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b) || a.root_display() == b.root_display(),
            (None, None) => true,
            _ => false,
        };
        if same {
            self.fs = fs;
            return;
        }
        self.fs = fs;
        self.selected = None;
        self.expanded.clear();
        self.status.clear();
        self.dir_status.clear();
        self.dir_cache.clear();
        self.dir_loading.clear();
        self.pending_open = None;
        self.pending_open_line = None;
        self.last_error = None;
    }

    /// Drop cached directory listings so a remote tree is re-fetched
    /// from scratch (e.g. on a manual refresh, when files may have been
    /// created/deleted inside the container). Expansion/selection state
    /// is preserved.
    pub fn invalidate_dirs(&mut self) {
        self.dir_cache.clear();
        self.dir_loading.clear();
    }

    /// Directories that the current view needs but that aren't cached
    /// yet (and aren't already being fetched). Only meaningful for a
    /// remote filesystem; empty otherwise. The app turns each into a
    /// background `list_dir` and feeds the result back via
    /// [`Self::apply_dir`].
    pub fn missing_dirs(&self) -> Vec<PathBuf> {
        let Some(fs) = &self.fs else {
            return Vec::new();
        };
        if !fs.is_remote() {
            return Vec::new();
        }
        let mut missing = Vec::new();
        collect_missing_dirs(self, Path::new(""), &mut missing);
        missing
            .into_iter()
            .filter(|p| !self.dir_loading.contains(p))
            .collect()
    }

    /// Mark a directory as having a background listing in flight.
    pub fn mark_dir_loading(&mut self, rel: PathBuf) {
        self.dir_loading.insert(rel);
    }

    /// Install a freshly-fetched directory listing into the cache (or,
    /// on error, just clear the in-flight marker so it can be retried).
    pub fn apply_dir(&mut self, rel: PathBuf, result: Result<Vec<DirEntry>, String>) {
        self.dir_loading.remove(&rel);
        if let Ok(entries) = result {
            self.dir_cache.insert(rel, entries);
        }
    }

    /// Re-read git status via the workspace FS *synchronously*. Sets
    /// `last_error` on failure (e.g. workspace isn't a repo) rather
    /// than panicking.
    ///
    /// Install a freshly-fetched status map and rebuild the
    /// directory-aggregate index. Called by the app when an
    /// `AppEvent::ExplorerStatusReady` lands (and by tests
    /// that want to seed status without running git).
    pub fn apply_status(&mut self, result: Result<HashMap<PathBuf, StatusKind>, String>) {
        self.is_refreshing = false;
        match result {
            Ok(map) => {
                self.dir_status = build_dir_aggregates(&map);
                self.status = map;
                self.last_error = None;
            }
            Err(message) => {
                self.status.clear();
                self.dir_status.clear();
                self.last_error = Some(message);
            }
        }
    }

    /// Flat list of visible rows in display order.
    pub fn visible_entries(&self) -> Vec<EntryRow> {
        let Some(fs) = &self.fs else {
            return Vec::new();
        };
        let mut out = Vec::new();
        walk_dir(fs.as_ref(), Path::new(""), 0, self, &mut out);
        out
    }

    /// Position of the currently selected entry in
    /// [`Self::visible_entries`], or `0` if nothing is selected /
    /// the selected path is no longer visible (e.g. its parent has
    /// been collapsed).
    pub fn selected_index(&self, entries: &[EntryRow]) -> usize {
        match &self.selected {
            Some(p) => entries.iter().position(|e| &e.path == p).unwrap_or(0),
            None => 0,
        }
    }

    /// Move the cursor by `delta` rows (clamped to `[0, len-1]`).
    pub fn move_cursor(&mut self, delta: i64) {
        let entries = self.visible_entries();
        if entries.is_empty() {
            self.selected = None;
            return;
        }
        let current = self.selected_index(&entries) as i64;
        let next = current
            .saturating_add(delta)
            .clamp(0, entries.len() as i64 - 1) as usize;
        self.selected = Some(entries[next].path.clone());
    }

    /// Expand or collapse the selected directory. No-op when the
    /// selection points at a file.
    pub fn toggle_selected_dir(&mut self) {
        let entries = self.visible_entries();
        let Some(selected) = &self.selected else {
            return;
        };
        let Some(entry) = entries.iter().find(|e| &e.path == selected) else {
            return;
        };
        if !entry.is_dir {
            return;
        }
        if !self.expanded.remove(&entry.path) {
            self.expanded.insert(entry.path.clone());
        }
    }

    /// Currently-selected entry, if any.
    pub fn selected_entry(&self) -> Option<EntryRow> {
        let entries = self.visible_entries();
        let path = self.selected.as_ref()?;
        entries.into_iter().find(|e| &e.path == path)
    }
}

/// Sorted, filtered children of `rel` for the walk, or `None` when the
/// listing isn't available yet.
///
/// For a remote filesystem the listing comes from [`ExplorerState::dir_cache`]
/// (a miss returns `None`, signalling the caller to schedule a background
/// fetch rather than block the render path). For a local filesystem it's a
/// cheap synchronous `list_dir`.
fn children_of(
    state: &ExplorerState,
    fs: &dyn WorkspaceFs,
    rel: &Path,
) -> Option<Vec<(PathBuf, String, bool)>> {
    let read: Vec<DirEntry> = if fs.is_remote() {
        state.dir_cache.get(rel)?.clone()
    } else {
        fs.list_dir(rel).ok()?
    };

    let mut children: Vec<(PathBuf, String, bool)> = Vec::new();
    for entry in read {
        // Always hide the .git directory itself — it's noise, not
        // content the user is here to navigate.
        if entry.name == ".git" {
            continue;
        }
        let child_rel = if rel.as_os_str().is_empty() {
            PathBuf::from(&entry.name)
        } else {
            rel.join(&entry.name)
        };
        if !state.show_ignored && state.status.get(&child_rel) == Some(&StatusKind::Ignored) {
            continue;
        }
        children.push((child_rel, entry.name, entry.is_dir));
    }

    children.sort_by(|a, b| match (a.2, b.2) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.1.to_lowercase().cmp(&b.1.to_lowercase()),
    });
    Some(children)
}

/// Recursive directory walker honouring `expanded`. Directories
/// first (alphabetical, case-insensitive) then files. Children of a
/// non-expanded directory are not listed. A remote directory that
/// isn't cached yet contributes no rows (its fetch is scheduled
/// separately via [`ExplorerState::missing_dirs`]).
fn walk_dir(
    fs: &dyn WorkspaceFs,
    rel: &Path,
    depth: usize,
    state: &ExplorerState,
    out: &mut Vec<EntryRow>,
) {
    let Some(children) = children_of(state, fs, rel) else {
        return;
    };

    for (child_rel, name, is_dir) in children {
        let expanded = is_dir && state.expanded.contains(&child_rel);
        let status = derive_status(state, &child_rel, is_dir);
        out.push(EntryRow {
            path: child_rel.clone(),
            name,
            depth,
            is_dir,
            expanded,
            status,
        });
        if expanded {
            walk_dir(fs, &child_rel, depth + 1, state, out);
        }
    }
}

/// Collect remote directories the current view needs but that aren't
/// cached. Mirrors [`walk_dir`]'s traversal: an uncached directory is
/// recorded (and its subtree pruned, since we can't know its children
/// until it loads); a cached one recurses into its expanded subdirs.
fn collect_missing_dirs(state: &ExplorerState, rel: &Path, missing: &mut Vec<PathBuf>) {
    let Some(entries) = state.dir_cache.get(rel) else {
        missing.push(rel.to_path_buf());
        return;
    };
    for entry in entries {
        if !entry.is_dir {
            continue;
        }
        let child_rel = if rel.as_os_str().is_empty() {
            PathBuf::from(&entry.name)
        } else {
            rel.join(&entry.name)
        };
        if state.expanded.contains(&child_rel) {
            collect_missing_dirs(state, &child_rel, missing);
        }
    }
}

/// Status to display for a row. Files just use their entry from the
/// status map. Directory rows look up the pre-computed aggregate;
/// see [`build_dir_aggregates`].
fn derive_status(state: &ExplorerState, rel: &Path, is_dir: bool) -> Option<StatusKind> {
    if !is_dir {
        return state.status.get(rel).copied();
    }
    state.dir_status.get(rel).copied()
}

/// Build the `dir_path -> aggregated StatusKind` map in one O(n × depth)
/// pass over the status map, so the per-render directory lookup is
/// O(1) instead of O(status_count).
fn build_dir_aggregates(status: &HashMap<PathBuf, StatusKind>) -> HashMap<PathBuf, StatusKind> {
    let mut out: HashMap<PathBuf, StatusKind> = HashMap::new();
    for (path, kind) in status {
        let mut cursor: &Path = path;
        while let Some(parent) = cursor.parent() {
            if parent.as_os_str().is_empty() {
                break;
            }
            let entry = out.entry(parent.to_path_buf()).or_insert(*kind);
            if priority(*kind) > priority(*entry) {
                *entry = *kind;
            }
            cursor = parent;
        }
    }
    out
}

fn priority(kind: StatusKind) -> u8 {
    match kind {
        StatusKind::Conflicted => 7,
        StatusKind::Deleted => 6,
        StatusKind::Modified => 5,
        StatusKind::Added => 4,
        StatusKind::Renamed => 3,
        StatusKind::Untracked => 2,
        StatusKind::Ignored => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_commander_core::workspace_fs::LocalFs;
    use std::fs;
    use tempfile::TempDir;

    fn touch(root: &Path, rel: &str) {
        let abs = root.join(rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(abs, b"x").unwrap();
    }

    fn mkdir(root: &Path, rel: &str) {
        fs::create_dir_all(root.join(rel)).unwrap();
    }

    fn fixture() -> TempDir {
        // .
        // ├── README.md
        // ├── src/
        // │   ├── lib.rs
        // │   └── nested/
        // │       └── deep.rs
        // └── target/
        //     └── build.log
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "README.md");
        touch(tmp.path(), "src/lib.rs");
        touch(tmp.path(), "src/nested/deep.rs");
        mkdir(tmp.path(), "target");
        touch(tmp.path(), "target/build.log");
        tmp
    }

    fn state_for(tmp: &TempDir) -> ExplorerState {
        let mut state = ExplorerState::default();
        state.set_fs(Some(Arc::new(LocalFs::new(tmp.path()))));
        state
    }

    #[test]
    fn empty_state_yields_no_entries() {
        let state = ExplorerState::default();
        assert!(state.visible_entries().is_empty());
    }

    #[test]
    fn collapsed_root_lists_top_level_only() {
        let tmp = fixture();
        let state = state_for(&tmp);
        let entries = state.visible_entries();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        // Directories first (src, target), files second (README.md).
        assert_eq!(names, vec!["src", "target", "README.md"]);
        assert!(!entries.iter().any(|e| e.name == "lib.rs"));
    }

    #[test]
    fn expanding_a_dir_lists_children_at_correct_depth() {
        let tmp = fixture();
        let mut state = state_for(&tmp);
        state.expanded.insert(PathBuf::from("src"));
        let entries = state.visible_entries();
        let src = entries.iter().find(|e| e.name == "src").unwrap();
        let lib = entries.iter().find(|e| e.name == "lib.rs").unwrap();
        assert_eq!(src.depth, 0);
        assert_eq!(lib.depth, 1);
        assert!(src.expanded);
    }

    #[test]
    fn ignored_entries_hidden_by_default_visible_with_toggle() {
        let tmp = fixture();
        let mut state = state_for(&tmp);
        state.expanded.insert(PathBuf::from("target"));
        state
            .status
            .insert(PathBuf::from("target/build.log"), StatusKind::Ignored);

        let hidden = state.visible_entries();
        assert!(
            !hidden.iter().any(|e| e.name == "build.log"),
            "ignored file should be hidden by default"
        );

        state.show_ignored = true;
        let shown = state.visible_entries();
        assert!(
            shown.iter().any(|e| e.name == "build.log"),
            "ignored file should appear with toggle on"
        );
    }

    #[test]
    fn directories_aggregate_status_from_descendants() {
        let tmp = fixture();
        let mut state = state_for(&tmp);
        let mut map = HashMap::new();
        map.insert(PathBuf::from("src/nested/deep.rs"), StatusKind::Modified);
        state.apply_status(Ok(map));
        let entries = state.visible_entries();
        let src = entries.iter().find(|e| e.name == "src").unwrap();
        assert_eq!(src.status, Some(StatusKind::Modified));
    }

    #[test]
    fn priority_picks_more_interesting_status_for_dir_aggregation() {
        let tmp = fixture();
        let mut state = state_for(&tmp);
        let mut map = HashMap::new();
        map.insert(PathBuf::from("src/lib.rs"), StatusKind::Untracked);
        map.insert(PathBuf::from("src/nested/deep.rs"), StatusKind::Modified);
        state.apply_status(Ok(map));
        let entries = state.visible_entries();
        let src = entries.iter().find(|e| e.name == "src").unwrap();
        // Modified outranks Untracked.
        assert_eq!(src.status, Some(StatusKind::Modified));
    }

    #[test]
    fn dot_git_directory_is_always_hidden() {
        let tmp = fixture();
        mkdir(tmp.path(), ".git");
        touch(tmp.path(), ".git/HEAD");
        let state = state_for(&tmp);
        assert!(!state.visible_entries().iter().any(|e| e.name == ".git"));
    }

    #[test]
    fn move_cursor_clamps_at_boundaries() {
        let tmp = fixture();
        let mut state = state_for(&tmp);
        let entries = state.visible_entries();
        state.move_cursor(1);
        assert_eq!(state.selected, Some(entries[1].path.clone()));
        state.move_cursor(99);
        assert_eq!(state.selected, Some(entries.last().unwrap().path.clone()));
        state.move_cursor(-99);
        assert_eq!(state.selected, Some(entries[0].path.clone()));
    }

    #[test]
    fn toggle_selected_dir_expands_then_collapses() {
        let tmp = fixture();
        let mut state = state_for(&tmp);
        state.selected = Some(PathBuf::from("src"));

        state.toggle_selected_dir();
        assert!(state.expanded.contains(&PathBuf::from("src")));

        state.toggle_selected_dir();
        assert!(!state.expanded.contains(&PathBuf::from("src")));
    }

    #[test]
    fn toggle_selected_dir_is_noop_on_file() {
        let tmp = fixture();
        let mut state = state_for(&tmp);
        state.selected = Some(PathBuf::from("README.md"));
        state.toggle_selected_dir();
        assert!(state.expanded.is_empty());
    }

    #[test]
    fn set_fs_to_same_root_preserves_state() {
        let tmp = fixture();
        let mut state = state_for(&tmp);
        state.expanded.insert(PathBuf::from("src"));
        state.set_fs(Some(Arc::new(LocalFs::new(tmp.path()))));
        assert!(state.expanded.contains(&PathBuf::from("src")));
    }

    #[test]
    fn set_fs_to_different_root_resets_state() {
        let tmp1 = fixture();
        let tmp2 = fixture();
        let mut state = state_for(&tmp1);
        state.expanded.insert(PathBuf::from("src"));
        state.set_fs(Some(Arc::new(LocalFs::new(tmp2.path()))));
        assert!(state.expanded.is_empty());
        assert!(state.selected.is_none());
    }

    /// A remote [`WorkspaceFs`] double whose `list_dir` panics — proving the
    /// render walk never calls it for a remote backend (it reads the cache).
    #[derive(Debug)]
    struct PanicRemoteFs {
        root: PathBuf,
    }

    impl WorkspaceFs for PanicRemoteFs {
        fn root_display(&self) -> &Path {
            &self.root
        }
        fn list_dir(&self, _rel: &Path) -> std::io::Result<Vec<DirEntry>> {
            panic!("remote list_dir must not run on the render path");
        }
        fn read_file(&self, _rel: &Path) -> std::io::Result<Vec<u8>> {
            panic!("remote read_file must not run on the render path");
        }
        fn git_branch(&self) -> Option<String> {
            None
        }
        fn git_status(
            &self,
            _include_ignored: bool,
        ) -> Result<HashMap<PathBuf, StatusKind>, fleet_commander_core::git::StatusError> {
            Ok(HashMap::new())
        }
        fn is_remote(&self) -> bool {
            true
        }
    }

    fn remote_state() -> ExplorerState {
        let mut state = ExplorerState::default();
        state.set_fs(Some(Arc::new(PanicRemoteFs {
            root: PathBuf::from("/workspaces/repo"),
        })));
        state
    }

    fn dir(name: &str) -> DirEntry {
        DirEntry {
            name: name.into(),
            is_dir: true,
        }
    }
    fn file(name: &str) -> DirEntry {
        DirEntry {
            name: name.into(),
            is_dir: false,
        }
    }

    #[test]
    fn remote_render_uses_cache_not_list_dir() {
        let mut state = remote_state();
        // Nothing cached yet: the walk must not touch the (panicking) fs,
        // and the root is reported as needing a fetch.
        assert!(state.visible_entries().is_empty());
        assert_eq!(state.missing_dirs(), vec![PathBuf::from("")]);

        // Once the root listing lands, it renders from the cache.
        state.apply_dir(PathBuf::from(""), Ok(vec![dir("src"), file("README.md")]));
        let entries = state.visible_entries();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["src", "README.md"]);
        // Root is satisfied; nothing expanded, so nothing else is missing.
        assert!(state.missing_dirs().is_empty());
    }

    #[test]
    fn remote_expanded_dir_is_reported_missing_then_renders() {
        let mut state = remote_state();
        state.apply_dir(PathBuf::from(""), Ok(vec![dir("src")]));
        state.expanded.insert(PathBuf::from("src"));
        // The expanded but uncached subdir is the next fetch target.
        assert_eq!(state.missing_dirs(), vec![PathBuf::from("src")]);

        // While loading, it isn't re-reported.
        state.mark_dir_loading(PathBuf::from("src"));
        assert!(state.missing_dirs().is_empty());

        state.apply_dir(PathBuf::from("src"), Ok(vec![file("lib.rs")]));
        assert!(
            state
                .visible_entries()
                .iter()
                .any(|e| e.name == "lib.rs" && e.depth == 1)
        );
    }

    #[test]
    fn invalidate_dirs_forces_remote_refetch() {
        let mut state = remote_state();
        state.apply_dir(PathBuf::from(""), Ok(vec![file("a.txt")]));
        assert!(!state.visible_entries().is_empty());
        state.invalidate_dirs();
        assert!(state.visible_entries().is_empty());
        assert_eq!(state.missing_dirs(), vec![PathBuf::from("")]);
    }
}
