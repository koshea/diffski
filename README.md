# diffski

A live diff TUI you keep open next to a coding agent.

While Claude Code (or any agent, or you) edits a repo, `diffski` watches the
working tree and shows what changed. The right pane is one continuous diff of
**everything** that changed — just keep paging down through it. The left pane is
a table of contents: it tracks which file you're currently looking at, and
selecting a file jumps you straight to its section. It refreshes itself
automatically as files change on disk, and you can **search inside the
changes** — not the whole codebase, just the added and deleted lines — or
filter the changeset by filename or change type. Diffs are rendered by
[delta](https://github.com/dandavison/delta), so you get its themes, syntax
highlighting, and your existing `[delta]` gitconfig for free.

```
 CHANGES (3)  +12 -4        │ M src/main.rs   [12 / 96]
 [A][M][D][R][U]            │ src/main.rs
 [.rs] [.md]                │ ──────────────────────────────
 ▶ M src/main.rs +2 -1      │  fn main() {
   M notes.txt +9 -3        │ -    let x = 1;
   U fresh.txt +1           │ +    let x = 42;
                            │ +    let y = 7;
                            │      println!("hello");
                            │  }
c kinds · b base · / search · Tab collapse · ? help · q quit    main ↓3 [12/96]
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
- **Follow-latest** (`f`) turns the diff pane into a single-file focus view:
  it shows just the selected file's diff — nothing else — and jumps to each
  file as the agent changes it. `j`/`k`, clicks, and `n`/`N` re-aim the
  focus. Turn it off to get the full combined diff back, right where you
  left it. Off by default.
- The diff pane's header shows what you're diffing against and **`↓N`**
  when the base branch has commits your branch doesn't (it re-checks after
  fetches, too) — right next to your scroll position. The file list's header
  shows the current sort order the same way — click the field name to cycle
  it (same as `s`), or the arrow to reverse direction (same as `r`). The
  footer stays free for
  keybinding hints and whatever's actively unusual (filters, search matches,
  follow, status).
- **Per-file history** — with a file selected, `←` steps back through the
  commits that touched it (following renames, all the way past the branch
  point), `→` walks forward again, landing back on the live diff; `Esc`
  returns straight to live. The header shows the commit's short hash,
  subject, and position, plus a red `pre-branch` pill once you're looking at
  commits that predate the current branch. Untracked files have no history
  and say so.

## Search & filter

Everything below narrows both the file list and the combined diff together,
and all of it composes (content search AND kind filter AND filename filter):

- **`/` searches inside the changes.** Only added and deleted lines are
  searched — context lines never match. Matching is literal with smartcase (an
  all-lowercase query is case-insensitive; any uppercase makes it exact).
  While you type: files without matches drop out, each remaining file shows a
  `×N` match count, matches highlight in the diff (the current one stands
  out), the view jumps to the first match, and yellow marks on the scrollbar
  rail show where the rest are. `Tab` cycles the scope — `±` both, `+` added
  only, `−` deleted only. `↓`/`↑` step through matches without leaving the
  search box; `Enter` accepts and hands off to `n`/`N`; `Esc` clears.
- **`file:` filters filenames instead.** Type `file:` followed by
  comma-separated terms: plain text matches as a substring, `*`/`**`/`?` make
  a term a glob (`*` stays within a path segment, `**` crosses directories, a
  glob with no `/` matches the filename only), and a leading `!` excludes.
  Example: `file:src/**.rs, !test` — Rust files under `src/`, minus tests.
- **`c` filters by change type.** Then `a`/`m`/`d`/`r`/`u` toggle
  Added / Modified / Deleted / Renamed / Untracked live; `Enter` or `Esc`
  keeps the filter and returns to normal keys. The footer shows the active
  kinds; toggling everything off shows everything again.
- **Chips are clickable, not just keyboard-driven.** The file-list header
  shows two rows of toggle chips below `CHANGES`: change kinds (`[A][M][D]
  [R][U]`) and, dynamically, every file extension in the current changeset
  (`[.ts]`, `[.tsx]`, …). Click one to toggle it — identical to pressing its
  keyboard equivalent for kinds; there's no keyboard path for file types.
  Selected chips highlight; deselecting a file type never removes its own
  chip, so you can always turn it back on.
- **Hovering previews the click.** Any chip, the sort label, the sort
  arrow, and file-list rows underline when the mouse is over them, so it's
  obvious what's clickable before you click it.

## Requirements

- **git**.
- **[delta](https://github.com/dandavison/delta)** is recommended for full
  syntax highlighting and richer diff rendering. If it is missing, diffski
  falls back to plain ANSI `git diff` output so the TUI still runs.

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
| `s` | cycle sort: tree (path) → modified time → change size |
| `r` | reverse sort direction |
| `t` / `T` | cycle syntax theme forward / back |
| `b` | toggle diff mode: working changes ⇄ vs base branch |
| `c` | filter by change type — then `a`/`m`/`d`/`r`/`u` toggle kinds, `Enter`/`Esc` done |
| `Tab` | collapse / expand the file-list sidebar |
| `f` | follow-latest: jump to files as the agent changes them |
| `y` | copy the current text selection |
| `/` | search inside the changes (smartcase; `Tab` cycles ±/+/− scope). Prefix with `file:` to filter filenames instead — globs, `!` excludes |
| `n` / `N` | jump to the next / previous match (`↓`/`↑` while the search box is open) |
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

A live scrollbar on the diff shows your position in the whole changeset — and
during a content search, marks along it show where the matches are (cyan for
the current one).

## Themes

diffski uses delta's syntax themes. Cycle through them live with `t` / `T`, or
pick one explicitly:

```sh
diffski --theme Dracula
diffski --list-themes      # see what's available
```

With no `--theme`, diffski uses whatever your gitconfig `[delta] syntax-theme`
specifies. The theme you pick with `t`/`--theme` is remembered (see below).

## Tabs

Tabs in the diff expand to 4 spaces by default. Change it with `--tabs N` (for a
run) or `tab_width=N` in the config (persisted).

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

Your sort order, theme, diff mode, pane split, sidebar collapse, follow-latest,
tab width, and auto-update preference are persisted to
`$XDG_CONFIG_HOME/diffski/config` (default `~/.config/diffski/config`), so
diffski opens the same way every time. Filters and searches are deliberately
not persisted — they're situational.

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
- **Content search** runs against the raw `git diff` text (kept in memory
  alongside the rendered version, so typing never spawns a subprocess) and each
  hit is located in delta's decorated output by content-anchored alignment —
  walking the rendered lines forward and anchoring each raw diff line to the
  next rendered line that contains it. That's what lets highlights land on the
  exact columns even with delta's line numbers and decorations in the way, and
  it degrades gracefully (count + jump to the file, no highlight) if a line
  can't be aligned.
- **Live updates** come from polling `git status` on a background thread rather
  than a filesystem watcher. `git status` stays fast (tens of ms) even on a
  monorepo with over a million directories, where a recursive inotify watch
  would be slow and blow past the OS watch limit. The poll also folds in the
  mtimes of changed files, so re-editing an already-changed file is detected.

Built with [ratatui](https://ratatui.rs).
