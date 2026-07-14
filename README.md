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
as the agent works (filesystem writes are debounced ~150 ms).

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
| `/` | search filenames (type to filter; `Enter` accepts, `Esc` clears) |
| `q` / `Esc` / `Ctrl-C` | quit |

## Themes

diffski uses delta's syntax themes. Cycle through them live with `t` / `T`, or
pick one explicitly:

```sh
diffski --theme Dracula
diffski --list-themes      # see what's available
```

With no `--theme`, diffski uses whatever your gitconfig `[delta] syntax-theme`
specifies. The theme you pick with `t`/`--theme` is remembered (see below).

## Remembered settings

Your sort order, theme, and diff mode are persisted to
`$XDG_CONFIG_HOME/diffski/config` (default `~/.config/diffski/config`), so diffski
opens the same way every time.

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
- **Live updates** come from a debounced recursive [`notify`](https://github.com/notify-rs/notify)
  watcher; `git status` remains the source of truth for what changed.

Built with [ratatui](https://ratatui.rs).
