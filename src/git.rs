//! Git integration: resolve the repo, list changed files, and produce the raw
//! (ANSI-colored) per-file diff that we later feed to delta.
//!
//! Everything shells out to `git` so behavior matches the user's own git config.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// The empty-tree object id. Used as the diff base when the repo has no commits
/// yet (no `HEAD`), so staged/tracked files still render as fully added.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
}

impl ChangeKind {
    /// Single-character status marker shown in the file list.
    pub fn marker(self) -> char {
        match self {
            ChangeKind::Modified => 'M',
            ChangeKind::Added => 'A',
            ChangeKind::Deleted => 'D',
            ChangeKind::Renamed => 'R',
            ChangeKind::Untracked => '?',
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    /// Repo-relative path (used both for display and as the git pathspec).
    pub path: String,
    pub kind: ChangeKind,
    /// Worktree mtime, used for the "sort by modified date" mode. Deleted files
    /// (no worktree entry) fall back to the UNIX epoch.
    pub mtime: SystemTime,
}

/// Resolve the git repository (or worktree) root for `path`.
pub fn repo_root(path: &Path) -> Result<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("failed to run `git` — is it installed and on PATH?")?;
    if !out.status.success() {
        bail!("not a git repository: {}", path.display());
    }
    let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if root.is_empty() {
        bail!(
            "could not resolve git repository root for {}",
            path.display()
        );
    }
    Ok(PathBuf::from(root))
}

/// The diff base: `HEAD` when there is at least one commit, otherwise the empty
/// tree so that a fresh repo's staged files still show up as additions.
pub fn diff_base(root: &Path) -> String {
    let has_head = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if has_head {
        "HEAD".to_string()
    } else {
        EMPTY_TREE.to_string()
    }
}

/// List all files changed since the diff base, including untracked files.
///
/// Uses `git status --porcelain=v1 -z --untracked-files=all`, which honors
/// `.gitignore` automatically.
pub fn changed_files(root: &Path) -> Result<Vec<FileEntry>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .output()
        .context("failed to run `git status`")?;
    if !out.status.success() {
        bail!(
            "`git status` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let text = String::from_utf8_lossy(&out.stdout);
    // NUL-separated records. Rename/copy entries are followed by an extra
    // NUL-terminated field holding the original path, which we consume/skip.
    let tokens: Vec<&str> = text.split('\0').filter(|t| !t.is_empty()).collect();
    let mut entries = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        if tok.len() < 3 {
            i += 1;
            continue;
        }
        let bytes = tok.as_bytes();
        let x = bytes[0] as char;
        let y = bytes[1] as char;
        let path = tok[3..].to_string();
        let kind = classify(x, y);
        let is_rename = matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C');

        let mtime = std::fs::metadata(root.join(&path))
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);

        entries.push(FileEntry { path, kind, mtime });

        // Skip the original-path token that follows a rename/copy record.
        i += if is_rename { 2 } else { 1 };
    }

    Ok(entries)
}

fn classify(x: char, y: char) -> ChangeKind {
    if x == '?' && y == '?' {
        ChangeKind::Untracked
    } else if x == 'D' || y == 'D' {
        ChangeKind::Deleted
    } else if x == 'R' || y == 'R' || x == 'C' || y == 'C' {
        ChangeKind::Renamed
    } else if x == 'A' {
        ChangeKind::Added
    } else {
        ChangeKind::Modified
    }
}

fn mtime_of(root: &Path, path: &str) -> SystemTime {
    std::fs::metadata(root.join(path))
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Resolve the base branch to diff against: the remote's default branch if known
/// (`origin/HEAD`), else the first of a few common candidates that exists.
pub fn base_branch(root: &Path) -> Option<String> {
    // origin/HEAD -> e.g. "refs/remotes/origin/main"
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])
        .output()
        .ok()?;
    if out.status.success() {
        let full = String::from_utf8_lossy(&out.stdout);
        if let Some(name) = full.trim().strip_prefix("refs/remotes/") {
            return Some(name.to_string());
        }
    }

    for cand in ["origin/main", "origin/master", "main", "master"] {
        let ok = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "--verify", "--quiet", cand])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return Some(cand.to_string());
        }
    }
    None
}

/// The merge base between `HEAD` and `base_branch` — the commit the current work
/// diverged from, which is what we diff against in base-branch mode.
pub fn merge_base(root: &Path, base_branch: &str) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["merge-base", "HEAD", base_branch])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// Files that differ between `base` (a commit) and the current working tree —
/// committed *and* uncommitted tracked changes — plus untracked files. This is
/// the file list for base-branch mode.
pub fn branch_changes(root: &Path, base: &str) -> Result<Vec<FileEntry>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--name-status", "-z", base])
        .output()
        .context("failed to run `git diff --name-status`")?;
    if !out.status.success() {
        bail!(
            "`git diff --name-status` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let tokens: Vec<&str> = text.split('\0').filter(|t| !t.is_empty()).collect();
    let mut entries = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let status = tokens[i];
        let code = status.chars().next().unwrap_or('M');
        // Rename/copy records are `Rxxx\0old\0new`; everything else is `S\0path`.
        let (path, step) = if matches!(code, 'R' | 'C') {
            match tokens.get(i + 2) {
                Some(new) => (new.to_string(), 3),
                None => break,
            }
        } else {
            match tokens.get(i + 1) {
                Some(p) => (p.to_string(), 2),
                None => break,
            }
        };
        let kind = match code {
            'A' => ChangeKind::Added,
            'D' => ChangeKind::Deleted,
            'R' | 'C' => ChangeKind::Renamed,
            _ => ChangeKind::Modified,
        };
        let mtime = mtime_of(root, &path);
        entries.push(FileEntry { path, kind, mtime });
        i += step;
    }

    entries.extend(untracked_files(root)?);
    Ok(entries)
}

/// Untracked (and not-ignored) files, from `git status`.
fn untracked_files(root: &Path) -> Result<Vec<FileEntry>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .output()
        .context("failed to run `git status`")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut entries = Vec::new();
    let tokens: Vec<&str> = text.split('\0').filter(|t| !t.is_empty()).collect();
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        if tok.len() >= 3 && &tok[0..2] == "??" {
            let path = tok[3..].to_string();
            let mtime = mtime_of(root, &path);
            entries.push(FileEntry {
                path,
                kind: ChangeKind::Untracked,
                mtime,
            });
        } else if tok.len() >= 2 && matches!(tok.as_bytes()[0], b'R' | b'C') {
            // Consume the rename original-path token so it isn't misread.
            i += 1;
        }
        i += 1;
    }
    Ok(entries)
}

/// Produce the raw, ANSI-colored diff for a single file.
///
/// - Untracked files are diffed against `/dev/null` via `--no-index` (which
///   exits non-zero by design), rendering the whole file as added.
/// - Everything else is diffed against `base` (`HEAD` or the empty tree).
///
/// The output is git's colored unified diff, ready to be piped through delta.
pub fn raw_diff(root: &Path, entry: &FileEntry, base: &str) -> Result<Vec<u8>> {
    let out = if entry.kind == ChangeKind::Untracked {
        let abs = root.join(&entry.path);
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["diff", "--no-index", "--color=always", "--"])
            .arg("/dev/null")
            .arg(&abs)
            .output()
            .context("failed to run `git diff --no-index`")?
    } else {
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["diff", "--color=always", base, "--"])
            .arg(&entry.path)
            .output()
            .context("failed to run `git diff`")?
    };

    // `git diff --no-index` returns exit code 1 when files differ — that is the
    // normal "there is a diff" case, not an error. A real failure writes to
    // stderr and produces no stdout, so only surface those.
    if out.stdout.is_empty() && !out.stderr.is_empty() {
        bail!(
            "git diff failed for {}: {}",
            entry.path,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    Ok(out.stdout)
}
