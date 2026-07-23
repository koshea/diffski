//! Diff rendering via the `delta` binary, plus a small cache.
//!
//! We shell out to delta rather than linking it (delta exposes no stable
//! library API). delta is run in its *full* mode — not `--color-only` — so we
//! inherit its complete look (file-header decorations, line numbers, intra-line
//! highlighting) and the user's `[delta]` gitconfig settings. Empirically
//! delta 0.19 emits color even when its stdout is a pipe (our case), so no
//! pseudo-terminal is required; we still pass `--true-color always` to make the
//! color output deterministic regardless of the inherited environment.

use ansi_to_tui::IntoText;
use anyhow::{Context, Result, bail};
use ratatui::text::Text;
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Probe whether `delta` is installed.
///
/// diffski can fall back to plain ANSI `git diff` output when delta is missing,
/// so this is intentionally best-effort now.
pub fn ensure_available() -> Result<()> {
    let _ = is_available();
    Ok(())
}

/// Print the syntax-highlighting themes delta knows about (its
/// `--list-syntax-themes`), for `diffski --list-themes`.
pub fn list_syntax_themes() -> Result<()> {
    if !is_available() {
        bail!(
            "`delta` was not found on PATH.\n\
             Install it with `cargo install git-delta` or your system package manager."
        );
    }
    let status = Command::new("delta")
        .arg("--list-syntax-themes")
        .status()
        .context("failed to run `delta --list-syntax-themes`")?;
    if !status.success() {
        bail!("`delta --list-syntax-themes` failed");
    }
    Ok(())
}

/// Run delta over a raw (already git-colored) diff and parse the ANSI result
/// into a ratatui `Text`. Pure and thread-safe, so it can be called from a
/// worker pool to render many files concurrently.
///
/// `theme` overrides delta's syntax-highlighting theme (delta's
/// `--syntax-theme`); when `None`, delta uses whatever your gitconfig specifies.
/// `tabs` is the number of spaces a tab expands to.
pub fn render(raw: &[u8], width: u16, theme: Option<&str>, tabs: u16) -> Result<Text<'static>> {
    // Fall back to plain ANSI git diff output if delta isn't available. This
    // keeps diffski usable even on minimal systems where delta isn't installed.
    if !is_available() {
        return raw.into_text().context("failed to parse git's ANSI output");
    }

    // delta needs a sane width for its decorations/background fills.
    let width = width.max(20);

    let mut cmd = Command::new("delta");
    cmd.args(["--paging", "never", "--true-color", "always", "--width"])
        .arg(width.to_string())
        .arg("--tabs")
        .arg(tabs.to_string());
    if let Some(theme) = theme {
        cmd.args(["--syntax-theme", theme]);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn `delta`")?;

    // Feed delta on a separate thread: writing a large diff to stdin while delta
    // simultaneously writes a large output to stdout can deadlock if we do both
    // from this thread and a pipe buffer fills.
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let raw_owned = raw.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&raw_owned);
        // `stdin` is dropped here, signalling EOF to delta.
    });

    let out = child
        .wait_with_output()
        .context("failed to read delta output")?;
    let _ = writer.join();

    if !out.status.success() && out.stdout.is_empty() {
        bail!(
            "delta failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    out.stdout
        .into_text()
        .context("failed to parse delta's ANSI output")
}

pub fn is_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        Command::new("delta")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

/// Caches rendered diffs keyed by `(path, content signature, width)`.
///
/// The signature (see [`crate::app`]) changes when a file's content changes, so
/// an edit naturally produces a cache miss and re-render. Width is part of the
/// key so a terminal resize lazily re-renders each file at the new width.
#[derive(Default)]
pub struct DiffCache {
    map: HashMap<(String, u64, u16), Text<'static>>,
    /// ANSI-stripped raw diff text per (path, signature) — width- and
    /// theme-independent, used by content search.
    raw: HashMap<(String, u64), String>,
}

impl DiffCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the rendered diff for `path`, rendering and caching on a miss.
    /// `produce_raw` yields the raw colored diff and is invoked only on a miss,
    /// so cache hits never touch git or delta.
    pub fn get_or_render<F>(
        &mut self,
        path: &str,
        sig: u64,
        width: u16,
        theme: Option<&str>,
        tabs: u16,
        produce_raw: F,
    ) -> Result<Text<'static>>
    where
        F: FnOnce() -> Result<Vec<u8>>,
    {
        let key = (path.to_string(), sig, width);
        if let Some(text) = self.map.get(&key) {
            return Ok(text.clone());
        }
        let raw = produce_raw()?;
        self.raw
            .entry((path.to_string(), sig))
            .or_insert_with(|| crate::search::strip_ansi(&String::from_utf8_lossy(&raw)));
        let text = render(&raw, width, theme, tabs)?;
        self.map.insert(key, text.clone());
        Ok(text)
    }

    /// True if a render for this key is already cached.
    pub fn contains(&self, path: &str, sig: u64, width: u16) -> bool {
        self.map.contains_key(&(path.to_string(), sig, width))
    }

    /// Store a pre-rendered diff (used by the parallel pre-warm path).
    pub fn insert(&mut self, path: &str, sig: u64, width: u16, text: Text<'static>) {
        self.map.insert((path.to_string(), sig, width), text);
    }

    /// The ANSI-stripped raw diff for `path` at `sig`, if retained.
    pub fn raw_text(&self, path: &str, sig: u64) -> Option<&str> {
        self.raw.get(&(path.to_string(), sig)).map(String::as_str)
    }

    /// Retain the ANSI-stripped raw diff for `path` at `sig`.
    pub fn insert_raw(&mut self, path: &str, sig: u64, stripped: String) {
        self.raw.insert((path.to_string(), sig), stripped);
    }

    /// Drop all cached renders for a path (any signature/width). Called when the
    /// watcher reports a path changed, to bound memory over a long session.
    pub fn invalidate_path(&mut self, path: &str) {
        self.map.retain(|(p, _, _), _| p != path);
        self.raw.retain(|(p, _), _| p != path);
    }

    /// Drop every cached render. Used when the theme changes, since the theme
    /// is not part of the cache key (it's fixed except when explicitly switched).
    /// Raw text is intentionally kept: it's theme-independent, so a theme
    /// switch shouldn't force content search to re-fetch and re-strip diffs.
    pub fn clear(&mut self) {
        self.map.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::DiffCache;

    #[test]
    fn raw_text_round_trip_and_invalidation() {
        let mut cache = DiffCache::new();
        assert_eq!(cache.raw_text("a.rs", 1), None);
        cache.insert_raw("a.rs", 1, "+added".to_string());
        assert_eq!(cache.raw_text("a.rs", 1), Some("+added"));
        assert_eq!(cache.raw_text("a.rs", 2), None); // signature mismatch
        cache.clear(); // theme change: raw text survives
        assert_eq!(cache.raw_text("a.rs", 1), Some("+added"));
        cache.invalidate_path("a.rs"); // file change: raw text dropped
        assert_eq!(cache.raw_text("a.rs", 1), None);
    }
}
