//! Background self-update.
//!
//! diffski is installed with `cargo install --git`, so we update the same way:
//! on startup (rate-limited to once a day) a detached thread runs
//! `cargo install --git <repo>`, which rebuilds from `main` and atomically
//! swaps the binary in `~/.cargo/bin`. Because we don't pass `--force`, cargo
//! only reinstalls when the crate version is higher than what's installed — so
//! updates ship when the version is bumped, not on every commit, and you don't
//! recompile needlessly. The running process is unaffected; the new build is
//! picked up next launch.

use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const REPO_URL: &str = "https://github.com/koshea/diffski";
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Kick off a background update check if enabled and one is due. Never blocks;
/// on success-with-change it sends a short message on `tx` for the UI to show.
pub fn spawn_check(enabled: bool, tx: Sender<String>) {
    if !enabled || !check_due() {
        return;
    }
    thread::spawn(move || {
        let Ok(out) = Command::new("cargo")
            .args(["install", "--git", REPO_URL])
            .output()
        else {
            return; // cargo missing / offline — silently skip
        };
        // cargo logs progress to stderr; "Replacing" means the binary was
        // updated (vs. "already installed" when the version is unchanged).
        let log = String::from_utf8_lossy(&out.stderr);
        if out.status.success() && log.contains("Replacing") {
            let _ = tx.send("✨ diffski updated — restart to apply".to_string());
        }
    });
}

/// Whether enough time has passed since the last check. Records "now" as a side
/// effect when it returns true, so we check at most once per interval.
fn check_due() -> bool {
    let Some(path) = state_path() else {
        return false;
    };
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return false;
    };
    let now = now.as_secs();
    let last = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    if now.saturating_sub(last) < CHECK_INTERVAL.as_secs() {
        return false;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, now.to_string());
    true
}

fn state_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("diffski").join("update_check"))
}
