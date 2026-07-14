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
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::{Line, Text};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::UNIX_EPOCH;

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

pub struct App {
    pub root: PathBuf,
    /// Diff base: `HEAD`/empty tree in working mode, or the merge base in
    /// base-branch mode.
    base: String,
    /// delta syntax-theme override; `None` means "use gitconfig".
    theme: Option<String>,
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

    pub status: String,
    pub should_quit: bool,
}

impl App {
    pub fn new(root: PathBuf, config: Config) -> Self {
        let mut app = App {
            root,
            base: String::new(),
            theme: config.theme,
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
            status: String::new(),
            should_quit: false,
        };
        app.refresh();
        app
    }

    // --- queries -----------------------------------------------------------

    pub fn current_entry(&self) -> Option<&FileEntry> {
        self.filtered
            .get(self.selected)
            .and_then(|&i| self.files.get(i))
    }

    fn max_scroll(&self) -> u16 {
        let lines = self.combined.lines.len() as u16;
        lines.saturating_sub(self.diff_viewport_height)
    }

    /// The current viewport position as `(path, offset within that file)`, so it
    /// can be restored after the combined diff is rebuilt.
    fn current_anchor(&self) -> Option<(String, u16)> {
        let &i = self.filtered.get(self.selected)?;
        let base = self.offsets.get(self.selected).copied().unwrap_or(0) as u16;
        Some((self.files[i].path.clone(), self.diff_scroll.saturating_sub(base)))
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
        }
        .save();
    }

    /// React to filesystem changes: drop stale cache entries for the changed
    /// paths, then reload the file list.
    pub fn on_fs_change(&mut self, paths: Vec<PathBuf>) {
        for p in &paths {
            if let Ok(rel) = p.strip_prefix(&self.root) {
                self.cache
                    .invalidate_path(&rel.to_string_lossy().replace('\\', "/"));
            }
        }
        self.refresh();
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
        let entries: Vec<FileEntry> = self.filtered.iter().map(|&i| self.files[i].clone()).collect();

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
            match self.cache.get_or_render(&entry.path, sig, width, self.theme.as_deref(), move || {
                git::raw_diff(&root, &e, &base)
            }) {
                Ok(text) if text.lines.is_empty() => lines.push(Line::from("  (no textual diff)")),
                Ok(text) => lines.extend(text.lines),
                Err(err) => {
                    lines.push(Line::from(format!("  error rendering {}: {err}", entry.path)));
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
            && let Some(pos) = self.filtered.iter().position(|&i| self.files[i].path == path)
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
                            .and_then(|raw| delta::render(&raw, width, theme.as_deref()))
                            .map_err(|e| e.to_string());
                        out.lock().unwrap().push((entry.path.clone(), sig, rendered));
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

    /// Set `selected` to whichever file's section contains the top of the viewport.
    fn update_selected_from_scroll(&mut self) {
        if self.offsets.is_empty() {
            self.selected = 0;
            return;
        }
        let s = self.diff_scroll as usize;
        let mut sel = 0;
        for (idx, &off) in self.offsets.iter().enumerate() {
            if off <= s {
                sel = idx;
            } else {
                break;
            }
        }
        self.selected = sel;
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
        match self.mode {
            Mode::Normal => self.handle_normal_key(key),
            Mode::Search => self.handle_search_key(key),
        }
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
