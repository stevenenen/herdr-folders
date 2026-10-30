# Herdr Folders

Walk a folder tree in a popup, with recently opened folders pinned to the top, and open a tab
or a git worktree in one keystroke.

```
  / to filter
──────────────────────────────────────────────────────
 Recent
   ~/Development/acme-api
   ~/Library/Mobile Documents/iCloud~md~obsidian/Documents/sharedObsidian
 Browse
 ⎇ Development
   sharedObsidian
──────────────────────────────────────────────────────
 j/k move  Tab go into folder  Backspace up  Enter open (worktree if git)  Shift+Tab plain tab  u paste link/path  / filter  Esc back/close
```

- **Recent**, at the top of the root view only, lists folders you've opened before, most
  recently first — the chance you want the same one again is high.
- **Browse** lists each configured root. `⎇` marks a folder that is itself a git repo.
- **Tab** walks into the selected folder, showing its subfolders. **Backspace** walks back
  up a level.
- **`/`** filters the folders currently shown (every word must match, case-insensitive). Esc
  stops typing but keeps the filter; Esc again clears it; Esc again walks up a level; Esc at
  the root closes the popup.
- **Enter** opens the selected folder: a new git worktree when it (or an ancestor) is a repo, else
  a plain tab. Every press creates a fresh worktree with Claude started in it, branched from the
  default branch as last fetched (no network wait). Branch, worktree folder and workspace are
  named after the folder: the repo root becomes `work`, a subfolder its path relative to the
  repo, slugged (`docs/setup` → `docs-setup`). A name already in use gets a number: `work-2`,
  `work-3`.
- **Shift+Tab** always opens a plain tab at the folder, even inside a git repo — for folders
  you just want to look at (notes, docs) without spinning up a branch.
- **`u`** opens a centered popup to paste a GitHub issue/pull-request link or an absolute path;
  Enter opens it, Esc closes the popup without doing anything:
  - a GitHub link finds your clone of that repo (cloning it into the first configured root
    when there is none), opens a worktree for it and starts `claude "<link>"` — an issue gets a
    new branch from the latest default branch, a pull request checks out its own branch.
    Pasting the same link again reopens that worktree. Needs `gh`, signed in to an account
    that can see the repo.
  - an absolute path (a folder, or a file — its containing folder is used) opens exactly like
    `Enter` would on that folder: a worktree if it's inside a git repo, else a plain tab.

## Install

```sh
herdr plugin install stevenenen/herdr-folders
```

Needs Rust (`cargo`); Herdr builds the plugin on install.

Tell it which folders to walk. It has no default, so until you set this, the popup shows where
to put it. Create `config.toml` in the plugin's config directory:

```sh
cat > "$(herdr plugin config-dir stevenenen.herdr-folders)/config.toml" <<'TOML'
[dirs]
roots = ["~/Development"]   # each root is itself an entry; Tab walks into it
TOML
```

Bind a key in `~/.config/herdr/config.toml`:

```toml
[[keys.command]]
key = "prefix+t"
type = "plugin_action"
command = "stevenenen.herdr-folders.open"
description = "Folders: walk the tree, open a tab or worktree"
```

## Keys

| Key | Action |
| --- | --- |
| `j` / `k`, `↑` / `↓` | Move |
| `g` / `G`, `Home` / `End` | Top / bottom |
| `Tab` | Walk into the selected folder |
| `Backspace` | Walk up one level |
| `/` | Filter the folders currently shown |
| `Enter` | Open: a new worktree if the folder is (or is inside) a git repo, else a plain tab |
| `Shift+Tab` | Open as a plain tab, even inside a git repo |
| `u` | Paste a GitHub issue/PR link or an absolute path; Enter opens it, Esc cancels |
| `Esc` | Stop typing, then clear the filter, then walk up a level, then close |

## State

Recently opened folders live in `$HERDR_PLUGIN_STATE_DIR/recent.json`, capped at 200 entries;
only the 25 most recent (and still existing) show in the popup.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
herdr plugin link .
```

`herdr plugin link` points Herdr at this checkout directly, no GitHub round-trip needed while
iterating.
