use std::fs;
use std::path::PathBuf;
use rusqlite::Connection;

use zsh_flex_history::db::{default_custom_history_path, ensure_custom_history_file};

fn default_input_history_path() -> PathBuf {
    if let Ok(hist) = std::env::var("HISTFILE") {
        if !hist.trim().is_empty() {
            return PathBuf::from(hist.trim());
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".zsh_history")
    } else {
        PathBuf::from(".zsh_history")
    }
}

fn normalize_epoch_timestamp(epoch_str: &str) -> String {
    epoch_str
        .parse::<i64>()
        .map(|epoch| epoch.to_string())
        .unwrap_or_default()
}

fn parse_mixed_zsh_history(path: &std::path::Path) -> Vec<(String, String)> {
    if !path.exists() {
        return Vec::new();
    }
    let raw_bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return Vec::new(),
    };
    let raw = String::from_utf8_lossy(&raw_bytes);
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    let mut entries = Vec::new();
    let mut current_extended_command: Option<String> = None;
    let mut current_extended_timestamp = String::new();

    let flush_extended = |cmd_opt: &mut Option<String>, ts: &mut String, out: &mut Vec<(String, String)>| {
        if let Some(cmd) = cmd_opt.take() {
            let cleaned = cmd.replace("\r\n", "\n").replace('\r', "\n").replace('\0', "");
            let trimmed = cleaned.trim_matches('\n');
            if !trimmed.trim().is_empty() {
                out.push((trimmed.to_string(), std::mem::take(ts)));
            }
        }
        ts.clear();
    };

    for line in normalized.split('\n') {
        if line.starts_with(": ") && line.contains(';') {
            if let Some(semi_pos) = line.find(';') {
                let meta = &line[2..semi_pos];
                let epoch = meta.split(':').next().unwrap_or("");
                flush_extended(&mut current_extended_command, &mut current_extended_timestamp, &mut entries);
                current_extended_timestamp = normalize_epoch_timestamp(epoch);
                current_extended_command = Some(line[semi_pos + 1..].to_string());
                continue;
            }
        }

        if let Some(ext) = &mut current_extended_command {
            if ext.ends_with('\\') {
                ext.push('\n');
                ext.push_str(line);
                continue;
            }
            flush_extended(&mut current_extended_command, &mut current_extended_timestamp, &mut entries);
        }

        let cleaned = line.replace("\r\n", "\n").replace('\r', "\n").replace('\0', "");
        let trimmed = cleaned.trim_matches('\n');
        if !trimmed.trim().is_empty() {
            entries.push((trimmed.to_string(), String::new()));
        }
    }

    flush_extended(&mut current_extended_command, &mut current_extended_timestamp, &mut entries);
    entries
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut source = default_input_history_path();
    let mut target = default_custom_history_path();
    let mut append = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--source" => {
                i += 1;
                if i < args.len() {
                    source = PathBuf::from(&args[i]);
                }
            }
            "--target" => {
                i += 1;
                if i < args.len() {
                    target = PathBuf::from(&args[i]);
                }
            }
            "--append" => append = true,
            _ => {}
        }
        i += 1;
    }

    let entries = parse_mixed_zsh_history(&source);
    if let Err(e) = ensure_custom_history_file(&target) {
        eprintln!("Failed to initialize database {}: {}", target.display(), e);
        std::process::exit(1);
    }

    let mut conn = match Connection::open(&target) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to open database {}: {}", target.display(), e);
            std::process::exit(1);
        }
    };

    let tx = match conn.transaction() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to begin transaction: {}", e);
            std::process::exit(1);
        }
    };

    if !append {
        let _ = tx.execute("DELETE FROM custom_history", []);
    }

    for (cmd, ts) in &entries {
        let _ = tx.execute(
            "INSERT INTO custom_history(command, cwd, timestamp) VALUES(?, ?, ?)",
            rusqlite::params![cmd, "", ts],
        );
    }

    if let Err(e) = tx.commit() {
        eprintln!("Failed to commit import: {}", e);
        std::process::exit(1);
    }

    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM custom_history", [], |r| r.get(0))
        .unwrap_or(0);

    println!("Imported {} entries into {} (total rows: {}).", entries.len(), target.display(), total);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_history_replaces_invalid_utf8_without_discarding_entries() {
        let path = std::env::temp_dir().join(format!(
            "zfh_import_invalid_utf8_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        fs::write(&path, b"git \xffstatus\ncargo test\n").unwrap();

        let entries = parse_mixed_zsh_history(&path);
        assert_eq!(
            entries,
            vec![
                ("git \u{fffd}status".to_string(), String::new()),
                ("cargo test".to_string(), String::new()),
            ]
        );

        let _ = fs::remove_file(path);
    }
}
