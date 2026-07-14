//! Filesystem watching. A recursive, debounced `notify` watcher on the repo
//! root forwards the set of changed paths into the event loop over an mpsc
//! channel. `git status` remains the source of truth for *what* changed; these
//! events just tell the app *when* to refresh and which cache entries to drop.

use anyhow::{Context, Result};
use notify_debouncer_mini::notify::RecursiveMode;
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

/// Coalesce a burst of writes (e.g. an agent saving many files) into one refresh.
const DEBOUNCE: Duration = Duration::from_millis(150);

/// A change event: the absolute paths that changed since the last debounce tick,
/// with anything under `.git/` already filtered out.
pub type WatchEvent = Vec<PathBuf>;

/// Owns the running watcher. Dropping it stops watching, so the caller must keep
/// it alive for as long as updates are wanted.
pub struct Watcher {
    _debouncer: Debouncer<notify_debouncer_mini::notify::RecommendedWatcher>,
}

/// Start watching `root` recursively. Changed paths are sent on `tx`.
pub fn watch(root: &Path, tx: Sender<WatchEvent>) -> Result<Watcher> {
    let mut debouncer = new_debouncer(DEBOUNCE, move |res: DebounceEventResult| {
        if let Ok(events) = res {
            let paths: Vec<PathBuf> = events
                .into_iter()
                .map(|e| e.path)
                .filter(|p| !is_git_internal(p))
                .collect();
            if !paths.is_empty() {
                // If the receiver is gone the app is shutting down; ignore.
                let _ = tx.send(paths);
            }
        }
    })
    .context("failed to create filesystem watcher")?;

    debouncer
        .watcher()
        .watch(root, RecursiveMode::Recursive)
        .with_context(|| format!("failed to watch {}", root.display()))?;

    Ok(Watcher {
        _debouncer: debouncer,
    })
}

/// True for paths inside a `.git` directory — git's own churn (index, refs,
/// lock files) would otherwise cause a refresh storm during commits.
fn is_git_internal(path: &Path) -> bool {
    path.components()
        .any(|c| matches!(c, Component::Normal(name) if name == ".git"))
}
