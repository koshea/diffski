# diffski

A live diff TUI you keep open next to a coding agent.

While Claude Code (or any agent, or you) edits a repo, `diffski` watches the
working tree and shows what changed. The right pane is one continuous diff of
**everything** that changed — just keep paging down through it. The left pane is
a table of contents: it tracks which file you're currently looking at, and
selecting a file jumps you straight to its section. It refreshes itself
automatically as files change on disk. Diffs are rendered by
[delta](https://github.com/dandavison/delta), so you get its themes, syntax
highlighting, and your existing `[delta]` gitconfig for free.

```
┌ changed files (3) ─────────┐┌ M src/main.rs ─────────────────────────────┐
│▶ M src/main.rs             ││src/main.rs                                 │
│  M notes.txt               ││────────────────────────────────────────────│
│  ? fresh.txt               ││ fn main() {                                │
│                            ││-    let x = 1;                             │
│                            ││+    let x = 42;                            │
│                            ││+    let y = 7;                             │
│                            ││     println!("hello");                     │
│                            ││ }                                          │
└────────────────────────────┘└────────────────────────────────────────────┘
↑/↓ file · PgUp/PgDn scroll · s sort · t theme · b base · / search · q quit
```

(delta renders the real thing with full color, syntax highlighting, and line numbers.)

## What it shows

Two modes, toggled with `b`:

- **Working** (default) — everything changed since the last commit: staged **and**
  unstaged changes (`git diff HEAD`) plus untracked files (new files the agent
  created).
- **Base branch** — everything on your current branch versus its base branch
  (the merge base with `origin/HEAD`, or `main`/`master`): committed **and**
  uncommitted changes, plus untracked files. Good for reviewing a whole branch.

Deletions and renames show up in both. `.gitignore` is honored.

As the agent works, diffski keeps you oriented without getting noisy:

- The file-list title shows a **changeset summary** — `N files · +added -removed`.
- Files the agent touches that you **haven't looked at yet** get a small `●`; it
  clears once you view them.
- **Follow-latest** (`f`) jumps the view to each file as it changes, so review
  tracks the agent live. Off by default; it never fights manual navigation.

## Requirements

- **[delta](https://github.com/dandavison/delta)** must be installed and on your
  `PATH` (`cargo install git-delta`, or your system package manager). diffski
  checks for it at startup and exits with instructions if it's missing.
- **git**.

## Install

Straight from git (installs the `diffski` binary into `~/.cargo/bin`):

```sh
cargo install --git https://github.com/koshea/diffski
```

Or from a local checkout:

```sh
git clone https://github.com/koshea/diffski
cd diffski
cargo install --path .
# or just build it: cargo build --release  (binary at target/release/diffski)
```

## Usage

```sh
diffski            # watch the current directory's repo
diffski path/to/repo
diffski path/to/worktree
```

Leave it running in a spare pane or on a second monitor; it updates on its own
as the agent works (it checks for changes a couple of times a second).

## Keys

| Key | Action |
|-----|--------|
| `↑` / `↓` (or `k` / `j`) | jump to the previous / next file's section |
| `PageUp` / `PageDown` (or `Space`) | scroll the combined diff |
| `Home` / `End` (or `g` / `G`) | jump to the very top / bottom |
| `s` | cycle sort: by tree (path) ⇄ by modified time |
| `r` | reverse sort direction |
| `t` / `T` | cycle syntax theme forward / back |
| `b` | toggle diff mode: working changes ⇄ vs base branch |
| `f` | follow-latest: jump to files as the agent changes them |
| `y` | copy the current text selection |
| `/` | search filenames (type to filter; `Enter` accepts, `Esc` clears) |
| `?` | show the help overlay |
| `q` / `Esc` / `Ctrl-C` | quit |

## Mouse

diffski captures the mouse, so:

- **Wheel** scrolls the diff (over the file list, it moves between files).
- **Click** a file to jump to it, or click in the diff to place the cursor.
- **Drag in the diff** to select text — just the diff content, no borders or the
  file column. Drag past the top or bottom edge to auto-scroll and keep
  selecting beyond the visible area. The selection is copied to your clipboard on
  release (via OSC 52, which works locally and over SSH; in tmux, enable
  `set-clipboard on`). Press `y` to copy again.
- **Drag the divider** between the panes to resize them; the width is remembered.

A live scrollbar on the diff shows your position in the whole changeset.

## Themes

diffski uses delta's syntax themes. Cycle through them live with `t` / `T`, or
pick one explicitly:

```sh
diffski --theme Dracula
diffski --list-themes      # see what's available
```

With no `--theme`, diffski uses whatever your gitconfig `[delta] syntax-theme`
specifies. The theme you pick with `t`/`--theme` is remembered (see below).

## Updating

diffski keeps itself up to date. On startup (at most once a day, in the
background) it runs `cargo install --git` again; when a newer version is
available it rebuilds and swaps the binary, and you pick it up on the next
launch (you'll see a "restart to apply" note when one lands). Updates ship when
the crate version is bumped, so you don't recompile on every commit.

Disable it for one run with `--no-update`, or permanently with
`auto_update=false` in the config. To update by hand:

```sh
cargo install --git https://github.com/koshea/diffski --force
```

## Remembered settings

Your sort order, theme, diff mode, pane split, follow-latest, and auto-update
preference are persisted to `$XDG_CONFIG_HOME/diffski/config` (default
`~/.config/diffski/config`), so diffski opens the same way every time.

## How it works

- **File list** comes from `git status --porcelain` — cheap, so navigation is
  instant.
- **Diffs** are produced per-file with `git diff` and piped through the `delta`
  binary (delta has no stable library API, so diffski shells out to it). delta's
  ANSI output is parsed into the TUI with
  [`ansi-to-tui`](https://github.com/ratatui/ansi-to-tui). The per-file diffs are
  concatenated into one continuous view, and diffski tracks each file's line
  offset so the table of contents can jump to it and follow your scrolling.
- **Rendered diffs are cached** by `(path, content signature, width)`, so a file
  is only re-rendered when it actually changes or the terminal is resized;
  rebuilding the combined view is otherwise just concatenation. Cold renders run
  in parallel across cores, and the file list paints before the diffs are built,
  so startup is fast even for large changesets.
- **Live updates** come from polling `git status` on a background thread rather
  than a filesystem watcher. `git status` stays fast (tens of ms) even on a
  monorepo with over a million directories, where a recursive inotify watch
  would be slow and blow past the OS watch limit. The poll also folds in the
  mtimes of changed files, so re-editing an already-changed file is detected.

Built with [ratatui](https://ratatui.rs).
