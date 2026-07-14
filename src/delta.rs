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
use anyhow::{Context, Result, anyhow, bail};
use ratatui::text::Text;
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

/// Verify the `delta` binary is available. Called once at startup so we can fail
/// with a helpful message instead of erroring on the first diff.
pub fn ensure_available() -> Result<()> {
    Command::new("delta")
        .arg("--version")
        .output()
        .map(|_| ())
        .map_err(|_| {
            anyhow!(
                "`delta` was not found on PATH.\n\n\
                 diffski renders diffs with delta (https://github.com/dandavison/delta).\n\
                 Install it with `cargo install git-delta` or your system package manager."
            )
        })
}

/// Print the syntax-highlighting themes delta knows about (its
/// `--list-syntax-themes`), for `diffski --list-themes`.
pub fn list_syntax_themes() -> Result<()> {
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
pub fn render(raw: &[u8], width: u16, theme: Option<&str>) -> Result<Text<'static>> {
    // delta needs a sane width for its decorations/background fills.
    let width = width.max(20);

    let mut cmd = Command::new("delta");
    cmd.args(["--paging", "never", "--true-color", "always", "--width"])
        .arg(width.to_string());
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
        bail!("delta failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }

    out.stdout
        .into_text()
        .context("failed to parse delta's ANSI output")
}

/// Caches rendered diffs keyed by `(path, content signature, width)`.
///
/// The signature (see [`crate::app`]) changes when a file's content changes, so
/// an edit naturally produces a cache miss and re-render. Width is part of the
/// key so a terminal resize lazily re-renders each file at the new width.
#[derive(Default)]
pub struct DiffCache {
    map: HashMap<(String, u64, u16), Text<'static>>,
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
        let text = render(&raw, width, theme)?;
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

    /// Drop all cached renders for a path (any signature/width). Called when the
    /// watcher reports a path changed, to bound memory over a long session.
    pub fn invalidate_path(&mut self, path: &str) {
        self.map.retain(|(p, _, _), _| p != path);
    }

    /// Drop every cached render. Used when the theme changes, since the theme
    /// is not part of the cache key (it's fixed except when explicitly switched).
    pub fn clear(&mut self) {
        self.map.clear();
    }
}
