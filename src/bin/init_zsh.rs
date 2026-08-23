use std::fs;
use std::path::PathBuf;

pub const HOOK_SNIPPET: &str = r#"_zsh_flex_history_line_init() {
  local cmd
  local zsh_flex_history_bin="${ZSH_FLEX_HISTORY_BIN:-${commands[zsh-flex-history]:-zsh-flex-history}}"
  cmd="$("$zsh_flex_history_bin" --use-custom-history --print-only 2>/dev/null)" || return
  [[ -z "$cmd" ]] && return

  BUFFER="$cmd"
  CURSOR=${#BUFFER}
  zle redisplay
  zle -U $'\n'
}

_zsh_flex_history_preexec() {
  _zsh_flex_history_last_cmd="$1"
  _zsh_flex_history_last_cwd="$PWD"
}

_zsh_flex_history_precmd() {
  local exit_status=$?
  local zsh_flex_history_bin="${ZSH_FLEX_HISTORY_BIN:-${commands[zsh-flex-history]:-zsh-flex-history}}"
  [[ -z "${_zsh_flex_history_last_cmd:-}" ]] && return

  if (( exit_status != 0 )); then
    "$zsh_flex_history_bin" \
      --use-custom-history \
      --record-status \
      --status-code "$exit_status" \
      --status-cwd "${_zsh_flex_history_last_cwd:-$PWD}" \
      --status-command "$_zsh_flex_history_last_cmd" \
      >/dev/null 2>&1 || true
  fi

  unset _zsh_flex_history_last_cmd
  unset _zsh_flex_history_last_cwd
}

autoload -Uz add-zle-hook-widget
autoload -Uz add-zsh-hook
add-zle-hook-widget line-init _zsh_flex_history_line_init
add-zsh-hook preexec _zsh_flex_history_preexec
add-zsh-hook precmd _zsh_flex_history_precmd
"#;

fn default_hook_path() -> PathBuf {
    if let Ok(config_home) = std::env::var("XDG_CONFIG_HOME") {
        if !config_home.trim().is_empty() {
            return PathBuf::from(config_home.trim())
                .join("zsh-flex-history")
                .join("hook.zsh");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home)
            .join(".config")
            .join("zsh-flex-history")
            .join("hook.zsh")
    } else {
        PathBuf::from("hook.zsh")
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut print_hook = false;
    let mut hook_path = String::new();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--hook" => print_hook = true,
            "--hook-path" => {
                i += 1;
                if i < args.len() {
                    hook_path = args[i].clone();
                }
            }
            _ => {}
        }
        i += 1;
    }

    if print_hook {
        print!("{}", HOOK_SNIPPET);
    } else {
        let target_path = if hook_path.is_empty() {
            default_hook_path()
        } else {
            PathBuf::from(hook_path)
        };
        if let Some(parent) = target_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(e) = fs::write(&target_path, HOOK_SNIPPET) {
            eprintln!("Failed to write hook file: {}", e);
            std::process::exit(1);
        }
        println!("# Start: Automatically added by zsh-flex-history");
        println!("source \"{}\"", target_path.display());
        println!("# End: Automatically added by zsh-flex-history");
    }
}
