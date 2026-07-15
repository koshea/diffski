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
mod update;
mod watch;

use anyhow::{Context, Result};
use app::App;
use clap::Parser;
use config::Config;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind,
};
use ratatui::crossterm::execute;
use std::io::{self, Write};
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

    /// Don't check for updates on startup (this run only).
    #[arg(long)]
    no_update: bool,
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

    // Background self-update (rate-limited, opt-out). Sends a note when done.
    let (utx, urx) = mpsc::channel::<String>();
    update::spawn_check(config.auto_update && !cli.no_update, utx);

    // Change detection runs on a background thread and signals over this channel.
    // `_watcher` must stay alive for the duration of the run.
    let (tx, rx) = mpsc::channel();
    let _watcher = watch::watch(&root, tx).context("failed to start change watcher")?;

    let mut app = App::new(root, config);

    // `ratatui::init()` installs a panic hook that restores the terminal, but it
    // doesn't know about mouse capture — chain a hook that disables it too, so a
    // panic never leaves the terminal in mouse-reporting mode.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        prev_hook(info);
    }));

    let mut terminal = ratatui::init();
    let mouse_on = execute!(io::stdout(), EnableMouseCapture).is_ok();
    let result = run(&mut terminal, &mut app, rx, urx);
    if mouse_on {
        let _ = execute!(io::stdout(), DisableMouseCapture);
    }
    ratatui::restore();
    result
}

/// Copy `text` to the system clipboard via an OSC 52 escape sequence. Works in
/// most modern terminals and over SSH (and through tmux with `set-clipboard on`).
fn copy_to_clipboard(text: &str) {
    let b64 = base64_encode(text.as_bytes());
    let mut stdout = io::stdout();
    // ESC ] 52 ; c ; <base64> BEL
    let _ = write!(stdout, "\x1b]52;c;{b64}\x07");
    let _ = stdout.flush();
}

/// Minimal standard base64 encoder (avoids a dependency for the OSC 52 payload).
fn base64_encode(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn run(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    rx: mpsc::Receiver<watch::WatchEvent>,
    urx: mpsc::Receiver<String>,
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
        // Skip while the divider is being dragged so we don't re-render every
        // file on each drag step; it rebuilds once the drag ends.
        if app.needs_build() && !app.is_dragging() {
            app.ensure_combined();
            dirty = true;
            continue;
        }

        // Block for input, but wake periodically to poll for changes. While a
        // selection is auto-scrolling past an edge, tick faster for smoothness.
        let timeout = if app.is_autoscrolling() {
            Duration::from_millis(30)
        } else {
            Duration::from_millis(100)
        };
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app.handle_key(key);
                    dirty = true;
                }
                Event::Mouse(m) => {
                    app.handle_mouse(m);
                    dirty = true;
                }
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }

        // Continue auto-scrolling a selection while the pointer is held past an
        // edge (crossterm sends no events while the mouse is stationary).
        if app.selection_autoscroll_tick() {
            dirty = true;
        }

        // Emit any queued clipboard copy (from a text selection).
        if let Some(text) = app.pending_copy.take() {
            copy_to_clipboard(&text);
        }

        // Coalesce any pending change signals into one refresh.
        let mut changed = false;
        while rx.try_recv().is_ok() {
            changed = true;
        }
        if changed {
            app.on_fs_change();
            dirty = true;
        }

        // A background update finished?
        if let Ok(msg) = urx.try_recv() {
            app.set_update_ready(msg);
            dirty = true;
        }

        // Scroll the selected file's name if it's too long for its column.
        if app.active_path_overflow && app.marquee_step() {
            dirty = true;
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::base64_encode;

    #[test]
    fn base64_rfc4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(input.as_bytes()), expected, "input={input:?}");
        }
    }
}
