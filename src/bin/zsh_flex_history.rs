use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use zsh_flex_history::daemon::{run_history_daemon, HistoryDaemonClient};
use zsh_flex_history::db::*;
use zsh_flex_history::ui::run;

fn parse_history_length(raw: &str) -> Result<usize, String> {
    let s = raw.trim().to_lowercase().replace('_', "");
    if s.is_empty() {
        return Err("empty history length".to_string());
    }
    if let Some(rest) = s.strip_suffix('k') {
        let n: usize = rest.parse().map_err(|_| format!("invalid history length: {}", raw))?;
        return Ok(n * 1000);
    }
    if let Some(rest) = s.strip_suffix('m') {
        let n: usize = rest.parse().map_err(|_| format!("invalid history length: {}", raw))?;
        return Ok(n * 1_000_000);
    }
    let n: usize = s.parse().map_err(|_| format!("invalid history length: {}", raw))?;
    if n == 0 {
        return Err("history length must be positive".to_string());
    }
    Ok(n)
}

fn print_help() {
    println!(
        "Interactive zsh history search with flex matching and mouse support

Usage: zsh-flex-history [OPTIONS]

Options:
  --print-only                      Print selected command to stdout instead of executing it.
  --no-save-history                 Do not add the selected command to custom history.
  --history-length <N>              Maximum SQLite history rows to load on startup (e.g. 10000 or 10k).
  --debug-daemon                    Print daemon connection/startup diagnostics to stderr.
  --use-custom-history              Use per-user SQLite history (command, cwd, timestamp).
  -h, --help                        Show this help message and exit.
"
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let mut print_only = false;
    let mut no_save_history = false;
    let mut daemon = false;
    let mut socket_path: Option<PathBuf> = None;
    let mut history_file: Option<PathBuf> = None;
    let mut history_length: Option<usize> = None;
    let mut debug_daemon = false;
    let mut use_custom_history = false;
    let mut record_status = false;
    let mut status_command = String::new();
    let mut status_code: i32 = 0;
    let mut status_cwd = String::new();

    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "--print-only" => print_only = true,
            "--no-save-history" => no_save_history = true,
            "--daemon" => daemon = true,
            "--socket-path" => {
                i += 1;
                if i < args.len() {
                    socket_path = Some(PathBuf::from(&args[i]));
                }
            }
            "--history-file" => {
                i += 1;
                if i < args.len() {
                    history_file = Some(PathBuf::from(&args[i]));
                }
            }
            "--history-length" => {
                i += 1;
                if i < args.len() {
                    match parse_history_length(&args[i]) {
                        Ok(len) => history_length = Some(len),
                        Err(e) => {
                            eprintln!("zsh_flex_history: {}", e);
                            std::process::exit(2);
                        }
                    }
                }
            }
            "--debug-daemon" => debug_daemon = true,
            "--use-custom-history" => use_custom_history = true,
            "--record-status" => record_status = true,
            "--status-command" => {
                i += 1;
                if i < args.len() {
                    status_command = args[i].clone();
                }
            }
            "--status-code" => {
                i += 1;
                if i < args.len() {
                    status_code = args[i].parse().unwrap_or(0);
                }
            }
            "--status-cwd" => {
                i += 1;
                if i < args.len() {
                    status_cwd = args[i].clone();
                }
            }
            _ => {}
        }
        i += 1;
    }

    let history_path = if let Some(hf) = history_file {
        hf
    } else if use_custom_history {
        default_custom_history_path()
    } else if let Ok(hist) = std::env::var("HISTFILE") {
        PathBuf::from(hist)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".zsh_history")
    } else {
        PathBuf::from(".zsh_history")
    };

    if record_status {
        if !use_custom_history {
            eprintln!("zsh_flex_history: --record-status requires --use-custom-history");
            std::process::exit(2);
        }
        let cwd = if status_cwd.is_empty() {
            std::env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default()
        } else {
            status_cwd
        };
        let success = update_custom_history_exit_status(&history_path, &status_command, &cwd, status_code);
        std::process::exit(if success { 0 } else { 1 });
    }

    let resolved_socket_path = socket_path.unwrap_or_else(|| default_daemon_socket_path(use_custom_history));

    if daemon {
        let code = run_history_daemon(
            &history_path,
            &resolved_socket_path,
            debug_daemon,
            history_length,
            use_custom_history,
        );
        std::process::exit(code);
    }

    let client = Arc::new(HistoryDaemonClient {
        socket_path: resolved_socket_path,
        history_path: history_path.clone(),
        debug: debug_daemon,
        history_length,
        use_custom_history,
    });

    if !client.ensure_running() {
        eprintln!("zsh_flex_history: daemon unavailable");
        std::process::exit(1);
    }

    let empty_space_cmd = std::env::var("ZSH_FLEX_HISTORY_EMPTY_SPACE_COMMAND")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let selection = run(print_only, client, empty_space_cmd);

    if let Some((selected_cmd, skip_record)) = selection {
        let cleaned = selected_cmd.replace("\r\n", "\n").replace('\r', "\n").replace('\0', "");
        if cleaned.trim().is_empty() {
            std::process::exit(1);
        }

        if use_custom_history && !no_save_history && !skip_record {
            let cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs().to_string())
                .unwrap_or_default();
            append_custom_history_entry(&history_path, &cleaned, &cwd, &timestamp);
        }

        if print_only {
            println!("{}", cleaned);
            std::process::exit(0);
        }

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        println!("$ {}", cleaned);
        let status = Command::new(shell).arg("-lc").arg(&cleaned).status();
        std::process::exit(status.map(|s| s.code().unwrap_or(1)).unwrap_or(1));
    }

    std::process::exit(1);
}
