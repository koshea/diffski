//! Git integration: resolve the repo, list changed files, and produce the raw
//! (ANSI-colored) per-file diff that we later feed to delta.
//!
//! Everything shells out to `git` so behavior matches the user's own git config.

use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// The empty-tree object id. Used as the diff base when the repo has no commits
/// yet (no `HEAD`), so staged/tracked files still render as fully added.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
            ChangeKind::Untracked => 'U',
        }
    }
}

/// Canonical order the five change kinds are always shown in — chips,
/// footer pills, and the kind-filter keyboard mode all share this order so
/// rendering and click hit-testing can never drift out of sync.
pub const KIND_ORDER: [ChangeKind; 5] = [
    ChangeKind::Added,
    ChangeKind::Modified,
    ChangeKind::Deleted,
    ChangeKind::Renamed,
    ChangeKind::Untracked,
];

#[derive(Debug, Clone)]
pub struct FileEntry {
    /// Repo-relative path (used both for display and as the git pathspec).
    pub path: String,
    pub kind: ChangeKind,
    /// Worktree mtime, used for the "sort by modified date" mode. Deleted files
    /// (no worktree entry) fall back to the UNIX epoch.
    pub mtime: SystemTime,
    /// Added / removed line counts (`None` for binary files or when unknown).
    pub added: Option<u32>,
    pub removed: Option<u32>,
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

        entries.push(FileEntry {
            path,
            kind,
            mtime,
            added: None,
            removed: None,
        });

        // Skip the original-path token that follows a rename/copy record.
        i += if is_rename { 2 } else { 1 };
    }

    fill_stats(root, &diff_base(root), &mut entries);
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

/// Added/removed line counts per path, from `git diff --numstat <base>`.
/// Binary files (numstat `-`) are omitted.
fn numstat(root: &Path, base: &str) -> HashMap<String, (u32, u32)> {
    let mut map = HashMap::new();
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--numstat", base])
        .output()
    else {
        return map;
    };
    if !out.status.success() {
        return map;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut it = line.splitn(3, '\t');
        let (Some(a), Some(d), Some(path)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        // Renames appear as "old => new"; index by the new path we display.
        let path = path
            .rsplit(" => ")
            .next()
            .unwrap_or(path)
            .trim_end_matches('}');
        if let (Ok(a), Ok(d)) = (a.parse::<u32>(), d.parse::<u32>()) {
            map.insert(path.to_string(), (a, d));
        }
    }
    map
}

/// Fill each entry's added/removed counts. Tracked files come from `numstat`;
/// untracked files count their whole contents as additions.
fn fill_stats(root: &Path, base: &str, entries: &mut [FileEntry]) {
    let stats = numstat(root, base);
    for e in entries.iter_mut() {
        if e.kind == ChangeKind::Untracked {
            let lines = std::fs::read_to_string(root.join(&e.path))
                .map(|s| s.lines().count() as u32)
                .ok();
            e.added = lines;
            e.removed = Some(0);
        } else if let Some(&(a, d)) = stats.get(&e.path) {
            e.added = Some(a);
            e.removed = Some(d);
        }
    }
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

/// How many commits `HEAD` is behind `base` (commits reachable from `base` but
/// not from `HEAD`). `None` when the count can't be determined.
pub fn behind_count(root: &Path, base: &str) -> Option<u64> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-list", "--count", &format!("HEAD..{base}")])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
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
        entries.push(FileEntry {
            path,
            kind,
            mtime,
            added: None,
            removed: None,
        });
        i += step;
    }

    entries.extend(untracked_files(root)?);
    fill_stats(root, base, &mut entries);
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
                added: None,
                removed: None,
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

/// One commit in a file's history, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    /// Full commit id.
    pub hash: String,
    /// Abbreviated commit id, as `git log --format=%h` produced it.
    pub short_hash: String,
    /// Commit subject line.
    pub subject: String,
    /// The path the file had at that commit (differs across renames).
    pub path_at_commit: String,
    /// True when the commit is not one of the current branch's own commits.
    pub pre_branch: bool,
}

/// Parse `git log --follow --format=%x01%H%x1f%h%x1f%s --name-only` output
/// into `(hash, short_hash, subject, path_at_commit)` tuples. Records start
/// at `\x01`; the head line holds hash/short/subject separated by `\x1f`, and
/// the first non-empty line after it is the file's name at that commit.
fn parse_file_history(out: &str) -> Vec<(String, String, String, String)> {
    let mut entries = Vec::new();
    for record in out.split('\u{1}').skip(1) {
        let mut lines = record.lines();
        let Some(head) = lines.next() else { continue };
        let mut parts = head.splitn(3, '\u{1f}');
        let (Some(hash), Some(short), Some(subject)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let Some(path) = lines.find(|l| !l.is_empty()) else {
            continue;
        };
        entries.push((
            hash.to_string(),
            short.to_string(),
            subject.to_string(),
            path.to_string(),
        ));
    }
    entries
}

/// Commits that belong to the current branch: reachable from `HEAD` but not
/// from the merge base with the base branch. Empty when there is no base
/// branch, no merge base, or `HEAD` *is* the merge base (sitting on the base
/// branch itself) — callers treat "empty" as "suppress the pre-branch pill",
/// since flagging every commit would be noise, not signal.
pub fn branch_commits(root: &Path) -> HashSet<String> {
    let Some(bb) = base_branch(root) else {
        return HashSet::new();
    };
    let Some(mb) = merge_base(root, &bb) else {
        return HashSet::new();
    };
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-list", &format!("{mb}..HEAD")])
        .output()
    else {
        return HashSet::new();
    };
    if !out.status.success() {
        return HashSet::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// The commits that touched `path`, newest first, following renames.
/// `branch` is [`branch_commits`]' output; when empty, no entry is flagged
/// `pre_branch`.
pub fn file_history(
    root: &Path,
    path: &str,
    branch: &HashSet<String>,
) -> Result<Vec<HistoryEntry>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        // quotepath=off keeps non-ASCII paths literal instead of escaped.
        .args([
            "-c",
            "core.quotepath=off",
            "log",
            "--follow",
            "--format=%x01%H%x1f%h%x1f%s",
            "--name-only",
            "--",
            path,
        ])
        .output()
        .context("failed to run `git log --follow`")?;
    if !out.status.success() {
        bail!(
            "`git log --follow` failed for {path}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let suppress = branch.is_empty();
    Ok(parse_file_history(&String::from_utf8_lossy(&out.stdout))
        .into_iter()
        .map(|(hash, short_hash, subject, path_at_commit)| HistoryEntry {
            pre_branch: !suppress && !branch.contains(&hash),
            hash,
            short_hash,
            subject,
            path_at_commit,
        })
        .collect())
}

/// The raw, ANSI-colored diff a single commit made to `path` (the file's name
/// at that commit). Root commits have no parent and diff against the empty
/// tree, so the file renders as fully added.
pub fn commit_diff(root: &Path, commit: &str, path: &str) -> Result<Vec<u8>> {
    let parent = format!("{commit}^");
    let has_parent = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "--quiet", &parent])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let base = if has_parent {
        parent
    } else {
        EMPTY_TREE.to_string()
    };
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--color=always", &base, commit, "--"])
        .arg(path)
        .output()
        .context("failed to run `git diff` for a history commit")?;
    if out.stdout.is_empty() && !out.stderr.is_empty() {
        bail!(
            "git diff failed for {path} at {commit}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_file_history_handles_multiple_records_and_rename_paths() {
        // Two commits; the older one has a different (pre-rename) path and a
        // subject that itself contains the field separator — splitn(3) must
        // keep it intact.
        let out = "\u{1}aaa\u{1f}a1\u{1f}newest\n\nnew.rs\n\n\u{1}bbb\u{1f}b1\u{1f}has \u{1f} inside\n\nold.rs\n";
        let parsed = parse_file_history(out);
        assert_eq!(
            parsed,
            vec![
                (
                    "aaa".to_string(),
                    "a1".to_string(),
                    "newest".to_string(),
                    "new.rs".to_string()
                ),
                (
                    "bbb".to_string(),
                    "b1".to_string(),
                    "has \u{1f} inside".to_string(),
                    "old.rs".to_string()
                ),
            ]
        );
    }

    #[test]
    fn parse_file_history_skips_malformed_records_and_empty_output() {
        assert!(parse_file_history("").is_empty());
        // A record with no path line (e.g. history simplification artifacts)
        // is dropped rather than panicking.
        assert!(parse_file_history("\u{1}aaa\u{1f}a1\u{1f}subject\n\n").is_empty());
        // A head line missing the separators is dropped.
        assert!(parse_file_history("\u{1}garbage\n\npath.rs\n").is_empty());
    }

    /// Run git in `dir`, isolated from the user's global/system config so
    /// options like commit signing can't break the scratch repo.
    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn scratch_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        git(p, &["init", "-b", "main"]);
        git(p, &["config", "user.name", "t"]);
        git(p, &["config", "user.email", "t@t"]);
        git(p, &["config", "commit.gpgsign", "false"]);
        dir
    }

    fn commit_file(dir: &Path, path: &str, contents: &str, msg: &str) {
        std::fs::write(dir.join(path), contents).expect("write");
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-m", msg]);
    }

    #[test]
    fn file_history_follows_renames_and_orders_newest_first() {
        let repo = scratch_repo();
        let p = repo.path();
        commit_file(p, "a.txt", "one\n", "add a");
        commit_file(p, "a.txt", "one\ntwo\n", "grow a");
        git(p, &["mv", "a.txt", "b.txt"]);
        git(p, &["commit", "-m", "rename a to b"]);

        let hist = file_history(p, "b.txt", &HashSet::new()).unwrap();
        let subjects: Vec<&str> = hist.iter().map(|h| h.subject.as_str()).collect();
        assert_eq!(subjects, ["rename a to b", "grow a", "add a"]);
        // Pre-rename commits must report the file's old name, so their diffs
        // can be produced with the pathspec git actually knows them by.
        let paths: Vec<&str> = hist.iter().map(|h| h.path_at_commit.as_str()).collect();
        assert_eq!(paths, ["b.txt", "a.txt", "a.txt"]);
        // Empty branch set = pill suppressed everywhere.
        assert!(hist.iter().all(|h| !h.pre_branch));
    }

    #[test]
    fn commit_diff_renders_root_commit_as_fully_added() {
        let repo = scratch_repo();
        let p = repo.path();
        commit_file(p, "a.txt", "hello\n", "add a");
        let hist = file_history(p, "a.txt", &HashSet::new()).unwrap();
        let raw = commit_diff(p, &hist[0].hash, &hist[0].path_at_commit).unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(
            text.contains("hello"),
            "root-commit diff via EMPTY_TREE shows the file's lines: {text}"
        );
    }

    #[test]
    fn commit_diff_across_a_rename_uses_the_old_path() {
        let repo = scratch_repo();
        let p = repo.path();
        commit_file(p, "a.txt", "one\n", "add a");
        commit_file(p, "a.txt", "one\ntwo\n", "grow a");
        git(p, &["mv", "a.txt", "b.txt"]);
        git(p, &["commit", "-m", "rename a to b"]);

        let hist = file_history(p, "b.txt", &HashSet::new()).unwrap();
        // "grow a" happened while the file was still a.txt.
        let raw = commit_diff(p, &hist[1].hash, &hist[1].path_at_commit).unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(
            text.contains("two"),
            "pre-rename commit diff is non-empty: {text}"
        );
    }

    #[test]
    fn branch_commits_flags_only_branch_work_and_is_empty_on_the_base_branch() {
        let repo = scratch_repo();
        let p = repo.path();
        commit_file(p, "a.txt", "one\n", "on main");
        // Sitting on main itself: merge base == HEAD → empty set → pill off.
        assert!(branch_commits(p).is_empty());

        git(p, &["checkout", "-b", "feature"]);
        commit_file(p, "a.txt", "one\ntwo\n", "on feature");
        let branch = branch_commits(p);
        assert_eq!(branch.len(), 1);

        let hist = file_history(p, "a.txt", &branch).unwrap();
        assert_eq!(hist.len(), 2);
        assert!(!hist[0].pre_branch); // "on feature" is the branch's own work
        assert!(hist[1].pre_branch); // "on main" predates the branch
    }
}
