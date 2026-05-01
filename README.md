# zsh mouse and flex history search

![zsh flex history screenshot](./screenshot.png)

Use the mouse to edit commands in your terminal just like regular text, drag and select, overwrite by typing. Automatically search zsh history with flexible priority fuzzy matching, and syntax highlighting. It works in other shells too when invoked directly.


## Install and Setup

```bash
uv tool install git+https://github.com/uAIex/zsh-mouse-and-flex-search
zsh-flex-history-init-zsh >> "${ZDOTDIR:-$HOME}/.zshrc"
```

Optionally, to import your existing Zsh history into the custom SQLite history database, run:

`zsh-flex-history-import`

## Behavior

- Uses in-order flexible fuzzy matching (similar to Emacs `flex`).
- Shows a completing-read style vertical completion menu with highlighted match chars.
- Prioritizes first-token matches (command completion and matching command prefixes) ahead of deeper in-string matches, then scores by recency and query fit.
- For directory-aware prioritization, use `--use-custom-history` so history scoring can include current `cwd`, which improves relevance for repeated workflows per folder.
- Takes over mouse `x` from the native terminal app only when there is any text in the prompt.
- Syntax highlighting is "good enough" but incomplete

## Options


- `--use-custom-history`
  - Uses an alternate per-user SQLite history backend.
  - Stores commands as UTF-8 text by default, unlike zsh
  - Includes extra metadata per entry (`command`, `cwd`, `timestamp`).
- `--history-length <N>`
  - Maximum number of SQLite history rows to load on the daemon's initial startup from the custom history DB (default: `10k`).
  - Accepts values like `10000` or `10k`.
  - Applies only to `--use-custom-history` and only on the daemon's first load; normal `~/.zsh_history` is not trimmed.
  - Does not delete rows from the SQLite file. Later daemon refreshes load normally without this cap.
- `--print-only`
  - Prints the selected command to stdout instead of executing it.




## Keys

- `Up` / `Down` / Scroll: move selection
- `Tab`: inserts selected command
- `PageUp` / `PageDown`: move faster
- `Backspace`: delete query char
- `Enter`: print and optionally runs the selected command
- `Cmd-C` / `Cmd-V`: copy/paste query text in kitty while mouse takeover is active
- `Esc` or `Ctrl-C`: quit
