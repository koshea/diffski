//! Application state and behavior.
//!
//! The diff pane is a single continuous view of *all* changed files' diffs,
//! concatenated in the current sort order. The left-hand file list is a table
//! of contents: selecting a file jumps the scroll to that file's section, and
//! as you page through the combined diff the selection tracks whichever file is
//! at the top of the viewport.

use crate::config::Config;
use crate::delta::{self, DiffCache};
use crate::git::{self, ChangeKind, FileEntry, KIND_ORDER};
use crate::search;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::ListState;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Per-file commit-history browsing state. `Some` overrides the diff pane
/// with a single historical commit's diff; `None` is the normal live view.
/// All live state (`combined`, `diff_scroll`, filters) stays intact
/// underneath, so every exit path is just dropping this struct.
pub struct HistoryView {
    /// Live changeset path this history belongs to; the view exits when the
    /// selection moves off it.
    pub path: String,
    /// Commits that touched the file, newest first.
    pub entries: Vec<git::HistoryEntry>,
    /// Index into `entries` currently shown (0 = newest commit).
    pub pos: usize,
    /// Rendered diff for `entries[pos]`, wrapped at `built_width`.
    pub text: Text<'static>,
    /// Vertical scroll offset within `text`.
    pub scroll: u16,
    /// Width `text` was rendered at; a mismatch forces a re-render.
    built_width: u16,
    /// Rendered diffs per commit hash — commits are immutable, so entries
    /// only clear on theme change or resize (rendering is width-dependent).
    cache: HashMap<String, Text<'static>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortField {
    /// Alphabetical by path (a tree-ish grouping).
    Tree,
    /// By worktree modification time.
    Modified,
    /// By total changed lines (added + removed).
    Size,
}

impl SortField {
    pub fn label(self) -> &'static str {
        match self {
            SortField::Tree => "tree",
            SortField::Modified => "modified",
            SortField::Size => "size",
        }
    }
    pub fn as_key(self) -> &'static str {
        match self {
            SortField::Tree => "tree",
            SortField::Modified => "modified",
            SortField::Size => "size",
        }
    }
    /// The file-list header's right-aligned sort text, e.g. `"tree ▼"` —
    /// shared by rendering (`ui::draw_file_list`) and click hit-testing
    /// (`App::click_sort_indicator`) so the two can never disagree on width.
    pub fn indicator(self, desc: bool) -> String {
        format!("{} {}", self.label(), if desc { "▼" } else { "▲" })
    }
    pub fn from_key(s: &str) -> Option<Self> {
        match s {
            "tree" => Some(SortField::Tree),
            "modified" => Some(SortField::Modified),
            "size" => Some(SortField::Size),
            _ => None,
        }
    }
}

/// What the diff is taken against.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DiffMode {
    /// Working-tree changes since the last commit (`HEAD`), plus untracked files.
    Working,
    /// Everything on this branch versus its base branch (the merge base).
    BaseBranch,
}

impl DiffMode {
    pub fn as_key(self) -> &'static str {
        match self {
            DiffMode::Working => "working",
            DiffMode::BaseBranch => "base",
        }
    }
    pub fn from_key(s: &str) -> Option<Self> {
        match s {
            "working" => Some(DiffMode::Working),
            "base" => Some(DiffMode::BaseBranch),
            _ => None,
        }
    }
}

/// Curated set of delta/bat syntax themes cycled by the theme hotkey. These ship
/// with delta, so they're always available.
const THEMES: &[&str] = &[
    "Dracula",
    "Monokai Extended",
    "Nord",
    "gruvbox-dark",
    "gruvbox-light",
    "Solarized (dark)",
    "Solarized (light)",
    "OneHalfDark",
    "OneHalfLight",
    "TwoDark",
    "Coldark-Dark",
    "Coldark-Cold",
    "Sublime Snazzy",
    "zenburn",
    "ansi",
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Search,
    /// Toggling change-kind filters (entered with `c`).
    KindFilter,
}

/// A text selection in the combined diff, in `(line, column)` coordinates where
/// `line` indexes `combined.lines` and `column` is a char offset within it.
#[derive(Clone, Copy)]
pub struct Selection {
    pub anchor: (usize, usize),
    pub cursor: (usize, usize),
}

impl Selection {
    /// `(start, end)` ordered so start <= end.
    pub fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }
    pub fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }
}

/// A content-search hit located in the combined diff.
pub struct AppMatch {
    /// Row in `combined` (post-wrap).
    pub row: usize,
    /// Char-column range within that row.
    pub cols: (usize, usize),
    /// Index into `filtered` of the owning file.
    pub file_pos: usize,
    /// Whether the hit was precisely located (false = navigate to the file's
    /// section top, no highlight).
    pub aligned: bool,
}

/// Which clickable element (if any) is currently under the mouse cursor.
/// Resolved fresh from current state on every read by `App::hover_target` —
/// never cached at move-time — so it can't go stale if the underlying data
/// (filtered list, file types, scroll offset) changes without a further
/// mouse move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hover {
    KindChip(ChangeKind),
    FileType(String),
    SortLabel,
    SortArrow,
    /// Index into `App::filtered`.
    FileRow(usize),
}

pub struct App {
    pub root: PathBuf,
    /// Diff base: `HEAD`/empty tree in working mode, or the merge base in
    /// base-branch mode.
    base: String,
    /// delta syntax-theme override; `None` means "use gitconfig".
    theme: Option<String>,
    /// Spaces a tab expands to in the diff.
    tab_width: u16,
    /// What the diff is taken against.
    pub diff_mode: DiffMode,
    /// Human-readable label for the current base (e.g. "HEAD" or "origin/main").
    pub base_label: String,

    /// All changed files, in current sort order.
    pub files: Vec<FileEntry>,
    /// Indices into `files` matching the active search (all of them if none).
    pub filtered: Vec<usize>,
    /// Index into `filtered` of the file at the top of the diff viewport.
    pub selected: usize,

    pub sort_field: SortField,
    pub sort_desc: bool,

    pub mode: Mode,
    pub search: String,
    /// Change kinds to show; empty = no filter (show all).
    pub kind_filter: HashSet<ChangeKind>,
    /// File extensions to show (lowercase, no dot); empty = no filter.
    pub file_type_filter: HashSet<String>,
    /// Distinct extensions among files passing every other active filter —
    /// the chip list. Recomputed on every `rebuild_filter`.
    pub file_types: Vec<String>,
    /// Scope for content search: both, added-only, or deleted-only lines.
    pub search_scope: search::Scope,
    /// Content-search hits in the current combined diff, in order.
    pub matches: Vec<AppMatch>,
    /// Index into `matches` of the current n/N target.
    pub current_match: usize,
    /// One-shot: center the current match after the next matches rebuild
    /// (set on content-query edits for the live incsearch-style jump).
    pending_match_jump: bool,

    cache: DiffCache,
    /// The concatenated diff of every file in `filtered`, in order.
    pub combined: Text<'static>,
    /// Start line of each `filtered` file within `combined` (parallel to it).
    offsets: Vec<usize>,

    /// Vertical scroll offset within the combined diff.
    pub diff_scroll: u16,
    /// Inner dimensions of the diff pane, updated each draw.
    pub diff_width: u16,
    pub diff_viewport_height: u16,

    /// Set when the combined diff must be rebuilt (data/sort/filter changed).
    needs_rebuild: bool,
    /// Width the combined diff was last built at (rebuild on resize).
    built_width: u16,
    /// After a rebuild, restore the viewport onto this `(path, intra-file offset)`.
    restore_anchor: Option<(String, u16)>,

    // --- layout & mouse (updated during draw / by mouse events) ------------
    /// Left-pane width as a percentage of the terminal.
    pub split_pct: u16,
    /// Hide the file-list sidebar entirely.
    pub sidebar_collapsed: bool,
    /// Pane rectangles from the last render, for mouse hit-testing.
    pub area_main: Rect,
    pub area_left: Rect,
    pub area_right: Rect,
    /// File-list scroll state (kept across frames for click mapping).
    pub list_state: ListState,
    dragging_divider: bool,
    /// Raw `(col, row)` of the last `MouseEventKind::Moved` event, or `None`
    /// if the mouse hasn't moved yet (or moved off-screen). What it resolves
    /// to is computed on demand by `hover_target`, not cached here.
    hover_pos: Option<(u16, u16)>,

    /// Active text selection in the diff pane, if any.
    pub selection: Option<Selection>,
    selecting: bool,
    /// Auto-scroll direction while dragging a selection past an edge:
    /// -1 up, +1 down, 0 none.
    sel_autoscroll: i32,
    /// Text queued to be copied to the clipboard by the event loop.
    pub pending_copy: Option<String>,

    /// Follow-latest: jump to files as they change on disk.
    pub follow: bool,
    /// Persisted auto-update preference (carried through for `save_config`).
    auto_update: bool,
    /// Set when a background update has been installed (shown in the footer).
    pub update_ready: bool,
    /// Paths changed on disk that the user hasn't looked at yet.
    pub recently_changed: HashSet<String>,
    /// Commits the base branch has that `HEAD` doesn't; `None` hides the cue.
    pub behind: Option<u64>,
    /// Set when the behind count should be recomputed (startup, mode toggle,
    /// fs change). The event loop owns the actual scheduling.
    pub behind_dirty: bool,
    /// Last-seen mtime per path, to tell real content changes from the
    /// filesystem noise our own reads generate (atime/attrib events).
    last_mtimes: HashMap<String, SystemTime>,

    // --- marquee (scrolling the selected file's long name) -----------------
    /// Set by the renderer when the selected file's name overflows its column.
    pub active_path_overflow: bool,
    pub marquee_offset: usize,
    /// Selection the marquee is tracking, to reset the scroll on change.
    pub marquee_sel: usize,
    marquee_last: Option<Instant>,

    pub show_help: bool,
    /// Per-file commit-history overlay; `None` is the normal live view.
    pub history: Option<HistoryView>,
    pub status: String,
    /// When the current status message was set (drives auto-expiry).
    status_since: Option<Instant>,
    pub should_quit: bool,
}

impl App {
    pub fn new(root: PathBuf, config: Config) -> Self {
        let mut app = App {
            root,
            base: String::new(),
            theme: config.theme,
            tab_width: config.tab_width,
            diff_mode: config.diff_mode,
            base_label: String::new(),
            files: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            sort_field: config.sort_field,
            sort_desc: config.sort_desc,
            mode: Mode::Normal,
            search: String::new(),
            kind_filter: HashSet::new(),
            file_type_filter: HashSet::new(),
            file_types: Vec::new(),
            search_scope: search::Scope::Both,
            matches: Vec::new(),
            current_match: 0,
            pending_match_jump: false,
            cache: DiffCache::new(),
            combined: Text::default(),
            offsets: Vec::new(),
            diff_scroll: 0,
            diff_width: 80,
            diff_viewport_height: 20,
            needs_rebuild: true,
            built_width: 0,
            restore_anchor: None,
            split_pct: config.split_pct.clamp(15, 85),
            sidebar_collapsed: config.sidebar_collapsed,
            area_main: Rect::default(),
            area_left: Rect::default(),
            area_right: Rect::default(),
            list_state: ListState::default(),
            dragging_divider: false,
            hover_pos: None,
            selection: None,
            selecting: false,
            sel_autoscroll: 0,
            pending_copy: None,
            follow: config.follow,
            auto_update: config.auto_update,
            update_ready: false,
            recently_changed: HashSet::new(),
            behind: None,
            behind_dirty: true,
            last_mtimes: HashMap::new(),
            active_path_overflow: false,
            marquee_offset: 0,
            marquee_sel: 0,
            marquee_last: None,
            show_help: false,
            history: None,
            status: String::new(),
            status_since: None,
            should_quit: false,
        };
        app.refresh();
        // Prime mtimes from the initial changeset so nothing is flagged as
        // "recently changed" at startup.
        app.last_mtimes = app
            .files
            .iter()
            .map(|f| (f.path.clone(), f.mtime))
            .collect();
        app
    }

    // --- queries -----------------------------------------------------------

    pub fn current_entry(&self) -> Option<&FileEntry> {
        self.filtered
            .get(self.selected)
            .and_then(|&i| self.files.get(i))
    }

    /// The `combined` line range the pane may show: the selected file's
    /// section while follow (focus) is on, the whole diff otherwise.
    pub fn view_window(&self) -> (usize, usize) {
        let total = self.combined.lines.len();
        if !self.follow || self.offsets.is_empty() {
            return (0, total);
        }
        let start = self.offsets.get(self.selected).copied().unwrap_or(0);
        let end = self
            .offsets
            .get(self.selected + 1)
            .copied()
            .unwrap_or(total);
        (start, end)
    }

    /// Clamp `diff_scroll` into the active view window — in focus mode the
    /// window's start is a lower bound, not just its end.
    fn clamp_scroll_to_window(&mut self) {
        let (wstart, _) = self.view_window();
        self.diff_scroll = self.diff_scroll.clamp(wstart as u16, self.max_scroll());
    }

    /// Entering focus mode: snap the viewport to the selected file's start
    /// if it is currently outside that file's section; keep it otherwise.
    fn snap_into_focus(&mut self) {
        let (wstart, wend) = self.view_window();
        let s = self.diff_scroll as usize;
        if s < wstart || s >= wend {
            self.diff_scroll = wstart as u16;
        }
        self.clamp_scroll_to_window();
    }

    fn max_scroll(&self) -> u16 {
        if self.follow {
            // Focus view: stop when the file's last line reaches the bottom;
            // a window shorter than the viewport pins to the file's start.
            let (wstart, wend) = self.view_window();
            let len = (wend - wstart) as u16;
            return wstart as u16 + len.saturating_sub(self.diff_viewport_height);
        }
        let total = self.combined.lines.len() as u16;
        // Everything fits: no scrolling.
        if total <= self.diff_viewport_height {
            return 0;
        }
        // Normally stop when the last line reaches the bottom of the viewport.
        // But also allow scrolling far enough that the *last file* can sit at the
        // top — otherwise a short final file could never be navigated to.
        let fit = total - self.diff_viewport_height;
        let last_file = self.offsets.last().copied().unwrap_or(0) as u16;
        fit.max(last_file).min(total - 1)
    }

    /// The current viewport position as `(path, offset within that file)`, so it
    /// can be restored after the combined diff is rebuilt.
    fn current_anchor(&self) -> Option<(String, u16)> {
        let &i = self.filtered.get(self.selected)?;
        let base = self.offsets.get(self.selected).copied().unwrap_or(0) as u16;
        Some((
            self.files[i].path.clone(),
            self.diff_scroll.saturating_sub(base),
        ))
    }

    // --- data refresh ------------------------------------------------------

    /// Reload the changed-file list from git, preserving the viewport position
    /// and marking the combined diff for rebuild.
    pub fn refresh(&mut self) {
        self.restore_anchor = self.current_anchor();

        // Resolve the diff base and file list according to the current mode.
        let files = match self.diff_mode {
            DiffMode::Working => {
                self.base = git::diff_base(&self.root);
                self.base_label = "HEAD".to_string();
                git::changed_files(&self.root)
            }
            DiffMode::BaseBranch => match git::base_branch(&self.root)
                .and_then(|bb| git::merge_base(&self.root, &bb).map(|mb| (bb, mb)))
            {
                Some((branch, merge_base)) => {
                    self.base = merge_base;
                    self.base_label = branch;
                    git::branch_changes(&self.root, &self.base)
                }
                None => {
                    // No base branch (no remote / single branch): fall back.
                    self.set_status("no base branch found — showing working changes");
                    self.diff_mode = DiffMode::Working;
                    self.base = git::diff_base(&self.root);
                    self.base_label = "HEAD".to_string();
                    git::changed_files(&self.root)
                }
            },
        };

        match files {
            Ok(files) => self.files = files,
            Err(e) => {
                self.set_status(format!("git error: {e}"));
                return;
            }
        }
        self.sort_files();
        self.rebuild_filter();
        self.needs_rebuild = true;
    }

    /// Persist the current sort/theme/mode so the next run matches.
    fn save_config(&self) {
        Config {
            sort_field: self.sort_field,
            sort_desc: self.sort_desc,
            theme: self.theme.clone(),
            diff_mode: self.diff_mode,
            split_pct: self.split_pct,
            follow: self.follow,
            auto_update: self.auto_update,
            tab_width: self.tab_width,
            sidebar_collapsed: self.sidebar_collapsed,
        }
        .save();
    }

    /// Note that a background update has been installed.
    pub fn set_update_ready(&mut self, msg: String) {
        self.update_ready = true;
        self.set_status(msg);
    }

    /// Show a transient status message in the footer (auto-expires).
    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_since = Some(Instant::now());
    }

    /// Clear the status once it has been shown long enough. Returns whether
    /// it cleared, so the event loop knows to redraw.
    pub fn status_tick(&mut self) -> bool {
        if self.status.is_empty() {
            return false;
        }
        let expired = self
            .status_since
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(4));
        if expired {
            self.status.clear();
            self.status_since = None;
        }
        expired
    }

    /// Restart the marquee scroll (called when the selection changes).
    pub fn marquee_reset(&mut self) {
        self.marquee_offset = 0;
        self.marquee_last = None;
    }

    /// Advance the marquee on a fixed timer. Returns whether it moved (so the
    /// event loop knows to redraw). Only meaningful while a long name is selected.
    pub fn marquee_step(&mut self) -> bool {
        let now = Instant::now();
        let due = self
            .marquee_last
            .is_none_or(|t| now.duration_since(t) >= Duration::from_millis(220));
        if due {
            self.marquee_offset = self.marquee_offset.wrapping_add(1);
            self.marquee_last = Some(now);
            true
        } else {
            false
        }
    }

    /// React to a change signal: reload, then figure out which files really
    /// changed (by mtime) for the recently-changed cue and follow-latest.
    pub fn on_fs_change(&mut self) {
        self.behind_dirty = true;
        self.refresh();

        // Detect genuine content changes via mtime, and refresh the baseline.
        let changed: Vec<String> = self
            .files
            .iter()
            .filter(|f| self.last_mtimes.get(&f.path).is_none_or(|&t| f.mtime > t))
            .map(|f| f.path.clone())
            .collect();
        self.last_mtimes = self
            .files
            .iter()
            .map(|f| (f.path.clone(), f.mtime))
            .collect();

        for p in &changed {
            self.cache.invalidate_path(p);
            self.recently_changed.insert(p.clone());
        }
        // Forget cues for paths no longer in the changeset.
        self.recently_changed
            .retain(|p| self.files.iter().any(|f| &f.path == p));

        // Follow-latest: jump to the most recently modified changed file.
        if self.follow
            && let Some(target) = self
                .files
                .iter()
                .filter(|f| changed.contains(&f.path))
                .max_by_key(|f| f.mtime)
                .map(|f| f.path.clone())
        {
            self.restore_anchor = Some((target, 0));
        }
    }

    /// Toggle follow-latest mode (a single-file focus view while on).
    pub fn toggle_follow(&mut self) {
        self.follow = !self.follow;
        if self.follow {
            self.snap_into_focus();
        }
        self.set_status(if self.follow {
            "follow: on"
        } else {
            "follow: off"
        });
        self.save_config();
    }

    /// Collapse or expand the file-list sidebar.
    pub fn toggle_sidebar(&mut self) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        self.save_config();
    }

    /// Mark the file at `path` as viewed, clearing its recently-changed cue.
    pub fn mark_viewed(&mut self, path: &str) {
        self.recently_changed.remove(path);
    }

    // --- building the combined diff ----------------------------------------

    /// Ensure `combined`/`offsets` reflect the current file set at `diff_width`.
    /// Cheap when nothing changed. Per-file diffs come from the cache, so a
    /// rebuild is mostly just concatenation.
    /// Whether [`Self::ensure_combined`] has work to do (data/sort/filter
    /// changed, or the pane was resized). Lets the event loop paint first, then
    /// build.
    pub fn needs_build(&self) -> bool {
        self.needs_rebuild || self.built_width != self.diff_width
    }

    pub fn ensure_combined(&mut self) {
        if !self.needs_rebuild && self.built_width == self.diff_width {
            return;
        }
        let width = self.diff_width;
        let root = self.root.clone();
        let base = self.base.clone();
        let entries: Vec<FileEntry> = self
            .filtered
            .iter()
            .map(|&i| self.files[i].clone())
            .collect();

        // Pre-warm the cache for cold files concurrently. This is where startup
        // time goes: each cold file needs a `git diff` + `delta` subprocess.
        // Rendering them in parallel cuts wall-clock to ~(serial / cores).
        self.prewarm(&entries, width, &root, &base);

        let (needle, _) = search::split_query(&self.search);
        let mut matches: Vec<AppMatch> = Vec::new();

        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut offsets: Vec<usize> = Vec::with_capacity(entries.len());
        for (file_pos, entry) in entries.iter().enumerate() {
            offsets.push(lines.len());
            let file_start = lines.len();
            let sig = signature(entry);
            let root = root.clone();
            let base = base.clone();
            let e = entry.clone();
            match self.cache.get_or_render(
                &entry.path,
                sig,
                width,
                self.theme.as_deref(),
                self.tab_width,
                move || git::raw_diff(&root, &e, &base),
            ) {
                Ok(text) if text.lines.is_empty() => lines.push(Line::from("  (no textual diff)")),
                Ok(text) => {
                    // delta doesn't wrap long lines when piped, so wrap here —
                    // keeping one row per line so scroll offsets stay exact.
                    // Record each pre-wrap line's starting row for match mapping.
                    let mut row_of: Vec<usize> = Vec::with_capacity(text.lines.len());
                    for line in &text.lines {
                        row_of.push(lines.len());
                        wrap_line_into(line, width as usize, &mut lines);
                    }
                    if let Some(needle) = needle.as_deref()
                        && let Some(raw) = self.cache.raw_text(&entry.path, sig)
                    {
                        let rendered: Vec<String> = text
                            .lines
                            .iter()
                            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                            .collect();
                        let w = (width as usize).max(1);
                        for m in search::locate_matches(
                            raw,
                            &rendered,
                            needle,
                            self.search_scope,
                            self.tab_width,
                        ) {
                            if m.aligned {
                                // Hard wrap at exactly `w` chars makes the
                                // post-wrap mapping pure arithmetic; a match
                                // crossing a wrap boundary clamps to its row.
                                let row = row_of[m.line] + m.cols.0 / w;
                                let cs = m.cols.0 % w;
                                let ce = (cs + (m.cols.1 - m.cols.0)).min(w);
                                matches.push(AppMatch {
                                    row,
                                    cols: (cs, ce),
                                    file_pos,
                                    aligned: true,
                                });
                            } else {
                                matches.push(AppMatch {
                                    row: file_start,
                                    cols: (0, 0),
                                    file_pos,
                                    aligned: false,
                                });
                            }
                        }
                    }
                }
                Err(err) => {
                    lines.push(Line::from(format!(
                        "  error rendering {}: {err}",
                        entry.path
                    )));
                    self.set_status(err.to_string());
                }
            }
        }

        self.combined = Text::from(lines);
        self.offsets = offsets;
        self.matches = matches;
        self.current_match = self.current_match.min(self.matches.len().saturating_sub(1));
        self.built_width = width;
        self.needs_rebuild = false;

        // Restore the viewport onto the previously anchored file, if still present.
        if let Some((path, intra)) = self.restore_anchor.take()
            && let Some(pos) = self
                .filtered
                .iter()
                .position(|&i| self.files[i].path == path)
        {
            self.selected = pos;
            self.diff_scroll = (self.offsets[pos] as u16).saturating_add(intra);
        }
        self.clamp_scroll_to_window();
        self.update_selected_from_scroll();

        // Live incsearch jump: land on the first match after a query edit.
        if self.pending_match_jump {
            self.pending_match_jump = false;
            if !self.matches.is_empty() {
                self.center_current_match();
            }
        }

        self.sync_history();
    }

    /// Render any not-yet-cached files concurrently and store them, so the
    /// subsequent concatenation is all cache hits. This is the main startup and
    /// live-update cost, so it runs across a small worker pool.
    fn prewarm(&mut self, entries: &[FileEntry], width: u16, root: &Path, base: &str) {
        let cold: Vec<&FileEntry> = entries
            .iter()
            .filter(|e| !self.cache.contains(&e.path, signature(e), width))
            .collect();
        if cold.is_empty() {
            return;
        }

        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(cold.len())
            .min(16);

        let theme = self.theme.clone();
        let tabs = self.tab_width;
        let next = AtomicUsize::new(0);
        type Rendered = (
            String,
            u64,
            std::result::Result<Text<'static>, String>,
            Option<String>,
        );
        let out: Mutex<Vec<Rendered>> = Mutex::new(Vec::new());

        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(entry) = cold.get(i) else { break };
                        let sig = signature(entry);
                        let (rendered, stripped) = match git::raw_diff(root, entry, base) {
                            Ok(raw) => {
                                let stripped =
                                    crate::search::strip_ansi(&String::from_utf8_lossy(&raw));
                                let text = delta::render(&raw, width, theme.as_deref(), tabs)
                                    .map_err(|e| e.to_string());
                                (text, Some(stripped))
                            }
                            Err(e) => (Err(e.to_string()), None),
                        };
                        out.lock()
                            .unwrap()
                            .push((entry.path.clone(), sig, rendered, stripped));
                    }
                });
            }
        });

        for (path, sig, res, stripped) in out.into_inner().unwrap() {
            if let Some(stripped) = stripped {
                self.cache.insert_raw(&path, sig, stripped);
            }
            match res {
                Ok(text) => self.cache.insert(&path, sig, width, text),
                Err(e) => self.set_status(e),
            }
        }
    }

    /// Index of the file whose section contains combined-diff line `line`.
    fn file_at_line(&self, line: usize) -> usize {
        let mut sel = 0;
        for (idx, &off) in self.offsets.iter().enumerate() {
            if off <= line {
                sel = idx;
            } else {
                break;
            }
        }
        sel
    }

    /// Set `selected` to whichever file's section contains the top of the viewport.
    fn update_selected_from_scroll(&mut self) {
        if self.offsets.is_empty() {
            self.selected = 0;
            return;
        }
        // Focus view: the window is derived FROM the selection; deriving the
        // selection back from the scroll would be circular and can misfire
        // while the scroll is being clamped into a newly selected window.
        if self.follow {
            return;
        }
        self.selected = self.file_at_line(self.diff_scroll as usize);
    }

    fn scroll_to_selected(&mut self) {
        if let Some(&off) = self.offsets.get(self.selected) {
            self.diff_scroll = (off as u16).min(self.max_scroll());
        }
    }

    // --- navigation & scrolling --------------------------------------------

    /// Jump the viewport to the next file in the table of contents.
    pub fn select_next(&mut self) {
        if self.selected + 1 < self.filtered.len() {
            self.history = None;
            self.selected += 1;
            self.scroll_to_selected();
        }
    }

    /// Jump the viewport to the previous file in the table of contents.
    pub fn select_prev(&mut self) {
        if self.selected > 0 {
            self.history = None;
            self.selected -= 1;
            self.scroll_to_selected();
        }
    }

    pub fn scroll_down(&mut self, amount: u16) {
        if let Some(h) = self.history.as_mut() {
            let max = history_max_scroll(h.text.lines.len(), self.diff_viewport_height);
            h.scroll = (h.scroll + amount).min(max);
            return;
        }
        // No window floor needed on downward moves: `max_scroll` already
        // carries the focus window's upper bound, and the scroll can only
        // start inside the window.
        self.diff_scroll = (self.diff_scroll + amount).min(self.max_scroll());
        self.update_selected_from_scroll();
    }

    pub fn scroll_up(&mut self, amount: u16) {
        if let Some(h) = self.history.as_mut() {
            h.scroll = h.scroll.saturating_sub(amount);
            return;
        }
        self.diff_scroll = self.diff_scroll.saturating_sub(amount);
        self.clamp_scroll_to_window();
        self.update_selected_from_scroll();
    }

    pub fn scroll_home(&mut self) {
        if let Some(h) = self.history.as_mut() {
            h.scroll = 0;
            return;
        }
        let (wstart, _) = self.view_window();
        self.diff_scroll = wstart as u16;
        self.update_selected_from_scroll();
    }

    pub fn scroll_end(&mut self) {
        if let Some(h) = self.history.as_mut() {
            h.scroll = history_max_scroll(h.text.lines.len(), self.diff_viewport_height);
            return;
        }
        self.diff_scroll = self.max_scroll();
        self.update_selected_from_scroll();
    }

    fn page(&self) -> u16 {
        // Scroll by nearly a full viewport, keeping one line of context.
        self.diff_viewport_height.saturating_sub(1).max(1)
    }

    /// Jump to the next (`dir` = 1) or previous (`dir` = -1) content match,
    /// wrapping around, and center its row in the viewport.
    fn next_match(&mut self, dir: i32) {
        if self.matches.is_empty() {
            return;
        }
        let len = self.matches.len() as i32;
        self.current_match = (self.current_match as i32 + dir).rem_euclid(len) as usize;
        self.center_current_match();
    }

    /// Center the viewport on the current match, if any. In focus mode the
    /// window is re-aimed at the match's file first, so cross-file jumps
    /// land visible.
    fn center_current_match(&mut self) {
        let Some(m) = self.matches.get(self.current_match) else {
            return;
        };
        if self.follow {
            self.selected = m.file_pos;
        }
        let half = self.diff_viewport_height / 2;
        self.diff_scroll = (m.row as u16).saturating_sub(half);
        self.clamp_scroll_to_window();
        self.update_selected_from_scroll();
    }

    // --- per-file commit history --------------------------------------------

    /// `←`: enter history at the newest commit, or step one commit older.
    pub fn history_back(&mut self) {
        match self.history.as_mut() {
            None => self.enter_history(),
            Some(h) if h.pos + 1 < h.entries.len() => {
                h.pos += 1;
                h.scroll = 0;
                self.render_history();
            }
            Some(_) => {} // already at the oldest commit
        }
    }

    /// `→`: step one commit newer; from the newest commit, back to live.
    pub fn history_forward(&mut self) {
        let Some(h) = self.history.as_mut() else {
            return;
        };
        if h.pos == 0 {
            self.history = None;
        } else {
            h.pos -= 1;
            h.scroll = 0;
            self.render_history();
        }
    }

    /// Drop back to the live diff (no-op when already there).
    pub fn exit_history(&mut self) {
        self.history = None;
    }

    fn enter_history(&mut self) {
        let Some(entry) = self.current_entry() else {
            return;
        };
        if entry.kind == ChangeKind::Untracked {
            self.set_status("no commit history for this file");
            return;
        }
        let path = entry.path.clone();
        let branch = git::branch_commits(&self.root);
        let entries = match git::file_history(&self.root, &path, &branch) {
            Ok(e) => e,
            Err(e) => {
                self.set_status(format!("git error: {e}"));
                return;
            }
        };
        if entries.is_empty() {
            self.set_status("no commit history for this file");
            return;
        }
        self.selection = None;
        self.history = Some(HistoryView {
            path,
            entries,
            pos: 0,
            text: Text::default(),
            scroll: 0,
            built_width: 0,
            cache: HashMap::new(),
        });
        self.render_history();
    }

    /// Render (or re-render) the current history entry at the current width,
    /// through the per-commit cache. Preserves (clamps) the scroll offset —
    /// stepping resets it to 0 before calling this.
    fn render_history(&mut self) {
        let width = self.diff_width;
        let theme = self.theme.clone();
        let tabs = self.tab_width;
        let root = self.root.clone();
        let viewport = self.diff_viewport_height;
        let Some(h) = self.history.as_mut() else {
            return;
        };
        let entry = h.entries[h.pos].clone();
        let text = match h.cache.get(&entry.hash).cloned() {
            Some(t) => t,
            None => {
                let rendered = git::commit_diff(&root, &entry.hash, &entry.path_at_commit)
                    .and_then(|raw| delta::render(&raw, width, theme.as_deref(), tabs));
                let text = match rendered {
                    Ok(t) if t.lines.is_empty() => Text::from("  (no textual diff)"),
                    Ok(t) => {
                        // delta doesn't wrap when piped — wrap here, same as
                        // the live combined diff, so scrolling stays exact.
                        let mut lines = Vec::with_capacity(t.lines.len());
                        for line in &t.lines {
                            wrap_line_into(line, width as usize, &mut lines);
                        }
                        Text::from(lines)
                    }
                    Err(e) => Text::from(format!("  error rendering history: {e}")),
                };
                h.cache.insert(entry.hash.clone(), text.clone());
                text
            }
        };
        h.text = text;
        h.built_width = width;
        let max = history_max_scroll(h.text.lines.len(), viewport);
        h.scroll = h.scroll.min(max);
    }

    /// Keep the history view consistent after a combined-diff rebuild: exit
    /// if the selection moved off the file it belongs to (vanished file,
    /// follow jump, filter change), and re-render after a resize or theme
    /// change (`built_width` of 0 forces it).
    fn sync_history(&mut self) {
        let Some(h) = self.history.as_ref() else {
            return;
        };
        let still_selected = self.current_entry().is_some_and(|e| e.path == h.path);
        if !still_selected {
            self.history = None;
            return;
        }
        if h.built_width != self.diff_width {
            if let Some(h) = self.history.as_mut() {
                h.cache.clear();
            }
            self.render_history();
        }
    }

    // --- sorting -----------------------------------------------------------

    pub fn cycle_sort_field(&mut self) {
        self.exit_history();
        self.sort_field = match self.sort_field {
            SortField::Tree => SortField::Modified,
            SortField::Modified => SortField::Size,
            SortField::Size => SortField::Tree,
        };
        self.resort_preserving();
        self.announce_sort();
    }

    pub fn toggle_sort_dir(&mut self) {
        self.exit_history();
        self.sort_desc = !self.sort_desc;
        self.resort_preserving();
        self.announce_sort();
    }

    /// Flash the new sort order in the footer (mirrors the theme/follow cues).
    fn announce_sort(&mut self) {
        self.set_status(format!(
            "sort: {}",
            self.sort_field.indicator(self.sort_desc)
        ));
    }

    fn sort_files(&mut self) {
        sort_entries(&mut self.files, self.sort_field);
        if self.sort_desc {
            self.files.reverse();
        }
    }

    fn resort_preserving(&mut self) {
        self.restore_anchor = self.current_anchor();
        self.sort_files();
        self.rebuild_filter();
        self.needs_rebuild = true;
        self.save_config();
    }

    // --- theme & diff mode -------------------------------------------------

    /// Cycle the syntax theme (`dir` = +1 forward, -1 back), clearing the render
    /// cache so every diff re-renders with the new theme, and persisting it.
    pub fn cycle_theme(&mut self, dir: i32) {
        let current = self
            .theme
            .as_deref()
            .and_then(|t| THEMES.iter().position(|&x| x == t));
        let len = THEMES.len() as i32;
        let next = match current {
            Some(i) => (i as i32 + dir).rem_euclid(len),
            None => 0,
        };
        let theme = THEMES[next as usize];
        self.theme = Some(theme.to_string());
        self.set_status(format!("theme: {theme}"));
        self.cache.clear();
        if let Some(h) = self.history.as_mut() {
            h.cache.clear();
            // Force sync_history's re-render on the next build.
            h.built_width = 0;
        }
        self.restore_anchor = self.current_anchor();
        self.needs_rebuild = true;
        self.save_config();
    }

    /// Toggle between working-tree changes and base-branch changes.
    pub fn toggle_diff_mode(&mut self) {
        self.diff_mode = match self.diff_mode {
            DiffMode::Working => DiffMode::BaseBranch,
            DiffMode::BaseBranch => DiffMode::Working,
        };
        self.selected = 0;
        self.diff_scroll = 0;
        self.restore_anchor = None;
        self.behind_dirty = true;
        self.refresh();
        self.save_config();
    }

    // --- search ------------------------------------------------------------

    /// Whether the search box currently holds a content query (as opposed to
    /// being empty or a `file:` filename filter).
    pub fn is_content_search(&self) -> bool {
        search::split_query(&self.search).0.is_some()
    }

    /// Scope-filtered content-match count for `files[i]`, fetching (and
    /// retaining) the raw diff on a cache miss — which only happens for files
    /// that changed since their last render.
    fn count_content_matches(&mut self, i: usize, needle: &str) -> usize {
        let entry = self.files[i].clone();
        let sig = signature(&entry);
        if self.cache.raw_text(&entry.path, sig).is_none()
            && let Ok(raw) = git::raw_diff(&self.root, &entry, &self.base)
        {
            let stripped = search::strip_ansi(&String::from_utf8_lossy(&raw));
            self.cache.insert_raw(&entry.path, sig, stripped);
        }
        self.cache
            .raw_text(&entry.path, sig)
            .map(|raw| search::count_matches(raw, needle, self.search_scope))
            .unwrap_or(0)
    }

    fn rebuild_filter(&mut self) {
        let (needle, path_query) = search::split_query(&self.search);
        let q = crate::filter::Query::parse(&path_query);
        // With a content query active, a file must have >= 1 match to stay.
        let counts: Option<Vec<usize>> = needle.as_deref().map(|n| {
            (0..self.files.len())
                .map(|i| self.count_content_matches(i, n))
                .collect()
        });

        // Everything except the file-type filter — this also determines
        // which extensions are offered as chips, so deselecting one never
        // makes its own chip disappear.
        let pre_type: Vec<usize> = self
            .files
            .iter()
            .enumerate()
            .filter(|(i, f)| {
                q.matches(&f.path)
                    && (self.kind_filter.is_empty() || self.kind_filter.contains(&f.kind))
                    && counts.as_ref().is_none_or(|c| c[*i] > 0)
            })
            .map(|(i, _)| i)
            .collect();

        let mut types: Vec<String> = pre_type
            .iter()
            .filter_map(|&i| file_extension(&self.files[i].path))
            .collect();
        types.sort();
        types.dedup();
        self.file_types = types;

        self.filtered = pre_type
            .into_iter()
            .filter(|&i| {
                self.file_type_filter.is_empty()
                    || file_extension(&self.files[i].path)
                        .is_some_and(|e| self.file_type_filter.contains(&e))
            })
            .collect();

        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }

    /// Toggle whether `ext` is included in the active file-type filter,
    /// re-filtering. The only entry point today is clicking a file-type
    /// chip — there is no keyboard equivalent.
    fn toggle_file_type(&mut self, ext: String) {
        self.exit_history();
        if !self.file_type_filter.remove(&ext) {
            self.file_type_filter.insert(ext);
        }
        self.apply_filter();
    }

    /// Re-filter after a filter edit, focusing the top of the (new) diff.
    fn apply_filter(&mut self) {
        self.rebuild_filter();
        self.selected = 0;
        self.diff_scroll = 0;
        self.restore_anchor = None;
        self.current_match = 0;
        self.pending_match_jump = self.is_content_search();
        self.needs_rebuild = true;
    }

    // --- input -------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) {
        // Ctrl-C always quits.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        // While help is open, any key (except quit) dismisses it.
        if self.show_help {
            if matches!(key.code, KeyCode::Char('q')) {
                self.should_quit = true;
            }
            self.show_help = false;
            return;
        }
        match self.mode {
            Mode::Normal => self.handle_normal_key(key),
            Mode::Search => self.handle_search_key(key),
            Mode::KindFilter => self.handle_kind_key(key),
        }
    }

    // --- mouse -------------------------------------------------------------

    /// Returns whether this event changed anything that needs a redraw.
    /// `Moved` events fire on every cell the cursor crosses, so they only
    /// report `true` when the resolved hover target actually changed — every
    /// other event kind is a discrete user action and always redraws, same
    /// as before this method returned a value.
    pub fn handle_mouse(&mut self, ev: MouseEvent) -> bool {
        let (col, row) = (ev.column, ev.row);

        // Any click dismisses the help overlay.
        if self.show_help && matches!(ev.kind, MouseEventKind::Down(_)) {
            self.show_help = false;
            return true;
        }

        if let MouseEventKind::Moved = ev.kind {
            let before = self.hover_target();
            self.hover_pos = Some((col, row));
            let after = self.hover_target();
            return before != after;
        }

        match ev.kind {
            MouseEventKind::ScrollDown => {
                if self.in_left(col) {
                    self.select_next();
                } else {
                    self.scroll_down(3);
                }
            }
            MouseEventKind::ScrollUp => {
                if self.in_left(col) {
                    self.select_prev();
                } else {
                    self.scroll_up(3);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.selection = None;
                if self.near_divider(col) {
                    self.dragging_divider = true;
                } else if self.in_left(col) {
                    if row == self.area_left.y {
                        self.click_sort_indicator(col);
                    } else if row == self.area_left.y + 1 {
                        self.click_kind_chip(col);
                    } else if row == self.area_left.y + 2 {
                        self.click_file_type_chip(col);
                    } else {
                        self.click_toc(row);
                    }
                } else if self.in_diff(col, row) {
                    self.begin_selection(col, row);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.dragging_divider {
                    self.set_split_from_col(col);
                } else if self.selecting {
                    self.update_selection(col, row);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self.dragging_divider {
                    self.dragging_divider = false;
                    self.save_config();
                } else if self.selecting {
                    self.selecting = false;
                    self.sel_autoscroll = 0;
                    self.copy_selection();
                }
            }
            _ => {}
        }
        true
    }

    /// True while a divider drag is in progress (used to defer diff rebuilds).
    pub fn is_dragging(&self) -> bool {
        self.dragging_divider
    }

    fn divider_col(&self) -> u16 {
        self.area_left.x + self.area_left.width
    }

    fn near_divider(&self, col: u16) -> bool {
        if self.sidebar_collapsed {
            return false;
        }
        let b = self.divider_col();
        col + 1 >= b && col <= b
    }

    fn in_left(&self, col: u16) -> bool {
        if self.sidebar_collapsed {
            return false;
        }
        col >= self.area_left.x && col + 1 < self.divider_col()
    }

    fn in_diff(&self, col: u16, row: u16) -> bool {
        // Inside the diff pane's content: below the header row, left of the
        // reserved scrollbar-rail column.
        col >= self.area_right.x
            && col + 1 < self.area_right.x + self.area_right.width
            && row > self.area_right.y
            && row < self.area_right.y + self.area_right.height
    }

    /// Click the file-list header's right-aligned sort indicator (e.g.
    /// `"tree ▼"`): the label half cycles the sort field (same as `s`), the
    /// arrow half reverses direction (same as `r`). A no-op on the gap space
    /// between them or anywhere else on the header row.
    fn click_sort_indicator(&mut self, col: u16) {
        let label_w = self.sort_field.label().chars().count() as u16;
        // Matches the row width `ui::draw_file_list` renders against: the
        // pane's true last column is reserved (it doubles as part of the
        // divider's mouse grab zone, see `near_divider`, and a click there
        // can never reach this dispatch in the first place).
        let row_width = self.area_left.width.saturating_sub(1);
        match sort_indicator_part(col, self.area_left.x, row_width, label_w) {
            Some(SortIndicatorPart::Label) => self.cycle_sort_field(),
            Some(SortIndicatorPart::Arrow) => self.toggle_sort_dir(),
            None => {}
        }
    }

    fn click_kind_chip(&mut self, col: u16) {
        let rel = col.saturating_sub(self.area_left.x);
        if let Some(idx) = chip_at_col(rel, &[3, 3, 3, 3, 3], 0) {
            self.toggle_kind(KIND_ORDER[idx]);
        }
    }

    fn click_file_type_chip(&mut self, col: u16) {
        let rel = col.saturating_sub(self.area_left.x);
        let widths: Vec<u16> = self
            .file_types
            .iter()
            .map(|ext| ext.chars().count() as u16 + 3) // "[.ext]"
            .collect();
        if let Some(idx) = chip_at_col(rel, &widths, 1) {
            let ext = self.file_types[idx].clone();
            self.toggle_file_type(ext);
        }
    }

    fn click_toc(&mut self, row: u16) {
        // Rows inside the list start below the header, kind-chip, and
        // file-type-chip rows.
        let top = self.area_left.y + 3;
        if let Some(idx) =
            row_to_filtered_index(row, top, self.list_state.offset(), self.filtered.len())
        {
            self.history = None;
            self.selected = idx;
            self.scroll_to_selected();
        }
    }

    /// Resolves `hover_pos` against current state to determine which
    /// clickable element (if any) the mouse is currently over. Reuses the
    /// exact same hit-test helpers the click handlers use, so hovering and
    /// clicking can never disagree about what's where.
    pub fn hover_target(&self) -> Option<Hover> {
        if self.sidebar_collapsed || self.show_help {
            return None;
        }
        let (col, row) = self.hover_pos?;
        if !self.in_left(col) {
            return None;
        }
        if row == self.area_left.y {
            let label_w = self.sort_field.label().chars().count() as u16;
            let row_width = self.area_left.width.saturating_sub(1);
            return match sort_indicator_part(col, self.area_left.x, row_width, label_w) {
                Some(SortIndicatorPart::Label) => Some(Hover::SortLabel),
                Some(SortIndicatorPart::Arrow) => Some(Hover::SortArrow),
                None => None,
            };
        }
        if row == self.area_left.y + 1 {
            let rel = col.saturating_sub(self.area_left.x);
            return chip_at_col(rel, &[3, 3, 3, 3, 3], 0)
                .map(|idx| Hover::KindChip(KIND_ORDER[idx]));
        }
        if row == self.area_left.y + 2 {
            let rel = col.saturating_sub(self.area_left.x);
            let widths: Vec<u16> = self
                .file_types
                .iter()
                .map(|ext| ext.chars().count() as u16 + 3)
                .collect();
            return chip_at_col(rel, &widths, 1)
                .map(|idx| Hover::FileType(self.file_types[idx].clone()));
        }
        row_to_filtered_index(
            row,
            self.area_left.y + 3,
            self.list_state.offset(),
            self.filtered.len(),
        )
        .map(Hover::FileRow)
    }

    /// Map a diff-pane cell to a `(line, column)` in the active view's text.
    fn diff_cell(&self, col: u16, row: u16) -> (usize, usize) {
        let inner_top = self.area_right.y + 1;
        let inner_left = self.area_right.x;
        let (scroll, last_line) = match &self.history {
            Some(h) => (h.scroll, h.text.lines.len().saturating_sub(1)),
            None => {
                let (_, wend) = self.view_window();
                (self.diff_scroll, wend.saturating_sub(1))
            }
        };
        let line = (scroll as usize + row.saturating_sub(inner_top) as usize).min(last_line);
        let column = col.saturating_sub(inner_left) as usize;
        (line, column)
    }

    fn begin_selection(&mut self, col: u16, row: u16) {
        let pos = self.diff_cell(col, row);
        self.selection = Some(Selection {
            anchor: pos,
            cursor: pos,
        });
        self.selecting = true;
        self.sel_autoscroll = 0;
        if self.history.is_none() {
            self.selected = self.file_at_line(pos.0);
        }
    }

    fn update_selection(&mut self, col: u16, row: u16) {
        let top = self.area_right.y + 1;
        // Last content row (inside the bottom border).
        let bottom = self.area_right.y + self.area_right.height.saturating_sub(2);
        if row < top {
            self.sel_autoscroll = -1;
            self.selection_autoscroll_tick();
        } else if row > bottom {
            self.sel_autoscroll = 1;
            self.selection_autoscroll_tick();
        } else {
            self.sel_autoscroll = 0;
            let pos = self.diff_cell(col, row);
            if let Some(sel) = &mut self.selection {
                sel.cursor = pos;
            }
        }
    }

    /// True while a selection drag is auto-scrolling past a pane edge.
    pub fn is_autoscrolling(&self) -> bool {
        self.selecting && self.sel_autoscroll != 0
    }

    /// Advance an in-progress edge auto-scroll by one step, extending the
    /// selection to the new top/bottom line. Returns whether it scrolled.
    pub fn selection_autoscroll_tick(&mut self) -> bool {
        if !self.is_autoscrolling() {
            return false;
        }
        const STEP: u16 = 3;
        // Read before taking `history`'s mutable borrow: `self.diff_viewport_height`
        // is a disjoint field, but `self.sel_autoscroll` needs to survive as a
        // plain value since the `else` arm below re-reads it after `h` is dropped.
        let viewport = self.diff_viewport_height;
        let dir = self.sel_autoscroll;
        if let Some(h) = self.history.as_mut() {
            let before = h.scroll;
            if dir < 0 {
                h.scroll = h.scroll.saturating_sub(STEP);
                if let Some(sel) = &mut self.selection {
                    sel.cursor = (h.scroll as usize, 0);
                }
            } else {
                let max = history_max_scroll(h.text.lines.len(), viewport);
                h.scroll = (h.scroll + STEP).min(max);
                let bottom_line = (h.scroll as usize + viewport.saturating_sub(1) as usize)
                    .min(h.text.lines.len().saturating_sub(1));
                if let Some(sel) = &mut self.selection {
                    sel.cursor = (bottom_line, usize::MAX);
                }
            }
            // History has no file-selection concept to resync — unlike the
            // live path, scrolling it never touches `self.selected`.
            return h.scroll != before;
        }
        if dir < 0 {
            let before = self.diff_scroll;
            self.diff_scroll = self.diff_scroll.saturating_sub(STEP);
            self.clamp_scroll_to_window();
            if let Some(sel) = &mut self.selection {
                sel.cursor = (self.diff_scroll as usize, 0);
            }
            self.update_selected_from_scroll();
            self.diff_scroll != before
        } else {
            let before = self.diff_scroll;
            self.diff_scroll = (self.diff_scroll + STEP).min(self.max_scroll());
            let (_, wend) = self.view_window();
            // Extend to the bottom visible line; usize::MAX = end of line (clamped later).
            let bottom_line = (self.diff_scroll as usize + viewport.saturating_sub(1) as usize)
                .min(wend.saturating_sub(1));
            if let Some(sel) = &mut self.selection {
                sel.cursor = (bottom_line, usize::MAX);
            }
            self.update_selected_from_scroll();
            self.diff_scroll != before
        }
    }

    /// Copy the current selection's plain text to the clipboard (queued for the
    /// event loop to emit as an OSC 52 sequence).
    pub fn copy_selection(&mut self) {
        let Some(sel) = self.selection else { return };
        if sel.is_empty() {
            return;
        }
        let (start, end) = sel.ordered();
        let lines = match &self.history {
            Some(h) => &h.text.lines,
            None => &self.combined.lines,
        };
        let mut out = String::new();
        for li in start.0..=end.0 {
            let Some(line) = lines.get(li) else {
                break;
            };
            let chars: Vec<char> = line.spans.iter().flat_map(|s| s.content.chars()).collect();
            let from = if li == start.0 { start.1 } else { 0 };
            let to = if li == end.0 { end.1 } else { chars.len() };
            let from = from.min(chars.len());
            let to = to.min(chars.len()).max(from);
            out.extend(&chars[from..to]);
            if li != end.0 {
                out.push('\n');
            }
        }
        if out.is_empty() {
            return;
        }
        let n = out.chars().count();
        self.pending_copy = Some(out);
        self.set_status(format!("copied {n} chars"));
    }

    fn set_split_from_col(&mut self, col: u16) {
        let main = self.area_main;
        if main.width == 0 {
            return;
        }
        let left_w = col.saturating_sub(main.x);
        let pct = (left_w as u32 * 100 / main.width as u32) as u16;
        self.split_pct = pct.clamp(15, 85);
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => {
                if self.history.is_some() {
                    self.history = None;
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::Left => self.history_back(),
            KeyCode::Right => self.history_forward(),
            KeyCode::Down | KeyCode::Char('j') => self.select_next(),
            KeyCode::Up | KeyCode::Char('k') => self.select_prev(),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_down(self.page()),
            KeyCode::PageUp => self.scroll_up(self.page()),
            KeyCode::Home | KeyCode::Char('g') => self.scroll_home(),
            KeyCode::End | KeyCode::Char('G') => self.scroll_end(),
            KeyCode::Char('s') => self.cycle_sort_field(),
            KeyCode::Char('r') => self.toggle_sort_dir(),
            KeyCode::Char('t') => self.cycle_theme(1),
            KeyCode::Char('T') => self.cycle_theme(-1),
            KeyCode::Char('b') => {
                self.exit_history();
                self.toggle_diff_mode();
            }
            KeyCode::Char('f') => self.toggle_follow(),
            KeyCode::Char('c') => {
                self.exit_history();
                self.mode = Mode::KindFilter;
            }
            KeyCode::Tab => self.toggle_sidebar(),
            KeyCode::Char('y') => self.copy_selection(),
            KeyCode::Char('n') => {
                if self.history.is_none() {
                    self.next_match(1);
                }
            }
            KeyCode::Char('N') => {
                if self.history.is_none() {
                    self.next_match(-1);
                }
            }
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('/') => {
                self.exit_history();
                self.mode = Mode::Search;
                self.search.clear();
                self.apply_filter();
            }
            _ => {}
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                // Cancel: clear the query and show everything again.
                self.search_scope = search::Scope::Both;
                self.search.clear();
                self.apply_filter();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => {
                // Accept: keep the filter, return to normal navigation.
                self.mode = Mode::Normal;
            }
            KeyCode::Backspace => {
                self.search.pop();
                self.apply_filter();
            }
            KeyCode::Tab => {
                self.search_scope = self.search_scope.cycle();
                self.apply_filter();
            }
            KeyCode::Down => self.next_match(1),
            KeyCode::Up => self.next_match(-1),
            KeyCode::Char(c) => {
                self.search.push(c);
                self.apply_filter();
            }
            _ => {}
        }
    }

    fn handle_kind_key(&mut self, key: KeyEvent) {
        let kind = match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('c') => {
                self.mode = Mode::Normal;
                return;
            }
            KeyCode::Char('a') => ChangeKind::Added,
            KeyCode::Char('m') => ChangeKind::Modified,
            KeyCode::Char('d') => ChangeKind::Deleted,
            KeyCode::Char('r') => ChangeKind::Renamed,
            KeyCode::Char('u') => ChangeKind::Untracked,
            _ => return,
        };
        self.toggle_kind(kind);
    }

    /// Toggle whether `kind` is included in the active kind filter,
    /// re-filtering. Shared by the `c`-mode keyboard toggle and clicking a
    /// kind chip directly, so the two paths can never disagree.
    fn toggle_kind(&mut self, kind: ChangeKind) {
        self.exit_history();
        if !self.kind_filter.remove(&kind) {
            self.kind_filter.insert(kind);
        }
        self.apply_filter();
    }
}

/// Max scroll for a history view: stop when the last line reaches the bottom.
fn history_max_scroll(lines: usize, viewport: u16) -> u16 {
    (lines as u16).saturating_sub(viewport)
}

/// Content signature for cache keying: worktree mtime combined with the change
/// kind. An edit bumps the mtime, producing a fresh signature and re-render.
fn signature(entry: &FileEntry) -> u64 {
    let nanos = entry
        .mtime
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ (entry.kind as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// Sort `files` by `field`, ascending (`App::sort_files` reverses afterwards
/// for descending order).
fn sort_entries(files: &mut [FileEntry], field: SortField) {
    match field {
        SortField::Tree => files.sort_by(|a, b| a.path.cmp(&b.path)),
        SortField::Modified => files.sort_by_key(|f| f.mtime),
        SortField::Size => files.sort_by(|a, b| {
            size_key(a)
                .cmp(&size_key(b))
                .then_with(|| a.path.cmp(&b.path))
        }),
    }
}

/// Total changed lines for size sorting; unknown stats (binary files) count as 0.
fn size_key(f: &FileEntry) -> u64 {
    f.added.unwrap_or(0) as u64 + f.removed.unwrap_or(0) as u64
}

/// The lowercased extension of `path` (no leading dot); `None` for
/// extensionless files and dotfiles with no further suffix (e.g.
/// `.gitignore`, `Makefile`) — matches `std::path::Path::extension()`.
fn file_extension(path: &str) -> Option<String> {
    Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
}

/// Which half of the sort indicator column `col` lands in.
#[derive(Debug, PartialEq, Eq)]
enum SortIndicatorPart {
    Label,
    Arrow,
}

/// Classifies `col` against the file-list header's right-aligned sort
/// indicator — `"{label} {arrow}"` (e.g. `"tree ▼"`), flush against the
/// right edge of a row spanning `[row_x, row_x + row_width)`. `label_w` is
/// the label's width; the arrow is always exactly 1 column, separated from
/// the label by a 1-column gap. `None` for the gap space or outside the
/// indicator entirely (the indicator itself is never truncated, unlike the
/// stats it shares the header row with).
fn sort_indicator_part(
    col: u16,
    row_x: u16,
    row_width: u16,
    label_w: u16,
) -> Option<SortIndicatorPart> {
    let total_w = label_w + 2; // label + 1-column gap + 1-column arrow
    let right_edge = row_x + row_width;
    let start = right_edge.saturating_sub(total_w);
    if col < start || col >= right_edge {
        return None;
    }
    let rel = col - start;
    if rel < label_w {
        Some(SortIndicatorPart::Label)
    } else if rel == label_w {
        None // the gap space
    } else {
        Some(SortIndicatorPart::Arrow)
    }
}

/// Which chip (0-based) contains column `rel` (relative to the row's left
/// edge, which starts with a 1-column margin), given each chip's on-screen
/// `widths` in order and the `gap` columns between adjacent chips. `None` if
/// `rel` falls in the margin, in a gap between two chips, or past the last
/// chip. Shared by the fixed-width kind-chip row (`gap = 0`) and the
/// variable-width file-type-chip row (`gap = 1`).
fn chip_at_col(rel: u16, widths: &[u16], gap: u16) -> Option<usize> {
    if rel < 1 {
        return None;
    }
    let mut pos: u16 = 1;
    for (i, &w) in widths.iter().enumerate() {
        if rel < pos + w {
            return Some(i);
        }
        pos += w + gap;
        if rel < pos {
            return None;
        }
    }
    None
}

/// Maps screen row `row` to an index into a `filtered_len`-long list, given
/// the list's first visible row (`top`) and its current scroll `offset`
/// (`list_state.offset()`). `None` if `row` is above the list or past its
/// last entry. Shared by `click_toc` and `App::hover_target` so the two can
/// never disagree on which entry a row corresponds to.
fn row_to_filtered_index(row: u16, top: u16, offset: usize, filtered_len: usize) -> Option<usize> {
    if row < top {
        return None;
    }
    let idx = offset + (row - top) as usize;
    if idx < filtered_len { Some(idx) } else { None }
}

/// Hard-wrap a styled line to `width` columns, appending one `Line` per visual
/// row to `out` (styles preserved). This keeps the invariant that one `Line` is
/// exactly one rendered row, which the scroll/offset logic relies on. Width is
/// approximated as one column per char (fine for the ASCII-heavy content of
/// diffs; wide glyphs are rare here).
fn wrap_line_into(line: &Line<'static>, width: usize, out: &mut Vec<Line<'static>>) {
    let total: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
    if width == 0 || total <= width {
        out.push(line.clone());
        return;
    }

    let mut row: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;
    for span in &line.spans {
        let style = span.style;
        let mut buf = String::new();
        for c in span.content.chars() {
            buf.push(c);
            col += 1;
            if col >= width {
                row.push(Span::styled(std::mem::take(&mut buf), style));
                out.push(Line::from(std::mem::take(&mut row)));
                col = 0;
            }
        }
        if !buf.is_empty() {
            row.push(Span::styled(buf, style));
        }
    }
    if !row.is_empty() {
        out.push(Line::from(row));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        App, AppMatch, Selection, SortField, SortIndicatorPart, chip_at_col, file_extension,
        row_to_filtered_index, sort_entries, sort_indicator_part,
    };
    use crate::git::{ChangeKind, FileEntry};
    use ratatui::crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::layout::Rect;
    use std::time::SystemTime;

    fn entry(path: &str, added: Option<u32>, removed: Option<u32>) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            kind: ChangeKind::Modified,
            mtime: SystemTime::UNIX_EPOCH,
            added,
            removed,
        }
    }

    /// Three files occupying combined-diff lines 0-9, 10-24, 25-29, with a
    /// viewport of 8 lines. No git or delta involved: fields are set
    /// directly, which in-module tests may do.
    fn focus_app() -> App {
        let mut app = App::new(
            std::path::PathBuf::from("."),
            crate::config::Config::default(),
        );
        app.combined = ratatui::text::Text::from(
            (0..30)
                .map(|i| ratatui::text::Line::from(format!("line {i}")))
                .collect::<Vec<_>>(),
        );
        app.offsets = vec![0, 10, 25];
        app.files = vec![
            entry("a.rs", Some(1), Some(0)),
            entry("b.rs", Some(1), Some(0)),
            entry("c.rs", Some(1), Some(0)),
        ];
        app.filtered = vec![0, 1, 2];
        app.diff_viewport_height = 8;
        app
    }

    #[test]
    fn view_window_is_the_full_diff_when_follow_is_off_and_the_file_section_when_on() {
        let mut app = focus_app();
        assert_eq!(app.view_window(), (0, 30));
        app.follow = true;
        app.selected = 0;
        assert_eq!(app.view_window(), (0, 10));
        app.selected = 1;
        assert_eq!(app.view_window(), (10, 25));
        // Last file's window ends at the diff's end, not at a phantom offset.
        app.selected = 2;
        assert_eq!(app.view_window(), (25, 30));
    }

    #[test]
    fn focus_scrolling_clamps_to_the_selected_files_section() {
        let mut app = focus_app();
        app.follow = true;
        app.selected = 1;
        app.scroll_home();
        assert_eq!(app.diff_scroll, 10);
        // Window is 15 lines, viewport 8: the last scroll that keeps the
        // pane inside the file is 10 + (15 - 8) = 17.
        app.scroll_end();
        assert_eq!(app.diff_scroll, 17);
        app.scroll_down(100); // clamped, never bleeds into c.rs
        assert_eq!(app.diff_scroll, 17);
        app.scroll_up(100); // clamped at the file's first line
        assert_eq!(app.diff_scroll, 10);
        // A page never crosses the file edge either.
        app.scroll_down(app.page());
        assert_eq!(app.diff_scroll, 17);
    }

    #[test]
    fn focus_window_shorter_than_the_viewport_pins_to_the_file_start() {
        let mut app = focus_app();
        app.follow = true;
        app.selected = 2; // 5 lines < 8-line viewport
        app.scroll_end();
        assert_eq!(app.diff_scroll, 25);
        app.scroll_home();
        assert_eq!(app.diff_scroll, 25);
    }

    #[test]
    fn snapping_into_focus_jumps_to_the_file_start_only_when_outside_its_section() {
        let mut app = focus_app();
        app.selected = 1;
        // Outside the section (viewing a.rs): snap to b.rs's start.
        app.diff_scroll = 3;
        app.follow = true;
        app.snap_into_focus();
        assert_eq!(app.diff_scroll, 10);
        // Already inside: keep the position (just clamped).
        app.diff_scroll = 12;
        app.snap_into_focus();
        assert_eq!(app.diff_scroll, 12);
    }

    #[test]
    fn update_selected_from_scroll_is_inert_in_focus_mode() {
        let mut app = focus_app();
        app.follow = true;
        app.selected = 1;
        // A stale scroll must not re-derive the selection: the window is
        // derived FROM the selection, so the reverse mapping is circular.
        app.diff_scroll = 0;
        app.update_selected_from_scroll();
        assert_eq!(app.selected, 1);
        // Follow off: same scroll re-derives selection as before.
        app.follow = false;
        app.update_selected_from_scroll();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn centering_a_match_in_another_file_reaims_the_focus_window() {
        let mut app = focus_app();
        app.follow = true;
        app.selected = 0;
        app.matches = vec![AppMatch {
            row: 27,
            cols: (0, 1),
            file_pos: 2,
            aligned: true,
        }];
        app.current_match = 0;
        app.next_match(0); // dir 0: recenter on the current match
        assert_eq!(app.selected, 2, "focus follows the match's file");
        // Scroll landed inside c.rs's window (25..30), clamped to its start
        // since the 5-line window is shorter than the viewport.
        assert_eq!(app.diff_scroll, 25);
    }

    #[test]
    fn file_extension_handles_common_cases() {
        assert_eq!(file_extension("src/app.rs"), Some("rs".to_string()));
        assert_eq!(file_extension("README.MD"), Some("md".to_string())); // lowercased
        assert_eq!(file_extension("app.test.ts"), Some("ts".to_string())); // last segment
        assert_eq!(file_extension(".gitignore"), None); // dotfile, no further suffix
        assert_eq!(file_extension("Makefile"), None); // no extension at all
        assert_eq!(file_extension("src/no_ext"), None);
    }

    #[test]
    fn sort_field_config_keys_round_trip() {
        for field in [SortField::Tree, SortField::Modified, SortField::Size] {
            assert_eq!(SortField::from_key(field.as_key()), Some(field));
        }
        assert_eq!(SortField::from_key("size"), Some(SortField::Size));
        assert_eq!(SortField::from_key("bogus"), None);
    }

    #[test]
    fn size_sort_is_ascending_by_total_lines_with_path_tiebreak() {
        let mut files = vec![
            entry("big.rs", Some(90), Some(30)),
            entry("b_mid.rs", Some(5), Some(5)),
            entry("a_mid.rs", Some(10), Some(0)),
            entry("small.rs", Some(1), Some(0)),
        ];
        sort_entries(&mut files, SortField::Size);
        let order: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        // 1 < 10 == 10 (tie → path order) < 120
        assert_eq!(order, ["small.rs", "a_mid.rs", "b_mid.rs", "big.rs"]);
    }

    #[test]
    fn size_sort_treats_missing_stats_as_zero() {
        let mut files = vec![
            entry("changed.rs", Some(3), Some(1)),
            entry("binary.bin", None, None),
        ];
        sort_entries(&mut files, SortField::Size);
        let order: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(order, ["binary.bin", "changed.rs"]);
    }

    #[test]
    fn chip_at_col_fixed_width_no_gap() {
        let widths = [3u16; 5];
        assert_eq!(chip_at_col(0, &widths, 0), None); // margin
        assert_eq!(chip_at_col(1, &widths, 0), Some(0));
        assert_eq!(chip_at_col(3, &widths, 0), Some(0));
        assert_eq!(chip_at_col(4, &widths, 0), Some(1));
        assert_eq!(chip_at_col(15, &widths, 0), Some(4));
        assert_eq!(chip_at_col(16, &widths, 0), None); // past the last chip
    }

    #[test]
    fn chip_at_col_variable_width_with_gap() {
        let widths = [5u16, 6u16]; // "[.ts]" then "[.tsx]"
        assert_eq!(chip_at_col(0, &widths, 1), None); // margin
        assert_eq!(chip_at_col(1, &widths, 1), Some(0));
        assert_eq!(chip_at_col(5, &widths, 1), Some(0)); // last col of chip 0
        assert_eq!(chip_at_col(6, &widths, 1), None); // inside the gap
        assert_eq!(chip_at_col(7, &widths, 1), Some(1)); // first col of chip 1
        assert_eq!(chip_at_col(12, &widths, 1), Some(1)); // last col of chip 1
        assert_eq!(chip_at_col(13, &widths, 1), None); // past the last chip
    }

    #[test]
    fn chip_at_col_no_chips() {
        assert_eq!(chip_at_col(1, &[], 0), None);
    }

    #[test]
    fn row_to_filtered_index_below_top_is_none() {
        assert_eq!(row_to_filtered_index(2, 3, 0, 10), None);
    }

    #[test]
    fn row_to_filtered_index_applies_scroll_offset() {
        // top=3, offset=5: row 3 is the 0th visible row, which is index 5.
        assert_eq!(row_to_filtered_index(3, 3, 5, 10), Some(5));
        assert_eq!(row_to_filtered_index(5, 3, 5, 10), Some(7));
    }

    #[test]
    fn row_to_filtered_index_past_the_last_entry_is_none() {
        // top=3, offset=0, filtered_len=10: row 12 maps to index 9 (last valid).
        assert_eq!(row_to_filtered_index(12, 3, 0, 10), Some(9));
        assert_eq!(row_to_filtered_index(13, 3, 0, 10), None);
    }

    #[test]
    fn sort_indicator_part_splits_label_gap_and_arrow() {
        // Row spans [10, 20); label_w=4 -> total width 6, flush region [14, 20).
        assert_eq!(sort_indicator_part(13, 10, 10, 4), None); // just left of the region
        assert_eq!(
            sort_indicator_part(14, 10, 10, 4),
            Some(SortIndicatorPart::Label)
        ); // first label column
        assert_eq!(
            sort_indicator_part(17, 10, 10, 4),
            Some(SortIndicatorPart::Label)
        ); // last label column
        assert_eq!(sort_indicator_part(18, 10, 10, 4), None); // the gap space
        assert_eq!(
            sort_indicator_part(19, 10, 10, 4),
            Some(SortIndicatorPart::Arrow)
        );
        assert_eq!(sort_indicator_part(20, 10, 10, 4), None); // past the row entirely
    }

    #[test]
    fn sort_indicator_part_zero_label_width_is_just_the_arrow() {
        // label_w=0 -> total width 2 (gap + arrow only), flush region [18, 20).
        assert_eq!(sort_indicator_part(17, 10, 10, 0), None);
        assert_eq!(sort_indicator_part(18, 10, 10, 0), None); // the gap space
        assert_eq!(
            sort_indicator_part(19, 10, 10, 0),
            Some(SortIndicatorPart::Arrow)
        );
    }

    fn test_git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
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

    /// A repo with two commits touching a.txt plus an uncommitted edit, so
    /// the working changeset contains exactly a.txt with real history.
    fn history_app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        test_git(p, &["init", "-b", "main"]);
        test_git(p, &["config", "user.name", "t"]);
        test_git(p, &["config", "user.email", "t@t"]);
        test_git(p, &["config", "commit.gpgsign", "false"]);
        std::fs::write(p.join("a.txt"), "one\n").unwrap();
        test_git(p, &["add", "-A"]);
        test_git(p, &["commit", "-m", "add a"]);
        std::fs::write(p.join("a.txt"), "one\ntwo\n").unwrap();
        test_git(p, &["add", "-A"]);
        test_git(p, &["commit", "-m", "grow a"]);
        std::fs::write(p.join("a.txt"), "one\ntwo\nthree\n").unwrap();

        let mut app = App::new(p.to_path_buf(), crate::config::Config::default());
        app.refresh();
        assert_eq!(
            app.filtered.len(),
            1,
            "changeset is exactly the edited file"
        );
        (dir, app)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn left_enters_history_steps_older_and_clamps_at_the_oldest_commit() {
        let (_dir, mut app) = history_app();
        app.handle_key(key(KeyCode::Left));
        let h = app.history.as_ref().expect("history entered");
        assert_eq!((h.pos, h.entries.len()), (0, 2));
        assert_eq!(h.entries[0].subject, "grow a"); // newest first
        assert!(!h.text.lines.is_empty(), "entry rendered on entry");

        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.history.as_ref().unwrap().pos, 1);
        app.handle_key(key(KeyCode::Left)); // already at the oldest: no-op
        assert_eq!(app.history.as_ref().unwrap().pos, 1);
    }

    #[test]
    fn right_steps_newer_and_returns_to_live_from_the_newest_commit() {
        let (_dir, mut app) = history_app();
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.history.as_ref().unwrap().pos, 0);
        app.handle_key(key(KeyCode::Right)); // newest → back to live
        assert!(app.history.is_none());
        app.handle_key(key(KeyCode::Right)); // already live: no-op, not a crash
        assert!(app.history.is_none());
    }

    #[test]
    fn esc_exits_history_first_and_only_quits_from_live() {
        let (_dir, mut app) = history_app();
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.history.is_none());
        assert!(!app.should_quit, "Esc in history must not quit");
        app.handle_key(key(KeyCode::Esc));
        assert!(app.should_quit);
    }

    #[test]
    fn untracked_files_report_no_history_instead_of_entering() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        test_git(p, &["init", "-b", "main"]);
        test_git(p, &["config", "user.name", "t"]);
        test_git(p, &["config", "user.email", "t@t"]);
        std::fs::write(p.join("fresh.txt"), "new\n").unwrap();
        let mut app = App::new(p.to_path_buf(), crate::config::Config::default());
        app.refresh();
        assert_eq!(app.filtered.len(), 1);

        app.handle_key(key(KeyCode::Left));
        assert!(app.history.is_none());
        assert_eq!(app.status, "no commit history for this file");
    }

    #[test]
    fn changing_the_selection_exits_history() {
        let (_dir, mut app) = history_app();
        // Add a second changed file so j/k can move the selection.
        std::fs::write(_dir.path().join("z.txt"), "untracked\n").unwrap();
        app.refresh();
        assert_eq!(app.filtered.len(), 2);
        // Select the tracked file explicitly — sort order isn't guaranteed to
        // put it first, and history can only be entered on a committed file.
        let a_idx = app
            .filtered
            .iter()
            .position(|&i| app.files[i].path == "a.txt")
            .unwrap();
        app.selected = a_idx;
        app.handle_key(key(KeyCode::Left));
        assert!(app.history.is_some());
        // Step toward the other file, whichever side it's on.
        app.handle_key(key(if a_idx == 0 {
            KeyCode::Down
        } else {
            KeyCode::Up
        }));
        assert!(app.history.is_none(), "j/k selection change resets to live");
    }

    fn mouse(kind: MouseEventKind, col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn drag_selection_autoscroll_advances_the_history_scroll_not_the_live_diff() {
        let (_dir, mut app) = history_app();
        app.handle_key(key(KeyCode::Left));
        assert!(app.history.is_some(), "history entered before dragging");
        // Shrink the viewport well below the rendered commit diff's line count
        // so a single STEP-sized autoscroll tick is guaranteed to move `scroll`,
        // without needing an oversized fixture commit.
        app.diff_viewport_height = 1;
        app.area_right = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 5,
        };

        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 2));
        assert!(app.selecting, "drag started inside the pane");

        // bottom = area_right.y + height.saturating_sub(2) = 3; row 6 is past it,
        // so this drag both sets sel_autoscroll and fires the first tick itself.
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 5, 6));
        let after_drag = app.history.as_ref().unwrap().scroll;
        assert!(
            after_drag > 0,
            "autoscroll must move the history view's own scroll, not diff_scroll"
        );
        assert_eq!(
            app.diff_scroll, 0,
            "the live diff's scroll must stay untouched while history is active"
        );

        // A second, explicit tick (standing in for the event loop's timer-driven
        // repeat) must also land on history, not silently fall through to live.
        app.selection_autoscroll_tick();
        let h = app.history.as_ref().unwrap();
        assert!(
            h.scroll > after_drag,
            "a further autoscroll tick keeps advancing the history scroll"
        );
        assert_eq!(app.diff_scroll, 0);

        // The selection cursor must clamp against the history text, not the
        // (much shorter, unrelated) live combined diff.
        let cursor_line = app.selection.unwrap().cursor.0;
        assert!(cursor_line < h.text.lines.len());
    }

    #[test]
    fn mouse_driven_sort_and_filter_reshapes_exit_history() {
        let (_dir, mut app) = history_app();

        // Sort-field cycling reshapes the file list (and combined diff order),
        // so it must drop out of history exactly like the `s` keyboard arm.
        app.handle_key(key(KeyCode::Left));
        assert!(app.history.is_some());
        app.cycle_sort_field();
        assert!(
            app.history.is_none(),
            "cycle_sort_field must exit history itself, not rely on the caller"
        );

        // Sort-direction toggling is the same story as `r`.
        app.handle_key(key(KeyCode::Left));
        assert!(app.history.is_some());
        app.toggle_sort_dir();
        assert!(app.history.is_none());

        // Kind filtering changes which files are even visible.
        app.handle_key(key(KeyCode::Left));
        assert!(app.history.is_some());
        app.toggle_kind(ChangeKind::Modified);
        assert!(app.history.is_none());

        // File-type filtering (click-only, no keyboard equivalent) reshapes
        // the list the same way — use the fixture's own "txt" extension.
        app.toggle_kind(ChangeKind::Modified); // undo the filter above first
        app.handle_key(key(KeyCode::Left));
        assert!(app.history.is_some());
        app.toggle_file_type("txt".to_string());
        assert!(app.history.is_none());
    }

    #[test]
    fn entering_history_clears_any_live_text_selection() {
        let (_dir, mut app) = history_app();
        app.selection = Some(Selection {
            anchor: (0, 0),
            cursor: (0, 3),
        });
        app.handle_key(key(KeyCode::Left));
        assert!(app.history.is_some(), "history entered");
        assert!(
            app.selection.is_none(),
            "a leftover live selection must not paint over the historical diff"
        );
    }
}
