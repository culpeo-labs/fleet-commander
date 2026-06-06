//! Tab-completion for filesystem paths in command mode.
//!
//! The completer scans the directory implied by the partial path and offers
//! matching entries.  Repeated Tab presses cycle through candidates; any
//! other keypress resets the state.

use std::path::Path;

/// Persistent state for an in-progress tab-completion session.
#[derive(Debug, Default)]
pub struct PathCompleter {
    /// The candidates generated from the last Tab press.
    candidates: Vec<String>,
    /// Current index into `candidates`.
    index: usize,
    /// The raw user-typed text that triggered completion (everything after the
    /// command verb, e.g. the path portion of `:open /home/u`).
    trigger: String,
}

impl PathCompleter {
    /// Reset completion state — call when the user types any non-Tab key.
    pub fn reset(&mut self) {
        self.candidates.clear();
        self.index = 0;
        self.trigger.clear();
    }

    /// Return the next completion candidate (or the first if this is a fresh
    /// Tab press).  Returns `None` when no matches are found.
    pub fn complete(&mut self, partial: &str) -> Option<&str> {
        if self.candidates.is_empty() || self.trigger != partial {
            self.trigger = partial.to_string();
            self.candidates = list_candidates(partial);
            self.index = 0;
        } else {
            self.index = (self.index + 1) % self.candidates.len();
        }
        self.candidates.get(self.index).map(String::as_str)
    }

    /// Like `complete` but cycles backwards.
    pub fn complete_prev(&mut self, partial: &str) -> Option<&str> {
        if self.candidates.is_empty() || self.trigger != partial {
            self.trigger = partial.to_string();
            self.candidates = list_candidates(partial);
            self.index = self.candidates.len().saturating_sub(1);
        } else if self.index == 0 {
            self.index = self.candidates.len().saturating_sub(1);
        } else {
            self.index -= 1;
        }
        self.candidates.get(self.index).map(String::as_str)
    }

    /// Whether a completion session is active.
    #[allow(dead_code)]
    pub fn is_active(&self) -> bool {
        !self.candidates.is_empty()
    }

    /// Number of candidates in the current session.
    #[cfg(test)]
    pub fn candidate_count(&self) -> usize {
        self.candidates.len()
    }
}

/// Build sorted completion candidates for `partial`.
///
/// If `partial` ends with `/` we list that directory's children.
/// Otherwise we treat the last path component as a prefix filter
/// inside the parent directory.
fn list_candidates(partial: &str) -> Vec<String> {
    if partial.is_empty() {
        // List current directory.
        return read_dir_sorted(Path::new("."), "");
    }

    let path = Path::new(partial);

    if partial.ends_with('/') || partial.ends_with(std::path::MAIN_SEPARATOR) {
        // User typed a full directory — list its children.
        return read_dir_sorted(path, "");
    }

    // Split into parent dir + prefix.
    let parent = path.parent().unwrap_or(Path::new("."));
    let prefix = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    // If the partial is already a directory (no trailing slash), list children.
    if path.is_dir() {
        return read_dir_sorted(path, "");
    }

    read_dir_sorted(parent, prefix)
}

/// Read a directory and return entries whose name starts with `prefix`,
/// sorted alphabetically.  Directories get a trailing `/`.
fn read_dir_sorted(dir: &Path, prefix: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut results: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|entry| {
            let name = entry.file_name();
            let name_str = name.to_str()?;
            // Skip hidden entries unless the user typed a dot prefix.
            if name_str.starts_with('.') && !prefix.starts_with('.') {
                return None;
            }
            if !name_str.starts_with(prefix) {
                return None;
            }
            let full = entry.path();
            let display = if full.is_dir() {
                format!("{}/", full.display())
            } else {
                format!("{}", full.display())
            };
            // Normalise `./` prefix away for cleaner display.
            let display = display.strip_prefix("./").unwrap_or(&display).to_string();
            Some(display)
        })
        .collect();

    results.sort();
    results
}

/// Extract the path portion from a command buffer like `"open /home/user/re"`.
/// Returns `("open", "/home/user/re")`.
pub fn split_command_and_path(buf: &str) -> (&str, &str) {
    let buf = buf.trim_start();
    match buf.split_once(' ') {
        Some((verb, rest)) => (verb, rest.trim_start()),
        None => (buf, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn with_temp_dir(f: impl FnOnce(&Path)) {
        let dir = tempfile::tempdir().unwrap();
        f(dir.path());
    }

    #[test]
    fn list_candidates_finds_matching_entries() {
        with_temp_dir(|dir| {
            fs::create_dir(dir.join("alpha")).unwrap();
            fs::create_dir(dir.join("alpha-two")).unwrap();
            fs::write(dir.join("beta.txt"), "").unwrap();

            let partial = format!("{}/al", dir.display());
            let candidates = list_candidates(&partial);
            assert_eq!(candidates.len(), 2);
            let names: Vec<&str> = candidates.iter().map(|s| s.as_str()).collect();
            assert!(names.iter().any(|n| n.contains("alpha-two")));
            assert!(names.iter().any(|n| n.contains("alpha") && !n.contains("alpha-two")));
            // Directories get trailing slash.
            assert!(candidates.iter().all(|c| c.ends_with('/')));
        });
    }

    #[test]
    fn list_candidates_trailing_slash_lists_children() {
        with_temp_dir(|dir| {
            fs::write(dir.join("foo.rs"), "").unwrap();
            fs::write(dir.join("bar.rs"), "").unwrap();

            let partial = format!("{}/", dir.display());
            let candidates = list_candidates(&partial);
            assert!(candidates.len() >= 2);
        });
    }

    #[test]
    fn completer_cycles_through_candidates() {
        with_temp_dir(|dir| {
            fs::create_dir(dir.join("aaa")).unwrap();
            fs::create_dir(dir.join("aab")).unwrap();
            fs::create_dir(dir.join("aac")).unwrap();

            let partial = format!("{}/aa", dir.display());
            let mut completer = PathCompleter::default();

            let first = completer.complete(&partial).unwrap().to_string();
            assert!(first.contains("aaa"));

            let second = completer.complete(&partial).unwrap().to_string();
            assert!(second.contains("aab"));

            let third = completer.complete(&partial).unwrap().to_string();
            assert!(third.contains("aac"));

            // Wraps around.
            let fourth = completer.complete(&partial).unwrap().to_string();
            assert_eq!(fourth, first);
        });
    }

    #[test]
    fn completer_prev_cycles_backwards() {
        with_temp_dir(|dir| {
            fs::create_dir(dir.join("x1")).unwrap();
            fs::create_dir(dir.join("x2")).unwrap();

            let partial = format!("{}/x", dir.display());
            let mut completer = PathCompleter::default();

            // First prev should land on last candidate.
            let last = completer.complete_prev(&partial).unwrap().to_string();
            assert!(last.contains("x2"));

            let prev = completer.complete_prev(&partial).unwrap().to_string();
            assert!(prev.contains("x1"));
        });
    }

    #[test]
    fn split_command_and_path_works() {
        assert_eq!(split_command_and_path("open /home"), ("open", "/home"));
        assert_eq!(split_command_and_path("o ./foo"), ("o", "./foo"));
        assert_eq!(split_command_and_path("quit"), ("quit", ""));
        assert_eq!(split_command_and_path("  open   /x"), ("open", "/x"));
    }

    #[test]
    fn reset_clears_state() {
        let mut c = PathCompleter::default();
        c.candidates = vec!["a".into()];
        c.index = 5;
        c.trigger = "hello".into();
        c.reset();
        assert!(c.candidates.is_empty());
        assert_eq!(c.index, 0);
        assert!(c.trigger.is_empty());
    }
}
