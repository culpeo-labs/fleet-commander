//! Sources of file-change events that the TUI can surface as diffs.
//!
//! The default implementation is a filesystem watcher (`FsWatcher`) but the
//! trait is intentionally small so richer per-agent integrations (structured
//! stdout protocol, wrapper/proxy) can plug in later.

use anyhow::Result;
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::thread;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    Created,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeEvent {
    pub path: PathBuf,
    pub kind: ChangeKind,
}

/// A handle that keeps the source alive. Drop to stop emitting events.
pub struct ChangeSourceHandle {
    _inner: Box<dyn std::any::Any + Send>,
}

impl ChangeSourceHandle {
    pub fn new<T: std::any::Any + Send>(inner: T) -> Self {
        Self {
            _inner: Box::new(inner),
        }
    }

    #[cfg(test)]
    pub fn empty() -> Self {
        Self::new(())
    }
}

pub trait ChangeSource: Send {
    /// Start emitting events into `sink`. The returned handle must be kept
    /// alive for as long as the caller wants to keep receiving events.
    fn start(
        self: Box<Self>,
        sink: mpsc::UnboundedSender<ChangeEvent>,
    ) -> Result<ChangeSourceHandle>;
}

/// Default `ChangeSource`: a recursive filesystem watcher rooted at `root`.
pub struct FsWatcher {
    pub root: PathBuf,
}

impl FsWatcher {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl ChangeSource for FsWatcher {
    fn start(
        self: Box<Self>,
        sink: mpsc::UnboundedSender<ChangeEvent>,
    ) -> Result<ChangeSourceHandle> {
        use notify::{EventKind, RecursiveMode, Watcher};

        let (tx, rx) = std_mpsc::channel::<notify::Result<notify::Event>>();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })?;
        watcher.watch(&self.root, RecursiveMode::Recursive)?;

        thread::spawn(move || {
            for res in rx {
                let Ok(event) = res else { continue };
                let kind = match event.kind {
                    EventKind::Create(_) => ChangeKind::Created,
                    EventKind::Modify(_) => ChangeKind::Modified,
                    EventKind::Remove(_) => ChangeKind::Deleted,
                    _ => continue,
                };
                for path in event.paths {
                    if sink.send(ChangeEvent { path, kind }).is_err() {
                        return;
                    }
                }
            }
        });

        Ok(ChangeSourceHandle::new(watcher))
    }
}

/// A deterministic `ChangeSource` for tests. Emits the supplied events
/// synchronously when started.
#[cfg(test)]
pub struct MockChangeSource {
    pub events: Vec<ChangeEvent>,
}

#[cfg(test)]
impl ChangeSource for MockChangeSource {
    fn start(
        self: Box<Self>,
        sink: mpsc::UnboundedSender<ChangeEvent>,
    ) -> Result<ChangeSourceHandle> {
        for event in self.events {
            let _ = sink.send(event);
        }
        Ok(ChangeSourceHandle::empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;

    #[tokio::test]
    async fn mock_source_emits_scripted_events() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let source: Box<dyn ChangeSource> = Box::new(MockChangeSource {
            events: vec![
                ChangeEvent {
                    path: PathBuf::from("a.rs"),
                    kind: ChangeKind::Modified,
                },
                ChangeEvent {
                    path: PathBuf::from("b.rs"),
                    kind: ChangeKind::Created,
                },
            ],
        });
        let _handle = source.start(tx).unwrap();

        let first = rx.recv().await.unwrap();
        assert_eq!(first.path, PathBuf::from("a.rs"));
        assert_eq!(first.kind, ChangeKind::Modified);

        let second = rx.recv().await.unwrap();
        assert_eq!(second.path, PathBuf::from("b.rs"));
        assert_eq!(second.kind, ChangeKind::Created);
    }

    #[tokio::test]
    async fn fs_watcher_emits_on_file_creation() {
        let dir = tempdir().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let source: Box<dyn ChangeSource> = Box::new(FsWatcher::new(dir.path().to_path_buf()));
        let _handle = source.start(tx).unwrap();

        // Give the watcher a moment to set up before we mutate the directory.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let file_path = dir.path().join("new.txt");
        std::fs::write(&file_path, b"hi").unwrap();

        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for fs event")
            .expect("channel closed");
        assert!(matches!(
            event.kind,
            ChangeKind::Created | ChangeKind::Modified
        ));
    }
}
