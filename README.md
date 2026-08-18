# zsh mouse and flex history search

![zsh flex history screenshot](./demo.png)

Use the mouse to edit commands in your terminal just like regular text, drag and select, overwrite by typing. Automatically search zsh history with directory aware fuzzy matching, and syntax highlighting. It runs on zsh hooks every time your prompt opens.


## Install and Setup

Install from GitHub. This requires UV (Python) and rust:

```bash
git clone https://github.com/AIex7/zsh-mouse-and-flex-search
cd zsh-mouse-and-flex-search
uv tool install --editable --force .
printf '%s\n' 'eval "$(zsh-flex-history-init-zsh --hook)"' >> "${ZDOTDIR:-$HOME}/.zshrc"
```

Optionally, to import your existing Zsh history into the custom SQLite history database, run:

`zsh-flex-history-import`

## Behavior

- Uses in-order directory aware flexible fuzzy matching.
- Failed commands show with a ○ character
- Prioritizes first-token matches (command completion and matching command prefixes) ahead of deeper in-string matches, then scores by recency and query fit.
- Takes over mouse `x` from the native terminal app only when there is any text in the prompt.
- Syntax highlighting
- Runtime path completion is supported. Will appear first when a path is detected, then all subsequent results appear after all history completions.

## Env Variables

- `ZSH_FLEX_HISTORY_EMPTY_SPACE_COMMAND`
  - Pressing Space with an empty query accepts and runs that command immediately. For example, "emacs ."
  - The command is never added to custom history.
- `ZSH_FLEX_HISTORY_COLOR`
  - Sets the ANSI color used for normal history results.
  - Accepts `0`-`15` or names like `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `white`, `gray`, and `bright-blue`.
  - Defaults to the terminal's foreground color.
- `ZSH_FLEX_HISTORY_RUNTIME_COLOR`
  - Sets the ANSI color used for runtime completions.
  - Accepts the same `0`-`15` values and color names as `ZSH_FLEX_HISTORY_COLOR`.
  - Defaults to the terminal's foreground color.
- `ZSH_FLEX_HISTORY_CURSOR_COLOR`
  - Sets the ANSI background color of the visual cursor.
  - Accepts the same `0`-`15` values and color names as `ZSH_FLEX_HISTORY_COLOR`, plus `#RRGGBB` values such as `#ff0000`.
  - Defaults to the terminal detected cursor.
    
## Keys

- `Up` / `Down` / Scroll: move selection
- `Tab`: inserts selected command
- `Enter`: print and optionally runs the selected command
- `Cmd-C` / `Cmd-V`: copy/paste query text in kitty while mouse takeover is active
- `Opt-C` / `Opt-V`: copy/paste query text in non-kitty terminals
- `Esc` or `Ctrl-C`: quit
