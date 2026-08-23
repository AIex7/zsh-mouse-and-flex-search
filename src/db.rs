use std::fs;
use std::path::{Path, PathBuf};
use rusqlite::{params, Connection, OptionalExtension};

use crate::layout::shell_words_for_matching;
use crate::search::{CandidateInput, NativeHistory};

pub fn default_app_state_dir() -> PathBuf {
    if let Ok(val) = std::env::var("XDG_STATE_HOME") {
        if !val.trim().is_empty() {
            return PathBuf::from(val.trim()).join("zsh-flex-history");
        }
    }
    if cfg!(target_os = "macos") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("zsh-flex-history");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("zsh-flex-history")
    } else {
        PathBuf::from(".zsh-flex-history")
    }
}

pub fn default_custom_history_path() -> PathBuf {
    default_app_state_dir().join("history.db")
}

pub fn default_history_log_path() -> PathBuf {
    if let Ok(val) = std::env::var("ZSH_FLEX_HISTORY_LOG_FILE") {
        if !val.trim().is_empty() {
            return PathBuf::from(val.trim());
        }
    }
    default_app_state_dir().join("history_rebuild.log")
}

pub fn default_daemon_socket_path(use_custom_history: bool) -> PathBuf {
    let base_dir = if let Ok(val) = std::env::var("XDG_RUNTIME_DIR") {
        if !val.trim().is_empty() {
            PathBuf::from(val.trim())
        } else {
            std::env::temp_dir()
        }
    } else {
        std::env::temp_dir()
    };
    let uid = unsafe { libc::getuid() };
    let suffix = if use_custom_history { "-custom" } else { "" };
    base_dir.join(format!("zsh-flex-history-{}{}.sock", uid, suffix))
}

pub fn normalize_cwd_value(cwd: &str) -> String {
    let stripped = cwd.trim();
    if stripped.is_empty() {
        return String::new();
    }
    let p = Path::new(stripped);
    p.to_string_lossy().to_string()
}

pub fn ensure_custom_history_file(path: &Path) -> Result<(), rusqlite::Error> {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)?;
    conn.execute(
        "
        CREATE TABLE IF NOT EXISTS custom_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            command TEXT NOT NULL,
            cwd TEXT NOT NULL,
            timestamp TEXT NOT NULL,
            failed INTEGER NOT NULL DEFAULT 0,
            status_revision INTEGER NOT NULL DEFAULT 0
        )
        ",
        [],
    )?;

    let mut stmt = conn.prepare("PRAGMA table_info(custom_history)")?;
    let cols: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<_, _>>()?;
    drop(stmt);

    if !cols.iter().any(|column| column == "failed") {
        conn.execute(
            "ALTER TABLE custom_history ADD COLUMN failed INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !cols.iter().any(|column| column == "status_revision") {
        conn.execute(
            "ALTER TABLE custom_history ADD COLUMN status_revision INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }

    conn.execute(
        "
        CREATE TABLE IF NOT EXISTS custom_history_metadata (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            status_revision INTEGER NOT NULL DEFAULT 0
        )
        ",
        [],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO custom_history_metadata(id, status_revision) VALUES(1, 0)",
        [],
    )?;
    conn.execute(
        "
        UPDATE custom_history_metadata
        SET status_revision = MAX(
            status_revision,
            COALESCE((SELECT MAX(status_revision) FROM custom_history), 0)
        )
        WHERE id = 1
        ",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_custom_history_command_cwd ON custom_history(command, cwd)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_custom_history_id_desc ON custom_history(id DESC)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_custom_history_status_revision ON custom_history(status_revision)",
        [],
    )?;
    Ok(())
}

pub fn make_candidate_input(
    command: String,
    cwd: Option<String>,
    failed: bool,
) -> Option<CandidateInput> {
    let cleaned = command.replace("\r\n", "\n").replace('\r', "\n").replace('\0', "");
    let trimmed = cleaned.trim_matches('\n');
    if trimmed.trim().is_empty() {
        return None;
    }
    let text = trimmed.to_string();
    let text_lower = text.to_lowercase();
    let words = shell_words_for_matching(&text_lower);
    let normalized_cwd = cwd.map(|c| normalize_cwd_value(&c)).filter(|c| !c.is_empty());
    Some((text, text_lower, normalized_cwd, words, failed))
}

#[derive(Debug, Clone)]
pub struct CustomHistoryWatermark {
    pub row_id: i64,
    pub command: String,
    pub cwd: Option<String>,
    pub timestamp: Option<String>,
}

pub fn build_native_custom_history_candidates(
    path: &Path,
    limit: Option<usize>,
) -> (NativeHistory, Option<CustomHistoryWatermark>, i64) {
    let mut native_candidates = NativeHistory::new(Vec::new());
    if !path.exists() {
        return (native_candidates, None, 0);
    }

    let conn = match Connection::open(path) {
        Ok(c) => c,
        Err(_) => return (native_candidates, None, 0),
    };

    let status_revision: i64 = conn
        .query_row(
            "SELECT status_revision FROM custom_history_metadata WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let query = if let Some(lim) = limit {
        format!("SELECT id, command, cwd, timestamp, failed FROM custom_history ORDER BY id DESC LIMIT {}", lim)
    } else {
        "SELECT id, command, cwd, timestamp, failed FROM custom_history ORDER BY id DESC".to_string()
    };

    let mut watermark = None;
    if let Ok(mut stmt) = conn.prepare(&query) {
        let mut rows = match stmt.query([]) {
            Ok(r) => r,
            Err(_) => return (native_candidates, None, status_revision),
        };

        let mut batch = Vec::new();
        while let Ok(Some(row)) = rows.next() {
            let id: i64 = match row.get(0) { Ok(v) => v, Err(_) => continue };
            let cmd: String = match row.get(1) { Ok(v) => v, Err(_) => continue };
            let cwd: String = match row.get(2) { Ok(v) => v, Err(_) => String::new() };
            let timestamp: String = match row.get(3) { Ok(v) => v, Err(_) => String::new() };
            let failed: i64 = row.get(4).unwrap_or(0);

            if watermark.is_none() {
                watermark = Some(CustomHistoryWatermark {
                    row_id: id,
                    command: cmd.clone(),
                    cwd: if cwd.is_empty() { None } else { Some(normalize_cwd_value(&cwd)) },
                    timestamp: if timestamp.is_empty() { None } else { Some(timestamp.clone()) },
                });
            }

            if let Some(candidate) = make_candidate_input(cmd, Some(cwd), failed != 0) {
                batch.push(candidate);
            }
        }
        native_candidates.extend(batch);
    }

    (native_candidates, watermark, status_revision)
}

pub fn append_custom_history_entry(path: &Path, command: &str, cwd: &str, timestamp: &str) -> bool {
    let normalized_command = command.trim();
    let normalized_cwd = normalize_cwd_value(cwd);
    if normalized_command.is_empty() {
        return false;
    }

    let _ = ensure_custom_history_file(path);
    let mut conn = match Connection::open(path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    let tx = match conn.transaction() {
        Ok(t) => t,
        Err(_) => return false,
    };

    let _ = tx.execute(
        "DELETE FROM custom_history WHERE command = ? AND cwd = ?",
        params![normalized_command, normalized_cwd],
    );
    let _ = tx.execute(
        "INSERT INTO custom_history(command, cwd, timestamp, failed) VALUES(?, ?, ?, 0)",
        params![normalized_command, normalized_cwd, timestamp],
    );

    tx.commit().is_ok()
}

pub fn update_custom_history_exit_status(
    path: &Path,
    command: &str,
    cwd: &str,
    status: i32,
) -> bool {
    let normalized_command = command.trim();
    let normalized_cwd = normalize_cwd_value(cwd);
    if normalized_command.is_empty() || !path.exists() {
        return false;
    }

    let mut conn = match Connection::open(path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    let tx = match conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate) {
        Ok(t) => t,
        Err(_) => return false,
    };

    let row_id: Option<i64> = tx
        .query_row(
            "SELECT id FROM custom_history WHERE command = ? AND cwd = ? ORDER BY id DESC LIMIT 1",
            params![normalized_command, normalized_cwd],
            |r| r.get(0),
        )
        .optional()
        .unwrap_or(None);

    let row_id = match row_id {
        Some(id) => id,
        None => return false,
    };

    if status == 0 {
        return tx.commit().is_ok();
    }

    let _ = tx.execute(
        "UPDATE custom_history_metadata SET status_revision = status_revision + 1 WHERE id = 1",
        [],
    );
    let revision: i64 = match tx.query_row(
        "SELECT status_revision FROM custom_history_metadata WHERE id = 1",
        [],
        |r| r.get(0),
    ) {
        Ok(rev) => rev,
        Err(_) => return false,
    };

    let _ = tx.execute(
        "UPDATE custom_history SET failed = 1, status_revision = ? WHERE id = ?",
        params![revision, row_id],
    );

    tx.commit().is_ok()
}

pub fn load_plain_zsh_history(path: &Path) -> Vec<CandidateInput> {
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
    let mut current_extended: Option<String> = None;

    let flush_entry = |text: &str, out: &mut Vec<CandidateInput>| {
        let cmd = text.trim_end_matches('\n').replace("\\\n", "");
        if let Some(candidate) = make_candidate_input(cmd, None, false) {
            out.push(candidate);
        }
    };

    for line in normalized.split('\n') {
        if line.starts_with(": ") && line.contains(';') {
            if let Some(semi_pos) = line.find(';') {
                if let Some(ext) = current_extended.take() {
                    flush_entry(&ext, &mut entries);
                }
                current_extended = Some(line[semi_pos + 1..].to_string());
                continue;
            }
        }

        if let Some(ext) = &mut current_extended {
            ext.push('\n');
            ext.push_str(line);
            continue;
        }

        let plain = line.trim();
        if !plain.is_empty() {
            flush_entry(plain, &mut entries);
        }
    }

    if let Some(ext) = current_extended {
        flush_entry(&ext, &mut entries);
    }

    // Newest first, deduplicating while preserving order
    entries.reverse();
    let mut deduped = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in entries {
        if seen.insert(entry.0.clone()) {
            deduped.push(entry);
        }
    }
    deduped
}
