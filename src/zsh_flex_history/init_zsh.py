from __future__ import annotations

from argparse import ArgumentParser
import os
from pathlib import Path


def default_hook_path() -> Path:
    config_home = os.environ.get("XDG_CONFIG_HOME")
    if config_home:
        return Path(config_home).expanduser() / "zsh-flex-history" / "hook.zsh"
    return Path.home() / ".config" / "zsh-flex-history" / "hook.zsh"


HOOK_SNIPPET = """_zsh_flex_history_line_init() {
  local cmd
  local zsh_flex_history_bin="${ZSH_FLEX_HISTORY_BIN:-${commands[zsh-flex-history]:-zsh-flex-history}}"
  cmd="$("$zsh_flex_history_bin" --use-custom-history --print-only 2>/dev/null)" || return
  [[ -z "$cmd" ]] && return

  BUFFER="$cmd"
  CURSOR=${#BUFFER}
  zle redisplay
  zle -U $'\\n'
}

_zsh_flex_history_preexec() {
  _zsh_flex_history_last_cmd="$1"
  _zsh_flex_history_last_cwd="$PWD"
}

_zsh_flex_history_precmd() {
  local exit_status=$?
  local zsh_flex_history_bin="${ZSH_FLEX_HISTORY_BIN:-${commands[zsh-flex-history]:-zsh-flex-history}}"
  [[ -z "${_zsh_flex_history_last_cmd:-}" ]] && return

  "$zsh_flex_history_bin" \\
    --use-custom-history \\
    --record-status \\
    --status-code "$exit_status" \\
    --status-cwd "${_zsh_flex_history_last_cwd:-$PWD}" \\
    --status-command "$_zsh_flex_history_last_cmd" \\
    >/dev/null 2>&1 || true

  unset _zsh_flex_history_last_cmd
  unset _zsh_flex_history_last_cwd
}

autoload -Uz add-zle-hook-widget
autoload -Uz add-zsh-hook
add-zle-hook-widget line-init _zsh_flex_history_line_init
add-zsh-hook preexec _zsh_flex_history_preexec
add-zsh-hook precmd _zsh_flex_history_precmd
"""


def main() -> int:
    parser = ArgumentParser()
    parser.add_argument("--hook", action="store_true", help="Print the zsh hook body.")
    parser.add_argument("--hook-path", default="", help="Path to write/read the generated zsh hook file.")
    args = parser.parse_args()
    if args.hook:
        print(HOOK_SNIPPET, end="")
    else:
        hook_path = Path(args.hook_path).expanduser() if args.hook_path else default_hook_path()
        hook_path.parent.mkdir(parents=True, exist_ok=True)
        hook_path.write_text(HOOK_SNIPPET, encoding="utf-8")
        print("# Start: Automatically added by zsh-flex-history")
        print(f'source "{hook_path}"')
        print("# End: Automatically added by zsh-flex-history")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
