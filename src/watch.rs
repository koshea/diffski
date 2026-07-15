//! Change detection by polling `git status` on a background thread.
//!
//! We poll rather than use an inotify-style recursive watcher: a large monorepo
//! can have well over a million directories, which is slow to watch and blows
//! past the OS watch limit. `git status` stays fast (tens of milliseconds) at
//! any repo size and honors `.gitignore` for free, so it's both cheaper and
//! more robust. The signature also folds in the mtimes of changed files, so an
//! edit to an already-modified file (whose status line is unchanged) is caught.

use anyhow::Result;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

/// Baseline poll cadence. We never poll more often than ~3× the time a poll
/// takes, so on a repo where `git status` is slow we back off automatically.
const POLL_INTERVAL: Duration = Duration::from_millis(600);

/// A change signal. Carries no payload — the app recomputes what changed itself.
pub type WatchEvent = ();

/// Owns the polling thread. The thread stops on its own when `tx`'s receiver is
/// dropped (i.e. the app is exiting), so there's nothing to join explicitly.
pub struct Watcher {
    _handle: thread::JoinHandle<()>,
}

pub fn watch(root: &Path, tx: Sender<WatchEvent>) -> Result<Watcher> {
    let root = root.to_path_buf();
    let handle = thread::spawn(move || {
        let mut prev: Option<String> = None;
        loop {
            let started = Instant::now();
            let sig = change_signature(&root);
            // Signal on a real change; a send error means the app exited.
            if let Some(p) = &prev
                && *p != sig
                && tx.send(()).is_err()
            {
                break;
            }
            prev = Some(sig);

            let elapsed = started.elapsed();
            thread::sleep(POLL_INTERVAL.max(elapsed.saturating_mul(3)));
        }
    });
    Ok(Watcher { _handle: handle })
}

/// A cheap fingerprint of the working tree: `git status` output plus the mtimes
/// of the files it lists. Changes whenever files are added/removed/renamed or an
/// existing changed file is edited again.
fn change_signature(root: &Path) -> String {
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .output()
    else {
        return String::new();
    };
    let status = String::from_utf8_lossy(&out.stdout);
    let mut sig = status.to_string();

    for tok in status.split('\0') {
        // Status entries look like "XY path"; skip rename-origin path tokens and
        // anything that isn't a proper entry. `get` avoids slicing panics.
        if tok.as_bytes().get(2) != Some(&b' ') {
            continue;
        }
        let Some(path) = tok.get(3..) else { continue };
        if let Ok(m) = std::fs::metadata(root.join(path)).and_then(|md| md.modified())
            && let Ok(d) = m.duration_since(UNIX_EPOCH)
        {
            let _ = write!(sig, "|{}:{}", path, d.as_nanos());
        }
    }
    sig
}
