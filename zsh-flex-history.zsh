_zsh_flex_history_line_init() {
  [[ -n ${widgets[fh-orig-line-init]} ]] && zle fh-orig-line-init

  local cmd
  local zsh_flex_history_bin="${ZSH_FLEX_HISTORY_BIN:-zsh-flex-history}"
  cmd="$("$zsh_flex_history_bin" --use-custom-history --print-only 2>/dev/null)" || return
  [[ -z "$cmd" ]] && return

  BUFFER="$cmd"
  CURSOR=${#BUFFER}
  zle redisplay
  zle -U $'\n'
}

zle -N zle-line-init _zsh_flex_history_line_init
