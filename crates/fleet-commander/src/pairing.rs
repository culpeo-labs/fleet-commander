//! Cross-workspace pairing state (Feature 2).
//!
//! Before an agent in one workspace may message an agent in another, the user
//! must explicitly "connect" the two. Pairings are **undirected** (A↔B) and
//! persisted globally in `~/.config/fleet-commander/pairings.yaml`, keyed by
//! [`AgentId`]. Feature 2c consults this store to filter which peers a
//! `send_to_workspace` call may target.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::info;

use crate::agent::AgentId;

/// On-disk format: a flat list of `[a, b]` pairs. Order within a pair is
/// normalized on load, so the persisted set is undirected regardless of how it
/// was written.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PairingFile {
    #[serde(default)]
    pairs: Vec<[AgentId; 2]>,
}

/// In-memory set of undirected pairings, each stored as an ordered
/// `(low, high)` tuple so `(A,B)` and `(B,A)` collapse to one entry.
#[derive(Debug, Clone, Default)]
pub struct PairingStore {
    pairs: BTreeSet<(AgentId, AgentId)>,
}

/// Order a pair canonically, rejecting self-pairs (`a == b`).
fn normalize(a: &str, b: &str) -> Option<(AgentId, AgentId)> {
    match a.cmp(b) {
        std::cmp::Ordering::Less => Some((a.to_string(), b.to_string())),
        std::cmp::Ordering::Greater => Some((b.to_string(), a.to_string())),
        std::cmp::Ordering::Equal => None,
    }
}

impl PairingStore {
    /// Where the file lives: `~/.config/fleet-commander/pairings.yaml`.
    pub fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("fleet-commander").join("pairings.yaml"))
    }

    /// Load pairings from disk. Returns an empty store on any error.
    pub fn load() -> Self {
        match Self::config_path() {
            Some(path) => Self::load_from(&path),
            None => Self::default(),
        }
    }

    fn load_from(path: &Path) -> Self {
        let Ok(contents) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let file: PairingFile = serde_yaml::from_str(&contents).unwrap_or_default();
        let mut pairs = BTreeSet::new();
        for [a, b] in file.pairs {
            if let Some(p) = normalize(&a, &b) {
                pairs.insert(p);
            }
        }
        info!(count = pairs.len(), "Loaded workspace pairings");
        Self { pairs }
    }

    /// Persist pairings to disk, creating parent dirs if needed.
    pub fn save(&self) -> Result<(), String> {
        let path = Self::config_path().ok_or("Could not determine config directory")?;
        self.save_to(&path)
    }

    fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config dir: {e}"))?;
        }
        let file = PairingFile {
            pairs: self
                .pairs
                .iter()
                .map(|(a, b)| [a.clone(), b.clone()])
                .collect(),
        };
        let yaml = serde_yaml::to_string(&file).map_err(|e| format!("Failed to serialize: {e}"))?;
        std::fs::write(path, yaml)
            .map_err(|e| format!("Failed to write {}: {e}", path.display()))?;
        Ok(())
    }

    /// Connect two agents. Returns `true` if this created a new pairing (`false`
    /// if they were already connected or `a == b`).
    pub fn connect(&mut self, a: &str, b: &str) -> bool {
        match normalize(a, b) {
            Some(p) => self.pairs.insert(p),
            None => false,
        }
    }

    /// Disconnect two agents. Returns `true` if a pairing was removed.
    pub fn disconnect(&mut self, a: &str, b: &str) -> bool {
        match normalize(a, b) {
            Some(p) => self.pairs.remove(&p),
            None => false,
        }
    }

    /// Whether `a` and `b` are connected. Feature 2c's `send_to_workspace`
    /// authorization gate.
    pub fn is_connected(&self, a: &str, b: &str) -> bool {
        match normalize(a, b) {
            Some(p) => self.pairs.contains(&p),
            None => false,
        }
    }

    /// All agents paired with `agent`, sorted ascending.
    pub fn peers(&self, agent: &str) -> Vec<AgentId> {
        let mut out: Vec<AgentId> = self
            .pairs
            .iter()
            .filter_map(|(a, b)| {
                if a == agent {
                    Some(b.clone())
                } else if b == agent {
                    Some(a.clone())
                } else {
                    None
                }
            })
            .collect();
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_is_undirected_and_deduped() {
        let mut store = PairingStore::default();
        assert!(store.connect("copilot-a", "copilot-b"));
        // Reversed order is the same pairing.
        assert!(!store.connect("copilot-b", "copilot-a"));
        assert!(store.is_connected("copilot-a", "copilot-b"));
        assert!(store.is_connected("copilot-b", "copilot-a"));
    }

    #[test]
    fn self_pair_is_rejected() {
        let mut store = PairingStore::default();
        assert!(!store.connect("copilot-a", "copilot-a"));
        assert!(!store.is_connected("copilot-a", "copilot-a"));
    }

    #[test]
    fn disconnect_removes_pairing() {
        let mut store = PairingStore::default();
        store.connect("copilot-a", "copilot-b");
        assert!(store.disconnect("copilot-b", "copilot-a"));
        assert!(!store.is_connected("copilot-a", "copilot-b"));
        assert!(!store.disconnect("copilot-a", "copilot-b"));
    }

    #[test]
    fn peers_lists_all_connections_sorted() {
        let mut store = PairingStore::default();
        store.connect("copilot-a", "copilot-c");
        store.connect("copilot-a", "copilot-b");
        store.connect("copilot-b", "copilot-d");
        assert_eq!(store.peers("copilot-a"), vec!["copilot-b", "copilot-c"]);
        assert_eq!(store.peers("copilot-d"), vec!["copilot-b"]);
        assert!(store.peers("copilot-z").is_empty());
    }

    #[test]
    fn round_trip_yaml_normalizes_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairings.yaml");

        let mut store = PairingStore::default();
        store.connect("copilot-b", "copilot-a"); // written reversed
        store.save_to(&path).unwrap();

        let loaded = PairingStore::load_from(&path);
        assert!(loaded.is_connected("copilot-a", "copilot-b"));
        assert_eq!(loaded.peers("copilot-a"), vec!["copilot-b"]);
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let store = PairingStore::load_from(Path::new("/nonexistent/pairings.yaml"));
        assert!(store.peers("copilot-a").is_empty());
    }
}
