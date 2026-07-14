//! diffski — a live diff TUI for reviewing changes as a coding agent works.
//!
//! Watches a git repo/worktree, lists changed files (left), and shows the
//! selected file's delta-rendered diff (right), refreshing automatically as
//! files change on disk.

mod app;
mod config;
mod delta;
mod git;
mod ui;
mod watch;

use anyhow::{Context, Result};
use app::App;
use clap::Parser;
use config::Config;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

/// Live diff TUI for reviewing changes as a coding agent works.
#[derive(Parser)]
#[command(name = "diffski", version, about, long_about = None)]
struct Cli {
    /// Path to the git repository or worktree to watch.
    #[arg(default_value = ".")]
    path: PathBuf,

    /// delta syntax theme to use (overrides the saved/gitconfig theme).
    #[arg(long, value_name = "NAME")]
    theme: Option<String>,

    /// List the available delta syntax themes and exit.
    #[arg(long)]
    list_themes: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Fail early with clear messages before touching the terminal.
    delta::ensure_available()?;

    if cli.list_themes {
        return delta::list_syntax_themes();
    }

    let root = git::repo_root(&cli.path)?;

    // Load persisted UI state; a `--theme` flag overrides the saved theme.
    let mut config = Config::load();
    if cli.theme.is_some() {
        config.theme = cli.theme;
    }

    // Filesystem watcher -> channel drained by the event loop. `_watcher` must
    // stay alive for the duration of the run.
    let (tx, rx) = mpsc::channel();
    let _watcher = watch::watch(&root, tx).context("failed to start filesystem watcher")?;

    let mut app = App::new(root, config);

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app, rx);
    ratatui::restore();
    result
}

fn run(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    rx: mpsc::Receiver<watch::WatchEvent>,
) -> Result<()> {
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|f| ui::draw(f, app))?;
            dirty = false;
        }

        // Build (or rebuild) the combined diff *after* the frame is on screen, so
        // the file list appears instantly and a "rendering…" placeholder shows
        // while the diffs are produced. `diff_width` is known once we've drawn.
        if app.needs_build() {
            app.ensure_combined();
            dirty = true;
            continue;
        }

        // Block for input, but wake ~10x/sec to drain the filesystem channel.
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app.handle_key(key);
                    dirty = true;
                }
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }

        // Coalesce all pending filesystem changes into one refresh.
        let mut changed = Vec::new();
        while let Ok(paths) = rx.try_recv() {
            changed.extend(paths);
        }
        if !changed.is_empty() {
            app.on_fs_change(changed);
            dirty = true;
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}
