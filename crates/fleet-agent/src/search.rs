//! Content search over the workspace, backed by ripgrep's libraries
//! (`ignore` for the gitignore-aware walk, `grep-searcher` + `grep-regex`
//! for line matching). Kept free of any protocol/streaming concerns: the
//! caller supplies an `on_match` callback and a cancellation flag, and this
//! module drives the walk sequentially so cancellation and result caps are
//! honored promptly.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use fleet_protocol::SearchMatch;
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::sinks::UTF8;
use grep_searcher::{Searcher, SearcherBuilder};
use ignore::WalkBuilder;

/// A parsed content-search request (protocol-agnostic).
pub struct SearchRequest {
    pub query: String,
    pub is_regex: bool,
    pub case_sensitive: bool,
    pub max_results: Option<u64>,
}

/// How a search finished.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SearchOutcome {
    pub count: u64,
    /// Stopped early because `max_results` was reached.
    pub truncated: bool,
    /// Stopped early because the cancellation flag was set.
    pub cancelled: bool,
}

/// Run a content search rooted at `root`, invoking `on_match` for every hit.
///
/// The walk respects `.gitignore` and skips hidden files, mirroring
/// ripgrep's defaults. `cancel` is polled between files (and per match) so a
/// long search stops promptly. Returns `Err` only for an invalid query;
/// per-file IO errors are skipped so one unreadable file can't abort the run.
pub fn run_search(
    root: &Path,
    req: &SearchRequest,
    cancel: &AtomicBool,
    mut on_match: impl FnMut(SearchMatch),
) -> Result<SearchOutcome, String> {
    let matcher = build_matcher(req)?;

    let mut searcher = SearcherBuilder::new().line_number(true).build();

    let mut outcome = SearchOutcome::default();
    for result in WalkBuilder::new(root)
        .hidden(true)
        .require_git(false)
        .build()
    {
        if cancel.load(Ordering::Relaxed) {
            outcome.cancelled = true;
            break;
        }
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");

        search_file(
            &mut searcher,
            &matcher,
            entry.path(),
            &rel,
            cancel,
            req.max_results,
            &mut outcome,
            &mut on_match,
        );

        if outcome.truncated || outcome.cancelled {
            break;
        }
    }
    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
fn search_file(
    searcher: &mut Searcher,
    matcher: &RegexMatcher,
    path: &Path,
    rel: &str,
    cancel: &AtomicBool,
    max_results: Option<u64>,
    outcome: &mut SearchOutcome,
    on_match: &mut impl FnMut(SearchMatch),
) {
    let _ = searcher.search_path(
        matcher,
        path,
        UTF8(|line_number, line| {
            if cancel.load(Ordering::Relaxed) {
                outcome.cancelled = true;
                return Ok(false);
            }
            // Column of the first match within the line (1-based byte offset).
            let column = matcher
                .find(line.as_bytes())
                .ok()
                .flatten()
                .map(|m| m.start() as u64 + 1)
                .unwrap_or(1);
            on_match(SearchMatch {
                path: rel.to_string(),
                line: line_number,
                column,
                text: line.trim_end_matches(['\r', '\n']).to_string(),
            });
            outcome.count += 1;
            if let Some(max) = max_results
                && outcome.count >= max
            {
                outcome.truncated = true;
                return Ok(false);
            }
            Ok(true)
        }),
    );
}

/// Build the line matcher for `req`, escaping the query unless it's a regex.
/// Shared by [`run_search`] and [`validate`] so both agree on pattern syntax.
fn build_matcher(req: &SearchRequest) -> Result<RegexMatcher, String> {
    let pattern = if req.is_regex {
        req.query.clone()
    } else {
        escape_literal(&req.query)
    };
    RegexMatcherBuilder::new()
        .case_insensitive(!req.case_sensitive)
        .build(&pattern)
        .map_err(|e| format!("invalid search pattern: {e}"))
}

/// Validate that `req`'s query compiles, without walking the workspace.
/// Used to reject an invalid pattern before spawning a search worker.
pub fn validate(req: &SearchRequest) -> Result<(), String> {
    build_matcher(req).map(|_| ())
}

/// Escape regex metacharacters so a literal query matches verbatim.
fn escape_literal(query: &str) -> String {
    const META: &str = r"\.+*?()|[]{}^$";
    let mut out = String::with_capacity(query.len());
    for c in query.chars() {
        if META.contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn fixture() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/a.rs"), "fn alpha() {}\nlet x = 1;\n").unwrap();
        fs::write(
            tmp.path().join("src/b.rs"),
            "fn beta() {}\n// alpha again\n",
        )
        .unwrap();
        fs::write(tmp.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(tmp.path().join("ignored.txt"), "alpha in ignored\n").unwrap();
        tmp
    }

    fn collect(tmp: &TempDir, req: SearchRequest) -> (Vec<SearchMatch>, SearchOutcome) {
        let cancel = AtomicBool::new(false);
        let mut hits = Vec::new();
        let outcome = run_search(tmp.path(), &req, &cancel, |m| hits.push(m)).unwrap();
        (hits, outcome)
    }

    #[test]
    fn finds_literal_matches_across_files() {
        let tmp = fixture();
        let (hits, outcome) = collect(
            &tmp,
            SearchRequest {
                query: "alpha".into(),
                is_regex: false,
                case_sensitive: false,
                max_results: None,
            },
        );
        // Two matches in tracked files; the gitignored file is skipped.
        assert_eq!(outcome.count, 2, "hits: {hits:?}");
        assert!(hits.iter().any(|m| m.path == "src/a.rs" && m.line == 1));
        assert!(hits.iter().any(|m| m.path == "src/b.rs" && m.line == 2));
        assert!(hits.iter().all(|m| m.path != "ignored.txt"));
    }

    #[test]
    fn literal_query_does_not_treat_metacharacters_as_regex() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("f.txt"), "a.b\naxb\n").unwrap();
        let (hits, outcome) = collect(
            &tmp,
            SearchRequest {
                query: "a.b".into(),
                is_regex: false,
                case_sensitive: true,
                max_results: None,
            },
        );
        assert_eq!(outcome.count, 1, "hits: {hits:?}");
        assert_eq!(hits[0].text, "a.b");
    }

    #[test]
    fn regex_query_matches_pattern() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("f.txt"), "a.b\naxb\n").unwrap();
        let (_, outcome) = collect(
            &tmp,
            SearchRequest {
                query: "a.b".into(),
                is_regex: true,
                case_sensitive: true,
                max_results: None,
            },
        );
        assert_eq!(outcome.count, 2);
    }

    #[test]
    fn case_sensitivity_is_honored() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("f.txt"), "Alpha\nalpha\n").unwrap();
        let (_, sensitive) = collect(
            &tmp,
            SearchRequest {
                query: "alpha".into(),
                is_regex: false,
                case_sensitive: true,
                max_results: None,
            },
        );
        assert_eq!(sensitive.count, 1);
    }

    #[test]
    fn max_results_truncates() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("f.txt"), "x\nx\nx\nx\n").unwrap();
        let (hits, outcome) = collect(
            &tmp,
            SearchRequest {
                query: "x".into(),
                is_regex: false,
                case_sensitive: false,
                max_results: Some(2),
            },
        );
        assert_eq!(hits.len(), 2);
        assert!(outcome.truncated);
    }

    #[test]
    fn reports_column() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("f.txt"), "  needle\n").unwrap();
        let (hits, _) = collect(
            &tmp,
            SearchRequest {
                query: "needle".into(),
                is_regex: false,
                case_sensitive: false,
                max_results: None,
            },
        );
        assert_eq!(hits[0].column, 3);
    }

    #[test]
    fn invalid_regex_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let cancel = AtomicBool::new(false);
        let err = run_search(
            tmp.path(),
            &SearchRequest {
                query: "(unclosed".into(),
                is_regex: true,
                case_sensitive: false,
                max_results: None,
            },
            &cancel,
            |_| {},
        );
        assert!(err.is_err());
    }
}
