//! Application state and behavior.
//!
//! The diff pane is a single continuous view of *all* changed files' diffs,
//! concatenated in the current sort order. The left-hand file list is a table
//! of contents: selecting a file jumps the scroll to that file's section, and
//! as you page through the combined diff the selection tracks whichever file is
//! at the top of the viewport.

use crate::config::Config;
use crate::delta::{self, DiffCache};
use crate::git::{self, FileEntry};
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortField {
    /// Alphabetical by path (a tree-ish grouping).
    Tree,
    /// By worktree modification time.
    Modified,
}

impl SortField {
    pub fn label(self) -> &'static str {
        match self {
            SortField::Tree => "tree",
            SortField::Modified => "modified",
        }
    }
    pub fn as_key(self) -> &'static str {
        match self {
            SortField::Tree => "tree",
            SortField::Modified => "modified",
        }
    }
    pub fn from_key(s: &str) -> Option<Self> {
        match s {
            "tree" => Some(SortField::Tree),
            "modified" => Some(SortField::Modified),
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
    /// Pane rectangles from the last render, for mouse hit-testing.
    pub area_main: Rect,
    pub area_left: Rect,
    pub area_right: Rect,
    /// File-list scroll state (kept across frames for click mapping).
    pub list_state: ListState,
    dragging_divider: bool,

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
    pub status: String,
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
            area_main: Rect::default(),
            area_left: Rect::default(),
            area_right: Rect::default(),
            list_state: ListState::default(),
            dragging_divider: false,
            selection: None,
            selecting: false,
            sel_autoscroll: 0,
            pending_copy: None,
            follow: config.follow,
            auto_update: config.auto_update,
            update_ready: false,
            recently_changed: HashSet::new(),
            last_mtimes: HashMap::new(),
            active_path_overflow: false,
            marquee_offset: 0,
            marquee_sel: 0,
            marquee_last: None,
            show_help: false,
            status: String::new(),
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

    fn max_scroll(&self) -> u16 {
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
                    self.status = "no base branch found — showing working changes".to_string();
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
                self.status = format!("git error: {e}");
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
        }
        .save();
    }

    /// Note that a background update has been installed.
    pub fn set_update_ready(&mut self, msg: String) {
        self.update_ready = true;
        self.status = msg;
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

    /// Toggle follow-latest mode.
    pub fn toggle_follow(&mut self) {
        self.follow = !self.follow;
        self.status = if self.follow {
            "follow: on".into()
        } else {
            "follow: off".into()
        };
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

        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut offsets: Vec<usize> = Vec::with_capacity(entries.len());
        for entry in &entries {
            offsets.push(lines.len());
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
                    for line in text.lines {
                        wrap_line_into(&line, width as usize, &mut lines);
                    }
                }
                Err(err) => {
                    lines.push(Line::from(format!(
                        "  error rendering {}: {err}",
                        entry.path
                    )));
                    self.status = err.to_string();
                }
            }
        }

        self.combined = Text::from(lines);
        self.offsets = offsets;
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
        self.diff_scroll = self.diff_scroll.min(self.max_scroll());
        self.update_selected_from_scroll();
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
        type Rendered = (String, u64, std::result::Result<Text<'static>, String>);
        let out: Mutex<Vec<Rendered>> = Mutex::new(Vec::new());

        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(entry) = cold.get(i) else { break };
                        let sig = signature(entry);
                        let rendered = git::raw_diff(root, entry, base)
                            .and_then(|raw| delta::render(&raw, width, theme.as_deref(), tabs))
                            .map_err(|e| e.to_string());
                        out.lock()
                            .unwrap()
                            .push((entry.path.clone(), sig, rendered));
                    }
                });
            }
        });

        for (path, sig, res) in out.into_inner().unwrap() {
            match res {
                Ok(text) => self.cache.insert(&path, sig, width, text),
                Err(e) => self.status = e,
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
            self.selected += 1;
            self.scroll_to_selected();
        }
    }

    /// Jump the viewport to the previous file in the table of contents.
    pub fn select_prev(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            self.scroll_to_selected();
        }
    }

    pub fn scroll_down(&mut self, amount: u16) {
        self.diff_scroll = (self.diff_scroll + amount).min(self.max_scroll());
        self.update_selected_from_scroll();
    }

    pub fn scroll_up(&mut self, amount: u16) {
        self.diff_scroll = self.diff_scroll.saturating_sub(amount);
        self.update_selected_from_scroll();
    }

    pub fn scroll_home(&mut self) {
        self.diff_scroll = 0;
        self.update_selected_from_scroll();
    }

    pub fn scroll_end(&mut self) {
        self.diff_scroll = self.max_scroll();
        self.update_selected_from_scroll();
    }

    fn page(&self) -> u16 {
        // Scroll by nearly a full viewport, keeping one line of context.
        self.diff_viewport_height.saturating_sub(1).max(1)
    }

    // --- sorting -----------------------------------------------------------

    pub fn cycle_sort_field(&mut self) {
        self.sort_field = match self.sort_field {
            SortField::Tree => SortField::Modified,
            SortField::Modified => SortField::Tree,
        };
        self.resort_preserving();
    }

    pub fn toggle_sort_dir(&mut self) {
        self.sort_desc = !self.sort_desc;
        self.resort_preserving();
    }

    fn sort_files(&mut self) {
        match self.sort_field {
            SortField::Tree => self.files.sort_by(|a, b| a.path.cmp(&b.path)),
            SortField::Modified => self.files.sort_by_key(|f| f.mtime),
        }
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
        self.status = format!("theme: {theme}");
        self.cache.clear();
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
        self.refresh();
        self.save_config();
    }

    // --- search ------------------------------------------------------------

    fn rebuild_filter(&mut self) {
        let q = self.search.to_lowercase();
        self.filtered = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| q.is_empty() || f.path.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }

    /// Re-filter after a search edit, focusing the top of the (new) diff.
    fn apply_search(&mut self) {
        self.rebuild_filter();
        self.selected = 0;
        self.diff_scroll = 0;
        self.restore_anchor = None;
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
        }
    }

    // --- mouse -------------------------------------------------------------

    pub fn handle_mouse(&mut self, ev: MouseEvent) {
        let (col, row) = (ev.column, ev.row);

        // Any click dismisses the help overlay.
        if self.show_help && matches!(ev.kind, MouseEventKind::Down(_)) {
            self.show_help = false;
            return;
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
                    self.click_toc(row);
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
    }

    /// True while a divider drag is in progress (used to defer diff rebuilds).
    pub fn is_dragging(&self) -> bool {
        self.dragging_divider
    }

    fn divider_col(&self) -> u16 {
        self.area_left.x + self.area_left.width
    }

    fn near_divider(&self, col: u16) -> bool {
        let b = self.divider_col();
        col + 1 >= b && col <= b
    }

    fn in_left(&self, col: u16) -> bool {
        col >= self.area_left.x && col + 1 < self.divider_col()
    }

    fn in_diff(&self, col: u16, row: u16) -> bool {
        // Inside the diff pane's inner (content) area, excluding borders.
        col > self.area_right.x
            && col < self.area_right.x + self.area_right.width.saturating_sub(1)
            && row > self.area_right.y
            && row < self.area_right.y + self.area_right.height.saturating_sub(1)
    }

    fn click_toc(&mut self, row: u16) {
        // Rows inside the list start one below the top border.
        let top = self.area_left.y + 1;
        if row < top {
            return;
        }
        let idx = self.list_state.offset() + (row - top) as usize;
        if idx < self.filtered.len() {
            self.selected = idx;
            self.scroll_to_selected();
        }
    }

    /// Map a diff-pane cell to a `(line, column)` in the combined diff.
    fn diff_cell(&self, col: u16, row: u16) -> (usize, usize) {
        let inner_top = self.area_right.y + 1;
        let inner_left = self.area_right.x + 1;
        let line = self.diff_scroll as usize + row.saturating_sub(inner_top) as usize;
        let line = line.min(self.combined.lines.len().saturating_sub(1));
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
        self.selected = self.file_at_line(pos.0);
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
        if self.sel_autoscroll < 0 {
            let before = self.diff_scroll;
            self.diff_scroll = self.diff_scroll.saturating_sub(STEP);
            if let Some(sel) = &mut self.selection {
                sel.cursor = (self.diff_scroll as usize, 0);
            }
            self.update_selected_from_scroll();
            self.diff_scroll != before
        } else {
            let before = self.diff_scroll;
            self.diff_scroll = (self.diff_scroll + STEP).min(self.max_scroll());
            // Extend to the bottom visible line; usize::MAX = end of line (clamped later).
            let bottom_line = (self.diff_scroll as usize
                + self.diff_viewport_height.saturating_sub(1) as usize)
                .min(self.combined.lines.len().saturating_sub(1));
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
        let mut out = String::new();
        for li in start.0..=end.0 {
            let Some(line) = self.combined.lines.get(li) else {
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
        self.status = format!("copied {n} chars");
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
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
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
            KeyCode::Char('b') => self.toggle_diff_mode(),
            KeyCode::Char('f') => self.toggle_follow(),
            KeyCode::Char('y') => self.copy_selection(),
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('/') => {
                self.mode = Mode::Search;
                self.search.clear();
                self.apply_search();
            }
            _ => {}
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                // Cancel: clear the query and show everything again.
                self.search.clear();
                self.apply_search();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => {
                // Accept: keep the filter, return to normal navigation.
                self.mode = Mode::Normal;
            }
            KeyCode::Backspace => {
                self.search.pop();
                self.apply_search();
            }
            KeyCode::Char(c) => {
                self.search.push(c);
                self.apply_search();
            }
            _ => {}
        }
    }
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
